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
# packet. Its fields are: direction byte at +0 (0=read, 1=write, 2=probe),
# status byte at +1, BIOS drive at +2, sector count byte at +3 (1..=126,
# ignored by the probe direction), LBA low/high dwords at +4/+8, buffer
# physical address at +12, and the DAP (direction 0/1) or the AH=48h "get
# extended drive parameters" result buffer (direction 2) at +16, both of
# which fit within the 64 bytes reserved before DISK_SAVED_ESP. +42/+44 hold
# a second copy of the caller buffer's offset/segment, used only by the
# probe path: its result buffer at +16 overlaps the DAP's own offset/segment
# fields at +20/+22, so probe must rebuild ES:DI from this untouched copy
# instead of reading back through fields AH=48h just overwrote.
#
# The sector-count byte at +3 lets one BIOS call transfer up to 126
# contiguous sectors instead of always exactly one; a single byte suffices
# since 126 fits comfortably. It is written only after the initial
# direction dword-store below (which zeroes bytes +1..+3), the same pattern
# already used for the drive-number byte at +2.
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
    movw $(48 + MAX_MEMORY_REGIONS * 24 + 2 + 4) / 2, %cx
    rep stosw
    movw $0x5342, BOOT_INFO
    movw $0x464f, BOOT_INFO + 2
    movw $5, BOOT_INFO + 4
    movw $(48 + MAX_MEMORY_REGIONS * 24 + 2 + 4), BOOT_INFO + 6
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
    # `BOOT_INFO + 816` (the new `booted_slot` byte, right after the fixed
    # 32-entry memory map -- see kernel/src/boot_info.rs) is left at the
    # zero `select_kernel_slot` will overwrite once it actually decides
    # which kernel slot this boot is using; a byte at +817 is reserved
    # padding, also left zero. `BOOT_INFO + 818` (`soft_reboot`, version 5)
    # points at `soft_reboot_entry` below, the kernel's callback for
    # warm-restarting through this same stage two instead of a hardware
    # reset -- see that label's own comment for what it does.
    movl $(soft_reboot_entry + STAGE2_BASE), BOOT_INFO + 818
    ret

collect_memory_map:
    xorw %ax, %ax
    movw %ax, %es
    movw $MEMORY_MAP, %di
    xorl %ebx, %ebx
    # Must clear the *full* 32-bit %ebp, not just %bp: on an ordinary BIOS
    # cold boot the CPU happens to leave %ebp's upper 16 bits zero already,
    # but a soft reboot re-enters this same code from the still-running
    # 32-bit Rust kernel (`reboot::soft_reboot`), which leaves %ebp holding
    # whatever 32-bit stack-frame-pointer value it was last using. The
    # `cmpl $MAX_MEMORY_REGIONS, %ebp` check below reads the full register,
    # so stale non-zero upper bits made it compare as already-past-the-limit
    # on the very first iteration -- silently producing a zero-entry memory
    # map (and, in turn, zero usable RAM) on every soft reboot instead of a
    # real E820 query.
    xorl %ebp, %ebp
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
    call select_kernel_slot
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

# Reads the one-sector boot metadata (LBA `METADATA_LBA`) and decides which
# of the two fixed-size kernel slots to boot from, implementing the A/B
# fallback/confirm state machine debug-mode remote kernel updates rely on
# (see kernel/src/debug.rs's `SlotMetadata`, which mirrors this exact byte
# layout and does the *writing* half of this same protocol -- this routine
# only ever advances metadata forward through states a genuinely running
# kernel already requested). Every reachable exit sets `kernel_packet`'s
# LBA field, `kernel_remaining`, and `BOOT_INFO + 816` (`booted_slot`)
# before returning, so `load_kernel_chunk`'s existing transfer loop and the
# kernel's own milestone-confirm logic both see a consistent choice.
#
# Metadata sector byte layout (must stay in sync with
# tools/build_image.py's `write_boot_metadata` and kernel/src/debug.rs):
#   +0..4   magic "FOSM"
#   +4      version (1)
#   +5      active_slot (0 or 1)
#   +6      pending_slot (0xff = none)
#   +7      attempted (0/1): has `pending_slot` been booted at least once
#   +8      confirmed (0/1): did that boot reach the kernel's milestone
#   +10     slot0_sectors (u16)
#   +12     slot1_sectors (u16)
select_kernel_slot:
    movb boot_drive + STAGE2_BASE, %dl
    movw $(metadata_packet + STAGE2_BASE), %si
    movb $0x42, %ah
    int $0x13
    jc select_kernel_slot_default

    movl metadata_buffer + STAGE2_BASE, %eax
    cmpl $0x4d534f46, %eax
    jne select_kernel_slot_default
    movb metadata_buffer + STAGE2_BASE + 4, %al
    cmpb $1, %al
    jne select_kernel_slot_default

    movb metadata_buffer + STAGE2_BASE + 6, %al
    cmpb $0xff, %al
    je select_kernel_slot_use_active

    # A pending slot exists; `attempted`/`confirmed` decide what to do
    # with it, per the state machine documented in AGENTS.md's "A/B kernel
    # slots" section.
    movb metadata_buffer + STAGE2_BASE + 8, %ah
    cmpb $1, %ah
    je select_kernel_slot_promote
    movb metadata_buffer + STAGE2_BASE + 7, %ah
    cmpb $0, %ah
    je select_kernel_slot_first_attempt

    # attempted && !confirmed: the last boot into the pending slot never
    # reached the kernel's milestone -- give up on it and fall back to the
    # still-known-good active slot.
    movb $0xff, metadata_buffer + STAGE2_BASE + 6
    movb $0, metadata_buffer + STAGE2_BASE + 7
    call select_kernel_slot_write_back
    jmp select_kernel_slot_use_active

