//! The AoE initiator of the server kernel: a disk another machine exports over ATA over Ethernet (a fos host's `aoe export`,
//! Linux's `vblade`) becomes one more `diskN` here (`block::Backend::Aoe`), so `sector`, FOSFS, ext4 and the tapes' `vfs_*`
//! calls use it unchanged. The protocol and the bookkeeping of a transfer are `aoe::initiator`; this is the glue
//! (docs/block-sharing.md, "The initiator").
//!
//! **Threads.** A block request runs in the caller's thread: it builds the frames, sends them
//! under the NIC lock (released at once), and sleeps on its remote's key until replies arrive or the next resend is due
//! (`sched::wait_while_for`). `frame_hook` only copies a reply into the queue of the remote it is from and wakes it, as it
//! does for the target: no protocol work runs with the NIC lock held. On a machine with a polled NIC (no interrupt, so
//! `net-rx` never wakes for it) the waiter drains the NICs itself every tick. Between requests an `aoe-ahead` thread per
//! remote carries on with the read-ahead (below); replies wake it instead of a caller while no request runs.
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
//! speed comes from, and that the window is larger than what the target's worker serves in one wake-up (measured:
//! docs/block-sharing.md, "Speed"). Smaller reads (ext4 asks for about 122 KB at a time) would each start with an empty
//! window and end draining it, and leave the wire idle between them, so a read that continues the previous one also asks
//! for up to four window-sized chunks past its end (`aoe::readahead`). The `aoe-ahead` thread takes their replies and
//! sends the rest while the caller works on what it got, and the next read finds them there. Reads elsewhere and every
//! write or flush leave or drop them as that module explains.

use crate::sched::{self, SleepMutex};
use crate::sync::SpinLock;
use crate::{block, net};
use aoe::initiator::{self, Address, Failure, Kind, Op, Response, Transfer};
use aoe::readahead::ReadAhead;
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use kernel_core::{nic, nicctl};

const MAX_REMOTES: usize = 4;
/// Replies a remote's queue holds: above the largest window, so a full window of replies is never dropped here.
const QUEUE: usize = 128;
const FRAME: usize = 1600;
/// The most frames in flight per request, whatever the target's `bufcount` says (and never more than the queue holds).
const MAX_WINDOW: usize = 64;
const TIMEOUT_TICKS: u64 = 2;
const MAX_TRIES: u8 = 8;
/// Read-ahead not used for longer than this is dropped (between 20 and 30 ms): the bound on serving sectors another host
/// has changed since, the same as the target's cache.
const STALE_TICKS: u64 = 2;
/// How long `aoe discover` listens for Query Config replies.
const DISCOVER_TICKS: u64 = 50;
const MAX_FOUND: usize = 16;
/// How long the boot waits for `aoe-attach` targets (link, then an answer) before `start` runs: 10 s. What has not
/// answered by then is looked for by the `aoe-attach` thread every `RETRY_TICKS` (10 s), for as long as it takes.
const BOOT_WAIT_TICKS: u64 = 1000;
const RETRY_TICKS: u64 = 1000;
const WAKE_KEY: usize = 0xa0e1_0000;
/// What the `aoe-ahead` thread of each remote sleeps on.
const AHEAD_KEY: usize = 0xa0e2_0000;

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
    /// Sequential reads' read-ahead (`aoe::readahead`) and the chunks it fills (256 KiB at 2 sectors a frame, window 64).
    ahead: Box<ReadAhead>,
    store: Box<[u8]>,
}

