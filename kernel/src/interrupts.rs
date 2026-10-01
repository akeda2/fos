//! Minimal interrupt infrastructure: an IDT with all 32 reserved CPU
//! exception vectors (each reporting what fired and halting -- see
//! `report_exception`, except #DF, which uses an emergency task stack) plus
//! the four vectors this kernel actually drives (the PIT timer, the PS/2
//! keyboard, and the master/slave PICs' own spurious-interrupt vectors -- see
//! `spurious_master_interrupt`), remapped away from the CPU exception range;
//! plus tiny lock-free counters and a scancode queue that the shell drains
//! between prompts.
//!
//! There is still no fault *recovery* here (an exception halts, it never
//! resumes) -- that remains deliberately out of scope, per the project's
//! teaching goals. But leaving exceptions completely unhandled meant any
//! unexpected fault (page fault, GPF, ...) fell through to a double, then a
//! triple fault -- an unconditional CPU reset with no diagnostic at all,
//! which is exactly what real hardware looks like when something in this
//! kernel goes wrong that QEMU never reproduces. A minimal, halting handler
//! for every vector turns that silent reset loop into a legible message.

use core::arch::asm;

use crate::arch::x86::{inb, io_wait, outb};
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

/// The master PIC's IRQ7 and the slave PIC's IRQ15 double as each PIC's
/// "spurious interrupt" vector: real hardware occasionally raises INTR and
/// then withdraws the request line before it can be latched (electrical
/// noise, a device glitch, ...), and the 8259 reports that as if it were
/// IRQ7/IRQ15 even though nothing genuine happened and no device is wired
/// there. QEMU's idealized PIC essentially never does this, but real
/// chipsets do -- these two vectors need real IDT gates (so a spurious
/// firing doesn't fall through to the CPU's #GP-on-invalid-gate path, which
/// is what actually produced the "CPU EXCEPTION 13 ... error code 0x13b"
/// report on the AMD Athlon 64 X2 machine: 0x13b decodes to IDT index 39,
/// i.e. vector 0x27, IRQ7) and that check the in-service register before
/// deciding whether to send an EOI at all -- see `spurious_master_interrupt`
/// and `spurious_slave_interrupt`.
const SPURIOUS_MASTER_VECTOR: u8 = IRQ_BASE + 7;
const SPURIOUS_SLAVE_VECTOR: u8 = IRQ_BASE + 15;

/// A separate vector range for Message Signaled Interrupts
/// (`register_msi_irq`/`dispatch_msi_irq`), clear of the CPU exception
/// range (`0x00-0x1f`), the PIC's remapped legacy range (`IRQ_BASE`
/// through `IRQ_BASE + 15`, i.e. `0x20-0x2f`), and `lapic::SPURIOUS_VECTOR`
/// (`0x60`, chosen to sit just past this range). `gdt::
/// DOUBLE_FAULT_TSS_SELECTOR` (`0x30`) is a GDT *selector*, a completely
/// different namespace from an IDT vector number, so it is not a
/// conflict despite the overlapping numeral. 16 is a generous, arbitrary
/// bound -- comfortably more than this project's small, fixed set of
/// MSI-capable devices (currently just `hda.rs`) could ever need at once.
const MSI_VECTOR_BASE: u8 = 0x50;
const MSI_VECTOR_COUNT: usize = 16;

/// OCW3 command selecting the in-service register for the next read of a
/// PIC's command port.
const PIC_READ_ISR: u8 = 0x0b;

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
static mut SPURIOUS_IRQ_COUNT: u64 = 0;

const SCANCODE_QUEUE_CAPACITY: usize = 32;
static mut SCANCODE_QUEUE: [u8; SCANCODE_QUEUE_CAPACITY] = [0; SCANCODE_QUEUE_CAPACITY];
static mut QUEUE_HEAD: usize = 0;
static mut QUEUE_TAIL: usize = 0;
static mut SCANCODE_DROPS: u32 = 0;

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct IdtEntry {
    offset_low: u16,
    selector: u16,
    zero: u8,
    type_attributes: u8,
    offset_high: u16,
}

