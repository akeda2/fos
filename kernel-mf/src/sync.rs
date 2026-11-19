//! Ticket spinlocks that also keep interrupts off while held.
//!
//! A ticket lock hands the lock over in arrival order, so no CPU starves under contention. Interrupts are disabled for the
//! time the lock is held (and restored to their previous state afterwards): an interrupt handler on the same CPU that
//! wanted the lock would wait for a holder that cannot run until the handler returns.
//!
//! For the debug tools (`sysrq.rs`, the deadlock report) every lock records where it was taken and by which CPU, each CPU keeps
//! the few locks it holds and the one it waits for, all in plain atomics: a dump can say "CPU 2 waits at net.rs:212 for the lock
//! CPU 0 took at sched.rs:252" without taking any lock itself. A guard is always dropped on the CPU that took it (interrupts stay
//! off while it is held, so the thread cannot be switched away).

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::panic::Location;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use crate::percpu::MAX_CPUS;

static REPORTED: AtomicBool = AtomicBool::new(false);

/// Locks one CPU can be seen holding at once (deeper nesting is held but not listed).
pub const HELD_SLOTS: usize = 4;

/// What one CPU holds and waits for. Lock addresses are 0 in an empty slot.
pub struct CpuLocks {
    pub held: [AtomicUsize; HELD_SLOTS],
    pub held_at: [AtomicPtr<Location<'static>>; HELD_SLOTS],
    pub waiting: AtomicUsize,
    pub waiting_at: AtomicPtr<Location<'static>>,
    /// The kernel tick the wait began.
    pub waiting_since: AtomicU64,
}

const NO_LOCKS: CpuLocks = CpuLocks {
    held: [const { AtomicUsize::new(0) }; HELD_SLOTS],
    held_at: [const { AtomicPtr::new(core::ptr::null_mut()) }; HELD_SLOTS],
    waiting: AtomicUsize::new(0),
    waiting_at: AtomicPtr::new(core::ptr::null_mut()),
    waiting_since: AtomicU64::new(0),
};

pub static CPU_LOCKS: [CpuLocks; MAX_CPUS] = [NO_LOCKS; MAX_CPUS];

pub struct SpinLock<T> {
    next: AtomicU32,
    serving: AtomicU32,
    holder: AtomicPtr<Location<'static>>,
    /// The CPU (index + 1) that holds it, 0 while free.
    owner: AtomicU32,
    data: UnsafeCell<T>,
}

/// What a dump can say about a lock at `address` without touching it beyond these atomics: where it was taken and by
/// which CPU (`None` when it is free).
pub struct LockState {
    pub taken_at: Option<&'static Location<'static>>,
    pub cpu: Option<usize>,
}

unsafe impl<T: Send> Sync for SpinLock<T> {}

pub struct Guard<'a, T> {
    lock: &'a SpinLock<T>,
    interrupts_were_on: bool,
}

impl<T> SpinLock<T> {
    pub const fn new(value: T) -> Self {
        SpinLock { next: AtomicU32::new(0), serving: AtomicU32::new(0), holder: AtomicPtr::new(core::ptr::null_mut()), owner: AtomicU32::new(0), data: UnsafeCell::new(value) }
    }

