//! The AoE initiator of the server kernel: a disk another machine exports over ATA over Ethernet (a fos host's `aoe export`,
//! Linux's `vblade`) becomes one more `diskN` here (`block::Backend::Aoe`), so `sector`, FOSFS, ext4 and the tapes' `vfs_*`
//! calls use it unchanged. The protocol and the bookkeeping of a transfer are `aoe::initiator`; this is the glue
//! (docs/block-sharing.md, "The initiator").
//!
//! **Threads.** There is none of its own. A block request runs in the caller's thread: it builds the frames, sends them
//! under the NIC lock (released at once), and sleeps on its remote's key until replies arrive or the next resend is due
//! (`sched::wait_while_for`). `frame_hook` only copies a reply into the queue of the remote it is from and wakes it, as it
//! does for the target: no protocol work runs with the NIC lock held. On a machine with a polled NIC (no interrupt, so
//! `net-rx` never wakes for it) the waiter drains the NICs itself every tick.
//!
//! **Time** is counted in timer ticks (10 ms): the first resend after `TIMEOUT_TICKS`, doubling, `MAX_TRIES` sends in all
//! (about 5 s) before the request fails as a device error. A LAN round trip is far below a tick, so this only decides how
//! quickly a lost frame is noticed, not throughput.
//!
//! **Safety.** Attach is read-only unless `rw` is given (writes then fail here, before the wire). Standard AoE has no
//! claim: nothing stops two hosts writing one disk, so the target's `rw confirm` and this `rw` are the two places a person
//! decides that. A target with one of this machine's own MACs is refused: it would be our own export, served by a worker
//! that needs the disk lock the waiting caller may hold.
//!
//! Requests to one remote are serial (its `SleepMutex`); each keeps up to `window` frames in flight, which is where the
//! speed comes from. Running requests side by side would gain little: `cp` and `sha256` ask for 1 MiB at a time, about
//! 1000 frames, so a request's empty start and draining end are a few percent of it. What counts is that the window is
//! larger than what the target's worker serves in one wake-up (measured: docs/block-sharing.md, "Speed").

use crate::sched::{self, SleepMutex};
use crate::sync::SpinLock;
use crate::{block, net};
use aoe::initiator::{self, Address, Failure, Kind, Op, Transfer};
use alloc::boxed::Box;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use kernel_core::{nic, nicctl};

const MAX_REMOTES: usize = 4;
/// Replies a remote's queue holds: above the largest window, so a full window of replies is never dropped here.
const QUEUE: usize = 64;
const FRAME: usize = 1600;
/// The most frames in flight per request, whatever the target's `bufcount` says (and never more than the queue holds).
const MAX_WINDOW: usize = 32;
const TIMEOUT_TICKS: u64 = 2;
const MAX_TRIES: u8 = 8;
/// How long `aoe discover` listens for Query Config replies.
const DISCOVER_TICKS: u64 = 50;
const MAX_FOUND: usize = 16;
const WAKE_KEY: usize = 0xa0e1_0000;

/// The fixed half of an attached disk, which the hook reads to route replies.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Route {
    mac: [u8; 6],
    shelf: u16,
    slot: u8,
}

struct Queue {
    route: Option<Route>,
    /// When the oldest reply in the queue arrived (TSC cycles), 0 while it is empty.
    arrived_at: u64,
    frames: [[u8; FRAME]; QUEUE],
    lengths: [u16; QUEUE],
    head: usize,
    count: usize,
    dropped: u64,
}

struct Remote {
    nic: usize,
    address: Address,
    per_frame: usize,
    window: usize,
    sectors: u64,
    writable: bool,
    /// Where frames are built and replies copied to before they are read (on the heap: 3 KiB is a lot for a caller's stack).
    out: Box<[u8; FRAME]>,
    rx: Box<[u8; FRAME]>,
}

struct Entry {
    inbox: SpinLock<Queue>,
    /// `None` while the entry is free or detached; held for a whole request.
    remote: SleepMutex<Option<Remote>>,
    /// The block-layer index this entry was registered as (it keeps it across detach and attach), `usize::MAX` for none.
    disk: AtomicU64,
    requests: AtomicU64,
    frames: AtomicU64,
    resent: AtomicU64,
    failed: AtomicU64,
    /// Times a request slept for replies, how many of those the timer ended with no reply waiting, and the time asleep
    /// (TSC cycles, converted when printed).
    waits: AtomicU64,
    timer_wakes: AtomicU64,
    waited: AtomicU64,
    /// When the last burst of frames was sent (TSC cycles), and the time from it to the first reply that came into an
    /// empty queue (the network and the target), summed with its count; and from a reply's arrival to the request
    /// taking it (this machine's wake-up), summed with its count.
    sent_at: AtomicU64,
    answer: AtomicU64,
    answers: AtomicU64,
    pickup: AtomicU64,
    pickups: AtomicU64,
}

