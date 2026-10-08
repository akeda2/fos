/*
 * Fosfor OS, BIOS boot path, stage two.
 *
 * Runs at 0x8000 in 16-bit real mode, entered from stage one (boot/bios/stage1.S) with the boot
 * drive in DL. It is the BIOS counterpart of the UEFI loader (boot/uefi/src/main.rs): it does the
 * same jobs and hands the kernel the same `BootInfo` (abi/src/lib.rs, version 5), so the kernel
 * does not know which firmware started it. In order, narrating each step to the screen and COM1:
 *
 *   1. Enable the A20 line (BIOS, then the 8042 keyboard controller, then port 0x92), verified.
 *   2. Collect the physical memory map (INT 15h E820) and find the ACPI RSDP (scan of the BIOS
 *      areas): a BIOS machine has no firmware tables to hand over, only these.
 *   3. Read the GPT and find the payload partition by its type GUID; read its slot-metadata
 *      sector and run the A/B state machine (abi::select_slot, byte for byte, with the same
 *      `running`/`crash_count`/`unclean_boots` bookkeeping), writing the metadata back.
 *   4. Read the chosen kernel slot (an ELF64 position-independent image) with EDD in 16 KiB
 *      pieces into a low buffer and copy each piece above 1 MiB ("unreal mode", see enter_unreal)
 *      to a staging area: the BIOS can only write below 1 MiB and real mode only addresses 1 MiB.
 *   4b. Video: set a VBE linear-framebuffer mode (last, so narration still works) and describe it in
 *      BootInfo like the UEFI loader describes GOP; the kernel's framebuffer console then needs no change.
 *   5. Identity-map the first 4 GiB (2 MiB pages), enter long mode (PAE, EFER.LME, paging), and
 *      in 64-bit code: place the ELF's PT_LOAD segments at kernel_base, apply its
 *      R_X86_64_RELATIVE relocations, convert the E820 map to UEFI memory descriptors, fill in
 *      BootInfo and jump to the kernel entry with a pointer to it in RDI.
 *
 * Memory used (all below the kernel at kernel_base; the kernel treats everything below 1 MiB as
 * reserved loader data because the memory map we give it says so):
 *   0x1000 BootInfo (144 bytes)   0x1100 raw E820 table   0x2000 UEFI-style memory map
 *   0x5000-0x5fff is left alone: the kernel keeps its crash record there so the next boot can report it
 *   0x7a00 real-mode stack        0x7c00 stage one        0x8000 this code and its data
 *   0x10000 read buffer (64 KiB aligned: an EDD read must not cross a 64 KiB boundary)
 *   0x20000 page tables (PML4, PDPT, four page directories)      0x90000 64-bit stack
 *   kernel_base / staging_base: chosen from the E820 map (choose_placement): the lowest usable region above 1 MiB
 *   that holds 8 MiB for the placed kernel image followed by room for the ELF as read from disk
 */
.code16
.set BOOT_INFO, 0x1000
.set E820_TABLE, 0x1100
.set E820_MAX, 64
.set MEMORY_MAP, 0x2000
.set MEMORY_MAP_STRIDE, 48              /* a UEFI-style descriptor stride; the struct itself is 40 bytes */
.set BUFFER_SEGMENT, 0x1000             /* 0x10000 */
.set CHUNK_SECTORS, 32                  /* 16 KiB per EDD read */
.set PAGE_TABLES, 0x20000
.set STACK16, 0x7a00
.set STACK64, 0x90000
.set KERNEL_SPAN, 0x800000              /* room reserved for the placed kernel image: 8 MiB (its memory size must fit) */
.set SLOT_SECTORS, 16384                /* abi::SLOT_SECTORS: 8 MiB per kernel slot */

.global _start
_start:
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

    /* ---- 1. A20 ---- */
    movw $m_a20, %si
    call puts16
    call a20_check
    jnz a20_ok
    movw $0x2401, %ax                   /* INT 15h AX=2401h: enable A20 through the BIOS */
    int $0x15
    call a20_check
    jnz a20_ok
    call a20_8042                       /* the keyboard controller's output port has an A20 bit */
    call a20_check
    jnz a20_ok
    inb $0x92, %al                      /* the "fast A20 gate": bit 1 enables; bit 0 would reset the CPU */
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

    /* ---- 2. memory map and ACPI RSDP ---- */
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
    call print_e820
    movw $m_rsdp, %si
    call puts16
    call find_rsdp
    movl rsdp_address, %eax
    testl %eax, %eax
    jnz 3f
    movw $m_rsdp_none, %si
    call puts16
    jmp 4f
3:  call puthex32
    movw $m_crlf, %si
    call puts16
4:

    /* ---- 3. GPT: find the payload partition ---- */
    movw $m_gpt, %si
    call puts16
    movl $1, dap_lba                    /* LBA 1: the GPT header */
    movw $1, dap_count
    movw $0, dap_offset
    call disk_read
    jc disk_fail_code
    movw $BUFFER_SEGMENT, %ax
    movw %ax, %es
    cmpl $0x20494645, %es:0             /* "EFI " */
    jne gpt_bad
    cmpl $0x54524150, %es:4             /* "PART" */
    jne gpt_bad
    movl %es:80, %eax                   /* number of partition entries */
    movl %eax, gpt_count
    movl %es:84, %eax                   /* size of each entry */
    movl %eax, gpt_entry_size
    movl %es:72, %eax                   /* LBA of the entry array (low 32 bits) */
    movl %eax, dap_lba
    cmpl $128, gpt_entry_size           /* what every tool writes; anything else we would have to walk differently */
    jne gpt_bad
    cmpl $128, gpt_count
    ja gpt_bad
    movw $32, dap_count                 /* 128 entries x 128 bytes = 32 sectors */
    movw $0x0200, dap_offset            /* after the header sector in the buffer */
    call disk_read
    jc disk_fail_code
    movw $BUFFER_SEGMENT, %ax
    movw %ax, %es
    movw $0x0200, %di
gpt_scan:
    cmpl $0, gpt_count
    je gpt_none
    movw $payload_type_guid, %si
    pushw %di
    movw $16, %cx
    repe cmpsb                          /* DS:SI (the wanted type GUID) against ES:DI (the entry's) */
    popw %di
    je gpt_found
    addw $128, %di
    decl gpt_count
    jmp gpt_scan
gpt_found:
    xorw %bx, %bx                       /* the partition's own unique GUID, passed on in BootInfo */
1:  movb %es:16(%bx,%di), %al
    movb %al, payload_guid(%bx)
    incw %bx
    cmpw $16, %bx
    jb 1b
    movl %es:32(%di), %eax              /* first LBA */
    movl %eax, payload_first
    cmpl $0, %es:36(%di)
    jne gpt_bad                         /* beyond 2 TiB: not supported by this loader */
    movl %es:40(%di), %eax              /* last LBA */
    subl payload_first, %eax
    incl %eax
    movl %eax, payload_sectors
    movw $m_found, %si
    call puts16
    movl payload_first, %eax
    call puthex32
    movw $m_sectors, %si
    call puts16
    movl payload_sectors, %eax
    call puthex32
    movw $m_crlf, %si
    call puts16
    jmp read_metadata
