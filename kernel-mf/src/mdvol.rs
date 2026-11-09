//! Linux software RAID for the server kernel: the glue between `crates/md` (docs/md.md) and the block layer.
//!
//! An assembled array is registered as one more disk (`block::Backend::Md`), so everything that works on a disk (`disks`,
//! `ext4 diskN`, `fstab`, `mount`, tapes through `//` paths and `@LABEL`) works on it unchanged.
//!
//! Boot never waits for it. With `md auto` in `settings` one thread (`md-assemble`) looks for members, waits up to
//! `md wait` seconds (default 10) for all of them, then assembles what it has (degraded if it must) and keeps retrying with
//! a growing pause (1, 2, 4 ... 30 s), and at once when a disk appears, until an array is up. The same thread closes the
//! array ("in step" in its superblocks) after a few idle seconds, so only a crash while writing leaves it dirty. A dirty
//! array is only read, never written, until `md force-rw`.
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

/// Array sectors a resync or rebuild covers per hold of the lock; reads and writes get in between.
const JOB_BUDGET: u64 = 2048;

static JOB_THREAD: AtomicBool = AtomicBool::new(false);
/// The disk index `md add` announced and waits to have confirmed (usize::MAX: none).
static PENDING_ADD: AtomicUsize = AtomicUsize::new(usize::MAX);

/// Forgets an unconfirmed `md add`: a confirmation must be the very next command.
pub fn cancel() {
    PENDING_ADD.store(usize::MAX, Ordering::Relaxed);
}

#[derive(Clone, Copy)]
struct Member {
    disk: usize,
    base: u64,
    sectors: u64,
}

struct Candidates {
    list: [Member; md::array::MAX_MEMBERS],
    count: usize,
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
        block::read(m.disk, m.base + lba, buffer).map_err(|_| Error::Device)
    }
    fn write(&mut self, member: usize, lba: u64, buffer: &[u8]) -> Result<(), Error> {
        let m = self.list[member];
        if lba + (buffer.len() / 512) as u64 > m.sectors {
            return Err(Error::Range);
        }
        block::write_deferred(m.disk, m.base + lba, buffer).map_err(|_| Error::Device)
    }
    fn flush(&mut self, member: usize) -> Result<(), Error> {
        block::flush(self.list[member].disk).map_err(|_| Error::Device)
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
    members: Candidates { list: [Member { disk: 0, base: 0, sectors: 0 }; md::array::MAX_MEMBERS], count: 0 },
    disk: None,
    force_rw: false,
    last_write: 0,
    status: StatusText { bytes: [0; 96], length: 0 },
});

fn now() -> u64 {
    crate::clock::unix_seconds()
}

fn seconds() -> u64 {
    crate::apic::ticks() / 100
}

/// Finds every disk and Linux RAID partition that carries an md superblock. The array's own disk is skipped.
fn scan(own: Option<usize>) -> Candidates {
    let mut found = Candidates { list: [Member { disk: 0, base: 0, sectors: 0 }; md::array::MAX_MEMBERS], count: 0 };
    for disk in 0..block::count() {
        if Some(disk) == own || matches!(block::get(disk), Some(block::Disk { backend: block::Backend::Md, .. }) | None) {
            continue;
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
                return found;
            }
            let index = found.count;
            found.list[index] = *member;
            if md::array::find_superblock(&mut found, index).is_some() {
                found.count += 1;
            }
        }
    }
    found
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
    let mut state = STATE.lock();
    if state.array.is_some() {
        return Outcome::Assembled;
    }
    let mut members = scan(state.disk);
    if members.count == 0 {
        state.status.set(format_args!("no member found yet"));
        return Outcome::NoMembers;
    }
    let mut array = match Array::assemble(&mut members, None) {
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
        Some(disk) => block::set_md(disk, sectors, model).then_some(disk),
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
    Outcome::Assembled
}

/// The boot thread: tries until an array is up, never blocks the boot, then closes the array after idle seconds.
pub extern "C" fn assemble_thread(_: usize) {
    let started = seconds();
    let (mut pause, mut next, mut seen) = (1u64, 0u64, 0usize);
    loop {
        let t = seconds();
        let up = STATE.lock().array.is_some();
        if !up && (t >= next || block::count() != seen) {
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

/// Runs a resync or rebuild to its end, a slice at a time, then exits.
extern "C" fn job_thread(_: usize) {
    let mut report = 0;
    loop {
        let outcome = {
            let mut state = STATE.lock();
            let state = &mut *state;
            state.last_write = seconds();
            state.array.as_mut().map(|array| {
                let before = array.job();
                (before, array.step(&mut state.members, JOB_BUDGET, now()), array.job(), array.sectors())
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
        sched::sleep_ticks(1);
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

/// A read of the array as disk sectors, for the block layer.
pub fn read(lba: u64, buffer: &mut [u8]) -> Result<(), block::Error> {
    let mut state = STATE.lock();
    let state = &mut *state;
    let array = state.array.as_ref().ok_or(block::Error::Device)?;
    array.read(&mut state.members, lba, buffer).map_err(|error| if error == Error::Range { block::Error::OutOfRange } else { block::Error::Device })
}

pub fn write(lba: u64, buffer: &[u8]) -> Result<(), block::Error> {
    let mut state = STATE.lock();
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
    let mut state = STATE.lock();
    let state = &mut *state;
    match state.array.as_mut() {
        Some(array) if array.is_writing() => array.flush(&mut state.members, now()).map_err(|_| block::Error::Device),
        _ => Ok(()),
    }
}

/// Closes the array clean (shutdown, reboot, `md stop`). Does not wait for a lock that someone holds across a hang.
pub fn stop_all() {
    let Some(mut state) = STATE.try_lock() else { return };
    let state = &mut *state;
    if let Some(array) = state.array.as_mut() {
        if array.is_writing() {
            let _ = array.stop(&mut state.members, now());
        }
    }
}

pub fn command(mut words: core::str::SplitWhitespace) {
    match words.next() {
        None => {
            let state = STATE.lock();
            let text = core::str::from_utf8(&state.status.bytes[..state.status.length]).unwrap_or("");
            match state.array.as_ref() {
                Some(array) => {
                    println!("md: {} ({} MiB, events {}): {}", array.name(), array.sectors() >> 11, array.events(), text);
                    println!("    members in use: {} of {}{}", array.present(), array.expected(), if state.force_rw { ", force-rw" } else { "" });
                    if let Some((job, done)) = array.job() {
                        println!("    {}: {}% of {} MiB", if job == Job::Resync { "resync" } else { "rebuild" }, done * 100 / array.sectors().max(1), array.sectors() >> 11);
                    }
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
            match state.array.as_mut() {
                None => println!("md: no array is up."),
                Some(array) => {
                    if array.is_writing() {
                        let _ = array.stop(&mut state.members, now());
                    }
                    state.array = None;
                    state.status.set(format_args!("stopped"));
                    println!("md: stopped; members are clean. `md assemble` brings it back.");
                }
            }
        }
        Some("resync") => resync_command(),
        Some("add") => add_command(words),
        Some("resume") => resume_command(),
        Some("force-rw") => {
            let mut state = STATE.lock();
            state.force_rw = true;
            println!("md: writes allowed even though the array was not clean (nothing resyncs it here; run a check on Linux).");
        }
        Some(_) => println!("usage: md [assemble | stop | resync | resume | add diskN | force-rw]"),
    }
}
