//! Kernel thread context switching on aarch64: what `sched` asks of the CPU.
//!
//! `switch_context` pushes the callee-saved registers (x19-x30) on the old stack, swaps `sp` and pops them from the new one.
//! A new thread's stack is built so that the first switch "returns" (x30) into `thread_trampoline`, which finishes the switch,
//! unmasks IRQs and calls `entry(argument)` (kept in x19 and x20). x18 is the platform register and is left alone; the kernel is
//! soft-float, so no vector registers need saving.

core::arch::global_asm!(
    r#"
    .text
    .global switch_context
switch_context:                      // x0 = where to store the old sp, x1 = the new sp
    sub sp, sp, #96
    stp x19, x20, [sp, #0]
    stp x21, x22, [sp, #16]
    stp x23, x24, [sp, #32]
    stp x25, x26, [sp, #48]
    stp x27, x28, [sp, #64]
    stp x29, x30, [sp, #80]
    mov x2, sp
    str x2, [x0]
    mov sp, x1
    ldp x19, x20, [sp, #0]
    ldp x21, x22, [sp, #16]
    ldp x23, x24, [sp, #32]
    ldp x25, x26, [sp, #48]
    ldp x27, x28, [sp, #64]
    ldp x29, x30, [sp, #80]
    add sp, sp, #96
    ret

    .global thread_trampoline
thread_trampoline:
    bl sched_finish_switch
    msr daifclr, #2                  // IRQs on
    mov x0, x20
    blr x19
    bl sched_exit
    brk #0
"#
);

unsafe extern "C" {
    fn switch_context(old_sp: *mut u64, new_sp: u64);
    fn thread_trampoline();
}

/// Saves the running thread's registers on its stack (its stack pointer goes to `*old`) and continues the thread whose saved
/// stack pointer is `new`.
pub unsafe fn switch(old: *mut u64, new: u64) {
    unsafe { switch_context(old, new) };
}

/// Builds the first frame of a new thread on the stack ending at `top` (16-aligned); returns the stack pointer to switch to.
pub fn prepare_stack(top: u64, entry: extern "C" fn(usize), argument: usize) -> u64 {
    // Twelve words, as `switch_context` pops them: x19 = entry, x20 = argument, x30 = the trampoline.
    let start = top - 96;
    let mut words = [0u64; 12];
    words[0] = entry as usize as u64;
    words[1] = argument as u64;
    words[11] = thread_trampoline as usize as u64;
    for (index, word) in words.iter().enumerate() {
        unsafe { ((start + index as u64 * 8) as *mut u64).write(*word) };
    }
    start
}

/// The software-generated interrupt that makes an idle CPU leave `wfi` and look at the run queue.
pub const WAKE_SGI: u32 = 0;

/// Wakes CPU `cpu` with a GIC software-generated interrupt.
pub fn send_wake_ipi(cpu: usize) {
    let target = crate::percpu::get(cpu).arch.gic_target.load(core::sync::atomic::Ordering::Relaxed);
    crate::gic::send_sgi(target, WAKE_SGI);
}

pub fn end_wake_interrupt() {}
