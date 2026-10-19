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