impl Entry {
    const fn new() -> Entry {
        Entry {
            inbox: SpinLock::new(Queue { route: None, arrived_at: 0, frames: [[0; FRAME]; QUEUE], lengths: [0; QUEUE], head: 0, count: 0, dropped: 0 }),
            remote: SleepMutex::new(None),
            disk: AtomicU64::new(u64::MAX),
            requests: AtomicU64::new(0),
            frames: AtomicU64::new(0),
            resent: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            waits: AtomicU64::new(0),
            timer_wakes: AtomicU64::new(0),
            waited: AtomicU64::new(0),
            sent_at: AtomicU64::new(0),
            answer: AtomicU64::new(0),
            answers: AtomicU64::new(0),
            pickup: AtomicU64::new(0),
            pickups: AtomicU64::new(0),
        }
    }
}

static ENTRIES: [Entry; MAX_REMOTES] = [const { Entry::new() }; MAX_REMOTES];
/// Tags for the next transfer; each takes as many as it has frames, so a late reply to an earlier one never matches.
static TAGS: AtomicU32 = AtomicU32::new(0x1000);
/// Whether the hook should take 0x88a2 replies at all: something is attached or a discovery listens.
static WANTED: AtomicBool = AtomicBool::new(false);

/// A target that answered `aoe discover`.
#[derive(Clone, Copy)]
struct Found {
    nic: usize,
    mac: [u8; 6],
    shelf: u16,
    slot: u8,
    bufcount: u16,
    sectors: u8,
}

struct Discovery {
    listening: bool,
    found: [Option<Found>; MAX_FOUND],
}

static DISCOVERY: SpinLock<Discovery> = SpinLock::new(Discovery { listening: false, found: [None; MAX_FOUND] });

fn update_wanted() {
    let attached = ENTRIES.iter().any(|entry| entry.inbox.lock().route.is_some());
    WANTED.store(attached || DISCOVERY.lock().listening, Ordering::Release);
}

/// Whether `frame_hook` should hand AoE replies (the R flag) to `reply`.
pub fn wanted() -> bool {
    WANTED.load(Ordering::Acquire)
}

/// An AoE reply from the NIC (the caller checked the ethertype and the R flag). Runs under the NIC lock, possibly in the
/// SysRq rescue's interrupt context (`wake` false then): copy, route, wake; nothing else.
pub fn reply(nic: usize, frame: &[u8], wake: bool) {
    if frame.len() > FRAME {
        return;
    }
    let Some(response) = initiator::parse(frame) else { return };
    if let Kind::Config(config) = response.kind {
        let mut discovery = DISCOVERY.lock();
        if discovery.listening {
            let found = Found { nic, mac: response.source, shelf: response.shelf, slot: response.slot, bufcount: config.bufcount, sectors: config.sectors };
            let known = discovery.found.iter().flatten().any(|f| (f.mac, f.shelf, f.slot) == (found.mac, found.shelf, found.slot));
            if !known {
                if let Some(free) = discovery.found.iter_mut().find(|f| f.is_none()) {
                    *free = Some(found);
                }
            }
        }
        return;
    }
    let route = Route { mac: response.source, shelf: response.shelf, slot: response.slot };
    for (index, entry) in ENTRIES.iter().enumerate() {
        let mut queue = entry.inbox.lock();
        if queue.route != Some(route) {
            continue;
        }
        if queue.count == QUEUE {
            queue.dropped += 1;
        } else {
            if queue.count == 0 {
                let now = crate::cpu::cycles();
                queue.arrived_at = now;
                let sent = entry.sent_at.load(Ordering::Relaxed);
                if sent != 0 {
                    entry.answer.fetch_add(now.saturating_sub(sent), Ordering::Relaxed);
                    entry.answers.fetch_add(1, Ordering::Relaxed);
                }
            }
            let at = (queue.head + queue.count) % QUEUE;
            queue.frames[at][..frame.len()].copy_from_slice(frame);
            queue.lengths[at] = frame.len() as u16;
            queue.count += 1;
        }
        drop(queue);
        if wake {
            sched::wake_all(WAKE_KEY + index);
        }
        return;
    }
}

