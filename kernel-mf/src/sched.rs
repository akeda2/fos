//! Kernel threads. A thread is a kernel stack (4 KiB pages in `vm::STACK_REGION`, an unmapped guard page below each) and a saved
//! stack pointer; switching is `switch_context`, which pushes the callee-saved registers on the old stack, swaps `rsp` and pops
//! them off the new one. Kernel code is built without SSE, so there is no FP state to save for these threads (tapes get theirs
//! with ring 3).
//!
//! Slice 7a/7b: threads give way with `yield_now`, and the timer tick preempts (10 ms slice). Slice 7c: every CPU schedules from
//! the one run queue. A thread may be pinned to a CPU (`spawn` pins to CPU 0, where the unlocked `kernel-core` drivers live;
//! `spawn_any` lets any CPU run it). Each application processor has an idle thread that is never queued; it runs when nothing
//! eligible is ready and looks again at every tick (10 ms; there is no wake IPI yet). A thread is skipped by other CPUs while
//! `on_cpu`, i.e. until the CPU that switched away from it has saved its stack pointer.
//! A thread can `sleep_ticks`, or block on a key (`wait_while`/`wake_all`, what `SleepMutex` is made of). The boot context becomes thread 0, the idle thread. A thread that
//! exits cannot free the stack it is running on, so the next thread to run after the switch does it (`finish_switch`).

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::vec::Vec;

use crate::percpu::MAX_CPUS;
use crate::sync::SpinLock;
use crate::{cpu, pmm, vm};

/// Pages per kernel thread stack (256 KiB: a tape's audio call and the ext4 volume code run on it, and both keep large buffers on the stack).
const STACK_PAGES: u64 = 64;
pub const WAKE_VECTOR: u8 = 0x42;

const NO_THREAD: usize = usize::MAX;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    Running,
    Ready,
    /// Waiting for `wake_all(key)`.
    Blocked,
    /// Waiting for the tick count to reach `wake_at`.
    Sleeping,
    Dead,
}

pub struct Thread {
    pub id: usize,
    pub name: [u8; 16],
    pub state: State,
    /// Saved stack pointer while the thread is not running.
    rsp: u64,
    /// Stack slot and the frames behind it; the boot thread (own stack) has none.
    slot: Option<(u64, u64)>,
    key: usize,
    wake_at: u64,
    /// Only this CPU may run the thread (`None`: any).
    pinned: Option<usize>,
    /// Still on a CPU's stack, or being switched away from: nobody else may pick it yet.
    on_cpu: bool,
    /// The page-table root of the process this thread runs (its user half), 0 for a plain kernel thread.
    root: u64,
    /// Top of this thread's kernel stack, where interrupts and `syscall` from ring 3 arrive; 0 for the boot contexts.
    kstack_top: u64,
    /// Set by `kill`: a process thread ends itself the next time it is in the kernel.
    killed: bool,
    /// Bytes the process has written to the console (for the self-test).
    written: u64,
    /// Unique process number (thread ids are reused), 0 for a kernel thread.
    pid: u64,
    /// Next free address of the process's `alloc` heap (0 until the first allocation).
    heap_next: u64,
    /// Timer ticks this thread was the one running when a CPU's timer fired (10 ms each): its CPU time, sampled.
    cpu_ticks: u64,
    /// The CPU it was last seen running on.
    last_cpu: u8,
    /// Floating point state, saved while a process thread is switched out.
    fpu: Option<crate::fpu::Area>,
}

struct Sched {
    threads: Vec<Option<Box<Thread>>>,
    queue: VecDeque<usize>,
    free_slots: Vec<u64>,
    next_slot: u64,
    current: [usize; MAX_CPUS],
    /// The thread each CPU switched away from: it is released (if dead) by whoever runs next on that CPU.
    previous: [usize; MAX_CPUS],
    /// Each application processor's idle thread (never queued); `NO_THREAD` on CPU 0 and before the CPU has attached.
    idle: [usize; MAX_CPUS],
    switches: u64,
    /// Ticks each CPU found a thread other than an idle thread or the polling `service` thread running (`top`).
    busy: [u64; MAX_CPUS],
}