select_kernel_slot_first_attempt:
    movb $1, metadata_buffer + STAGE2_BASE + 7
    call select_kernel_slot_write_back
    movb metadata_buffer + STAGE2_BASE + 6, %al
    jmp select_kernel_slot_apply

select_kernel_slot_promote:
    # The kernel confirmed but may have reset before it could clear
    # `pending_slot` itself (see kernel/src/debug.rs); finish that
    # promotion here so the next ordinary boot sees a clean, settled
    # `active_slot`/no-pending state.
    movb metadata_buffer + STAGE2_BASE + 6, %al
    movb %al, metadata_buffer + STAGE2_BASE + 5
    movb $0xff, metadata_buffer + STAGE2_BASE + 6
    movb $0, metadata_buffer + STAGE2_BASE + 7
    movb $0, metadata_buffer + STAGE2_BASE + 8
    call select_kernel_slot_write_back
    movb metadata_buffer + STAGE2_BASE + 5, %al
    jmp select_kernel_slot_apply

select_kernel_slot_use_active:
    movb metadata_buffer + STAGE2_BASE + 5, %al

select_kernel_slot_apply:
    movb %al, BOOT_INFO + 816
    cmpb $0, %al
    je select_kernel_slot_zero
    movw metadata_buffer + STAGE2_BASE + 12, %cx
    movl $SLOT1_LBA, kernel_packet + STAGE2_BASE + 8
    jmp select_kernel_slot_finish
select_kernel_slot_zero:
    movw metadata_buffer + STAGE2_BASE + 10, %cx
    movl $SLOT0_LBA, kernel_packet + STAGE2_BASE + 8
select_kernel_slot_finish:
    movw %cx, kernel_remaining + STAGE2_BASE
    ret

# Reached when the metadata sector can't be read at all, or doesn't carry
# this routine's expected magic/version -- a very old image predating this
# feature, or a corrupted sector. Falls back to this build's own slot 0
# kernel and its actual sector count (from `layout.inc`), the one case
# where a compile-time default is the best available answer.
select_kernel_slot_default:
    movb $0, BOOT_INFO + 816
    movw $KERNEL_SECTORS, kernel_remaining + STAGE2_BASE
    movl $SLOT0_LBA, kernel_packet + STAGE2_BASE + 8
    ret

select_kernel_slot_write_back:
    movb boot_drive + STAGE2_BASE, %dl
    movw $(metadata_packet + STAGE2_BASE), %si
    movb $0x43, %ah
    xorb %al, %al
    int $0x13
    ret