/// Connects one legacy PCI IRQ line to a tiny device-acknowledgement callback.
/// IDT gates are installed before any PCI line is unmasked; handlers only
/// acknowledge/count/mark work and leave descriptor parsing to foreground
/// polling code. IRQ0/1 remain owned by the PIT/keyboard and IRQ2 is the PIC
/// cascade, so none may be claimed by PCI.
///
/// A legacy INTx line is routinely shared between two or more PCI functions
/// -- QEMU's own i440fx/PIIX3 IRQ router hands out exactly this on an
/// ordinary machine (confirmed real-hardware-plausible, not just an
/// emulator quirk: seen assigning an RTL8139 NIC and an ES1370 audio card
/// to the same IRQ 11 with no other configuration involved), and a real
/// motherboard's fixed PCI slot wiring can do the same. Each backend's own
/// `handle_interrupt()` already checks its device's own status register
/// before doing anything (`nic.rs`'s documented "ack/count/flag, nothing
/// else" design), so calling every handler registered on a shared line is
/// safe: a handler that isn't the actual interrupt source simply finds its
/// own status bit clear and no-ops. `PCI_IRQ_HANDLERS` therefore holds up
/// to `MAX_SHARED_HANDLERS` handlers per line rather than one -- the same
/// bounded-fixed-array convention this kernel already uses everywhere else
/// -- instead of the previous single-slot design, which silently failed to
/// register (and therefore left that whole device uninitialized) the
/// instant it happened to share a line with an already-registered device.
pub fn register_pci_irq(irq: u8, handler: IrqHandler) -> bool {
    if irq < 3 || irq > 15 || irq == 7 || irq == 15 {
        return false;
    }
    unsafe {
        let flags: u32;
        asm!(
            "pushfd",
            "pop {flags:e}",
            flags = out(reg) flags,
            options(nomem, preserves_flags)
        );
        asm!("cli", options(nomem, nostack));
        let registered = {
            let slots = &mut PCI_IRQ_HANDLERS[usize::from(irq)];
            if slots
                .iter()
                .filter_map(|slot| *slot)
                .any(|existing| core::ptr::fn_addr_eq(existing, handler))
            {
                true
            } else if let Some(empty) = slots.iter_mut().find(|slot| slot.is_none()) {
                *empty = Some(handler);
                true
            } else {
                false
            }
        };
        if registered && STORM_MASKED & (1 << irq) == 0 {
            if irq < 8 {
                PIC1_MASK &= !(1 << irq);
                outb(PIC1_DATA, PIC1_MASK);
                io_wait();
            } else {
                PIC1_MASK &= !(1 << 2);
                PIC2_MASK &= !(1 << (irq - 8));
                outb(PIC1_DATA, PIC1_MASK);
                io_wait();
                outb(PIC2_DATA, PIC2_MASK);
                io_wait();
            }
        }
        if flags & (1 << 9) != 0 {
            asm!("sti", options(nomem, nostack));
        }
        registered
    }
}

pub fn pci_irq_registered(irq: u8) -> bool {
    irq < 16
        && unsafe { PCI_IRQ_HANDLERS[usize::from(irq)].iter().any(Option::is_some) }
}

pub fn pci_irq_count(irq: u8) -> u64 {
    if irq >= 16 {
        return 0;
    }
    read_u64_atomically(unsafe { core::ptr::addr_of!(PCI_IRQ_COUNTS[usize::from(irq)]) })
}

fn dispatch_pci_irq(irq: u8) {
    unsafe {
        PCI_IRQ_COUNTS[usize::from(irq)] += 1;
        for handler in PCI_IRQ_HANDLERS[usize::from(irq)].into_iter().flatten() {
            handler(irq);
        }
    }
    guard_against_storm(irq);
}

/// More dispatches of one PCI line than this within a single 10 ms timer tick
/// (50,000 a second) is treated as an interrupt storm, not traffic.
const STORM_LIMIT: u16 = 500;
static mut STORM_TICK: [u64; 16] = [0; 16];
static mut STORM_HITS: [u16; 16] = [0; 16];
/// Bit N set: the storm guard has masked IRQ N in the PIC.
static mut STORM_MASKED: u16 = 0;

/// Protects the whole machine from one misbehaving device. Legacy PCI IRQs are
/// level-triggered: a device whose interrupt cause the handler cannot clear
/// keeps the line asserted, the CPU re-enters the handler the instant it
/// returns, and the foreground code -- including the keyboard escape and the
/// network shell -- never runs again. That is a frozen machine that only a
/// hardware reset recovers. So count dispatches per line per timer tick and,
/// past `STORM_LIMIT`, mask the line in the PIC. Drivers poll anyway, so a
/// masked line costs interrupt wake-ups (callers see
/// `pci_irq_storm_masked` and stop sleeping on that NIC) but not correctness.
/// IRQ7/IRQ15 are the spurious-interrupt lines and are never PCI-registered.
fn guard_against_storm(irq: u8) {
    if irq == 7 || irq == 15 {
        return;
    }
    let index = usize::from(irq);
    unsafe {
        let now = ticks();
        if STORM_TICK[index] != now {
            STORM_TICK[index] = now;
            STORM_HITS[index] = 0;
        }
        STORM_HITS[index] = STORM_HITS[index].saturating_add(1);
        if STORM_HITS[index] > STORM_LIMIT && STORM_MASKED & (1 << irq) == 0 {
            if irq < 8 {
                PIC1_MASK |= 1 << irq;
                outb(PIC1_DATA, PIC1_MASK);
            } else {
                PIC2_MASK |= 1 << (irq - 8);
                outb(PIC2_DATA, PIC2_MASK);
            }
            STORM_MASKED |= 1 << irq;
        }
    }
}

