//! Ring 3 self-test (slice 8b): five tiny processes, hand-assembled and position independent, exercise the system calls, the
//! pointer checks, exception containment and preemption of user code.

use crate::process::{self, How};
use crate::{apic, fpu, pmm, sched};

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

    .global user_elf_start, user_elf_end, user_elf_pointer, user_elf_message
user_elf_start:                        # a tape: rdi = the TapeApi
    sub rsp, 8
    mov r14, rdi
    mov rdi, [rip + user_elf_pointer]  # the relocated pointer to the message
    mov esi, 19
    call [r14 + {PUTS}]
    mov edi, 8192
    call [r14 + {ALLOC}]
    test rax, rax
    jz 1f
    mov byte ptr [rax + 4096], 1
    mov rdi, rax
    mov esi, 8192
    call [r14 + {FREE}]
    mov edi, 4096                       # the address space is usable again
    call [r14 + {ALLOC}]
    test rax, rax
    jz 1f
    call [r14 + {TICKS}]
    add rsp, 8
    ret                                 # back into the shim: exit(0)
1:  mov eax, 1
    mov edi, 2
    syscall
    .balign 8
user_elf_pointer:
    .quad 0
user_elf_message:
    .ascii "Hello from an ELF!\n\0"
user_elf_end:

    .macro fpcheck name, pattern
    .global user_fp_\name\()_start, user_fp_\name\()_end
user_fp_\name\()_start:
    movabs rax, \pattern
    movq xmm0, rax
    punpcklqdq xmm0, xmm0
    mov r12d, 40
1:  mov r13d, 200000
2:  dec r13d
    jnz 2b
    mov eax, 2
    syscall                            # yield: another process loads its own pattern meanwhile
    movq rax, xmm0
    movhlps xmm1, xmm0
    movq rbx, xmm1
    movabs rdx, \pattern
    cmp rax, rdx
    jne 3f
    cmp rbx, rdx
    jne 3f
    dec r12d
    jnz 1b
    mov eax, 1
    xor edi, edi
    syscall
3:  mov eax, 1
    mov edi, 1
    syscall
user_fp_\name\()_end:
    .endm
    fpcheck a, 0x1111111122222222
    fpcheck b, 0x3333333344444444

    .global user_avx_start, user_avx_end
user_avx_start:
    movabs rax, 0x5555555566666666
    movq xmm0, rax
    vpbroadcastq ymm0, xmm0
    mov r12d, 40
1:  mov r13d, 200000
2:  dec r13d
    jnz 2b
    mov eax, 2
    syscall
    vextracti128 xmm1, ymm0, 1         # the upper half must survive the switches too
    movq rax, xmm1
    movabs rdx, 0x5555555566666666
    cmp rax, rdx
    jne 3f
    dec r12d
    jnz 1b
    mov eax, 1
    xor edi, edi
    syscall
3:  mov eax, 1
    mov edi, 1
    syscall
user_avx_end:

    .global user_spin_start, user_spin_end
user_spin_start:
    inc rax
    jmp user_spin_start
user_spin_end:
"#,
    PUTS = const core::mem::offset_of!(fosfor_abi::tape::TapeApi, puts),
    ALLOC = const core::mem::offset_of!(fosfor_abi::tape::TapeApi, alloc),
    FREE = const core::mem::offset_of!(fosfor_abi::tape::TapeApi, free),
    TICKS = const core::mem::offset_of!(fosfor_abi::tape::TapeApi, ticks),
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
    static user_elf_start: u8;
    static user_elf_end: u8;
    static user_elf_pointer: u8;
    static user_elf_message: u8;
    static user_fp_a_start: u8;
    static user_fp_a_end: u8;
    static user_fp_b_start: u8;
    static user_fp_b_end: u8;
    static user_avx_start: u8;
    static user_avx_end: u8;
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

