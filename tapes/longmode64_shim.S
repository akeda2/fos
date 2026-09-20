/*
 * tapes/longmode64_shim.S -- reusable 64-bit-excursion plumbing, shared
 * by every "visits long mode and comes back" tape.
 *
 * This file implements the entire `tape_main` entry point (the one thing
 * `shell::tape_run` actually calls) and every piece of privileged,
 * CPU-mode-switching machinery a tape like this needs: the CPUID
 * long-mode check, a tiny identity-mapped page table set, a private GDT,
 * `cli`-protected state save/restore, and the actual mode transition in
 * both directions. None of that benefits from being written twice, or
 * from being written in Rust -- it is inherently privileged-instruction
 * work (CR0/CR3/CR4/EFER/GDTR manipulation, far jumps between code
 * segments) that Rust has no special vocabulary for anyway. See
 * AGENTS.md's "A tape that visits 64-bit long mode and comes back"
 * section for the full design rationale (why the page tables/GDT are
 * needed at all, why interrupts stay off for the whole excursion, why
 * VGA is written directly instead of through the `putc` callback, why a
 * real `ld` link step is required, and the CPUID/QEMU testing caveats)
 * -- none of that is repeated here.
 *
 * The reusable contract: this file expects exactly one external symbol,
 * `tape64_body`, an ordinary SysV64-ABI function (`extern "C" fn()` in
 * Rust terms) taking no arguments and returning nothing, to be linked in
 * from elsewhere. Between entering 64-bit mode and leaving it again, this
 * file does nothing but `call tape64_body` -- so whatever that body does
 * runs with paging on, CR4.PAE/EFER.LME/CS.L all set, this file's own
 * private GDT loaded, and interrupts masked, exactly like the inline demo
 * `tapes/longmode.S` (asm-only, this shim's original all-in-one form)
 * used to run directly. `tapes/longmode.S` now provides `tape64_body` in
 * hand-written assembly; `tapes/longmode-rs/` provides an equivalent
 * `tape64_body` compiled from ordinary `#![no_std]` Rust instead -- see
 * that crate's `src/lib.rs` for how little is actually different between
 * the two. A future tape wanting to run something else in 64-bit mode
 * (a bigger computation, more VGA output, anything not needing disk/BIOS
 * access -- see AGENTS.md's note on disk I/O's real-mode dependency)
 * only needs to provide its own `tape64_body`, in whichever language, and
 * link against this same shim.
 *
 * `tape64_body` runs with the stack pointer 16-byte aligned (SysV64's
 * calling-convention requirement, unlike cdecl, which this shim's own
 * 32-bit code otherwise follows) -- `long_entry` below saves the
 * pre-alignment %rsp in %r15 (caller-saved in the sense that this shim
 * doesn't need it for anything else, and SysV64 guarantees a
 * well-behaved callee preserves it anyway) and restores the exact
 * original value immediately after `tape64_body` returns, before the
 * `push`+`push`+`lretq` sequence back to compatibility mode -- getting
 * this wrong would leave %esp permanently offset by up to 15 bytes once
 * back in 32-bit code, corrupting this file's own register-restore
 * epilogue and the final `ret` back to the kernel.
 *
 * Build: never built directly (`.PHONY` in the Makefile has no
 * `tape-longmode64-shim` target) -- always assembled with `as --64` and
 * linked together with whatever object/archive provides `tape64_body`,
 * via `ld -Ttext=0x100000 --oformat=elf64-x86-64 -e tape_main`. See
 * `make tape-longmode`/`make tape-longmode-rs` in the Makefile.
 */

.code32
.text
.global tape_main
.extern tape64_body

.equ CODE64_SEL, 0x08
.equ CODE32_SEL, 0x10