fn pop(entry: &Entry, out: &mut [u8; FRAME]) -> usize {
    let mut queue = entry.inbox.lock();
    if queue.count == 0 {
        return 0;
    }
    if queue.arrived_at != 0 {
        entry.pickup.fetch_add(crate::cpu::cycles().saturating_sub(queue.arrived_at), Ordering::Relaxed);
        entry.pickups.fetch_add(1, Ordering::Relaxed);
        queue.arrived_at = 0;
    }
    let at = queue.head;
    let length = usize::from(queue.lengths[at]);
    out[..length].copy_from_slice(&queue.frames[at][..length]);
    queue.head = (at + 1) % QUEUE;
    queue.count -= 1;
    length
}

fn is_empty(entry: &Entry) -> bool {
    entry.inbox.lock().count == 0
}

/// Runs one transfer on remote `index` to its end, in the calling thread. `buffer` takes what a read or IDENTIFY returns,
/// `data` is what a write sends.
fn run(index: usize, remote: &mut Remote, op: Op, lba: u64, sectors: u64, buffer: &mut [u8], data: &[u8]) -> Result<(), Failure> {
    let entry = &ENTRIES[index];
    let base = TAGS.fetch_add(initiator::frames_for(op, sectors, remote.per_frame) as u32 + 1, Ordering::Relaxed);
    let mut transfer = Transfer::new(op, lba, sectors, remote.per_frame, remote.window, base, TIMEOUT_TICKS, MAX_TRIES);
    // Replies to an earlier request that came after it ended: their tags cannot match, but they would sit in the queue.
    entry.inbox.lock().count = 0;
    entry.requests.fetch_add(1, Ordering::Relaxed);
    let polled = !nic::all_interrupt_driven();
    let result = loop {
        loop {
            let length = pop(entry, &mut remote.rx);
            if length == 0 {
                break;
            }
            if let Some(response) = initiator::parse(&remote.rx[..length]) {
                transfer.receive(&remote.address, &response, buffer);
            }
        }
        if let Some(result) = transfer.finished() {
            break result;
        }
        let now = crate::arch::ticks();
        {
            let _nics = net::lock_nics();
            let mut sent = false;
            loop {
                let length = transfer.next(now, &remote.address, data, &mut remote.out[..]);
                if length == 0 {
                    break;
                }
                nicctl::nic_send_deferred(remote.nic, &remote.out[..length]);
                entry.frames.fetch_add(1, Ordering::Relaxed);
                sent = true;
            }
            if sent {
                nicctl::nic_kick(remote.nic);
                entry.sent_at.store(crate::cpu::cycles(), Ordering::Relaxed);
            }
            if polled {
                nicctl::drain_nics_to_fifo();
            }
        }
        if transfer.finished().is_some() {
            continue;
        }
        let wait = if polled { 1 } else { transfer.deadline().map_or(1, |due| due.saturating_sub(now)) };
        if !is_empty(entry) {
            continue;
        }
        let asleep = crate::cpu::cycles();
        sched::wait_while_for(WAKE_KEY + index, wait, || is_empty(entry));
        entry.waited.fetch_add(crate::cpu::cycles().wrapping_sub(asleep), Ordering::Relaxed);
        entry.waits.fetch_add(1, Ordering::Relaxed);
        if is_empty(entry) {
            entry.timer_wakes.fetch_add(1, Ordering::Relaxed);
        }
    };
    entry.resent.fetch_add(u64::from(transfer.resent), Ordering::Relaxed);
    if result.is_err() {
        entry.failed.fetch_add(1, Ordering::Relaxed);
    }
    result
}

/// The remote behind `Backend::Aoe(index)`, locked for one request; `Device` when it is detached.
fn with_remote<T>(index: usize, work: impl FnOnce(&mut Remote) -> Result<T, block::Error>) -> Result<T, block::Error> {
    let mut guard = ENTRIES.get(index).ok_or(block::Error::NoSuchDisk)?.remote.lock();
    let remote = guard.as_mut().ok_or(block::Error::Device)?;
    work(remote)
}