enter_protected_mode:
    cli
    lgdt gdt_descriptor + STAGE2_BASE
    # Install a real, valid protected-mode-format IDT (temp_idt below)
    # before ever setting CR0.PE, instead of leaving whatever small,
    # real-mode-shaped IDTR use_real_mode_idt() installed (see
    # kernel/src/interrupts.rs) in effect across the transition. That
    # tiny table (base 0, limit 0x3ff) is only ever safe to service an
    # interrupt against while genuinely in real mode -- real mode always
    # reads a 4-byte-per-vector table at physical address 0 regardless of
    # IDTR, so it works there even though it's the "wrong" table for
    # protected mode. A hardware interrupt landing in protected mode
    # while that table is still loaded misreads its bytes as 8-byte gate
    # descriptors instead and jumps to garbage -- exactly the class of
    # bug AGENTS.md's "Real hardware notes" documents for disk.rs's own
    # analogous window, and confirmed here via QEMU CPU tracing as a
    # real, reproducible triple fault: masking the 8259 (and even an
    # explicit EOI) immediately before this transition does not reliably
    # prevent an already-in-flight IRQ0 request from still being
    # delivered a handful of instructions into protected_entry below.
    # Rather than keep chasing that exact timing race, sidestep it
    # entirely: `temp_idt`'s 256 gates all point at one harmless stub
    # that just re-EOIs both PICs and returns, so any interrupt that
    # still manages to land here is a silent no-op instead of a fatal
    # exception, regardless of the exact mechanism that delivered it.
    # This IDT stays loaded until the kernel's own interrupts::initialize()
    # installs its real one.
    lidt temp_idt_descriptor + STAGE2_BASE
    movb $0xff, %al
    outb %al, $0xa1
    outb %al, $0x21
    movb $0x20, %al
    outb %al, $0xa0
    outb %al, $0x20
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
    .quad SLOT0_LBA
kernel_remaining:
    .word 0

.align 4
metadata_packet:
    .byte 0x10, 0
    .word 1
    .word (metadata_buffer + STAGE2_BASE), 0
    .quad METADATA_LBA
metadata_buffer:
    .space 512

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

# A minimal, valid protected-mode IDT (256 8-byte interrupt gates, all
# pointing at the one harmless `spurious_stub` below) installed by
# enter_protected_mode before CR0.PE is ever set, and left in place until
# the kernel's own interrupts::initialize() replaces it with the real
# one. See enter_protected_mode's own comment for why this exists: an
# interrupt landing in protected mode is only safe if IDTR points at a
# genuine 8-byte-gate-format table, unlike the tiny real-mode-shaped one
# (kernel/src/interrupts.rs's use_real_mode_idt()) this trampoline relies
# on elsewhere, which is only safe while genuinely in real mode.
.align 8
temp_idt:
.rept 256
    # Low 16 bits: `.word` truncates a 32-bit value naturally, so no
    # explicit mask is needed (and GAS's bitwise operators refuse a
    # still-section-relative symbol expression like this one anyway,
    # since this file has no separate link step -- see enter_protected_mode's
    # comment and the Makefile's plain `as --32` + `objcopy -O binary`
    # pipeline). High 16 bits: stage2 is capped at 32 sectors (16 KiB) and
    # STAGE2_BASE is 0x8000, so every address in this file is always well
    # under 0x10000 and its high word is always 0 -- hardcoded rather than
    # computed for the same GAS-limitation reason.
    .word spurious_stub + STAGE2_BASE
    .word 0x08
    .byte 0
    .byte 0x8e
    .word 0
.endr
temp_idt_descriptor:
    .word 256 * 8 - 1
    .long temp_idt + STAGE2_BASE

.code32
# Entered only via an interrupt gate (see temp_idt above), never by
# sequential execution -- any interrupt/exception taken while this
# temporary IDT is loaded lands here, harmlessly. Re-EOIs both PICs
# (harmless even if the interrupt wasn't a genuine, in-service one) and
# returns; the interrupted code resumes exactly where it left off.
spurious_stub:
    pushal
    movb $0x20, %al
    outb %al, $0xa0
    outb %al, $0x20
    popal
    iret