gpt_none:
    movw $m_no_payload, %si
    jmp fail
gpt_bad:
    movw $m_gpt_bad, %si
    jmp fail

    /* ---- 3b. slot metadata and the A/B state machine ---- */
read_metadata:
    movl payload_first, %eax
    movl %eax, dap_lba
    movw $1, dap_count
    movw $0, dap_offset
    call disk_read
    jc disk_fail_code
    call copy_buffer_to_meta
    cmpl $0x4d534f46, meta              /* "FOSM" */
    jne meta_bad
    cmpb $1, meta+4                     /* metadata version */
    jne meta_bad

    /* Did the previous boot end badly? running: 1 = never reached the kernel's milestone,
     * 2 = reached it but ended without a clean stop, 0 = clean. */
    movb meta+14, %al
    cmpb $1, %al
    jne 5f
    cmpb $255, meta+9
    je 4f
    incb meta+9                         /* crash_count, saturating */
4:  movw $m_crashed, %si
    call puts16
    jmp 7f
5:  cmpb $2, %al
    jne 7f
    cmpb $255, meta+15
    je 6f
    incb meta+15                        /* unclean_boots, saturating */
6:  movw $m_unclean, %si
    call puts16
7:
    /* abi::select_slot */
    movb meta+6, %al                    /* pending slot, 0xff = none */
    cmpb $0xff, %al
    jne has_pending
    movb meta+5, %al                    /* no pending slot: boot the active one */
    andb $1, %al
    jmp chosen
has_pending:
    cmpb $0, meta+7                     /* pending, not yet attempted: one try */
    jne attempted
    movb $1, meta+7
    andb $1, %al
    jmp chosen
attempted:
    cmpb $0, meta+8
    je give_up
    andb $1, %al                        /* attempted and confirmed: promote it */
    movb %al, meta+5
    movb $0xff, meta+6
    movb $0, meta+7
    movb $0, meta+8
    jmp chosen
give_up:                                /* attempted but never confirmed: back to the active slot */
    movb $0xff, meta+6
    movb $0, meta+7
    movb $0, meta+8
    movb meta+5, %al
    andb $1, %al
chosen:
    movb %al, slot
    movw $m_slot, %si
    call puts16
    movzbl slot, %eax
    call putdec32
    movw $m_crlf, %si
    call puts16
    cmpb $0, slot
    jne 1f
    movzwl meta+10, %eax
    jmp 2f
1:  movzwl meta+12, %eax
2:  movl %eax, kernel_sectors
    testl %eax, %eax
    jz slot_empty
    movw $m_kernel_slot, %si            /* "Kernel slot N: S sectors (K KiB)", as the UEFI loader says it */
    call puts16
    movzbl slot, %eax
    call putdec32
    movw $m_colon, %si
    call puts16
    movl kernel_sectors, %eax
    call putdec32
    movw $m_sectors_kib, %si
    call puts16
    movl kernel_sectors, %eax
    shrl $1, %eax
    call putdec32
    movw $m_kib_end, %si
    call puts16
    movl kernel_sectors, %eax
    cmpl $SLOT_SECTORS, %eax
    ja slot_empty                       /* more than a slot can hold: the metadata is wrong */
    movb $1, meta+14                    /* running = booting: the kernel sets it to 2 at its milestone */
    movb meta+9, %al
    movb %al, boot_crash_count
    call copy_meta_to_buffer
    movl payload_first, %eax
    movl %eax, dap_lba
    movw $1, dap_count
    movw $0, dap_offset
    call disk_write
    jc 2f
    /* Some BIOSes report a USB write as done without it taking effect (or corrupt it), and the kernel's
     * crash tracking and A/B fallback live in this sector: read it back and compare before trusting it. */
    movl payload_first, %eax
    movl %eax, dap_lba
    movw $1, dap_count
    movw $0, dap_offset
    call disk_read
    jc 2f
    movw $BUFFER_SEGMENT, %ax
    movw %ax, %es
    xorw %di, %di
    movw $meta, %si
    movw $512, %cx
    cld
    repe cmpsb                          /* DS:SI (our copy) against ES:DI (what the disk now holds) */
    xorw %ax, %ax
    movw %ax, %es
    jne 3f
    movw $m_meta_ok, %si
    call puts16
    jmp 1f
3:  movw $m_meta_mismatch, %si          /* it did not stick: say so, the disk's old content is what the kernel will see */
    call puts16
    jmp 1f
2:  movw $m_meta_ro, %si                /* read-only media: carry on, but fallback tracking is off this boot */
    call puts16
1:

    /* ---- 4. choose where the kernel goes, then read the slot above 1 MiB ---- */
    call choose_placement
    movw $m_place, %si
    call puts16
    movl kernel_base, %eax
    call puthex32
    movw $m_place_staging, %si
    call puts16
    movl staging_base, %eax
    call puthex32
    movw $m_crlf, %si
    call puts16
    movw $m_load, %si
    call puts16
    movl payload_first, %eax
    incl %eax                           /* slot 0 starts at payload sector 1 ... */
    cmpb $0, slot
    je 1f
    addl $SLOT_SECTORS, %eax            /* ... slot 1 one slot later */
1:  movl %eax, rd_lba
    movl kernel_sectors, %eax
    movl %eax, rd_sectors
    movl staging_base, %eax
    movl %eax, rd_dest
    call read_high
    jc disk_fail_code
    movw $m_ok, %si
    call puts16

    /* ---- 5. long mode ---- */
    movw $m_long, %si
    call puts16
    call vbe_setup                      /* last real-mode job: after this the screen may no longer take INT 10h text */
    cli
    lgdt gdt_descriptor
    movl %cr0, %eax
    orl $1, %eax                        /* PE: protected mode on */
    movl %eax, %cr0
    ljmpl $0x08, $protected32           /* the far jump loads CS with the 32-bit code descriptor */

meta_bad:
    movw $m_meta_bad, %si
    jmp fail
slot_empty:
    movw $m_slot_empty, %si
    jmp fail
disk_fail_code:
    movb %ah, %al                       /* INT 13h status in AH (01 bad command, 04 sector not found, ...) */
    call puthex8
    movw $m_disk_fail, %si
    jmp fail
fail:
    call puts16
    cli
1:  hlt
    jmp 1b

/* ---- disk: one EDD call on the DAP below; CF set and AH = status on error ---- */
disk_read:
    movb $0x42, %ah                     /* INT 13h AH=42h: extended read */
    jmp disk_call
disk_write:
    movb $0x43, %ah                     /* INT 13h AH=43h: extended write, AL=0: no verify */
    xorb %al, %al
