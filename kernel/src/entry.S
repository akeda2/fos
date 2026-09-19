.section .text.entry, "ax"
.global _start
.extern kmain
.extern __bss_start
.extern __bss_end

_start:
    cli
    mov esp, offset kernel_stack_top
    xor ebp, ebp

    mov edi, offset __bss_start
    mov ecx, offset __bss_end
    sub ecx, edi
    xor eax, eax
    rep stosb

    push ebx
    call kmain

.halt:
    cli
    hlt
    jmp .halt

.section .bss
.align 16
kernel_stack:
    # Networking tapes can run nested inside `term net` and borrow this
    # stack for their fixed packet buffers and TCP state.  Keep enough
    # headroom for that call chain without introducing a tape-specific ABI.
    .skip 32768
kernel_stack_top:
