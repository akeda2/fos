.code16
.section .text
.include "layout.inc"

.set STAGE1_BASE, 0x7c00

# Stage one only writes to the VGA text console (BIOS INT 10h teletype
# output). It deliberately does not initialize the COM1 UART itself: doing
# so would duplicate stage two's serial setup inside this 512-byte MBR
# sector, which has very little spare room once the partition table (bytes
# 446-509) and the 0x55aa boot signature are reserved. Stage two performs
# full serial initialization immediately after it takes over, so run with
# a VGA display (`make run`) to see these very first messages.
.global _start
_start:
    cli
    xorw %ax, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    movw $0x7c00, %sp
    ljmp $0, $(start_normalized + STAGE1_BASE)

start_normalized:
    movb %dl, boot_drive + STAGE1_BASE
    movw $(banner_message + STAGE1_BASE), %si
    call print
    movw $(loading_message + STAGE1_BASE), %si
    call print
    call load_stage2
    jc disk_error
    movw $(jump_message + STAGE1_BASE), %si
    call print
    movb boot_drive + STAGE1_BASE, %dl
    ljmp $0, $0x8000

disk_error:
    movw $(error_message + STAGE1_BASE), %si
    call print
hang:
    cli
    hlt
    jmp hang

load_stage2:
    movb boot_drive + STAGE1_BASE, %dl
    movw $(disk_packet + STAGE1_BASE), %si
    movb $0x42, %ah
    int $0x13
    ret

print:
    lodsb
    testb %al, %al
    jz print_done
    movb $0x0e, %ah
    movb $0x07, %bl
    int $0x10
    jmp print
print_done:
    ret

boot_drive:
    .byte 0
banner_message:
    .asciz "FOS stage 1 (MBR): BIOS placed 512 bytes at 0x7c00 and jumped here.\r\n"
loading_message:
    .asciz "Reading stage 2 (32 sectors from LBA 2048) via BIOS EDD INT 13h AH=42h...\r\n"
jump_message:
    .asciz "Stage 2 loaded at 0x8000. Jumping there now.\r\n"
error_message:
    .asciz "FOS: disk read failed.\r\n"

.align 4
disk_packet:
    .byte 0x10, 0
    .word STAGE2_SECTORS
    .word 0x8000, 0
    .quad STAGE2_LBA

.org 446
.space 64
.word 0xaa55