disk_call:
    movb boot_drive, %dl
    movw $dap, %si
    sti                                 /* some BIOSes wait for a disk interrupt inside INT 13h */
    int $0x13
    pushf                               /* keep the call's carry flag (error) through the check below */
    cli                                 /* and nothing may interrupt us while ES holds its 4 GiB limit (enter_unreal) */
    testb $3, 0x417                     /* BIOS data area: a Shift key is down? interrupts were on, so it is current */
    jz 1f
    movb $1, shift_seen                 /* remembered: the video step is later, and a key held at power-on may be lost */
1:  popf
    ret

/* Reads rd_sectors from rd_lba and copies them to the linear address rd_dest (above 1 MiB), piece by piece.
 * The buffer, destination and count live in memory because the BIOS may clobber the upper halves of 32-bit
 * registers. CF set and AH = status on a disk error. */
read_high:
    movl rd_lba, %eax
    movl %eax, dap_lba
    movw $0, dap_offset
rh_loop:
    movl rd_sectors, %eax
    testl %eax, %eax
    jz rh_done
    cmpl $CHUNK_SECTORS, %eax
    jbe 1f
    movl $CHUNK_SECTORS, %eax
1:  movl %eax, rh_chunk
    movw %ax, dap_count
    call disk_read
    jc rh_error
    call enter_unreal                   /* the BIOS may have reset ES's limit; get it back */
    pushw %ds
    movw $BUFFER_SEGMENT, %ax
    movw %ax, %ds                       /* source DS:ESI = the 0x10000 buffer */
    xorl %esi, %esi
    movl %cs:rd_dest, %edi              /* our own variables through CS, since DS now points at the buffer */
    movl %cs:rh_chunk, %ecx             /* sectors -> 32-bit words: x 512 / 4 */
    shll $7, %ecx
    addr32 rep movsl %ds:(%esi), %es:(%edi)
    popw %ds
    movl rh_chunk, %eax
    movl %eax, %ecx
    shll $9, %ecx
    addl %ecx, rd_dest
    addl %eax, dap_lba
    subl %eax, rd_sectors
    jmp rh_loop
rh_done:
    clc
    ret
rh_error:
    stc
    ret

/* ---- where the kernel goes: from the E820 map, not a fixed address ----
 * The kernel image needs KERNEL_SPAN bytes (8 MiB; the real image is about 2 MiB, its .bss included) and the ELF as read
 * from disk needs the slot's size again (up to 8 MiB) as a staging area, both below 4 GiB (the copy and the page tables
 * here reach that far). Take the lowest usable E820 region above 1 MiB that can hold both back to back, starting on a
 * 2 MiB boundary: kernel_base = that start, staging_base = kernel_base + KERNEL_SPAN. Fails with a message if there is none
 * (a machine with a memory hole right above 1 MiB needs a region of about this size somewhere). */
choose_placement:
    movl kernel_sectors, %eax
    shll $9, %eax                       /* bytes of the slot image ... */
    addl $0xfffff, %eax
    andl $0xfff00000, %eax              /* ... rounded up to 1 MiB */
    addl $KERNEL_SPAN, %eax
    movl %eax, place_need
    movw $E820_TABLE, %bx
    movl e820_count, %eax
    movl %eax, e820_left
1:  cmpl $0, e820_left
    je place_fail
    cmpl $1, 16(%bx)                    /* usable RAM only */
    jne 5f
    cmpl $0, 4(%bx)                     /* base above 4 GiB: out of reach */
    jne 5f
    movl (%bx), %eax                    /* base */
    movl 8(%bx), %edx
    addl %eax, %edx                     /* end = base + length, low dword; carry into the high one */
    movl 12(%bx), %ecx
    adcl $0, %ecx
    jz 3f
    movl $0xfffff000, %edx              /* the region reaches past 4 GiB: use it up to there */
3:  cmpl $0x100000, %eax
    jae 4f
    movl $0x100000, %eax                /* nothing below 1 MiB: BIOS data, stage two and the page tables live there */
4:  addl $0x1fffff, %eax
    jc 5f
    andl $0xffe00000, %eax              /* start on a 2 MiB boundary */
    cmpl %eax, %edx
    jbe 5f
    subl %eax, %edx                     /* how much is there from that start */
    cmpl place_need, %edx
    jb 5f
    movl %eax, kernel_base
    addl $KERNEL_SPAN, %eax
    movl %eax, staging_base
    ret
5:  addw $24, %bx
    decl e820_left
    jmp 1b
place_fail:
    movw $m_no_room, %si
    jmp fail

/* ---- unreal mode: ES gets a flat 4 GiB limit, then we return to real mode (the limit stays cached) ---- */
enter_unreal:
    cli
    pushw %ds
    lgdt gdt_descriptor
    movl %cr0, %eax
    orl $1, %eax
    movl %eax, %cr0
    movw $0x10, %bx
    movw %bx, %es                       /* descriptor 0x10: base 0, limit 4 GiB, data */
    andl $0xfffffffe, %eax
    movl %eax, %cr0                     /* back to real mode; ES keeps its 4 GiB limit */
    popw %ds
    ret                                 /* interrupts stay off: a BIOS interrupt handler reloading ES would reset the limit */

/* The 512-byte metadata sector between the read buffer and our own copy (meta). */
copy_buffer_to_meta:
    pushw %ds
    pushw %es
    xorw %ax, %ax
    movw %ax, %es
    movw $BUFFER_SEGMENT, %ax
    movw %ax, %ds
    xorw %si, %si
    movw $meta, %di
    movw $512, %cx
    cld
    rep movsb
    popw %es
    popw %ds
    ret
copy_meta_to_buffer:
    pushw %ds
    pushw %es
    movw $BUFFER_SEGMENT, %ax
    movw %ax, %es
    xorw %di, %di
    movw $meta, %si
    movw $512, %cx
    cld
    rep movsb
    popw %es
    popw %ds
    ret

/* ---- A20: does 0000:0500 alias to FFFF:0510? ZF clear = A20 is on ---- */
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
    cmpb $0xff, (%si)                   /* equal: the write through FFFF:0510 landed on 0000:0500 (A20 off) */
    movb a20_saved_low, %al
    movb %al, (%si)
    movb a20_saved_high, %al
    movb %al, %es:(%di)
    popw %di
    popw %si
    popw %es
    popw %ds
    ret

/* The 8042 keyboard controller's output port (command 0xD0 reads it, 0xD1 writes it) has A20 as bit 1. */
a20_8042:
    cli
    call wait_8042_input
    movb $0xad, %al                     /* disable the keyboard while we talk to the controller */
    outb %al, $0x64
    call wait_8042_input
    movb $0xd0, %al                     /* read the output port */
    outb %al, $0x64
    call wait_8042_output
    inb $0x60, %al
    pushw %ax
    call wait_8042_input
    movb $0xd1, %al                     /* write the output port */
    outb %al, $0x64
    call wait_8042_input
    popw %ax
    orb $2, %al
    outb %al, $0x60
    call wait_8042_input
    movb $0xae, %al                     /* re-enable the keyboard */
    outb %al, $0x64
    call wait_8042_input
    sti
    ret
