.code16
.section .text
.include "layout.inc"

.set STAGE2_BASE, 0x8000
.set BOOT_INFO, 0x5000
.set MEMORY_MAP, BOOT_INFO + 48
.set MAX_MEMORY_REGIONS, 32
.set COM1, 0x3f8
.set DISK_REQUEST, 0x5400
# DISK_REQUEST holds the protected-mode call arguments and a 16-byte EDD
# packet. Its fields are: direction byte at +0, status byte at +1, BIOS
# drive at +2, LBA low/high dwords at +4/+8, buffer physical address at +12,
# and the DAP at +16.
.set DISK_SAVED_ESP, 0x5440
.set DISK_SAVED_CR0, 0x5444

.global _start
_start:
    cli
    xorw %ax, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    movw $0x7a00, %sp
    movb %dl, boot_drive + STAGE2_BASE

    call initialize_serial
    movw $(banner + STAGE2_BASE), %si
    call print
    call initialize_boot_info
    call collect_memory_map
    jc memory_map_error
    movw $(memory_map_done + STAGE2_BASE), %si
    call print
    call enable_a20
    jc a20_error
    movw $(a20_done + STAGE2_BASE), %si
    call print
    call load_kernel
    jc kernel_error
    movw $(kernel_done + STAGE2_BASE), %si
    call print
    movw $(transition_message + STAGE2_BASE), %si
    call print
    call enter_protected_mode

memory_map_error:
    movw $(memory_map_failed + STAGE2_BASE), %si
    jmp fatal
a20_error:
    movw $(a20_failed + STAGE2_BASE), %si
    jmp fatal
kernel_error:
    movw $(kernel_failed + STAGE2_BASE), %si
fatal:
    call print
halt:
    cli
    hlt
    jmp halt

initialize_boot_info:
    movw $BOOT_INFO, %di
    xorw %ax, %ax
    movw $(48 + MAX_MEMORY_REGIONS * 24) / 2, %cx
    rep stosw
    movw $0x5342, BOOT_INFO
    movw $0x464f, BOOT_INFO + 2
    movw $3, BOOT_INFO + 4
    movw $(48 + MAX_MEMORY_REGIONS * 24), BOOT_INFO + 6
    movb boot_drive + STAGE2_BASE, %al
    movb %al, BOOT_INFO + 8
    movb serial_available + STAGE2_BASE, %al
    movb %al, BOOT_INFO + 9
    movl $0x10000, BOOT_INFO + 12
    movl $KERNEL_END, BOOT_INFO + 16
    movl $(bios_disk_io + STAGE2_BASE), BOOT_INFO + 20
    movl $IMAGE_SECTORS_LO, BOOT_INFO + 24
    movl $IMAGE_SECTORS_HI, BOOT_INFO + 28
    movl $DATA_START_LBA_LO, BOOT_INFO + 32
    movl $DATA_START_LBA_HI, BOOT_INFO + 36
    movl $DATA_SECTORS_LO, BOOT_INFO + 40
    movl $DATA_SECTORS_HI, BOOT_INFO + 44
    ret

collect_memory_map:
    xorw %ax, %ax
    movw %ax, %es
    movw $MEMORY_MAP, %di
    xorl %ebx, %ebx
    xorw %bp, %bp
memory_next:
    cmpl $MAX_MEMORY_REGIONS, %ebp
    jae memory_done
    movl $0xe820, %eax
    movl $0x534d4150, %edx
    movl $24, %ecx
    movl $1, %es:20(%di)
    int $0x15
    jc memory_failed
    cmpl $0x534d4150, %eax
    jne memory_failed
    cmpw $20, %cx
    jb memory_failed
    addw $24, %di
    incw %bp
    testl %ebx, %ebx
    jnz memory_next
memory_done:
    movw %bp, BOOT_INFO + 10
    clc
    ret
memory_failed:
    stc
    ret

