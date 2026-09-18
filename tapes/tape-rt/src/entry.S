/* `tape_run` (kernel/src/shell.rs) calls this tape with an ordinary cdecl
 * `call`, on the kernel's own stack, exactly the way any Rust `extern "C"
 * fn` is called -- unlike the kernel's own entry.S, which exists only to
 * bridge boot/stage2.S's raw `jmp` (which hands off arguments in a
 * register, not on the stack) to a normal Rust calling convention. A tape
 * needs no such bridge for that reason; this stub exists solely to zero
 * .bss before Rust code can rely on statics being zeroed, then tail-calls
 * into the real entry point so its `ret` returns straight to `tape_run`.
 *
 * Identical, byte for byte, to the `entry.S` every other Rust tape in
 * this repository (tapes/hello-rs, tapes/bigsieve) still keeps as its own
 * local copy -- see lib.rs's header comment for why this one copy lives
 * here instead, and why the other two were deliberately left alone.
 */
.section .text.entry, "ax"
.global _start
.extern tape_main
.extern __bss_start
.extern __bss_end

_start:
    mov edi, offset __bss_start
    mov ecx, offset __bss_end
    sub ecx, edi
    xor eax, eax
    rep stosb

    jmp tape_main
