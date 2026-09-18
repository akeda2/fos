/*
 * tapes/longmode.S -- a tape that visits 64-bit long mode and comes back.
 *
 * fos is, and stays, a 32-bit protected-mode kernel (see AGENTS.md's
 * "Deferred optional x86-64 path" section: a real 64-bit boot path is a
 * much bigger, permanent architecture change -- a parallel boot target,
 * a 64-bit linker/entry, CPUID long-mode detection at boot, identity page
 * tables sized for the whole machine, and an explicitly versioned boot
 * ABI). This tape is not that. It is a small, fully self-contained, fully
 * reversible demonstration that a 64-bit CPU mode switch can happen at
 * all from a running 32-bit fos kernel, entirely inside one `tape run`
 * call: it builds its own tiny page tables and its own private GDT, jumps
 * into IA-32e (long) mode, proves it got there by writing a 64-bit-only
 * value straight to VGA text memory, then jumps back out, restores every
 * piece of CPU state it touched, and returns to the shell exactly like
 * any other tape. Nothing about the kernel's own GDT, page tables (there
 * are none -- fos never enables paging outside this tape), or IDT is
 * left touched or replaced once this tape returns.
 *
 * Why this needs its own page tables and GDT
 * -------------------------------------------
 * Long mode requires paging to be enabled (CR0.PG) with CR4.PAE set and
 * EFER.LME set before CR0.PG is turned on, and it requires at least one
 * 64-bit code descriptor to jump to (the L bit in the descriptor, not any
 * CR4 bit, is what actually makes the CPU execute 64-bit-encoded
 * instructions). fos's stage2 loader sets up flat, non-paged 32-bit
 * descriptors only (see boot/stage2.S's `gdt`), and the kernel never
 * builds a page table at all -- so this tape builds a minimal identity
 * map and a minimal private GDT of its own, uses them for the few
 * instructions it needs 64-bit mode for, and then throws them away by
 * restoring the kernel's original GDTR/CR0/CR3/CR4/EFER before
 * returning. The "page tables" here are just three static tables,
 * identity-mapping physical 0..6 MiB with 2 MiB pages -- enough to cover
 * the kernel's own code/stack (0x10000..0x40000), the VGA text buffer
 * (0xb8000), and this tape's own load image (0x100000..0x500000, i.e.
 * TAPE_BASE plus the 4 MiB TAPE_LEN from kernel/src/pmem.rs). Nothing
 * else needs to be mapped: this tape's entire 64-bit excursion touches no
 * other physical memory.
 *
 * Why the whole excursion runs with interrupts off
 * ---------------------------------------------------
 * kernel/src/interrupts.rs installs a real IDT and enables real hardware
 * interrupts: the PIT timer fires at 100 Hz (IRQ0) and the PS/2 keyboard
 * fires on keypress (IRQ1), both live the entire time fos's shell is
 * running, including while a tape runs. That IDT is a 32-bit-format IDT
 * (its gate descriptors are only valid for 32-bit protected mode); it is
 * not just unusable but actively *wrong* to service an interrupt against
 * while transitioning into or out of long mode, or while running 64-bit
 * code with a private GDT the kernel doesn't know about. So this tape
 * executes a single `cli` before it changes CR4/CR3/EFER/CR0/GDTR at all,
 * and does not `popfl` the original EFLAGS (re-enabling interrupts, among
 * anything else that flag word encodes) until every one of those has
 * been restored to its original value and the kernel's own GDT is back
 * in CS. If this tape were interrupted mid-excursion, the PIT/keyboard
 * handlers would run against a live IDT that no longer matches the CPU's
 * actual segmentation/paging state -- this is the actual reason for the
 * `cli`, not just defensive caution.
 *
 * Why direct VGA writes, not the `putc` callback, prove long mode
 * ------------------------------------------------------------------
 * Every other tape (see tapes/primes.S) calls back into the kernel's own
 * `console::print_byte` through the passed-in `putc` function pointer,
 * because that one function already knows how to place characters, wrap
 * lines, scroll, mirror to COM1, and relay to a `term net` session, all
 * in one place. This tape's setup and its final wrap-up message do
 * exactly that. But *while actually running in 64-bit mode*, the
 * `putc` callback cannot be called at all: it is 32-bit code, reached
 * with a plain `call`, and there is no cheap way to call 32-bit code
 * from 64-bit mode without a second, nested mode transition for the sole
 * benefit of one debug print. So the one thing this tape does while it
 * is truly in 64-bit mode is write a fixed, distinctive 64-bit
 * immediate value (0xdeadbeefcafebabe -- something no 32-bit `mov`
 * could ever load in one instruction) as hex digits directly into VGA
 * text memory at 0xb8000, one (character, attribute) cell per the same
 * convention tapes/vga-colors/src/main.rs uses. That direct write is
 * proof, on the real screen, that 64-bit-encoded instructions actually
 * ran -- not merely that some code claiming to be 64-bit executed.
 *
 * Why this needs a real `ld` link step, unlike tapes/primes.S
 * -----------------------------------------------------------
 * tapes/primes.S is pure position-independent 32-bit code, assembled
 * with `as --32` and turned straight into a flat binary with
 * `objcopy -O binary`, no linker involved at all -- that works because
 * x86-32 ELF relocations are REL-style: GAS bakes the resolved address
 * directly into the instruction bytes at assemble time, so a linker-free
 * objcopy still produces correct code. x86-64 ELF relocations are
 * RELA-style instead: the resolved address lives only in a separate
 * `.rela` section that plain `objcopy -O binary` throws away, leaving
 * zeroed placeholders at every address reference. Since this file needs
 * real 64-bit-context relocations (its `.code64` region references
 * 32-bit-mode labels back in `.code32` code, and vice versa), it must be
 * assembled with `as --64` (producing a 64-bit ELF, even though most of
 * its instructions are still 32-bit-encoded `.code32` code) and then
 * actually linked with `ld -Ttext=0x100000 --oformat=elf64-x86-64`
 * before `objcopy -O binary` -- see the Makefile's `tape-longmode`
 * target. One side benefit of the real link step: unlike primes.S, this
 * file never needs the `call`/`pop` PIC-base trick to find its own
 * runtime address -- `ld -Ttext=0x100000` already resolves every label
 * to its final linked address (which matches TAPE_BASE, since the tape
 * loader places this image there unmodified), so ordinary label
 * arithmetic (`.quad pdpt`, `leal msg_intro, %esi`, and so on) just
 * works.
 *
 * Register-preservation contract
 * -------------------------------
 * Same cdecl contract as every other tape (see tapes/primes.S's header):
 * %ebx, %esi, %edi, %ebp, and %esp are preserved across the call, and the
 * one argument (the kernel's `putc` callback, `extern "C" fn(u32)`) is
 * read once at entry. Everything this tape additionally touches --
 * CR0, CR3, CR4, EFER, GDTR, and EFLAGS -- is saved to static scratch
 * memory before being changed and restored, verbatim, before returning:
 * restoring the saved CR0 clears PG (and every other bit) in one move,
 * restoring saved CR4 clears PAE, restoring saved EFER (via `wrmsr`)
 * clears LME, `lgdt` with the saved descriptor restores the kernel's own
 * GDT, and `popfl` with the saved EFLAGS restores the original interrupt
 * flag (and everything else) in one move. If this tape is ever
 * interrupted (a bug, a fault) before that restoration completes, the
 * kernel is left running against invalid CPU state -- there is no
 * recovery path for that, by design: this is a narrow, one-shot
 * educational demonstration, not a production mode-switching library.
 *
 * If long-mode support is missing (checked via CPUID leaf 0x80000001,
 * EDX bit 29), this tape prints a short message and returns immediately
 * without touching CR0/CR3/CR4/EFER/GDTR/EFLAGS at all -- QEMU's TCG
 * backend under `qemu-system-i386` cannot emulate this CPUID bit at all
 * (confirmed: it warns and reports the feature absent), so running this
 * tape's actual 64-bit excursion for real requires `qemu-system-x86_64`
 * instead -- see the e2e test and AGENTS.md for why that is the one
 * documented exception to this project's usual QEMU binary choice.
 *
 * Build: `make tape-longmode` assembles this with `as --64`, links it
 * with a fixed `-Ttext=0x100000` (matching TAPE_BASE), and extracts a
 * flat binary with `objcopy -O binary`, producing
 * build/tapes/longmode.bin.
 */

