/*
 * Fosfor OS, BIOS path, stage two (spike B1: docs/shared-codebase.md, section 4).
 *
 * Runs at 0x8000 in 16-bit real mode, entered from stage one with the boot drive in DL.
 * What it does, in order, narrating each step to the screen and to COM1:
 *
 *   1. Initialise COM1 (115200 8N1) so everything below can also be read over serial.
 *   2. Enable the A20 line (the BIOS first, then the "fast A20 gate" at port 0x92) and
 *      verify it: without A20 every address with bit 20 set aliases to the one below it.
 *   3. Collect the physical memory map with INT 15h AX=E820h into a table at 0x5100.
 *   4. Read the kernel image from disk with EDD (INT 13h AH=42h) in 16 KiB pieces into a low
 *      buffer and copy each piece above 1 MiB. The BIOS can only write below 1 MiB, and real
 *      mode can only address 1 MiB, so the copy uses "unreal mode": enter protected mode
 *      just long enough to load a 4 GiB segment limit into ES, return to real mode (the
 *      limit stays cached) and `rep movsl` through it. A BIOS call can reset that limit, so
 *      it is re-established before every copy.
 *   5. Identity-map the first 4 GiB with 2 MiB pages, turn on PAE and long mode
 *      (EFER.LME, then CR0.PG) and jump to the kernel in 64-bit mode with a pointer to
 *      the boot information in RDI.
 *
 * Boot information (the same shape the UEFI loader will hand over, in time) at 0x5000:
 *   +0 u32 magic 'FBI0'   +4 u32 E820 entry count   +8 u64 address of the E820 table
 * Each E820 entry is 24 bytes: u64 base, u64 length, u32 type, u32 ACPI extended attributes.
 *
 * Disk layout of the spike image (tools/build_bios_spike.py): the kernel's first sector, its
 * length and its destination are patched into the header at offset 0x10 by the builder.
 */
.code16
.set BOOT_INFO, 0x5000
.set E820_TABLE, 0x5100
.set E820_MAX, 64
.set LOW_BUFFER_SEGMENT, 0x1000        /* 0x10000, 64 KiB aligned so a read never crosses a 64 KiB boundary */
.set CHUNK_SECTORS, 32                 /* 16 KiB per EDD read */
.set PAGE_TABLES, 0x20000              /* PML4, PDPT, then four page directories */
.set STACK16, 0x7a00
.set STACK32, 0x90000

.global _start
_start:
    jmp start
.org 0x10
kernel_lba:     .quad 0                /* patched by the image builder */
kernel_sectors: .long 0
kernel_dest:    .long 0x100000

start:
    cli
    xorw %ax, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    movw $STACK16, %sp
    movb %dl, boot_drive
    call serial_init
    movw $m_banner, %si
    call puts16

    /* ---- A20 ---- */
    movw $m_a20, %si
    call puts16
    call a20_check
    jnz a20_ok
    movw $0x2401, %ax                  /* INT 15h AX=2401h: enable A20 through the BIOS */
    int $0x15
    call a20_check
    jnz a20_ok
    inb $0x92, %al                     /* fast A20 gate: bit 1 enables, bit 0 would reset the CPU */
    orb $2, %al
    andb $0xfe, %al
    outb %al, $0x92
    call a20_check
    jnz a20_ok
    movw $m_a20_fail, %si
    jmp fail
a20_ok:
    movw $m_ok, %si
    call puts16

    /* ---- memory map ---- */
    movw $m_e820, %si
    call puts16
    call collect_e820
    jc 1f
    cmpl $0, e820_count
    jne 2f
1:  movw $m_e820_fail, %si
    jmp fail
2:  movw $m_ok, %si
    call puts16

    /* ---- kernel: read in pieces, copy above 1 MiB ---- */
    movw $m_load, %si
    call puts16
    movl kernel_sectors, %eax
    testl %eax, %eax
    jnz 3f
    movw $m_no_kernel, %si
    jmp fail
3:  movl kernel_dest, %ecx             /* linear destination, advances by each piece; kept in memory because */
    movl %ecx, dest_cursor             /* the BIOS may clobber the upper halves of 32-bit registers */
    movl %eax, remaining_sectors
    movl kernel_lba, %eax
    movl %eax, dap_lba                 /* low 32 bits; the spike image is far below 2 TiB */
load_loop:
    movl remaining_sectors, %eax
    cmpl $CHUNK_SECTORS, %eax
    jbe 4f
    movl $CHUNK_SECTORS, %eax