/// Snapshot of the two 8259s as `(irr, isr, imr)`, each `slave << 8 | master`
/// (bit N = IRQ N). IRR is the *pending* requests, ISR those being serviced,
/// IMR the mask. A line that is asserted but masked sits in IRR with its IMR bit
/// set -- the way to find where a device's interrupt actually lands when the
/// line firmware recorded for it never fires. IRR/ISR are read by writing OCW3
/// (`0x0a`/`0x0b`) to the command port and reading it back.
pub fn pic_registers() -> (u16, u16, u16) {
    unsafe {
        let flags: u32;
        asm!("pushfd", "pop {flags:e}", flags = out(reg) flags, options(nomem, preserves_flags));
        asm!("cli", options(nomem, nostack));
        outb(PIC1_COMMAND, 0x0a);
        outb(PIC2_COMMAND, 0x0a);
        let irr = u16::from(inb(PIC1_COMMAND)) | u16::from(inb(PIC2_COMMAND)) << 8;
        outb(PIC1_COMMAND, 0x0b);
        outb(PIC2_COMMAND, 0x0b);
        let isr = u16::from(inb(PIC1_COMMAND)) | u16::from(inb(PIC2_COMMAND)) << 8;
        let imr = u16::from(inb(PIC1_DATA)) | u16::from(inb(PIC2_DATA)) << 8;
        if flags & (1 << 9) != 0 {
            asm!("sti", options(nomem, nostack));
        }
        (irr, isr, imr)
    }
}

/// Whether the storm guard has masked this PCI IRQ line (see
/// `guard_against_storm`); it stays masked until reboot.
pub fn pci_irq_storm_masked(irq: u8) -> bool {
    irq < 16 && unsafe { STORM_MASKED } & (1 << irq) != 0
}

/// One handler slot per MSI vector, deliberately unlike
/// `PCI_IRQ_HANDLERS`'s `MAX_SHARED_HANDLERS`-wide sharing. MSI's entire
/// point is exclusive, unshared ownership: each device gets its own
/// address/data pair pointing at its own dedicated vector, unlike legacy
/// INTx lines, whose sharing is a motherboard-topology fact no driver
/// controls. Reusing the shared-slot design here would defend against a
/// problem MSI cannot have.
static mut MSI_HANDLERS: [Option<IrqHandler>; MSI_VECTOR_COUNT] = [None; MSI_VECTOR_COUNT];
static mut MSI_IRQ_COUNTS: [u64; MSI_VECTOR_COUNT] = [0; MSI_VECTOR_COUNT];

/// Claims the next free MSI vector for a caller about to program a
/// device's MSI capability (`pci::PciAddress::enable_msi`). Returns
/// `None` once every slot in `MSI_VECTOR_BASE..MSI_VECTOR_BASE +
/// MSI_VECTOR_COUNT` is taken -- callers should fall back to legacy
/// `register_pci_irq` in that case, exactly as they would if the device
/// had no MSI capability at all.
pub fn allocate_msi_vector() -> Option<u8> {
    unsafe {
        let index = (0..MSI_VECTOR_COUNT).find(|&i| MSI_HANDLERS[i].is_none())?;
        Some(MSI_VECTOR_BASE + index as u8)
    }
}

/// Registers `handler` for a vector previously returned by
/// `allocate_msi_vector`. No PIC mask manipulation happens here at all --
/// unlike `register_pci_irq`, MSI never routes through the 8259, so there
/// is nothing to unmask.
pub fn register_msi_irq(vector: u8, handler: IrqHandler) -> bool {
    if vector < MSI_VECTOR_BASE || vector >= MSI_VECTOR_BASE + MSI_VECTOR_COUNT as u8 {
        return false;
    }
    let index = usize::from(vector - MSI_VECTOR_BASE);
    unsafe {
        MSI_HANDLERS[index] = Some(handler);
    }
    true
}

pub fn msi_irq_count(vector: u8) -> u64 {
    if vector < MSI_VECTOR_BASE || vector >= MSI_VECTOR_BASE + MSI_VECTOR_COUNT as u8 {
        return 0;
    }
    let index = usize::from(vector - MSI_VECTOR_BASE);
    read_u64_atomically(unsafe { core::ptr::addr_of!(MSI_IRQ_COUNTS[index]) })
}

