//! Boot-time scheduler self-tests: interleaving, preemption, sleep, the sleeping mutex, SMP placement and wake latency.
//! They need only `sched`, `percpu` and the arch hooks (`arch::ticks`, `arch::wait_for_interrupt`), not any one CPU.

use crate::{percpu, pmm, sched, tlb};

static SELFTEST_STEPS: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

extern "C" fn selftest_worker(steps: usize) {
    for _ in 0..steps {
        SELFTEST_STEPS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        sched::yield_now();
    }
}

/// Starts three threads that each take five steps, interleaved by yielding, and checks that they all finish and that their
/// stacks (frames and mappings) come back.
static SPIN_STOP: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
static SPIN_COUNT: [core::sync::atomic::AtomicU64; 2] = [core::sync::atomic::AtomicU64::new(0), core::sync::atomic::AtomicU64::new(0)];
static SHARED: sched::SleepMutex<u64> = sched::SleepMutex::new(0);
static SLEPT_TICKS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static FINISHED: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Never yields: only the timer tick can take the CPU from it.
extern "C" fn spinner(index: usize) {
    use core::sync::atomic::Ordering::Relaxed;
    while !SPIN_STOP.load(Relaxed) {
        SPIN_COUNT[index].fetch_add(1, Relaxed);
    }
    FINISHED.fetch_add(1, Relaxed);
}

/// Read-modify-write under the sleeping mutex with a yield in the middle, so the other thread contends and has to block.
extern "C" fn incrementer(rounds: usize) {
    for _ in 0..rounds {
        let mut guard = SHARED.lock();
        let value = *guard;
        sched::yield_now();
        *guard = value + 1;
    }
    FINISHED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
}

extern "C" fn sleeper(ticks: usize) {
    let start = crate::arch::ticks();
    sched::sleep_ticks(ticks as u64);
    SLEPT_TICKS.store(crate::arch::ticks() - start, core::sync::atomic::Ordering::Relaxed);
    FINISHED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
}

/// Preemption, sleeping and the sleeping mutex: two threads that never yield both make progress, a sleeper wakes on time and
/// two contending incrementers lose no update.
fn sched_preempt_selftest() {
    use core::sync::atomic::Ordering::Relaxed;
    for index in 0..2 {
        sched::spawn("spinner", spinner, index).expect("a spinner");
    }
    for _ in 0..2 {
        sched::spawn("incrementer", incrementer, 25).expect("an incrementer");
    }
    sched::spawn("sleeper", sleeper, 20).expect("a sleeper");
    let start = crate::arch::ticks();
    // The boot thread is the idle thread and may not block: it sleeps in `hlt` until the tick brings it back.
    while crate::arch::ticks() - start < 60 {
        crate::arch::wait_for_interrupt();
    }
    SPIN_STOP.store(true, Relaxed);
    let mut waited = 0;
    while FINISHED.load(Relaxed) < 5 && waited < 300 {
        crate::arch::wait_for_interrupt();
        waited += 1;
    }
    for _ in 0..4 {
        sched::yield_now();
    }
    let (a, b) = (SPIN_COUNT[0].load(Relaxed), SPIN_COUNT[1].load(Relaxed));
    let total = *SHARED.lock();
    let slept = SLEPT_TICKS.load(Relaxed);
    println!("Scheduler: preemption {} ({} and {} spins by two threads that never yield), sleep {} ({slept} ticks for 20), mutex {} ({total} of 50 increments).",
        if a > 0 && b > 0 { "ok" } else { "FAILED" }, a, b, if (20..=23).contains(&slept) { "ok" } else { "FAILED" }, if total == 50 { "ok" } else { "FAILED" });
}

static SMP_SEEN: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static SMP_DONE: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

extern "C" fn smp_worker(_: usize) {
    use core::sync::atomic::Ordering::Relaxed;
    let until = crate::arch::ticks() + 5;
    while crate::arch::ticks() < until {
        SMP_SEEN.fetch_or(1 << percpu::this().index, Relaxed);
        core::hint::spin_loop();
    }
    SMP_DONE.fetch_add(1, Relaxed);
}