protected_entry:
    # Defensive re-close of the interrupt window as early as physically
    # possible after landing here. A soft reboot's real-mode _start pass
    # (kernel/src/reboot.rs -> use_bios_pic_mapping()) deliberately leaves
    # IRQ0 unmasked on the master PIC so BIOS's own real-mode tick/timeout
    # logic keeps working during the later BIOS disk reads that reload the
    # kernel image; enter_protected_mode (this same _start pass, moments
    # earlier) already re-masks both PICs to 0xff/0xff and does its own
    # cli before this far jump. Tracing a real, reproducible triple fault
    # here nonetheless showed IRQ0 still being serviced a handful of
    # instructions into protected_entry, with IF measured clear at the
    # time -- i.e. some part of that state did not reliably survive the
    # real-mode-to-protected-mode transition. Rather than keep chasing the
    # exact mechanism, re-do both unconditionally, immediately, before
    # touching any segment register or memory: a `cli` this early can only
    # ever narrow the window further, and masking both PICs again is
    # idempotent and cheap. This closes the race regardless of its exact
    # cause. use_real_mode_idt()'s small (base 0, limit 0x3ff) IDTR is
    # still loaded at this point -- it is only safe once real mode is
    # active, and this is exactly the class of hazard it protects against:
    # a hardware interrupt in protected mode using that table's bytes as
    # 8-byte gates instead of 4-byte real-mode vectors produces a garbage
    # jump target (see AGENTS.md's "Real hardware notes" for the same bug
    # class in disk.rs's own protected/real-mode IRQ window).
    cli
    movb $0xff, %al
    outb %al, $0xa1
    outb %al, $0x21
    movb $0x20, %al
    outb %al, $0xa0
    outb %al, $0x20

    movw $0x10, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %fs
    movw %ax, %gs
    movw %ax, %ss
    movl $0x9f000, %esp
    movl $BOOT_INFO, %ebx


    # A soft reboot (kernel/src/reboot.rs) can land here after
    # `load_kernel` just re-read a fresh kernel image, via a BIOS-emulated
    # disk transfer, into this same physical range (0x10000..0x80000) that
    # this VM instance was already executing as code moments earlier -- a
    # pattern no cold boot ever exercises. Empirically, jumping straight
    # into it from here is unreliable (see `kernel/src/entry.S`'s own
    # `_start` comment for the fuller account of this investigation); an
    # ordinary CPU load-then-store touch of every page in that range,
    # right here in flat 32-bit protected mode, reliably avoids it, unlike
    # the same *quantity* of unrelated I/O-port writes tried at this exact
    # location. The leading theory is that this forces QEMU/TCG's
    # self-modifying-code tracking to invalidate any translation cached
    # for the *old* kernel's code across the whole image, not just its
    # first few instructions, in a way the BIOS/DMA-style write path alone
    # did not reliably trigger for a translation block that had already
    # executed in this VM instance.
    movl $0x10000, %edi
    movl $0x80000, %ecx
    subl %edi, %ecx
    shrl $12, %ecx
resync_kernel_pages:
    movl (%edi), %edx
    movl %edx, (%edi)
    addl $0x1000, %edi
    loop resync_kernel_pages

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
    cmpl $2, %eax
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
    # The caller (kernel/src/disk.rs) now always supplies the target BIOS
    # drive number explicitly as a 5th cdecl argument, instead of this
    # trampoline hardcoding the one boot drive read from BOOT_INFO + 8 --
    # that is what lets a second, non-boot disk (BIOS drive 0x81, 0x82, ...)
    # be addressed through this same routine. Reject anything outside the
    # BIOS hard-disk drive-number range (0x80-0xff): a floppy or an
    # out-of-range value here would indicate a caller bug, not a disk that
    # merely happens to be absent.
    movl 24(%ebp), %eax
    testl $0xffffff00, %eax
    jnz bios_disk_invalid
    cmpl $0x80, %eax
    jb bios_disk_invalid
    movb %al, DISK_REQUEST + 2
    # A 6th cdecl argument lets the caller (kernel/src/disk.rs) request up
    # to 126 contiguous sectors in one BIOS call instead of always exactly
    # one; 126 matches the kernel loader's own already-proven safe transfer
    # size (0xfc00 bytes, comfortably clear of a real-mode segment's 64 KiB
    # boundary given this trampoline's 16-byte-aligned buffer requirement).
    # Reject 0 or anything above that bound as a caller bug.
    movl 28(%ebp), %eax
    testl $0xffffff00, %eax
    jnz bios_disk_invalid
    cmpl $0, %eax
    je bios_disk_invalid
    cmpl $126, %eax
    ja bios_disk_invalid
    movb %al, DISK_REQUEST + 3
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
    movzbw DISK_REQUEST + 3, %ax
    movw %ax, DISK_REQUEST + 18
    movl DISK_REQUEST + 4, %eax
    movl %eax, DISK_REQUEST + 24
    movl DISK_REQUEST + 8, %eax
    movl %eax, DISK_REQUEST + 28
    movl DISK_REQUEST + 12, %eax
    shrl $4, %eax
    movw %ax, DISK_REQUEST + 22
    movw $0, DISK_REQUEST + 20
    # bios_disk_probe reuses the DISK_REQUEST+16..+41 area as the AH=48h
    # result buffer, which overlaps the ES:DI destination segment/offset
    # word just stored at +20/+22 (the AH=48h "number of cylinders" field
    # lands on the exact same bytes). Stash a second copy outside that
    # range so the probe path can rebuild ES:DI afterward without reading
    # back BIOS-overwritten data.
    movw %ax, DISK_REQUEST + 44
    movw $0, DISK_REQUEST + 42
    movb DISK_REQUEST + 2, %dl
    movw $DISK_REQUEST + 16, %si
    # `disk::transfer` deliberately keeps IF=0 while the CPU is still in
    # protected mode with IDTR=0 (the real-mode IVT is not a valid
    # protected-mode IDT). The PIC/IVT are now ready and we are genuinely in
    # real mode, so BIOS disk timeout/tick handling can safely run IRQ0.
    sti
    cmpb $2, DISK_REQUEST
    je bios_disk_probe
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
bios_disk_probe:
    # A second, non-boot disk arrives with no drive number handed to us at
    # boot the way the boot drive does (in DL), so the kernel must probe for
    # it: AH=41h ("check extensions present") confirms the drive exists and
    # supports the same AH=42h/43h calls the read/write path above already
    # relies on, then AH=48h ("get extended drive parameters") reports its
    # total sector count so the kernel can bounds-check reads/writes against
    # it exactly as it already does for the boot disk.
    movw $0x55aa, %bx
    movb $0x41, %ah
    int $0x13
    jc bios_disk_failed
    cmpw $0xaa55, %bx
    jne bios_disk_failed
    # AH=48h requires the caller to fill in the buffer's own size (in bytes)
    # before the call; it overwrites this same DS:SI buffer the DAP above
    # would otherwise occupy, which is safe since direction 2 never reaches
    # the read/write path that needs the DAP layout.
    movw $26, DISK_REQUEST + 16
    movb $0x48, %ah
    int $0x13
    jc bios_disk_failed
    cmpw $26, DISK_REQUEST + 16
    jb bios_disk_failed
    # Copy the 8-byte total-sector-count field (offset 16 within the AH=48h
    # result buffer, i.e. DISK_REQUEST + 16 + 16) to the caller-supplied
    # buffer at ES:DI. Its segment:offset was computed above from the
    # caller's physical buffer address, but must be read back from the +42/
    # +44 safe stash rather than +20/+22: the AH=48h call above just
    # overwrote +20/+22 with its own "number of cylinders" result field,
    # since that field happens to occupy the exact same two bytes.
    movw DISK_REQUEST + 44, %ax
    movw %ax, %es
    movw DISK_REQUEST + 42, %di
    movw $(DISK_REQUEST + 32), %si
    movw $8, %cx
    cld
    rep movsb
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

