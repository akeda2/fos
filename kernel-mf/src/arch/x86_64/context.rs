//! Kernel thread context switching on x86-64: what `sched` asks of the CPU.
//!
//! `switch_context` pushes the callee-saved registers on the old stack, swaps `rsp` and pops them off the new one. A new
//! thread's stack is built so that the first switch "returns" into `thread_trampoline`, which finishes the switch, enables
//! interrupts and calls `entry(argument)`.

core::arch::global_asm!(
    r#"
    .text
    .global switch_context
switch_context:
    push rbp
    push rbx
    push r12
    push r13
    push r14
    push r15
    mov [rdi], rsp
    mov rsp, rsi
    pop r15
    pop r14
    pop r13
    pop r12
    pop rbx
    pop rbp
    ret

    .global thread_trampoline
thread_trampoline:
    call sched_finish_switch
    sti
    mov rdi, r13
    call r12
    call sched_exit
    ud2
"#
);

unsafe extern "C" {
    fn switch_context(old_rsp: *mut u64, new_rsp: u64);
    fn thread_trampoline();
}

/// The vector of the IPI that wakes an idle CPU to look at the run queue.
pub const WAKE_VECTOR: u8 = 0x42;

/// Saves the running thread's registers on its stack (its stack pointer goes to `*old`) and continues the thread whose saved
/// stack pointer is `new`.
pub unsafe fn switch(old: *mut u64, new: u64) {
    unsafe { switch_context(old, new) };
}

/// Builds the first frame of a new thread on the stack ending at `top`; returns the stack pointer to switch to.
pub fn prepare_stack(top: u64, entry: extern "C" fn(usize), argument: usize) -> u64 {
    // The first switch pops r15, r14, r13, r12, rbx, rbp and returns into the trampoline, which calls `entry(argument)`.
    // 72 bytes below the (16-aligned) top leaves rsp 16-aligned at the trampoline's `call`.
    let start = top - 72;
    let words: [u64; 7] = [0, 0, argument as u64, entry as usize as u64, 0, 0, thread_trampoline as *const () as u64];
    for (index, word) in words.iter().enumerate() {
        unsafe { ((start + index as u64 * 8) as *mut u64).write(*word) };
    }
    start
}

/// Wakes the idle CPU `cpu` (an index into `percpu`) so it looks at the run queue now.
pub fn send_wake_ipi(cpu: usize) {
    crate::apic::send_ipi(crate::percpu::get(cpu).arch.apic_id, 0x4000 | u32::from(WAKE_VECTOR));
}

/// Acknowledges the wake IPI.
pub fn end_wake_interrupt() {
    crate::apic::end_of_interrupt();
}

