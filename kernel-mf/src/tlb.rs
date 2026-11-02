//! TLB shootdown: making every other CPU forget a range of kernel mappings that has just been removed.
//!
//! `invlpg` only flushes the CPU that runs it, and the TLBs are not coherent. A kernel stack that was unmapped (and whose
//! frames are about to be reused) could still be cached on a CPU that once ran its thread, so the stack's slot and frames
//! are released only after this has returned. The initiator publishes the range, sets a per-CPU "pending" flag for every
//! other online CPU, sends each a fixed IPI on `TLB_VECTOR` and waits until all flags are clear; each target flushes the range
//! and clears its flag. Only one shootdown runs at a time. A CPU waiting for that turn also services its own flag, so two
//! initiators cannot wait for each other with interrupts off. The caller must hold no spinlock another CPU may wait for with
//! interrupts off (the ticket locks in `sync` keep them off), since that CPU could not take the IPI.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::{apic, percpu};

pub const TLB_VECTOR: u8 = 0x41;

static BUSY: AtomicBool = AtomicBool::new(false);
static START: AtomicU64 = AtomicU64::new(0);
static PAGES: AtomicU64 = AtomicU64::new(0);
static PENDING: [AtomicBool; percpu::MAX_CPUS] = [const { AtomicBool::new(false) }; percpu::MAX_CPUS];
pub static SHOOTDOWNS: AtomicU64 = AtomicU64::new(0);

/// Flushes the published range on this CPU if a shootdown asked for it.
fn service(cpu: usize) {
    if PENDING[cpu].load(Ordering::Acquire) {
        let (start, pages) = (START.load(Ordering::Relaxed), PAGES.load(Ordering::Relaxed));
        for page in 0..pages {
            unsafe { core::arch::asm!("invlpg [{}]", in(reg) start + page * 4096, options(nostack, preserves_flags)) };
        }
        PENDING[cpu].store(false, Ordering::Release);
    }
}

/// The IPI handler.
pub fn handle() {
    service(percpu::this().index as usize);
    apic::end_of_interrupt();
}

/// Flushes `pages` 4 KiB pages from `start` on this CPU and on every other online CPU, returning when all have done it.
pub fn shootdown(start: u64, pages: u64) {
    let me = percpu::this().index as usize;
    let count = percpu::count();
    while BUSY.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
        service(me);
        core::hint::spin_loop();
    }
    START.store(start, Ordering::Relaxed);
    PAGES.store(pages, Ordering::Relaxed);
    for page in 0..pages {
        unsafe { core::arch::asm!("invlpg [{}]", in(reg) start + page * 4096, options(nostack, preserves_flags)) };
    }
    for cpu in (0..count).filter(|&c| c != me) {
        let target = percpu::get(cpu);
        if target.online.load(Ordering::Acquire) {
            PENDING[cpu].store(true, Ordering::Release);
            apic::send_ipi(target.apic_id, 0x4000 | u32::from(TLB_VECTOR));
        }
    }
    for cpu in (0..count).filter(|&c| c != me) {
        while PENDING[cpu].load(Ordering::Acquire) {
            service(me);
            core::hint::spin_loop();
        }
    }
    SHOOTDOWNS.fetch_add(1, Ordering::Relaxed);
    BUSY.store(false, Ordering::Release);
}