static SCHED: SpinLock<Option<Sched>> = SpinLock::new(None);

core::arch::global_asm!(
    r#"
    .text
    .global switch_context
switch_context:
    push rbp
    push rbx
    push r12
    push r13
    push r14
    push r15
    mov [rdi], rsp
    mov rsp, rsi
    pop r15
    pop r14
    pop r13
    pop r12
    pop rbx
    pop rbp
    ret

    .global thread_trampoline
thread_trampoline:
    call sched_finish_switch
    sti
    mov rdi, r13
    call r12
    call sched_exit
    ud2
"#
);

unsafe extern "C" {
    fn switch_context(old_rsp: *mut u64, new_rsp: u64);
    fn thread_trampoline();
}

fn this_cpu() -> usize {
    crate::percpu::this().index as usize
}

/// Makes the calling (boot) context thread 0. Call once, on the boot CPU, before spawning.
pub fn init() {
    let boot = Box::new(Thread { id: 0, name: *b"idle0\0\0\0\0\0\0\0\0\0\0\0", state: State::Running, rsp: 0, slot: None, key: 0, wake_at: 0, pinned: Some(0), on_cpu: true, root: 0, kstack_top: 0, killed: false, written: 0, pid: 0, heap_next: 0, cpu_ticks: 0, last_cpu: 0, fpu: None });
    let mut sched = Sched { threads: Vec::new(), queue: VecDeque::new(), free_slots: Vec::new(), next_slot: 0, current: [NO_THREAD; MAX_CPUS], previous: [NO_THREAD; MAX_CPUS], idle: [NO_THREAD; MAX_CPUS], switches: 0, busy: [0; MAX_CPUS] };
    sched.threads.push(Some(boot));
    sched.current[0] = 0;
    *SCHED.lock() = Some(sched);
}

pub fn ready() -> bool {
    SCHED.lock().is_some()
}

/// Called by an application processor once the scheduler exists (interrupts off): its boot context becomes its idle thread.
pub fn attach_cpu() {
    let cpu_index = this_cpu();
    let mut guard = SCHED.lock();
    let Some(sched) = guard.as_mut() else { return };
    if sched.idle[cpu_index] != NO_THREAD {
        return;
    }
    let mut name = *b"idle\0\0\0\0\0\0\0\0\0\0\0\0";
    name[4] = b'0' + (cpu_index % 10) as u8;
    let id = sched.threads.iter().position(Option::is_none).unwrap_or(sched.threads.len());
    let thread = Box::new(Thread { id, name, state: State::Running, rsp: 0, slot: None, key: 0, wake_at: 0, pinned: Some(cpu_index), on_cpu: true, root: 0, kstack_top: 0, killed: false, written: 0, pid: 0, heap_next: 0, cpu_ticks: 0, last_cpu: 0, fpu: None });
    if id == sched.threads.len() {
        sched.threads.push(Some(thread));
    } else {
        sched.threads[id] = Some(thread);
    }
    sched.idle[cpu_index] = id;
    sched.current[cpu_index] = id;
}

/// Starts a thread on CPU 0 running `entry(argument)`; it is queued, not run yet. Returns its id.
pub fn spawn(name: &str, entry: extern "C" fn(usize), argument: usize) -> Option<usize> {
    spawn_on(name, entry, argument, Some(0), 0)
}

/// Starts a thread that runs a process: `root` is its address space (`vm::create_user_space`), loaded whenever the thread runs.
/// Any CPU may run it; its entry function ends in `process::enter_user`.
pub fn spawn_user(name: &str, entry: extern "C" fn(usize), argument: usize, root: u64) -> Option<usize> {
    spawn_on(name, entry, argument, None, root)
}