wait_8042_input:                        /* bit 1 of the status port: input buffer full */
    movw $0xffff, %cx
1:  inb $0x64, %al
    testb $2, %al
    jz 2f
    loop 1b
2:  ret
wait_8042_output:                       /* bit 0: output buffer full */
    movw $0xffff, %cx
1:  inb $0x64, %al
    testb $1, %al
    jnz 2f
    loop 1b
2:  ret

/* ---- E820: fill E820_TABLE, set e820_count; CF set only if the BIOS gave no entry at all ---- */
collect_e820:
    xorl %ebx, %ebx
    movw $E820_TABLE, %di
    movl $0, e820_count
1:  movl $1, 20(%di)                    /* ask for the ACPI extended attributes word */
    movl $0xe820, %eax
    movl $24, %ecx
    movl $0x534d4150, %edx              /* 'SMAP' */
    int $0x15
    jc e820_error
    cmpl $0x534d4150, %eax
    jne e820_error
    cmpl $0, 8(%di)                     /* skip zero-length entries */
    jne 5f
    cmpl $0, 12(%di)
    je 6f
5:  incl e820_count
    addw $24, %di
6:  testl %ebx, %ebx
    jz 7f
    cmpl $E820_MAX, e820_count
    jb 1b
7:  clc
    ret
e820_error:
    cmpl $0, e820_count                 /* an error after at least one entry just ends the list */
    jne 7b
    stc
    ret

/* One line per E820 entry: base, length, type (1 usable, 2 reserved, 3 ACPI reclaimable, 4 ACPI NVS, 5 bad). */
print_e820:
    movw $E820_TABLE, %bx
    movl e820_count, %ecx
    movl %ecx, e820_left
1:  cmpl $0, e820_left
    je 2f
    movw $m_e820_line, %si
    call puts16
    movl 4(%bx), %eax                   /* base, high then low dword */
    call puthex32
    movl (%bx), %eax
    call puthex32
    movw $m_e820_len, %si
    call puts16
    movl 12(%bx), %eax                  /* length */
    call puthex32
    movl 8(%bx), %eax
    call puthex32
    movw $m_e820_type, %si
    call puts16
    movl 16(%bx), %eax
    call puthex8
    movw $m_crlf, %si
    call puts16
    addw $24, %bx
    decl e820_left
    jmp 1b
2:  ret

/* ---- ACPI RSDP: "RSD PTR " on a 16-byte boundary in the first KiB of the EBDA or in 0xE0000-0xFFFFF,
 * with the first 20 bytes summing to zero. rsdp_address stays 0 if there is none. ---- */
find_rsdp:
    movl $0, rsdp_address
    xorw %ax, %ax
    movw %ax, %es
    movw %es:0x40e, %ax                 /* BIOS data area: segment of the EBDA */
    testw %ax, %ax
    jz 1f
    movw $64, %cx                       /* 64 paragraphs = 1 KiB */
    call rsdp_scan
    cmpl $0, rsdp_address
    jne 2f
1:  movw $0xe000, %ax
    movw $0x2000, %cx                   /* 0xE0000-0xFFFFF: 8192 paragraphs */
    call rsdp_scan
2:  xorw %ax, %ax
    movw %ax, %es
    ret
/* AX = first paragraph (segment), CX = how many; sets rsdp_address and returns on a match */
rsdp_scan:
    movw %ax, %es
    xorw %di, %di
3:  cmpl $0x20445352, %es:(%di)         /* "RSD " */
    jne 4f
    cmpl $0x20525450, %es:4(%di)        /* "PTR " */
    jne 4f
    xorb %dl, %dl                       /* checksum of the first 20 bytes */
    xorw %bx, %bx
5:  addb %es:(%bx,%di), %dl
    incw %bx
    cmpw $20, %bx
    jb 5b
    testb %dl, %dl
    jnz 4f
    /* Found. The RSDP sits in BIOS ROM space (0xE0000-0xFFFFF), which E820 reports as reserved, so the kernel
     * would not map it. Copy the 36 bytes (the ACPI 2.0 size; an older table just has trailing junk the kernel
     * ignores) into our own data below 1 MiB instead, which the memory map we hand over does cover. */
    xorw %bx, %bx
6:  movb %es:(%bx,%di), %al
    movb %al, rsdp_copy(%bx)
    incw %bx
    cmpw $36, %bx
    jb 6b
    movl $rsdp_copy, rsdp_address
    ret
4:  movw %es, %ax
    incw %ax                            /* next paragraph */
    movw %ax, %es
    loop 3b
    ret

/* ---- The monitor's native size, from its EDID ----
 * A monitor describes itself in a 128-byte EDID block, read over the display cable's DDC channel; the video BIOS offers it
 * through VBE function 15h (INT 10h AX=4F15h, BL=01h: read EDID block DX of controller CX into ES:DI). The block starts with
 * 00 FF FF FF FF FF FF 00, sums to zero modulo 256, and its first 18-byte "detailed timing descriptor" (offset 54) is the
 * monitor's preferred -- native -- mode: horizontal active pixels are byte 56 plus the high nibble of byte 58 (as bits 8-11),
 * vertical active lines are byte 59 plus the high nibble of byte 61. Anything unexpected leaves native_width at 0, which
 * means "unknown". Buffer: 0x3400, 128 bytes. Returns with ES = 0. */
read_edid:
    xorw %ax, %ax
    movw %ax, %es
    movw $0x3400, %di
    movw $0x4f15, %ax
    movw $0x0001, %bx                   /* BL=1: read EDID (BL=0 would only report DDC capabilities) */
    xorw %cx, %cx                       /* controller 0 */
    xorw %dx, %dx                       /* EDID block 0 */
    sti
    int $0x10
    cli
    cmpw $0x004f, %ax
    jne 9f
    xorw %ax, %ax
    movw %ax, %es
    cmpl $0xffffff00, %es:0x3400        /* 00 FF FF FF */
    jne 9f
    cmpl $0x00ffffff, %es:0x3404        /* FF FF FF 00 */
    jne 9f
    xorw %si, %si
    xorb %dl, %dl
    movw $128, %cx
1:  addb %es:0x3400(%si), %dl
    incw %si
    loop 1b
    testb %dl, %dl
    jnz 9f                              /* the checksum is wrong: do not trust it */
    cmpw $0, %es:0x3436                 /* a detailed timing descriptor starts with a non-zero pixel clock */
    je 9f
    movzbl %es:0x3438, %eax             /* horizontal active, low 8 bits */
    movzbl %es:0x343a, %edx
    andl $0xf0, %edx
    shll $4, %edx                       /* high nibble of byte 58 -> bits 8-11 */
    orl %edx, %eax
    movl %eax, %ebx
    movzbl %es:0x343b, %eax             /* vertical active, low 8 bits */
    movzbl %es:0x343d, %edx
    andl $0xf0, %edx
    shll $4, %edx
    orl %edx, %eax
    cmpl $640, %ebx                     /* sanity: a real monitor */
    jb 9f
    cmpl $8192, %ebx
    ja 9f
    cmpl $480, %eax
    jb 9f
    cmpl $8192, %eax
    ja 9f
    movl %ebx, native_width
    movl %eax, native_height
    movw $m_edid, %si                   /* narrate while text output still works */
    call puts16
    movl native_width, %eax
    call putdec32
    movw $m_by, %si
    call puts16
    movl native_height, %eax
    call putdec32
    movw $m_crlf, %si
    call puts16
