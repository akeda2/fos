/*
 * primes.S -- a "paper tape" test program for fos.
 *
 * This is a self-contained flat 32-bit binary meant to be loaded with the
 * shell's `tape` command (see kernel/src/shell.rs and README.md's "Tape"
 * section) and executed with `tape run`. It computes every prime up to
 * 1,000,000 with a classic Sieve of Eratosthenes, then prints a short
 * summary (count found, largest prime, elapsed CPU cycles) directly to VGA
 * text memory, and returns to the fos shell exactly as it found it.
 *
 * Why this file looks the way it does
 * ------------------------------------
 * A tape has no fixed load address. `tape load net`/`tape load disk`
 * allocate a physical buffer wherever pmem::allocate_contiguous() happens to
 * find room (see kernel/src/pmem.rs), which shifts depending on whatever
 * else the kernel has already allocated (RTL8139 ring buffers, etc). Unlike
 * the tiny single-instruction test tape used while developing the `tape`
 * feature itself, this program has real data (the sieve array, message
 * strings) that must be reachable no matter where it ends up in memory. So
 * this is genuine hand-written position-independent code (PIC): the very
 * first thing it does is the classic `call`/`pop` idiom to capture its own
 * runtime load address in %ebx, and every later reference to a label in
 * this file is computed as "%ebx + (label's assemble-time offset from the
 * start of this file)" rather than as a fixed absolute address. GNU `as`
 * resolves `label - get_base` to a plain constant at assembly time (both
 * symbols live in the same, single, unlinked section), exactly like
 * boot/stage2.S already does for its own `label + STAGE2_BASE` style
 * addressing -- no linker is involved for this file either, it goes
 * straight from `as` to `objcopy -O binary`, just like the boot stages.
 *
 * fos runs this in ring 0 with no memory protection and no fault recovery
 * (see AGENTS.md's "tape" notes) -- exactly like any other kernel code. It
 * is trusted to behave, and any mistake here (a bad address, a runaway
 * loop) is a real CPU fault or a real hang, not a sandboxed one.
 *
 * Why the file is about a megabyte
 * ---------------------------------
 * The sieve needs one byte per candidate number from 0 to 999,999. Since a
 * tape's buffer is sized to *exactly* the declared length that was sent
 * (nothing more is guaranteed to be free/owned by the tape -- see
 * AGENTS.md), the sieve array is embedded directly in this file as a literal
 * 1,000,000-byte zero-filled block (`.fill`), so the space is unquestionably
 * part of the tape's own buffer. This makes the assembled binary roughly a
 * megabyte, almost all of it zeros. `tape send`/`tools/send_tape.py` have no
 * size cap, so this is simply sent as ~1000 Ethernet frames; there is no
 * disk path here because FOSFS's 512-byte file cap can't hold it (see
 * README.md's "Tape" section for the disk-vs-network tradeoff).
 *
 * Registers and calling convention
 * ----------------------------------
 * The shell calls this tape as a plain `extern "C" fn()` (see
 * kernel/src/shell.rs's `tape_run`), i.e. the cdecl convention: %eax, %ecx,
 * %edx are ours to clobber freely, but %ebx, %esi, %edi, %ebp (and %esp)
 * must come back exactly as they went in, or the shell code that resumes
 * after `tape run` could behave unpredictably. This file saves those four
 * registers on entry and restores them immediately before the final `ret`,
 * even though %ebx is then immediately repurposed as this file's own load
 * base for the rest of its run.
 *
 * VGA and serial output
 * ----------------------
 * There is no ABI to call back into the kernel's own console code, so this
 * writes VGA text memory (0xb8000, one word per cell: attribute byte 0x07
 * in the high byte, ASCII in the low byte) and drives the CRTC cursor
 * ports (0x3d4 index / 0x3d5 data, registers 0x0e/0x0f) directly, matching
 * kernel/src/console.rs's own conventions exactly (see that file's
 * comments on why writing the framebuffer alone does not move the real,
 * blinking hardware cursor). It starts from whatever cursor position the
 * shell already left behind, so its first line continues naturally after
 * the shell's own last output. If printing would run past the last VGA
 * row, it scrolls the screen up by one row first, the same as
 * kernel/src/console.rs's own newline() does -- this was not implemented
 * in an earlier version of this file (reasoned, incorrectly, that four
 * short lines could never need it), which on real hardware silently wrote
 * this tape's entire summary off-screen whenever the cursor was already
 * further down the screen than a fresh QEMU boot typically leaves it.
 * Because the tape moves the hardware cursor but has no
 * way to update the kernel's own cached cursor position, kernel/src/shell.rs
 * re-syncs it via console::resync_cursor() immediately after this tape
 * returns, so the shell's next line of output continues from the right
 * place instead of overwriting this summary.
 *
 * Every character is also mirrored to the COM1 UART (0x3f8, same port the
 * kernel's own console uses), matching kernel/src/console.rs's own
 * dual-output habit. Besides being consistent with the rest of fos, this
 * is what makes the tape's result checkable by the same serial-driven
 * automated smoke test the rest of the project uses (see
 * tools/qemu_smoke.py) instead of needing a screen-capture/OCR pipeline
 * just for this one test tape.
 *
 * Timing
 * -------
 * There is likewise no stable way to call into the kernel's timer code, so
 * elapsed time is measured with the RDTSC instruction (always available in
 * ring 0) around the sieve, reported as raw CPU cycles rather than
 * calibrated seconds -- it is a relative, uncalibrated measurement, not a
 * wall-clock time, and is printed as such. The 64-bit TSC delta's high
 * 32 bits are assumed to be zero: at any realistic CPU clock speed, sieving
 * a million numbers finishes in far under the ~4 billion cycles the low
 * 32 bits alone can hold, so only the low half is printed.
 *
 * Build: `make tape-primes` assembles this with the same `as --32` +
 * `objcopy -O binary` recipe used for boot/stage1.S and boot/stage2.S,
 * producing build/tapes/primes.bin.
 */

