//! Message Signaled Interrupts: a device raises an interrupt by *writing* a small message
//! (the data word = the vector) to the local APIC's address, instead of driving a shared
//! wire. No I/O APIC routing, no sharing, no level/edge subtleties: each device owns a
//! vector. This module hands out vectors `BASE..BASE + COUNT`, keeps a handler per vector
//! and counts deliveries (which is also how a driver's self-test knows a message arrived).
//!
//! Handlers run in interrupt context and must only acknowledge the device and note that
//! work is pending; descriptor and protocol parsing stays in the foreground.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

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

// ---- legacy INTx through the I/O APIC (the fallback for devices without MSI) ----
//
// A PCI device's interrupt pin (INTA#-INTD#) reaches the I/O APIC on a pin the board wiring
// and the ACPI `_PRT` method decide; without an AML interpreter the kernel cannot read
// that table. So drivers *probe*: route a candidate pin to a vector, make the device raise
// an interrupt on purpose, and see whether it arrives (`attach_intx` + the driver's own
// self-test). PCI interrupts are level-triggered and active low, and a line may be shared,
// so every handler attached to a vector runs on each interrupt and each must clear its
// own device's cause.

pub const INTX_BASE: u8 = 0x60;
pub const INTX_COUNT: usize = 8;
const SHARERS: usize = 4;
/// More interrupts than this on one vector in a single tick is an interrupt storm (a
/// cause nobody clears on a level-triggered line freezes the machine): mask the pin.
const STORM_LIMIT: u64 = 2000;

/// Set by a handler that found a cause on its device (`claim`); `dispatch_intx` clears it first and counts an
/// interrupt nobody claimed. A line that floats or is driven by an unhandled device shows up as a flood of those.
static CLAIMED: AtomicBool = AtomicBool::new(false);
/// Unclaimed interrupts on one pin within a tick that mean a storm. Far below `STORM_LIMIT`: a flood nobody answers
/// is never legitimate (skylake3's pin 11 gave 1250 per tick and slipped under the general limit).
const UNCLAIMED_LIMIT: u64 = 200;
static UNCLAIMED_TICK: [AtomicU64; INTX_COUNT] = [const { AtomicU64::new(0) }; INTX_COUNT];
static UNCLAIMED_SEEN: [AtomicU64; INTX_COUNT] = [const { AtomicU64::new(0) }; INTX_COUNT];

/// A handler calls this once it has found (and is serving) a cause on its own device.
pub fn claim() {
    CLAIMED.store(true, Ordering::Relaxed);
}

/// The pins to probe for a PCI device's legacy interrupt, best first: GSI 16-23, where Intel and QEMU chipsets route
/// PCI INTx, then the firmware's "interrupt line" (an ISA IRQ number, which on a PCH may be a stray copy of a PIRQ).
/// Entries that cannot be used are `u32::MAX`; the caller still checks the I/O APIC's range and that the pin is
/// proven by a self-test that counts only interrupts the device itself claimed.
pub fn intx_candidates(interrupt_line: u8) -> [u32; 9] {
    let mut candidates = [u32::MAX; 9];
    for (slot, gsi) in (16..24u32).enumerate() {
        candidates[slot] = gsi;
    }
    candidates[8] = u32::from(interrupt_line);
    candidates
}

struct Intx {
    gsi: u32,
    handlers: [Option<(fn(usize), usize)>; SHARERS],
}

static mut INTX: [Option<Intx>; INTX_COUNT] = [const { None }; INTX_COUNT];
static INTX_COUNTS: [AtomicU64; INTX_COUNT] = [const { AtomicU64::new(0) }; INTX_COUNT];
static STORM_TICK: [AtomicU64; INTX_COUNT] = [const { AtomicU64::new(0) }; INTX_COUNT];
static STORM_SEEN: [AtomicU64; INTX_COUNT] = [const { AtomicU64::new(0) }; INTX_COUNT];
static STORMS: AtomicU64 = AtomicU64::new(0);