struct Entry {
    inbox: SpinLock<Queue>,
    /// `None` while the entry is free or detached; held for a whole request.
    /// Boxed so that `None` is a null pointer (an `Option<Remote>` may mark it with a non-zero byte, see `disk`).
    remote: SleepMutex<Option<Box<Remote>>>,
    /// A request is running (its caller takes the replies); otherwise the `aoe-ahead` thread does. `ahead_started`: that
    /// thread exists (one per entry, started at the first attach, kept).
    busy: AtomicBool,
    ahead_started: AtomicBool,
    /// A block request holds `remote`: the thread sleeps until it is let go (`with_remote` wakes it), never spins on it.
    held: AtomicBool,
    /// The `aoe-ahead` thread: times it ran, found the remote taken, and frames it sent.
    ahead_runs: AtomicU64,
    ahead_locked_out: AtomicU64,
    ahead_frames: AtomicU64,
    /// The block-layer index this entry was registered as, plus one (it keeps it across detach and attach); 0 for none.
    /// Zero rather than `u64::MAX` for "none" keeps `ENTRIES` all zeros, so its 400 KiB of buffers sit in `.bss` and not
    /// in the kernel image.
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
            busy: AtomicBool::new(false),
            ahead_started: AtomicBool::new(false),
            held: AtomicBool::new(false),
            ahead_runs: AtomicU64::new(0),
            ahead_locked_out: AtomicU64::new(0),
            ahead_frames: AtomicU64::new(0),
            disk: AtomicU64::new(0),
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

    /// Clears the counters `aoe` shows: they belong to one attach, not to the table entry, so a target attached again
    /// (the same entry, the same disk number) starts at 0 instead of showing an earlier attach's failures.
    fn reset_counters(&self) {
        for counter in [
            &self.ahead_runs,
            &self.ahead_locked_out,
            &self.ahead_frames,
            &self.requests,
            &self.frames,
            &self.resent,
            &self.failed,
            &self.waits,
            &self.timer_wakes,
            &self.waited,
            &self.answer,
            &self.answers,
            &self.pickup,
            &self.pickups,
        ] {
            counter.store(0, Ordering::Relaxed);
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
            let busy = entry.busy.load(Ordering::Acquire);
            sched::wake_all(if busy { WAKE_KEY } else { AHEAD_KEY } + index);
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

/// What `drive` runs to its end: one transfer, or a read with its read-ahead.
trait Work {
    fn next(&mut self, now: u64, to: &Address, out: &mut [u8]) -> usize;
    fn receive(&mut self, to: &Address, response: &Response) -> bool;
    fn finished(&self) -> Option<Result<(), Failure>>;
    fn deadline(&self) -> Option<u64>;
    /// Resends so far, for the counters.
    fn resent(&self) -> u64;
}

struct Single<'a> {
    transfer: Transfer,
    buffer: &'a mut [u8],
    data: &'a [u8],
}

impl Work for Single<'_> {
    fn next(&mut self, now: u64, to: &Address, out: &mut [u8]) -> usize {
        self.transfer.next(now, to, self.data, out)
    }
    fn receive(&mut self, to: &Address, response: &Response) -> bool {
        self.transfer.receive(to, response, self.buffer)
    }
    fn finished(&self) -> Option<Result<(), Failure>> {
        self.transfer.finished()
    }
    fn deadline(&self) -> Option<u64> {
        self.transfer.deadline()
    }
    fn resent(&self) -> u64 {
        u64::from(self.transfer.resent)
    }
}

struct Streamed<'a> {
    ahead: &'a mut ReadAhead,
    buffer: &'a mut [u8],
    store: &'a mut [u8],
}

impl Work for Streamed<'_> {
    fn next(&mut self, now: u64, to: &Address, out: &mut [u8]) -> usize {
        self.ahead.next(now, to, out)
    }
    fn receive(&mut self, to: &Address, response: &Response) -> bool {
        self.ahead.receive(to, response, self.buffer, self.store)
    }
    fn finished(&self) -> Option<Result<(), Failure>> {
        self.ahead.finished()
    }
    fn deadline(&self) -> Option<u64> {
        self.ahead.deadline()
    }
    fn resent(&self) -> u64 {
        self.ahead.resent()
    }
}

