//! Tape threads (ABI 37): several kernel threads that share one process (one address space, one pid), for a tape that serves
//! many clients at once (`smbd`: a network thread and a worker per connection).
//!
//! A thread is an ordinary scheduler thread whose `root` is the process's page-table root, started in ring 3 on a stack the
//! tape allocated (`thread_spawn`) through the shim's `tape_thread_start`, which ends it when the entry returns. What the
//! threads of a process share lives in `sched::Proc` (live thread count, heap cursor, console byte count, first outcome); the
//! process is torn down by its last thread (`process::leave`). The first thread ending the process (exit, fault, kill) flags
//! all others, and a flagged thread blocked in `futex_wait` or `thread_join` is made ready, so `kill` never hangs on a sleeper.
//!
//! Page tables are edited by several CPUs at once, so `alloc` and `free` hold `MAP_LOCK`; `free` also does a TLB shootdown so
//! no CPU keeps a freed page mapped. `futex_wait` reads its word through the physical direct map, with the scheduler locked,
//! so a page fault can never happen inside the lock.

use fosfor_abi::tape::{THREAD_BAD_ARGUMENT, THREAD_LIMIT};

use crate::sync::SpinLock;
use crate::{process, sched, vm};

pub static MAP_LOCK: SpinLock<()> = SpinLock::new(());

/// Futex keys are the user address plus the pid, so tapes cannot wake each other; user addresses are below 2^47, so no
/// key reaches the join keys' bit 63.
fn key(address: u64) -> usize {
    ((sched::current_pid() & 0xffff) << 47 | address) as usize
}

pub fn spawn(entry: u64, argument: u64, stack_top: u64) -> isize {
    let root = sched::current_root();
    if stack_top % 16 != 0 || !vm::user_range_ok(root, entry, 1, false) || !vm::user_range_ok(root, stack_top.wrapping_sub(16), 16, true) {
        return THREAD_BAD_ARGUMENT;
    }
    match process::spawn_thread(entry, argument, stack_top) {
        Some(tid) => tid as isize,
        None => THREAD_LIMIT,
    }
}

pub fn join(tid: u64) -> isize {
    sched::join(tid);
    0
}

pub fn futex_wait(address: u64, expected: u64) -> isize {
    if address % 4 != 0 {
        return THREAD_BAD_ARGUMENT;
    }
    let Some(physical) = vm::user_physical(sched::current_root(), address) else { return THREAD_BAD_ARGUMENT };
    let word = vm::direct(physical) as *const u32; // kernel-memory
    sched::wait_while(key(address), || unsafe { word.read_volatile() } == expected as u32);
    0
}

pub fn futex_wake(address: u64, count: u64) -> isize {
    sched::wake_up_to(key(address), count.min(usize::MAX as u64) as usize) as isize
}