pub fn read(index: usize, lba: u64, buffer: &mut [u8]) -> Result<(), block::Error> {
    with_remote(index, |remote| {
        let sectors = (buffer.len() / 512) as u64;
        run(index, remote, Op::Read, lba, sectors, buffer, &[]).map_err(|_| block::Error::Device)
    })
}

/// Writes, then (when `flush`) FLUSH CACHE EXT, which the target answers once the data is on its disk.
pub fn write(index: usize, lba: u64, data: &[u8], flush: bool) -> Result<(), block::Error> {
    with_remote(index, |remote| {
        if !remote.writable {
            return Err(block::Error::Device);
        }
        run(index, remote, Op::Write, lba, (data.len() / 512) as u64, &mut [], data).map_err(|_| block::Error::Device)?;
        if flush {
            run(index, remote, Op::Flush, 0, 0, &mut [], &[]).map_err(|_| block::Error::Device)?;
        }
        Ok(())
    })
}

pub fn flush(index: usize) -> Result<(), block::Error> {
    with_remote(index, |remote| {
        if !remote.writable {
            return Ok(());
        }
        run(index, remote, Op::Flush, 0, 0, &mut [], &[]).map_err(|_| block::Error::Device)
    })
}

/// Broadcasts Query Config on every NIC with a link and listens `DISCOVER_TICKS`; the targets that answered, in order.
fn discover() -> ([Option<Found>; MAX_FOUND], usize) {
    {
        let mut discovery = DISCOVERY.lock();
        discovery.listening = true;
        discovery.found = [None; MAX_FOUND];
    }
    update_wanted();
    let mut frame = [0u8; 64];
    let mut asked = 0;
    {
        let _nics = net::lock_nics();
        for index in 0..nic::count() {
            let Some(card) = nic::at(index) else { continue };
            if !card.link_up() {
                continue;
            }
            let length = initiator::query_config(&mut frame, card.mac(), 0xffff, 0xff, TAGS.fetch_add(1, Ordering::Relaxed));
            nicctl::nic_send(index, &frame[..length]);
            asked += 1;
        }
    }
    if asked > 0 {
        sched::sleep_ticks(DISCOVER_TICKS);
    }
    let found = {
        let mut discovery = DISCOVERY.lock();
        discovery.listening = false;
        discovery.found
    };
    update_wanted();
    (found, asked)
}

fn our_mac(mac: [u8; 6]) -> bool {
    let _nics = net::lock_nics();
    (0..nic::count()).any(|index| nic::at(index).is_some_and(|card| card.mac() == mac))
}

#[derive(Debug)]
pub enum AttachError {
    NotFound,
    Ours,
    Attached,
    TableFull,
    Identify(Failure),
    NoDiskNumber,
}