9:  xorw %ax, %ax
    movw %ax, %es
    ret

/* ---- Video: VBE (VESA BIOS Extensions) linear framebuffer ----
 * The UEFI loader passes the firmware's graphics framebuffer; a BIOS machine's equivalent is VBE. Ask the video BIOS
 * for its list of modes (INT 10h AX=4F00h), look at each (AX=4F01h) and keep the largest mode that is a 32-bit-per-pixel
 * direct-colour mode with a linear framebuffer (attribute bits 0 supported, 3 colour, 4 graphics, 7 linear), whose
 * colour layout is one the kernel's console draws (blue in the low byte = BGR, or red in the low byte = RGB), and no
 * larger than 1280x1024. Then set it (AX=4F02h, bit 14 = use the linear framebuffer) and describe it in BootInfo
 * exactly as the UEFI loader describes GOP. This is the last real-mode step, because many video BIOSes stop honouring
 * text output in a graphics mode. If anything is missing the screen stays in text mode and the kernel is serial-only.
 * Buffers: 0x3000 controller information (512 bytes), 0x3200 mode information (256 bytes). */
vbe_setup:
    movl $0, vbe_area
    movl $0, fb_base
    movl $0, native_width
    movl $0, native_height
    /* The operator's way out of a video BIOS that hangs: either the slot-metadata flag (`debug video skip` in the
     * shell, byte 16 bit 0) or a Shift key held down right now. The BIOS keeps the keyboard's shift state in its data
     * area (0x417: bit 0 right Shift, bit 1 left Shift), but only updates it when it handles the key interrupt, so let
     * interrupts through for a moment first. */
    sti
    movw $0xffff, %cx
1:  inb $0x80, %al                      /* ~1 us per read on the ISA bus: about 65 ms for pending key events to be handled */
    loop 1b
    cli
    xorw %ax, %ax
    movw %ax, %es
    testb $3, %es:0x417
    jnz vbe_skipped
    cmpb $0, shift_seen                 /* or it was seen at any disk call since this program started */
    jne vbe_skipped
    testb $1, meta+16
    jnz vbe_skipped
    call read_edid                      /* the monitor's own idea of its native size, if it will tell (ES is 0 again after) */
    movl $0x32454256, %es:0x3000        /* "VBE2" in the signature field asks for VBE 2.0+ information */
    movw $0x3000, %di
    movw $0x4f00, %ax                   /* VBE function 00h: controller information */
    sti
    int $0x10
    cli
    cmpw $0x004f, %ax                   /* AL = 4Fh: function supported, AH = 0: succeeded */
    jne vbe_none
    xorw %ax, %ax
    movw %ax, %es
    cmpl $0x41534556, %es:0x3000        /* "VESA" */
    jne vbe_none
    movw %es:0x3004, %ax                /* VbeVersion, BCD: 0x0200 = 2.0 */
    movw %ax, vbe_version
    cmpw $0x0200, %ax                   /* linear framebuffers are a VBE 2.0 feature */
    jb vbe_none
    movw %es:0x300e, %ax                /* VideoModePtr: a far pointer (offset, then segment) to a list ending in 0xFFFF */
    movw %ax, vbe_list_offset
    movw %es:0x3010, %ax
    movw %ax, vbe_list_segment
vbe_next:
    movw vbe_list_segment, %fs          /* reloaded each time: the BIOS call below need not keep it */
    movw vbe_list_offset, %bx
    movw %fs:(%bx), %cx
    cmpw $0xffff, %cx
    je vbe_listed
    addw $2, vbe_list_offset
    movw %cx, vbe_candidate
    xorw %ax, %ax
    movw %ax, %es
    movw $0x3200, %di
    movw $0x4f01, %ax                   /* VBE function 01h: information about mode CX */
    sti
    int $0x10
    cli
    cmpw $0x004f, %ax
    jne vbe_next
    xorw %ax, %ax
    movw %ax, %es
    movw %es:0x3200, %ax                /* ModeAttributes */
    andw $0x0099, %ax
    cmpw $0x0099, %ax                   /* supported, colour, graphics, linear framebuffer available */
    jne vbe_next
    cmpb $32, %es:0x3219                /* BitsPerPixel */
    jne vbe_next
    cmpb $6, %es:0x321b                 /* MemoryModel 6 = direct colour */
    jne vbe_next
    movb %es:0x3220, %al                /* RedFieldPosition */
    movb %es:0x3224, %ah                /* BlueFieldPosition */
    cmpb $16, %al
    jne 1f
    testb %ah, %ah                      /* red at bit 16, blue at bit 0: bytes B, G, R in memory = UEFI's BGR (1) */
    jnz vbe_next
    movl $1, %edx
    jmp 2f
1:  testb %al, %al                      /* red at bit 0, blue at bit 16: UEFI's RGB (0) */
    jnz vbe_next
    cmpb $16, %ah
    jne vbe_next
    xorl %edx, %edx
2:  movzwl %es:0x3212, %eax             /* XResolution */
    cmpl $640, %eax
    jb vbe_next
    movzwl %es:0x3214, %ecx             /* YResolution */
    movl %eax, %ebx                     /* ebx = width, ecx = height, from here on */
    cmpl $0, native_width
    jne 3f
    cmpl $1280, %ebx                    /* no EDID: the old, safe rule -- the largest mode up to 1280x1024, since a mode */
    ja vbe_next                         /* the monitor cannot show would be worse than a scaled one */
    cmpl $1024, %ecx
    ja vbe_next
    imull %ecx, %eax                    /* score = area */
    jmp 5f
3:  cmpl native_width, %ebx             /* with the EDID: never larger than the monitor ... */
    ja vbe_next
    cmpl native_height, %ecx
    ja vbe_next
    imull %ecx, %eax                    /* ... and score = area ... */
    movl %ebx, %esi
    imull native_height, %esi           /* w * native height */
    movl %ecx, %edi
    imull native_width, %edi            /* h * native width: equal when the shapes (aspect ratios) are equal */
    subl %edi, %esi
    jns 4f
    negl %esi
4:  imull $50, %esi, %esi               /* |difference| * 50 <= h * native width means the shapes agree within 2% */
    cmpl %edi, %esi
    jbe 5f
    shrl $2, %eax                       /* ... but a quarter of it if the shape differs: a monitor stretches a mode of the */
                                        /* wrong shape (or leaves bars), which is what made the text look blurry */
