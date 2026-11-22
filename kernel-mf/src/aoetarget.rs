//! The AoE target of the server kernel: serves disks to Linux (`aoe` module, `vblade`'s initiators) and to other fos hosts
//! as ATA over Ethernet (ethertype 0x88a2). The protocol is `crates/aoe`; this is the glue (docs/block-sharing.md).
//!
//! Frames never reach a disk from the receive path. `frame_hook` runs with the NIC lock held, and a disk read there would
//! stall every interface (and the audio drivers that share the lock), so it only copies the frame into a small queue and
//! wakes the `aoe` thread. That thread asks the disk, builds the reply and sends it. A full queue drops the frame: the
//! initiator retransmits, as it would for a frame lost on the wire.
//!
//! Writes go to the drive with `block::write_deferred` and are flushed when the queue runs empty (the initiator paused) and
//! on FLUSH CACHE; the IDENTIFY data says there is a write cache, which is what lets the initiator rely on that.
//!
//! Exports are read-only unless `rw confirm` is given, and disk 1 (the boot disk) is never writable. Nothing stops a second
//! writer: do not mount an exported disk here or on another host while it is exported `rw`.

use crate::sched::{self, SleepMutex};
use crate::sync::SpinLock;
use crate::{block, net};
use aoe::{Disk, Target};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use kernel_core::nicctl;

const MAX_EXPORTS: usize = 8;
const QUEUE: usize = 32;
const FRAME: usize = 1600;
/// The Ethernet payload size the target assumes: jumbo frames do not reach the receive path yet (`nicctl` drains into
/// 1600-byte buffers), so it advertises 2 sectors per frame.
const MTU: usize = 1500;
/// What `aoe-shelf` and the shell leave to the machine: derived from NIC 1's MAC.
const AUTO_SHELF: u32 = u32::MAX;
const WAKE_KEY: usize = 0xa0e0_0001;

struct Export {
    /// 0-based index in the block layer; the slot is this plus one, the disk number `disks` shows.
    disk: usize,
    target: Target,
    /// Written since the last flush.
    dirty: bool,
}

static EXPORTS: SleepMutex<[Option<Export>; MAX_EXPORTS]> = SleepMutex::new([const { None }; MAX_EXPORTS]);
static SHELF: AtomicU32 = AtomicU32::new(AUTO_SHELF);
static RUNNING: AtomicBool = AtomicBool::new(false);
/// True while at least one disk is exported: only then does `frame_hook` take 0x88a2 frames (otherwise a tape may have them).
static ACTIVE: AtomicBool = AtomicBool::new(false);

struct Queue {
    frames: [[u8; FRAME]; QUEUE],
    lengths: [u16; QUEUE],
    nics: [u8; QUEUE],
    head: usize,
    count: usize,
    dropped: u64,
}

static INBOX: SpinLock<Queue> = SpinLock::new(Queue { frames: [[0; FRAME]; QUEUE], lengths: [0; QUEUE], nics: [0; QUEUE], head: 0, count: 0, dropped: 0 });

// The `aoe` thread is the only user of these two.
static mut REQUEST: [u8; FRAME] = [0; FRAME];
static mut REPLY: [u8; FRAME] = [0; FRAME];

/// Whether `frame_hook` should hand 0x88a2 frames to this module.
pub fn wanted() -> bool {
    ACTIVE.load(Ordering::Acquire)
}

/// Queues a received AoE frame for the thread. Runs under the NIC lock and possibly in the SysRq rescue's interrupt context:
/// no disk, no sleeping; the wake-up is left out when `wake` is false (the thread polls once more when it next runs).
pub fn enqueue(nic: usize, frame: &[u8], wake: bool) {
    if frame.len() > FRAME {
        return;
    }
    let pushed = {
        let mut queue = INBOX.lock();
        if queue.count == QUEUE {
            queue.dropped += 1;
            false
        } else {
            let at = (queue.head + queue.count) % QUEUE;
            queue.frames[at][..frame.len()].copy_from_slice(frame);
            queue.lengths[at] = frame.len() as u16;
            queue.nics[at] = nic as u8;
            queue.count += 1;
            true
        }
    };
    if pushed && wake {
        sched::wake_all(WAKE_KEY);
    }
}

/// Takes the oldest queued frame into `REQUEST`; (nic, length).
fn pop() -> Option<(usize, usize)> {
    let mut queue = INBOX.lock();
    if queue.count == 0 {
        return None;
    }
    let head = queue.head;
    let length = usize::from(queue.lengths[head]);
    let request = unsafe { &mut *core::ptr::addr_of_mut!(REQUEST) };
    request[..length].copy_from_slice(&queue.frames[head][..length]);
    let nic = usize::from(queue.nics[head]);
    queue.head = (head + 1) % QUEUE;
    queue.count -= 1;
    Some((nic, length))
}

