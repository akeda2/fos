//! The system timer on the ARM generic timer: a free-running 64-bit counter (CNTVCT_EL0) at the frequency the firmware
//! programs into CNTFRQ_EL0 (62.5 MHz on QEMU, 54 MHz on a Pi 4). It needs no interrupt: `ticks` is the counter scaled to
//! `TICK_HZ`, the same units the x86 code gets from its APIC tick, so portable code (`crate::timer`) is unchanged.

use core::arch::asm;

pub const TICK_HZ: u64 = 100;

pub fn counter() -> u64 {
    let value: u64;
    unsafe { asm!("isb", "mrs {}, cntvct_el0", out(reg) value, options(nomem, nostack)) };
    value
}

pub fn frequency() -> u64 {
    let value: u64;
    unsafe { asm!("mrs {}, cntfrq_el0", out(reg) value, options(nomem, nostack)) };
    value.max(1)
}

pub fn micros() -> u64 {
    (u128::from(counter()) * 1_000_000 / u128::from(frequency())) as u64
}

pub fn ticks() -> u64 {
    micros() / (1_000_000 / TICK_HZ)
}

pub fn busy_wait_ms(milliseconds: u64) {
    let start = micros();
    while micros().wrapping_sub(start) < milliseconds * 1000 {
        core::hint::spin_loop();
    }
}

pub fn wait_ticks(count: u64) {
    let start = ticks();
    while ticks().wrapping_sub(start) < count {
        core::hint::spin_loop();
    }
}