/// A minimal ET_DYN image: ELF header, two program headers (PT_LOAD read/write/execute, PT_DYNAMIC), the dynamic table, one
/// RELATIVE relocation that points the blob's message pointer at the message, then the blob. Entry is the blob's first byte.
fn build_elf() -> alloc::vec::Vec<u8> {
    let code = blob(unsafe { &user_elf_start }, unsafe { &user_elf_end });
    let start = unsafe { &user_elf_start } as *const u8 as usize;
    let pointer_at = unsafe { &user_elf_pointer } as *const u8 as usize - start;
    let message_at = unsafe { &user_elf_message } as *const u8 as usize - start;
    let (dynamic, rela, blob_at) = (64u64 + 2 * 56, 64u64 + 2 * 56 + 48, 64u64 + 2 * 56 + 48 + 24);
    let total = blob_at + code.len() as u64;
    let mut out = alloc::vec::Vec::new();
    out.extend_from_slice(&[0x7f, b'E', b'L', b'F', 2, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    out.extend_from_slice(&3u16.to_le_bytes()); // ET_DYN
    out.extend_from_slice(&62u16.to_le_bytes()); // x86-64
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&blob_at.to_le_bytes()); // entry
    out.extend_from_slice(&64u64.to_le_bytes()); // program headers
    out.extend_from_slice(&0u64.to_le_bytes()); // no sections
    out.extend_from_slice(&0u32.to_le_bytes());
    for half in [64u16, 56, 2, 0, 0, 0] {
        out.extend_from_slice(&half.to_le_bytes());
    }
    let phdr = |out: &mut alloc::vec::Vec<u8>, kind: u32, flags: u32, from: u64, size: u64| {
        out.extend_from_slice(&kind.to_le_bytes());
        out.extend_from_slice(&flags.to_le_bytes());
        for value in [from, from, from, size, size, 0x1000] {
            out.extend_from_slice(&value.to_le_bytes());
        }
    };
    phdr(&mut out, 1, 7, 0, total);
    phdr(&mut out, 2, 6, dynamic, 48);
    for value in [7u64, rela, 8, 24, 0, 0] {
        out.extend_from_slice(&value.to_le_bytes()); // DT_RELA, DT_RELASZ, DT_NULL
    }
    for value in [blob_at + pointer_at as u64, 8, blob_at + message_at as u64] {
        out.extend_from_slice(&value.to_le_bytes()); // offset, R_X86_64_RELATIVE, addend
    }
    out.extend_from_slice(code);
    out
}

pub fn run() {
    let free_before = pmm::stats().free;
    let hello = process::spawn("hello", blob(unsafe { &user_hello_start }, unsafe { &user_hello_end })).expect("hello");
    let badptr = process::spawn("badptr", blob(unsafe { &user_badptr_start }, unsafe { &user_badptr_end })).expect("badptr");
    let fault = process::spawn("fault", blob(unsafe { &user_fault_start }, unsafe { &user_fault_end })).expect("fault");
    let gp = process::spawn("gp", blob(unsafe { &user_gp_start }, unsafe { &user_gp_end })).expect("gp");
    let spin = process::spawn("spin", blob(unsafe { &user_spin_start }, unsafe { &user_spin_end })).expect("spin");
    let elf = process::spawn_elf("elf", &build_elf(), b"").expect("elf");
    let fp_a = blob(unsafe { &user_fp_a_start }, unsafe { &user_fp_a_end });
    let fp_b = blob(unsafe { &user_fp_b_start }, unsafe { &user_fp_b_end });
    let mut fp_ids = alloc::vec![];
    for (name, code) in [("fp-a1", fp_a), ("fp-b1", fp_b), ("fp-a2", fp_a), ("fp-b2", fp_b)] {
        fp_ids.push(process::spawn(name, code).expect("fp process"));
    }
    let avx = (fpu::level() == fosfor_abi::tape::FP_AVX2)
        .then(|| process::spawn("avx", blob(unsafe { &user_avx_start }, unsafe { &user_avx_end })).expect("avx process"));
    let expected = 6 + fp_ids.len() + usize::from(avx.is_some());
    let start = apic::ticks();
    while apic::ticks() - start < 4 {
        halt_a_tick();
    }
    sched::kill(spin);
    let mut waited = 0;
    while process::outcomes().len() < expected && waited < 300 {
        halt_a_tick();
        waited += 1;
    }
    // A dead thread's stack is freed by the next thread that runs on its CPU.
    for _ in 0..4 {
        sched::yield_now();
        halt_a_tick();
    }
    let outcomes = process::outcomes();
    let find = |id: u64| outcomes.iter().find(|o| o.pid == id);
    let hello_ok = matches!(find(hello), Some(o) if o.how == How::Exited(7) && o.written == 38);
    let badptr_ok = matches!(find(badptr), Some(o) if o.how == How::Exited(-1));
    let fault_ok = matches!(find(fault), Some(o) if matches!(o.how, How::Fault(14, 0xffff_8000_0000_0000)));
    let gp_ok = matches!(find(gp), Some(o) if matches!(o.how, How::Fault(13, _)));
    let elf_ok = matches!(find(elf), Some(o) if o.how == How::Exited(0) && o.written == 19);
    let fp_ok = fp_ids.iter().all(|&id| matches!(find(id), Some(o) if o.how == How::Exited(0)))
        && avx.is_none_or(|id| matches!(find(id), Some(o) if o.how == How::Exited(0)));
    let spin_ok = matches!(find(spin), Some(o) if o.how == How::Killed);
    // Fresh stack slots can need a new page-table page (128 KiB stacks: one per ~15 threads); tables are kept for reuse, not leaked.
    let leaked = free_before.saturating_sub(pmm::stats().free).saturating_sub(2);
    let ok = |b: bool| if b { "ok" } else { "FAILED" };
    println!(
        "Ring 3: hello {}, bad pointer {}, page fault {}, #GP {}, preempted spinner {}, ELF {}, SSE{} state {}, {} frames not returned.",
        ok(hello_ok), ok(badptr_ok), ok(fault_ok), ok(gp_ok), ok(spin_ok), ok(elf_ok), if avx.is_some() { "+AVX" } else { "" }, ok(fp_ok), leaked
    );
}
