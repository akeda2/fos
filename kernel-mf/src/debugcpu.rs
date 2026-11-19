//! What every CPU and every thread is doing, kept so the debug tools (`sysrq.rs`, the crash reports) can say it without
//! taking a lock. Shared by both architectures; the platform parts are `arch::capture_cpus` (an NMI on x86, an SGI on
//! aarch64), `arch::readable` (may a stack word be read) and `arch::frame_pointer`.
//!
//! - **CPU snapshots.** On request each CPU records the place it was interrupted at (pc, stack and frame pointer, whether it
//!   was in a process and whether interrupts were on) and a backtrace. On x86 the request is a non-maskable interrupt, so a
//!   CPU spinning with interrupts off answers too; that is what makes a deadlock or a `deadhang` readable.
//! - **Backtraces** follow the frame-pointer chain (kernel-mf is built with `-C force-frame-pointers=yes`): on both CPUs a
//!   frame record is the caller's frame pointer followed by the return address. Every word is checked against the page
//!   tables before it is read, the chain must climb, and it stops after `FRAMES`. Addresses inside the kernel image are
//!   printed as offsets from its start, which is what `addr2line -e fosfor-kernel-mf` wants (the image is linked at 0).
//! - **Call records**: per thread, the tape call it is in (or made last), since when and how many it made.

use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};


use crate::percpu::{self, MAX_CPUS};

/// Return addresses kept per CPU snapshot.
pub const FRAMES: usize = 12;

pub struct Snapshot {
    /// Bumped each time the CPU fills the record (odd while it writes).
    pub sequence: AtomicU64,
    pub pc: AtomicU64,
    pub sp: AtomicU64,
    pub fp: AtomicU64,
    /// Interrupted in a process (ring 3 / EL0): no kernel backtrace.
    pub user: AtomicBool,
    /// Interrupts were enabled where it was interrupted.
    pub interrupts: AtomicBool,
    pub thread: AtomicUsize,
    pub depth: AtomicUsize,
    pub frames: [AtomicU64; FRAMES],
}

const EMPTY: Snapshot = Snapshot {
    sequence: AtomicU64::new(0),
    pc: AtomicU64::new(0),
    sp: AtomicU64::new(0),
    fp: AtomicU64::new(0),
    user: AtomicBool::new(false),
    interrupts: AtomicBool::new(false),
    thread: AtomicUsize::new(0),
    depth: AtomicUsize::new(0),
    frames: [const { AtomicU64::new(0) }; FRAMES],
};

pub static SNAPSHOTS: [Snapshot; MAX_CPUS] = [EMPTY; MAX_CPUS];
/// Capture interrupts sent to each CPU and not yet taken. Counted rather than flagged so that one arriving after
/// `capture_all` gave up waiting is still recognised (on x86 an unexpected NMI is fatal).
static PENDING: [AtomicUsize; MAX_CPUS] = [const { AtomicUsize::new(0) }; MAX_CPUS];

/// For the trace ring: the calling CPU and the thread it runs.
pub fn origin() -> (u32, Option<u32>) {
    let cpu = percpu::index();
    (cpu as u32, Some(percpu::get(cpu).thread.load(Ordering::Relaxed) as u32))
}

/// The kernel image's range in memory (it is linked at 0, so an offset from `start` is a link address).
pub fn image() -> (u64, u64) {
    unsafe extern "C" {
        static __kernel_start: u8;
        static __kernel_end: u8;
    }
    (&raw const __kernel_start as u64, &raw const __kernel_end as u64)
}

/// Follows the frame-pointer chain from `fp`, writing return addresses to `out`; returns how many.
pub fn walk(mut fp: u64, out: &mut [u64]) -> usize {
    let mut count = 0;
    while count < out.len() {
        if fp == 0 || fp & 7 != 0 || !crate::arch::readable(fp) || !crate::arch::readable(fp + 8) {
            break;
        }
        let (next, ret) = unsafe { ((fp as *const u64).read_volatile(), ((fp + 8) as *const u64).read_volatile()) };
        // Only kernel frames are walked: a return address outside the image is the bottom of the stack (an AP's start, a
        // thread's first frame) or not a frame at all.
        let (start, end) = image();
        if ret < start || ret >= end {
            break;
        }
        out[count] = ret;
        count += 1;
        // Stacks grow down, so a caller's frame is above; a frame more than 1 MiB away is not on the same stack.
        if next <= fp || next - fp > 1 << 20 {
            break;
        }
        fp = next;
    }
    count
}