.code32
.text
.global tape_main

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
    # Load a value no 32-bit `mov` could ever put in one register in one
    # instruction, and hex-dump it straight to VGA text memory as proof
    # that this is genuinely running as 64-bit-encoded code (see the
    # file header comment on why this can't go through `putc` instead).
    movabs $0xb8000, %rdi             # VGA text memory, row 0 column 0
    movabs $msg_vga_prefix, %rsi
.Lvga_prefix_loop:
    movb (%rsi), %al
    testb %al, %al
    jz .Lvga_prefix_done
    movb %al, (%rdi)
    movb $0x0f, 1(%rdi)              # white on black
    addq $2, %rdi
    incq %rsi
    jmp .Lvga_prefix_loop
.Lvga_prefix_done:

    movabs $0xdeadbeefcafebabe, %rax
    movl $16, %ecx                   # 16 hex digits, most significant first
.Lvga_hex_loop:
    rolq $4, %rax
    movl %eax, %edx
    andl $0xf, %edx
    movb hex_digits(%rdx), %dl
    movb %dl, (%rdi)
    movb $0x0a, 1(%rdi)              # green on black, to stand out
    addq $2, %rdi
    decl %ecx
    jnz .Lvga_hex_loop

    # ---- back to compatibility mode: a far return is the standard,
    # documented way to leave 64-bit mode for a fixed target (Intel SDM
    # Vol 3 9.8.5), since a plain `ljmp` with an immediate absolute
    # target is not encodable in 64-bit mode ----
    pushq $CODE32_SEL
    pushq $back32
    lretq

.code32
back32:
    # ---- undo everything above, in reverse, before touching interrupts
    # again ----
    movl saved_cr0, %eax
    movl %eax, %cr0                  # clears PG (and restores every other bit)
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

hex_digits:
    .ascii "0123456789abcdef"

msg_banner:
    .asciz "longmode: a self-contained, fully reversible 64-bit excursion\n"
msg_supported: .asciz "longmode: CPUID reports long-mode support; making the jump\n"
msg_unsupported:
    .asciz "longmode: CPUID reports no long-mode support on this CPU; nothing to do\n"
msg_vga_prefix:
    .asciz "64-BIT MODE REACHED, RAX="
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