# Tries progressively less gentle ways to enable the A20 address line,
# stopping as soon as `verify_a20` reports success. Real hardware varies in
# which of these a given chipset actually implements, and -- more
# importantly -- some older boards react badly (occasionally even
# resetting the CPU) to writes on the "fast A20 gate" system control port
# (0x92) used below, so it is deliberately tried last, only once the two
# safer options have already failed. Every step re-verifies rather than
# trusting a method's own reported success, since a BIOS or chipset can
# claim success without actually toggling the line.
enable_a20:
    call verify_a20
    jnc enable_a20_done

    # BIOS-assisted enable (INT 15h, AX=2401h): asks the platform to do it,
    # touching no chipset ports directly.
    movw $0x2401, %ax
    int $0x15
    call verify_a20
    jnc enable_a20_done

    # The traditional 8042 keyboard-controller gate, supported by every PC
    # chipset since the original IBM AT and far less likely than port 0x92
    # to have unwanted side effects on quirky real hardware.
    call enable_a20_via_keyboard_controller
    call verify_a20
    jnc enable_a20_done

    # Last resort: the "fast A20 gate". Bit 0 of this port can trigger an
    # immediate CPU reset on some chipsets, so it is explicitly cleared
    # here (never set) before the write.
    inb $0x92, %al
    orb $0x02, %al
    andb $0xfe, %al
    outb %al, $0x92
    call verify_a20
enable_a20_done:
    ret

# Enables A20 via the keyboard controller's "write output port" command,
# the same technique used by DOS-era bootloaders and, later, GRUB. Every
# keyboard-controller access is bounded (see `kbc_wait_input_clear`/
# `kbc_wait_output_full`) so a machine with no responding 8042 controller
# still returns instead of hanging the boot forever.
enable_a20_via_keyboard_controller:
    call kbc_wait_input_clear
    movb $0xad, %al
    outb %al, $0x64
    call kbc_wait_input_clear
    movb $0xd0, %al
    outb %al, $0x64
    call kbc_wait_output_full
    inb $0x60, %al
    pushw %ax
    call kbc_wait_input_clear
    movb $0xd1, %al
    outb %al, $0x64
    call kbc_wait_input_clear
    popw %ax
    orb $0x02, %al
    outb %al, $0x60
    call kbc_wait_input_clear
    movb $0xae, %al
    outb %al, $0x64
    call kbc_wait_input_clear
    ret

kbc_wait_input_clear:
    movw $0xffff, %cx
kbc_wait_input_clear_loop:
    inb $0x64, %al
    testb $0x02, %al
    jz kbc_wait_input_clear_done
    loop kbc_wait_input_clear_loop
kbc_wait_input_clear_done:
    ret

kbc_wait_output_full:
    movw $0xffff, %cx
kbc_wait_output_full_loop:
    inb $0x64, %al
    testb $0x01, %al
    jnz kbc_wait_output_full_done
    loop kbc_wait_output_full_loop
kbc_wait_output_full_done:
    ret

verify_a20:
    pushw %ds
    pushw %es
    xorw %ax, %ax
    movw %ax, %ds
    movw $0xffff, %ax
    movw %ax, %es
    movw $0x0500, %si
    movw $0x0510, %di
    movb (%si), %al
    movb %es:(%di), %ah
    pushw %ax
    movb $0, (%si)
    movb $0xff, %es:(%di)
    cmpb $0xff, (%si)
    popw %ax
    movb %al, (%si)
    movb %ah, %es:(%di)
    popw %es
    popw %ds
    je a20_still_disabled
    clc
    ret
a20_still_disabled:
    stc
    ret

load_kernel:
    movb boot_drive + STAGE2_BASE, %dl
    movw $KERNEL_SECTORS, kernel_remaining + STAGE2_BASE
    movl $KERNEL_LBA, kernel_packet + STAGE2_BASE + 8
    movw $0x1000, kernel_packet + STAGE2_BASE + 6
load_kernel_chunk:
    movw kernel_remaining + STAGE2_BASE, %cx
    cmpw $126, %cx
    jbe load_kernel_final

    # An EDD transfer may not cross a 64 KiB boundary.  Transfer 126
    # sectors (0xfc00 bytes), then finish the current 64 KiB window with
    # one or two sectors before moving the segment base to the next
    # boundary.  The old loader used one 127-sector transfer, which is why
    # the image builder historically rejected larger kernels.
    movw $126, kernel_packet + STAGE2_BASE + 2
    call load_kernel_packet
    jc load_kernel_done
    subw $126, kernel_remaining + STAGE2_BASE
    addl $126, kernel_packet + STAGE2_BASE + 8
    addw $0xfc0, kernel_packet + STAGE2_BASE + 6

    movw kernel_remaining + STAGE2_BASE, %cx
    cmpw $2, %cx
    jae load_kernel_boundary_pair
    movw %cx, kernel_packet + STAGE2_BASE + 2
    call load_kernel_packet
    jmp load_kernel_done

