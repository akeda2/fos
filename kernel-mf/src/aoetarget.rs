//! The AoE target of the server kernel: serves disks to Linux (`aoe` module, `vblade`'s initiators) and to other fos hosts
//! as ATA over Ethernet (ethertype 0x88a2). The protocol is `crates/aoe`; this is the glue (docs/block-sharing.md).
//!
//! **Threads.** Frames never reach a disk from the receive path. `frame_hook` runs with the NIC lock held, and a disk read
//! there would stall every interface (and the audio drivers that share the lock), so it only copies the frame into the
//! queue of the export it is for and wakes that export's worker thread. Each export has its own queue and its own worker,
//! so a disk that is slow or has a backlog (a USB stick) fills only its own queue and drops only its own frames; the
//! initiator retransmits, as it would for a frame lost on the wire. One more thread (`aoe-flush`) flushes written disks
//! after a pause in writing.
//!
//! **The disk lock.** The block drivers keep their state in statics, poll, and do one command at a time, so every disk user
//! in this kernel (the shell, tapes, NETFS, KUP) takes `tapefs::lock()`. So does the worker, around each batch of up to
//! `BATCH` frames, and the flusher around a flush. The consequence is that disk transfers of all exports and of everything
//! else are serial however many workers there are: the threads isolate queues and keep the network side moving, they do
//! not make two disks read at once. That would need locks per controller in the drivers (docs/block-sharing.md).
//!
//! **Writes** go to the drive with `block::write_deferred`. The IDENTIFY data says there is a write cache, so the initiator
//! may rely on FLUSH CACHE; the flusher also flushes a disk `FLUSH_AFTER` ticks after its last write, and `unexport` does.
//!
//! Exports are read-only unless `rw confirm` is given, and disk 1 (the boot disk) is never writable. Nothing stops a second
//! writer: do not mount an exported disk here or on another host while it is exported `rw`.

use crate::sched::{self, SleepMutex};
use crate::sync::SpinLock;
use crate::{block, net, tapefs};
use aoe::cache::{Cache, CacheStats};
use aoe::{Disk, Target};
use alloc::vec;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use kernel_core::nicctl;

const MAX_EXPORTS: usize = 8;
const QUEUE: usize = 32;
const FRAME: usize = 1600;
/// Frames a worker handles per hold of the disk lock: fewer lock round trips under load, and still short enough that the
/// shell or a tape waiting for the disk is not kept out for long (with the cache most frames of a batch are memory copies).
const BATCH: usize = 32;
/// Each worker's read-ahead and write-gather buffers (`aoe::cache`): one disk command of up to 64 KiB instead of one per
/// 1 KiB frame.
const CACHE_BYTES: usize = 64 * 1024;
/// A worker that slept this many ticks (10 ms each) or more drops its read buffer: the bound on how stale a read-ahead
/// can be when something other than this export writes the disk.
const IDLE_TICKS: u64 = 2;
/// Replies queued per doorbell (`E1000::send_deferred`): one register write, which leaves the guest under a hypervisor,
/// for this many frames, without holding replies back for a whole batch of disk work.
const KICK_EVERY: usize = 8;
/// The flusher looks every `FLUSH_TICK` ticks (10 ms each) and flushes a disk that has not been written for `FLUSH_AFTER`.
const FLUSH_TICK: u64 = 2;
const FLUSH_AFTER: u64 = 5;
/// The Ethernet payload size the target assumes: jumbo frames do not reach the receive path yet (`nicctl` drains into
/// 1600-byte buffers), so it advertises 2 sectors per frame.
const MTU: usize = 1500;
/// What `aoe-shelf` and the shell leave to the machine: derived from NIC 1's MAC.
const AUTO_SHELF: u32 = u32::MAX;
const WAKE_KEY: usize = 0xa0e0_0000;

struct Export {
    /// 0-based index in the block layer; the slot is this plus one, the disk number `disks` shows.
    disk: usize,
    target: Target,
}