/// Like `spawn`, but any CPU may run the thread: it must not touch the unlocked boot-CPU drivers.
pub fn spawn_any(name: &str, entry: extern "C" fn(usize), argument: usize) -> Option<usize> {
    spawn_on(name, entry, argument, None, 0)
}

fn spawn_on(name: &str, entry: extern "C" fn(usize), argument: usize, pinned: Option<usize>, root: u64) -> Option<usize> {
    let mut guard = SCHED.lock();
    let sched = guard.as_mut()?;
    let slot = sched.free_slots.pop().unwrap_or_else(|| {
        sched.next_slot += 1;
        sched.next_slot - 1
    });
    let Some(frames) = pmm::allocate(STACK_PAGES, None) else {
        sched.free_slots.push(slot);
        return None;
    };
    let top = vm::map_stack(slot, STACK_PAGES, frames);
    // The first switch pops r15, r14, r13, r12, rbx, rbp and returns into the trampoline, which calls `entry(argument)`.
    // 72 bytes below the (16-aligned) top leaves rsp 16-aligned at the trampoline's `call`.
    let start = top - 72;
    let words: [u64; 7] = [0, 0, argument as u64, entry as usize as u64, 0, 0, thread_trampoline as usize as u64];
    for (index, word) in words.iter().enumerate() {
        unsafe { ((start + index as u64 * 8) as *mut u64).write(*word) };
    }
    let mut bytes = [0u8; 16];
    let take = name.len().min(15);
    bytes[..take].copy_from_slice(&name.as_bytes()[..take]);
    let id = match sched.threads.iter().position(Option::is_none) {
        Some(free) => free,
        None => {
            sched.threads.push(None);
            sched.threads.len() - 1
        }
    };
    sched.threads[id] = Some(Box::new(Thread { id, name: bytes, state: State::Ready, rsp: start, slot: Some((slot, frames)), key: 0, wake_at: 0, pinned, on_cpu: false, root, kstack_top: top, killed: false, written: 0, pid: 0, heap_next: 0, cpu_ticks: 0, last_cpu: 0, fpu: if root != 0 { crate::fpu::Area::new() } else { None } }));
    sched.queue.push_back(id);
    let mask = idle_cpu_for(sched, pinned, 0);
    drop(guard);
    kick(mask);
    Some(id)
}

/// One idle CPU (not this one, not in `taken`) that may run a thread pinned to `pinned` (any CPU for None), as a bit mask, or 0.
fn idle_cpu_for(sched: &Sched, pinned: Option<usize>, taken: u128) -> u128 {
    let me = this_cpu();
    for cpu in (0..crate::percpu::count()).filter(|&c| c != me && pinned.is_none_or(|p| p == c)) {
        if taken & (1 << cpu) == 0 && sched.idle[cpu] != NO_THREAD && sched.current[cpu] == sched.idle[cpu] {
            return 1 << cpu;
        }
    }
    0
}

/// Wakes the idle CPUs in `mask` with an IPI so they run what was just made ready now, not at their next 10 ms tick. Call
/// after the scheduler lock is released.
fn kick(mask: u128) {
    for cpu in (0..crate::percpu::count()).filter(|&c| mask & (1 << c) != 0) {
        crate::apic::send_ipi(crate::percpu::get(cpu).apic_id, 0x4000 | u32::from(WAKE_VECTOR));
    }
}

/// The wake IPI's handler: the CPU leaves `hlt` and looks at the run queue.
pub fn wake_interrupt() {
    crate::apic::end_of_interrupt();
    switch_away(true);
}

/// Gives the CPU to the next ready thread, if there is one; the caller stays ready.
pub fn yield_now() {
    let flags = cpu::interrupts_save_and_disable();
    switch_away(true);
    cpu::interrupts_restore(flags);
}