/// Tags for a transfer of `frames` frames; one spare, so the next transfer's range never touches this one's.
fn take_tags(frames: u64) -> u32 {
    TAGS.fetch_add(frames as u32 + 1, Ordering::Relaxed)
}

/// Runs `work` on remote `index` to its end, in the calling thread. `fresh` empties the reply queue first (replies to an
/// earlier request that came after it ended); a read with read-ahead keeps it, since replies to its chunks wait there.
fn drive(index: usize, nic: usize, address: &Address, out: &mut [u8; FRAME], rx: &mut [u8; FRAME], work: &mut dyn Work, fresh: bool) -> Result<(), Failure> {
    let entry = &ENTRIES[index];
    if fresh {
        entry.inbox.lock().count = 0;
    }
    entry.requests.fetch_add(1, Ordering::Relaxed);
    entry.busy.store(true, Ordering::Release);
    let resent_before = work.resent();
    let polled = !nic::all_interrupt_driven();
    let result = loop {
        take_replies(entry, address, rx, work);
        if let Some(result) = work.finished() {
            break result;
        }
        let now = crate::arch::ticks();
        send_frames(entry, nic, address, out, work, now, polled);
        if work.finished().is_some() {
            continue;
        }
        let wait = if polled { 1 } else { work.deadline().map_or(1, |due| due.saturating_sub(now)) };
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
    entry.resent.fetch_add(work.resent().saturating_sub(resent_before), Ordering::Relaxed);
    if result.is_err() {
        entry.failed.fetch_add(1, Ordering::Relaxed);
    }
    entry.busy.store(false, Ordering::Release);
    result
}

/// Hands every queued reply to `work`.
fn take_replies(entry: &Entry, address: &Address, rx: &mut [u8; FRAME], work: &mut dyn Work) {
    loop {
        let length = pop(entry, rx);
        if length == 0 {
            return;
        }
        if let Some(response) = initiator::parse(&rx[..length]) {
            work.receive(address, &response);
        }
    }
}

/// Sends what `work` has to send at `now` under the NIC lock, with one doorbell for the burst.
fn send_frames(entry: &Entry, nic: usize, address: &Address, out: &mut [u8; FRAME], work: &mut dyn Work, now: u64, polled: bool) {
    let _nics = net::lock_nics();
    let mut sent = false;
    loop {
        let length = work.next(now, address, &mut out[..]);
        if length == 0 {
            break;
        }
        nicctl::nic_send_deferred(nic, &out[..length]);
        entry.frames.fetch_add(1, Ordering::Relaxed);
        sent = true;
    }
    if sent {
        nicctl::nic_kick(nic);
        entry.sent_at.store(crate::cpu::cycles(), Ordering::Relaxed);
    }
    if polled {
        nicctl::drain_nics_to_fifo();
    }
}

/// The `aoe-ahead` thread of entry `index`: while no request runs, it takes the replies to read-ahead and sends the
/// rest of it, so a reader that works on what it got (a tape decoding, a hash over each block) finds the next sectors
/// already here. It only takes the remote with `try_lock` and sleeps while a request holds it (`with_remote` wakes it
/// when it lets go): spinning on it took a CPU from the very request it waited for, and under QEMU frames timed out.
/// Otherwise it sleeps until a reply comes, a tick at most while read-ahead is in flight (for resends).
extern "C" fn ahead_thread(index: usize) {
    let entry = &ENTRIES[index];
    loop {
        let Some(mut guard) = entry.remote.try_lock() else {
            entry.ahead_locked_out.fetch_add(1, Ordering::Relaxed);
            sched::wait_while_for(AHEAD_KEY + index, 1, || entry.held.load(Ordering::Acquire));
            continue;
        };
        entry.ahead_runs.fetch_add(1, Ordering::Relaxed);
        let active = match guard.as_mut() {
            None => false,
            Some(remote) => {
                let Remote { nic, address, out, rx, ahead, store, .. } = &mut **remote;
                let mut work = Streamed { ahead, buffer: &mut [], store };
                take_replies(entry, address, rx, &mut work);
                if work.ahead.active() {
                    let polled = !nic::all_interrupt_driven();
                    let before = entry.frames.load(Ordering::Relaxed);
                    send_frames(entry, *nic, address, out, &mut work, crate::arch::ticks(), polled);
                    entry.ahead_frames.fetch_add(entry.frames.load(Ordering::Relaxed) - before, Ordering::Relaxed);
                }
                work.ahead.active()
            }
        };
        drop(guard);
        let ticks = if active { 1 } else { 100 };
        sched::wait_while_for(AHEAD_KEY + index, ticks, || is_empty(entry) || entry.held.load(Ordering::Acquire));
    }
}

/// Runs one transfer (a write, a flush, IDENTIFY) on remote `index`. `buffer` takes what IDENTIFY returns, `data` is
/// what a write sends. Read-ahead is dropped first, since after our own write it could hold old sectors, and its frames
/// still in flight are waited for: they sit in the target's queue, and a write burst on top of them would overflow it
/// (seen under QEMU as resends after every switch from reading to writing).
fn run(index: usize, remote: &mut Remote, op: Op, lba: u64, sectors: u64, buffer: &mut [u8], data: &[u8]) -> Result<(), Failure> {
    remote.ahead.drain();
    if remote.ahead.finished().is_none() {
        let Remote { nic, address, out, rx, ahead, store, .. } = &mut *remote;
        // Never fails: an orphan whose reply is lost is let go at its deadline.
        let _ = drive(index, *nic, address, out, rx, &mut Streamed { ahead, buffer: &mut [], store }, false);
    }
    remote.ahead.finish(&mut [], &remote.store);
    let transfer = Transfer::new(op, lba, sectors, remote.per_frame, remote.window, take_tags(initiator::frames_for(op, sectors, remote.per_frame)), TIMEOUT_TICKS, MAX_TRIES);
    let mut work = Single { transfer, buffer, data };
    drive(index, remote.nic, &remote.address, &mut remote.out, &mut remote.rx, &mut work, true)
}

/// The remote behind `Backend::Aoe(index)`, locked for one request; `Device` when it is detached.
/// Afterwards the remote is the `aoe-ahead` thread's: it is woken once the lock is free (woken before, it found the remote
/// taken and slept until the next tick, missing the reader's whole pause: seen on aneby as a quarter of the read-ahead
/// sent by the thread).
fn with_remote<T>(index: usize, work: impl FnOnce(&mut Remote) -> Result<T, block::Error>) -> Result<T, block::Error> {
    let entry = ENTRIES.get(index).ok_or(block::Error::NoSuchDisk)?;
    let result = {
        let mut guard = entry.remote.lock();
        entry.held.store(true, Ordering::Release);
        let result = guard.as_deref_mut().ok_or(block::Error::Device).and_then(work);
        entry.held.store(false, Ordering::Release);
        result
    };
    sched::wake_all(AHEAD_KEY + index);
    result
}

pub fn read(index: usize, lba: u64, buffer: &mut [u8]) -> Result<(), block::Error> {
    with_remote(index, |remote| {
        let sectors = (buffer.len() / 512) as u64;
        let Remote { nic, address, out, rx, ahead, store, .. } = remote;
        ahead.begin(lba, sectors, crate::arch::ticks(), &mut take_tags);
        let result = drive(index, *nic, address, out, rx, &mut Streamed { ahead, buffer: &mut *buffer, store }, false);
        match result {
            Ok(()) => ahead.finish(buffer, store),
            Err(_) => ahead.clear(),
        }
        result.map_err(|_| block::Error::Device)
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
    // One listener at a time: the `aoe-attach` thread and the shell share `DISCOVERY`.
    let _one = DISCOVERING.lock();
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

/// Serialises discovery runs (`discover`).
static DISCOVERING: SleepMutex<()> = SleepMutex::new(());
/// Serialises attaches: picking a free entry and registering its disk is not safe twice at once.
static ATTACHING: SleepMutex<()> = SleepMutex::new(());

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
/// block index. A target with several NICs answers on each; `via` picks the one to use by its MAC, else the first answer is.
fn attach(shelf: u16, slot: u8, writable: bool, via: Option<[u8; 6]>) -> Result<usize, AttachError> {
    let _one = ATTACHING.lock();
    let (found, _) = discover();
    let target = found
        .iter()
        .flatten()
        .find(|f| f.shelf == shelf && f.slot == slot && via.is_none_or(|mac| mac == f.mac))
        .copied()
        .ok_or(AttachError::NotFound)?;
    if our_mac(target.mac) {
        return Err(AttachError::Ours);
    }
    let route = Route { mac: target.mac, shelf, slot };
    // By shelf.slot, not MAC: a target with several NICs answers on each, and the same disk attached twice through two of
    // them would be two disks here (two writers, with `rw`). Through another NIC it is still the same disk number.
    let same = |other: Option<Route>| other.is_some_and(|other| (other.shelf, other.slot) == (shelf, slot));
    if ENTRIES.iter().any(|entry| same(entry.inbox.lock().route)) {
        return Err(AttachError::Attached);
    }
    // The entry this target had before (it keeps its disk number), else one never used.
    let index = (0..MAX_REMOTES)
        .find(|&index| same(LAST_ROUTE.lock()[index]))
        .or_else(|| (0..MAX_REMOTES).find(|&index| ENTRIES[index].disk.load(Ordering::Relaxed) == 0 && LAST_ROUTE.lock()[index].is_none()))
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
    entry.reset_counters();
    let mut remote = Remote {
        nic: target.nic,
        address: Address { ours, target: target.mac, shelf, slot },
        per_frame,
        window,
        sectors: 0,
        writable,
        out: Box::new([0; FRAME]),
        rx: Box::new([0; FRAME]),
        ahead: Box::new(ReadAhead::new(per_frame, window, TIMEOUT_TICKS, MAX_TRIES, 0, STALE_TICKS)),
        store: Box::new([]),
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
    *remote.ahead = ReadAhead::new(per_frame, window, TIMEOUT_TICKS, MAX_TRIES, geometry.sectors, STALE_TICKS);
    remote.store = alloc::vec![0u8; remote.ahead.store_bytes()].into_boxed_slice();
    // "e5.2 " and as much of the target's model as fits.
    let mut model = [b' '; 40];
    let name = alloc::format!("e{shelf}.{slot} ");
    let prefix = name.len().min(40);
    model[..prefix].copy_from_slice(&name.as_bytes()[..prefix]);
    let text = trim(&geometry.model);
    let room = (40 - prefix).min(text.len());
    model[prefix..prefix + room].copy_from_slice(&text[..room]);
    *entry.remote.lock() = Some(Box::new(remote));
    LAST_ROUTE.lock()[index] = Some(route);
    let disk = match entry.disk.load(Ordering::Relaxed) {
        0 => block::register(block::Backend::Aoe(index), geometry.sectors, model, "aoe"),
        known => block::set_virtual(known as usize - 1, geometry.sectors, model).then_some(known as usize - 1),
    };
    match disk {
        Some(disk) => {
            entry.disk.store(disk as u64 + 1, Ordering::Relaxed);
            forget_mount();
            // Without it read-ahead still works, only while a read runs.
            if !entry.ahead_started.swap(true, Ordering::AcqRel) && sched::spawn_any("aoe-ahead", ahead_thread, index).is_none() {
                entry.ahead_started.store(false, Ordering::Release);
            }
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

/// Drops the ext volume this kernel keeps mounted (`ext4vol::shared`), committing what it has pending first. Attaching a
/// target again gives it the same disk number, and the kept mount is only re-read when the number changes, so without this
/// a re-attached disk would list the folders it had before (seen on hardware: a file written on the target meanwhile was
/// missing until `disks` happened to remount). Under the disk lock, like every other use of that state.
fn forget_mount() {
    let _disk = crate::tapefs::lock();
    let _ = kernel_core::ext4vol::commit_pending();
    kernel_core::ext4vol::forget_shared();
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
            forget_mount();
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

fn parse_mac(word: &str) -> Option<[u8; 6]> {
    let mut mac = [0u8; 6];
    let mut parts = word.split(':');
    for byte in mac.iter_mut() {
        *byte = u8::from_str_radix(parts.next()?, 16).ok()?;
    }
    parts.next().is_none().then_some(mac)
}

fn mac_text(mac: [u8; 6]) -> alloc::string::String {
    alloc::format!("{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", mac[0], mac[1], mac[2], mac[3], mac[4], mac[5])
}

/// The `aoe` listing's part about attached disks. `rate` is TSC cycles per microsecond.
pub fn list(rate: u64) {
    // A copy: no printing with the spin lock held.
    let pending: Vec<Wanted> = PENDING.lock().clone();
    for wanted in &pending {
        crate::println!("  waiting for e{}.{} (settings; looked for every {} s)", wanted.shelf, wanted.slot, RETRY_TICKS / 100);
    }
    for entry in ENTRIES.iter() {
        let Some(route) = entry.inbox.lock().route else { continue };
        let guard = entry.remote.lock();
        let Some(remote) = guard.as_ref() else { continue };
        crate::println!(
            "  attached e{}.{} as disk {}: {} MiB, {}, {}",
            route.shelf,
            route.slot,
            entry.disk.load(Ordering::Relaxed),
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
        crate::println!("    read ahead {} KiB, of it used {} KiB", remote.ahead.fetched / 2, remote.ahead.used / 2);
        crate::println!(
            "    aoe-ahead: ran {}, sent {} frames, found it taken {}",
            entry.ahead_runs.load(Ordering::Relaxed),
            entry.ahead_frames.load(Ordering::Relaxed),
            entry.ahead_locked_out.load(Ordering::Relaxed)
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

/// A target to attach, as `aoe attach` and `aoe-attach` in `settings` give it: `SHELF.SLOT [rw] [MAC]`.
#[derive(Clone, Copy)]
pub struct Wanted {
    pub shelf: u16,
    pub slot: u8,
    pub writable: bool,
    /// Which of the target's NICs to use, by its MAC (a target with several answers on each).
    pub via: Option<[u8; 6]>,
}

/// `SHELF.SLOT [rw] [MAC]`; None when the words are something else.
pub fn parse_wanted<'a>(mut words: impl Iterator<Item = &'a str>) -> Option<Wanted> {
    let (shelf, slot) = parse_address(words.next())?;
    let mut wanted = Wanted { shelf, slot, writable: false, via: None };
    for word in words {
        match word {
            "rw" => wanted.writable = true,
            _ => wanted.via = Some(parse_mac(word)?),
        }
    }
    Some(wanted)
}

fn say_attached(wanted: &Wanted, disk: usize) {
    let (shelf, slot) = (wanted.shelf, wanted.slot);
    let sectors = block::get(disk).map_or(0, |info| info.sectors);
    let mode = if wanted.writable { "READ-WRITE" } else { "read-only" };
    crate::println!("aoe: e{shelf}.{slot} is disk {}, {} MiB, {mode}.", disk + 1, sectors >> 11);
    if wanted.writable {
        crate::println!("     No other machine may write it while it is attached here.");
    }
}

fn say_failed(wanted: &Wanted, error: &AttachError) {
    let (shelf, slot) = (wanted.shelf, wanted.slot);
    match error {
        AttachError::NotFound => {
            let from = if wanted.via.is_some() { " from that MAC" } else { "" };
            crate::println!("aoe: no target e{shelf}.{slot} answered{from} (see `aoe discover`).");
        }
        AttachError::Ours => crate::println!("aoe: e{shelf}.{slot} is this machine's own export."),
        AttachError::Attached => crate::println!("aoe: e{shelf}.{slot} is already attached."),
        AttachError::TableFull => crate::println!("aoe: {MAX_REMOTES} disks are attached already."),
        AttachError::Identify(failure) => crate::println!("aoe: e{shelf}.{slot} did not answer IDENTIFY ({failure:?})."),
        AttachError::NoDiskNumber => crate::println!("aoe: no free disk number."),
    }
}

/// `aoe-attach` targets from `settings` that have not answered yet; the `aoe-attach` thread keeps trying them.
static PENDING: SpinLock<Vec<Wanted>> = SpinLock::new(Vec::new());

/// One try at a `settings` target: true when it is settled (attached, or refused for a reason a retry cannot change,
/// which is printed); false when it did not answer, to try again.
fn settle(wanted: &Wanted) -> bool {
    match attach(wanted.shelf, wanted.slot, wanted.writable, wanted.via) {
        Ok(disk) => {
            say_attached(wanted, disk);
            true
        }
        Err(AttachError::NotFound | AttachError::Identify(_)) => false,
        // Attached by hand meanwhile.
        Err(AttachError::Attached) => true,
        Err(error) => {
            say_failed(wanted, &error);
            true
        }
    }
}

/// At the end of boot, before `start` runs (the service thread): attaches the `aoe-attach` targets in `settings`, so the
/// `start` file and the services find their disks. A NIC's link takes a few seconds to come up and the target may be
/// booting too, so it tries for up to `BOOT_WAIT_TICKS`; what is still missing then is left to the `aoe-attach` thread,
/// and the boot goes on without it.
pub fn attach_from_settings() {
    let mut left = crate::config::aoe_attach();
    if left.is_empty() {
        return;
    }
    crate::println!("aoe: attaching {} target(s) from settings (up to {} s).", left.len(), BOOT_WAIT_TICKS / 100);
    let deadline = crate::arch::ticks() + BOOT_WAIT_TICKS;
    loop {
        left.retain(|wanted| !settle(wanted));
        if left.is_empty() || crate::arch::ticks() >= deadline {
            break;
        }
        sched::sleep_ticks(100);
    }
    if left.is_empty() {
        return;
    }
    for wanted in &left {
        crate::println!("aoe: e{}.{} has not answered; it is looked for every {} s.", wanted.shelf, wanted.slot, RETRY_TICKS / 100);
    }
    *PENDING.lock() = left;
    if sched::spawn_any("aoe-attach", retry_thread, 0).is_none() {
        crate::println!("aoe: could not start the aoe-attach thread; `aoe attach` them by hand.");
        PENDING.lock().clear();
    }
}

/// Tries the pending `settings` targets every `RETRY_TICKS` and ends when none is left.
extern "C" fn retry_thread(_: usize) {
    loop {
        sched::sleep_ticks(RETRY_TICKS);
        let mut left: Vec<Wanted> = PENDING.lock().clone();
        left.retain(|wanted| !settle(wanted));
        let done = left.is_empty();
        *PENDING.lock() = left;
        if done {
            sched::exit();
        }
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
            let Some(wanted) = parse_wanted(words.by_ref()) else {
                crate::println!("aoe attach SHELF.SLOT [rw] [MAC]   (`aoe discover` lists them;");
                crate::println!("     MAC: which of the target's NICs)");
                return true;
            };
            match attach(wanted.shelf, wanted.slot, wanted.writable, wanted.via) {
                Ok(disk) => say_attached(&wanted, disk),
                Err(error) => say_failed(&wanted, &error),
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
