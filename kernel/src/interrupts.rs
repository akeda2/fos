//! Minimal interrupt infrastructure: an IDT with just the two vectors this
//! kernel actually drives (the PIT timer and the PS/2 keyboard), remapped
//! away from the CPU exception range, plus tiny lock-free counters and a
//! scancode queue that the shell drains between prompts.
//!
//! Everything else (page faults, double faults, other IRQs) is intentionally
//! left unhandled: robust fault recovery is deliberately deferred, per the
//! project's teaching scope.

use core::arch::asm;

use crate::arch::x86::outb;
use crate::keyboard;

const CODE_SELECTOR: u16 = 0x08;
const IDT_ENTRIES: usize = 256;

/// PIC ports. The master handles IRQ0-7, the slave IRQ8-15 (cascaded on IRQ2).
const PIC1_COMMAND: u16 = 0x20;
const PIC1_DATA: u16 = 0x21;
const PIC2_COMMAND: u16 = 0xa0;
const PIC2_DATA: u16 = 0xa1;
const PIC_EOI: u8 = 0x20;

/// Remapped vector base: IRQ0 -> 0x20, IRQ1 -> 0x21, ... clear of the CPU's
/// reserved 0x00-0x1f exception range.
const IRQ_BASE: u8 = 0x20;
const TIMER_VECTOR: u8 = IRQ_BASE;
const KEYBOARD_VECTOR: u8 = IRQ_BASE + 1;

/// PIT channel 0, mode 3 (square wave), running at this tick rate.
const PIT_FREQUENCY_HZ: u32 = 100;
const PIT_INPUT_HZ: u32 = 1_193_182;
const PIT_CHANNEL0_DATA: u16 = 0x40;
const PIT_COMMAND: u16 = 0x43;

// This i686 target has no native 64-bit atomics, and these counters are
// only ever written from interrupt handlers (which run with IF=0, so they
// cannot race each other). Reads from mainline code briefly disable
// interrupts to avoid observing a torn 64-bit value.
static mut TIMER_TICKS: u64 = 0;
static mut TIMER_IRQ_COUNT: u64 = 0;
static mut KEYBOARD_IRQ_COUNT: u64 = 0;

const SCANCODE_QUEUE_CAPACITY: usize = 32;
static mut SCANCODE_QUEUE: [u8; SCANCODE_QUEUE_CAPACITY] = [0; SCANCODE_QUEUE_CAPACITY];
static mut QUEUE_HEAD: usize = 0;
static mut QUEUE_TAIL: usize = 0;

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct IdtEntry {
    offset_low: u16,
    selector: u16,
    zero: u8,
    type_attributes: u8,
    offset_high: u16,
}

impl IdtEntry {
    const fn missing() -> Self {
        Self {
            offset_low: 0,
            selector: 0,
            zero: 0,
            type_attributes: 0,
            offset_high: 0,
        }
    }

    fn gate(handler: extern "x86-interrupt" fn(InterruptStackFrame)) -> Self {
        let address = handler as usize as u32;
        Self {
            offset_low: address as u16,
            selector: CODE_SELECTOR,
            zero: 0,
            // Present, ring 0, 32-bit interrupt gate (0x8e): the CPU clears
            // IF on entry, so handlers cannot be re-entered by same IRQ.
            type_attributes: 0x8e,
            offset_high: (address >> 16) as u16,
        }
    }
}

#[repr(C, packed)]
struct IdtDescriptor {
    limit: u16,
    base: u32,
}

static mut IDT: [IdtEntry; IDT_ENTRIES] = [IdtEntry::missing(); IDT_ENTRIES];

