.code16
.section .text
.include "netrom_layout.inc"

# A PCI Expansion ROM for the RTL8139 NIC: an alternative to boot/stage1.S +
# boot/stage2.S that boots fos entirely from the network card's own boot ROM
# chip instead of from a disk. This is an additional, optional boot path --
# `make image`/`make run`/`make test` and USB/disk booting are completely
# unaffected; see `make netrom` and README.md's "Booting from a NIC ROM"
# section.
#
# Legacy BIOS looks for exactly this kind of code on every PCI card at every
# POST: each card may expose a small ROM (via its PCI Expansion ROM Base
# Address Register) starting with the same 0x55 0xaa signature our MBR uses,
# plus a checksum and a "PCIR" data structure identifying which PCI device it
# belongs to. If BIOS finds one, it copies ("shadows") the ROM into memory
# somewhere in the 0xc0000-0xe0000 range and far-calls into it -- this is the
# exact mechanism PXE network-boot ROMs use, and it runs *before* BIOS
# attempts to boot from any disk.
#
# Unlike stage1.S/stage2.S, this code cannot know at build time which
# physical address (and therefore which real-mode segment) BIOS will shadow
# it to -- that depends on how many other cards' option ROMs come before it.
# So, unlike stage2.S (which is always loaded at a fixed 0x8000 and can bake
# that address into its instructions directly), every routine here first
# reads its own running segment from %cs and keeps everything else relative
# to that, rather than assuming any fixed load address.
#
# A second, easy-to-miss consequence of being shadowed: a PCI expansion ROM's
# memory is a *ROM*, and BIOS's shadow copy of it is conventionally read-only
# at runtime, exactly like the real chip it stands in for -- so, unlike
# stage2.S (which freely keeps mutable variables alongside its own code),
# this file cannot use any label inside its own image as writable storage.
# Every piece of state this code needs to change at run time (its own
# segment, the computed physical base, whether serial is present, and the
# two run-time-patched descriptors used to enter protected mode) instead
# lives in a small fixed scratch area in genuine low RAM, addressed the same
# way as the E820/BootInfo buffer below: ES = 0, with an explicit `%es:`
# override on every access.

.set BOOT_INFO, 0x5000
.set MEMORY_MAP, BOOT_INFO + 36
.set MAX_MEMORY_REGIONS, 32
.set COM1, 0x3f8
.set KERNEL_LOAD_SEGMENT, 0x1000       # Segment for physical 0x10000, the kernel's fixed load address.
.set KERNEL_LOAD_ADDRESS, 0x10000
.set BOOT_STACK_POINTER, 0x7a00        # Same stack stage1.S/stage2.S already use; segment 0, so it can't collide with wherever our own ROM code got shadowed to.
.set INT19_VECTOR, 0x19 * 4            # IVT offset of INT 19h ("bootstrap loader"), the BIOS-conventional way an option ROM defers its actual boot action until BIOS is done with the rest of POST.
.set PCI_VENDOR_ID, 0x10ec
.set PCI_DEVICE_ID, 0x8139

# Fixed scratch RAM for this file's own mutable state (see the note above on
# why none of it can live inside the ROM image itself). 0x600-0x6ff sits in
# the same conventionally "free for use" low-memory area stage2.S's own
# 0x500-0x510 A20 test bytes use, well clear of the IVT (0-0x3ff), the BIOS
# Data Area (0x400-0x4ff), and this file's own BOOT_INFO/E820 buffer
# (0x5000 and up).
.set SCRATCH_ROM_SEGMENT, 0x600        # word: this ROM's own shadow segment, read back from %cs once at startup.
.set SCRATCH_ROM_PHYSICAL_BASE, 0x602  # long: SCRATCH_ROM_SEGMENT << 4.
.set SCRATCH_SERIAL_AVAILABLE, 0x606   # byte: whether COM1 answered during initialize_serial.
.set SCRATCH_GDT_DESCRIPTOR, 0x608     # word limit + long base (6 bytes): the GDTR operand `lgdt` reads.
.set SCRATCH_PROTECTED_ENTRY_PTR, 0x60e # long offset + word selector (6 bytes): the `ljmp *` operand into protected_entry.

