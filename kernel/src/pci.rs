//! Minimal PCI configuration-space access, used only to find serial
//! ("16550-compatible UART") controllers on add-on cards.
//!
//! This uses PCI Configuration Mechanism #1 (the `0xcf8` address port /
//! `0xcfc` data port pair), the original PCI configuration access method
//! supported by essentially every x86 chipset since PCI's introduction in
//! the 1990s -- including PCIe systems, whose root complex still answers
//! these legacy accesses for backward compatibility. This is deliberately
//! chosen over the newer, PCIe-specific memory-mapped ECAM mechanism
//! (which needs an MCFG ACPI table to locate) to keep this narrow and
//! broadly compatible, matching the rest of this project's approach to
//! hardware access.
//!
//! Only enough of PCI is implemented to serve this project's two uses:
//! walking every bus/device/function to find serial controllers by class
//! code (reading BAR0 for the UART's I/O port), and finding one specific
//! Ethernet card by vendor/device ID for the RTL8139 driver (`rtl8139.rs`,
//! reading its BAR0 the same way, plus writing the PCI command register to
//! enable I/O space and bus mastering, which DMA needs). There is no
//! support for memory-mapped BARs, PCI bridges, or any other device class;
//! this is a teaching-scoped lookup, not a general PCI driver.

use crate::arch::x86::{inl, outl};

const CONFIG_ADDRESS: u16 = 0xcf8;
const CONFIG_DATA: u16 = 0xcfc;

/// PCI class code for "simple communication controllers", subclass 0x00
/// for "serial controller" (regardless of the specific UART generation --
/// 8250/16450/16550/16750/... -- reported in the prog-if byte; this
/// project's UART code already targets the common, backward-compatible
/// 16550 register layout that all of them accept).
const CLASS_SIMPLE_COMMUNICATION: u8 = 0x07;
const SUBCLASS_SERIAL: u8 = 0x00;

const HEADER_TYPE_MULTIFUNCTION: u8 = 0x80;
const BAR0_OFFSET: u8 = 0x10;
const BAR_IS_IO_SPACE: u32 = 0b01;

fn config_address(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    0x8000_0000
        | (u32::from(bus) << 16)
        | (u32::from(device) << 11)
        | (u32::from(function) << 8)
        | u32::from(offset & 0xfc)
}

fn read_u32(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    unsafe {
        outl(CONFIG_ADDRESS, config_address(bus, device, function, offset));
        inl(CONFIG_DATA)
    }
}

fn read_u16(bus: u8, device: u8, function: u8, offset: u8) -> u16 {
    let value = read_u32(bus, device, function, offset);
    let shift = u32::from(offset & 2) * 8;
    ((value >> shift) & 0xffff) as u16
}

fn read_u8(bus: u8, device: u8, function: u8, offset: u8) -> u8 {
    let value = read_u32(bus, device, function, offset);
    let shift = u32::from(offset & 3) * 8;
    ((value >> shift) & 0xff) as u8
}

fn write_u32(bus: u8, device: u8, function: u8, offset: u8, value: u32) {
    unsafe {
        outl(CONFIG_ADDRESS, config_address(bus, device, function, offset));
        outl(CONFIG_DATA, value);
    }
}

/// The address of one PCI function, as found by `find_device`. Enough to
/// re-read its BARs or reconfigure its command register without walking
/// the bus again.
#[derive(Clone, Copy)]
pub struct PciAddress {
    bus: u8,
    device: u8,
    function: u8,
}

impl PciAddress {
    /// Reads BAR0, returning its I/O base address if it is I/O-mapped
    /// (`None` for memory-mapped BARs, which this project's port-I/O code
    /// cannot use).
    pub fn io_bar0(&self) -> Option<u16> {
        let bar0 = read_u32(self.bus, self.device, self.function, BAR0_OFFSET);
        if bar0 & BAR_IS_IO_SPACE == 0 {
            return None;
        }
        Some((bar0 & 0xffff_fffc) as u16)
    }