fn dispatch_msi_irq(vector: u8) {
    let index = usize::from(vector - MSI_VECTOR_BASE);
    unsafe {
        MSI_IRQ_COUNTS[index] += 1;
        if let Some(handler) = MSI_HANDLERS[index] {
            handler(vector);
        }
    }
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
        Self::from_address(handler as usize as u32)
    }

    /// For the handful of CPU exception vectors where the processor itself
    /// pushes a 32-bit error code below the return address (see
    /// `install_exception_handlers`); the handler's signature must include
    /// it, or it will read the return address as if it were one instead.
    fn gate_with_error_code(handler: extern "x86-interrupt" fn(InterruptStackFrame, u32)) -> Self {
        Self::from_address(handler as usize as u32)
    }

    /// A task gate lets #DF abandon an unusable current stack and begin in a
    /// fully specified TSS with its own emergency stack.
    const fn task_gate(tss_selector: u16) -> Self {
        Self {
            offset_low: 0,
            selector: tss_selector,
            zero: 0,
            type_attributes: 0x85,
            offset_high: 0,
        }
    }

    fn from_address(address: u32) -> Self {
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
    crate::gdt::initialize();
    unsafe {
        install_exception_handlers();
        IDT[usize::from(TIMER_VECTOR)] = IdtEntry::gate(timer_interrupt);
        IDT[usize::from(KEYBOARD_VECTOR)] = IdtEntry::gate(keyboard_interrupt);
        IDT[usize::from(IRQ_BASE + 3)] = IdtEntry::gate(pci_irq3_interrupt);
        IDT[usize::from(IRQ_BASE + 4)] = IdtEntry::gate(pci_irq4_interrupt);
        IDT[usize::from(IRQ_BASE + 5)] = IdtEntry::gate(pci_irq5_interrupt);
        IDT[usize::from(IRQ_BASE + 6)] = IdtEntry::gate(pci_irq6_interrupt);
        IDT[usize::from(IRQ_BASE + 8)] = IdtEntry::gate(pci_irq8_interrupt);
        IDT[usize::from(IRQ_BASE + 9)] = IdtEntry::gate(pci_irq9_interrupt);
        IDT[usize::from(IRQ_BASE + 10)] = IdtEntry::gate(pci_irq10_interrupt);
        IDT[usize::from(IRQ_BASE + 11)] = IdtEntry::gate(pci_irq11_interrupt);
        IDT[usize::from(IRQ_BASE + 12)] = IdtEntry::gate(pci_irq12_interrupt);
        IDT[usize::from(IRQ_BASE + 13)] = IdtEntry::gate(pci_irq13_interrupt);
        IDT[usize::from(IRQ_BASE + 14)] = IdtEntry::gate(pci_irq14_interrupt);
        IDT[usize::from(SPURIOUS_MASTER_VECTOR)] = IdtEntry::gate(spurious_master_interrupt);
        IDT[usize::from(SPURIOUS_SLAVE_VECTOR)] = IdtEntry::gate(spurious_slave_interrupt);
        IDT[usize::from(MSI_VECTOR_BASE)] = IdtEntry::gate(msi_irq_50_interrupt);
        IDT[usize::from(MSI_VECTOR_BASE + 1)] = IdtEntry::gate(msi_irq_51_interrupt);
        IDT[usize::from(MSI_VECTOR_BASE + 2)] = IdtEntry::gate(msi_irq_52_interrupt);
        IDT[usize::from(MSI_VECTOR_BASE + 3)] = IdtEntry::gate(msi_irq_53_interrupt);
        IDT[usize::from(MSI_VECTOR_BASE + 4)] = IdtEntry::gate(msi_irq_54_interrupt);
        IDT[usize::from(MSI_VECTOR_BASE + 5)] = IdtEntry::gate(msi_irq_55_interrupt);
        IDT[usize::from(MSI_VECTOR_BASE + 6)] = IdtEntry::gate(msi_irq_56_interrupt);
        IDT[usize::from(MSI_VECTOR_BASE + 7)] = IdtEntry::gate(msi_irq_57_interrupt);
        IDT[usize::from(MSI_VECTOR_BASE + 8)] = IdtEntry::gate(msi_irq_58_interrupt);
        IDT[usize::from(MSI_VECTOR_BASE + 9)] = IdtEntry::gate(msi_irq_59_interrupt);
        IDT[usize::from(MSI_VECTOR_BASE + 10)] = IdtEntry::gate(msi_irq_5a_interrupt);
        IDT[usize::from(MSI_VECTOR_BASE + 11)] = IdtEntry::gate(msi_irq_5b_interrupt);
        IDT[usize::from(MSI_VECTOR_BASE + 12)] = IdtEntry::gate(msi_irq_5c_interrupt);
        IDT[usize::from(MSI_VECTOR_BASE + 13)] = IdtEntry::gate(msi_irq_5d_interrupt);
        IDT[usize::from(MSI_VECTOR_BASE + 14)] = IdtEntry::gate(msi_irq_5e_interrupt);
        IDT[usize::from(MSI_VECTOR_BASE + 15)] = IdtEntry::gate(msi_irq_5f_interrupt);
        IDT[usize::from(crate::lapic::SPURIOUS_VECTOR)] = IdtEntry::gate(lapic_spurious_interrupt);
    }

    load_kernel_idt();
    remap_pic();
    program_pit();
    #[cfg(feature = "page-protection-test")]
    crate::paging::trigger_text_write_for_test();

    unsafe {
        asm!("sti", options(nomem, nostack));
    }

    crate::println!(
        "Interrupts enabled: IDT loaded, PIC remapped to 0x{IRQ_BASE:02x}/0x{:02x},",
        IRQ_BASE + 8
    );
    crate::println!(
        "PIT at {PIT_FREQUENCY_HZ} Hz drives IRQ0 (timer), IRQ1 drives the keyboard."
    );
    // The local APIC is additive, alongside the PIC above, not a
    // replacement -- see "MSI/MSI-X" in AGENTS.md. This only maps its
    // MMIO window and arms its spurious-vector register; it does not
    // touch the PIC's own INTx routing at all.
    if crate::lapic::initialize() {
        crate::println!(
            "Local APIC found and enabled (ID {}): MSI available for capable devices.",
            crate::lapic::id()
        );
    } else {
        crate::println!("No local APIC found; MSI-capable devices will use legacy IRQs.");
    }
    #[cfg(feature = "stack-guard-test")]
    crate::stack::trigger_guard_page_for_test();
}

fn load_idt(limit: u16, base: u32) {
    let descriptor = IdtDescriptor { limit, base };
    unsafe {
        asm!("lidt [{0}]", in(reg) core::ptr::addr_of!(descriptor), options(nostack, preserves_flags));
    }
}

