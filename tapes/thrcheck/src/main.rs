//! `thrcheck`: threads inside one tape (ABI 37, `make test-mf-threads`). On the server kernel it starts four counting threads
//! that share memory, wakes a sleeper through `futex_wait`/`futex_wake`, allocates and frees pages from two threads at once,
//! and finally returns while one thread sleeps forever and another spins: the process must still end. The embedded kernels
//! have no threads; there it says so and passes.
#![no_std]
#![no_main]

use core::sync::atomic::{AtomicU32, Ordering};

use tape_rt::{TapeApi, println};

const WORKERS: usize = 4;
const STEPS: u32 = 100_000;

static COUNTER: AtomicU32 = AtomicU32::new(0);
static FLAG: AtomicU32 = AtomicU32::new(0);
static GOT: AtomicU32 = AtomicU32::new(0);
static ALLOC_OK: AtomicU32 = AtomicU32::new(0);
static NEVER: AtomicU32 = AtomicU32::new(0);

extern "C" fn count(_argument: usize) {
    for _ in 0..STEPS {
        COUNTER.fetch_add(1, Ordering::SeqCst);
    }
}

extern "C" fn sleeper(_argument: usize) {
    while FLAG.load(Ordering::SeqCst) == 0 {
        tape_rt::futex_wait(&FLAG, 0);
    }
    GOT.store(FLAG.load(Ordering::SeqCst), Ordering::SeqCst);
}

/// Allocates, writes and frees pages over and over: two of these editing one page table at once.
extern "C" fn churn(_argument: usize) {
    for round in 0..200usize {
        let bytes = 4096 * (1 + round % 8);
        let memory = (tape_rt::api().alloc)(bytes);
        if memory == 0 {
            return;
        }
        let words = unsafe { core::slice::from_raw_parts_mut(memory as *mut u8, bytes) };
        words.fill(round as u8);
        if words.iter().any(|&b| b != round as u8) {
            return;
        }
        (tape_rt::api().free)(memory, bytes);
    }
    ALLOC_OK.fetch_add(1, Ordering::SeqCst);
}

extern "C" fn sleep_forever(_argument: usize) {
    loop {
        tape_rt::futex_wait(&NEVER, 0);
    }
}

extern "C" fn spin_forever(_argument: usize) {
    loop {
        core::hint::spin_loop();
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(api: &'static TapeApi) {
    tape_rt::init(api);
    if !tape_rt::threads_supported() {
        println!("thrcheck: SKIP this kernel has no tape threads");
        return;
    }
    let mut failures = 0;
    let mut check = |ok: bool, what: &str| {
        println!("thrcheck: {} {}", if ok { "ok  " } else { "FAIL" }, what);
        if !ok {
            failures += 1;
        }
    };

    let workers: [Option<tape_rt::Thread>; WORKERS] = core::array::from_fn(|_| tape_rt::spawn_thread(count, 0, 16384).ok());
    check(workers.iter().all(Option::is_some), "four threads started");
    for worker in workers.into_iter().flatten() {
        worker.join();
    }
    check(COUNTER.load(Ordering::SeqCst) == WORKERS as u32 * STEPS, "shared atomic counter is exact after join");

    let waiter = tape_rt::spawn_thread(sleeper, 0, 16384);
    check(waiter.is_ok(), "sleeper started");
    let start = tape_rt::micros();
    while tape_rt::micros() - start < 50_000 {
        core::hint::spin_loop();
    }
    FLAG.store(7, Ordering::SeqCst);
    tape_rt::futex_wake(&FLAG, 1);
    if let Ok(waiter) = waiter {
        waiter.join();
    }
    check(GOT.load(Ordering::SeqCst) == 7, "futex_wake releases a sleeping thread");

    let a = tape_rt::spawn_thread(churn, 0, 16384);
    let b = tape_rt::spawn_thread(churn, 0, 16384);
    churn(0);
    for thread in [a, b].into_iter().flatten() {
        thread.join();
    }
    check(ALLOC_OK.load(Ordering::SeqCst) == 3, "alloc and free from three threads at once");

    check(tape_rt::spawn_thread(sleep_forever, 0, 16384).is_ok(), "a thread that sleeps forever started");
    check(tape_rt::spawn_thread(spin_forever, 0, 16384).is_ok(), "a thread that spins forever started");
    if failures == 0 {
        println!("thrcheck: PASS threads share memory, wake each other and stop with the tape");
    } else {
        println!("thrcheck: FAILED {failures} check(s)");
    }
}