/// Picks the next thread and switches to it. Interrupts must be off. With `requeue` the current thread stays ready.
fn switch_away(requeue: bool) {
    let cpu_index = this_cpu();
    let (old, new) = {
        let mut guard = SCHED.lock();
        let Some(sched) = guard.as_mut() else { return };
        let current = sched.current[cpu_index];
        if current == NO_THREAD {
            return;
        }
        let idle = sched.idle[cpu_index];
        let eligible = sched.queue.iter().position(|&id| {
            let thread = sched.threads[id].as_ref().unwrap();
            !thread.on_cpu && thread.pinned.is_none_or(|pin| pin == cpu_index)
        });
        let next = match eligible {
            Some(position) => sched.queue.remove(position).unwrap(),
            None => {
                let state = sched.threads[current].as_ref().unwrap().state;
                if requeue || state == State::Running {
                    return;
                }
                if state == State::Ready {
                    // Woken (on another CPU) between blocking and switching: it is still queued; just carry on.
                    sched.queue.retain(|&id| id != current);
                    sched.threads[current].as_mut().unwrap().state = State::Running;
                    return;
                }
                if idle != NO_THREAD && idle != current {
                    idle
                } else {
                    // The boot CPU's thread 0 is always ready, so only an idle application processor can get here.
                    return;
                }
            }
        };
        if let Some(area) = sched.threads[current].as_ref().and_then(|t| t.fpu.as_ref()) {
            area.save();
        }
        let old_pointer = {
            let thread = sched.threads[current].as_mut().unwrap();
            if thread.state == State::Running {
                thread.state = State::Ready;
            }
            &mut thread.rsp as *mut u64
        };
        if requeue && current != idle {
            sched.queue.push_back(current);
        }
        let thread = sched.threads[next].as_mut().unwrap();
        debug_assert!(thread.state == State::Ready);
        thread.state = State::Running;
        thread.on_cpu = true;
        let new_rsp = thread.rsp;
        sched.previous[cpu_index] = current;
        sched.current[cpu_index] = next;
        sched.switches += 1;
        (old_pointer, new_rsp)
    };
    unsafe { switch_context(old, new) };
    finish_switch();
}

/// Runs on the new stack right after a switch: releases the thread this CPU switched away from if it has exited.
fn finish_switch() {
    let cpu_index = this_cpu();
    let mut released = None;
    {
        let mut guard = SCHED.lock();
        let Some(sched) = guard.as_mut() else { return };
        let previous = core::mem::replace(&mut sched.previous[cpu_index], NO_THREAD);
        if previous != NO_THREAD {
            if let Some(thread) = sched.threads[previous].as_mut() {
                thread.on_cpu = false;
            }
        }
        if previous != NO_THREAD && sched.threads[previous].as_ref().is_some_and(|t| t.state == State::Dead) {
            let thread = sched.threads[previous].take().unwrap();
            if let Some((slot, frames)) = thread.slot {
                released = Some((slot, frames));
            }
        }
    }
    if let Some((slot, frames)) = released {
        // Another CPU may still cache the old mapping: flush it everywhere before the frames or the slot are reused.
        vm::unmap_stack(slot, STACK_PAGES);
        crate::tlb::shootdown(vm::stack_base(slot, STACK_PAGES), STACK_PAGES);
        pmm::free(frames, STACK_PAGES);
        if let Some(sched) = SCHED.lock().as_mut() {
            sched.free_slots.push(slot);
        }
    }
    load_context(cpu_index);
}

/// Makes this CPU's address space and kernel stack (for ring 3 entries) those of the thread it now runs.
fn load_context(cpu_index: usize) {
    let (root, top) = {
        let guard = SCHED.lock();
        let Some(sched) = guard.as_ref() else { return };
        match sched.threads.get(sched.current[cpu_index]).and_then(|t| t.as_ref()) {
            Some(thread) => (thread.root, thread.kstack_top),
            None => return,
        }
    };
    if top != 0 {
        crate::percpu::set_kernel_stack(top);
    }
    let guard = SCHED.lock();
    if let Some(area) = guard.as_ref().and_then(|s| s.threads.get(s.current[cpu_index])).and_then(|t| t.as_ref()).and_then(|t| t.fpu.as_ref()) {
        area.restore();
    }
    drop(guard);
    let wanted = if root == 0 { vm::root() } else { root };
    if cpu::read_cr3() != wanted {
        cpu::write_cr3(wanted);
    }
}

