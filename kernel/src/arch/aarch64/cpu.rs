//! Core control: halt, wait for interrupt, and the DAIF interrupt mask.

pub fn halt() -> ! {
    loop {
        unsafe { core::arch::asm!("wfe", options(nomem, nostack)) };
    }
}

/// Sleeps until an interrupt or event arrives.
pub fn sleep() {
    unsafe { core::arch::asm!("wfi", options(nomem, nostack)) };
}

pub fn interrupts_enabled() -> bool {
    let daif: u64;
    unsafe { core::arch::asm!("mrs {}, daif", out(reg) daif, options(nomem, nostack)) };
    daif & (1 << 7) == 0
}

pub fn interrupts_enable() {
    unsafe { core::arch::asm!("msr daifclr, #2", options(nomem, nostack)) };
}

pub fn interrupts_disable() {
    unsafe { core::arch::asm!("msr daifset, #2", options(nomem, nostack)) };
}

/// Masks interrupts and returns whether they were enabled, for `interrupts_restore` (a critical section).
pub fn interrupts_save_and_disable() -> bool {
    let was = interrupts_enabled();
    interrupts_disable();
    was
}

pub fn interrupts_restore(was: bool) {
    if was {
        interrupts_enable();
    }
}

/// Makes freshly written code visible to instruction fetch: clean the data cache to the point of unification, then
/// invalidate the instruction cache (the loader does the same for the kernel image).
pub fn sync_code(start: u64, length: u64) {
    let mut address = start & !63;
    while address < start + length {
        unsafe { core::arch::asm!("dc cvau, {}", in(reg) address, options(nostack)) };
        address += 64;
    }
    unsafe { core::arch::asm!("dsb ish", "ic ialluis", "dsb ish", "isb", options(nostack)) };
}

/// Lets code at the current exception level use the FP/SIMD registers (tapes are built hard-float; the kernel is not).
pub fn fp_enable() {
    unsafe {
        let el: u64;
        core::arch::asm!("mrs {}, CurrentEL", out(reg) el, options(nomem, nostack));
        if el >> 2 == 2 {
            // CPTR_EL2.TFP (bit 10) traps FP use; clear it (E2H = 0 layout)
            core::arch::asm!("mrs {t}, cptr_el2", "bic {t}, {t}, #(1 << 10)", "msr cptr_el2, {t}", "isb", t = out(reg) _, options(nostack));
        } else {
            core::arch::asm!("mrs {t}, cpacr_el1", "orr {t}, {t}, #(3 << 20)", "msr cpacr_el1, {t}", "isb", t = out(reg) _, options(nostack));
        }
    }
}
