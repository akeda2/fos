//! The ring 3 self-test's machine code (see `user_test.rs`): tiny position-independent processes in this CPU's own assembly.
//! Same symbol names on every architecture; `HELLO_BYTES` is the length of the greeting `user_hello` writes twice.

pub const HELLO_BYTES: u64 = 19;

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

    .global user_overflow_start, user_overflow_end
user_overflow_start:                   # pushes until the stack runs into the guard page below it
1:  push rax
    jmp 1b
user_overflow_end:

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
    pub static user_hello_start: u8;
    pub static user_hello_end: u8;
    pub static user_badptr_start: u8;
    pub static user_badptr_end: u8;
    pub static user_fault_start: u8;
    pub static user_fault_end: u8;
    pub static user_overflow_start: u8;
    pub static user_overflow_end: u8;
    pub static user_gp_start: u8;
    pub static user_gp_end: u8;
    pub static user_elf_start: u8;
    pub static user_elf_end: u8;
    pub static user_elf_pointer: u8;
    pub static user_elf_message: u8;
    pub static user_fp_a_start: u8;
    pub static user_fp_a_end: u8;
    pub static user_fp_b_start: u8;
    pub static user_fp_b_end: u8;
    pub static user_avx_start: u8;
    pub static user_avx_end: u8;
    pub static user_spin_start: u8;
    pub static user_spin_end: u8;
}