.code32
.text
.global _start

.equ N,                   1000000
.equ VGA_BASE,             0xb8000
.equ VGA_ATTR,             0x07
.equ VGA_WIDTH,            80
.equ VGA_HEIGHT,           25
.equ CRTC_INDEX_PORT,      0x3d4
.equ CRTC_DATA_PORT,       0x3d5
.equ CRTC_CURSOR_HIGH,     0x0e
.equ CRTC_CURSOR_LOW,      0x0f
.equ COM1_PORT,            0x3f8
.equ COM1_LINE_STATUS,     0x3fd
.equ COM1_TX_EMPTY,        0x20
.equ COM1_WAIT_ATTEMPTS,   100000

_start:
    # Preserve the caller's callee-saved registers (cdecl): we are about to
    # repurpose %ebx as our own position-independent load base, and %esi /
    # %edi as loop and pointer scratch, for the whole run.
    pushl %ebp
    pushl %edi
    pushl %esi
    pushl %ebx
    cld

    call get_base
get_base:
    pop %ebx                       # %ebx = our own runtime load address

    # Record the starting TSC so we can report elapsed cycles later.
    rdtsc
    movl %eax, (tsc_start_lo - get_base)(%ebx)
    movl %edx, (tsc_start_hi - get_base)(%ebx)

    # Read the current hardware cursor position so our first printed
    # character continues right after whatever the shell already printed,
    # instead of starting at row/col 0.
    movb $CRTC_CURSOR_HIGH, %al
    movw $CRTC_INDEX_PORT, %dx
    outb %al, %dx
    movw $CRTC_DATA_PORT, %dx
    inb %dx, %al
    movb %al, %ch
    movb $CRTC_CURSOR_LOW, %al
    movw $CRTC_INDEX_PORT, %dx
    outb %al, %dx
    movw $CRTC_DATA_PORT, %dx
    inb %dx, %al
    movb %al, %cl
    movzwl %cx, %ecx
    movl %ecx, (cursor_pos - get_base)(%ebx)

    # Zero the sieve array explicitly. The tape's own bytes already arrive
    # as zero (it is a literal .fill in this file), but pmem-allocated
    # frames are not guaranteed to be pre-zeroed in general (see
    # AGENTS.md), so this does not rely on that.
    leal (sieve - get_base)(%ebx), %edi
    movl $N, %ecx
    xorl %eax, %eax
    rep stosb

    # sieve[i] == 0 means "still considered prime"; sieve[i] == 1 means
    # "known composite". 0 and 1 are neither prime by definition.
    leal (sieve - get_base)(%ebx), %edi
    movb $1, 0(%edi)
    movb $1, 1(%edi)

    # Classic Sieve of Eratosthenes: for every i from 2 up to sqrt(N),
    # if i is still a candidate, mark all of its multiples (starting at
    # i*i, since smaller multiples were already marked by smaller factors)
    # as composite.
    movl $2, %esi
sieve_outer:
    movl %esi, %eax
    imull %esi, %eax           # eax = i * i
    cmpl $N, %eax
    jae sieve_outer_done
    cmpb $0, (%edi, %esi, 1)
    jne sieve_next_i
    movl %eax, %edx             # edx = j, starting at i * i
