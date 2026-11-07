//! Ticket spinlocks that also keep interrupts off while held.
//!
//! A ticket lock hands the lock over in arrival order, so no CPU starves under contention. Interrupts are disabled for the
//! time the lock is held (and restored to their previous state afterwards): an interrupt handler on the same CPU that
//! wanted the lock would wait for a holder that cannot run until the handler returns.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::panic::Location;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};

/// TSC cycles a CPU may wait for one lock before it is called a deadlock (several seconds on any real CPU: a lock is held for microseconds).
const DEADLOCK_CYCLES: u64 = 20_000_000_000;
static REPORTED: AtomicBool = AtomicBool::new(false);

pub struct SpinLock<T> {
    next: AtomicU32,
    serving: AtomicU32,
    holder: AtomicPtr<Location<'static>>,
    data: UnsafeCell<T>,
}

unsafe impl<T: Send> Sync for SpinLock<T> {}

pub struct Guard<'a, T> {
    lock: &'a SpinLock<T>,
    interrupts_were_on: bool,
}

fn interrupts_on() -> bool {
    let flags: u64;
    unsafe { core::arch::asm!("pushfq", "pop {}", out(reg) flags, options(nomem, preserves_flags)) };
    flags & (1 << 9) != 0
}

impl<T> SpinLock<T> {
    pub const fn new(value: T) -> Self {
        SpinLock { next: AtomicU32::new(0), serving: AtomicU32::new(0), holder: AtomicPtr::new(core::ptr::null_mut()), data: UnsafeCell::new(value) }
    }

    #[track_caller]
    pub fn lock(&self) -> Guard<'_, T> {
        let interrupts_were_on = interrupts_on();
        unsafe { core::arch::asm!("cli", options(nomem, nostack)) };
        let ticket = self.next.fetch_add(1, Ordering::Relaxed);
        if self.serving.load(Ordering::Acquire) != ticket {
            let mut spins = 0u32;
            let mut since = 0u64;
            while self.serving.load(Ordering::Acquire) != ticket {
                core::hint::spin_loop();
                spins = spins.wrapping_add(1);
                if spins & 0xffff == 0 {
                    let now = crate::cpu::rdtsc();
                    if since == 0 {
                        since = now;
                    } else if now.wrapping_sub(since) > DEADLOCK_CYCLES {
                        deadlock(Location::caller(), self.holder.load(Ordering::Relaxed));
                    }
                }
            }
        }
        self.holder.store(Location::caller() as *const _ as *mut _, Ordering::Relaxed);
        Guard { lock: self, interrupts_were_on }
    }

    /// Opens the lock whoever holds it. For the crash paths only: a CPU that faults while printing must still be able to say so.
    ///
    /// # Safety
    /// The previous holder must never run again (or the data it guards must be expendable).
    pub unsafe fn force_unlock(&self) {
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
        self.lock.serving.fetch_add(1, Ordering::Release);
        if self.interrupts_were_on {
            unsafe { core::arch::asm!("sti", options(nomem, nostack)) };
        }
    }
}

/// A lock has been waited for too long. Interrupts are off on this CPU, so the watchdog (a timer tick) can never fire: say who
/// waits and who holds, write a crash record and reset, instead of freezing the machine for good.
#[cold]
fn deadlock(waiter: &Location<'static>, holder: *mut Location<'static>) -> ! {
    if REPORTED.swap(true, Ordering::AcqRel) {
        loop {
            core::hint::spin_loop();
        }
    }
    crate::serial::break_lock();
    crate::println!("\n*** lock deadlock: a CPU waited for a lock at {waiter}");
    match unsafe { holder.as_ref() } {
        Some(place) => crate::println!("*** the lock was taken at {place} and never released"),
        None => crate::println!("*** the lock has no recorded holder"),
    }
    let mut frame: crate::trap::Frame = unsafe { core::mem::zeroed() };
    frame.vector = crate::debug::DEADLOCK_VECTOR;
    frame.rip = waiter.line() as u64;
    crate::trap::fatal_reset(&frame)
}