/// Attaches `shelf.slot` (found by a discovery run here): IDENTIFY for its size, then a disk number. Returns the 0-based
/// block index.
fn attach(shelf: u16, slot: u8, writable: bool) -> Result<usize, AttachError> {
    let (found, _) = discover();
    let target = found.iter().flatten().find(|f| f.shelf == shelf && f.slot == slot).copied().ok_or(AttachError::NotFound)?;
    if our_mac(target.mac) {
        return Err(AttachError::Ours);
    }
    let route = Route { mac: target.mac, shelf, slot };
    if ENTRIES.iter().any(|entry| entry.inbox.lock().route == Some(route)) {
        return Err(AttachError::Attached);
    }
    // The entry this target had before (it keeps its disk number), else one never used.
    let index = (0..MAX_REMOTES)
        .find(|&index| LAST_ROUTE.lock()[index] == Some(route))
        .or_else(|| (0..MAX_REMOTES).find(|&index| ENTRIES[index].disk.load(Ordering::Relaxed) == u64::MAX && LAST_ROUTE.lock()[index].is_none()))
        .ok_or(AttachError::TableFull)?;
    let mut ours = [0u8; 6];
    {
        let _nics = net::lock_nics();
        nicctl::nic_mac(target.nic, &mut ours);
    }
    let mtu = {
        let _nics = net::lock_nics();
        nic::at(target.nic).map_or(1500, |card| card.mtu().0)
    };
    // What the target takes (0 from old targets means 2), what our MTU carries, and what the reply queue's frames hold.
    let advertised = if target.sectors == 0 { 2 } else { usize::from(target.sectors) };
    let per_frame = advertised.min(aoe::max_sectors(mtu)).min((FRAME - aoe::DATA_AT) / 512).max(1);
    let window = usize::from(target.bufcount).clamp(1, MAX_WINDOW);
    let entry = &ENTRIES[index];
    let mut remote = Remote {
        nic: target.nic,
        address: Address { ours, target: target.mac, shelf, slot },
        per_frame,
        window,
        sectors: 0,
        writable,
        out: Box::new([0; FRAME]),
        rx: Box::new([0; FRAME]),
    };
    entry.inbox.lock().route = Some(route);
    update_wanted();
    let mut sector = [0u8; 512];
    let identified = run(index, &mut remote, Op::Identify, 0, 0, &mut sector, &[]);
    let geometry = match identified.map(|_| initiator::parse_identify(&sector)) {
        Ok(Some(geometry)) if geometry.sectors > 0 => geometry,
        Ok(_) => {
            detach_route(index);
            return Err(AttachError::Identify(Failure::Short));
        }
        Err(failure) => {
            detach_route(index);
            return Err(AttachError::Identify(failure));
        }
    };
    remote.sectors = geometry.sectors;
    // "e5.2 " and as much of the target's model as fits.
    let mut model = [b' '; 40];
    let name = alloc::format!("e{shelf}.{slot} ");
    let prefix = name.len().min(40);
    model[..prefix].copy_from_slice(&name.as_bytes()[..prefix]);
    let text = trim(&geometry.model);
    let room = (40 - prefix).min(text.len());
    model[prefix..prefix + room].copy_from_slice(&text[..room]);
    *entry.remote.lock() = Some(remote);
    LAST_ROUTE.lock()[index] = Some(route);
    let disk = match entry.disk.load(Ordering::Relaxed) {
        u64::MAX => block::register(block::Backend::Aoe(index), geometry.sectors, model, "aoe"),
        known => block::set_virtual(known as usize, geometry.sectors, model).then_some(known as usize),
    };
    match disk {
        Some(disk) => {
            entry.disk.store(disk as u64, Ordering::Relaxed);
            Ok(disk)
        }
        None => {
            *entry.remote.lock() = None;
            detach_route(index);
            Err(AttachError::NoDiskNumber)
        }
    }
}

/// The last target each entry was attached to, so attaching it again gives it its old disk number.
static LAST_ROUTE: SpinLock<[Option<Route>; MAX_REMOTES]> = SpinLock::new([None; MAX_REMOTES]);

fn trim(text: &[u8]) -> &[u8] {
    let end = text.iter().rposition(|&byte| byte != b' ' && byte != 0).map_or(0, |at| at + 1);
    &text[..end]
}

fn detach_route(index: usize) {
    let mut queue = ENTRIES[index].inbox.lock();
    queue.route = None;
    queue.count = 0;
    drop(queue);
    update_wanted();
}

/// Detaches `shelf.slot`: requests fail from now on (after the one in progress); the disk number stays for a later attach.
fn detach(shelf: u16, slot: u8) -> bool {
    for (index, entry) in ENTRIES.iter().enumerate() {
        let matches = entry.inbox.lock().route.is_some_and(|route| route.shelf == shelf && route.slot == slot);
        if matches {
            *entry.remote.lock() = None;
            detach_route(index);
            return true;
        }
    }
    false
}

fn parse_address(word: Option<&str>) -> Option<(u16, u8)> {
    let word = word?.trim_start_matches('e');
    let (shelf, slot) = word.split_once('.')?;
    let shelf = shelf.parse::<u16>().ok().filter(|&shelf| shelf != 0xffff)?;
    let slot = slot.parse::<u8>().ok().filter(|&slot| slot != 0xff)?;
    Some((shelf, slot))
}

fn mac_text(mac: [u8; 6]) -> alloc::string::String {
    alloc::format!("{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", mac[0], mac[1], mac[2], mac[3], mac[4], mac[5])
}