sieve_inner:
    cmpl $N, %edx
    jae sieve_next_i
    movb $1, (%edi, %edx, 1)
    addl %esi, %edx
    jmp sieve_inner
sieve_next_i:
    incl %esi
    jmp sieve_outer
sieve_outer_done:

    # Count the survivors and remember the largest one.
    xorl %ecx, %ecx             # ecx = count of primes found
    xorl %edx, %edx             # edx = largest prime found so far
    movl $2, %esi
count_loop:
    cmpl $N, %esi
    jae count_done
    cmpb $0, (%edi, %esi, 1)
    jne count_next
    incl %ecx
    movl %esi, %edx
count_next:
    incl %esi
    jmp count_loop
count_done:
    movl %ecx, (saved_count - get_base)(%ebx)
    movl %edx, (saved_last_prime - get_base)(%ebx)

    # Elapsed cycles since the recorded starting TSC (see the file header
    # comment on why the high 32 bits of this 64-bit delta are ignored).
    rdtsc
    subl (tsc_start_lo - get_base)(%ebx), %eax
    sbbl (tsc_start_hi - get_base)(%ebx), %edx
    movl %eax, (saved_elapsed - get_base)(%ebx)

    # Print the summary.
    leal (msg_banner - get_base)(%ebx), %esi
    call print_string

    leal (msg_count - get_base)(%ebx), %esi
    call print_string
    movl (saved_count - get_base)(%ebx), %eax
    call print_decimal
    movb $'\n', %al
    call print_char

    leal (msg_last - get_base)(%ebx), %esi
    call print_string
    movl (saved_last_prime - get_base)(%ebx), %eax
    call print_decimal
    movb $'\n', %al
    call print_char

    leal (msg_elapsed - get_base)(%ebx), %esi
    call print_string
    movl (saved_elapsed - get_base)(%ebx), %eax
    call print_decimal
    leal (msg_elapsed_suffix - get_base)(%ebx), %esi
    call print_string

    popl %ebx
    popl %esi
    popl %edi
    popl %ebp
    ret

#
# print_string: writes the NUL-terminated string at %esi (an already
# resolved, absolute runtime address) via print_char, one byte at a time.
# Clobbers %eax; preserves %esi by advancing a local copy.
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
# print_decimal: prints the unsigned 32-bit value in %eax as decimal digits,
# most significant digit first, with no leading zeroes (0 itself prints as
# a single "0"). Uses the stack to reverse the digit order from the
# repeated-division-by-10 algorithm. Clobbers %eax, %ecx, %edx. Tracks its
# pushed-digit count in %edi rather than %ebx: %ebx is this whole file's
# PIC load-base register, live across every call to print_char (which
# dereferences `cursor_pos - get_base` off of it), so repurposing it here
# -- an earlier version of this routine did exactly that -- silently broke
# every digit's on-screen position while this loop ran, without ever
# affecting the parallel COM1 mirror byte serial_putc also writes; that
# mismatch is exactly why this bug was invisible to a serial-only e2e
# test and only showed up watching the real VGA screen.
#
print_decimal:
    pushl %edi                  # repurposed here as a pushed-digit counter
    xorl %edi, %edi
    movl $10, %ecx
print_decimal_split:
    xorl %edx, %edx
    divl %ecx                   # eax = eax / 10, edx = eax % 10
    addl $'0', %edx
    pushl %edx
    incl %edi
    testl %eax, %eax
    jnz print_decimal_split
print_decimal_emit:
    popl %edx
    movb %dl, %al
    call print_char
    decl %edi
    jnz print_decimal_emit
    popl %edi
    ret

#
# print_char: prints the character in %al at the tracked cursor position,
# then re-programs the CRTC hardware cursor to match, and mirrors the same
# byte to COM1 (matching kernel/src/console.rs's own dual VGA+serial
# habit). '\n' moves to the start of the next text row without touching
# VGA memory. If that would advance past the last row (VGA_HEIGHT), the
# whole screen is scrolled up by one row first -- mirroring
# kernel/src/console.rs's own newline()/scroll logic -- since real
# hardware, unlike a fresh QEMU boot, can easily have already scrolled the
# cursor well down the screen by the time a tape runs (a longer BIOS/boot
# narration, more commands typed first, etc.); without this, this tape's
# own summary lines silently wrote past the visible 80x25 window and were
# never seen at all, a real bug found by running this tape on physical
# hardware. Clobbers nothing visible to the caller.
#
print_char:
    pushl %eax
    pushl %ecx
    pushl %edx
    call serial_putc
    cmpb $'\n', %al
    je print_char_newline

    movl (cursor_pos - get_base)(%ebx), %ecx
    movb %al, %dl
    movb $VGA_ATTR, %dh
    movw %dx, VGA_BASE(, %ecx, 2)
    incl %ecx
    movl %ecx, (cursor_pos - get_base)(%ebx)
    cmpl $(VGA_WIDTH * VGA_HEIGHT), %ecx
    jae print_char_scroll
    jmp print_char_move_cursor

