//! Message Signaled Interrupts: a device raises an interrupt by *writing* a small message
//! (the data word = the vector) to the local APIC's address, instead of driving a shared
//! wire. No I/O APIC routing, no sharing, no level/edge subtleties: each device owns a
//! vector. This module hands out vectors `BASE..BASE + COUNT`, keeps a handler per vector
//! and counts deliveries (which is also how a driver's self-test knows a message arrived).
//!
//! Handlers run in interrupt context and must only acknowledge the device and note that
//! work is pending; descriptor and protocol parsing stays in the foreground.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::apic;

pub const BASE: u8 = 0x50;
pub const COUNT: usize = 16;

static mut HANDLERS: [Option<fn(usize)>; COUNT] = [None; COUNT];
static mut CONTEXTS: [usize; COUNT] = [0; COUNT];
static COUNTS: [AtomicU64; COUNT] = [const { AtomicU64::new(0) }; COUNT];

/// Claims a free vector for `handler(context)`.
pub fn allocate(handler: fn(usize), context: usize) -> Option<u8> {
    unsafe {
        let handlers = &mut *core::ptr::addr_of_mut!(HANDLERS);
        let slot = handlers.iter().position(Option::is_none)?;
        handlers[slot] = Some(handler);
        CONTEXTS[slot] = context;
        COUNTS[slot].store(0, Ordering::Relaxed);
        Some(BASE + slot as u8)
    }
}

pub fn release(vector: u8) {
    unsafe { (*core::ptr::addr_of_mut!(HANDLERS))[usize::from(vector - BASE)] = None };
}

/// How many messages arrived on `vector`.
pub fn count(vector: u8) -> u64 {
    COUNTS[usize::from(vector - BASE)].load(Ordering::Relaxed)
}

pub fn handles(vector: u64) -> bool {
    (u64::from(BASE)..u64::from(BASE) + COUNT as u64).contains(&vector)
}

/// Called from `interrupt_dispatch` for every vector in range.
pub fn dispatch(vector: u64) {
    let slot = (vector - u64::from(BASE)) as usize;
    COUNTS[slot].fetch_add(1, Ordering::Relaxed);
    if let Some(handler) = unsafe { (*core::ptr::addr_of!(HANDLERS))[slot] } {
        handler(unsafe { CONTEXTS[slot] });
    }
    apic::end_of_interrupt();
}
