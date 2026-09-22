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
 * `tape64_body`, an ordinary SysV64-ABI function linked from elsewhere.
 * This shim calls it as:
 *
 *   extern "C" fn(args_ptr: u64, args_len: u64, report_ptr: u64,
 *                 arena_ptr: u64, arena_len: u64, cycles_per_second: u64)
 *
 * where:
 * - `args_ptr`/`args_len` mirror `shell::tape_run`'s ordinary startup-
 *   argument slice (same bytes as `tape run ...` passed after `run`).
 * - `report_ptr` points to this shim's writable report buffer
 *   (`report_magic`, `report_version`, `report_mode`, `report_support`,
 *   `report_tests`, then a 128-byte NUL-terminated `report_summary`
 *   text buffer) for optional structured/human-readable status handoff
 *   back to the 32-bit side after the round trip. A body that leaves
 *   `report_summary`'s first byte zero (the default -- this shim clears
 *   the whole report before every run) gets no extra line printed; one
 *   that fills in a friendly NUL-terminated message gets it echoed
 *   through `putc` right after the usual hex-coded report line.
 * - `arena_ptr`/`arena_len` are an optional large temporary buffer
 *   allocated through tape ABI `alloc` before entering long mode and
 *   released via `free` after returning (0/0 when unavailable).
 * - `cycles_per_second` is this shim's own short TSC-vs-PIT calibration
 *   (see `.Lcalibrate_wait` below), so a body that wants to run for a
 *   real minimum wall-clock duration (rather than a fixed instruction or
 *   frame count) can do so using only `rdtsc` -- it cannot safely call
 *   back into 32-bit code (`ticks`/`should_abort`/...) once the mode
 *   switch below has happened, since those cdecl functions assume a
 *   32-bit stack/operand-size world this shim has already left behind.
 *   Zero means calibration could not run (should not happen in
 *   practice, but a body should treat it as "unknown" rather than
 *   trusting a suspiciously small nonzero value blindly).
 *
 * Under SysV64, a body declaring fewer arguments remains valid (unused
 * trailing register arguments are ignored), so existing zero-argument
 * longmode demos still link/run unchanged. Between entering 64-bit mode
 * and leaving it again, this file does nothing but call that body -- so
 * whatever it does runs with paging on, CR4.PAE/EFER.LME/CS.L all set,
 * this file's own private GDT loaded, and interrupts masked, exactly like
 * the inline demo `tapes/longmode.S` (asm-only, this shim's original
 * all-in-one form) used to run directly. `tapes/longmode.S` now provides
 * `tape64_body` in hand-written assembly; `tapes/longmode-rs/` provides
 * an equivalent body compiled from ordinary `#![no_std]` Rust instead.
 * A future tape wanting to run something else in 64-bit mode (a bigger
 * computation, more VGA output, anything not needing disk/BIOS access --
 * see AGENTS.md's note on disk I/O's real-mode dependency) only needs to
 * provide its own `tape64_body`, in whichever language, and link against
 * this same shim.
 *
 * CR4.OSFXSR/OSXMMEXCPT (bits 9/10) are also temporarily set for the
 * duration of the excursion, alongside CR4.PAE, and restored to their
 * original (always-clear) value on the way back like every other saved
 * register. The 32-bit kernel deliberately never sets these -- it never
 * uses floating point/SSE at all -- which meant `cpu64-lab`'s and
 * `cpu64-show`'s own CR4-gated "is SSE actually usable" checks always
 * read false and their SSE/SSE2 code paths never actually ran on any real
 * boot, even on hardware whose CPUID reports full SSE2 support. Since
 * this excursion is fully self-contained and every other piece of CR4 is
 * restored exactly as saved, enabling these two bits only while inside
 * this shim's own isolated 64-bit world is safe: the kernel's own
 * "never enables float" policy is completely undisturbed the instant
 * control returns to 32-bit code.
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
 * via `ld -T tapes/longmode64.ld --oformat=elf64-x86-64` (that linker
 * script fixes the load address at 0x100000 and sets `tape_main` as the
 * entry point). See `make tape-longmode`/`make tape-longmode-rs` in the
 * Makefile.
 */