.global _start
_start:
rom_header:
    .byte 0x55, 0xaa        # PCI/legacy expansion ROM signature, just like the MBR's own 0x55 0xaa.
    .byte 0                 # Size in 512-byte blocks; patched by tools/build_netrom.py once the final padded size is known.
    .byte 0xe9               # Near JMP opcode...
    .word init_entry - . - 2 # ...to init_entry, encoded as a 2-byte rel16 (this 5-byte sequence, all together, is the mandatory "init vector" at header offset 3).
    .space 0x18 - (. - rom_header), 0  # Reserved by the spec; BIOS ignores this region, so it is just padding up to the pointer field below.
pci_struct_pointer:
    .word pci_struct - rom_header      # Header offset 0x18: pointer (from ROM start) to the PCI Data Structure below.
pci_struct:
    .ascii "PCIR"
    .word PCI_VENDOR_ID
    .word PCI_DEVICE_ID
    .word 0                            # Reserved.
    .word pci_struct_end - pci_struct  # Data structure length (24 bytes).
    .byte 0                            # Data structure revision.
    .byte 0x00, 0x00, 0x02              # Class code (prog-if, subclass, base class): 02/00/00 = network/ethernet controller.
    .word 0                            # Image length in 512-byte blocks; patched by tools/build_netrom.py, same value as rom_header's size byte.
    .word 0                            # Revision level.
    .byte 0x00                         # Code type: 0x00 = IA-32 (PC-AT) real-mode code.
    .byte 0x80                         # Indicator: bit 7 set = last (only) image in this ROM.
    .word 0                            # Reserved.
pci_struct_end:

# Called far by BIOS during PCI device/option-ROM scanning, early in POST --
# well before BIOS is ready to boot anything. Well-behaved option ROMs do
# their one-time setup and return normally (`lret`) so BIOS can continue
# initializing everything else; the *actual* boot action is deferred to
# INT 19h, which BIOS only invokes once POST has otherwise finished. This is
# exactly how real network (PXE) boot ROMs are structured, and it is the
# only way this can work at all, since BIOS has not yet finished setting up
# the rest of the machine (video, keyboard, other disks) at the point this
# runs.
init_entry:
    pushw %ax
    pushw %bx
    pushw %cx
    pushw %dx
    pushw %si
    pushw %di
    pushw %bp
    pushw %ds
    pushw %es
    pushfw

    movw %cs, %ax
    movw %ax, %ds              # DS must be set before reading/writing our own labels below -- BIOS leaves DS at an arbitrary value on entry.
    xorw %ax, %ax
    movw %ax, %es              # ES = 0 for the scratch-RAM writes below.
    movw %cs, %ax
    movw %ax, %es:SCRATCH_ROM_SEGMENT

    call initialize_serial
    movw $banner, %si
    call print

    call install_int19_hook

    popfw
    popw %es
    popw %ds
    popw %bp
    popw %di
    popw %si
    popw %dx
    popw %cx
    popw %bx
    popw %ax
    lret

# Overwrites the real-mode IVT's INT 19h vector (4 bytes at physical
# 0x00064) to point at boot_entry, in our own segment. Whichever option ROM
# or BIOS routine last installs this vector before POST finishes is the one
# that gets to boot the machine -- if a bootable disk is also present, this
# NIC ROM booting fos instead is simply a matter of running its own PCI
# device scan slot after the disk subsystem's own INT 19h setup.
install_int19_hook:
    cli
    movw $boot_entry, %ax
    movw %ax, %es:INT19_VECTOR
    movw %es:SCRATCH_ROM_SEGMENT, %ax
    movw %ax, %es:INT19_VECTOR + 2
    sti
    ret