/// One code address as text: `k+0xOFFSET` inside the kernel image, else the raw address.
pub struct Code(pub u64);

impl core::fmt::Display for Code {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        CodeIn(self.0, image()).fmt(f)
    }
}

/// The same for an address of a kernel image at `.1` (start, end): a previous boot's, from the crash record.
pub struct CodeIn(pub u64, pub (u64, u64));

impl core::fmt::Display for CodeIn {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        let (start, end) = self.1;
        if (start..end).contains(&self.0) { write!(f, "k+{:#x}", self.0 - start) } else { write!(f, "{:#x}", self.0) }
    }
}

/// Records the calling CPU's state as interrupted at `pc` (called by the capture interrupt, or by a CPU about itself).
pub fn record(cpu: usize, pc: u64, sp: u64, fp: u64, user: bool, interrupts: bool) {
    let snapshot = &SNAPSHOTS[cpu];
    snapshot.sequence.fetch_add(1, Ordering::AcqRel);
    snapshot.pc.store(pc, Ordering::Relaxed);
    snapshot.sp.store(sp, Ordering::Relaxed);
    snapshot.fp.store(fp, Ordering::Relaxed);
    snapshot.user.store(user, Ordering::Relaxed);
    snapshot.interrupts.store(interrupts, Ordering::Relaxed);
    snapshot.thread.store(percpu::get(cpu).thread.load(Ordering::Relaxed), Ordering::Relaxed);
    let mut frames = [0u64; FRAMES];
    let depth = if user { 0 } else { walk(fp, &mut frames) };
    for (slot, frame) in snapshot.frames.iter().zip(frames) {
        slot.store(frame, Ordering::Relaxed);
    }
    snapshot.depth.store(depth, Ordering::Relaxed);
    snapshot.sequence.fetch_add(1, Ordering::AcqRel);
}

/// The capture interrupt on CPU `cpu`, interrupted at `pc`: records the state and returns true if a capture asked for it,
/// false for an interrupt nobody sent (the caller then treats it as before).
pub fn interrupted(cpu: usize, pc: u64, sp: u64, fp: u64, user: bool, interrupts: bool) -> bool {
    let taken = PENDING[cpu].try_update(Ordering::AcqRel, Ordering::Acquire, |pending| pending.checked_sub(1)).is_ok();
    if taken {
        record(cpu, pc, sp, fp, user, interrupts);
    }
    taken
}

/// Asks every other online CPU to record its state, records the caller's own, and waits up to `wait_ms` for the answers.
/// Returns the CPUs that answered (bit per CPU, the caller included). Safe from any context: it takes no lock.
pub fn capture_all(wait_ms: u64) -> u128 {
    let me = percpu::index();
    let count = percpu::count().min(MAX_CPUS);
    let mut before = [0u64; MAX_CPUS];
    let mut asked: u128 = 0;
    for cpu in (0..count).filter(|&cpu| cpu != me && percpu::get(cpu).online.load(Ordering::Acquire)) {
        before[cpu] = SNAPSHOTS[cpu].sequence.load(Ordering::Acquire);
        PENDING[cpu].fetch_add(1, Ordering::AcqRel);
        asked |= 1 << cpu;
    }
    if asked != 0 {
        crate::arch::capture_cpus(asked);
    }
    let fp = crate::arch::frame_pointer();
    record(me, capture_all as *const () as usize as u64, fp, fp, false, crate::cpu::interrupts_enabled());
    let mut answered: u128 = 1 << me;
    for _ in 0..wait_ms.max(1) {
        for cpu in 0..count {
            let sequence = SNAPSHOTS[cpu].sequence.load(Ordering::Acquire);
            if asked & 1 << cpu != 0 && sequence != before[cpu] && sequence & 1 == 0 {
                answered |= 1 << cpu;
            }
        }
        if answered & asked == asked {
            break;
        }
        crate::time::busy_wait_ms(1);
    }
    answered
}

