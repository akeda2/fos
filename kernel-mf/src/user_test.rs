//! Ring 3 self-test (slice 8b): five tiny processes, hand-assembled and position independent, exercise the system calls, the
//! pointer checks, exception containment and preemption of user code.

use crate::process::{self, How};
use crate::{apic, pmm, sched};

core::arch::global_asm!(
    r#"
    .text
    .global user_hello_start, user_hello_end
user_hello_start:
    lea rdi, [rip + 2f]
    mov esi, 19
    xor eax, eax
    syscall
    mov eax, 2
    syscall
    lea rdi, [rip + 2f]
    mov esi, 19
    xor eax, eax
    syscall
    mov eax, 1
    mov edi, 7
    syscall
2:  .ascii "Hello from ring 3!\n"
user_hello_end:

    .global user_badptr_start, user_badptr_end
user_badptr_start:
    movabs rdi, 0xffff800000000000
    mov esi, 16
    xor eax, eax
    syscall
    mov rdi, rax
    mov eax, 1
    syscall
user_badptr_end:

    .global user_fault_start, user_fault_end
user_fault_start:
    movabs rax, 0xffff800000000000
    mov byte ptr [rax], 1
user_fault_end:

    .global user_gp_start, user_gp_end
user_gp_start:
    hlt
user_gp_end:

    .global user_spin_start, user_spin_end
user_spin_start:
    inc rax
    jmp user_spin_start
user_spin_end:
"#
);

unsafe extern "C" {
    static user_hello_start: u8;
    static user_hello_end: u8;
    static user_badptr_start: u8;
    static user_badptr_end: u8;
    static user_fault_start: u8;
    static user_fault_end: u8;
    static user_gp_start: u8;
    static user_gp_end: u8;
    static user_spin_start: u8;
    static user_spin_end: u8;
}

fn blob(start: &'static u8, end: &'static u8) -> &'static [u8] {
    let start = start as *const u8;
    unsafe { core::slice::from_raw_parts(start, end as *const u8 as usize - start as usize) }
}

fn halt_a_tick() {
    unsafe { core::arch::asm!("sti", "hlt", options(nomem, nostack)) };
}

pub fn run() {
    let free_before = pmm::stats().free;
    let hello = process::spawn("hello", blob(unsafe { &user_hello_start }, unsafe { &user_hello_end })).expect("hello");
    let badptr = process::spawn("badptr", blob(unsafe { &user_badptr_start }, unsafe { &user_badptr_end })).expect("badptr");
    let fault = process::spawn("fault", blob(unsafe { &user_fault_start }, unsafe { &user_fault_end })).expect("fault");
    let gp = process::spawn("gp", blob(unsafe { &user_gp_start }, unsafe { &user_gp_end })).expect("gp");
    let spin = process::spawn("spin", blob(unsafe { &user_spin_start }, unsafe { &user_spin_end })).expect("spin");
    let start = apic::ticks();
    while apic::ticks() - start < 4 {
        halt_a_tick();
    }
    sched::kill(spin);
    let mut waited = 0;
    while process::outcomes().len() < 5 && waited < 300 {
        halt_a_tick();
        waited += 1;
    }
    // A dead thread's stack is freed by the next thread that runs on its CPU.
    for _ in 0..4 {
        sched::yield_now();
        halt_a_tick();
    }
    let outcomes = process::outcomes();
    let find = |id: usize| outcomes.iter().find(|o| o.thread == id);
    let hello_ok = matches!(find(hello), Some(o) if o.how == How::Exited(7) && o.written == 38);
    let badptr_ok = matches!(find(badptr), Some(o) if o.how == How::Exited(-1));
    let fault_ok = matches!(find(fault), Some(o) if matches!(o.how, How::Fault(14, 0xffff_8000_0000_0000)));
    let gp_ok = matches!(find(gp), Some(o) if matches!(o.how, How::Fault(13, _)));
    let spin_ok = matches!(find(spin), Some(o) if o.how == How::Killed);
    let leaked = free_before.saturating_sub(pmm::stats().free);
    let ok = |b: bool| if b { "ok" } else { "FAILED" };
    println!(
        "Ring 3: hello {}, bad pointer {}, page fault {}, #GP {}, preempted spinner {}, {} frames not returned.",
        ok(hello_ok), ok(badptr_ok), ok(fault_ok), ok(gp_ok), ok(spin_ok), leaked
    );
}