    /// The lock only if it is free right now, never waiting: for the debug dump, which must not hang on a lock a stuck CPU
    /// holds.
    #[track_caller]
    pub fn try_lock(&self) -> Option<Guard<'_, T>> {
        let interrupts_were_on = crate::cpu::interrupts_enabled();
        crate::cpu::interrupts_disable();
        let serving = self.serving.load(Ordering::Acquire);
        if self.next.compare_exchange(serving, serving.wrapping_add(1), Ordering::Acquire, Ordering::Relaxed).is_err() {
            if interrupts_were_on {
                crate::cpu::interrupts_enable();
            }
            return None;
        }
        self.taken(Location::caller());
        Some(Guard { lock: self, interrupts_were_on })
    }

    /// Who holds the lock, as far as the record goes.
    pub fn state(&self) -> LockState {
        let held = self.next.load(Ordering::Relaxed) != self.serving.load(Ordering::Relaxed);
        if !held {
            return LockState { taken_at: None, cpu: None };
        }
        let owner = self.owner.load(Ordering::Relaxed) as usize;
        LockState { taken_at: unsafe { self.holder.load(Ordering::Relaxed).as_ref() }, cpu: owner.checked_sub(1) }
    }

    fn taken(&self, place: &'static Location<'static>) {
        self.holder.store(place as *const _ as *mut _, Ordering::Relaxed);
        let cpu = crate::percpu::index();
        self.owner.store(cpu as u32 + 1, Ordering::Relaxed);
        let locks = &CPU_LOCKS[cpu];
        let address = self as *const Self as usize;
        if let Some(slot) = locks.held.iter().position(|held| held.load(Ordering::Relaxed) == 0) {
            locks.held_at[slot].store(place as *const _ as *mut _, Ordering::Relaxed);
            locks.held[slot].store(address, Ordering::Release);
        }
    }

    fn released(&self) {
        self.owner.store(0, Ordering::Relaxed);
        let address = self as *const Self as usize;
        let locks = &CPU_LOCKS[crate::percpu::index()];
        if let Some(slot) = locks.held.iter().position(|held| held.load(Ordering::Relaxed) == address) {
            locks.held[slot].store(0, Ordering::Release);
        }
    }

    #[track_caller]
    pub fn lock(&self) -> Guard<'_, T> {
        let interrupts_were_on = crate::cpu::interrupts_enabled();
        crate::cpu::interrupts_disable();
        let ticket = self.next.fetch_add(1, Ordering::Relaxed);
        if self.serving.load(Ordering::Acquire) != ticket {
            let locks = &CPU_LOCKS[crate::percpu::index()];
            locks.waiting_at.store(Location::caller() as *const _ as *mut _, Ordering::Relaxed);
            locks.waiting_since.store(crate::time::ticks(), Ordering::Relaxed);
            locks.waiting.store(self as *const Self as usize, Ordering::Release);
            let mut spins = 0u32;
            let mut since = 0u64;
            while self.serving.load(Ordering::Acquire) != ticket {
                core::hint::spin_loop();
                spins = spins.wrapping_add(1);
                if spins & 0xffff == 0 {
                    let now = crate::cpu::cycles();
                    if since == 0 {
                        since = now;
                    } else if now.wrapping_sub(since) > crate::cpu::DEADLOCK_CYCLES {
                        deadlock(Location::caller(), self.holder.load(Ordering::Relaxed), self.owner.load(Ordering::Relaxed));
                    }
                }
            }
            locks.waiting.store(0, Ordering::Release);
        }
        self.taken(Location::caller());
        Guard { lock: self, interrupts_were_on }
    }

    /// Opens the lock whoever holds it. For the crash paths only: a CPU that faults while printing must still be able to say so.
    ///
    /// # Safety
    /// The previous holder must never run again (or the data it guards must be expendable).
    pub unsafe fn force_unlock(&self) {
        self.owner.store(0, Ordering::Relaxed);
        self.serving.store(self.next.load(Ordering::Relaxed), Ordering::Release);
    }
}

impl<T> Deref for Guard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for Guard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T> Drop for Guard<'_, T> {
    fn drop(&mut self) {
        self.lock.released();
        self.lock.serving.fetch_add(1, Ordering::Release);
        if self.interrupts_were_on {
            crate::cpu::interrupts_enable();
        }
    }
}

/// A lock has been waited for too long. Interrupts are off on this CPU, so the watchdog (a timer tick) can never fire: say who
/// waits and who holds, write a crash record and reset, instead of freezing the machine for good.
#[cold]
fn deadlock(waiter: &Location<'static>, holder: *mut Location<'static>, owner: u32) -> ! {
    if REPORTED.swap(true, Ordering::AcqRel) {
        loop {
            core::hint::spin_loop();
        }
    }
    crate::serial::break_lock();
    crate::println!("\n*** lock deadlock: a CPU waited for a lock at {waiter}");
    match unsafe { holder.as_ref() } {
        Some(place) if owner != 0 => crate::println!("*** CPU {} took the lock at {place} and never released it", owner - 1),
        Some(place) => crate::println!("*** the lock was taken at {place} and never released"),
        None => crate::println!("*** the lock has no recorded holder"),
    }
    let mut frame: crate::trap::Frame = unsafe { core::mem::zeroed() };
    frame.vector = crate::debug::DEADLOCK_VECTOR;
    frame.rip = waiter.line() as u64;
    crate::trap::fatal_reset(&frame)
}
