//! SysRq for the server kernel: the state of every CPU, thread and lock, asked for over the wire (tag 0x24, the same
//! request as the embedded kernel's, `tools/term_net.py --sysrq`, `fosadm sysrq`) or with the shell's `sysrq`.
//!
//! The embedded kernel answers from its shell loop. Here the service thread answers, and when it has been silent for more
//! than two seconds (stuck, or a CPU sits on a lock it needs) the timer interrupt takes over: `tick` drains the NICs
//! itself, answers a SysRq request from the interrupt and leaves every other frame of ours for the service thread
//! (`net::rescue_drain`). Nothing in the dump waits for a lock: what is held is reported as held, with who took it and where.
//! The other CPUs are stopped for a moment with a non-maskable interrupt (an SGI on aarch64) so their program counters and
//! backtraces are read even with interrupts off (`debugcpu::capture_all`).

use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::debugcpu::{self, Buffer};
use crate::percpu::MAX_CPUS;
use crate::time::{self, TICK_HZ};

/// The `term net` tag of a SysRq request (no payload). The answer is DATA frames to the requester, ending in `END`.
pub const TAG_SYSRQ: u8 = 0x24;
const END: &str = "--- end sysrq ---\n";

/// One dump at a time: the buffer is too big for a stack.
static BUSY: AtomicBool = AtomicBool::new(false);
static mut TEXT: Buffer<16384> = Buffer::new();

/// Writes the dump into `TEXT` and passes it on; false when another dump is being made.
fn with_dump(rescue: bool, then: impl FnOnce(&str)) -> bool {
    if BUSY.swap(true, Ordering::Acquire) {
        return false;
    }
    // SAFETY: BUSY gives this caller the buffer alone.
    let out = unsafe { &mut *core::ptr::addr_of_mut!(TEXT) };
    out.clear();
    write_dump(out, rescue);
    then(out.as_str());
    BUSY.store(false, Ordering::Release);
    true
}

fn write_dump(out: &mut dyn Write, rescue: bool) {
    let ticks = time::ticks();
    let millis = ticks * 1000 / TICK_HZ;
    let _ = writeln!(out, "--- sysrq: up {}.{:03} s, answered by cpu{} ---", millis / 1000, millis % 1000, crate::percpu::index());
    let stalled = crate::debug::since_pet_ticks() * 1000 / TICK_HZ;
    let frozen = FROZEN_FOR.load(Ordering::Relaxed);
    if rescue && frozen != 0 {
        let _ = writeln!(out, "the clock (cpu0's timer) stopped {} ms ago: answered from the timer interrupt", frozen * 1000 / TICK_HZ);
    } else if rescue {
        let _ = writeln!(out, "service thread silent for {stalled} ms: answered from the timer interrupt");
    } else if stalled > 100 {
        let _ = writeln!(out, "service thread last went round {stalled} ms ago");
    }
    let answered = debugcpu::capture_all(100);
    debugcpu::write_cpus(out, answered);
    crate::sched::write_threads(out);
    let _ = write!(out, "memory:");
    match crate::pmm::try_stats() {
        Some(stats) => {
            let _ = write!(out, " {} of {} MiB free", stats.free * 4096 >> 20, stats.managed * 4096 >> 20);
        }
        None => {
            let _ = write!(out, " frame allocator locked");
        }
    }
    match crate::heap::try_stats() {
        Some(stats) => {
            let _ = writeln!(out, ", heap {} of {} KiB free", stats.free >> 10, stats.total >> 10);
        }
        None => {
            let _ = writeln!(out, ", heap locked");
        }
    }
    crate::net::write_sessions(out);
    let (print_us, pass_us) = crate::fb::timings();
    let _ = writeln!(out, "screen: a print held interrupts off up to {print_us} us, a console pass took up to {pass_us} us");
    let total = kernel_core::trace::total();
    let _ = writeln!(out, "last trace events ({total} recorded; `trace` for more):");
    kernel_core::trace::recent(16, |event| {
        let _ = writeln!(out, "{}", kernel_core::trace::Line(event));
    });
    let _ = out.write_str(END);
}

/// A SysRq request from `source` on NIC `index`, from the service thread's frame handler (the NIC lock held).
pub fn request(index: usize, source: [u8; 6]) {
    if crate::config::deny_term_net() || !crate::config::term_net_allowed(index) {
        return;
    }
    kernel_core::trace::event(kernel_core::trace::Category::Net, "sysrq", index as u64, 0);
    // Only a note on the console: the frame handler holds the NIC lock with interrupts off, and the whole dump at 115200
    // baud would keep them off for a second.
    with_dump(false, |text| crate::net::send_text(index, &source, text.as_bytes()));
    crate::println!("sysrq: state dump sent to {}.", kernel_core::netdump::Mac(&source));
}

/// The shell's `sysrq`: the same dump on the console.
pub fn command() {
    if !with_dump(false, |text| crate::print(format_args!("{text}"))) {
        crate::println!("sysrq: another dump is being made.");
    }
}

/// When the rescue last looked at the NICs (ticks).
static LAST_RESCUE: AtomicU64 = AtomicU64::new(0);
/// Silence of the service thread (ticks) before the rescue starts, and its pace after that.
const STALL_TICKS: u64 = 2 * TICK_HZ;
const RESCUE_EVERY: u64 = TICK_HZ / 2;
/// The tick count is advanced by CPU 0's timer alone: when CPU 0 hangs with interrupts off it stops, and the service
/// thread's silence cannot be measured by it. So every CPU also counts its own timer interrupts while the count stands still.
static SEEN: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
static FROZEN: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
/// How long (own ticks) the clock had stood still when the rescue last answered, for the dump's header.
static FROZEN_FOR: AtomicU64 = AtomicU64::new(0);

/// From every CPU's timer interrupt: once the service thread has been silent for `STALL_TICKS` (or the clock has stood still
/// that long), one CPU at a time drains the NICs every `RESCUE_EVERY` and answers a SysRq. No trace events and no `micros`
/// here (both may take locks).
pub fn tick() {
    let me = crate::percpu::index();
    let now = time::ticks();
    let frozen = if SEEN[me].swap(now, Ordering::Relaxed) == now { FROZEN[me].fetch_add(1, Ordering::Relaxed) + 1 } else {
        FROZEN[me].store(0, Ordering::Relaxed);
        0
    };
    if frozen >= STALL_TICKS {
        // Without a clock, each CPU paces itself; `BUSY` keeps two from dumping at once.
        if frozen % RESCUE_EVERY != 0 {
            return;
        }
        FROZEN_FOR.store(frozen, Ordering::Relaxed);
    } else {
        if crate::debug::since_pet_ticks() < STALL_TICKS {
            return;
        }
        let last = LAST_RESCUE.load(Ordering::Relaxed);
        if now.saturating_sub(last) < RESCUE_EVERY
            || LAST_RESCUE.compare_exchange(last, now, Ordering::AcqRel, Ordering::Relaxed).is_err()
        {
            return;
        }
        FROZEN_FOR.store(0, Ordering::Relaxed);
    }
    crate::net::rescue_drain(|index, source| {
        if crate::config::deny_term_net() || !crate::config::term_net_allowed(index) {
            return;
        }
        with_dump(true, |text| crate::net::send_text(index, &source, text.as_bytes()));
    });
}