struct Queue {
    frames: [[u8; FRAME]; QUEUE],
    lengths: [u16; QUEUE],
    nics: [u8; QUEUE],
    head: usize,
    count: usize,
    dropped: u64,
}

/// One export: its queue (written by `frame_hook`, read by its worker), its worker thread and the facts the hook and the
/// flusher read without taking a sleeping lock.
struct Slot {
    inbox: SpinLock<Queue>,
    /// The slot number the export answers to, 0 while the slot is free: what the hook routes by.
    number: AtomicU8,
    started: AtomicBool,
    /// Written since the last flush, and the tick of the last write.
    dirty: AtomicBool,
    last_write: AtomicU64,
    export: SleepMutex<Option<Export>>,
    stats: Stats,
    /// Moves on every export into this slot, so the worker drops a read buffer that belonged to the previous disk.
    generation: AtomicU64,
}

/// Where a worker's time goes (`aoe` shows it, `aoe reset` clears it). Kept in CPU cycles (`cpu::cycles`, the TSC), which
/// cost nothing to read, and turned into microseconds only when printed: `time::micros` may read the ACPI PM timer through an
/// I/O port (no invariant TSC, as under KVM's default CPU model), which cost the very path it measured about 40 us a frame: how often it woke and how many frames it took per
/// wake-up, how long a frame waited in the queue before the worker ran, how long it waited for the disk lock, and how long
/// serving it took (disk, reply, send).
struct Stats {
    wakes: AtomicU64,
    frames: AtomicU64,
    wake_us: AtomicU64,
    lock_us: AtomicU64,
    serve_us: AtomicU64,
    /// Of `serve_us`: the target's own work (the disk transfer and the reply), and the send (NIC lock and the driver).
    handle_us: AtomicU64,
    send_us: AtomicU64,
    /// The cache's counters (`aoe::cache::CacheStats`), added after every batch.
    disk_reads: AtomicU64,
    disk_writes: AtomicU64,
    read_hits: AtomicU64,
    gathered: AtomicU64,
    write_errors: AtomicU64,
    /// When the first frame was queued into an empty queue (µs), 0 for none.
    queued_at: AtomicU64,
}

impl Stats {
    const fn new() -> Stats {
        Stats { wakes: AtomicU64::new(0), frames: AtomicU64::new(0), wake_us: AtomicU64::new(0), lock_us: AtomicU64::new(0), serve_us: AtomicU64::new(0), handle_us: AtomicU64::new(0),
            send_us: AtomicU64::new(0),
            disk_reads: AtomicU64::new(0),
            disk_writes: AtomicU64::new(0),
            read_hits: AtomicU64::new(0),
            gathered: AtomicU64::new(0),
            write_errors: AtomicU64::new(0),
            queued_at: AtomicU64::new(0),
        }
    }

    fn reset(&self) {
        for counter in [
            &self.wakes,
            &self.frames,
            &self.wake_us,
            &self.lock_us,
            &self.serve_us,
            &self.handle_us,
            &self.send_us,
            &self.disk_reads,
            &self.disk_writes,
            &self.read_hits,
            &self.gathered,
            &self.write_errors,
        ] {
            counter.store(0, Ordering::Relaxed);
        }
    }

    /// Adds what the worker's cache counted since `before`.
    fn add_cache(&self, now: &CacheStats, before: &CacheStats) {
        self.disk_reads.fetch_add(now.disk_reads - before.disk_reads, Ordering::Relaxed);
        self.disk_writes.fetch_add(now.disk_writes - before.disk_writes, Ordering::Relaxed);
        self.read_hits.fetch_add(now.read_hits - before.read_hits, Ordering::Relaxed);
        self.gathered.fetch_add(now.gathered - before.gathered, Ordering::Relaxed);
        self.write_errors.fetch_add(now.write_errors - before.write_errors, Ordering::Relaxed);
    }
}

/// AoE frames by the thread that drained them from the NIC.
static FROM_RX: AtomicU64 = AtomicU64::new(0);
static FROM_SERVICE: AtomicU64 = AtomicU64::new(0);

