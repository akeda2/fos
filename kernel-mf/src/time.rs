//! Time as the shared kernel sees it: the scheduler tick, a microsecond clock and a bounded busy wait. The sources are the
//! architecture's (the local APIC timer and PM timer / TSC on x86, the generic timer and GIC on aarch64).

pub use crate::arch::{TICK_HZ, busy_wait_ms, micros, ticks};

use core::sync::atomic::{AtomicU64, Ordering};

/// Tick lateness, a cyclictest of sorts: each CPU's timer interrupt is periodic, so the gap between two of its ticks is the
/// period plus however long the second one was held off (interrupts off on that CPU, a long handler, or firmware: an SMI
/// stops every CPU). The worst per CPU since `sysrq` last read it, and when (µs since boot), find the stalls that make an
/// audio card's interrupt late. A stall shows only if a tick falls in it: one of 1 ms is caught about one time in ten, so
/// let it run for a while. Gaps of a second or more (boot, a debugger) are not counted.
const MAX_TRACKED: usize = 64;
/// A tick at least this late is also a `tick-late` trace event (a = CPU, b = µs): above the scheduler's usual jitter, below
/// the shortest audio ring half that matters (0.36 ms at 176.4 kHz, period 64).
const TRACE_LATE_US: u64 = 300;
static LAST_TICK_US: [AtomicU64; MAX_TRACKED] = [const { AtomicU64::new(0) }; MAX_TRACKED];
static WORST_LATE_US: [AtomicU64; MAX_TRACKED] = [const { AtomicU64::new(0) }; MAX_TRACKED];
static WORST_AT_US: [AtomicU64; MAX_TRACKED] = [const { AtomicU64::new(0) }; MAX_TRACKED];

/// From each CPU's timer interrupt (`sched::tick`).
pub fn note_tick(cpu: usize) {
    if cpu >= MAX_TRACKED {
        return;
    }
    let now = micros();
    let last = LAST_TICK_US[cpu].swap(now, Ordering::Relaxed);
    let gap = now.saturating_sub(last);
    if last == 0 || gap >= 1_000_000 {
        return;
    }
    let late = gap.saturating_sub(1_000_000 / TICK_HZ);
    // Each stall worth an audio ring also goes to the trace ring with its time, to line up with the driver's own events.
    if late >= TRACE_LATE_US {
        kernel_core::trace::event(kernel_core::trace::Category::Audio, "tick-late", cpu as u64, late);
    }
    // Only this CPU writes its slots; a reader may see the two out of step, which a report can live with.
    if late > WORST_LATE_US[cpu].load(Ordering::Relaxed) {
        WORST_LATE_US[cpu].store(late, Ordering::Relaxed);
        WORST_AT_US[cpu].store(now, Ordering::Relaxed);
    }
}

/// (worst lateness, when) of `cpu` since the last call, which starts a new window.
pub fn take_tick_lateness(cpu: usize) -> (u64, u64) {
    if cpu >= MAX_TRACKED {
        return (0, 0);
    }
    (WORST_LATE_US[cpu].swap(0, Ordering::Relaxed), WORST_AT_US[cpu].swap(0, Ordering::Relaxed))
}