/// The calling thread's process root (0 for a kernel thread).
pub fn current_root() -> u64 {
    let flags = cpu::interrupts_save_and_disable();
    let root = with_current(|t| t.root).unwrap_or(0);
    cpu::interrupts_restore(flags);
    root
}

fn with_current<R>(f: impl FnOnce(&mut Thread) -> R) -> Option<R> {
    let mut guard = SCHED.lock();
    let sched = guard.as_mut()?;
    let current = sched.current[this_cpu()];
    sched.threads.get_mut(current)?.as_mut().map(|t| f(t))
}

/// The calling thread stops using its process's address space (back on the kernel's) and returns the root, to be destroyed.
pub fn detach_root() -> u64 {
    let flags = cpu::interrupts_save_and_disable();
    let root = with_current(|t| core::mem::replace(&mut t.root, 0)).unwrap_or(0);
    cpu::write_cr3(vm::root());
    cpu::interrupts_restore(flags);
    root
}

/// Asks a process to end: it does so the next time it is in the kernel (at the latest on its next timer tick).
pub fn kill(pid: u64) {
    let mut guard = SCHED.lock();
    if let Some(sched) = guard.as_mut() {
        for thread in sched.threads.iter_mut().flatten() {
            if thread.pid == pid {
                thread.killed = true;
            }
        }
    }
}

/// Gives the calling thread its process number.
pub fn set_pid(pid: u64) {
    with_current(|t| t.pid = pid);
}

/// The process heap's next free address (0 before the first allocation) and a way to move it.
pub fn heap_next() -> u64 {
    with_current(|t| t.heap_next).unwrap_or(0)
}

pub fn set_heap_next(address: u64) {
    with_current(|t| t.heap_next = address);
}

pub fn current_pid() -> u64 {
    with_current(|t| t.pid).unwrap_or(0)
}

pub fn current_killed() -> bool {
    with_current(|t| t.killed).unwrap_or(false)
}

/// Counts console bytes a process wrote; returns the running total.
pub fn add_written(bytes: u64) -> u64 {
    with_current(|t| {
        t.written += bytes;
        t.written
    })
    .unwrap_or(0)
}

#[unsafe(no_mangle)]
extern "C" fn sched_finish_switch() {
    finish_switch();
}

/// Ends the calling thread. Its stack is released by the thread that runs next.
pub fn exit() -> ! {
    cpu::interrupts_disable();
    {
        let mut guard = SCHED.lock();
        let sched = guard.as_mut().unwrap();
        let current = sched.current[this_cpu()];
        sched.threads[current].as_mut().unwrap().state = State::Dead;
    }
    switch_away(false);
    unreachable!("a dead thread was resumed");
}

#[unsafe(no_mangle)]
extern "C" fn sched_exit() -> ! {
    exit()
}

/// Called from the timer interrupt (after the end-of-interrupt): wakes sleepers that are due and ends the running thread's slice.
pub fn tick() {
    let now = crate::apic::ticks();
    let mut mask = 0;
    {
        let mut guard = SCHED.lock();
        let Some(sched) = guard.as_mut() else { return };
        let cpu = this_cpu();
        let running = sched.current[cpu];
        if let Some(thread) = sched.threads.get_mut(running).and_then(Option::as_mut) {
            thread.cpu_ticks += 1;
            thread.last_cpu = cpu as u8;
            if running != 0 && running != sched.idle[cpu] && !thread.name.starts_with(b"service\0") {
                sched.busy[cpu] += 1;
            }
        }
        for id in 0..sched.threads.len() {
            if let Some(thread) = sched.threads[id].as_mut() {
                if thread.state == State::Sleeping && thread.wake_at <= now {
                    thread.state = State::Ready;
                    let pinned = thread.pinned;
                    sched.queue.push_back(id);
                    mask |= idle_cpu_for(sched, pinned, mask);
                }
            }
        }
    }
    kick(mask);
    switch_away(true);
}