impl Slot {
    const fn new() -> Slot {
        Slot {
            inbox: SpinLock::new(Queue { frames: [[0; FRAME]; QUEUE], lengths: [0; QUEUE], nics: [0; QUEUE], head: 0, count: 0, dropped: 0 }),
            number: AtomicU8::new(0),
            started: AtomicBool::new(false),
            dirty: AtomicBool::new(false),
            last_write: AtomicU64::new(0),
            export: SleepMutex::new(None),
            stats: Stats::new(),
            generation: AtomicU64::new(0),
        }
    }
}

static SLOTS: [Slot; MAX_EXPORTS] = [const { Slot::new() }; MAX_EXPORTS];
static SHELF: AtomicU32 = AtomicU32::new(AUTO_SHELF);
/// How many disks are exported: only while it is above 0 does `frame_hook` take 0x88a2 frames (otherwise a tape may have them).
static ACTIVE: AtomicUsize = AtomicUsize::new(0);
static FLUSHER: AtomicBool = AtomicBool::new(false);

/// Whether `frame_hook` should hand 0x88a2 frames to this module.
pub fn wanted() -> bool {
    ACTIVE.load(Ordering::Acquire) > 0
}

/// Queues a received AoE frame for the export(s) it is addressed to (a broadcast Query Config goes to all). Runs under the NIC
/// lock and possibly in the SysRq rescue's interrupt context: no disk, no sleeping; the wake-up is left out when `wake` is
/// false (the worker finds the frame the next time it runs).
pub fn enqueue(nic: usize, frame: &[u8], wake: bool) {
    if frame.len() > FRAME || frame.len() < 24 {
        return;
    }
    let (shelf, slot) = (u16::from_be_bytes([frame[16], frame[17]]), frame[18]);
    if shelf != 0xffff && shelf != self::shelf() {
        return;
    }
    if net::draining_from_rx_thread() { &FROM_RX } else { &FROM_SERVICE }.fetch_add(1, Ordering::Relaxed);
    for (index, entry) in SLOTS.iter().enumerate() {
        let number = entry.number.load(Ordering::Acquire);
        if number == 0 || (slot != 0xff && slot != number) {
            continue;
        }
        let pushed = {
            let mut queue = entry.inbox.lock();
            if queue.count == QUEUE {
                queue.dropped += 1;
                false
            } else {
                let at = (queue.head + queue.count) % QUEUE;
                queue.frames[at][..frame.len()].copy_from_slice(frame);
                queue.lengths[at] = frame.len() as u16;
                queue.nics[at] = nic as u8;
                queue.count += 1;
                if queue.count == 1 {
                    let _ = entry.stats.queued_at.compare_exchange(0, crate::cpu::cycles().max(1), Ordering::AcqRel, Ordering::Relaxed);
                }
                true
            }
        };
        if pushed && wake {
            sched::wake_all(WAKE_KEY + index);
        }
    }
}

/// Takes the oldest queued frame of `slot` into `buffer`; (nic, length).
fn pop(slot: &Slot, buffer: &mut [u8]) -> Option<(usize, usize)> {
    let mut queue = slot.inbox.lock();
    if queue.count == 0 {
        return None;
    }
    let head = queue.head;
    let length = usize::from(queue.lengths[head]);
    buffer[..length].copy_from_slice(&queue.frames[head][..length]);
    let nic = usize::from(queue.nics[head]);
    queue.head = (head + 1) % QUEUE;
    queue.count -= 1;
    Some((nic, length))
}

/// The block layer as the target's disk (512-byte sectors, bounds already checked by the target and by `block`). Only used
/// with the disk lock held.
struct BlockDisk(usize);

impl Disk for BlockDisk {
    fn sectors(&self) -> u64 {
        block::get(self.0).map_or(0, |disk| disk.sectors)
    }
    fn read(&mut self, lba: u64, buffer: &mut [u8]) -> bool {
        block::read(self.0, lba, buffer).is_ok()
    }
    fn write(&mut self, lba: u64, data: &[u8]) -> bool {
        block::write_deferred(self.0, lba, data).is_ok()
    }
    fn flush(&mut self) -> bool {
        block::flush(self.0).is_ok()
    }
}