/// Unpinned threads that spin for a while are picked up by every online CPU (an idle application processor looks at each tick).
fn sched_smp_selftest() {
    use core::sync::atomic::Ordering::Relaxed;
    let cpus = percpu::count();
    let workers = 2 * cpus;
    for _ in 0..workers {
        sched::spawn_any("smp-worker", smp_worker, 0).expect("an smp worker");
    }
    let start = crate::arch::ticks();
    while SMP_DONE.load(Relaxed) < workers && crate::arch::ticks() - start < 300 {
        crate::arch::wait_for_interrupt();
        sched::yield_now();
    }
    for _ in 0..4 {
        sched::yield_now();
    }
    let seen = SMP_SEEN.load(Relaxed).count_ones() as usize;
    println!("Scheduler: SMP {} ({} unpinned threads ran on {seen} of {cpus} CPU(s)).", if seen == cpus && SMP_DONE.load(Relaxed) == workers { "ok" } else { "FAILED" }, workers);
}

static WAKE_STAMP: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

extern "C" fn wake_worker(_: usize) {
    WAKE_STAMP.store(crate::arch::micros(), core::sync::atomic::Ordering::Release);
}

/// A thread spawned while the other CPUs idle starts within the IPI's latency, not at their next 10 ms tick (which would
/// average 5 ms).
fn sched_wake_selftest() {
    use core::sync::atomic::Ordering::{Acquire, Relaxed};
    let cpus = percpu::count();
    if cpus < 2 {
        return;
    }
    for _ in 0..4 {
        sched::yield_now();
    }
    let mut total = 0;
    const TRIALS: u64 = 10;
    for _ in 0..TRIALS {
        WAKE_STAMP.store(0, Relaxed);
        crate::arch::busy_wait_ms(3);
        let begin = crate::arch::micros();
        sched::spawn_any("wake-worker", wake_worker, 0).expect("a wake worker");
        let mut spins = 0u64;
        while WAKE_STAMP.load(Acquire) == 0 && spins < 100_000_000 {
            core::hint::spin_loop();
            spins += 1;
        }
        total += WAKE_STAMP.load(Acquire).saturating_sub(begin);
    }
    let average = total / TRIALS;
    println!("Scheduler: wake IPI {} (a thread spawned on {cpus} idle CPU(s) started after {average} us on average).", if average < 2000 { "ok" } else { "FAILED" });
}

pub fn run() {
    sched::init();
    // The first stack creates the page-table pages of the stack region, which stay: do that before counting frames.
    sched::spawn("warmup", selftest_worker, 0).expect("a warm-up thread");
    for _ in 0..4 {
        sched::yield_now();
    }
    let free_before = pmm::stats().free;
    for name in ["worker-a", "worker-b", "worker-c"] {
        sched::spawn(name, selftest_worker, 5).expect("a worker thread");
    }
    let mut passes = 0;
    while SELFTEST_STEPS.load(core::sync::atomic::Ordering::Relaxed) < 15 && passes < 1000 {
        sched::yield_now();
        passes += 1;
    }
    // One more pass each lets the last switch release the final dead thread.
    for _ in 0..4 {
        sched::yield_now();
    }
    let mut left = 0;
    sched::for_each(|id, name, _| left += (id == 0 || !name.starts_with("idle")) as usize);
    let steps = SELFTEST_STEPS.load(core::sync::atomic::Ordering::Relaxed);
    let leaked = free_before - pmm::stats().free;
    println!("Scheduler: 3 kernel threads took {steps} interleaved steps in {} context switches; {left} thread(s) left, {leaked} stack frame(s) not returned.", sched::switches());
    println!("TLB: {} shootdown(s) so far (every exited thread's stack is flushed on all {} CPU(s) before its slot is reused).", tlb::SHOOTDOWNS.load(core::sync::atomic::Ordering::Relaxed), percpu::count());
    if steps != 15 || left != 1 || leaked != 0 {
        println!("Scheduler self-test FAILED.");
    }
    sched_preempt_selftest();
    sched_smp_selftest();
    sched_wake_selftest();
    // Ring 3 / EL0 processes exist on x86 only so far.
    #[cfg(target_arch = "x86_64")]
    crate::user_test::run();
}