print_char_newline:
    movl (cursor_pos - get_base)(%ebx), %eax
    xorl %edx, %edx
    movl $VGA_WIDTH, %ecx
    divl %ecx                   # eax = pos / 80, edx = pos % 80 (unused)
    incl %eax
    imull %ecx, %eax             # eax = (pos / 80 + 1) * 80
    movl %eax, (cursor_pos - get_base)(%ebx)
    cmpl $(VGA_WIDTH * VGA_HEIGHT), %eax
    jae print_char_scroll
    jmp print_char_move_cursor

#
# print_char_scroll: copies text rows 1..VGA_HEIGHT-1 up by one row, blanks
# the new last row, and resets cursor_pos to the start of that last row --
# the same outcome as kernel/src/console.rs's own newline()/scroll, just
# using `rep movsw`/`rep stosw` (hence the explicit %esi/%edi save/restore,
# since both are live caller state -- e.g. print_string's own %esi loop
# pointer -- that must come back unchanged, unlike %eax/%ecx/%edx, which
# print_char already saves/restores around its entire body).
#
print_char_scroll:
    pushl %esi
    pushl %edi
    movl $(VGA_BASE + VGA_WIDTH * 2), %esi
    movl $VGA_BASE, %edi
    movl $(VGA_WIDTH * (VGA_HEIGHT - 1)), %ecx
    rep movsw
    movl $(VGA_BASE + VGA_WIDTH * (VGA_HEIGHT - 1) * 2), %edi
    movw $((VGA_ATTR << 8) | ' '), %ax
    movl $VGA_WIDTH, %ecx
    rep stosw
    popl %edi
    popl %esi
    movl $(VGA_WIDTH * (VGA_HEIGHT - 1)), %eax
    movl %eax, (cursor_pos - get_base)(%ebx)

print_char_move_cursor:
    movl (cursor_pos - get_base)(%ebx), %ecx
    movb $CRTC_CURSOR_LOW, %al
    movw $CRTC_INDEX_PORT, %dx
    outb %al, %dx
    movb %cl, %al
    movw $CRTC_DATA_PORT, %dx
    outb %al, %dx
    movb $CRTC_CURSOR_HIGH, %al
    movw $CRTC_INDEX_PORT, %dx
    outb %al, %dx
    movb %ch, %al
    movw $CRTC_DATA_PORT, %dx
    outb %al, %dx

    popl %edx
    popl %ecx
    popl %eax
    ret

#
# serial_putc: writes the character in %al to COM1, waiting (with a
# bounded retry count, matching kernel/src/console.rs's own
# write_serial()) for the UART's transmit-holding-register-empty bit.
# Gives up silently if COM1 never becomes ready, exactly like the kernel
# does, rather than hanging forever. Preserves %al for the caller.
#
serial_putc:
    pushl %eax
    pushl %ecx
    pushl %edx
    movb %al, %ah
    movl $COM1_WAIT_ATTEMPTS, %ecx
serial_putc_wait:
    movw $COM1_LINE_STATUS, %dx
    inb %dx, %al
    testb $COM1_TX_EMPTY, %al
    jnz serial_putc_ready
    decl %ecx
    jnz serial_putc_wait
    jmp serial_putc_done
serial_putc_ready:
    movb %ah, %al
    movw $COM1_PORT, %dx
    outb %al, %dx
serial_putc_done:
    popl %edx
    popl %ecx
    popl %eax
    ret

    .align 4
tsc_start_lo:        .long 0
tsc_start_hi:         .long 0
cursor_pos:            .long 0
saved_count:            .long 0
saved_last_prime:        .long 0
saved_elapsed:            .long 0

msg_banner:       .asciz "fos test tape: Sieve of Eratosthenes up to 1,000,000\n"
msg_count:        .asciz "  primes found:   "
msg_last:         .asciz "  largest prime:  "
msg_elapsed:      .asciz "  elapsed:        "
msg_elapsed_suffix: .asciz " CPU cycles (TSC, uncalibrated)\n"

    # The sieve itself: one byte per candidate number from 0 to N - 1.
    # Embedded here as literal zero-filled space so it is unquestionably
    # part of this tape's own transferred buffer (see the file header
    # comment on why).
    .align 4
sieve:
    .fill N, 1, 0