# The "soft reboot" trampoline (`kernel/src/reboot.rs`'s `soft_reboot`
# callback, `BootInfo + 818`, version 5): warm-restarts the machine by
# dropping back into this very file's own `_start`, instead of asking the
# 8042 controller to pulse the hardware RESET line. Unlike
# `bios_disk_io` above, this never returns to the caller -- there is no
# "caller" left to return to once `_start` has zeroed `BOOT_INFO` and
# begun a fresh boot. The caller (kernel/src/reboot.rs) is responsible for
# `cli` and restoring the real-mode IVT/BIOS PIC mapping first, exactly as
# `disk::call_trampoline` already does before its own real-mode drop --
# `load_kernel`'s BIOS disk reads below need working BIOS timer-tick
# interrupt delivery exactly as much as an ordinary `bios_disk_io` call
# does, and the running kernel's IDT/PIC state is otherwise still loaded.
#
# Reuses the exact same protected-to-real-mode mechanics as
# `bios_disk_protected16`/`bios_disk_real` above -- selectors 0x18/0x20
# (see `kernel/src/gdt.rs`'s own comment on why it keeps these two 16-bit
# descriptors around), and the identical CR0.PE|PG-clearing mask -- since
# stage two's own `_start` reinitializes `BOOT_INFO`, re-collects the E820
# map, re-verifies A20, and re-runs `select_kernel_slot`'s full A/B
# fallback logic before reloading the kernel fresh from disk: a genuine
# repeat of an ordinary boot, just without BIOS POST.
.code32
soft_reboot_entry:
    ljmp $0x18, $(soft_reboot_16 + STAGE2_BASE)

.code16
soft_reboot_16:
    movw $0x20, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    movl %cr0, %eax
    andl $0x7ffffffe, %eax
    movl %eax, %cr0
    ljmp $0, $(soft_reboot_real + STAGE2_BASE)

soft_reboot_real:
    xorw %ax, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    # `_start` re-stores whatever is in %dl back into `boot_drive` as its
    # very first act; reload the value it already holds from the original
    # boot rather than leaving %dl as whatever garbage the last BIOS call
    # left behind, which would otherwise silently corrupt this saved value.
    movb boot_drive + STAGE2_BASE, %dl
    jmp _start
