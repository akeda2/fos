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
 * The shell calls this tape as `extern "C" fn(putc: extern "C" fn(u32))`
 * (see kernel/src/shell.rs's `tape_run`), i.e. plain cdecl with one
 * argument: a function pointer this file must call to print one byte,
 * passed on the stack (at `4(%esp)` on entry, before any of this file's
 * own pushes) exactly the way any C caller would pass it. %eax, %ecx,
 * %edx are ours to clobber freely (and calling through the passed
 * pointer clobbers them too, same as any other cdecl call); %ebx, %esi,
 * %edi, %ebp (and %esp) must come back exactly as they went in, or the
 * shell code that resumes after `tape run` could behave unpredictably.
 * This file saves those four registers on entry and restores them
 * immediately before the final `ret`, even though %ebx is then
 * immediately repurposed as this file's own load base for the rest of
 * its run, and the passed callback pointer is saved (relative to that
 * base) into `putc_fn` before %eax is clobbered by anything else.
 *
 * VGA, serial, and cursor output
 * --------------------------------
 * All output goes through the one passed-in `putc_fn` callback, which
 * `tape_run` sets to `console::print_byte` -- a thin wrapper around the
 * same `console::write_bytes` every other kernel message uses. That means
 * this file no longer touches VGA memory, the CRTC cursor ports, or COM1
 * directly at all: the kernel's own console code already handles VGA
 * placement, scrolling, the hardware cursor, the COM1 mirror, and (when a
 * `term net` session is active) forwarding to a remote client, all in one
 * place, uniformly for every caller. `print_char` below is now just a
 * one-byte-at-a-time wrapper around calling that pointer.
 *
 * This is a deliberate, narrow exception to "a tape cannot call back into
 * the kernel" (see below): earlier versions of this file reimplemented
 * VGA/cursor/serial output by hand instead, which needed its own
 * from-scratch scroll-bounds logic (a real bug here once let output
 * silently scroll off an already-mostly-full real-hardware screen) and
 * kept its own `cursor_pos`, disconnected from the kernel's own -- both
 * problems this callback design eliminates entirely, since there is now
 * only ever one piece of code (the kernel's own) that ever touches VGA
 * state. It also means this tape's output is no longer local-only: it
 * now reaches a remote `term net` client exactly like any other command's
 * output does (see AGENTS.md's "Remote terminal" section), which was the
 * entire reason this callback was added.
 *
 * Timing
 * -------
 * There is still no callback for timing (RDTSC is a plain CPU instruction
 * available in ring 0 regardless, so one was never needed), so elapsed
 * time is measured with RDTSC around the sieve and reported as raw CPU
 * cycles rather than calibrated seconds -- it is a relative, uncalibrated
 * measurement, not a wall-clock time, and is printed as such. The 64-bit
 * TSC delta's high 32 bits are assumed to be zero: at any realistic CPU
 * clock speed, sieving a million numbers finishes in far under the ~4
 * billion cycles the low 32 bits alone can hold, so only the low half is
 * printed.
 *
 * Build: `make tape-primes` assembles this with the same `as --32` +
 * `objcopy -O binary` recipe used for boot/stage1.S and boot/stage2.S,
 * producing build/tapes/primes.bin.
 */

.code32
.text
.global _start

.equ N,                   1000000

_start:
    # cdecl argument #1 (the kernel's print-byte callback) sits at
    # 4(%esp) right now -- 0(%esp) is the return address `call _start`
    # pushed, and nothing else has touched %esp yet. Grab it into %eax
    # before the pushes below shift everything, so it can be saved
    # (relative to our PIC base) once that base is known.
    movl 4(%esp), %eax

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
    movl %eax, (putc_fn - get_base)(%ebx)

    # Record the starting TSC so we can report elapsed cycles later.
    rdtsc
    movl %eax, (tsc_start_lo - get_base)(%ebx)
    movl %edx, (tsc_start_hi - get_base)(%ebx)

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
# dereferences `putc_fn - get_base` off of it to find the kernel's print
# callback), so repurposing it here -- an earlier version of this routine
# did exactly that -- silently broke every digit's on-screen position
# while this loop ran, without affecting the COM1 mirror the kernel's own
# callback also produced; that mismatch is exactly why this bug was
# invisible to a serial-only e2e test and only showed up watching the
# real VGA screen.
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
# print_char: prints the byte in %al by calling back into the kernel's own
# console code (see the file header comment) via the saved `putc_fn`
# pointer, zero-extended to 32 bits to match its `extern "C" fn(u32)`
# signature and pushed as its one cdecl argument. The callback (usually
# `console::print_byte`) handles VGA placement, scrolling, the hardware
# cursor, the COM1 mirror, and term_net relaying all by itself -- this
# routine no longer needs to know or care about any of that. Clobbers
# nothing visible to the caller.
#
print_char:
    pushl %eax
    pushl %ecx
    pushl %edx
    movzbl %al, %eax
    pushl %eax
    call *(putc_fn - get_base)(%ebx)
    addl $4, %esp
    popl %edx
    popl %ecx
    popl %eax
    ret

    .align 4
putc_fn:              .long 0
tsc_start_lo:        .long 0
tsc_start_hi:         .long 0
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