/// One line per CPU (and a backtrace line): what `capture_all` collected, plus the locks each CPU holds and waits for.
pub fn write_cpus(out: &mut dyn Write, answered: u128) {
    let now = crate::time::ticks();
    for cpu in 0..percpu::count().min(MAX_CPUS) {
        let online = percpu::get(cpu).online.load(Ordering::Acquire);
        let thread = percpu::get(cpu).thread.load(Ordering::Relaxed);
        if !online {
            let _ = writeln!(out, "cpu{cpu}: offline");
            continue;
        }
        let snapshot = &SNAPSHOTS[cpu];
        if answered & 1 << cpu == 0 {
            let _ = writeln!(out, "cpu{cpu}: thread {thread} ({}), no answer to the capture", thread_name(thread));
        } else {
            let _ = writeln!(
                out,
                "cpu{cpu}: thread {thread} ({}), {} at {}, interrupts {}",
                thread_name(thread),
                if snapshot.user.load(Ordering::Relaxed) { "in its process" } else { "kernel" },
                Code(snapshot.pc.load(Ordering::Relaxed)),
                if snapshot.interrupts.load(Ordering::Relaxed) { "on" } else { "OFF" }
            );
            let depth = snapshot.depth.load(Ordering::Relaxed).min(FRAMES);
            if depth > 0 {
                let _ = write!(out, "  bt:");
                for frame in &snapshot.frames[..depth] {
                    let _ = write!(out, " {}", Code(frame.load(Ordering::Relaxed)));
                }
                let _ = writeln!(out);
            }
        }
        write_locks(out, cpu, now);
    }
}

fn write_locks(out: &mut dyn Write, cpu: usize, now: u64) {
    let locks = &crate::sync::CPU_LOCKS[cpu];
    for (held, place) in locks.held.iter().zip(&locks.held_at) {
        if held.load(Ordering::Acquire) != 0 {
            if let Some(place) = unsafe { place.load(Ordering::Relaxed).as_ref() } {
                let _ = writeln!(out, "  holds the lock it took at {place}");
            }
        }
    }
    let waiting = locks.waiting.load(Ordering::Acquire);
    if waiting != 0 {
        let since = now.saturating_sub(locks.waiting_since.load(Ordering::Relaxed)) * 1000 / crate::time::TICK_HZ;
        let at = unsafe { locks.waiting_at.load(Ordering::Relaxed).as_ref() };
        let _ = write!(out, "  WAITS {since} ms for a lock");
        if let Some(at) = at {
            let _ = write!(out, " at {at}");
        }
        let _ = writeln!(out);
        // Which CPU holds that lock, found through the other CPUs' held lists (the lock itself is not touched).
        for other in 0..percpu::count().min(MAX_CPUS) {
            let theirs = &crate::sync::CPU_LOCKS[other];
            for (held, place) in theirs.held.iter().zip(&theirs.held_at) {
                if held.load(Ordering::Acquire) == waiting {
                    if let Some(place) = unsafe { place.load(Ordering::Relaxed).as_ref() } {
                        let _ = writeln!(out, "    held by cpu{other}, taken at {place}");
                    }
                }
            }
        }
    }
}

// ---- thread names without the scheduler's lock ----

const NAME_SLOTS: usize = 256;
/// Per thread id (modulo `NAME_SLOTS`): the id + 1 and the name, filled in when the thread starts.
static NAME_IDS: [AtomicUsize; NAME_SLOTS] = [const { AtomicUsize::new(0) }; NAME_SLOTS];
static NAMES: [[AtomicU64; 2]; NAME_SLOTS] = [const { [AtomicU64::new(0), AtomicU64::new(0)] }; NAME_SLOTS];

pub fn thread_name(id: usize) -> ThreadName {
    let slot = id % NAME_SLOTS;
    let mut bytes = [0u8; 16];
    if NAME_IDS[slot].load(Ordering::Acquire) == id + 1 {
        bytes[..8].copy_from_slice(&NAMES[slot][0].load(Ordering::Relaxed).to_le_bytes());
        bytes[8..].copy_from_slice(&NAMES[slot][1].load(Ordering::Relaxed).to_le_bytes());
    }
    ThreadName(bytes)
}

pub struct ThreadName([u8; 16]);

impl core::fmt::Display for ThreadName {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        let length = self.0.iter().position(|&b| b == 0).unwrap_or(16);
        f.write_str(core::str::from_utf8(&self.0[..length]).unwrap_or("?"))
    }
}

// ---- call records ----

/// Bit 63 of `CALLS`: the thread is inside the call.
const INSIDE: u64 = 1 << 63;
static CALLS: [AtomicU64; NAME_SLOTS] = [const { AtomicU64::new(0) }; NAME_SLOTS];
static CALL_SINCE: [AtomicU64; NAME_SLOTS] = [const { AtomicU64::new(0) }; NAME_SLOTS];
static CALL_COUNT: [AtomicU64; NAME_SLOTS] = [const { AtomicU64::new(0) }; NAME_SLOTS];

