//! The EL0 self-test's machine code (see `user_test.rs`): tiny position-independent processes in AArch64 assembly. Same symbol
//! names as the x86 file. System calls: number in x8 (0 write, 1 exit, 2 yield), arguments x0-x5. Every blob starts 16-aligned
//! so its literals keep their alignment when the kernel copies it to a page.

pub const HELLO_BYTES: u64 = 16;

core::arch::global_asm!(
    r#"
    .arch_extension fp
    .arch_extension simd
    .text
    .balign 16
    .global user_hello_start, user_hello_end
user_hello_start:
    adr x0, 2f
    mov x1, #16
    mov x8, #0
    svc #0
    mov x8, #2
    svc #0
    adr x0, 2f
    mov x1, #16
    mov x8, #0
    svc #0
    mov x8, #1
    mov x0, #7
    svc #0
2:  .ascii "Hello from EL0!\n"
user_hello_end:

    .balign 16
    .global user_badptr_start, user_badptr_end
user_badptr_start:
    movz x0, #0xffff, lsl #48
    mov x1, #16
    mov x8, #0
    svc #0                           // refused: x0 = -1, which becomes the exit code
    mov x8, #1
    svc #0
user_badptr_end:

    .balign 16
    .global user_fault_start, user_fault_end
user_fault_start:
    movz x0, #0x8000, lsl #32
    movk x0, #0xffff, lsl #48
    strb w1, [x0]
user_fault_end:

    .balign 16
    .global user_overflow_start, user_overflow_end
user_overflow_start:                 // pushes until the stack runs into the guard page below it
1:  str x0, [sp, #-16]!
    b 1b
user_overflow_end:

    .balign 16
    .global user_gp_start, user_gp_end
user_gp_start:
    udf #0
user_gp_end:

    .balign 16
    .global user_elf_start, user_elf_end, user_elf_pointer, user_elf_message
user_elf_start:                      // a tape: x0 = the TapeApi
    stp x29, x30, [sp, #-32]!
    str x19, [sp, #16]
    mov x19, x0
    ldr x0, user_elf_pointer         // the relocated pointer to the message
    mov x1, #19
    ldr x2, [x19, {PUTS}]
    blr x2
    mov x0, #8192
    ldr x2, [x19, {ALLOC}]
    blr x2
    cbz x0, 1f
    mov w3, #1
    add x4, x0, #4096
    strb w3, [x4]
    mov x1, #8192
    ldr x2, [x19, {FREE}]
    blr x2
    mov x0, #4096                    // the address space is usable again
    ldr x2, [x19, {ALLOC}]
    blr x2
    cbz x0, 1f
    ldr x2, [x19, {TICKS}]
    blr x2
    ldr x19, [sp, #16]
    ldp x29, x30, [sp], #32
    ret                              // back into the shim: exit(0)
1:  mov x8, #1
    mov x0, #2
    svc #0
    .balign 8
user_elf_pointer:
    .quad 0
user_elf_message:
    .ascii "Hello from an ELF!\n\0"
user_elf_end:

    .macro fpcheck name, pattern
    .balign 16
    .global user_fp_\name\()_start, user_fp_\name\()_end
user_fp_\name\()_start:
    ldr x0, 5f
    dup v0.2d, x0
    mov x12, #40
1:  movz x13, #0x0d40
    movk x13, #3, lsl #16            // 200000
2:  subs x13, x13, #1
    b.ne 2b
    mov x8, #2
    svc #0                           // yield: another process loads its own pattern meanwhile
    mov x1, v0.d[0]
    mov x2, v0.d[1]
    ldr x3, 5f
    cmp x1, x3
    b.ne 3f
    cmp x2, x3
    b.ne 3f
    subs x12, x12, #1
    b.ne 1b
    mov x8, #1
    mov x0, #0
    svc #0
3:  mov x8, #1
    mov x0, #1
    svc #0
    .balign 8
5:  .quad \pattern
user_fp_\name\()_end:
    .endm
    fpcheck a, 0x1111111122222222
    fpcheck b, 0x3333333344444444

    .balign 16
    .global user_spin_start, user_spin_end
user_spin_start:
    add x0, x0, #1
    b user_spin_start
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
    pub static user_spin_start: u8;
    pub static user_spin_end: u8;
}