tape_main:
    # cdecl argument #1 (the kernel's print-byte callback) sits at
    # 4(%esp) right now -- nothing has touched %esp yet.
    movl 4(%esp), %eax

    # Preserve the caller's callee-saved registers (cdecl).
    pushl %ebp
    pushl %edi
    pushl %esi
    pushl %ebx
    movl %eax, putc_fn

    leal msg_banner, %esi
    call print_string

    # ---- CPUID long-mode support check ----
    # Leaf 0x80000000's return value in %eax tells us the highest
    # supported extended leaf; we need leaf 0x80000001 (which carries
    # the long-mode feature bit) to exist at all before querying it.
    movl $0x80000000, %eax
    cpuid
    cmpl $0x80000001, %eax
    jb .Lno_long_mode

    movl $0x80000001, %eax
    cpuid
    testl $(1 << 29), %edx          # EDX bit 29 = long mode (LM)
    jz .Lno_long_mode

    leal msg_supported, %esi
    call print_string

    # ---- save every piece of CPU state we are about to change ----
    # From here until the matching restore below, interrupts stay off:
    # the live PIT/keyboard IDT is a 32-bit-format IDT, invalid to
    # service against while CR0/CR3/CR4/EFER/GDTR are mid-transition or
    # while a private GDT is loaded (see the file header comment).
    cli
    pushfl
    popl saved_eflags
    sgdt saved_gdtr
    movl %cr0, %eax
    movl %eax, saved_cr0
    movl %cr3, %eax
    movl %eax, saved_cr3
    movl %cr4, %eax
    movl %eax, saved_cr4
    movl $0xc0000080, %ecx           # IA32_EFER
    rdmsr
    movl %eax, saved_efer_lo
    movl %edx, saved_efer_hi

    # The kernel's paging foundation uses non-PAE 4 MiB pages. PAE must not
    # change while those page tables are active, so first leave paging; the
    # kernel, stack, and this fixed-address tape remain directly reachable.
    movl %cr0, %eax
    andl $0x7fffffff, %eax
    movl %eax, %cr0

    # ---- CR4.PAE, then point CR3 at our own identity-map page tables ----
    movl %cr4, %eax
    orl $0x20, %eax
    movl %eax, %cr4

    movl $pml4, %eax
    movl %eax, %cr3

    # ---- EFER.LME (must be set before CR0.PG per the SDM) ----
    movl $0xc0000080, %ecx
    rdmsr
    orl $0x100, %eax
    wrmsr

    # ---- CR0.PG: this enters IA-32e *compatibility* submode, still
    # executing 32-bit-encoded code until the far jump below reloads CS
    # with our 64-bit code descriptor ----
    movl %cr0, %eax
    orl $0x80000000, %eax
    movl %eax, %cr0

    # ---- swap in our own private GDT and jump into 64-bit code ----
    lgdt long_gdt_descriptor
    ljmp $CODE64_SEL, $long_entry

.code64
long_entry:
    # SysV64 requires %rsp 16-byte aligned at the point of `call` --
    # save the exact pre-alignment value in %r15 (see the file header
    # comment on why) and restore it right after `tape64_body` returns.
    movq %rsp, %r15
    andq $-16, %rsp
    call tape64_body
    movq %r15, %rsp

    # ---- back to compatibility mode: a far return is the standard,
    # documented way to leave 64-bit mode for a fixed target (Intel SDM
    # Vol 3 9.8.5), since a plain `ljmp` with an immediate absolute
    # target is not encodable in 64-bit mode ----
    pushq $CODE32_SEL
    pushq $back32
    lretq

.code32
back32:
    # ---- leave IA-32e before restoring the kernel's non-PAE paging
    # state. CR4.PAE must change only while paging is disabled. ----
    movl %cr0, %eax
    andl $0x7fffffff, %eax
    movl %eax, %cr0

    movl saved_cr4, %eax
    movl %eax, %cr4                  # clears PAE
    movl saved_cr3, %eax
    movl %eax, %cr3

    movl $0xc0000080, %ecx
    movl saved_efer_lo, %eax
    movl saved_efer_hi, %edx
    wrmsr                            # clears LME

    lgdt saved_gdtr
    ljmp $0x08, $after_gdt_restore