5:  cmpl vbe_area, %eax
    jbe vbe_next
    movl %eax, vbe_area
    movw vbe_candidate, %ax
    movw %ax, vbe_mode
    movl %ebx, fb_width
    movl %ecx, fb_height
    movl %edx, fb_format
    movzwl %es:0x3210, %eax             /* BytesPerScanLine */
    movl %eax, fb_pitch
    movl %es:0x3228, %eax               /* PhysBasePtr: where the linear framebuffer is */
    movl %eax, fb_base
    jmp vbe_next
vbe_listed:
    cmpl $0, vbe_area
    je vbe_none
    movw $m_video, %si                  /* narrate while text output still works */
    call puts16
    movzwl vbe_version, %eax
    call puthex16
    movw $m_video_mode, %si
    call puts16
    movzwl vbe_mode, %eax
    call puthex16
    movw $m_colon, %si
    call puts16
    movl fb_width, %eax
    call putdec32
    movw $m_by, %si
    call puts16
    movl fb_height, %eax
    call putdec32
    movw $m_video_at, %si
    call puts16
    movl fb_base, %eax
    call puthex32
    movw $m_crlf, %si
    call puts16
    movw vbe_mode, %bx
    orw $0x4000, %bx                    /* bit 14: use the linear framebuffer */
    movw $0x4f02, %ax                   /* VBE function 02h: set mode */
    sti
    int $0x10
    cli
    cmpw $0x004f, %ax
    jne vbe_failed
    movl fb_pitch, %eax                 /* the size, and the stride in pixels (4 bytes each), as BootInfo wants them */
    mull fb_height
    movl %eax, fb_size
    movl fb_pitch, %eax
    shrl $2, %eax
    movl %eax, fb_stride
    ret
vbe_skipped:
    movl $0, fb_base
    movw $m_video_skipped, %si
    call puts16
    ret
vbe_failed:
    movl $0, fb_base
    movw $m_video_failed, %si
    call puts16
    ret
vbe_none:
    movl $0, fb_base
    movw $m_video_none, %si
    call puts16
    ret

/* ---- console: every character to COM1 and the screen (BIOS teletype) ---- */
serial_init:
    movw $0x3f9, %dx
    xorb %al, %al
    outb %al, %dx                       /* no interrupts */
    movw $0x3fb, %dx
    movb $0x80, %al
    outb %al, %dx                       /* DLAB on: divisor latch */
    movw $0x3f8, %dx
    movb $1, %al                        /* divisor 1 = 115200 baud */
    outb %al, %dx
    movw $0x3f9, %dx
    xorb %al, %al
    outb %al, %dx
    movw $0x3fb, %dx
    movb $0x03, %al                     /* 8 data bits, no parity, 1 stop, DLAB off */
    outb %al, %dx
    movw $0x3fa, %dx
    xorb %al, %al                       /* FIFOs off: one byte at a time, waiting for the transmitter each time */
    outb %al, %dx
    ret

/* EAX as an unsigned decimal number */
putdec32:
    pushl %ebx
    pushl %ecx
    pushl %edx
    xorw %cx, %cx                       /* digits so far, kept on the stack until all are known */
    movl $10, %ebx
1:  xorl %edx, %edx
    divl %ebx
    pushw %dx
    incw %cx
    testl %eax, %eax
    jnz 1b
2:  popw %ax
    addb $'0', %al
    pushw %cx
    call putc16
    popw %cx
    loop 2b
    popl %edx
    popl %ecx
    popl %ebx
    ret

puthex32:                               /* EAX as eight hex digits */
    pushl %eax
    shrl $16, %eax
    call puthex16
    popl %eax
puthex16:
    pushw %ax
    movb %ah, %al
    call puthex8
    popw %ax
puthex8:                                /* AL as two hex digits */
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
    andb $0x60, %al                     /* bit 5: holding register empty, bit 6: shifter empty too */
    cmpb $0x60, %al
    jne 2b
    movw $0x3f8, %dx
    movb %ah, %al
    outb %al, %dx
    popw %dx
    popw %ax
    pushw %ax
    pushw %bx
    movb $0x0e, %ah                     /* INT 10h AH=0Eh: teletype output to the screen */
    movw $0x0007, %bx
    int $0x10
    popw %bx
    popw %ax
    ret

/* ================= 32-bit protected mode, then long mode ================= */
.code32
protected32:
    movw $0x10, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    movl $STACK64, %esp
    movl $PAGE_TABLES, %edi             /* zero the six tables (PML4, PDPT, 4 x PD) */
    xorl %eax, %eax
    movl $(6 * 4096 / 4), %ecx
    rep stosl
    movl $(PAGE_TABLES + 0x1000 + 3), PAGE_TABLES               /* PML4[0] -> PDPT, present + writable */
    movl $PAGE_TABLES + 0x1000, %edi
    movl $(PAGE_TABLES + 0x2000 + 3), %eax
    movl $4, %ecx
5:  movl %eax, (%edi)                                            /* PDPT[0..3] -> the four PDs */
    addl $0x1000, %eax
    addl $8, %edi
    loop 5b
    movl $(PAGE_TABLES + 0x2000), %edi
    movl $0x83, %eax                                            /* 2 MiB page, present + writable + PS */
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
    orl $0x80000000, %eax                                       /* PG: with LME set, this switches long mode on */
    movl %eax, %cr0
    ljmp $0x18, $long64

/* ================= 64-bit: place the kernel, build BootInfo, jump ================= */
.code64
long64:
    movw $0x10, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    movq $STACK64, %rsp
    movl kernel_base, %ebx              /* rbx = where the image goes, for the whole ELF phase */
    movl staging_base, %ebp             /* rbp = the ELF as read from disk */

    cmpl $0x464c457f, (%rbp)            /* "\x7fELF" */
    jne elf_bad
    cmpw $3, 16(%rbp)                   /* e_type ET_DYN: a position-independent image */
    jne elf_bad
    movq 0x18(%rbp), %r15               /* e_entry */
    movq 0x20(%rbp), %r14
    addq %rbp, %r14                     /* first program header */
    movzwl 0x38(%rbp), %r13d            /* e_phnum */
    movzwl 0x36(%rbp), %r12d            /* e_phentsize */
    xorq %r11, %r11                     /* highest end address of any segment */
    xorq %r10, %r10                     /* virtual address of PT_DYNAMIC */
ph_loop:
    testl %r13d, %r13d
    jz ph_done
    movl (%r14), %eax                   /* p_type */
    cmpl $2, %eax
    je ph_dynamic
    cmpl $1, %eax                       /* PT_LOAD */
    jne ph_next
    movq 0x08(%r14), %rsi               /* p_offset */
    addq %rbp, %rsi
    movq 0x10(%r14), %rdi               /* p_vaddr */
    addq %rbx, %rdi
    movq 0x20(%r14), %rcx               /* p_filesz */
    cld
    rep movsb
    movq 0x28(%r14), %rcx               /* p_memsz: the rest (.bss) is zeroed */
    subq 0x20(%r14), %rcx
    xorl %eax, %eax
    rep stosb
    movq 0x10(%r14), %rax
    addq 0x28(%r14), %rax
    cmpq %r11, %rax
    jbe ph_next
    movq %rax, %r11
    jmp ph_next