load_kernel_boundary_pair:
    movw $2, kernel_packet + STAGE2_BASE + 2
    call load_kernel_packet
    jc load_kernel_done
    subw $2, kernel_remaining + STAGE2_BASE
    addl $2, kernel_packet + STAGE2_BASE + 8
    addw $0x40, kernel_packet + STAGE2_BASE + 6
    jmp load_kernel_chunk

load_kernel_final:
    movw %cx, kernel_packet + STAGE2_BASE + 2
    call load_kernel_packet

load_kernel_done:
    ret

load_kernel_packet:
    movw $(kernel_packet + STAGE2_BASE), %si
    movb $0x42, %ah
    int $0x13
    ret

enter_protected_mode:
    cli
    lgdt gdt_descriptor + STAGE2_BASE
    movb $0xff, %al
    outb %al, $0xa1
    outb %al, $0x21
    movl %cr0, %eax
    orl $1, %eax
    movl %eax, %cr0
    ljmp $0x08, $(protected_entry + STAGE2_BASE)

print:
    lodsb
    testb %al, %al
    jz print_done
    pushw %ax
    movb $0x0e, %ah
    movb $0x07, %bl
    int $0x10
    popw %ax
    call serial_write_byte
    jmp print
print_done:
    ret

initialize_serial:
    movb $0, serial_available + STAGE2_BASE
    movb $0, %al
    movw $(COM1 + 1), %dx
    outb %al, %dx
    movb $0x80, %al
    movw $(COM1 + 3), %dx
    outb %al, %dx
    movb $1, %al
    movw $COM1, %dx
    outb %al, %dx
    movb $0, %al
    movw $(COM1 + 1), %dx
    outb %al, %dx
    movb $0x03, %al
    movw $(COM1 + 3), %dx
    outb %al, %dx
    movb $0xc7, %al
    movw $(COM1 + 2), %dx
    outb %al, %dx
    movb $0x0b, %al
    movw $(COM1 + 4), %dx
    outb %al, %dx
    movw $(COM1 + 5), %dx
    inb %dx, %al
    cmpb $0xff, %al
    je serial_init_done
    movb $1, serial_available + STAGE2_BASE
serial_init_done:
    ret

serial_write_byte:
    cmpb $0, serial_available + STAGE2_BASE
    je serial_write_done
    pushw %bx
    pushw %cx
    pushw %dx
    movb %al, %bl
    movw $10000, %cx
serial_wait:
    movw $(COM1 + 5), %dx
    inb %dx, %al
    testb $0x20, %al
    jnz serial_send
    loop serial_wait
    movb $0, serial_available + STAGE2_BASE
    jmp serial_restore
serial_send:
    movw $COM1, %dx
    movb %bl, %al
    outb %al, %dx
serial_restore:
    popw %dx
    popw %cx
    popw %bx
serial_write_done:
    ret

boot_drive:
    .byte 0
serial_available:
    .byte 0
banner:
    .asciz "FOS stage 2: real mode active. Now running from 0x8000.\r\nQuerying BIOS INT 15h EAX=E820h for the physical memory map...\r\n"
memory_map_done:
    .asciz "Memory map collected.\r\nEnabling the A20 address line (needed to address memory above 1 MiB)...\r\n"
a20_done:
    .asciz "A20 enabled.\r\nLoading the Rust kernel from disk to physical 0x10000...\r\n"
kernel_done:
    .asciz "Kernel loaded. BIOS services end now.\r\nMasking the legacy 8259 PIC and installing a flat 32-bit GDT...\r\n"
transition_message:
    .asciz "GDT loaded, CR0.PE set. Far-jumping into 32-bit protected mode and the Rust kernel.\r\n"
memory_map_failed:
    .asciz "FOS ERROR: E820 memory map unavailable.\r\n"
a20_failed:
    .asciz "FOS ERROR: unable to enable A20.\r\n"
kernel_failed:
    .asciz "FOS ERROR: kernel disk read failed.\r\n"

.align 4
kernel_packet:
    .byte 0x10, 0
    .word KERNEL_SECTORS
    .word 0, 0x1000
    .quad KERNEL_LBA
kernel_remaining:
    .word 0

.align 8
gdt:
    .quad 0
    .quad 0x00cf9a000000ffff
    .quad 0x00cf92000000ffff
    .quad 0x00009a000000ffff
    .quad 0x000092000000ffff
gdt_descriptor:
    .word gdt_descriptor - gdt - 1
    .long gdt + STAGE2_BASE

