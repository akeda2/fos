//! Linux software RAID for the server kernel: the glue between `crates/md` (docs/md.md) and the block layer.
//!
//! An assembled array is registered as one more disk (`block::Backend::Md`), so everything that works on a disk (`disks`,
//! `ext4 diskN`, `fstab`, `mount`, tapes through `//` paths and `@LABEL`) works on it unchanged.
//!
//! Boot never waits for it. With `md auto` in `settings` one thread (`md-assemble`) looks for members, waits up to
//! `md wait` seconds (default 10) for all of them, then assembles what it has (degraded if it must) and keeps retrying with
//! a growing pause (1, 2, 4 ... 30 s), and at once when a disk appears, until an array is up. The same thread closes the
//! array ("in step" in its superblocks) after a few idle seconds, so only a crash while writing leaves it dirty; it also
//! starts, without the assembling, when an array comes up by `md create` or `md assemble` and there is no `md auto`.
//! `md stop` holds it back from assembling again until `md assemble` or `md create`. Every restart (`reboot`, `shutdown`,
//! a kernel update) closes the array first (`stop_all`). A dirty array is only read, never written, until `md force-rw`.
//!
//! One lock covers the array; the member reads and writes under it are the drivers' bounded ones, and a member that
//! fails is dropped by `md` (a read falls back to the other member), so a dead disk costs a failed transfer, not a stall.

use crate::{block, sched};
use alloc::boxed::Box;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use md::{
    Error, Members,
    array::{Array, Job, Step},
};

/// GPT type "Linux RAID" (A19D880F-05FC-4D3B-A006-743F0F84911E), in on-disk mixed-endian order.
const RAID_GUID: [u8; 16] = [0x0f, 0x88, 0x9d, 0xa1, 0xfc, 0x05, 0x3b, 0x4d, 0xa0, 0x06, 0x74, 0x3f, 0x0f, 0x84, 0x91, 0x1e];

/// Seconds without a write after which the array is closed clean.
const IDLE_SECONDS: u64 = 3;

/// Array sectors a resync or rebuild covers per hold of the lock (2 MiB; a few milliseconds on SATA SSDs, whose members
/// are read at once); reads and writes get in between.
const JOB_BUDGET: u64 = 4096;

/// Block-layer reads, writes and flushes waiting for `STATE`. The job thread takes the lock again right after it lets
/// go, before a waiter woken on another CPU has run, so while this is non-zero it sleeps a tick instead.
static IO_WAITING: AtomicUsize = AtomicUsize::new(0);

/// `STATE` for a block-layer read, write or flush: counted in `IO_WAITING` while it waits, so a resync steps aside.
fn io_lock() -> sched::SleepGuard<'static, State> {
    IO_WAITING.fetch_add(1, Ordering::AcqRel);
    let guard = STATE.lock();
    IO_WAITING.fetch_sub(1, Ordering::AcqRel);
    guard
}

static JOB_THREAD: AtomicBool = AtomicBool::new(false);
/// The `md-assemble` thread is running (it never ends).
static KEEPER: AtomicBool = AtomicBool::new(false);
/// Set by `md stop`: the thread does not assemble again (`md auto`, a disk appearing) until `md assemble` or `md create`.
static HELD: AtomicBool = AtomicBool::new(false);
/// The disk index `md add` announced and waits to have confirmed (usize::MAX: none).
static PENDING_ADD: AtomicUsize = AtomicUsize::new(usize::MAX);

/// Forgets an unconfirmed `md add`: a confirmation must be the very next command.
pub fn cancel() {
    PENDING_ADD.store(usize::MAX, Ordering::Relaxed);
    *PENDING_CREATE.lock() = None;
}

/// The `md create` arguments announced and waiting for `confirm` as the very next command.
static PENDING_CREATE: crate::sync::SpinLock<Option<alloc::string::String>> = crate::sync::SpinLock::new(None);

#[derive(Clone, Copy)]
struct Member {
    disk: usize,
    base: u64,
    sectors: u64,
}

struct Candidates {
    list: [Member; md::array::MAX_MEMBERS],
    count: usize,
    /// Microseconds spent in the member calls below, so the job thread can tell disk time from md's own work.
    spent_us: u64,
    /// Reads `read_ahead` started, oldest first: a resync keeps the next piece in flight while it waits for this one.
    ahead: [Option<block::Ahead>; 2],
}

impl Candidates {
    fn timed<R>(&mut self, work: impl FnOnce() -> R) -> R {
        let begin = crate::arch::micros();
        let result = work();
        self.spent_us += crate::arch::micros().saturating_sub(begin);
        result
    }
}