/// The idle loop of an application processor, forever: waits for the scheduler, becomes its idle thread, then gives way to
/// whatever is ready at every tick.
pub fn idle_loop() -> ! {
    loop {
        if !ready() {
            unsafe { core::arch::asm!("sti; hlt", options(nomem, nostack)) };
            continue;
        }
        let flags = cpu::interrupts_save_and_disable();
        attach_cpu();
        cpu::interrupts_restore(flags);
        break;
    }
    loop {
        yield_now();
        unsafe { core::arch::asm!("sti; hlt", options(nomem, nostack)) };
    }
}

/// Pins the calling thread to CPU `cpu` and moves it there: the unlocked `kernel-core` drivers (USB, audio) share state with
/// their interrupt handlers, which run on the boot CPU, so a tape that uses them must run there too. Not for the idle thread.
pub fn pin_current(cpu: usize) {
    let flags = cpu::interrupts_save_and_disable();
    let already = {
        let mut guard = SCHED.lock();
        let sched = guard.as_mut().unwrap();
        let current = sched.current[this_cpu()];
        let thread = sched.threads[current].as_mut().unwrap();
        let already = thread.pinned == Some(cpu);
        thread.pinned = Some(cpu);
        already
    };
    cpu::interrupts_restore(flags);
    if !already {
        // Asleep it is queued and only `cpu` may take it, so it wakes there.
        while this_cpu() != cpu {
            sleep_ticks(1);
        }
    }
}

/// Sleeps for at least `ticks` timer ticks (10 ms each). Not for the idle thread.
pub fn sleep_ticks(ticks: u64) {
    let flags = cpu::interrupts_save_and_disable();
    {
        let mut guard = SCHED.lock();
        let sched = guard.as_mut().unwrap();
        let current = sched.current[this_cpu()];
        let thread = sched.threads[current].as_mut().unwrap();
        thread.state = State::Sleeping;
        thread.wake_at = crate::apic::ticks() + ticks;
    }
    switch_away(false);
    cpu::interrupts_restore(flags);
}

/// Blocks the calling thread on `key` if `condition()` is true, checked with the scheduler locked so a `wake_all` cannot slip
/// in between the check and the block. Returns when woken (the caller re-checks its condition). Not for the idle thread.
pub fn wait_while(key: usize, condition: impl FnOnce() -> bool) {
    let flags = cpu::interrupts_save_and_disable();
    let blocked = {
        let mut guard = SCHED.lock();
        let sched = guard.as_mut().unwrap();
        if condition() {
            let current = sched.current[this_cpu()];
            let thread = sched.threads[current].as_mut().unwrap();
            thread.state = State::Blocked;
            thread.key = key;
            true
        } else {
            false
        }
    };
    if blocked {
        switch_away(false);
    }
    cpu::interrupts_restore(flags);
}

/// Makes every thread blocked on `key` ready.
pub fn wake_all(key: usize) {
    let mut mask = 0;
    {
        let mut guard = SCHED.lock();
        let Some(sched) = guard.as_mut() else { return };
        for id in 0..sched.threads.len() {
            if let Some(thread) = sched.threads[id].as_mut() {
                if thread.state == State::Blocked && thread.key == key {
                    thread.state = State::Ready;
                    let pinned = thread.pinned;
                    sched.queue.push_back(id);
                    mask |= idle_cpu_for(sched, pinned, mask);
                }
            }
        }
    }
    kick(mask);
}

/// A mutex that puts a contending thread to sleep instead of spinning, so it may be held across `yield_now`, sleeps and long
/// work (a `SpinLock` keeps interrupts off). Never take it from an interrupt handler or the idle thread.
pub struct SleepMutex<T> {
    locked: core::sync::atomic::AtomicBool,
    data: core::cell::UnsafeCell<T>,
}