/// Sets up the IDT, remaps the PIC clear of the exception range, arms the
/// PIT for periodic timer interrupts, unmasks only IRQ0/IRQ1, and enables
/// interrupts. Must run after `keyboard::initialize` and before the shell
/// loop starts consuming `pop_scancode`.
pub fn initialize() {
    unsafe {
        IDT[usize::from(TIMER_VECTOR)] = IdtEntry::gate(timer_interrupt);
        IDT[usize::from(KEYBOARD_VECTOR)] = IdtEntry::gate(keyboard_interrupt);
    }

    load_kernel_idt();
    remap_pic();
    program_pit();

    unsafe {
        asm!("sti", options(nomem, nostack));
    }

    crate::println!(
        "Interrupts enabled: IDT loaded, PIC remapped to 0x{IRQ_BASE:02x}/0x{:02x}, PIT at {PIT_FREQUENCY_HZ} Hz drives IRQ0 (timer), IRQ1 drives the keyboard.",
        IRQ_BASE + 8
    );
}

fn load_idt(limit: u16, base: u32) {
    let descriptor = IdtDescriptor { limit, base };
    unsafe {
        asm!("lidt [{0}]", in(reg) core::ptr::addr_of!(descriptor), options(nostack, preserves_flags));
    }
}

fn load_kernel_idt() {
    load_idt((core::mem::size_of::<[IdtEntry; IDT_ENTRIES]>() - 1) as u16, core::ptr::addr_of!(IDT) as u32);
}

/// Restores `IDTR` to the traditional real-mode IVT (256 4-byte far
/// pointers at physical address 0). `lidt` does not care what mode the CPU
/// is in -- real-mode `int` dispatch always reads through whatever table
/// IDTR currently points at, interpreted as 4-byte real-mode entries. Since
/// our own IDT holds 8-byte protected-mode gate descriptors at a completely
/// different address, any interrupt taken during a BIOS real-mode round
/// trip while our IDT is still loaded would misinterpret those bytes and
/// jump to garbage. Call this (paired with `use_bios_pic_mapping`) before
/// any BIOS real-mode call, and `use_kernel_idt` after it returns.
pub fn use_real_mode_idt() {
    load_idt(0x3ff, 0);
}

/// Restores this kernel's own protected-mode IDT after a BIOS round trip.
pub fn use_kernel_idt() {
    load_kernel_idt();
}


fn remap_pic() {
    program_pic(IRQ_BASE, IRQ_BASE + 8, NORMAL_PIC1_MASK, 0xff);
}

/// The BIOS-conventional PIC vector offsets (IRQ0-7 -> INT 08h-0Fh, IRQ8-15
/// -> INT 70h-77h), with everything unmasked. SeaBIOS/legacy BIOS code
/// installs its own real-mode handlers at these vectors during POST -- most
/// importantly IRQ0's, which advances the BIOS's BDA tick counter that its
/// own disk-timeout logic polls. As long as our kernel has this remapped
/// elsewhere, any such BIOS code that runs (like the stage-two disk
/// trampoline's real-mode round trip) will silently stop advancing that
/// counter and hang. `disk::transfer` restores this mapping for the
/// duration of each BIOS call, then puts our own mapping back afterward.
const BIOS_PIC1_OFFSET: u8 = 0x08;
const BIOS_PIC2_OFFSET: u8 = 0x70;

fn program_pic(master_offset: u8, slave_offset: u8, master_mask: u8, slave_mask: u8) {
    unsafe {
        // ICW1: begin initialization, expect ICW4.
        outb(PIC1_COMMAND, 0x11);
        outb(PIC2_COMMAND, 0x11);
        // ICW2: vector offsets.
        outb(PIC1_DATA, master_offset);
        outb(PIC2_DATA, slave_offset);
        // ICW3: master has a slave on IRQ2 (bit 2); slave's cascade identity is 2.
        outb(PIC1_DATA, 0x04);
        outb(PIC2_DATA, 0x02);
        // ICW4: 8086 mode.
        outb(PIC1_DATA, 0x01);
        outb(PIC2_DATA, 0x01);
        outb(PIC1_DATA, master_mask);
        outb(PIC2_DATA, slave_mask);
    }
}

/// Mask reflecting IRQ0/IRQ1 unmasked and everything else off.
const NORMAL_PIC1_MASK: u8 = 0b1111_1100;

/// Switches the PIC back to the BIOS-conventional mapping with everything
/// unmasked. Call immediately before any BIOS real-mode round trip.
pub fn use_bios_pic_mapping() {
    program_pic(BIOS_PIC1_OFFSET, BIOS_PIC2_OFFSET, 0x00, 0x00);
}