.code32
.section .longmode_shim, "ax"
.global tape_main
.extern tape64_body

.equ CODE64_SEL, 0x08
.equ CODE32_SEL, 0x10

tape_main:
    # cdecl argument #1 (the kernel's print-byte callback) sits at
    # 4(%esp) right now -- nothing has touched %esp yet.
    movl 4(%esp), %eax
    movl 24(%esp), %ecx            # tape_run's args_ptr
    movl %ecx, saved_args_ptr
    movl 28(%esp), %ecx            # tape_run's args_len
    movl %ecx, saved_args_len
    movl 12(%esp), %ecx            # tape_run's alloc callback
    movl %ecx, alloc_fn
    movl 16(%esp), %ecx            # tape_run's free callback
    movl %ecx, free_fn
    movl 48(%esp), %ecx            # tape_run's ticks callback (12th cdecl arg)
    movl %ecx, ticks_fn

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

    # Ask the kernel for a large temporary arena. Some demos only need a
    # few KiB; others deliberately stress wide vector loops over many MiB.
    # A zero pointer is a normal, explicit "not available" case.
    movl $ARENA_BYTES, %eax
    pushl %eax
    call *alloc_fn
    addl $4, %esp
    movl %eax, saved_arena_ptr
    movl $0, saved_arena_len
    testl %eax, %eax
    jz .Lno_arena
    movl $ARENA_BYTES, saved_arena_len
.Lno_arena:

    # A 64-bit body may optionally fill this report buffer. Keep it zeroed
    # before every run so old bodies (that do not know about it) cannot
    # accidentally leave stale status from a previous invocation.
    movl $0, report_magic
    movl $0, report_version
    movl $0, report_mode
    movl $0, report_support
    movl $0, report_tests
    leal report_summary, %edi
    movl $REPORT_SUMMARY_CAPACITY, %ecx
    xorl %eax, %eax
    rep stosb

    # ---- calibrate a TSC-based "cycles per second" figure ----
    # A body cannot safely call back into 32-bit code (`ticks`,
    # `should_abort`, ...) once interrupts are masked and the mode switch
    # below has happened, so a body wanting a real minimum wall-clock
    # runtime needs its own time source purely from `rdtsc`. This has to
    # happen here, before `cli`, while the PIT-driven `ticks_fn` can still
    # actually increment.
    call *ticks_fn
    movl %eax, calib_tick0_lo
    movl %edx, calib_tick0_hi
    rdtsc
    movl %eax, calib_tsc0_lo
    movl %edx, calib_tsc0_hi
.Lcalibrate_wait:
    call *ticks_fn
    subl calib_tick0_lo, %eax
    sbbl calib_tick0_hi, %edx
    cmpl $0, %edx
    jg .Lcalibrate_done          # high word positive: far more than enough ticks passed
    jl .Lcalibrate_wait          # high word negative: not enough yet (should not happen)
    cmpl $CALIBRATE_TICKS, %eax
    jb .Lcalibrate_wait
