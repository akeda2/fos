//! EL0 entry and exit on aarch64 (`process.rs` and `tapeapi.rs` are the portable halves).
//!
//! Entering the kernel from EL0: an exception always lands on SP_EL1, so the invariant is that SP_EL1 equals the running thread's
//! kernel stack top whenever it is in EL0 (`enter_user_mode` sets it from `set_kernel_stack`, and every return to EL0 pops the
//! frame it pushed). The two vectors that matter (lower EL, AArch64: synchronous 8 and IRQ 9) save every register in a
//! `trap::Frame` on that stack and call Rust; `svc #0` is a system call (number in x8, arguments in x0-x5, result in x0), any
//! other synchronous exception (an abort, an undefined instruction) kills only the process. The same return path serves both.
//!
//! SP_EL0 is saved in the frame, so it needs no care in the context switch; TPIDR_EL0 is not used by any tape yet.
//! The kernel does not use PAN: a syscall reads user memory straight through the process's own mappings, after
//! `vm::user_range_ok`; SCTLR_EL1.SPAN is set so an exception does not turn PAN on behind its back (`init_cpu`).

use core::sync::atomic::Ordering::Relaxed;

use crate::trap::Frame;

core::arch::global_asm!(
    r#"
    .macro save_frame
    sub sp, sp, #304
    stp x0, x1, [sp, #0]
    stp x2, x3, [sp, #16]
    stp x4, x5, [sp, #32]
    stp x6, x7, [sp, #48]
    stp x8, x9, [sp, #64]
    stp x10, x11, [sp, #80]
    stp x12, x13, [sp, #96]
    stp x14, x15, [sp, #112]
    stp x16, x17, [sp, #128]
    stp x18, x19, [sp, #144]
    stp x20, x21, [sp, #160]
    stp x22, x23, [sp, #176]
    stp x24, x25, [sp, #192]
    stp x26, x27, [sp, #208]
    stp x28, x29, [sp, #224]
    str x30, [sp, #240]
    mrs x0, sp_el0
    mrs x1, elr_el1
    mrs x2, spsr_el1
    mrs x3, esr_el1
    mrs x4, far_el1
    str x0, [sp, #248]
    stp x1, x2, [sp, #256]
    stp x3, x4, [sp, #272]
    .endm

    .text
    .global lower_sync_entry, lower_irq_entry
lower_sync_entry:
    save_frame
    mov x0, #8
    str x0, [sp, #288]
    mov x0, sp
    bl el0_sync_dispatch
    b el0_return
lower_irq_entry:
    save_frame
    mov x0, #9
    str x0, [sp, #288]
    mov x0, sp
    bl el0_irq_dispatch
    b el0_return

el0_return:                          // back to EL0 with the (possibly updated) frame on top of the stack
    msr daifset, #2
    ldr x0, [sp, #248]
    msr sp_el0, x0
    ldp x1, x2, [sp, #256]
    msr elr_el1, x1
    msr spsr_el1, x2
    ldp x0, x1, [sp, #0]
    ldp x2, x3, [sp, #16]
    ldp x4, x5, [sp, #32]
    ldp x6, x7, [sp, #48]
    ldp x8, x9, [sp, #64]
    ldp x10, x11, [sp, #80]
    ldp x12, x13, [sp, #96]
    ldp x14, x15, [sp, #112]
    ldp x16, x17, [sp, #128]
    ldp x18, x19, [sp, #144]
    ldp x20, x21, [sp, #160]
    ldp x22, x23, [sp, #176]
    ldp x24, x25, [sp, #192]
    ldp x26, x27, [sp, #208]
    ldp x28, x29, [sp, #224]
    ldr x30, [sp, #240]
    add sp, sp, #304
    eret

    .global enter_user_mode
enter_user_mode:                     // x0 entry, x1 user stack top, x2 and x3 the entry's arguments, x4 this thread's kernel stack top
    mov sp, x4
    msr sp_el0, x1
    msr elr_el1, x0
    msr spsr_el1, xzr                // EL0t, all of DAIF clear: interrupts on
    mov x0, x2
    mov x1, x3
    mov x2, xzr
    mov x3, xzr
    mov x4, xzr
    mov x5, xzr
    mov x6, xzr
    mov x7, xzr
    mov x8, xzr
    mov x9, xzr
    mov x10, xzr
    mov x11, xzr
    mov x12, xzr
    mov x13, xzr
    mov x14, xzr
    mov x15, xzr
    mov x16, xzr
    mov x17, xzr
    mov x18, xzr
    mov x19, xzr
    mov x20, xzr
    mov x21, xzr
    mov x22, xzr
    mov x23, xzr
    mov x24, xzr
    mov x25, xzr
    mov x26, xzr
    mov x27, xzr
    mov x28, xzr
    mov x29, xzr
    mov x30, xzr
    eret

    .balign 16
    .global tape_shim_start, tape_shim_stubs, tape_shim_end, tape_thread_start
tape_shim_start:                     // entered with x0 = the table, x1 = the tape's entry point
    blr x1
    mov x8, #1                       // the tape returned: exit(0)
    mov x0, #0
    svc #0
    brk #0
    .balign 16
tape_thread_start:                   // another thread: x0 = its entry point, x1 = its argument (the stack is 16-aligned)
    mov x2, x0
    mov x0, x1
    blr x2
    mov x8, #5                       // the entry returned: end this thread only
    svc #0
    brk #0
    .balign 16
tape_shim_stubs:
    .set slot, 0
    .rept 192
    mov x8, #0x100 + slot
    svc #0
    ret
    .balign 16
    .set slot, slot + 1
    .endr
tape_shim_end:
"#
);

unsafe extern "C" {
    fn enter_user_mode(entry: u64, stack_top: u64, argument: u64, second: u64, kernel_stack: u64) -> !;
}

/// Starts running user code: `entry(argument, second)` on the stack ending at `stack_top`. Never returns.
pub fn enter(entry: u64, stack_top: u64, argument: u64, second: u64) -> ! {
    let kernel_stack = crate::percpu::this().arch.kernel_stack.load(Relaxed);
    unsafe { enter_user_mode(entry, stack_top, argument, second, kernel_stack) }
}

#[unsafe(no_mangle)]
extern "C" fn el0_sync_dispatch(frame: &mut Frame) {
    if frame.esr >> 26 == 0x15 {
        crate::process::syscall(frame);
    } else {
        crate::process::fault(frame);
    }
    crate::process::check_killed();
}

#[unsafe(no_mangle)]
extern "C" fn el0_irq_dispatch(_frame: &mut Frame) {
    crate::gic::dispatch();
    crate::process::check_killed();
}

/// Per-CPU setup: FP/SIMD on, and no implicit PAN (see the module comment).
pub fn init_cpu() {
    crate::fpu::init_cpu();
    let features: u64;
    unsafe { core::arch::asm!("mrs {}, id_aa64mmfr1_el1", out(reg) features, options(nomem, nostack)) };
    if (features >> 20) & 0xf != 0 {
        unsafe {
            core::arch::asm!(
                "mrs {t}, sctlr_el1", "orr {t}, {t}, #(1 << 23)", "msr sctlr_el1, {t}", "isb",
                "msr s3_0_c4_c2_3, xzr", // PAN = 0
                t = out(reg) _, options(nostack)
            );
        }
    }
}

/// Makes freshly written code visible to instruction fetch at EL0 (data cache clean, instruction cache invalidate).
pub fn sync_code(address: u64, length: u64) {
    crate::cpu::sync_code(address, length);
}

/// Runs `f`, which touches user memory; nothing to lift here (see the module comment).
pub fn user_access<R>(f: impl FnOnce() -> R) -> R {
    f()
}

/// ELF `e_machine` and the RELATIVE relocation type of tapes for this CPU (the self-test builds an image by hand).
pub const ELF_MACHINE: u16 = 183;
pub const RELATIVE_RELOCATION: u64 = 1027;
/// The exception classes `How::Fault` carries: ESR_EL1's EC for a data abort from EL0, and for an undefined instruction.
pub const PAGE_FAULT: u64 = 0x24;
pub const ILLEGAL_INSTRUCTION: u64 = 0x00;

/// How the self-test names things on this CPU.
pub const PRIVILEGE: &str = "EL0";
pub const ILLEGAL_NAME: &str = "undefined instruction";
pub const FP_NAME: &str = "NEON";

/// Lets one timer tick arrive: enable interrupts and wait for one.
pub fn halt_a_tick() {
    unsafe { core::arch::asm!("msr daifclr, #2", "wfi", options(nomem, nostack)) };
}