4:  movw %ax, dap_count
    movl %eax, this_chunk
    movb boot_drive, %dl
    movw $dap, %si
    movb $0x42, %ah                    /* INT 13h AH=42h: extended read into LOW_BUFFER_SEGMENT:0 */
    int $0x13
    jc disk_fail_code
    call enter_unreal                  /* the BIOS may have reset ES's limit; get it back */
    pushw %ds
    movw $LOW_BUFFER_SEGMENT, %ax
    movw %ax, %ds                      /* source DS:ESI = 0x10000 */
    xorl %esi, %esi
    movl %cs:dest_cursor, %edi         /* DS now points at the buffer, so read our own data through CS */
    movl %cs:this_chunk, %ecx          /* sectors -> 32-bit words: * 512 / 4 */
    shll $7, %ecx
    addr32 rep movsl %ds:(%esi), %es:(%edi)
    popw %ds
    movl this_chunk, %eax
    movl %eax, %ecx
    shll $9, %ecx
    addl %ecx, dest_cursor
    addl %eax, dap_lba
    subl %eax, remaining_sectors
    jnz load_loop
    movw $m_ok, %si
    call puts16

    /* ---- long mode ---- */
    movw $m_long, %si
    call puts16
    cli
    lgdt gdt_descriptor
    movl %cr0, %eax
    orl $1, %eax                       /* PE: protected mode on */
    movl %eax, %cr0
    ljmpl $0x08, $protected32          /* far jump loads CS with the 32-bit code descriptor */

disk_fail_code:
    movb %ah, %al                      /* INT 13h status in AH: 01 bad command, 02 no address mark, 04 not found, 0c ... */
    call puthex8
disk_fail:
    movw $m_disk_fail, %si
fail:
    call puts16
    cli
1:  hlt
    jmp 1b

/* ---- unreal mode: ES gets a flat 4 GiB limit, then we return to real mode ---- */
enter_unreal:
    cli
    pushw %ds
    lgdt gdt_descriptor
    movl %cr0, %eax
    orl $1, %eax
    movl %eax, %cr0
    movw $0x10, %bx
    movw %bx, %es                      /* descriptor 0x10: base 0, limit 4 GiB, data */
    andl $0xfffffffe, %eax
    movl %eax, %cr0                    /* back to real mode; ES keeps its cached 4 GiB limit */
    popw %ds
    sti
    ret

/* ---- A20 test: does 0000:0500 alias to FFFF:0510? ZF clear = A20 is on ---- */
a20_check:
    pushw %ds
    pushw %es
    pushw %si
    pushw %di
    xorw %ax, %ax
    movw %ax, %ds
    movw $0xffff, %ax
    movw %ax, %es
    movw $0x0500, %si
    movw $0x0510, %di
    movb (%si), %al
    movb %al, a20_saved_low
    movb %es:(%di), %al
    movb %al, a20_saved_high
    movb $0x00, (%si)
    movb $0xff, %es:(%di)
    cmpb $0xff, (%si)                  /* equal: the write through FFFF:0510 landed on 0000:0500 (A20 off) */
    movb a20_saved_low, %al
    movb %al, (%si)
    movb a20_saved_high, %al
    movb %al, %es:(%di)
    popw %di
    popw %si
    popw %es
    popw %ds
    ret                                /* ZF set (equal) = aliasing = A20 off; ZF clear = A20 on */

/* ---- E820: fill E820_TABLE, set e820_count and the boot information; CF set on failure ---- */
collect_e820:
    xorl %ebx, %ebx
    movw $E820_TABLE, %di
    movl $0, e820_count
1:  movl $1, 20(%di)                   /* ask for the ACPI extended attributes word */
    movl $0xe820, %eax
    movl $24, %ecx
    movl $0x534d4150, %edx             /* 'SMAP' */
    int $0x15
    jc e820_error
    cmpl $0x534d4150, %eax
    jne e820_error
    cmpl $0, 8(%di)                    /* skip zero-length entries */
    jne 5f
    cmpl $0, 12(%di)
    je 6f
5:  incl e820_count
    addw $24, %di
6:  testl %ebx, %ebx
    jz 7f
    cmpl $E820_MAX, e820_count
    jb 1b
7:  movl $0x30494246, BOOT_INFO        /* 'FBI0' */
    movl e820_count, %eax
    movl %eax, BOOT_INFO + 4
    movl $E820_TABLE, BOOT_INFO + 8
    movl $0, BOOT_INFO + 12
    clc
    ret
e820_error:
    cmpl $0, e820_count                /* an error after at least one entry just ends the list */
    jne 7b
    stc
    ret

/* ---- console output: every character to COM1 and to the screen (BIOS teletype) ---- */
serial_init:
    movw $0x3f9, %dx
    xorb %al, %al
    outb %al, %dx                      /* no interrupts */
    movw $0x3fb, %dx
    movb $0x80, %al
    outb %al, %dx                      /* DLAB on: divisor latch */
    movw $0x3f8, %dx
    movb $1, %al                       /* divisor 1 = 115200 baud */
    outb %al, %dx
    movw $0x3f9, %dx
    xorb %al, %al
    outb %al, %dx
    movw $0x3fb, %dx
    movb $0x03, %al                    /* 8 data bits, no parity, 1 stop, DLAB off */
    outb %al, %dx
    movw $0x3fa, %dx
    xorb %al, %al                      /* FIFOs off: one byte at a time, waiting for the transmitter each time */
    outb %al, %dx
    ret