after_gdt_restore:
    # Re-enable the caller's original (non-PAE) paging only after its CR3,
    # CR4, GDT, and code segment have all been restored.
    movl saved_cr0, %eax
    movl %eax, %cr0
    pushl saved_eflags
    popfl                            # restores the original interrupt flag last

    leal msg_done, %esi
    call print_string
    jmp .Lreturn

.Lno_long_mode:
    leal msg_unsupported, %esi
    call print_string

.Lreturn:
    popl %ebx
    popl %esi
    popl %edi
    popl %ebp
    ret

#
# print_string: writes the NUL-terminated string at %esi via print_char,
# one byte at a time. Clobbers nothing visible to the caller.
#
print_string:
    pushl %eax
    pushl %esi
print_string_loop:
    movb (%esi), %al
    testb %al, %al
    jz print_string_done
    call print_char
    incl %esi
    jmp print_string_loop
print_string_done:
    popl %esi
    popl %eax
    ret

#
# print_char: prints the byte in %al via the kernel's `putc` callback,
# zero-extended to 32 bits to match its `extern "C" fn(u32)` signature.
#
print_char:
    pushl %eax
    pushl %ecx
    pushl %edx
    movzbl %al, %eax
    pushl %eax
    call *putc_fn
    addl $4, %esp
    popl %edx
    popl %ecx
    popl %eax
    ret

    .align 4
putc_fn:            .long 0
saved_eflags:        .long 0
saved_cr0:           .long 0
saved_cr3:           .long 0
saved_cr4:           .long 0
saved_efer_lo:       .long 0
saved_efer_hi:       .long 0

    .align 8
saved_gdtr:
    .word 0
    .long 0

msg_banner:
    .asciz "longmode: a self-contained, fully reversible 64-bit excursion\n"
msg_supported: .asciz "longmode: CPUID reports long-mode support; making the jump\n"
msg_unsupported:
    .asciz "longmode: CPUID reports no long-mode support on this CPU; nothing to do\n"
msg_done:
    .asciz "longmode: back in ordinary 32-bit protected mode, all state restored\n"

# ---- identity-mapped page tables: physical 0..6 MiB via three 2 MiB
# pages, covering the kernel (0x10000..0x40000), VGA text memory
# (0xb8000), and this tape's own 4 MiB image (TAPE_BASE..TAPE_BASE+
# TAPE_LEN, i.e. 0x100000..0x500000). Nothing else is ever touched while
# in 64-bit mode, so nothing else needs to be mapped. Present + writable
# is 0x3; present + writable + page-size(2 MiB) is 0x83. ----
    .align 4096
pml4:
    .quad pdpt + 0x3
    .fill 4096 - 8, 1, 0

    .align 4096
pdpt:
    .quad pd + 0x3
    .fill 4096 - 8, 1, 0

    .align 4096
pd:
    .quad 0x0000000000000083         # 0x000000..0x1fffff
    .quad 0x0000000000200083         # 0x200000..0x3fffff
    .quad 0x0000000000400083         # 0x400000..0x5fffff
    .fill 4096 - 24, 1, 0

# ---- this tape's own private GDT, used only for the excursion above and
# never referenced again once the kernel's original GDTR is restored.
# Selector 0x08 is a 64-bit code descriptor (L bit set, D bit clear);
# selector 0x10 is an ordinary flat 32-bit code descriptor, used only as
# the landing pad immediately after leaving 64-bit mode, matched to the
# layout boot/stage2.S's own GDT uses for its selector 0x08. ----
    .align 8
long_gdt:
    .quad 0                           # null descriptor
    .quad 0x00209a0000000000          # 0x08: 64-bit code, flat
    .quad 0x00cf9a000000ffff          # 0x10: 32-bit code, flat
long_gdt_end:
long_gdt_descriptor:
    .word long_gdt_end - long_gdt - 1
    .long long_gdt