fn load_kernel_idt() {
    load_idt(
        (core::mem::size_of::<[IdtEntry; IDT_ENTRIES]>() - 1) as u16,
        core::ptr::addr_of!(IDT) as u32,
    );
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
    unsafe {
        PIC1_MASK = NORMAL_PIC1_MASK;
        PIC2_MASK = 0xff;
    }
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
    // Real 8259 PICs (or their chipset-integrated equivalents) can be slow
    // to latch a command byte; issuing the next one before that happens is
    // silently ignored, leaving the PIC mis-programmed rather than causing
    // any visible error. QEMU's emulated 8259 tolerates back-to-back writes
    // fine, but this has been observed to matter on real hardware (see
    // "Real hardware notes" in AGENTS.md) -- a mis-programmed PIC can leave
    // an IRQ vectored into the CPU exception range instead of `IRQ_BASE`,
    // and since this kernel installs no exception handlers, the very first
    // such interrupt double- (then triple-) faults the machine, which looks
    // exactly like a reset loop. `io_wait` after every `outb` here gives
    // the chip time to catch up.
    unsafe {
        // ICW1: begin initialization, expect ICW4.
        outb(PIC1_COMMAND, 0x11);
        io_wait();
        outb(PIC2_COMMAND, 0x11);
        io_wait();
        // ICW2: vector offsets.
        outb(PIC1_DATA, master_offset);
        io_wait();
        outb(PIC2_DATA, slave_offset);
        io_wait();
        // ICW3: master has a slave on IRQ2 (bit 2); slave's cascade identity is 2.
        outb(PIC1_DATA, 0x04);
        io_wait();
        outb(PIC2_DATA, 0x02);
        io_wait();
        // ICW4: 8086 mode.
        outb(PIC1_DATA, 0x01);
        io_wait();
        outb(PIC2_DATA, 0x01);
        io_wait();
        outb(PIC1_DATA, master_mask);
        io_wait();
        outb(PIC2_DATA, slave_mask);
        io_wait();
    }
}

/// Mask reflecting IRQ0/IRQ1 unmasked and everything else off.
const NORMAL_PIC1_MASK: u8 = 0b1111_1100;
static mut PIC1_MASK: u8 = NORMAL_PIC1_MASK;
static mut PIC2_MASK: u8 = 0xff;

type IrqHandler = fn(u8);
/// How many distinct device handlers one shared legacy PCI IRQ line can
/// hold. 4 comfortably covers any realistic sharing scenario this project's
/// small number of PCI backends (NIC, ES1370, ...) can produce -- QEMU's
/// own i440fx machine has at most a handful of PCI slots, each contributing
/// at most one interrupt-registering backend.
const MAX_SHARED_HANDLERS: usize = 4;
static mut PCI_IRQ_HANDLERS: [[Option<IrqHandler>; MAX_SHARED_HANDLERS]; 16] =
    [[None; MAX_SHARED_HANDLERS]; 16];
static mut PCI_IRQ_COUNTS: [u64; 16] = [0; 16];

/// Switches the PIC back to the BIOS-conventional mapping for a BIOS call.
/// Keep every device IRQ masked except IRQ0: BIOS disk timeouts need the
/// timer tick, but a kernel-owned PCI device interrupt would otherwise jump
/// through an unrelated real-mode IVT entry while the kernel IDT is absent.
pub fn use_bios_pic_mapping() {
    program_pic(BIOS_PIC1_OFFSET, BIOS_PIC2_OFFSET, 0xfe, 0xff);
}

/// Restores this kernel's own PIC mapping after a BIOS round trip, keeping
/// every IRQ the kernel had unmasked. `PIC1_MASK`/`PIC2_MASK` record the
/// masks as `register_pci_irq` last left them, and `use_bios_pic_mapping`
/// masks everything but IRQ0 for the BIOS call, so they must be written back
/// here. This used to call `remap_pic()`, which resets the masks to
/// "timer and keyboard only": the first BIOS disk transfer after NIC setup
/// then silently re-masked every PCI IRQ (and the cascade), leaving device
/// interrupts registered but never delivered -- invisible while everything
/// polled, but fatal to any idle loop that sleeps until a NIC interrupt.
pub fn use_kernel_pic_mapping() {
    let (master_mask, slave_mask) = unsafe { (PIC1_MASK, PIC2_MASK) };
    program_pic(IRQ_BASE, IRQ_BASE + 8, master_mask, slave_mask);
}