/// The `aoe` listing's part about attached disks. `rate` is TSC cycles per microsecond.
pub fn list(rate: u64) {
    for entry in ENTRIES.iter() {
        let Some(route) = entry.inbox.lock().route else { continue };
        let guard = entry.remote.lock();
        let Some(remote) = guard.as_ref() else { continue };
        crate::println!(
            "  attached e{}.{} as disk {}: {} MiB, {}, {}",
            route.shelf,
            route.slot,
            entry.disk.load(Ordering::Relaxed) + 1,
            remote.sectors >> 11,
            if remote.writable { "READ-WRITE" } else { "read-only" },
            mac_text(route.mac)
        );
        crate::println!("    nic {}, {} sector(s) a frame, window {}", remote.nic + 1, remote.per_frame, remote.window);
        crate::println!(
            "    {} requests, {} frames, {} resent, {} failed, {} dropped",
            entry.requests.load(Ordering::Relaxed),
            entry.frames.load(Ordering::Relaxed),
            entry.resent.load(Ordering::Relaxed),
            entry.failed.load(Ordering::Relaxed),
            entry.inbox.lock().dropped
        );
        let waits = entry.waits.load(Ordering::Relaxed);
        crate::println!(
            "    slept {} times, {} us each, {} woken by the timer",
            waits,
            entry.waited.load(Ordering::Relaxed) / waits.max(1) / rate.max(1),
            entry.timer_wakes.load(Ordering::Relaxed)
        );
        let (answers, pickups) = (entry.answers.load(Ordering::Relaxed), entry.pickups.load(Ordering::Relaxed));
        crate::println!(
            "    send to first reply {} us, reply to taken {} us",
            entry.answer.load(Ordering::Relaxed) / answers.max(1) / rate.max(1),
            entry.pickup.load(Ordering::Relaxed) / pickups.max(1) / rate.max(1)
        );
    }
}

/// `aoe discover`, `aoe attach SHELF.SLOT [rw]`, `aoe detach SHELF.SLOT`: true when `word` was one of them.
pub fn command(word: &str, words: &mut core::str::SplitWhitespace) -> bool {
    match word {
        "discover" => {
            let (found, asked) = discover();
            if asked == 0 {
                crate::println!("aoe: no network interface has a link.");
                return true;
            }
            let mut any = false;
            for target in found.iter().flatten() {
                any = true;
                crate::println!(
                    "  e{}.{}  {}  nic {}  {} sector(s) a frame, bufcount {}{}",
                    target.shelf,
                    target.slot,
                    mac_text(target.mac),
                    target.nic + 1,
                    if target.sectors == 0 { 2 } else { target.sectors },
                    target.bufcount,
                    if our_mac(target.mac) { "  (this machine)" } else { "" }
                );
            }
            if !any {
                crate::println!("aoe: no target answered on {asked} interface(s).");
            }
        }
        "attach" => {
            let Some((shelf, slot)) = parse_address(words.next()) else {
                crate::println!("aoe attach SHELF.SLOT [rw]   (`aoe discover` lists them)");
                return true;
            };
            let writable = match words.next() {
                None => false,
                Some("rw") => true,
                Some(_) => {
                    crate::println!("aoe attach SHELF.SLOT [rw]");
                    return true;
                }
            };
            match attach(shelf, slot, writable) {
                Ok(disk) => {
                    let sectors = block::get(disk).map_or(0, |info| info.sectors);
                    crate::println!("aoe: e{shelf}.{slot} is disk {}, {} MiB, {}.", disk + 1, sectors >> 11, if writable { "READ-WRITE" } else { "read-only" });
                    if writable {
                        crate::println!("     No other machine may write it while it is attached here.");
                    }
                }
                Err(AttachError::NotFound) => crate::println!("aoe: no target e{shelf}.{slot} answered (see `aoe discover`)."),
                Err(AttachError::Ours) => crate::println!("aoe: e{shelf}.{slot} is this machine's own export."),
                Err(AttachError::Attached) => crate::println!("aoe: e{shelf}.{slot} is already attached."),
                Err(AttachError::TableFull) => crate::println!("aoe: {MAX_REMOTES} disks are attached already."),
                Err(AttachError::Identify(failure)) => crate::println!("aoe: e{shelf}.{slot} did not answer IDENTIFY ({failure:?})."),
                Err(AttachError::NoDiskNumber) => crate::println!("aoe: no free disk number."),
            }
        }
        "detach" => match parse_address(words.next()) {
            Some((shelf, slot)) if detach(shelf, slot) => crate::println!("aoe: e{shelf}.{slot} detached; its disk number fails until it is attached again."),
            Some((shelf, slot)) => crate::println!("aoe: e{shelf}.{slot} is not attached."),
            None => crate::println!("aoe detach SHELF.SLOT"),
        },
        _ => return false,
    }
    true
}