/// The block layer as the target's disk (512-byte sectors, bounds already checked by the target and by `block`).
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

extern "C" fn thread(_: usize) {
    loop {
        match pop() {
            Some((nic, length)) => serve(nic, length),
            None => {
                flush_dirty();
                sched::wait_while(WAKE_KEY, || INBOX.lock().count == 0);
            }
        }
    }
}

fn serve(nic: usize, length: usize) {
    let Some(mac) = net::mac(nic) else { return };
    let request = unsafe { &*core::ptr::addr_of!(REQUEST) };
    let request = &request[..length];
    let reply = unsafe { &mut *core::ptr::addr_of_mut!(REPLY) };
    let mut exports = EXPORTS.lock();
    for export in exports.iter_mut().flatten() {
        export.target.mac = mac;
        let writes = export.target.writes;
        let reply_length = export.target.handle(request, &mut BlockDisk(export.disk), reply);
        export.dirty |= export.target.writes != writes;
        if reply_length > 0 {
            let _nics = net::lock_nics();
            nicctl::nic_send(nic, &reply[..reply_length]);
        }
    }
}

/// The initiator paused: put what it wrote on the platters (or flash).
fn flush_dirty() {
    let mut exports = EXPORTS.lock();
    for export in exports.iter_mut().flatten() {
        if export.dirty {
            export.dirty = false;
            if block::flush(export.disk).is_err() {
                crate::println!("aoe: flushing disk {} failed.", export.disk + 1);
            }
        }
    }
}

fn start_thread() -> bool {
    if RUNNING.swap(true, Ordering::AcqRel) {
        return true;
    }
    if sched::spawn("aoe", thread, 0).is_none() {
        RUNNING.store(false, Ordering::Release);
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
    {
        let mut exports = EXPORTS.lock();
        if exports.iter().flatten().any(|export| export.disk == disk) {
            return Err(ExportError::AlreadyExported);
        }
        let Some(free) = exports.iter_mut().find(|entry| entry.is_none()) else { return Err(ExportError::TableFull) };
        let mut target = Target::new(shelf(), number as u8, [0; 6], MTU);
        target.read_only = !writable;
        if let Some(info) = block::get(disk) {
            let model = core::str::from_utf8(&info.model).unwrap_or("").trim();
            aoe::set_text(&mut target.model, model.as_bytes());
        }
        let serial = alloc::format!("FOS{:04X}{:02X}", shelf(), number);
        aoe::set_text(&mut target.serial, serial.as_bytes());
        *free = Some(Export { disk, target, dirty: false });
    }
    if !start_thread() {
        remove(number);
        return Err(ExportError::NoThread);
    }
    ACTIVE.store(true, Ordering::Release);
    Ok(())
}

/// Stops exporting disk `number`; false when it was not exported. What it had queued is still answered or dropped.
pub fn remove(number: usize) -> bool {
    let mut exports = EXPORTS.lock();
    let mut found = false;
    for entry in exports.iter_mut() {
        if entry.as_ref().is_some_and(|export| export.disk + 1 == number) {
            if let Some(export) = entry.as_ref().filter(|export| export.dirty) {
                let _ = block::flush(export.disk);
            }
            *entry = None;
            found = true;
        }
    }
    ACTIVE.store(exports.iter().any(|entry| entry.is_some()), Ordering::Release);
    found
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
    let usage = "aoe [export N [rw confirm] | unexport N | shelf N]: serve disk N to Linux and other fos hosts (ATA over Ethernet).";
    match words.next() {
        None => list(),
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
                for export in EXPORTS.lock().iter_mut().flatten() {
                    export.target.shelf = number;
                }
                crate::println!("aoe: shelf {number}.");
            }
            None => crate::println!("aoe shelf N: N is 0..65534."),
        },
        Some(_) => crate::println!("{usage}"),
    }
}

fn list() {
    let exports = EXPORTS.lock();
    crate::println!("aoe: shelf {}, {} disk(s) exported.", shelf(), exports.iter().flatten().count());
    for export in exports.iter().flatten() {
        let sectors = block::get(export.disk).map_or(0, |disk| disk.sectors);
        crate::println!(
            "  disk {}: slot {}, {} MiB, {}; {} reads, {} writes, {} errors",
            export.disk + 1,
            export.target.slot,
            sectors >> 11,
            if export.target.read_only { "read-only" } else { "READ-WRITE" },
            export.target.reads,
            export.target.writes,
            export.target.errors
        );
    }
    let dropped = INBOX.lock().dropped;
    if dropped > 0 {
        crate::println!("  {dropped} frame(s) dropped because the queue was full.");
    }
}
