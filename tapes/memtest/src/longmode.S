/*
 * longmode.S -- the 64-bit half of memtest: tests RAM above 4 GiB, which a
 * 32-bit kernel without PAE cannot address at all.
 *
 * `memtest64_call(job)` (cdecl, called from longmode.rs) makes one short,
 * fully reversible excursion into 64-bit mode, runs one primitive operation
 * from the job over a window of physical memory, and returns in ordinary
 * 32-bit protected mode with every piece of CPU state it touched restored.
 * It follows tapes/longmode64_shim.S step for step (that sequence is
 * confirmed on real hardware), except that it is called many times, each time
 * for a slice of a few tens of MiB, so the 32-bit caller can poll the keyboard,
 * pet the kernel watchdog and serve the network between slices. Interrupts
 * (including the 100 Hz timer) are off for each excursion, so a slice has to be
 * short; NMIs are not masked and would find a 32-bit IDT.
 *
 * Job layout (u64 fields; keep in step with `Job` in longmode.rs):
 *    0 op   8 base   16 qwords   24 arg0   32 arg1   40 arg2
 *   48 error count   56 records stored   64 records[16] of (address,
 *   expected, actual)   448 per-bit flip counts[64]   960 PML4 address
 *
 * Operations (op in the first field):
 *   0 fill with pattern arg0        1 verify pattern arg0
 *   2 fill with address ^ arg1      3 verify address ^ arg1
 *   4 fill xorshift64(seed arg0) ^ arg1    5 verify the same stream
 *   6 sweep: each qword must be arg0, then becomes arg1;
 *     ascending if arg2 != 0, otherwise descending
 *   7 flip bits arg1 of the qword at address arg0 (fault injection)
 *
 * The 64-bit code touches no data symbols (everything lives in the job), so
 * it needs no RIP-relative addressing, and uses no SSE, so CR4 needs only PAE.
 */
.section .text.memtest64, "ax"
.code32
.global memtest64_call

memtest64_call:
    movl 4(%esp), %eax
    pushl %ebp
    pushl %edi
    pushl %esi
    pushl %ebx
    movl %eax, %ebx                  # the job pointer rides along in %ebx

    cli
    pushfl
    popl m64_eflags
    sgdt m64_gdtr
    movl %cr0, %eax
    movl %eax, m64_cr0
    movl %cr3, %eax
    movl %eax, m64_cr3
    movl %cr4, %eax
    movl %eax, m64_cr4
    movl $0xc0000080, %ecx           # IA32_EFER
    rdmsr
    movl %eax, m64_efer_lo
    movl %edx, m64_efer_hi

    # The kernel's non-PAE paging must be off while CR4.PAE changes.
    movl %cr0, %eax
    andl $0x7fffffff, %eax
    movl %eax, %cr0
    movl %cr4, %eax
    orl $0x20, %eax                  # PAE
    movl %eax, %cr4
    movl 960(%ebx), %eax             # PML4 (the tables live below 4 GiB)
    movl %eax, %cr3
    movl $0xc0000080, %ecx
    rdmsr
    orl $0x100, %eax                 # EFER.LME
    wrmsr
    movl %cr0, %eax
    orl $0x80000000, %eax            # CR0.PG: now in compatibility mode
    movl %eax, %cr0
    lgdt m64_gdt_descriptor
    ljmp $0x08, $m64_long_entry

.code64
m64_long_entry:
    movl %ebx, %r12d                 # job pointer, zero-extended
    cld
    movq 0(%r12), %rax
    cmpq $0, %rax
    je m64_op_fill_pattern
    cmpq $1, %rax
    je m64_op_verify_pattern
    cmpq $2, %rax
    je m64_op_fill_addr
    cmpq $3, %rax
    je m64_op_verify_addr
    cmpq $4, %rax
    je m64_op_fill_random
    cmpq $5, %rax
    je m64_op_verify_random
    cmpq $6, %rax
    je m64_op_sweep
    cmpq $7, %rax
    je m64_op_flip
    jmp m64_done

m64_op_fill_pattern:
    movq 8(%r12), %rdi
    movq 16(%r12), %rcx
    movq 24(%r12), %rax
    rep stosq
    jmp m64_done

m64_op_verify_pattern:
    movq 8(%r12), %rdi
    movq 16(%r12), %rcx
    movq 24(%r12), %rax
    testq %rcx, %rcx
    jz m64_done
1:  movq (%rdi), %rdx
    cmpq %rax, %rdx
    jne 2f
3:  addq $8, %rdi
    decq %rcx
    jnz 1b
    jmp m64_done
2:  call m64_record
    jmp 3b

m64_op_fill_addr:
    movq 8(%r12), %rdi
    movq 16(%r12), %rcx
    movq 32(%r12), %r13
    testq %rcx, %rcx
    jz m64_done
1:  movq %rdi, %rax
    xorq %r13, %rax
    movq %rax, (%rdi)
    addq $8, %rdi
    decq %rcx
    jnz 1b
    jmp m64_done

m64_op_verify_addr:
    movq 8(%r12), %rdi
    movq 16(%r12), %rcx
    movq 32(%r12), %r13
    testq %rcx, %rcx
    jz m64_done
1:  movq %rdi, %rax
    xorq %r13, %rax
    movq (%rdi), %rdx
    cmpq %rax, %rdx
    jne 2f
3:  addq $8, %rdi
    decq %rcx
    jnz 1b
    jmp m64_done
