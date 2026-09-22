/*
 * tapes/longmode.S -- the hand-written-assembly `tape64_body` for the
 * "visits 64-bit long mode and comes back" tape.
 *
 * All of the privileged CPU-mode-switching machinery (CPUID check, page
 * tables, private GDT, state save/restore, the actual mode transition)
 * lives in `tapes/longmode64_shim.S` now, shared with any other tape
 * that wants to run something in 64-bit mode -- see that file's header
 * comment for the reusable `tape64_body` contract, and AGENTS.md's "A
 * tape that visits 64-bit long mode and comes back" section for the
 * full design rationale (why direct VGA writes prove long mode instead
 * of the `putc` callback, why interrupts stay off throughout, the
 * RELA-vs-REL linker requirement, and the QEMU/CPUID testing caveats --
 * none of that is repeated here).
 *
 * This file is the *body*: everything `tape64_body` actually does once
 * the shim has already entered 64-bit mode, with its private GDT loaded
 * and interrupts masked. It loads a 64-bit-only immediate
 * (`0xdeadbeefcafebabe` -- a value no 32-bit `mov` could ever load in
 * one instruction) and hex-dumps it directly to VGA text memory at
 * 0xb8000, one (character, attribute) cell per the same convention
 * `tapes/vga-colors/src/main.rs` uses, as proof that 64-bit-encoded
 * instructions genuinely ran. `tapes/longmode-rs/` provides an
 * equivalent `tape64_body` written in Rust instead, linked against the
 * exact same shim -- compare the two to see how little is actually
 * different (page tables/GDT/mode-switch/register-preservation logic
 * needs none of it duplicated; only the "what to actually do in 64-bit
 * mode" part differs).
 *
 * `tape64_body` is called with an ordinary SysV64 `call` (stack aligned
 * by the shim beforehand) and returns with a plain `ret`. The shim now
 * passes trailing optional arguments (`args_ptr`, `args_len`,
 * `report_ptr`) for newer bodies, but this one intentionally ignores
 * them and remains a zero-argument body by design.
 *
 * Build: `make tape-longmode` assembles this and `longmode64_shim.S`
 * both with `as --64`, links them together with a fixed
 * `-Ttext=0x100000` (matching TAPE_BASE) and `-e tape_main` (the shim's
 * entry point, not this file's), and extracts a flat binary with
 * `objcopy -O binary`, producing build/tapes/longmode.bin.
 */

.code64
.text
.global tape64_body

tape64_body:
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

    ret

hex_digits:
    .ascii "0123456789abcdef"

msg_vga_prefix:
    .asciz "64-BIT MODE REACHED, RAX="