/// The shelf number: the one set by `aoe shelf N` or `aoe-shelf N` in `settings`, else the last two bytes of NIC 1's MAC.
pub fn shelf() -> u16 {
    let set = SHELF.load(Ordering::Relaxed);
    if set != AUTO_SHELF {
        return set as u16;
    }
    let mac = net::mac(0).unwrap_or([0; 6]);
    // 0xffff is the broadcast shelf.
    u16::from_be_bytes([mac[4], mac[5]]).min(0xfffe)
}

/// One worker per export: sleeps until its queue has a frame, then serves up to `BATCH` frames under one hold of the disk lock.
extern "C" fn worker(index: usize) {
    let slot = &SLOTS[index];
    let mut request = vec![0u8; FRAME];
    let mut reply = vec![0u8; FRAME];
    let (mut read_buffer, mut gather_buffer) = (vec![0u8; CACHE_BYTES], vec![0u8; CACHE_BYTES]);
    let mut cache = Cache::new(&mut read_buffer, &mut gather_buffer);
    let mut generation = u64::MAX;
    let mut reported = CacheStats::default();
    loop {
        let waiting = slot.inbox.lock().count;
        if waiting == 0 {
            // The read buffer survives a short pause (an initiator with one request outstanding pauses after every frame),
            // but not a long one: something else may write the disk meanwhile (a local user of an exported disk).
            let slept = crate::arch::ticks();
            sched::wait_while(WAKE_KEY + index, || slot.inbox.lock().count == 0);
            if crate::arch::ticks().wrapping_sub(slept) >= IDLE_TICKS {
                cache.invalidate();
            }
            slot.stats.wakes.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        let queued = slot.stats.queued_at.swap(0, Ordering::AcqRel);
        let before_lock = crate::cpu::cycles();
        if queued != 0 {
            slot.stats.wake_us.fetch_add(before_lock.saturating_sub(queued), Ordering::Relaxed);
        }
        let _disks = tapefs::lock();
        let mut guard = slot.export.lock();
        slot.stats.lock_us.fetch_add(crate::cpu::cycles().saturating_sub(before_lock), Ordering::Relaxed);
        let Some(export) = guard.as_mut() else {
            // Unexported while frames waited: they are dropped.
            while pop(slot, &mut request).is_some() {}
            continue;
        };
        let now = slot.generation.load(Ordering::Acquire);
        if now != generation {
            cache.invalidate();
            generation = now;
        }
        let mut disk = BlockDisk(export.disk);
        let mut cached = cache.on(&mut disk);
        // NICs with replies queued but not yet kicked (bit per NIC), and how many replies.
        let (mut unkicked, mut queued_replies) = (0u32, 0usize);
        for _ in 0..BATCH {
            let started = crate::cpu::cycles();
            let Some((nic, length)) = pop(slot, &mut request) else { break };
            let Some(mac) = net::mac(nic) else { continue };
            export.target.mac = mac;
            let writes = export.target.writes;
            let handle_start = crate::cpu::cycles();
            let reply_length = export.target.handle(&request[..length], &mut cached, &mut reply);
            let handled = crate::cpu::cycles();
            slot.stats.handle_us.fetch_add(handled.saturating_sub(handle_start), Ordering::Relaxed);
            if export.target.writes != writes {
                slot.last_write.store(crate::arch::ticks(), Ordering::Relaxed);
                slot.dirty.store(true, Ordering::Release);
            }
            if reply_length > 0 {
                let _nics = net::lock_nics();
                nicctl::nic_send_deferred(nic, &reply[..reply_length]);
                unkicked |= 1 << nic.min(31);
                queued_replies += 1;
                if queued_replies >= KICK_EVERY {
                    kick(unkicked);
                    (unkicked, queued_replies) = (0, 0);
                }
            }
            slot.stats.send_us.fetch_add(crate::cpu::cycles().saturating_sub(handled), Ordering::Relaxed);
            slot.stats.frames.fetch_add(1, Ordering::Relaxed);
            slot.stats.serve_us.fetch_add(crate::cpu::cycles().saturating_sub(started), Ordering::Relaxed);
        }
        if unkicked != 0 {
            let _nics = net::lock_nics();
            kick(unkicked);
        }
        // Acknowledged writes never outlive the batch in RAM: the flusher and `unexport` only see what reached the drive.
        if !cached.settle() {
            crate::println!("aoe: writing gathered sectors to disk {} failed; the next FLUSH reports it.", export.disk + 1);
        }
        let stats = cache.stats;
        slot.stats.add_cache(&stats, &reported);
        reported = stats;
    }
}

/// Flushes every written disk that has had no write for `FLUSH_AFTER` ticks, so the initiator's data reaches the drive even
/// if it never sends FLUSH CACHE (Linux's `aoe` does not).
extern "C" fn flusher(_: usize) {
    loop {
        sched::sleep_ticks(FLUSH_TICK);
        let now = crate::arch::ticks();
        for slot in SLOTS.iter() {
            if slot.dirty.load(Ordering::Acquire) && now.wrapping_sub(slot.last_write.load(Ordering::Relaxed)) >= FLUSH_AFTER {
                flush_slot(slot);
            }
        }
    }
}

/// Flushes the disk of `slot` if it was written. Takes the disk lock, then the slot's (the order `worker` uses).
fn flush_slot(slot: &Slot) {
    let _disks = tapefs::lock();
    let guard = slot.export.lock();
    // Cleared before the flush: a write that lands after it sets the flag again.
    if slot.dirty.swap(false, Ordering::AcqRel) {
        if let Some(export) = guard.as_ref() {
            if block::flush(export.disk).is_err() {
                crate::println!("aoe: flushing disk {} failed.", export.disk + 1);
            }
        }
    }
}

fn start_threads(index: usize) -> bool {
    if !SLOTS[index].started.load(Ordering::Acquire) {
        if sched::spawn_any("aoe", worker, index).is_none() {
            return false;
        }
        SLOTS[index].started.store(true, Ordering::Release);
    }
    if !FLUSHER.swap(true, Ordering::AcqRel) && sched::spawn_any("aoe-flush", flusher, 0).is_none() {
        FLUSHER.store(false, Ordering::Release);
        return false;
    }
    true
}

#[derive(Debug, PartialEq)]
pub enum ExportError {
    NoSuchDisk,
    AlreadyExported,
    TableFull,
    BootDiskWritable,
    NoThread,
}

/// Exports disk number `number` (1-based, as `disks` lists them) as slot `number` of this machine's shelf.
pub fn export(number: usize, writable: bool) -> Result<(), ExportError> {
    let Some(disk) = number.checked_sub(1).filter(|&index| block::get(index).is_some() && number < 255) else { return Err(ExportError::NoSuchDisk) };
    if writable && number == 1 {
        return Err(ExportError::BootDiskWritable);
    }
    if SLOTS.iter().any(|slot| usize::from(slot.number.load(Ordering::Acquire)) == number) {
        return Err(ExportError::AlreadyExported);
    }
    let Some(index) = SLOTS.iter().position(|slot| slot.number.load(Ordering::Acquire) == 0) else { return Err(ExportError::TableFull) };
    if !start_threads(index) {
        return Err(ExportError::NoThread);
    }
    let slot = &SLOTS[index];
    let mut target = Target::new(shelf(), number as u8, [0; 6], MTU);
    target.read_only = !writable;
    if let Some(info) = block::get(disk) {
        let model = core::str::from_utf8(&info.model).unwrap_or("").trim();
        aoe::set_text(&mut target.model, model.as_bytes());
    }
    let serial = alloc::format!("FOS{:04X}{:02X}", shelf(), number);
    aoe::set_text(&mut target.serial, serial.as_bytes());
    // Frames queued for an earlier export of this slot are stale.
    {
        let mut queue = slot.inbox.lock();
        queue.count = 0;
        queue.dropped = 0;
    }
    slot.dirty.store(false, Ordering::Release);
    slot.generation.fetch_add(1, Ordering::AcqRel);
    *slot.export.lock() = Some(Export { disk, target });
    slot.number.store(number as u8, Ordering::Release);
    ACTIVE.fetch_add(1, Ordering::AcqRel);
    Ok(())
}

/// Stops exporting disk `number`; false when it was not exported. Frames still queued are dropped, written data is flushed.
pub fn remove(number: usize) -> bool {
    let Some(slot) = SLOTS.iter().find(|slot| usize::from(slot.number.load(Ordering::Acquire)) == number) else { return false };
    // Stop routing first, then let the worker finish what it holds (the slot's lock), then flush and clear.
    slot.number.store(0, Ordering::Release);
    ACTIVE.fetch_sub(1, Ordering::AcqRel);
    flush_slot(slot);
    let _disks = tapefs::lock();
    *slot.export.lock() = None;
    slot.inbox.lock().count = 0;
    true
}

/// After boot: exports the disks `aoe-export N` lines in `settings` named (read-only; writable only by the shell).
pub fn init() {
    if let Some(shelf) = crate::config::aoe_shelf() {
        SHELF.store(u32::from(shelf), Ordering::Relaxed);
    }
    let wanted = crate::config::aoe_exports();
    for number in 1..32 {
        if wanted >> number & 1 != 0 {
            match export(number, false) {
                Ok(()) => crate::println!("aoe: disk {number} exported read-only as shelf {} slot {number}.", shelf()),
                Err(error) => crate::println!("aoe: cannot export disk {number}: {error:?}."),
            }
        }
    }
}

pub fn command(mut words: core::str::SplitWhitespace) {
    let usage = "aoe [export N [rw confirm] | unexport N | shelf N | reset]: serve disks over ATA over Ethernet.";
    match words.next() {
        None => list(),
        Some("reset") => {
            for slot in SLOTS.iter() {
                slot.stats.reset();
                slot.inbox.lock().dropped = 0;
            }
            FROM_RX.store(0, Ordering::Relaxed);
            FROM_SERVICE.store(0, Ordering::Relaxed);
            net::reset_rx_stats();
            crate::println!("aoe: counters cleared.");
        }
        Some("export") => {
            let Some(number) = words.next().and_then(|word| word.parse::<usize>().ok()) else { return crate::println!("{usage}") };
            let writable = match (words.next(), words.next()) {
                (None, _) => false,
                (Some("rw"), Some("confirm")) => true,
                (Some("rw"), _) => {
                    return crate::println!("aoe: `rw` lets every machine on this network write disk {number}. Type `aoe export {number} rw confirm` to do it.");
                }
                _ => return crate::println!("{usage}"),
            };
            match export(number, writable) {
                Ok(()) => {
                    crate::println!("aoe: disk {number} exported {} as shelf {} slot {number}.", if writable { "READ-WRITE" } else { "read-only" }, shelf());
                    if writable {
                        crate::println!("     Do not mount it here or anywhere else while it is exported rw.");
                    }
                }
                Err(ExportError::NoSuchDisk) => crate::println!("aoe: no disk {number} (see `disks`)."),
                Err(ExportError::AlreadyExported) => crate::println!("aoe: disk {number} is already exported (`aoe unexport {number}` first)."),
                Err(ExportError::TableFull) => crate::println!("aoe: {MAX_EXPORTS} disks are exported already."),
                Err(ExportError::BootDiskWritable) => crate::println!("aoe: disk 1 is the boot disk and is never exported writable."),
                Err(ExportError::NoThread) => crate::println!("aoe: could not start the target thread."),
            }
        }
        Some("unexport") => match words.next().and_then(|word| word.parse::<usize>().ok()) {
            Some(number) if remove(number) => crate::println!("aoe: disk {number} is no longer exported."),
            Some(number) => crate::println!("aoe: disk {number} is not exported."),
            None => crate::println!("{usage}"),
        },
        Some("shelf") => match words.next().and_then(|word| word.parse::<u16>().ok()).filter(|&shelf| shelf != 0xffff) {
            Some(number) => {
                SHELF.store(u32::from(number), Ordering::Relaxed);
                for slot in SLOTS.iter() {
                    if let Some(export) = slot.export.lock().as_mut() {
                        export.target.shelf = number;
                    }
                }
                crate::println!("aoe: shelf {number}.");
            }
            None => crate::println!("aoe shelf N: N is 0..65534."),
        },
        Some(_) => crate::println!("{usage}"),
    }
}

/// Rings the doorbell of every NIC in `mask` (bit n: NIC n). The caller holds the NIC lock.
fn kick(mask: u32) {
    for nic in (0..32).filter(|nic| mask >> nic & 1 != 0) {
        nicctl::nic_kick(nic);
    }
}

/// TSC cycles per microsecond, measured against `time::micros` over 10 ms (only when printing the counters).
fn cycles_per_us() -> u64 {
    let (cycles, micros) = (crate::cpu::cycles(), crate::time::micros());
    crate::arch::busy_wait_ms(10);
    let elapsed = crate::time::micros().saturating_sub(micros).max(1);
    (crate::cpu::cycles().wrapping_sub(cycles) / elapsed).max(1)
}

fn list() {
    let rate = cycles_per_us();
    crate::println!("aoe: shelf {}, {} disk(s) exported.", shelf(), ACTIVE.load(Ordering::Acquire));
    for slot in SLOTS.iter() {
        let guard = slot.export.lock();
        let Some(export) = guard.as_ref() else { continue };
        let sectors = block::get(export.disk).map_or(0, |disk| disk.sectors);
        crate::println!(
            "  disk {}: slot {}, {} MiB, {}; {} reads, {} writes, {} errors, {} frame(s) dropped",
            export.disk + 1,
            export.target.slot,
            sectors >> 11,
            if export.target.read_only { "read-only" } else { "READ-WRITE" },
            export.target.reads,
            export.target.writes,
            export.target.errors,
            slot.inbox.lock().dropped
        );
        let stats = &slot.stats;
        let (wakes, frames) = (stats.wakes.load(Ordering::Relaxed), stats.frames.load(Ordering::Relaxed));
        let per = |total: &AtomicU64| total.load(Ordering::Relaxed) / frames.max(1) / rate;
        crate::println!("    worker: {wakes} wake-ups, {frames} frames ({} per wake-up)", frames / wakes.max(1));
        crate::println!("    queued to running {} us a wake-up", stats.wake_us.load(Ordering::Relaxed) / wakes.max(1) / rate);
        crate::println!("    per frame: disk lock {} us, serve {} us", per(&stats.lock_us), per(&stats.serve_us));
        crate::println!("      of which disk and reply {} us, send {} us", per(&stats.handle_us), per(&stats.send_us));
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        crate::println!("    disk commands: {} reads, {} frames from read-ahead", load(&stats.disk_reads), load(&stats.read_hits));
        crate::println!("    disk commands: {} writes, {} frames gathered", load(&stats.disk_writes), load(&stats.gathered));
        if load(&stats.write_errors) > 0 {
            crate::println!("    {} gathered write run(s) FAILED after they were acknowledged", load(&stats.write_errors));
        }
    }
    let rx = &net::RX_STATS;
    let wakes = rx.wakes.load(Ordering::Relaxed);
    crate::println!("  frames drained by net-rx {}, by the service thread {}", FROM_RX.load(Ordering::Relaxed), FROM_SERVICE.load(Ordering::Relaxed));
    crate::println!(
        "  net-rx: {} interrupts, {wakes} wake-ups, {} us to run (max {})",
        rx.events.load(Ordering::Relaxed),
        rx.latency_us.load(Ordering::Relaxed) / wakes.max(1) / rate,
        rx.latency_max_us.load(Ordering::Relaxed) / rate
    );
}
