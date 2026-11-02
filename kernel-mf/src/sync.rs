//! Ticket spinlocks that also keep interrupts off while held.
//!
//! A ticket lock hands the lock over in arrival order, so no CPU starves under contention. Interrupts are disabled for the
//! time the lock is held (and restored to their previous state afterwards): an interrupt handler on the same CPU that
//! wanted the lock would wait for a holder that cannot run until the handler returns.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicU32, Ordering};

pub struct SpinLock<T> {
    next: AtomicU32,
    serving: AtomicU32,
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
        SpinLock { next: AtomicU32::new(0), serving: AtomicU32::new(0), data: UnsafeCell::new(value) }
    }

    pub fn lock(&self) -> Guard<'_, T> {
        let interrupts_were_on = interrupts_on();
        unsafe { core::arch::asm!("cli", options(nomem, nostack)) };
        let ticket = self.next.fetch_add(1, Ordering::Relaxed);
        while self.serving.load(Ordering::Acquire) != ticket {
            core::hint::spin_loop();
        }
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