2:  call m64_record
    jmp 3b

m64_op_fill_random:
    movq 8(%r12), %rdi
    movq 16(%r12), %rcx
    movq 24(%r12), %r14              # xorshift64 state: never zero
    movq 32(%r12), %r13
    testq %r14, %r14
    jnz 1f
    movq $1, %r14
1:  testq %rcx, %rcx
    jz m64_done
2:  movq %r14, %rax
    shlq $13, %rax
    xorq %rax, %r14
    movq %r14, %rax
    shrq $7, %rax
    xorq %rax, %r14
    movq %r14, %rax
    shlq $17, %rax
    xorq %rax, %r14
    movq %r14, %rax
    xorq %r13, %rax
    movq %rax, (%rdi)
    addq $8, %rdi
    decq %rcx
    jnz 2b
    jmp m64_done

m64_op_verify_random:
    movq 8(%r12), %rdi
    movq 16(%r12), %rcx
    movq 24(%r12), %r14
    movq 32(%r12), %r13
    testq %r14, %r14
    jnz 1f
    movq $1, %r14
1:  testq %rcx, %rcx
    jz m64_done
2:  movq %r14, %rax
    shlq $13, %rax
    xorq %rax, %r14
    movq %r14, %rax
    shrq $7, %rax
    xorq %rax, %r14
    movq %r14, %rax
    shlq $17, %rax
    xorq %rax, %r14
    movq %r14, %rax
    xorq %r13, %rax
    movq (%rdi), %rdx
    cmpq %rax, %rdx
    jne 3f
4:  addq $8, %rdi
    decq %rcx
    jnz 2b
    jmp m64_done
3:  call m64_record
    jmp 4b

m64_op_sweep:
    movq 8(%r12), %rdi
    movq 16(%r12), %rcx
    movq 24(%r12), %r13              # expected
    movq 32(%r12), %r14              # replacement
    testq %rcx, %rcx
    jz m64_done
    cmpq $0, 40(%r12)
    je 5f
1:  movq (%rdi), %rdx                # ascending
    cmpq %r13, %rdx
    jne 2f
3:  movq %r14, (%rdi)
    addq $8, %rdi
    decq %rcx
    jnz 1b
    jmp m64_done
2:  movq %r13, %rax
    call m64_record
    jmp 3b
5:  leaq -8(%rdi,%rcx,8), %rdi       # descending: start at the last qword
6:  movq (%rdi), %rdx
    cmpq %r13, %rdx
    jne 7f
8:  movq %r14, (%rdi)
    subq $8, %rdi
    decq %rcx
    jnz 6b
    jmp m64_done
7:  movq %r13, %rax
    call m64_record
    jmp 8b

m64_op_flip:
    movq 24(%r12), %rdi
    movq 32(%r12), %rax
    xorq %rax, (%rdi)
    jmp m64_done

# m64_record: a qword at %rdi held %rdx instead of %rax. Counts it, stores the
# first 16 in full and tallies which bits were wrong. Preserves %rax, %rcx,
# %rdx, %rdi and the r12-r14 the operations keep their state in.
m64_record:
    incq 48(%r12)
    movq 56(%r12), %r8
    cmpq $16, %r8
    jae 1f
    imulq $24, %r8, %r9
    leaq 64(%r12,%r9), %r9
    movq %rdi, (%r9)
    movq %rax, 8(%r9)
    movq %rdx, 16(%r9)
    incq 56(%r12)
1:  movq %rax, %r10
    xorq %rdx, %r10                  # the wrong bits
2:  testq %r10, %r10
    jz 3f
    bsfq %r10, %r11
    incq 448(%r12,%r11,8)
    leaq -1(%r10), %r9
    andq %r9, %r10                   # clear the lowest set bit
    jmp 2b
3:  ret

m64_done:
    movl $m64_back32, %eax
    pushq $0x10                      # CODE32 selector in our private GDT
    pushq %rax
    lretq                            # far return: back to compatibility mode

.code32
m64_back32:
    movl %cr0, %eax
    andl $0x7fffffff, %eax           # leave IA-32e before touching PAE
    movl %eax, %cr0
    movl m64_cr4, %eax
    movl %eax, %cr4                  # clears PAE
    movl m64_cr3, %eax
    movl %eax, %cr3
    movl $0xc0000080, %ecx
    movl m64_efer_lo, %eax
    movl m64_efer_hi, %edx
    wrmsr                            # clears LME
    lgdt m64_gdtr
    ljmp $0x08, $m64_restored
m64_restored:
    movl m64_cr0, %eax
    movl %eax, %cr0                  # the kernel's own paging again
    pushl m64_eflags
    popfl                            # the interrupt flag comes back last
    xorl %eax, %eax
    popl %ebx
    popl %esi
    popl %edi
    popl %ebp
    ret

.section .data.memtest64, "aw"
.balign 8
m64_gdtr:       .word 0
                .long 0
m64_eflags:     .long 0
m64_cr0:        .long 0
m64_cr3:        .long 0
m64_cr4:        .long 0
m64_efer_lo:    .long 0
m64_efer_hi:    .long 0
.balign 8
m64_gdt:
    .quad 0                          # null
    .quad 0x00209a0000000000         # 0x08: 64-bit code
    .quad 0x00cf9a000000ffff         # 0x10: 32-bit code (landing pad)
m64_gdt_end:
m64_gdt_descriptor:
    .word m64_gdt_end - m64_gdt - 1
    .long m64_gdt
