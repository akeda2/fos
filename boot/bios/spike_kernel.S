/*
 * Fosfor OS, BIOS path spike B1: a 64-bit test "kernel" for stage two to load and run.
 *
 * Linked at 0x100000 as a flat binary. It proves, over COM1 and on the screen, that stage two
 * got us into 64-bit long mode with the boot information, and that a multi-piece copy above
 * 1 MiB arrived intact: the image is padded to well over one 16 KiB read, and its last word is
 * a marker this code checks.
 */
.code64
.global _start
_start:
    movq %rdi, %rbx                    /* boot information from stage two */
    leaq msg_hello(%rip), %rsi
    call puts
    cmpl $0x30494246, (%rbx)           /* 'FBI0' */
    jne bad_info
    leaq msg_entries(%rip), %rsi
    call puts
    movl 4(%rbx), %eax
    call put_decimal
    leaq msg_usable(%rip), %rsi
    call puts
    movq 8(%rbx), %rsi                 /* E820 table: add up the type-1 (usable) lengths */
    movl 4(%rbx), %ecx
    xorq %rax, %rax
1:  cmpl $1, 16(%rsi)
    jne 2f
    addq 8(%rsi), %rax
2:  addq $24, %rsi
    decl %ecx
    jnz 1b
    shrq $20, %rax                     /* MiB */
    call put_decimal
    leaq msg_mib(%rip), %rsi
    call puts
    cmpl $0x21444e45, end_marker(%rip) /* 'END!' */
    jne bad_tail
    leaq msg_tail_ok(%rip), %rsi
    call puts
    leaq msg_done(%rip), %rsi
    call puts
    /* the screen too: white on blue "LONG MODE" in the top-left corner */
    movq $0xb8000, %rdi
    leaq msg_screen(%rip), %rsi
3:  lodsb
    testb %al, %al
    jz halt
    movb %al, (%rdi)
    movb $0x1f, 1(%rdi)
    addq $2, %rdi
    jmp 3b
halt:
    cli
4:  hlt
    jmp 4b
bad_info:
    leaq msg_bad_info(%rip), %rsi
    call puts
    jmp halt
bad_tail:
    leaq msg_bad_tail(%rip), %rsi
    call puts
    jmp halt

puts:
    lodsb
    testb %al, %al
    jz 1f
    call putc
    jmp puts
1:  ret
putc:
    movb %al, %ah
2:  movw $0x3fd, %dx
    inb %dx, %al
    testb $0x20, %al
    jz 2b
    movw $0x3f8, %dx
    movb %ah, %al
    outb %al, %dx
    ret
put_decimal:                           /* RAX as decimal */
    leaq digits_end(%rip), %rdi
    movq %rdi, %rsi
    movl $10, %ecx
1:  xorq %rdx, %rdx
    divq %rcx
    addb $'0', %dl
    decq %rdi
    movb %dl, (%rdi)
    testq %rax, %rax
    jnz 1b
    movq %rdi, %rsi
    leaq digits_end(%rip), %rdx
5:  cmpq %rdx, %rsi
    jae 6f
    lodsb
    pushq %rsi
    pushq %rdx
    call putc
    popq %rdx
    popq %rsi
    jmp 5b
6:  ret

msg_hello:    .asciz "SPIKE: long mode reached from BIOS, 64-bit kernel running above 1 MiB.\r\n"
msg_entries:  .asciz "SPIKE: E820 entries: "
msg_usable:   .asciz ", usable RAM: "
msg_mib:      .asciz " MiB\r\n"
msg_tail_ok:  .asciz "SPIKE: kernel tail intact (multi-piece copy ok)\r\n"
msg_done:     .asciz "SPIKE: done\r\n"
msg_bad_info: .asciz "SPIKE: boot information magic is wrong\r\n"
msg_bad_tail: .asciz "SPIKE: kernel tail is BAD\r\n"
msg_screen:   .asciz "LONG MODE"
digits:       .space 24
digits_end:
.fill 70000, 1, 0x5a                   /* padding: several 16 KiB pieces */
end_marker:   .ascii "END!"