/// A thread `id` named `name` starts: its records start empty.
pub fn thread_started(id: usize, name: &[u8; 16]) {
    let slot = id % NAME_SLOTS;
    NAME_IDS[slot].store(0, Ordering::Release);
    NAMES[slot][0].store(u64::from_le_bytes(name[..8].try_into().unwrap()), Ordering::Relaxed);
    NAMES[slot][1].store(u64::from_le_bytes(name[8..].try_into().unwrap()), Ordering::Relaxed);
    CALLS[slot].store(0, Ordering::Relaxed);
    CALL_COUNT[slot].store(0, Ordering::Relaxed);
    NAME_IDS[slot].store(id + 1, Ordering::Release);
}

/// The calling thread enters system call `number` (`process::syscall`).
pub fn call_enter(number: u64) {
    let slot = percpu::this().thread.load(Ordering::Relaxed) % NAME_SLOTS;
    CALL_SINCE[slot].store(crate::time::ticks(), Ordering::Relaxed);
    CALLS[slot].store(number | INSIDE, Ordering::Relaxed);
    CALL_COUNT[slot].fetch_add(1, Ordering::Relaxed);
}

pub fn call_leave() {
    let slot = percpu::this().thread.load(Ordering::Relaxed) % NAME_SLOTS;
    CALLS[slot].fetch_and(!INSIDE, Ordering::Relaxed);
}

/// " in audio_write 1520 ms (8123 calls)" or " last puts (12 calls)", empty for a thread that made none.
pub fn write_call(out: &mut dyn Write, id: usize) {
    let slot = id % NAME_SLOTS;
    let count = CALL_COUNT[slot].load(Ordering::Relaxed);
    if NAME_IDS[slot].load(Ordering::Acquire) != id + 1 || count == 0 {
        return;
    }
    let call = CALLS[slot].load(Ordering::Relaxed);
    let name = crate::tapeapi::call_name(call & !INSIDE);
    if call & INSIDE != 0 {
        let since = crate::time::ticks().saturating_sub(CALL_SINCE[slot].load(Ordering::Relaxed)) * 1000 / crate::time::TICK_HZ;
        let _ = write!(out, " in {name} {since} ms ({count} calls)");
    } else {
        let _ = write!(out, " last {name} ({count} calls)");
    }
}

/// The crash report's addition (interrupts off, the console lock broken): the crashing code's backtrace, then where every
/// other CPU is. Also kept in the crash record (`crashrec`) for the next boot.
pub fn report_crash(pc: u64, fp: u64, user: bool) {
    // A report without a real interrupted frame (a panic, a deadlock) starts from here: the walk then shows its callers.
    let fp = if fp == 0 { crate::arch::frame_pointer() } else { fp };
    let mut frames = [0u64; FRAMES];
    let depth = if user { 0 } else { walk(fp, &mut frames) };
    let mut line = Buffer::<160>::new();
    let _ = write!(line, "Backtrace from {}:", Code(pc));
    for frame in &frames[..depth] {
        let _ = write!(line, " {}", Code(*frame));
    }
    println!("{}", line.as_str());
    println!("(k+OFFSET: addr2line -f -e fosfor-kernel-mf OFFSET)");
    let mut answered = 0;
    if percpu::count() > 1 {
        answered = capture_all(100);
        write_cpus(&mut Console, answered);
    }
    crate::crashrec::record_extra(&frames[..depth], answered);
}

/// Writes to the console a piece at a time (for `write_cpus` when there is no buffer to fill).
pub struct Console;

impl Write for Console {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        crate::serial::print(format_args!("{text}"));
        Ok(())
    }
}

/// A fixed-size text buffer; what does not fit is dropped.
pub struct Buffer<const N: usize> {
    bytes: [u8; N],
    length: usize,
}

impl<const N: usize> Buffer<N> {
    pub const fn new() -> Self {
        Buffer { bytes: [0; N], length: 0 }
    }
    pub fn clear(&mut self) {
        self.length = 0;
    }
    pub fn as_str(&self) -> &str {
        let bytes = &self.bytes[..self.length];
        // A cut at the end may have split a character: keep what is whole.
        match core::str::from_utf8(bytes) {
            Ok(text) => text,
            Err(error) => core::str::from_utf8(&bytes[..error.valid_up_to()]).unwrap_or(""),
        }
    }
}

impl<const N: usize> Write for Buffer<N> {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        let take = text.len().min(N - self.length);
        self.bytes[self.length..self.length + take].copy_from_slice(&text.as_bytes()[..take]);
        self.length += take;
        Ok(())
    }
}