ph_dynamic:
    movq 0x10(%r14), %r10
ph_next:
    addq %r12, %r14
    decl %r13d
    jmp ph_loop
ph_done:
    cmpq $KERNEL_SPAN, %r11             /* the placed image (segments plus .bss) must fit the room reserved for it */
    ja image_too_big
    addq $0xfff, %r11
    andq $-4096, %r11                   /* kernel_size: the image, rounded up to a page */
    movq %r11, kernel_size

    /* Relocations: PT_DYNAMIC lists DT_RELA (7, address) and DT_RELASZ (8, bytes); each entry is
     * {offset, info, addend} and the only kind a position-independent kernel needs is
     * R_X86_64_RELATIVE (8): *(base + offset) = base + addend. */
    xorq %r8, %r8                       /* rela table */
    xorq %r9, %r9                       /* its size */
    testq %r10, %r10
    jz reloc_done
    addq %rbx, %r10
dyn_loop:
    movq (%r10), %rax
    testq %rax, %rax
    jz dyn_done
    cmpq $7, %rax
    jne 1f
    movq 8(%r10), %r8
    addq %rbx, %r8
1:  cmpq $8, %rax
    jne 2f
    movq 8(%r10), %r9
2:  addq $16, %r10
    jmp dyn_loop
dyn_done:
rela_loop:
    cmpq $24, %r9
    jb reloc_done
    cmpl $8, 8(%r8)                     /* r_info type */
    jne reloc_bad
    movq (%r8), %rax                    /* r_offset */
    movq 16(%r8), %rdx                  /* r_addend */
    addq %rbx, %rdx
    movq %rdx, (%rbx,%rax)
    addq $24, %r8
    subq $24, %r9
    jmp rela_loop
reloc_done:
    addq %rbx, %r15                     /* the entry point */

    /* The memory map: E820 -> UEFI-style descriptors (kind, padding, physical start, virtual start, pages,
     * attributes). Usable RAM below 1 MiB becomes "loader data" (type 2): it holds stage two, the page tables
     * and BootInfo, and the kernel only ever reuses types 3, 4 and 7, so it will never hand those pages out. */
    movq $E820_TABLE, %rsi
    movq $MEMORY_MAP, %rdi
    movl e820_count, %ecx
    xorl %r8d, %r8d                     /* descriptors written */
mm_loop:
    testl %ecx, %ecx
    jz mm_done
    movq (%rsi), %r10                   /* base */
    movq 8(%rsi), %r11
    addq %r10, %r11                     /* end */
    movl 16(%rsi), %eax                 /* E820 type */
    cmpl $1, %eax
    je mm_usable
    movl $0, %r9d                       /* reserved */
    cmpl $3, %eax
    jne 1f
    movl $9, %r9d                       /* ACPI reclaimable */
1:  cmpl $4, %eax
    jne 2f
    movl $10, %r9d                      /* ACPI NVS */
2:  cmpl $5, %eax
    jne 3f
    movl $8, %r9d                       /* bad memory: "unusable" */
3:  andq $-4096, %r10                   /* reserved regions round outwards */
    addq $0xfff, %r11
    andq $-4096, %r11
    call emit
    jmp mm_next
mm_usable:
    addq $0xfff, %r10                   /* free RAM rounds inwards */
    andq $-4096, %r10
    andq $-4096, %r11
    cmpq $0x100000, %r10
    jae mm_high
    cmpq $0x100000, %r11
    ja mm_split
    movl $2, %r9d                       /* wholly below 1 MiB */
    call emit
    jmp mm_next
mm_split:
    movq %r11, %rdx                     /* straddles 1 MiB: the low part is loader data, the rest free */
    movq $0x100000, %r11
    movl $2, %r9d
    call emit
    movq $0x100000, %r10
    movq %rdx, %r11
mm_high:
    movl $7, %r9d                       /* conventional memory */
    call emit
mm_next:
    addq $24, %rsi
    decl %ecx
    jmp mm_loop
mm_done:

    /* BootInfo (abi/src/lib.rs, version 5, 144 bytes; the offsets are pinned by a unit test in abi). */
    movq $BOOT_INFO, %rdx
    movq $0, %rax
    movq $18, %rcx                      /* 144 / 8 */
    movq %rdx, %rdi
    rep stosq
    movq magic_value, %rax
    movq %rax, 0(%rdx)                  /* magic "FOSV2BI\0" */
    movl $5, 8(%rdx)                    /* version */
    movl $144, 12(%rdx)                 /* size */
    movq $MEMORY_MAP, 16(%rdx)
    movq %r8, 24(%rdx)                  /* memory_map_count */
    movq $MEMORY_MAP_STRIDE, 32(%rdx)   /* memory_map_descriptor_size */
    movl fb_base, %eax                  /* framebuffer, as the UEFI loader fills it (zero base = none: text mode) */
    movq %rax, 40(%rdx)
    movl fb_size, %eax
    movq %rax, 48(%rdx)
    movl fb_width, %eax
    movl %eax, 56(%rdx)
    movl fb_height, %eax
    movl %eax, 60(%rdx)
    movl fb_stride, %eax
    movl %eax, 64(%rdx)
    movl fb_format, %eax
    movl %eax, 68(%rdx)
    movl rsdp_address, %eax
    movq %rax, 72(%rdx)                 /* acpi_rsdp */
    /* runtime_services at 80 stays zero: there is no firmware to call after boot */
    movq %rbx, 88(%rdx)                 /* kernel_base */
    movq kernel_size, %rax
    movq %rax, 96(%rdx)                 /* kernel_size */
    movzbl slot, %eax
    movl %eax, 104(%rdx)                /* boot_slot */
    movzbl boot_crash_count, %eax
    movl %eax, 108(%rdx)                /* crash_count */
    movq payload_guid, %rax
    movq %rax, 112(%rdx)
    movq payload_guid + 8, %rax
    movq %rax, 120(%rdx)                /* payload_guid */
    movl payload_first, %eax
    movq %rax, 128(%rdx)                /* payload_start */
    movl payload_sectors, %eax
    movq %rax, 136(%rdx)                /* payload_sectors */

    movq %rdx, %rdi                     /* System V: the first argument is the BootInfo pointer */
    jmp *%r15

/* Appends one descriptor {kind r9d, [r10, r11)} at rdi unless it is empty; counts it in r8. */
emit:
    cmpq %r10, %r11
    jbe 1f
    movl %r9d, (%rdi)
    movl $0, 4(%rdi)
    movq %r10, 8(%rdi)
    movq $0, 16(%rdi)
    movq %r11, %rax
    subq %r10, %rax
    shrq $12, %rax
    movq %rax, 24(%rdi)                 /* pages */
    movq $0, 32(%rdi)
    movq $0, 40(%rdi)
    addq $MEMORY_MAP_STRIDE, %rdi
    incq %r8