puthex8:                               /* AL as two hex digits */
    pushw %ax
    shrb $4, %al
    call puthex4
    popw %ax
    andb $0x0f, %al
puthex4:
    andb $0x0f, %al
    addb $'0', %al
    cmpb $'9', %al
    jbe 1f
    addb $7, %al
1:  jmp putc16

puts16:
    lodsb
    testb %al, %al
    jz 1f
    call putc16
    jmp puts16
1:  ret

putc16:
    pushw %ax
    pushw %dx
    movb %al, %ah
2:  movw $0x3fd, %dx
    inb %dx, %al
    andb $0x60, %al                    /* bit 5: holding register empty, bit 6: shifter empty too */
    cmpb $0x60, %al
    jne 2b
    movw $0x3f8, %dx
    movb %ah, %al
    outb %al, %dx
    popw %dx
    popw %ax
    pushw %ax
    pushw %bx
    movb $0x0e, %ah
    movw $0x0007, %bx
    int $0x10
    popw %bx
    popw %ax
    ret

/* ---- 32-bit protected mode, then long mode ---- */
.code32
protected32:
    movw $0x10, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    movl $STACK32, %esp
    /* zero the six tables (PML4, PDPT, 4 x PD) */
    movl $PAGE_TABLES, %edi
    xorl %eax, %eax
    movl $(6 * 4096 / 4), %ecx
    rep stosl
    movl $(PAGE_TABLES + 0x1000 + 3), PAGE_TABLES               /* PML4[0] -> PDPT, present+writable */
    movl $PAGE_TABLES + 0x1000, %edi
    movl $(PAGE_TABLES + 0x2000 + 3), %eax
    movl $4, %ecx
5:  movl %eax, (%edi)                                            /* PDPT[0..3] -> the four PDs */
    addl $0x1000, %eax
    addl $8, %edi
    loop 5b
    movl $(PAGE_TABLES + 0x2000), %edi
    movl $0x83, %eax                                            /* 2 MiB page, present+writable+PS */
    movl $(4 * 512), %ecx
6:  movl %eax, (%edi)
    addl $0x200000, %eax
    addl $8, %edi
    loop 6b
    movl $PAGE_TABLES, %eax
    movl %eax, %cr3
    movl %cr4, %eax
    orl $0x20, %eax                                             /* CR4.PAE */
    movl %eax, %cr4
    movl $0xc0000080, %ecx                                      /* IA32_EFER */
    rdmsr
    orl $0x100, %eax                                            /* LME: long mode enable */
    wrmsr
    movl %cr0, %eax
    orl $0x80000000, %eax                                       /* PG: with LME set, this activates long mode */
    movl %eax, %cr0
    ljmp $0x18, $long64

.code64
long64:
    movw $0x10, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    movq $STACK32, %rsp
    movl $BOOT_INFO, %edi
    movl kernel_dest, %eax
    jmp *%rax

.code16
.balign 8
gdt:
    .quad 0
    .quad 0x00cf9a000000ffff           /* 0x08: 32-bit code, base 0, limit 4 GiB */
    .quad 0x00cf92000000ffff           /* 0x10: data, base 0, limit 4 GiB */
    .quad 0x00af9a000000ffff           /* 0x18: 64-bit code (L=1) */
gdt_descriptor:
    .word gdt_descriptor - gdt - 1
    .long gdt

boot_drive:       .byte 0
a20_saved_low:    .byte 0
a20_saved_high:   .byte 0
.balign 4
e820_count:       .long 0
remaining_sectors:.long 0
this_chunk:       .long 0
dest_cursor:      .long 0
.balign 4
dap:
    .byte 0x10, 0
dap_count:        .word 0
    .word 0, LOW_BUFFER_SEGMENT        /* offset, segment */
dap_lba:          .quad 0

m_banner:    .asciz "Fosfor stage 2 (BIOS path spike): real mode, 16-bit.\r\n"
m_a20:       .asciz "  A20 line: "
m_e820:      .asciz "  Memory map (INT 15h E820): "
m_load:      .asciz "  Reading the kernel (INT 13h EDD) and copying it above 1 MiB (unreal mode): "
m_long:      .asciz "  Entering long mode (PAE, EFER.LME, paging) and jumping to the kernel.\r\n"
m_ok:        .asciz "ok\r\n"
m_a20_fail:  .asciz "FAILED, cannot enable A20.\r\n"
m_e820_fail: .asciz "FAILED, the BIOS gave no memory map.\r\n"
m_no_kernel: .asciz "FAILED, no kernel in the image header.\r\n"
m_disk_fail: .asciz "FAILED, disk read error.\r\n"