# Reached later, once BIOS itself invokes INT 19h wanting to bootstrap the
# machine -- this never returns, and is where the actual boot narrative
# (identical in spirit to stage2.S's, minus the disk read it replaces with a
# plain memory copy) begins.
boot_entry:
    cli
    xorw %ax, %ax
    movw %ax, %ss
    movw $BOOT_STACK_POINTER, %sp
    movw %cs, %ax
    movw %ax, %ds
    xorw %ax, %ax
    movw %ax, %es               # ES = 0 from here on: used, with an explicit %es: override, for every fixed-physical-address BootInfo/E820/scratch access below.
    movw %cs, %ax
    movw %ax, %es:SCRATCH_ROM_SEGMENT

    call compute_rom_physical_base
    call patch_transition_pointers

    movw $boot_banner, %si
    call print
    call initialize_boot_info
    call collect_memory_map
    jc memory_map_error
    movw $memory_map_done, %si
    call print
    call enable_a20
    jc a20_error
    movw $a20_done, %si
    call print
    call copy_kernel
    movw $kernel_done, %si
    call print
    movw $transition_message, %si
    call print
    call enter_protected_mode

memory_map_error:
    movw $memory_map_failed, %si
    jmp fatal
a20_error:
    movw $a20_failed, %si
fatal:
    call print
halt:
    cli
    hlt
    jmp halt

# The one flat physical-address computation this whole file needs: real
# mode's segment:offset addresses translate to physical addresses as
# (segment << 4) + offset, so this converts our own (otherwise unknown at
# build time) running segment into that same flat form, for the handful of
# places that need a genuine physical address rather than a segment-relative
# one (the protected-mode GDT descriptor's base field, and the far-jump
# target used for the real-mode-to-protected-mode transition below).
compute_rom_physical_base:
    xorl %eax, %eax
    movw %es:SCRATCH_ROM_SEGMENT, %ax
    shll $4, %eax
    movl %eax, %es:SCRATCH_ROM_PHYSICAL_BASE
    ret

# Unlike stage2.S (whose fixed STAGE2_BASE load address lets it bake
# physical far-jump targets directly into its instructions), this ROM's
# code segment is only known at run time, so the protected-mode-bound far
# jump below goes through a small in-memory far pointer (offset + selector,
# the same layout the CPU's own `ljmp *mem` form reads) that is filled in
# here, once, rather than by self-modifying the jump instruction itself.
# Both the GDT descriptor and this far pointer must live in the scratch RAM
# area (not alongside `gdt`/`protected_entry` in the ROM itself) because
# they are the only two things in this whole boot path that need to be
# *written* at run time -- everything else (code, strings, the GDT's three
# static entries) only ever needs to be read, which the read-only shadowed
# ROM handles fine.
patch_transition_pointers:
    movl %es:SCRATCH_ROM_PHYSICAL_BASE, %eax
    addl $gdt, %eax
    movl %eax, %es:SCRATCH_GDT_DESCRIPTOR + 2
    movw $(gdt_end - gdt - 1), %es:SCRATCH_GDT_DESCRIPTOR

    movl %es:SCRATCH_ROM_PHYSICAL_BASE, %eax
    addl $protected_entry, %eax
    movl %eax, %es:SCRATCH_PROTECTED_ENTRY_PTR
    movw $0x08, %es:SCRATCH_PROTECTED_ENTRY_PTR + 4  # Flat 32-bit code selector, same GDT layout as stage2.S's.
    ret

initialize_boot_info:
    movw $BOOT_INFO, %di
    xorw %ax, %ax
    movw $(36 + MAX_MEMORY_REGIONS * 24) / 2, %cx
    rep stosw
    movw $0x5342, %es:BOOT_INFO
    movw $0x464f, %es:BOOT_INFO + 2
    movw $2, %es:BOOT_INFO + 4
    movw $(36 + MAX_MEMORY_REGIONS * 24), %es:BOOT_INFO + 6
    movb $0x80, %es:BOOT_INFO + 8   # No BIOS-supplied boot-drive number reaches an option ROM's INT 19h handler, so this is a best-effort guess (the conventional first hard disk/USB drive), not a verified value.
    movb %es:SCRATCH_SERIAL_AVAILABLE, %al
    movb %al, %es:BOOT_INFO + 9
    movl $KERNEL_LOAD_ADDRESS, %es:BOOT_INFO + 12
    movl $(KERNEL_LOAD_ADDRESS + NETROM_KERNEL_SIZE), %es:BOOT_INFO + 16
    # BootInfo+20 (bios_disk_io) and beyond are left zeroed by the rep stosw
    # above: this ROM boot path does not implement the BIOS-disk real-mode
    # trampoline stage2.S provides (see README.md's "Booting from a NIC ROM"
    # section), so kernel/src/disk.rs's own address == 0 check reports disk
    # access as cleanly unavailable rather than attempting it.
    ret