.code32
protected_entry:
    movw $0x10, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %fs
    movw %ax, %gs
    movw %ax, %ss
    movl $0x9f000, %esp
    movl $BOOT_INFO, %ebx
    movl $0x10000, %eax
    call *%eax
protected_halt:
    cli
    hlt
    jmp protected_halt

bios_disk_io:
    pushl %ebp
    movl %esp, %ebp
    pushl %ebx
    pushl %esi
    pushl %edi
    movl %esp, DISK_SAVED_ESP
    movl 8(%ebp), %eax
    cmpl $1, %eax
    ja bios_disk_invalid
    movl %eax, DISK_REQUEST
    movl 12(%ebp), %eax
    movl %eax, DISK_REQUEST + 4
    movl 16(%ebp), %eax
    movl %eax, DISK_REQUEST + 8
    movl 20(%ebp), %eax
    testl $0x0f, %eax
    jnz bios_disk_invalid
    cmpl $0x100000, %eax
    jae bios_disk_invalid
    movl %eax, DISK_REQUEST + 12
    movb BOOT_INFO + 8, %al
    movb %al, DISK_REQUEST + 2
    ljmp $0x18, $(bios_disk_protected16 + STAGE2_BASE)

bios_disk_invalid:
    movl $1, %eax
    popl %edi
    popl %esi
    popl %ebx
    popl %ebp
    ret

.code16
bios_disk_protected16:
    movw $0x20, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    movl %cr0, %eax
    movl %eax, DISK_SAVED_CR0
    # A paged kernel must clear both PG and PE before the real-mode far
    # transfer. Real mode has no page-table translation, so leaving PG set
    # would make BIOS code execute with an invalid architectural state.
    andl $0x7ffffffe, %eax
    movl %eax, %cr0
    ljmp $0, $(bios_disk_real + STAGE2_BASE)

bios_disk_real:
    xorw %ax, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    movw $0x7a00, %sp
    movw $0x0010, DISK_REQUEST + 16
    movw $1, DISK_REQUEST + 18
    movl DISK_REQUEST + 4, %eax
    movl %eax, DISK_REQUEST + 24
    movl DISK_REQUEST + 8, %eax
    movl %eax, DISK_REQUEST + 28
    movl DISK_REQUEST + 12, %eax
    shrl $4, %eax
    movw %ax, DISK_REQUEST + 22
    movw $0, DISK_REQUEST + 20
    movb DISK_REQUEST + 2, %dl
    movw $DISK_REQUEST + 16, %si
    # `disk::transfer` deliberately keeps IF=0 while the CPU is still in
    # protected mode with IDTR=0 (the real-mode IVT is not a valid
    # protected-mode IDT). The PIC/IVT are now ready and we are genuinely in
    # real mode, so BIOS disk timeout/tick handling can safely run IRQ0.
    sti
    cmpb $0, DISK_REQUEST
    jne bios_disk_write
    movb $0x42, %ah
    jmp bios_disk_invoke
bios_disk_write:
    # INT 13h extensions use AL as the write-control flags for AH=43h.
    # The DAP segment calculation above leaves an unrelated value in AL;
    # request an ordinary write with no verify/reserved flags.
    xorb %al, %al
    movb $0x43, %ah
bios_disk_invoke:
    int $0x13
    jc bios_disk_failed
    movb $0, DISK_REQUEST + 1
    jmp bios_disk_return_to_pm
bios_disk_failed:
    movb $1, DISK_REQUEST + 1
bios_disk_return_to_pm:
    cli
    # BIOS interrupt handlers do not provide a kernel ABI guarantee for DF.
    # Rust/string operations require forward direction, so normalize it before
    # any protected-mode code or copied FOSFS/tape data runs after INT 13h.
    cld
    lgdt gdt_descriptor + STAGE2_BASE
    movl %cr0, %eax
    orl $1, %eax
    movl %eax, %cr0
    ljmp $0x08, $(bios_disk_pm_return + STAGE2_BASE)

.code32
bios_disk_pm_return:
    movw $0x10, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %fs
    movw %ax, %gs
    movw %ax, %ss
    movl DISK_SAVED_ESP, %esp
    # Restore the caller's complete CR0 only after protected-mode segments
    # and the original stack are usable again. The paging foundation keeps
    # this path identity-mapped, so the following instructions and return
    # address retain the same numeric addresses across the transition.
    movl DISK_SAVED_CR0, %eax
    movl %eax, %cr0
    movzbl DISK_REQUEST + 1, %eax
    popl %edi
    popl %esi
    popl %ebx
    popl %ebp
    ret