1:  ret

elf_bad:
    leaq m_elf_bad(%rip), %rsi
    jmp fail64
image_too_big:
    leaq m_image_big(%rip), %rsi
    jmp fail64
reloc_bad:
    leaq m_reloc_bad(%rip), %rsi
fail64:
    lodsb
    testb %al, %al
    jz 3f
    movb %al, %ah
2:  movw $0x3fd, %dx
    inb %dx, %al
    andb $0x60, %al
    cmpb $0x60, %al
    jne 2b
    movw $0x3f8, %dx
    movb %ah, %al
    outb %al, %dx
    jmp fail64
3:  cli
4:  hlt
    jmp 4b

/* ================= data ================= */
.code16
.balign 8
gdt:
    .quad 0
    .quad 0x00cf9a000000ffff            /* 0x08: 32-bit code, base 0, limit 4 GiB */
    .quad 0x00cf92000000ffff            /* 0x10: data, base 0, limit 4 GiB */
    .quad 0x00af9a000000ffff            /* 0x18: 64-bit code (L=1) */
gdt_descriptor:
    .word gdt_descriptor - gdt - 1
    .long gdt

/* GPT type GUID of the payload partition (abi::PAYLOAD_TYPE_GUID), in GPT's on-disk byte order:
 * f05b2a01-0000-4f05-8000-0000000000a1 */
payload_type_guid:
    .byte 0x01, 0x2a, 0x5b, 0xf0, 0x00, 0x00, 0x05, 0x4f, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xa1
magic_value:      .ascii "FOSV2BI\0"

boot_drive:       .byte 0
shift_seen:       .byte 0
slot:             .byte 0
boot_crash_count: .byte 0
a20_saved_low:    .byte 0
a20_saved_high:   .byte 0
.balign 4
e820_count:       .long 0
e820_left:        .long 0
rsdp_address:     .long 0
gpt_count:        .long 0
gpt_entry_size:   .long 0
payload_first:    .long 0
payload_sectors:  .long 0
kernel_sectors:   .long 0
kernel_base:      .long 0
staging_base:     .long 0
place_need:       .long 0
rd_lba:           .long 0
rd_sectors:       .long 0
rd_dest:          .long 0
rh_chunk:         .long 0
.balign 8
kernel_size:      .quad 0
vbe_area:         .long 0
native_width:     .long 0
native_height:    .long 0
vbe_version:      .word 0
vbe_mode:         .word 0
vbe_candidate:    .word 0
vbe_list_offset:  .word 0
vbe_list_segment: .word 0
.balign 4
fb_base:          .long 0
fb_size:          .long 0
fb_width:         .long 0
fb_height:        .long 0
fb_stride:        .long 0
fb_format:        .long 0
fb_pitch:         .long 0
payload_guid:     .space 16
rsdp_copy:        .space 36
.balign 4
dap:
    .byte 0x10, 0
dap_count:        .word 0
dap_offset:       .word 0
    .word BUFFER_SEGMENT
dap_lba:          .quad 0
.balign 16
meta:             .space 512

m_banner:     .asciz "Fosfor stage 2 (BIOS path): real mode, 16-bit. (Hold Shift to skip the graphics mode.)\r\n"
m_a20:        .asciz "  A20 line: "
m_e820:       .asciz "  Memory map (INT 15h E820): "
m_e820_line:  .asciz "    0x"
m_e820_len:   .asciz " + 0x"
m_e820_type:  .asciz " type "
m_rsdp:       .asciz "  ACPI RSDP: "
m_rsdp_none:  .asciz "none found (the kernel will run without ACPI tables)\r\n"
m_gpt:        .asciz "  GPT (LBA 1): looking for the payload partition... "
m_found:      .asciz "found at LBA 0x"
m_sectors:    .asciz ", 0x"
m_crashed:    .asciz "Previous boot never reached the kernel's milestone (crash, hang or power loss): counted as a crash.\r\n"
m_unclean:    .asciz "Previous boot ended without a clean stop (after reaching its milestone): counted as unclean.\r\n"
m_slot:       .asciz "  Slot metadata: booting slot "
m_kernel_slot:.asciz "  Kernel slot "
m_colon:      .asciz ": "
m_sectors_kib:.asciz " sectors ("
m_kib_end:    .asciz " KiB).\r\n"
m_meta_ok:    .asciz "  Slot metadata written back and read back identical.\r\n"
m_meta_mismatch: .asciz "  WARNING: the slot metadata read back DIFFERENT from what was written (the BIOS write did not take effect, or corrupted it).\r\n"
m_meta_ro:    .asciz "  Warning: the slot metadata could not be written back; fallback tracking is off this boot.\r\n"
m_place:      .asciz "  Placement (from the memory map): kernel image at 0x"
m_place_staging: .asciz ", read buffer at 0x"
m_no_room:    .asciz "FAILED: no usable RAM region above 1 MiB is large enough for the kernel and its read buffer (about 16 MiB).\r\n"
m_load:       .asciz "  Reading the kernel slot (INT 13h EDD) and copying it above 1 MiB (unreal mode): "
m_long:       .asciz "  Entering long mode (PAE, EFER.LME, paging); next the ELF segments are placed and the kernel starts.\r\n"
m_edid:       .asciz "  Video: the monitor reports its native size (EDID): "
m_video:      .asciz "  Video: VBE 0x"
m_video_mode: .asciz ", mode 0x"
m_by:         .asciz "x"
m_video_at:   .asciz " 32 bpp, linear framebuffer at 0x"
m_video_skipped: .asciz "  Video: skipped (Shift is held, or `debug video skip` is set): text mode stays, the kernel is serial only.\r\n"
m_video_none: .asciz "  Video: no usable VBE mode (a 32-bit linear one is needed); text mode stays, the kernel is serial only.\r\n"
m_video_failed: .asciz "  Video: the video BIOS refused the mode; the kernel is serial only.\r\n"
m_ok:         .asciz "ok\r\n"
m_crlf:       .asciz "\r\n"
m_a20_fail:   .asciz "FAILED, cannot enable A20.\r\n"
m_e820_fail:  .asciz "FAILED, the BIOS gave no memory map.\r\n"
m_no_payload: .asciz "none: this disk has no fos payload partition.\r\n"
m_gpt_bad:    .asciz "FAILED, the GPT is not what this loader expects.\r\n"
m_meta_bad:   .asciz "FAILED, the slot-metadata sector is not valid.\r\n"
m_slot_empty: .asciz "FAILED, the selected kernel slot is empty or larger than a slot.\r\n"
m_disk_fail:  .asciz " <- FAILED, BIOS disk error (INT 13h status above).\r\n"
m_elf_bad:    .asciz "FAILED: the kernel slot does not hold a position-independent ELF64 image.\r\n"
m_image_big:  .asciz "FAILED: the kernel image is larger than the 8 MiB reserved for it.\r\n"
m_reloc_bad:  .asciz "FAILED: the kernel has a relocation other than R_X86_64_RELATIVE.\r\n"