collect_memory_map:
    xorl %ebx, %ebx
    xorw %bp, %bp
    movw $MEMORY_MAP, %di
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
    movw %bp, %es:BOOT_INFO + 10
    clc
    ret
memory_failed:
    stc
    ret

enable_a20:
    inb $0x92, %al
    orb $0x02, %al
    andb $0xfe, %al
    outb %al, $0x92
    call verify_a20
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

# Where stage2.S issues a BIOS EDD disk read, this ROM already has the
# kernel bytes sitting right here in its own shadowed memory (see
# kernel_data below, embedded via `.incbin` at assemble time) -- so "loading"
# the kernel is nothing more than a same-segment-to-another-segment memory
# copy, no BIOS call needed at all.
copy_kernel:
    pushw %es
    movw $KERNEL_LOAD_SEGMENT, %ax
    movw %ax, %es
    movw $kernel_data, %si
    xorw %di, %di
    movw $NETROM_KERNEL_SIZE, %cx
    rep movsb
    popw %es
    ret

enter_protected_mode:
    cli
    lgdt %es:SCRATCH_GDT_DESCRIPTOR
    movb $0xff, %al
    outb %al, $0xa1
    outb %al, $0x21
    movl %cr0, %eax
    orl $1, %eax
    movl %eax, %cr0
    ljmpl *%es:SCRATCH_PROTECTED_ENTRY_PTR

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
    movb $0, %es:SCRATCH_SERIAL_AVAILABLE
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
    movb $1, %es:SCRATCH_SERIAL_AVAILABLE
serial_init_done:
    ret

serial_write_byte:
    cmpb $0, %es:SCRATCH_SERIAL_AVAILABLE
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
    movb $0, %es:SCRATCH_SERIAL_AVAILABLE
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

banner:
    .asciz "\r\nFOS NIC option ROM: PCI expansion ROM header valid, BIOS called our init routine.\r\nInstalling an INT 19h hook so fos boots once the rest of POST finishes...\r\n"
boot_banner:
    .asciz "FOS NIC option ROM: INT 19h reached; taking over the boot process.\r\nQuerying BIOS INT 15h EAX=E820h for the physical memory map...\r\n"
memory_map_done:
    .asciz "Memory map collected.\r\nEnabling the A20 address line (needed to address memory above 1 MiB)...\r\n"
a20_done:
    .asciz "A20 enabled.\r\nCopying the Rust kernel from this ROM's own memory to physical 0x10000...\r\n"
kernel_done:
    .asciz "Kernel copied. BIOS services end now.\r\nMasking the legacy 8259 PIC and installing a flat 32-bit GDT...\r\n"
transition_message:
    .asciz "GDT loaded, CR0.PE set. Far-jumping into 32-bit protected mode and the Rust kernel.\r\n"
memory_map_failed:
    .asciz "FOS ERROR: E820 memory map unavailable.\r\n"
a20_failed:
    .asciz "FOS ERROR: unable to enable A20.\r\n"

# The GDT's three entries are only ever read by the CPU (segment loads,
# `lgdt`'s own fetch of the descriptor), never written, so -- unlike
# gdt_descriptor's base field and protected_entry_ptr above -- this table is
# safe to keep right here in the read-only shadowed ROM.
.align 8
gdt:
    .quad 0
    .quad 0x00cf9a000000ffff
    .quad 0x00cf92000000ffff
gdt_end:

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
    movl $KERNEL_LOAD_ADDRESS, %eax
    call *%eax
protected_halt:
    cli
    hlt
    jmp protected_halt

.code16
kernel_data:
    .incbin "kernel.bin"