    /// Sets the I/O Space Enable (bit 0) and Bus Master Enable (bit 2)
    /// bits in the PCI command register (offset `0x04`), leaving every
    /// other bit as the firmware left it. Bus mastering is required for
    /// any device -- such as an Ethernet card -- that writes to memory on
    /// its own (DMA) instead of only being read from/written to via port
    /// I/O, as this project's UARTs are.
    pub fn enable_io_and_bus_master(&self) {
        const COMMAND_OFFSET: u8 = 0x04;
        const IO_SPACE_ENABLE: u32 = 1 << 0;
        const BUS_MASTER_ENABLE: u32 = 1 << 2;
        let command = read_u32(self.bus, self.device, self.function, COMMAND_OFFSET);
        write_u32(self.bus, self.device, self.function, COMMAND_OFFSET, command | IO_SPACE_ENABLE | BUS_MASTER_ENABLE);
    }
}

/// Finds the first PCI function matching the given vendor/device ID pair,
/// used by `rtl8139.rs` to locate that one specific Ethernet card. Unlike
/// `find_serial_ports`, this does not filter by class code, since some
/// chips (like the RTL8139) can be identified more precisely by ID than
/// by class alone.
pub fn find_device(vendor_id: u16, device_id: u16) -> Option<PciAddress> {
    for bus in 0..=255u8 {
        for device in 0..32u8 {
            let vendor = read_u16(bus, device, 0, 0x00);
            if vendor == 0xffff {
                continue;
            }
            let header_type = read_u8(bus, device, 0, 0x0e);
            let function_count = if header_type & HEADER_TYPE_MULTIFUNCTION != 0 { 8 } else { 1 };
            for function in 0..function_count {
                let function_vendor = if function == 0 { vendor } else { read_u16(bus, device, function, 0x00) };
                if function_vendor != vendor_id {
                    continue;
                }
                let function_device = read_u16(bus, device, function, 0x02);
                if function_device == device_id {
                    return Some(PciAddress { bus, device, function });
                }
            }
        }
    }
    None
}

/// Walks every PCI bus/device/function looking for serial controllers,
/// returning the I/O base address of each one's BAR0 (skipping any whose
/// BAR0 is memory-mapped rather than I/O-mapped, which this project's
/// port-I/O-based UART code cannot use). Stops once `out` is full.
///
/// Multi-port cards that expose one PCI function per port are handled
/// naturally (each function's own BAR0 is its own port); cards that pack
/// several ports behind a single BAR at fixed byte offsets are not
/// specially decoded, matching this project's narrow, single-purpose
/// scope elsewhere (e.g. the one-sector-at-a-time disk trampoline).
pub fn find_serial_ports(out: &mut [u16]) -> usize {
    let mut count = 0;
    for bus in 0..=255u8 {
        for device in 0..32u8 {
            let vendor = read_u16(bus, device, 0, 0x00);
            if vendor == 0xffff {
                continue;
            }
            let header_type = read_u8(bus, device, 0, 0x0e);
            let function_count = if header_type & HEADER_TYPE_MULTIFUNCTION != 0 { 8 } else { 1 };
            for function in 0..function_count {
                let function_vendor = if function == 0 { vendor } else { read_u16(bus, device, function, 0x00) };
                if function_vendor == 0xffff {
                    continue;
                }
                let class = read_u8(bus, device, function, 0x0b);
                let subclass = read_u8(bus, device, function, 0x0a);
                if class != CLASS_SIMPLE_COMMUNICATION || subclass != SUBCLASS_SERIAL {
                    continue;
                }
                let bar0 = read_u32(bus, device, function, BAR0_OFFSET);
                if bar0 & BAR_IS_IO_SPACE == 0 {
                    continue;
                }
                let Some(slot) = out.get_mut(count) else {
                    return count;
                };
                *slot = (bar0 & 0xffff_fffc) as u16;
                count += 1;
            }
        }
    }
    count
}