/// Routes global system interrupt `gsi` (level, active low) to a vector and adds
/// `handler(context)` to it. Returns the vector, or None if the pin cannot be routed or
/// the line is already shared by as many handlers as there is room for.
pub fn attach_intx(gsi: u32, handler: fn(usize), context: usize) -> Option<u8> {
    unsafe {
        let table = &mut *core::ptr::addr_of_mut!(INTX);
        let slot = match table.iter().position(|entry| entry.as_ref().is_some_and(|e| e.gsi == gsi)) {
            Some(slot) => slot,
            None => {
                let slot = table.iter().position(Option::is_none)?;
                let vector = INTX_BASE + slot as u8;
                if !crate::ioapic::route_gsi(gsi, vector, true, true) {
                    return None;
                }
                table[slot] = Some(Intx { gsi, handlers: [None; SHARERS] });
                slot
            }
        };
        let entry = table[slot].as_mut()?;
        let free = entry.handlers.iter().position(Option::is_none)?;
        entry.handlers[free] = Some((handler, context));
        INTX_COUNTS[slot].store(0, Ordering::Relaxed);
        Some(INTX_BASE + slot as u8)
    }
}

/// Removes a handler (e.g. a failed probe); the pin is masked again when none is left.
pub fn detach_intx(vector: u8, context: usize) {
    unsafe {
        let slot = usize::from(vector - INTX_BASE);
        let table = &mut *core::ptr::addr_of_mut!(INTX);
        if let Some(entry) = table[slot].as_mut() {
            for handler in entry.handlers.iter_mut() {
                if handler.is_some_and(|(_, c)| c == context) {
                    *handler = None;
                }
            }
            if entry.handlers.iter().all(Option::is_none) {
                crate::ioapic::mask_gsi(entry.gsi);
                table[slot] = None;
            }
        }
    }
}

pub fn intx_count(vector: u8) -> u64 {
    INTX_COUNTS[usize::from(vector - INTX_BASE)].load(Ordering::Relaxed)
}

pub fn storms() -> u64 {
    STORMS.load(Ordering::Relaxed)
}

pub fn handles_intx(vector: u64) -> bool {
    (u64::from(INTX_BASE)..u64::from(INTX_BASE) + INTX_COUNT as u64).contains(&vector)
}

pub fn dispatch_intx(vector: u64) {
    let slot = (vector - u64::from(INTX_BASE)) as usize;
    INTX_COUNTS[slot].fetch_add(1, Ordering::Relaxed);
    CLAIMED.store(false, Ordering::Relaxed);
    if let Some(entry) = unsafe { (*core::ptr::addr_of!(INTX))[slot].as_ref() } {
        for (handler, context) in entry.handlers.iter().flatten() {
            handler(*context);
        }
        let now = apic::ticks();
        if !CLAIMED.load(Ordering::Relaxed) {
            if UNCLAIMED_TICK[slot].swap(now, Ordering::Relaxed) != now {
                UNCLAIMED_SEEN[slot].store(0, Ordering::Relaxed);
            }
            if UNCLAIMED_SEEN[slot].fetch_add(1, Ordering::Relaxed) > UNCLAIMED_LIMIT {
                crate::ioapic::mask_gsi(entry.gsi);
                STORMS.fetch_add(1, Ordering::Relaxed);
            }
        }
        // Storm guard: count interrupts within the current tick.
        if STORM_TICK[slot].swap(now, Ordering::Relaxed) != now {
            STORM_SEEN[slot].store(0, Ordering::Relaxed);
        }
        if STORM_SEEN[slot].fetch_add(1, Ordering::Relaxed) > STORM_LIMIT {
            crate::ioapic::mask_gsi(entry.gsi);
            STORMS.fetch_add(1, Ordering::Relaxed);
        }
    }
    apic::end_of_interrupt();
}