unsafe impl<T: Send> Sync for SleepMutex<T> {}

pub struct SleepGuard<'a, T> {
    mutex: &'a SleepMutex<T>,
}

impl<T> SleepMutex<T> {
    pub const fn new(value: T) -> Self {
        SleepMutex { locked: core::sync::atomic::AtomicBool::new(false), data: core::cell::UnsafeCell::new(value) }
    }

    /// Takes the mutex if it is free, never sleeping: for callers that must not block (the NIC receive path).
    pub fn try_lock(&self) -> Option<SleepGuard<'_, T>> {
        use core::sync::atomic::Ordering;
        self.locked.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).ok().map(|_| SleepGuard { mutex: self })
    }

    pub fn lock(&self) -> SleepGuard<'_, T> {
        use core::sync::atomic::Ordering;
        loop {
            if self.locked.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_ok() {
                return SleepGuard { mutex: self };
            }
            wait_while(self as *const Self as usize, || self.locked.load(Ordering::Acquire));
        }
    }
}

impl<T> core::ops::Deref for SleepGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.mutex.data.get() }
    }
}

impl<T> core::ops::DerefMut for SleepGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.mutex.data.get() }
    }
}

impl<T> Drop for SleepGuard<'_, T> {
    fn drop(&mut self) {
        self.mutex.locked.store(false, core::sync::atomic::Ordering::Release);
        wake_all(self.mutex as *const SleepMutex<T> as usize);
    }
}

pub fn current_id() -> usize {
    SCHED.lock().as_ref().map_or(0, |s| s.current[this_cpu()])
}

pub fn switches() -> u64 {
    SCHED.lock().as_ref().map_or(0, |s| s.switches)
}

/// Calls `visit(id, name, state)` for every thread.
pub fn for_each(mut visit: impl FnMut(usize, &str, State)) {
    let guard = SCHED.lock();
    if let Some(sched) = guard.as_ref() {
        for thread in sched.threads.iter().flatten() {
            let length = thread.name.iter().position(|&b| b == 0).unwrap_or(16);
            visit(thread.id, core::str::from_utf8(&thread.name[..length]).unwrap_or("?"), thread.state);
        }
    }
}

/// What `top` shows of one thread.
pub struct Info {
    pub id: usize,
    pub name: [u8; 16],
    pub state: State,
    /// The process number, 0 for a kernel thread.
    pub pid: u64,
    /// The CPU it is running on right now.
    pub running_on: Option<usize>,
    /// The CPU it was last seen on.
    pub last_cpu: usize,
    pub pinned: Option<usize>,
    /// CPU time in timer ticks.
    pub ticks: u64,
    /// An idle thread (never shown as load).
    pub idle: bool,
}

impl Info {
    pub fn name(&self) -> &str {
        let length = self.name.iter().position(|&b| b == 0).unwrap_or(16);
        core::str::from_utf8(&self.name[..length]).unwrap_or("?")
    }
}

/// Calls `visit` for every thread with its CPU placement and accumulated CPU time.
pub fn snapshot(mut visit: impl FnMut(&Info)) {
    let guard = SCHED.lock();
    let Some(sched) = guard.as_ref() else { return };
    for thread in sched.threads.iter().flatten() {
        let running_on = (0..MAX_CPUS).find(|&cpu| sched.current[cpu] == thread.id);
        let idle = thread.id == 0 || sched.idle.contains(&thread.id);
        visit(&Info { id: thread.id, name: thread.name, state: thread.state, pid: thread.pid, running_on, last_cpu: thread.last_cpu as usize, pinned: thread.pinned, ticks: thread.cpu_ticks, idle });
    }
}

/// Ticks CPU `cpu` spent on real work since boot (not idle, not the polling service thread).
pub fn busy_ticks(cpu: usize) -> u64 {
    SCHED.lock().as_ref().map_or(0, |sched| sched.busy[cpu])
}
