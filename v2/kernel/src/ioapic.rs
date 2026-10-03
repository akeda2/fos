//! The I/O APIC: routes the legacy ISA interrupts (keyboard IRQ 1, ...) and PCI
//! interrupt lines to a vector on the CPU. Each pin has a 64-bit "redirection entry"
//! (vector, polarity, trigger mode, mask, destination). Registers are reached through
//! an index/data pair: write the register number to IOREGSEL (+0x00), then read or
//! write IOWIN (+0x10). The ACPI MADT says where it is and which ISA interrupts were
//! rewired to other pins ("interrupt source overrides").

use crate::{acpi, apic, paging};

static mut BASE: u64 = 0;
static mut GSI_BASE: u32 = 0;
static mut PINS: u32 = 0;
static mut OVERRIDES: [(u8, u32, u16); 16] = [(0, 0, 0); 16];
static mut OVERRIDE_COUNT: usize = 0;

fn read(register: u32) -> u32 {
    unsafe {
        (BASE as *mut u32).write_volatile(register);
        ((BASE + 0x10) as *const u32).read_volatile()
    }
}

fn write(register: u32, value: u32) {
    unsafe {
        (BASE as *mut u32).write_volatile(register);
        ((BASE + 0x10) as *mut u32).write_volatile(value);
    }
}

/// Maps the first I/O APIC and masks every pin. Returns the number of pins.
pub fn init(madt: &acpi::Madt) -> Option<u32> {
    if madt.io_apics == 0 || madt.io_apic_address == 0 {
        return None;
    }
    unsafe {
        BASE = madt.io_apic_address;
        GSI_BASE = madt.io_apic_gsi_base;
        OVERRIDES = madt.overrides;
        OVERRIDE_COUNT = madt.override_count;
    }
    paging::map_device(madt.io_apic_address, 4096);
    let pins = ((read(1) >> 16) & 0xff) + 1;
    unsafe { PINS = pins };
    for pin in 0..pins {
        write(0x10 + 2 * pin, 1 << 16); // masked
    }
    Some(pins)
}

/// Routes ISA interrupt `irq` to `vector` on this CPU. Returns the GSI it used.
/// ISA defaults are edge-triggered, active high; an override can change the pin and
/// both of those (MPS flags: bits 0-1 polarity, 2-3 trigger, 3 = active low / level).
pub fn route_isa(irq: u8, vector: u8) -> Option<u32> {
    let (mut gsi, mut flags) = (u32::from(irq), 0u16);
    unsafe {
        for &(source, target, f) in &OVERRIDES[..OVERRIDE_COUNT] {
            if source == irq {
                gsi = target;
                flags = f;
            }
        }
        if BASE == 0 || gsi < GSI_BASE || gsi >= GSI_BASE + PINS {
            return None;
        }
    }
    let pin = gsi - unsafe { GSI_BASE };
    let mut low = u32::from(vector);
    if flags & 3 == 3 {
        low |= 1 << 13; // active low
    }
    if (flags >> 2) & 3 == 3 {
        low |= 1 << 15; // level triggered
    }
    write(0x10 + 2 * pin + 1, apic::id() << 24);
    write(0x10 + 2 * pin, low);
    Some(gsi)
}

/// Routes global system interrupt `gsi` to `vector` on this CPU, with the given trigger
/// mode and polarity (PCI interrupts are level-triggered, active low). False if no I/O
/// APIC pin serves that GSI.
pub fn route_gsi(gsi: u32, vector: u8, level: bool, active_low: bool) -> bool {
    unsafe {
        if BASE == 0 || gsi < GSI_BASE || gsi >= GSI_BASE + PINS {
            return false;
        }
        let pin = gsi - GSI_BASE;
        let mut low = u32::from(vector);
        if active_low {
            low |= 1 << 13;
        }
        if level {
            low |= 1 << 15;
        }
        write(0x10 + 2 * pin + 1, apic::id() << 24);
        write(0x10 + 2 * pin, low);
    }
    true
}

/// Masks the pin of `gsi` (an interrupt storm, or a failed probe).
pub fn mask_gsi(gsi: u32) {
    unsafe {
        if BASE != 0 && gsi >= GSI_BASE && gsi < GSI_BASE + PINS {
            let pin = gsi - GSI_BASE;
            write(0x10 + 2 * pin, read(0x10 + 2 * pin) | (1 << 16));
        }
    }
}

/// Number of pins and the first GSI served (for probing candidates).
pub fn gsi_range() -> (u32, u32) {
    unsafe { (GSI_BASE, GSI_BASE + PINS) }
}