.Lcalibrate_done:
    rdtsc
    subl calib_tsc0_lo, %eax
    sbbl calib_tsc0_hi, %edx
    # cycles_per_second = delta_tsc * (100 / CALIBRATE_TICKS); with
    # CALIBRATE_TICKS = 25 that multiplier is exactly 4 (a left shift by
    # 2 across the 64-bit edx:eax pair), since the PIT runs at 100 Hz.
    shldl $2, %eax, %edx
    shll $2, %eax
    movl %eax, cycles_per_second_lo
    movl %edx, cycles_per_second_hi

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

    # ---- CR4.PAE, then point CR3 at our own identity-map page tables.
    # Also set OSFXSR/OSXMMEXCPT (bits 9/10) for the duration of this
    # excursion only -- see the file header comment on why this is safe:
    # `saved_cr4` above was captured before this OR, so the kernel's own
    # (always-clear) CR4 is restored exactly on the way back. ----
    movl %cr4, %eax
    orl $0x620, %eax
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
    movl saved_args_ptr, %edi
    movl saved_args_len, %esi
    leaq report_magic(%rip), %rdx
    movl saved_arena_ptr, %ecx
    movl saved_arena_len, %r8d
    # Build the 6th argument (cycles_per_second, a single 64-bit value)
    # from the two 32-bit halves the calibration above computed.
    movl cycles_per_second_lo, %r9d
    movl cycles_per_second_hi, %eax
    shlq $32, %rax
    orq %rax, %r9
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
    cmpl $0x43503634, report_magic
    jne .Lfree_arena
    leal msg_report_prefix, %esi
    call print_string
    movl report_mode, %eax
    call print_hex32
    leal msg_report_support, %esi
    call print_string
    movl report_support, %eax
    call print_hex32
    leal msg_report_tests, %esi
    call print_string
    movl report_tests, %eax
    call print_hex32
    leal msg_report_suffix, %esi
    call print_string
    cmpb $0, report_summary
    je .Lfree_arena
    leal report_summary, %esi
    call print_string
.Lfree_arena:
    movl saved_arena_ptr, %eax
    testl %eax, %eax
    jz .Lreturn
    pushl saved_arena_len
    pushl %eax
    call *free_fn
    addl $8, %esp
    movl $0, saved_arena_ptr
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

#
# print_hex32: prints %eax as eight lowercase hexadecimal digits.
#
print_hex32:
    pushl %eax
    pushl %ebx
    pushl %ecx
    pushl %edx
    movl %eax, %edx
    movl $8, %ecx
print_hex32_loop:
    movl %edx, %ebx
    shrl $28, %ebx
    movb hex_digits(%ebx), %al
    call print_char
    shll $4, %edx
    decl %ecx
    jnz print_hex32_loop
    popl %edx
    popl %ecx
    popl %ebx
    popl %eax
    ret

    .align 4
putc_fn:            .long 0
alloc_fn:           .long 0
free_fn:            .long 0
ticks_fn:            .long 0
saved_args_ptr:      .long 0
saved_args_len:      .long 0
saved_arena_ptr:     .long 0
saved_arena_len:     .long 0
saved_eflags:        .long 0
saved_cr0:           .long 0
saved_cr3:           .long 0
saved_cr4:           .long 0
saved_efer_lo:       .long 0
saved_efer_hi:       .long 0
calib_tick0_lo:      .long 0
calib_tick0_hi:      .long 0
calib_tsc0_lo:       .long 0
calib_tsc0_hi:       .long 0
cycles_per_second_lo: .long 0
cycles_per_second_hi: .long 0
report_magic:        .long 0
report_version:      .long 0
report_mode:         .long 0
report_support:      .long 0
report_tests:        .long 0
.equ REPORT_SUMMARY_CAPACITY, 128
report_summary:
    .fill REPORT_SUMMARY_CAPACITY, 1, 0

.equ CALIBRATE_TICKS, 25             # 250 ms at the PIT's 100 Hz rate

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
msg_report_prefix:
    .asciz "longmode: 64-bit body report mode=0x"
msg_report_support:
    .asciz ", support=0x"
msg_report_tests:
    .asciz ", tests=0x"
msg_report_suffix:
    .asciz "\n"
hex_digits:
    .asciz "0123456789abcdef"

.equ ARENA_BYTES, 8388608

# ---- identity-mapped page tables: first 1 GiB via 2 MiB pages. This
# still covers the original low-memory requirements (kernel low region,
# VGA text memory, and the tape image itself) and additionally covers the
# big-pool arena passed to newer 64-bit demos (`cpu64-show`) which can
# live well above 6 MiB. Present + writable is 0x3; present + writable +
# page-size(2 MiB) is 0x83. ----
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
    .set pd_phys, 0
    .rept 512
        .quad pd_phys + 0x83
        .set pd_phys, pd_phys + 0x200000
    .endr

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