fn program_pit() {
    let divisor = (PIT_INPUT_HZ / PIT_FREQUENCY_HZ) as u16;
    unsafe {
        outb(PIT_COMMAND, 0x36); // channel 0, low+high byte, mode 3, binary
        io_wait();
        outb(PIT_CHANNEL0_DATA, divisor as u8);
        io_wait();
        outb(PIT_CHANNEL0_DATA, (divisor >> 8) as u8);
        io_wait();
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

/// Human-readable names for the 32 reserved CPU exception vectors (Intel
/// SDM Vol. 3A, ch. 6; a few late entries are AMD-specific). Used only for
/// `report_exception`'s printed diagnostic -- a bare vector number would
/// work too, but a name is far more useful when trying to diagnose a fault
/// that only reproduces on real hardware and never under QEMU.
const EXCEPTION_NAMES: [&str; 32] = [
    "Divide Error",
    "Debug",
    "NMI Interrupt",
    "Breakpoint",
    "Overflow",
    "BOUND Range Exceeded",
    "Invalid Opcode",
    "Device Not Available",
    "Double Fault",
    "Coprocessor Segment Overrun",
    "Invalid TSS",
    "Segment Not Present",
    "Stack-Segment Fault",
    "General Protection Fault",
    "Page Fault",
    "Reserved",
    "x87 Floating-Point Error",
    "Alignment Check",
    "Machine Check",
    "SIMD Floating-Point Exception",
    "Virtualization Exception",
    "Control Protection Exception",
    "Reserved",
    "Reserved",
    "Reserved",
    "Reserved",
    "Reserved",
    "Reserved",
    "Hypervisor Injection Exception",
    "VMM Communication Exception",
    "Security Exception",
    "Reserved",
];

/// Prints what CPU exception fired, its error code (if that vector has
/// one), and where, then halts for good -- this kernel deliberately has no
/// fault recovery (see the module doc comment), but printing this instead
/// of silently double- (then triple-) faulting turns an otherwise
/// indistinguishable reset loop into a legible diagnostic. This is the
/// single most useful thing to look at first when a boot problem only
/// reproduces on real hardware and never under QEMU.
fn report_exception(vector: u8, error_code: Option<u32>, frame: &InterruptStackFrame) -> ! {
    let name = EXCEPTION_NAMES[usize::from(vector)];
    let instruction_pointer = frame.instruction_pointer;
    let code_segment = frame.code_segment;
    let action = if crate::debug::reboot_on_crash() {
        "Rebooting"
    } else {
        "Halting"
    };
    if vector == 14 {
        // The CPU always supplies an error code for #PF; retain a harmless
        // fallback so this diagnostic path can never panic recursively.
        let code = error_code.unwrap_or(0);
        let address = crate::paging::fault_address();
        let cause = if code & 1 == 0 {
            "non-present mapping"
        } else {
            "protection violation"
        };
        let access = if code & (1 << 1) == 0 {
            "read"
        } else {
            "write"
        };
        let privilege = if code & (1 << 2) == 0 {
            "supervisor"
        } else {
            "user"
        };
        let region = crate::paging::describe_address(address);
        crate::println!(
            "\nPAGE FAULT at virtual address 0x{address:08x} ({region}): {cause} on {privilege} {access}, error code 0x{code:08x}, at {code_segment:04x}:{instruction_pointer:08x}.\n{action}: this kernel has no fault recovery."
        );
        finish_exception(vector, error_code, frame);
    }
    match error_code {
        Some(code) => crate::println!(
            "\nCPU EXCEPTION {vector} ({name}), error code 0x{code:08x}, at {code_segment:04x}:{instruction_pointer:08x}.\n{action}: this kernel has no fault recovery."
        ),
        None => crate::println!(
            "\nCPU EXCEPTION {vector} ({name}) at {code_segment:04x}:{instruction_pointer:08x}.\n{action}: this kernel has no fault recovery."
        ),
    }
    finish_exception(vector, error_code, frame);
}

/// Records a [`crate::debug::CrashRecord`] for the exception just reported,
/// then either halts forever (the default) or, when `reboot-on-crash` is
/// active in the `debug` file, waits a short configured delay and resets --
/// letting an unattended real-hardware machine recover from a crash with no
/// physical intervention. Ordinary exception handlers here (unlike the
/// double-fault task gate in `gdt.rs`) use `extern "x86-interrupt"`, which
/// never exposes the interrupted code's general-purpose registers, so the
/// crash record's register fields stay zero for every vector but 8.
fn finish_exception(vector: u8, error_code: Option<u32>, frame: &InterruptStackFrame) -> ! {
    let info =
        unsafe { crate::boot_info::BootInfo::from_address(crate::boot_info::BOOT_INFO_ADDRESS) };
    let booted_slot = info.map_or(0, |info| info.booted_slot);
    crate::debug::record_crash(
        vector,
        error_code,
        frame.code_segment,
        frame.instruction_pointer,
        frame.flags,
        frame.stack_segment,
        frame.stack_pointer,
        None,
        booted_slot,
    );
    if crate::debug::reboot_on_crash() {
        crate::debug::handle_crash(info);
    }
    crate::arch::x86::halt_forever();
}

/// Generates one `extern "x86-interrupt"` handler function per exception
/// vector (the ABI requires a distinct function per IDT gate; a vector
/// number can't be passed as an ordinary argument), forwarding straight to
/// `report_exception`. Two variants exist because the CPU only pushes an
/// error code for a handful of vectors (`gate_with_error_code`'s doc
/// comment); calling the wrong one desyncs the stack.
macro_rules! exception_handler {
    ($name:ident, $vector:expr) => {
        extern "x86-interrupt" fn $name(frame: InterruptStackFrame) {
            report_exception($vector, None, &frame);
        }
    };
}
macro_rules! exception_handler_with_error_code {
    ($name:ident, $vector:expr) => {
        extern "x86-interrupt" fn $name(frame: InterruptStackFrame, error_code: u32) {
            report_exception($vector, Some(error_code), &frame);
        }
    };
}

exception_handler!(exception_00_divide_error, 0);
exception_handler!(exception_01_debug, 1);
exception_handler!(exception_02_nmi, 2);
exception_handler!(exception_03_breakpoint, 3);
exception_handler!(exception_04_overflow, 4);
exception_handler!(exception_05_bound_range_exceeded, 5);
exception_handler!(exception_06_invalid_opcode, 6);
exception_handler!(exception_07_device_not_available, 7);
exception_handler!(exception_09_coprocessor_segment_overrun, 9);
exception_handler_with_error_code!(exception_10_invalid_tss, 10);
exception_handler_with_error_code!(exception_11_segment_not_present, 11);
exception_handler_with_error_code!(exception_12_stack_segment_fault, 12);
exception_handler_with_error_code!(exception_13_general_protection_fault, 13);
exception_handler_with_error_code!(exception_14_page_fault, 14);
exception_handler!(exception_15_reserved, 15);
exception_handler!(exception_16_x87_floating_point, 16);
exception_handler_with_error_code!(exception_17_alignment_check, 17);
exception_handler!(exception_18_machine_check, 18);
exception_handler!(exception_19_simd_floating_point, 19);
exception_handler!(exception_20_virtualization, 20);
exception_handler_with_error_code!(exception_21_control_protection, 21);
exception_handler!(exception_22_reserved, 22);
exception_handler!(exception_23_reserved, 23);
exception_handler!(exception_24_reserved, 24);
exception_handler!(exception_25_reserved, 25);
exception_handler!(exception_26_reserved, 26);
exception_handler!(exception_27_reserved, 27);
exception_handler!(exception_28_hypervisor_injection, 28);
exception_handler_with_error_code!(exception_29_vmm_communication, 29);
exception_handler_with_error_code!(exception_30_security_exception, 30);
exception_handler!(exception_31_reserved, 31);

/// Installs all 32 reserved-vector exception entries (see `report_exception`)
/// into `IDT`, ahead of `initialize`'s own timer/keyboard gates at vectors
/// 0x20/0x21. Vector 8 is a task gate; the other entries are ordinary
/// interrupt gates, comfortably clear of this range.
unsafe fn install_exception_handlers() {
    unsafe {
        IDT[0] = IdtEntry::gate(exception_00_divide_error);
        IDT[1] = IdtEntry::gate(exception_01_debug);
        IDT[2] = IdtEntry::gate(exception_02_nmi);
        IDT[3] = IdtEntry::gate(exception_03_breakpoint);
        IDT[4] = IdtEntry::gate(exception_04_overflow);
        IDT[5] = IdtEntry::gate(exception_05_bound_range_exceeded);
        IDT[6] = IdtEntry::gate(exception_06_invalid_opcode);
        IDT[7] = IdtEntry::gate(exception_07_device_not_available);
        IDT[8] = IdtEntry::task_gate(crate::gdt::DOUBLE_FAULT_TSS_SELECTOR);
        IDT[9] = IdtEntry::gate(exception_09_coprocessor_segment_overrun);
        IDT[10] = IdtEntry::gate_with_error_code(exception_10_invalid_tss);
        IDT[11] = IdtEntry::gate_with_error_code(exception_11_segment_not_present);
        IDT[12] = IdtEntry::gate_with_error_code(exception_12_stack_segment_fault);
        IDT[13] = IdtEntry::gate_with_error_code(exception_13_general_protection_fault);
        IDT[14] = IdtEntry::gate_with_error_code(exception_14_page_fault);
        IDT[15] = IdtEntry::gate(exception_15_reserved);
        IDT[16] = IdtEntry::gate(exception_16_x87_floating_point);
        IDT[17] = IdtEntry::gate_with_error_code(exception_17_alignment_check);
        IDT[18] = IdtEntry::gate(exception_18_machine_check);
        IDT[19] = IdtEntry::gate(exception_19_simd_floating_point);
        IDT[20] = IdtEntry::gate(exception_20_virtualization);
        IDT[21] = IdtEntry::gate_with_error_code(exception_21_control_protection);
        IDT[22] = IdtEntry::gate(exception_22_reserved);
        IDT[23] = IdtEntry::gate(exception_23_reserved);
        IDT[24] = IdtEntry::gate(exception_24_reserved);
        IDT[25] = IdtEntry::gate(exception_25_reserved);
        IDT[26] = IdtEntry::gate(exception_26_reserved);
        IDT[27] = IdtEntry::gate(exception_27_reserved);
        IDT[28] = IdtEntry::gate(exception_28_hypervisor_injection);
        IDT[29] = IdtEntry::gate_with_error_code(exception_29_vmm_communication);
        IDT[30] = IdtEntry::gate_with_error_code(exception_30_security_exception);
        IDT[31] = IdtEntry::gate(exception_31_reserved);
    }
}

extern "x86-interrupt" fn timer_interrupt(_frame: InterruptStackFrame) {
    unsafe {
        TIMER_TICKS += 1;
        TIMER_IRQ_COUNT += 1;
    }
    if !crate::stack::is_intact() {
        crate::println!(
            "\nKERNEL STACK OVERFLOW: the low-end canary was overwritten.\nHalting before corrupted kernel state can continue."
        );
        crate::arch::x86::halt_forever();
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

macro_rules! pci_irq_handler {
    ($name:ident, $irq:expr) => {
        extern "x86-interrupt" fn $name(_frame: InterruptStackFrame) {
            dispatch_pci_irq($irq);
            send_eoi(IRQ_BASE + $irq);
        }
    };
}

pci_irq_handler!(pci_irq3_interrupt, 3);
pci_irq_handler!(pci_irq4_interrupt, 4);
pci_irq_handler!(pci_irq5_interrupt, 5);
pci_irq_handler!(pci_irq6_interrupt, 6);
pci_irq_handler!(pci_irq8_interrupt, 8);
pci_irq_handler!(pci_irq9_interrupt, 9);
pci_irq_handler!(pci_irq10_interrupt, 10);
pci_irq_handler!(pci_irq11_interrupt, 11);
pci_irq_handler!(pci_irq12_interrupt, 12);
pci_irq_handler!(pci_irq13_interrupt, 13);
pci_irq_handler!(pci_irq14_interrupt, 14);

/// Reads a PIC's in-service register: bit N is set only while a genuine
/// IRQ N is actually being serviced. Used to tell a real IRQ7/IRQ15 apart
/// from a spurious one on the same vector (see `SPURIOUS_MASTER_VECTOR`).
fn read_isr(command_port: u16) -> u8 {
    unsafe {
        outb(command_port, PIC_READ_ISR);
        inb(command_port)
    }
}

extern "x86-interrupt" fn spurious_master_interrupt(_frame: InterruptStackFrame) {
    // Bit 7 of the master's ISR is only set for a genuine IRQ7; if it is
    // clear, the PIC never actually latched a real interrupt and must not
    // be told EOI (there is nothing in service to end).
    if read_isr(PIC1_COMMAND) & 0x80 != 0 {
        dispatch_pci_irq(7);
        send_eoi(SPURIOUS_MASTER_VECTOR);
    } else {
        unsafe {
            SPURIOUS_IRQ_COUNT += 1;
        }
    }
}

extern "x86-interrupt" fn spurious_slave_interrupt(_frame: InterruptStackFrame) {
    // A spurious IRQ15 still came in on the cascade line the slave shares
    // with the master's IRQ2, so the master needs an EOI regardless; only
    // the slave's own EOI depends on whether it was genuine.
    if read_isr(PIC2_COMMAND) & 0x80 != 0 {
        dispatch_pci_irq(15);
        send_eoi(SPURIOUS_SLAVE_VECTOR);
    } else {
        unsafe {
            SPURIOUS_IRQ_COUNT += 1;
        }
        unsafe {
            outb(PIC1_COMMAND, PIC_EOI);
        }
    }
}

macro_rules! msi_irq_handler {
    ($name:ident, $vector:expr) => {
        extern "x86-interrupt" fn $name(_frame: InterruptStackFrame) {
            dispatch_msi_irq($vector);
            crate::lapic::send_eoi();
        }
    };
}

msi_irq_handler!(msi_irq_50_interrupt, 0x50);
msi_irq_handler!(msi_irq_51_interrupt, 0x51);
msi_irq_handler!(msi_irq_52_interrupt, 0x52);
msi_irq_handler!(msi_irq_53_interrupt, 0x53);
msi_irq_handler!(msi_irq_54_interrupt, 0x54);
msi_irq_handler!(msi_irq_55_interrupt, 0x55);
msi_irq_handler!(msi_irq_56_interrupt, 0x56);
msi_irq_handler!(msi_irq_57_interrupt, 0x57);
msi_irq_handler!(msi_irq_58_interrupt, 0x58);
msi_irq_handler!(msi_irq_59_interrupt, 0x59);
msi_irq_handler!(msi_irq_5a_interrupt, 0x5a);
msi_irq_handler!(msi_irq_5b_interrupt, 0x5b);
msi_irq_handler!(msi_irq_5c_interrupt, 0x5c);
msi_irq_handler!(msi_irq_5d_interrupt, 0x5d);
msi_irq_handler!(msi_irq_5e_interrupt, 0x5e);
msi_irq_handler!(msi_irq_5f_interrupt, 0x5f);

/// A genuine spurious local-APIC interrupt (the CPU vectors here only
/// when an interrupt was withdrawn before it could be dispatched to a
/// real handler) needs no EOI at all, unlike the PIC's IRQ7/IRQ15
/// spurious handling above -- per the Intel SDM, there is nothing "in
/// service" to end, since no real interrupt was ever accepted.
extern "x86-interrupt" fn lapic_spurious_interrupt(_frame: InterruptStackFrame) {}

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
        } else {
            SCANCODE_DROPS = SCANCODE_DROPS.wrapping_add(1);
        }
    }
}

/// Returns scancodes waiting for foreground decoding and bytes that could not
/// be queued because IRQ1 arrived faster than the shell consumed input.
pub fn scancode_queue_stats() -> (usize, u32) {
    unsafe {
        asm!("cli", options(nomem, nostack));
        let pending = if QUEUE_TAIL >= QUEUE_HEAD {
            QUEUE_TAIL - QUEUE_HEAD
        } else {
            SCANCODE_QUEUE_CAPACITY - QUEUE_HEAD + QUEUE_TAIL
        };
        let drops = SCANCODE_DROPS;
        asm!("sti", options(nomem, nostack));
        (pending, drops)
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

pub fn spurious_irq_count() -> u64 {
    read_u64_atomically(core::ptr::addr_of!(SPURIOUS_IRQ_COUNT))
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