impl Members for Candidates {
    fn count(&self) -> usize {
        self.count
    }
    fn sectors(&self, member: usize) -> u64 {
        self.list[member].sectors
    }
    fn read(&mut self, member: usize, lba: u64, buffer: &mut [u8]) -> Result<(), Error> {
        let m = self.list[member];
        if lba + (buffer.len() / 512) as u64 > m.sectors {
            return Err(Error::Range);
        }
        self.timed(|| block::read(m.disk, m.base + lba, buffer).map_err(|_| Error::Device))
    }
    /// A row's members at once: `block::read_many` overlaps the SATA ones, so a row costs its slowest member's time.
    /// Ranges `read_ahead` started are only waited for.
    fn read_many(&mut self, reads: &mut [md::Read<'_>]) -> Result<(), (usize, Error)> {
        let mut many = alloc::vec::Vec::with_capacity(reads.len());
        for (index, request) in reads.iter_mut().enumerate() {
            let m = self.list[request.member];
            if request.lba + (request.buffer.len() / 512) as u64 > m.sectors {
                return Err((index, Error::Range));
            }
            many.push(block::Read { disk: m.disk, lba: m.base + request.lba, buffer: &mut *request.buffer });
        }
        let started = self.ahead.iter().position(|ahead| ahead.as_ref().is_some_and(|ahead| ahead.matches(&many)));
        let ahead = started.and_then(|index| self.ahead[index].take());
        self.timed(|| match ahead {
            Some(ahead) => ahead.finish(&mut many),
            None => block::read_many(&mut many),
        })
        .map_err(|(index, _)| (index, Error::Device))
    }
    /// Queues the ranges on the SATA members (`block::start_ahead`); a third read-ahead first waits for the oldest.
    unsafe fn read_ahead(&mut self, reads: &mut [md::Read<'_>]) {
        let mut many = alloc::vec::Vec::with_capacity(reads.len());
        for request in reads.iter_mut() {
            let m = self.list[request.member];
            if request.lba + (request.buffer.len() / 512) as u64 > m.sectors {
                return;
            }
            many.push(block::Read { disk: m.disk, lba: m.base + request.lba, buffer: &mut *request.buffer });
        }
        if self.ahead.iter().all(Option::is_some) {
            let oldest = self.ahead[0].take();
            self.timed(|| drop(oldest));
            self.ahead.rotate_left(1);
        }
        let slot = self.ahead.iter().position(Option::is_none).unwrap_or(0);
        // the caller keeps the buffers as `start_ahead` needs (this method's own contract)
        self.ahead[slot] = Some(self.timed(|| unsafe { block::start_ahead(&mut many) }));
    }
    fn settle(&mut self) {
        let ahead = [self.ahead[0].take(), self.ahead[1].take()];
        self.timed(|| drop(ahead));
    }
    fn write(&mut self, member: usize, lba: u64, buffer: &[u8]) -> Result<(), Error> {
        let m = self.list[member];
        if lba + (buffer.len() / 512) as u64 > m.sectors {
            return Err(Error::Range);
        }
        self.timed(|| block::write_deferred(m.disk, m.base + lba, buffer).map_err(|_| Error::Device))
    }
    /// A row's data and parity at once: `block::write_many` overlaps the SATA members (no flush, as `write`).
    fn write_many(&mut self, writes: &[md::Write<'_>]) -> Result<(), (usize, Error)> {
        let mut many = alloc::vec::Vec::with_capacity(writes.len());
        for (index, request) in writes.iter().enumerate() {
            let m = self.list[request.member];
            if request.lba + (request.buffer.len() / 512) as u64 > m.sectors {
                return Err((index, Error::Range));
            }
            many.push(block::Write { disk: m.disk, lba: m.base + request.lba, buffer: request.buffer });
        }
        self.timed(|| block::write_many(&many).map_err(|(index, _)| (index, Error::Device)))
    }
    fn flush(&mut self, member: usize) -> Result<(), Error> {
        let disk = self.list[member].disk;
        self.timed(|| block::flush(disk).map_err(|_| Error::Device))
    }
}

struct State {
    array: Option<Box<Array>>,
    members: Candidates,
    /// Disk index of the array in the block layer, kept when the array is stopped so a later assembly reuses it.
    disk: Option<usize>,
    force_rw: bool,
    last_write: u64,
    status: StatusText,
}

/// A short line for `md`, kept without allocating under the lock.
struct StatusText {
    bytes: [u8; 96],
    length: usize,
}

impl Write for StatusText {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        let take = text.len().min(self.bytes.len() - self.length);
        self.bytes[self.length..self.length + take].copy_from_slice(&text.as_bytes()[..take]);
        self.length += take;
        Ok(())
    }
}

impl StatusText {
    fn set(&mut self, args: core::fmt::Arguments) {
        self.length = 0;
        let _ = self.write_fmt(args);
    }
}

static STATE: sched::SleepMutex<State> = sched::SleepMutex::new(State {
    array: None,
    members: Candidates { list: [Member { disk: 0, base: 0, sectors: 0 }; md::array::MAX_MEMBERS], count: 0, spent_us: 0, ahead: [None, None] },
    disk: None,
    force_rw: false,
    last_write: 0,
    status: StatusText { bytes: [0; 96], length: 0 },
});

fn now() -> u64 {
    crate::clock::unix_seconds()
}

fn seconds() -> u64 {
    crate::time::ticks() / 100
}

const NO_MEMBERS: Candidates = Candidates { list: [Member { disk: 0, base: 0, sectors: 0 }; md::array::MAX_MEMBERS], count: 0, spent_us: 0, ahead: [None, None] };

/// Finds every disk and Linux RAID partition that carries an md superblock. The array's own disk is skipped.
fn scan(own: Option<usize>) -> Candidates {
    let mut found = NO_MEMBERS;
    for disk in 0..block::count() {
        if Some(disk) != own {
            members_on(disk, &mut found);
        }
    }
    found
}

/// Adds the whole disk `disk` and its Linux RAID partitions to `found` where they carry an md superblock. An array's
/// own disk (and a missing one) has none.
fn members_on(disk: usize, found: &mut Candidates) {
    if matches!(block::get(disk), Some(block::Disk { backend: block::Backend::Md, .. }) | None) {
        return;
    }
    let total = block::get(disk).map_or(0, |info| info.sectors);
    let mut tries = [Member { disk, base: 0, sectors: total }; 5];
    let mut count = 1;
    kernel_core::gpt::find(disk, &RAID_GUID, |partition| {
        if count < tries.len() {
            tries[count] = Member { disk, base: partition.start, sectors: partition.sectors };
            count += 1;
        }
        false
    });
    for member in &tries[..count] {
        if found.count == found.list.len() {
            return;
        }
        let index = found.count;
        found.list[index] = *member;
        if md::array::find_superblock(found, index).is_some() {
            found.count += 1;
        }
    }
}

/// Whether disk index `disk` (or a RAID partition on it) carries an md superblock: a member of an array, assembled or
/// not. Exporting or writing it on its own would go behind the array's back (`aoetarget::export` refuses it).
pub fn holds_member(disk: usize) -> bool {
    let mut found = NO_MEMBERS;
    members_on(disk, &mut found);
    found.count > 0
}

/// The array's disk number (1-based) while one is up.
pub fn disk_number() -> Option<usize> {
    let state = STATE.lock();
    state.array.as_ref().and(state.disk).map(|disk| disk + 1)
}

/// Why a try failed or what it found, for `md`.
pub enum Outcome {
    Assembled,
    NoMembers,
    /// Present members, expected members.
    Waiting(usize, usize),
    Failed(Error),
}

/// One assembly try. `wait_over`: assemble with what is there even if members are missing.
fn try_assemble(wait_over: bool) -> Outcome {
    try_assemble_uuid(wait_over, None)
}

/// One assembly try, of the array `uuid` only when given (`md create` names the array it just wrote).
fn try_assemble_uuid(wait_over: bool, uuid: Option<[u8; 16]>) -> Outcome {
    let mut state = STATE.lock();
    if state.array.is_some() {
        return Outcome::Assembled;
    }
    let mut members = scan(state.disk);
    if members.count == 0 {
        state.status.set(format_args!("no member found yet"));
        return Outcome::NoMembers;
    }
    let mut array = match Array::assemble(&mut members, uuid) {
        Ok(array) => array,
        Err(error) => {
            state.status.set(format_args!("members found, cannot assemble: {error:?}"));
            return Outcome::Failed(error);
        }
    };
    if array.degraded() && !wait_over {
        state.status.set(format_args!("waiting for members ({} of {})", array.present(), array.expected()));
        return Outcome::Waiting(array.present(), array.expected());
    }
    // a partial parity log repairs what a crash left half written, before anything reads the array
    if array.has_ppl() && array.dirty() {
        match array.recover_ppl(&mut members) {
            Ok(report) => crate::println!("md: partial parity log: {} entries replayed, {} not usable.", report.entries, report.mismatched),
            Err(error) => crate::println!("md: partial parity log not replayed: {error:?}"),
        }
    }
    let (present, expected, sectors, dirty, pending) = (array.present(), array.expected(), array.sectors(), array.dirty(), array.rebuild_pending());
    let mut model = [b' '; 40];
    let mut text = StatusText { bytes: [0; 96], length: 0 };
    let _ = write!(text, "Linux md RAID {} {}", array.level(), array.name());
    let take = text.length.min(40);
    model[..take].copy_from_slice(&text.bytes[..take]);
    let disk = match state.disk {
        Some(disk) => block::set_virtual(disk, sectors, model).then_some(disk),
        None => block::register(block::Backend::Md, sectors, model, "md"),
    };
    let Some(disk) = disk else {
        state.status.set(format_args!("no free disk number"));
        return Outcome::Failed(Error::Unsupported);
    };
    state.disk = Some(disk);
    state.members = members;
    state.array = Some(Box::new(array));
    state.force_rw = false;
    state.status.set(format_args!("disk{} up: {present} of {expected} members{}", disk + 1, if dirty { ", dirty: read-only" } else { "" }));
    crate::println!("md: array assembled as disk{} ({} MiB, {present} of {expected} members{}).", disk + 1, sectors >> 11, if dirty { ", not clean: read-only until `md force-rw`" } else { "" });
    if present < expected {
        crate::println!("md: WARNING: running degraded, with no spare copy of some data.");
    }
    if pending {
        crate::println!("md: a rebuild was interrupted; `md resume` goes on from where it stopped.");
    }
    drop(state);
    // the thread that closes the array when idle, also without `md auto`
    start_keeper();
    // `aoe-export md` lines in settings wait for this
    crate::aoetarget::md_assembled();
    Outcome::Assembled
}

/// Starts `md-assemble` once: at boot with `md auto` (main.rs), and whenever an array comes up, so an array made by
/// `md create` or assembled by hand is closed clean when idle too.
pub fn start_keeper() {
    if !KEEPER.swap(true, Ordering::AcqRel) && sched::spawn("md-assemble", assemble_thread, 0).is_none() {
        KEEPER.store(false, Ordering::Release);
        crate::println!("md: could not start the md-assemble thread; the array is closed only by `md stop`.");
    }
}

/// The md thread: with `md auto`, tries until an array is up (never blocking the boot) unless `md stop` holds it back;
/// always closes the array after idle seconds.
extern "C" fn assemble_thread(_: usize) {
    let started = seconds();
    let (mut pause, mut next, mut seen) = (1u64, 0u64, 0usize);
    loop {
        let t = seconds();
        let up = STATE.lock().array.is_some();
        let wanted = crate::config::md_auto() && !HELD.load(Ordering::Acquire);
        if !up && wanted && (t >= next || block::count() != seen) {
            seen = block::count();
            let over = t.saturating_sub(started) >= crate::config::md_wait();
            match try_assemble(over) {
                Outcome::Assembled => pause = 1,
                Outcome::Waiting(..) => next = t + 1,
                _ => {
                    next = t + pause;
                    pause = (pause * 2).min(30);
                }
            }
        }
        idle_close();
        sched::sleep_ticks(100);
    }
}

fn idle_close() {
    let mut state = STATE.lock();
    let state = &mut *state;
    if let Some(array) = state.array.as_mut() {
        if array.is_writing() && array.job().is_none() && seconds().saturating_sub(state.last_write) >= IDLE_SECONDS {
            let _ = array.stop(&mut state.members, now());
        }
    }
}

fn start_job_thread() {
    if !JOB_THREAD.swap(true, Ordering::AcqRel) && sched::spawn("md-job", job_thread, 0).is_none() {
        JOB_THREAD.store(false, Ordering::Release);
        crate::println!("md: could not start the job thread.");
    }
}

/// Where a resync or rebuild spends its wall time, kept by the job thread and shown by `md` (during the job and after
/// it), so a slow job can be pinned on the disks, the copies, md's own work (parity, bookkeeping) or the pauses between
/// steps before anything is changed to speed it up. Times are microseconds.
#[derive(Clone, Copy)]
struct JobTimes {
    job: Job,
    began_us: u64,
    /// 0 while it runs.
    ended_us: u64,
    /// Inside `Array::step`, the array lock held.
    step_us: u64,
    /// Of that, in member reads, writes and flushes (`Candidates::spent_us`).
    member_us: u64,
    /// Of that, the overlapped SATA commands' share, step by step (`block::many_times`).
    many: block::ManyTimes,
    /// Array sectors covered.
    sectors: u64,
}

static JOB_TIMES: crate::sync::SpinLock<Option<JobTimes>> = crate::sync::SpinLock::new(None);

/// `md`'s lines about the running or last job's time (each at most 79 columns).
fn print_job_times() {
    let Some(times) = *JOB_TIMES.lock() else { return };
    let end = if times.ended_us == 0 { crate::arch::micros() } else { times.ended_us };
    let total = end.saturating_sub(times.began_us).max(1);
    let part = |us: u64| us * 100 / total;
    let many = times.many;
    let disks = many.start_us + many.wait_us + many.copy_us;
    let name = if times.job == Job::Resync { "resync" } else { "rebuild" };
    let when = if times.ended_us == 0 { "so far" } else { "in all" };
    let mib_s = |bytes: u64| bytes * 1_000_000 / total >> 20;
    println!("    {name} {when}: {} s, {} MiB/s of the array; of that time:", total / 1_000_000, mib_s(times.sectors * 512));
    println!(
        "      disk commands {}% (waiting {}, copying {}, starting {})",
        part(disks),
        part(many.wait_us),
        part(many.copy_us),
        part(many.start_us)
    );
    println!(
        "      other member I/O {}%, md's own work {}%, between steps {}%",
        part(times.member_us.saturating_sub(disks)),
        part(times.step_us.saturating_sub(times.member_us)),
        part(total.saturating_sub(times.step_us))
    );
    if many.commands > 0 {
        println!("      {} commands of {} KiB on average, {} MiB/s moved;", many.commands, many.bytes / many.commands >> 10, mib_s(many.bytes));
        println!("      {} straight to md's memory, {} read ahead (queued)", many.direct, many.ahead);
    }
}

/// Runs a resync or rebuild to its end, a slice at a time, then exits.
extern "C" fn job_thread(_: usize) {
    let mut report = 0;
    let began_us = crate::arch::micros();
    let mut times: Option<JobTimes> = None;
    loop {
        let outcome = {
            let mut state = STATE.lock();
            let state = &mut *state;
            state.last_write = seconds();
            state.array.as_mut().map(|array| {
                let before = array.job();
                let (step_begin, member_begin, many_begin) = (crate::arch::micros(), state.members.spent_us, block::many_times());
                let step = array.step(&mut state.members, JOB_BUDGET, now());
                let after = array.job();
                let job = after.or(before).map(|(job, _)| job);
                if let Some(job) = job {
                    let entry = times.get_or_insert(JobTimes {
                        job,
                        began_us,
                        ended_us: 0,
                        step_us: 0,
                        member_us: 0,
                        many: block::ManyTimes::default(),
                        sectors: 0,
                    });
                    entry.step_us += crate::arch::micros().saturating_sub(step_begin);
                    entry.member_us += state.members.spent_us.saturating_sub(member_begin);
                    let many = block::many_times().since(&many_begin);
                    entry.many.start_us += many.start_us;
                    entry.many.wait_us += many.wait_us;
                    entry.many.copy_us += many.copy_us;
                    entry.many.commands += many.commands;
                    entry.many.direct += many.direct;
                    entry.many.bytes += many.bytes;
                    entry.many.ahead += many.ahead;
                    let (from, to) = (before.map_or(0, |(_, done)| done), after.map_or(array.sectors(), |(_, done)| done));
                    entry.sectors += to.saturating_sub(from);
                    *JOB_TIMES.lock() = Some(*entry);
                }
                (before, step, after, array.sectors())
            })
        };
        match outcome {
            Some((_, Ok(Step::Running(_)), after, sectors)) => {
                let percent = after.map_or(0, |(_, done)| done * 100 / sectors.max(1)) / 10 * 10;
                if percent >= report + 10 {
                    report = percent;
                    crate::println!("md: {percent}% done.");
                }
            }
            Some((_, Ok(Step::Done(job)), _, _)) => {
                crate::println!("md: {} finished.", if job == Job::Resync { "resync" } else { "rebuild" });
                break;
            }
            Some((_, Ok(Step::Idle), _, _)) | None => break,
            Some((_, Err(error), _, _)) => {
                crate::println!("md: the job stopped: {error:?}");
                break;
            }
        }
        // Full speed while nobody waits (a fixed 10 ms sleep per step held the resync to a third of the disks' time);
        // a tick's pause when reads or writes queue for the lock, so they get it.
        if IO_WAITING.load(Ordering::Acquire) > 0 {
            sched::sleep_ticks(1);
        } else {
            sched::yield_now();
        }
    }
    if let Some(times) = JOB_TIMES.lock().as_mut().filter(|times| times.began_us == began_us) {
        times.ended_us = crate::arch::micros();
    }
    JOB_THREAD.store(false, Ordering::Release);
    sched::exit()
}

/// `md resync`: opens a dirty array and compares its copies in the background.
fn resync_command() {
    {
        let mut state = STATE.lock();
        let state = &mut *state;
        let Some(array) = state.array.as_mut() else { return crate::println!("md: no array is up.") };
        if !array.dirty() {
            return crate::println!("md: the array is clean; nothing to resync.");
        }
        if !array.is_writing() {
            if let Err(error) = array.start(&mut state.members, now()) {
                return crate::println!("md: cannot open the array for writing: {error:?}");
            }
        }
        match array.begin_resync() {
            Ok(()) => crate::println!("md: resync started in the background; `md` shows how far it is."),
            Err(error) => return crate::println!("md: cannot resync: {error:?}"),
        }
    }
    start_job_thread();
}

/// `md resume`: goes on with a rebuild that an earlier `md stop` or power cut interrupted.
fn resume_command() {
    {
        let mut state = STATE.lock();
        let state = &mut *state;
        let Some(array) = state.array.as_mut() else { return crate::println!("md: no array is up.") };
        if !array.rebuild_pending() {
            return crate::println!("md: no interrupted rebuild to go on with.");
        }
        if array.dirty() {
            return crate::println!("md: the array is not clean; run `md resync` first.");
        }
        if let Err(error) = array.start(&mut state.members, now()) {
            return crate::println!("md: cannot open the array for writing: {error:?}");
        }
        state.last_write = seconds();
        crate::println!("md: rebuild goes on in the background; `md` shows how far it is.");
    }
    start_job_thread();
}

/// `md add diskN` then `md add diskN confirm`: fills the disk in as the array's missing member.
fn add_command(mut words: core::str::SplitWhitespace) {
    let Some(disk) = words.next().and_then(|word| word.strip_prefix("disk")).and_then(|n| n.parse::<usize>().ok()).filter(|&n| n >= 1) else {
        return crate::println!("usage: md add diskN  then  md add diskN confirm");
    };
    let index = disk - 1;
    let confirm = words.next() == Some("confirm");
    {
        let mut state = STATE.lock();
        let state = &mut *state;
        let Some(array) = state.array.as_mut() else { return crate::println!("md: no array is up.") };
        let Some(info) = block::get(index) else { return crate::println!("md: disk{disk} does not exist.") };
        if index == 0 || state.disk == Some(index) || matches!(info.backend, block::Backend::Md) {
            return crate::println!("md: disk{disk} cannot be added (the boot disk and the array itself never can).");
        }
        if array.rebuild_pending() {
            return crate::println!("md: an interrupted rebuild is waiting; `md resume` goes on with it.");
        }
        if crate::aoetarget::exported(index) {
            return crate::println!("md: disk{disk} is exported over AoE; `aoe unexport` it first.");
        }
        if array.present() >= array.expected() {
            return crate::println!("md: no member is missing.");
        }
        if array.used_members().any(|m| state.members.list[m].disk == index) {
            return crate::println!("md: disk{disk} is already a member.");
        }
        if !confirm {
            PENDING_ADD.store(index, Ordering::Relaxed);
            crate::println!("md: disk{disk} ({} MiB) would be erased and filled in as the missing member.", info.sectors >> 11);
            return crate::println!("md: type `md add disk{disk} confirm` as the very next command to go on.");
        }
        if PENDING_ADD.swap(usize::MAX, Ordering::Relaxed) != index {
            return crate::println!("md: nothing announced for disk{disk}; run `md add disk{disk}` first.");
        }
        if array.dirty() {
            return crate::println!("md: the array is not clean; run `md resync` first.");
        }
        if !array.is_writing() {
            if let Err(error) = array.start(&mut state.members, now()) {
                return crate::println!("md: cannot open the array for writing: {error:?}");
            }
        }
        let known = (0..state.members.count).find(|&m| state.members.list[m].disk == index && state.members.list[m].base == 0);
        let pushed = known.is_none();
        let slot = match known {
            Some(slot) => slot,
            None if state.members.count < state.members.list.len() => {
                state.members.list[state.members.count] = Member { disk: index, base: 0, sectors: info.sectors };
                state.members.count += 1;
                state.members.count - 1
            }
            None => return crate::println!("md: no room for another member."),
        };
        let mut uuid = [0u8; 16];
        kernel_core::random::fill(&mut uuid);
        match array.begin_rebuild(&mut state.members, slot, uuid) {
            Ok(()) => crate::println!("md: rebuilding onto disk{disk} in the background; `md` shows how far it is."),
            Err(error) => {
                if pushed {
                    state.members.count -= 1;
                }
                return crate::println!("md: cannot add disk{disk}: {error:?}");
            }
        }
        state.last_write = seconds();
    }
    start_job_thread();
}

/// `md create LEVEL diskN diskN... [name NAME] [chunk KIB]`, then the same line with `confirm` as the very next
/// command: writes a new array over whole disks (`md::create`: v1.2 superblocks, an internal bitmap, the layout mdadm
/// uses, so Linux assembles it), assembles it and starts the resync that computes its parity or second copy. The
/// array is usable at once (reads and writes go on during the resync, as in Linux).
fn create_command(words: core::str::SplitWhitespace) {
    use alloc::string::String;
    use alloc::vec::Vec;
    const USAGE: &str = "usage: md create raid1|raid5|raid6 diskN diskN... [name NAME] [chunk KIB]";
    let mut words: Vec<&str> = words.collect();
    let confirm = words.last() == Some(&"confirm");
    if confirm {
        words.pop();
    }
    let key: String = words.join(" ");
    let level = match words.first().copied() {
        Some("raid1" | "1") => md::superblock::LEVEL_RAID1,
        Some("raid5" | "5") => md::superblock::LEVEL_RAID5,
        Some("raid6" | "6") => md::superblock::LEVEL_RAID6,
        _ => return crate::println!("{USAGE}"),
    };
    let (mut disks, mut name, mut chunk_kib): (Vec<usize>, &str, u32) = (Vec::new(), "md0", 512);
    let mut rest = words[1..].iter();
    while let Some(&word) = rest.next() {
        match word {
            "name" => match rest.next() {
                Some(&n) if !n.is_empty() && n.len() <= 16 && !n.contains(':') => name = n,
                _ => return crate::println!("md: name NAME: up to 16 characters, no `:`."),
            },
            "chunk" => match rest.next().and_then(|n| n.parse::<u32>().ok()).filter(|&k| (4..=65536).contains(&k) && k.is_power_of_two()) {
                Some(kib) => chunk_kib = kib,
                None => return crate::println!("md: chunk KIB: a power of two, 4 to 65536 (default 512)."),
            },
            _ => match word.strip_prefix("disk").and_then(|n| n.parse::<usize>().ok()).filter(|&n| n >= 1) {
                Some(number) if !disks.contains(&(number - 1)) => disks.push(number - 1),
                Some(_) => return crate::println!("md: {word} is named twice."),
                None => return crate::println!("{USAGE}"),
            },
        }
    }
    // Every disk must be a plain local disk nothing else is using.
    for &index in &disks {
        let disk = index + 1;
        let Some(info) = block::get(index) else { return crate::println!("md: disk{disk} does not exist (see `disks`).") };
        if index == 0 {
            return crate::println!("md: disk1 is the boot disk and is never part of an array.");
        }
        if matches!(info.backend, block::Backend::Md | block::Backend::Aoe(_)) {
            return crate::println!("md: disk{disk} is an array or another machine's disk, not a local one.");
        }
        if crate::aoetarget::exported(index) {
            return crate::println!("md: disk{disk} is exported over AoE; `aoe unexport` it first.");
        }
    }
    if STATE.lock().array.is_some() {
        return crate::println!("md: an array is up; this kernel runs one at a time (`md stop` first).");
    }
    let sizes: Vec<u64> = disks.iter().map(|&index| block::get(index).map_or(0, |info| info.sectors)).collect();
    let mut set_name = [0u8; 32];
    let host = crate::config::hostname().unwrap_or("fos");
    let full = alloc::format!("{host}:{name}");
    let take = full.len().min(32);
    set_name[..take].copy_from_slice(&full.as_bytes()[..take]);
    let mut spec = md::create::Spec { level, chunk_sectors: chunk_kib * 2, name: set_name, uuid: [0; 16], now: now() };
    let plan = match md::create::plan(&spec, &sizes) {
        Ok(plan) => plan,
        Err(Error::TooSmall) => return crate::println!("md: a disk is too small for an array."),
        Err(_) => return crate::println!("md: RAID 1 takes 2-8 disks, RAID 5 3-8, RAID 6 4-8."),
    };
    if !confirm {
        crate::println!("md: RAID {level} \"{full}\" of {} disks, {} MiB, would be made on:", disks.len(), plan.sectors >> 11);
        for &index in &disks {
            crate::println!("  disk{}: {} MiB", index + 1, sizes[disks.iter().position(|&d| d == index).unwrap_or(0)] >> 11);
            if holds_member(index) {
                crate::println!("    an md array member (that array is destroyed)");
            }
            let _files = crate::tapefs::lock();
            kernel_core::ext4vol::describe(index, &mut crate::sysshell::Console);
        }
        crate::println!("md: EVERYTHING on these disks is erased. To go on, type as the very next");
        crate::println!("    command: md create {key} confirm");
        *PENDING_CREATE.lock() = Some(key);
        return;
    }
    if PENDING_CREATE.lock().take().as_deref() != Some(key.as_str()) {
        return crate::println!("md: nothing announced for that; run it without `confirm` first.");
    }
    kernel_core::random::fill(&mut spec.uuid);
    let mut device_uuids = [[0u8; 16]; md::array::MAX_MEMBERS];
    for uuid in device_uuids[..disks.len()].iter_mut() {
        kernel_core::random::fill(uuid);
    }
    {
        // Keep tape file calls off the disks, and drop the ext volume this kernel may keep open on one of them.
        let _files = crate::tapefs::lock();
        let _ = kernel_core::ext4vol::commit_pending();
        kernel_core::ext4vol::forget_shared();
        let mut members = NO_MEMBERS;
        for (slot, &index) in disks.iter().enumerate() {
            members.list[slot] = Member { disk: index, base: 0, sectors: sizes[slot] };
        }
        members.count = disks.len();
        let _state = STATE.lock();
        if let Err(error) = md::create::create(&mut members, &spec, &device_uuids[..disks.len()]) {
            return crate::println!("md: create failed ({error:?}); the disks may be half written: run it again.");
        }
    }
    crate::println!("md: written ({} MiB per disk, bitmap chunk {} MiB); assembling.", plan.member_size >> 11, plan.bitmap_chunk >> 11);
    HELD.store(false, Ordering::Release);
    match try_assemble_uuid(true, Some(spec.uuid)) {
        Outcome::Assembled => resync_command(),
        Outcome::Failed(error) => crate::println!("md: the new array does not assemble: {error:?}"),
        _ => crate::println!("md: the new array was not found again."),
    }
}

/// A read of the array as disk sectors, for the block layer.
pub fn read(lba: u64, buffer: &mut [u8]) -> Result<(), block::Error> {
    let mut state = io_lock();
    let state = &mut *state;
    let array = state.array.as_ref().ok_or(block::Error::Device)?;
    array.read(&mut state.members, lba, buffer).map_err(|error| if error == Error::Range { block::Error::OutOfRange } else { block::Error::Device })
}

pub fn write(lba: u64, buffer: &[u8]) -> Result<(), block::Error> {
    let mut state = io_lock();
    let state = &mut *state;
    let force = state.force_rw;
    let array = state.array.as_mut().ok_or(block::Error::Device)?;
    if !array.is_writing() {
        if array.dirty() && !force {
            return Err(block::Error::Device);
        }
        array.start(&mut state.members, now()).map_err(|_| block::Error::Device)?;
    }
    state.last_write = seconds();
    array.write(&mut state.members, lba, buffer, now()).map_err(|error| if error == Error::Range { block::Error::OutOfRange } else { block::Error::Device })
}

pub fn flush() -> Result<(), block::Error> {
    let mut state = io_lock();
    let state = &mut *state;
    match state.array.as_mut() {
        Some(array) if array.is_writing() => array.flush(&mut state.members, now()).map_err(|_| block::Error::Device),
        _ => Ok(()),
    }
}

/// Closes the array clean (shutdown, reboot, `md stop`). Does not wait for a lock that someone holds across a hang.
pub fn stop_all() {
    // The job thread holds the lock for one step (a few ms); wait out that much, but never hang a restart on it.
    let mut tries = 0;
    let mut state = loop {
        match STATE.try_lock() {
            Some(state) => break state,
            None if tries < 500 => {
                tries += 1;
                crate::time::busy_wait_ms(1);
            }
            None => return crate::println!("md: the array is busy; restarting without closing it."),
        }
    };
    let state = &mut *state;
    if let Some(array) = state.array.as_mut() {
        if array.is_writing() {
            let _ = array.stop(&mut state.members, now());
        }
    }
}

pub fn command(mut words: core::str::SplitWhitespace) {
    let sub = words.clone().next();
    if sub == Some("assemble") {
        // asked for by hand: `md auto` may assemble again after a later disk change too
        HELD.store(false, Ordering::Release);
    }
    match words.next() {
        None => {
            let state = STATE.lock();
            let text = core::str::from_utf8(&state.status.bytes[..state.status.length]).unwrap_or("");
            match state.array.as_ref() {
                Some(array) => {
                    // the state now, not the line kept from assembly: a resync or `force-rw` changes it
                    let health = if array.job().is_some_and(|(job, _)| job == Job::Resync) {
                        ", resyncing (usable)"
                    } else if array.dirty() && !array.is_writing() && !state.force_rw {
                        ", dirty: read-only"
                    } else {
                        ""
                    };
                    let disk = state.disk.map_or(0, |disk| disk + 1);
                    println!("md: {} ({} MiB, events {}): disk{disk} up{health}", array.name(), array.sectors() >> 11, array.events());
                    println!("    members in use: {} of {}{}", array.present(), array.expected(), if state.force_rw { ", force-rw" } else { "" });
                    if let Some((job, done)) = array.job() {
                        println!("    {}: {}% of {} MiB", if job == Job::Resync { "resync" } else { "rebuild" }, done * 100 / array.sectors().max(1), array.sectors() >> 11);
                    }
                    print_job_times();
                }
                None => println!("md: no array{}{}", if text.is_empty() { "" } else { ": " }, text),
            }
        }
        Some("assemble") => match try_assemble(true) {
            Outcome::Assembled => {}
            Outcome::NoMembers => println!("md: no member with an md superblock found."),
            Outcome::Waiting(..) => unreachable!(),
            Outcome::Failed(error) => println!("md: cannot assemble: {error:?}"),
        },
        Some("stop") => {
            let _ = kernel_core::ext4vol::commit_pending();
            kernel_core::ext4vol::forget_shared();
            let mut state = STATE.lock();
            let state = &mut *state;
            if state.array.is_some() && state.disk.is_some_and(crate::aoetarget::exported) {
                return println!("md: the array is exported over AoE; `aoe unexport SLOT` first (see `aoe`).");
            }
            match state.array.as_mut() {
                None => println!("md: no array is up."),
                Some(array) => {
                    let running = array.job();
                    if array.is_writing() {
                        let _ = array.stop(&mut state.members, now());
                    }
                    let dirty = array.dirty();
                    state.array = None;
                    HELD.store(true, Ordering::Release);
                    state.status.set(format_args!("stopped (`md assemble` brings it back)"));
                    match running {
                        Some((job, _)) if dirty => {
                            let name = if job == Job::Rebuild { "rebuild" } else { "resync" };
                            println!("md: stopped; the members are NOT in step (its {name} did not finish).");
                            println!("    `md assemble` brings it back; {}.", if job == Job::Rebuild { "`md resume` goes on" } else { "`md resync` starts over" });
                        }
                        _ if dirty => {
                            println!("md: stopped; the members are still marked not clean, as when assembled.");
                            println!("    `md assemble` brings it back; `md resync` there makes it clean.");
                        }
                        _ => println!("md: stopped; members are clean. `md assemble` brings it back."),
                    }
                }
            }
        }
        Some("resync") => resync_command(),
        Some("add") => add_command(words),
        Some("create") => create_command(words),
        Some("resume") => resume_command(),
        Some("force-rw") => {
            let mut state = STATE.lock();
            state.force_rw = true;
            println!("md: writes allowed even though the array was not clean (nothing resyncs it here; run a check on Linux).");
        }
        Some(_) => println!("usage: md [assemble | stop | resync | resume | add diskN | force-rw]\n       md create raid1|raid5|raid6 diskN diskN... [name NAME] [chunk KIB]"),
    }
}
