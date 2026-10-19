//! Exception vectors. The firmware's table is replaced at once so a fault is reported in words instead of vanishing into
//! the firmware's handler (which no longer exists after ExitBootServices).
//!
//! The table has 16 entries of 0x80 bytes (current EL with SP0 / SPx, lower EL AArch64 / AArch32, each: synchronous, IRQ,
//! FIQ, SError); VBAR must be 2 KiB aligned. Every entry just records which one it was and calls `exception_report`,
//! which prints ESR/ELR/FAR and parks the CPU: nothing here returns yet (no interrupts are enabled, no faults are
//! recoverable). The firmware enters the kernel at EL1 on QEMU and may do so at EL2 on a Pi, so the level is read from
//! CurrentEL and the matching ESR/ELR/FAR/VBAR registers are used.

use core::arch::{asm, global_asm};

global_asm!(
    r#"
    .section .text.vectors, "ax"
    .balign 0x800
    .global __vectors
    __vectors:
    .macro vector index
        .balign 0x80
        mov x0, #\index
        b exception_common
    .endm
    vector 0
    vector 1
    vector 2
    vector 3
    vector 4
    .balign 0x80
    b irq_entry                        // 5: IRQ from the current EL on SP_ELx, the only IRQ we expect
    vector 6
    vector 7
    vector 8
    vector 9
    vector 10
    vector 11
    vector 12
    vector 13
    vector 14
    vector 15
    irq_entry:
        sub sp, sp, #176
        stp x0, x1, [sp, #0]
        stp x2, x3, [sp, #16]
        stp x4, x5, [sp, #32]
        stp x6, x7, [sp, #48]
        stp x8, x9, [sp, #64]
        stp x10, x11, [sp, #80]
        stp x12, x13, [sp, #96]
        stp x14, x15, [sp, #112]
        stp x16, x17, [sp, #128]
        stp x18, x30, [sp, #144]
        bl irq_dispatch
        ldp x18, x30, [sp, #144]
        ldp x16, x17, [sp, #128]
        ldp x14, x15, [sp, #112]
        ldp x12, x13, [sp, #96]
        ldp x10, x11, [sp, #80]
        ldp x8, x9, [sp, #64]
        ldp x6, x7, [sp, #48]
        ldp x4, x5, [sp, #32]
        ldp x2, x3, [sp, #16]
        ldp x0, x1, [sp, #0]
        add sp, sp, #176
        eret
    exception_common:
        bl exception_report
    1:  wfe
        b 1b
    "#
);

pub fn current_el() -> u64 {
    let value: u64;
    unsafe { asm!("mrs {}, CurrentEL", out(reg) value, options(nomem, nostack)) };
    (value >> 2) & 3
}

/// Points VBAR at our table. Call before anything can fault.
pub fn init() {
    unsafe {
        let table: u64;
        asm!("adr {}, __vectors", out(reg) table, options(nomem, nostack));
        if current_el() == 2 {
            asm!("msr vbar_el2, {}", in(reg) table, options(nomem, nostack));
        } else {
            asm!("msr vbar_el1, {}", in(reg) table, options(nomem, nostack));
        }
        asm!("isb", options(nomem, nostack));
    }
}

/// Called from `irq_entry` with the caller-saved registers on the stack: acknowledge, run the handler, end the interrupt.
#[unsafe(no_mangle)]
extern "C" fn irq_dispatch() {
    crate::gic::dispatch();
}

fn class_name(class: u64) -> &'static str {
    match class {
        0x00 => "unknown reason (undefined instruction)",
        0x07 => "FP/SIMD access trapped",
        0x15 => "SVC from AArch64",
        0x20 | 0x21 => "instruction abort",
        0x22 => "PC alignment fault",
        0x24 | 0x25 => "data abort",
        0x26 => "SP alignment fault",
        0x2f => "SError",
        0x3c => "BRK instruction",
        _ => "other",
    }
}

#[unsafe(no_mangle)]
extern "C" fn exception_report(index: u64) -> ! {
    let (esr, elr, far): (u64, u64, u64);
    unsafe {
        if current_el() == 2 {
            asm!("mrs {}, esr_el2", "mrs {}, elr_el2", "mrs {}, far_el2", out(reg) esr, out(reg) elr, out(reg) far, options(nomem, nostack));
        } else {
            asm!("mrs {}, esr_el1", "mrs {}, elr_el1", "mrs {}, far_el1", out(reg) esr, out(reg) elr, out(reg) far, options(nomem, nostack));
        }
    }
    let origin = ["current EL, SP0", "current EL", "lower EL (AArch64)", "lower EL (AArch32)"][(index / 4) as usize];
    let kind = ["synchronous", "IRQ", "FIQ", "SError"][(index % 4) as usize];
    let class = esr >> 26;
    crate::println!();
    crate::println!("EXCEPTION: {kind} from {origin}, running at EL{}.", current_el());
    crate::println!("  ESR {esr:#010x}: class {class:#04x} ({}), ISS {:#x}", class_name(class), esr & 0x1ff_ffff);
    crate::println!("  ELR {elr:#018x} (the faulting instruction)   FAR {far:#018x} (the faulting address, if the class has one)");
    crate::println!("Parking the CPU.");
    loop {
        unsafe { asm!("wfe") };
    }
}