/// Restores this kernel's own PIC mapping after a BIOS round trip.
pub fn use_kernel_pic_mapping() {
    remap_pic();
}

fn program_pit() {
    let divisor = (PIT_INPUT_HZ / PIT_FREQUENCY_HZ) as u16;
    unsafe {
        outb(PIT_COMMAND, 0x36); // channel 0, low+high byte, mode 3, binary
        outb(PIT_CHANNEL0_DATA, divisor as u8);
        outb(PIT_CHANNEL0_DATA, (divisor >> 8) as u8);
    }
}

fn send_eoi(vector: u8) {
    unsafe {
        if vector >= IRQ_BASE + 8 {
            outb(PIC2_COMMAND, PIC_EOI);
        }
        outb(PIC1_COMMAND, PIC_EOI);
    }
}

/// The CPU-pushed frame for a hardware interrupt with no error code. Unused
/// by these handlers, but required by the `x86-interrupt` ABI signature.
#[repr(C)]
struct InterruptStackFrame {
    instruction_pointer: u32,
    code_segment: u32,
    flags: u32,
    stack_pointer: u32,
    stack_segment: u32,
}

extern "x86-interrupt" fn timer_interrupt(_frame: InterruptStackFrame) {
    unsafe {
        TIMER_TICKS += 1;
        TIMER_IRQ_COUNT += 1;
    }
    send_eoi(TIMER_VECTOR);
}

extern "x86-interrupt" fn keyboard_interrupt(_frame: InterruptStackFrame) {
    unsafe {
        KEYBOARD_IRQ_COUNT += 1;
    }
    if let Some(scancode) = keyboard::read_pending_scancode() {
        push_scancode(scancode);
    }
    send_eoi(KEYBOARD_VECTOR);
}

fn push_scancode(byte: u8) {
    // Interrupts are already disabled for the duration of this handler (it
    // was entered through an interrupt gate), so plain reads/writes here
    // cannot race with `pop_scancode`, which briefly disables interrupts
    // itself when called from the (uninterruptible-with-respect-to-this-IRQ)
    // main loop.
    unsafe {
        let next = (QUEUE_TAIL + 1) % SCANCODE_QUEUE_CAPACITY;
        if next != QUEUE_HEAD {
            SCANCODE_QUEUE[QUEUE_TAIL] = byte;
            QUEUE_TAIL = next;
        }
    }
}

/// Pops one buffered scancode byte produced by the keyboard IRQ, if any.
pub fn pop_scancode() -> Option<u8> {
    unsafe {
        asm!("cli", options(nomem, nostack));
        let result = if QUEUE_HEAD == QUEUE_TAIL {
            None
        } else {
            let byte = SCANCODE_QUEUE[QUEUE_HEAD];
            QUEUE_HEAD = (QUEUE_HEAD + 1) % SCANCODE_QUEUE_CAPACITY;
            Some(byte)
        };
        asm!("sti", options(nomem, nostack));
        result
    }
}

pub fn ticks() -> u64 {
    read_u64_atomically(core::ptr::addr_of!(TIMER_TICKS))
}

pub fn tick_frequency_hz() -> u32 {
    PIT_FREQUENCY_HZ
}

pub fn timer_irq_count() -> u64 {
    read_u64_atomically(core::ptr::addr_of!(TIMER_IRQ_COUNT))
}

pub fn keyboard_irq_count() -> u64 {
    read_u64_atomically(core::ptr::addr_of!(KEYBOARD_IRQ_COUNT))
}

/// Reads a `u64` counter with interrupts disabled, so a concurrent IRQ
/// handler cannot be observed mid-increment (which would tear the two
/// 32-bit halves on this target).
fn read_u64_atomically(pointer: *const u64) -> u64 {
    unsafe {
        asm!("cli", options(nomem, nostack));
        let value = core::ptr::read_volatile(pointer);
        asm!("sti", options(nomem, nostack));
        value
    }
}
