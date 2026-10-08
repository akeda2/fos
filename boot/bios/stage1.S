/*
 * Fosfor OS, BIOS path, stage one: the 512-byte MBR.
 *
 * The BIOS loads this sector to 0x7c00 and jumps to it in 16-bit real mode, with the boot
 * drive number in DL. All stage one does is read stage two (16 sectors, 8 KiB) from the
 * "BIOS boot gap" at LBA 64 into 0x8000 with the BIOS's extended disk calls (INT 13h
 * AH=42h) and jump there. LBA 34-2047 of a GPT disk lies between the partition entry array
 * and the first partition, so nothing else uses it and no partition is needed.
 *
 * Bytes 446-509 are the partition table and 510-511 the 0xaa55 signature; the image builder
 * writes a protective MBR entry there, so the code and data below must stay under byte 446.
 * Narration goes to the screen only (INT 10h teletype): stage two sets up the serial port.
 */
.code16
.set STAGE2_LBA, 64
.set STAGE2_SECTORS, 16
.set STAGE2_ADDRESS, 0x8000

.global _start
_start:
    cli
    xorw %ax, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    movw $0x7c00, %sp
    ljmp $0, $start_normalized      /* CS may be 0x07c0:0 on some BIOSes; make it 0:0x7c00+ */

start_normalized:
    movb %dl, boot_drive
    movw $banner, %si
    call print
    movb boot_drive, %dl
    movw $disk_packet, %si
    movb $0x42, %ah                 /* INT 13h AH=42h: extended read, DS:SI -> disk address packet */
    int $0x13
    jc disk_error
    movw $jumping, %si
    call print
    movb boot_drive, %dl            /* stage two wants the drive number in DL too */
    ljmp $0, $STAGE2_ADDRESS

disk_error:
    movw $error, %si
    call print
hang:
    cli
    hlt
    jmp hang

print:
    lodsb
    testb %al, %al
    jz 1f
    movb $0x0e, %ah                 /* INT 10h AH=0Eh: teletype output */
    movb $0x07, %bl
    int $0x10
    jmp print
1:  ret

boot_drive: .byte 0
banner:  .asciz "Fosfor stage 1 (MBR): BIOS put 512 bytes at 0x7c00. Reading stage 2 from LBA 64...\r\n"
jumping: .asciz "Stage 2 is at 0x8000. Jumping there.\r\n"
error:   .asciz "Fosfor: disk read failed.\r\n"

.align 4
disk_packet:
    .byte 0x10, 0                   /* packet size, reserved */
    .word STAGE2_SECTORS
    .word STAGE2_ADDRESS, 0         /* offset, segment */
    .quad STAGE2_LBA

.org 510
.word 0xaa55
