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
//! Only enough of PCI is implemented to serve this project's four uses:
//! walking every bus/device/function to find serial controllers by class
//! code (reading BAR0 for the UART's I/O port), finding UHCI (USB 1.1) host
//! controllers the same class-code-matching way (`find_uhci_controllers`,
//! backing `uhci.rs`), finding Ethernet cards by vendor/device ID for the
//! RTL8139, 3C905, and Intel e1000 drivers (`rtl8139.rs`, `threecom.rs`, and
//! `e1000.rs`), and a general device dump (`scan_devices`, backing the
//! shell's `pci` command) used to identify hardware -- most concretely, an
//! unmarked or newly acquired NIC's real vendor/device ID -- before writing
//! or extending a driver for it. The serial/UHCI/NIC lookups read I/O BARs;
//! e1000 reads a 32-bit memory BAR. All DMA-capable backends enable bus
//! mastering through the PCI command register.
//! There is no support for 64-bit memory BARs, PCI bridges, or any other
//! device class; this is a teaching-scoped lookup, not a general PCI driver.
//!
//! One further, narrow addition: `find_capability`/`enable_msi` walk a
//! function's PCI capability list to enable Message Signaled Interrupts,
//! used by `hda.rs` (see `lapic.rs` and "MSI/MSI-X" in `AGENTS.md`).
//! MSI-X (a different, table-based capability, ID `0x11`) is not
//! implemented -- no current device needs it.

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

/// PCI class code for "serial bus controllers", subclass 0x03 for "USB",
/// prog-if 0x00 for the UHCI (USB 1.1) programming interface specifically
/// -- distinct from OHCI (`0x10`), EHCI (`0x20`), and xHCI (`0x30`), which
/// this project does not implement. Matching by class/subclass/prog-if
/// (like `find_serial_ports` below, not by vendor:device ID like every
/// NIC/audio backend) is correct here because UHCI is a standardized
/// programming interface many different vendors implement identically,
/// not one specific chip.
const CLASS_SERIAL_BUS: u8 = 0x0c;
const SUBCLASS_USB: u8 = 0x03;
const PROG_IF_UHCI: u8 = 0x00;
const PROG_IF_XHCI: u8 = 0x30;

const HEADER_TYPE_MULTIFUNCTION: u8 = 0x80;
const BAR0_OFFSET: u8 = 0x10;
const BAR_IS_IO_SPACE: u32 = 0b01;

const STATUS_OFFSET: u8 = 0x06;
const STATUS_CAPABILITIES_LIST: u16 = 1 << 4;
const CAPABILITIES_POINTER_OFFSET: u8 = 0x34;
/// PCI capability ID for Message Signaled Interrupts (`enable_msi`).
const CAPABILITY_ID_MSI: u8 = 0x05;
/// Bounds `find_capability`'s linked-list walk against a malformed or
/// looping list -- no real PCI function has ever needed anywhere near
/// this many capabilities.
const MAX_CAPABILITY_HOPS: u8 = 16;

fn config_address(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    0x8000_0000
        | (u32::from(bus) << 16)
        | (u32::from(device) << 11)
        | (u32::from(function) << 8)
        | u32::from(offset & 0xfc)
}

fn read_u32(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    unsafe {
        outl(
            CONFIG_ADDRESS,
            config_address(bus, device, function, offset),
        );
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
        outl(
            CONFIG_ADDRESS,
            config_address(bus, device, function, offset),
        );
        outl(CONFIG_DATA, value);
    }
}

/// Config space only accepts full 32-bit writes, so a 16-bit field
/// (everything the MSI capability's own registers below need) is written
/// as a read-modify-write of its containing dword, mirroring how
/// `read_u16` already extracts a 16-bit field by reading the dword and
/// shifting.
fn write_u16(bus: u8, device: u8, function: u8, offset: u8, value: u16) {
    let current = read_u32(bus, device, function, offset);
    let shift = u32::from(offset & 2) * 8;
    let mask = 0xffffu32 << shift;
    let updated = (current & !mask) | (u32::from(value) << shift);
    write_u32(bus, device, function, offset, updated);
}

/// Reads one dword of configuration space for the shell's `pci read`. Raw and
/// unfiltered: a debugging aid, not an interface any driver should use.
pub fn config_read32(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    read_u32(bus, device, function, offset)
}

/// Writes one dword of configuration space for the shell's `pci write`. Like
/// `config_read32` this is a debugging aid; a wrong write can disable a device
/// or the chipset, and a reset is then the only way back.
pub fn config_write32(bus: u8, device: u8, function: u8, offset: u8, value: u32) {
    write_u32(bus, device, function, offset, value);
}

/// The address of one PCI function, as found by `find_devices`. Enough to
/// re-read its BARs or reconfigure its command register without walking
/// the bus again.
#[derive(Clone, Copy)]
pub struct PciAddress {
    bus: u8,
    device: u8,
    function: u8,
}

pub const MAX_DEVICE_MATCHES: usize = 8;

impl PciAddress {
    /// Reads the PCI device identifier for this function.
    pub fn device_id(&self) -> u16 {
        read_u16(self.bus, self.device, self.function, 0x00)
    }

    /// Reads the revision ID (config offset `0x08`).
    pub fn revision(&self) -> u8 {
        read_u8(self.bus, self.device, self.function, 0x08)
    }

    /// Returns the legacy PCI interrupt line assigned by firmware, if this
    /// function has one. `0xff` means no usable legacy PIC line (for example,
    /// an MSI-only route), so callers must retain their polling fallback.
    pub fn interrupt_line(&self) -> Option<u8> {
        let line = read_u8(self.bus, self.device, self.function, 0x3c);
        (line != 0xff).then_some(line)
    }

    /// Reads an I/O BAR, returning its base address if it is I/O-mapped
    /// (`None` for memory-mapped BARs, which this project's port-I/O code
    /// cannot use).
    pub fn io_bar(&self, index: u8) -> Option<u16> {
        if index >= 6 {
            return None;
        }
        let offset = BAR0_OFFSET + index * 4;
        let bar = read_u32(self.bus, self.device, self.function, offset);
        if bar & BAR_IS_IO_SPACE == 0 {
            return None;
        }
        Some((bar & 0xffff_fffc) as u16)
    }

    /// Reads BAR0, returning its I/O base address if it is I/O-mapped.
    pub fn io_bar0(&self) -> Option<u16> {
        self.io_bar(0)
    }

    /// Assigns a fixed I/O port base to `index`'s BAR when firmware left
    /// it completely blank (reads back as literal `0` -- not even the
    /// `BAR_IS_IO_SPACE` type bit set) -- a real, confirmed condition on
    /// at least one physical machine's onboard AC'97 controller (see
    /// `ac97.rs`'s own doc comment), where BIOS apparently never bothers
    /// assigning resources to a function no boot-time driver needs.
    ///
    /// Follows the standard PCI BAR-sizing idiom: write all-1s, read back
    /// the resulting size mask, restore the BAR to `0`, then write the
    /// caller's requested base with the I/O-space type bit set and
    /// confirm it actually stuck. Refuses (`None`) rather than guessing
    /// further if the BAR already holds any nonzero value (never
    /// overwrite something firmware or hardware actually assigned or
    /// hardwired), if it doesn't behave like an I/O BAR at all, or if
    /// `requested_base` isn't aligned to the BAR's own required size.
    ///
    /// This kernel has no system-wide I/O port allocation table to
    /// consult, unlike a real OS -- `requested_base` is therefore a
    /// caller-chosen guess, not a verified-free address. Callers must
    /// pick a value deliberately far from every I/O range this project
    /// has ever observed a real chipset actually use (see `ac97.rs`'s own
    /// fallback constants for the reasoning behind its specific choice).
    /// Diagnostic-only: finds this function's PCI Power Management
    /// capability (ID `0x01`) if it has one, and returns its current
    /// power state (`PMCSR`'s low 2 bits: 0=D0/full power, 1=D1, 2=D2,
    /// 3=D3hot) -- useful for telling "firmware never assigned this BAR"
    /// apart from "this function is sitting in a low-power state where
    /// even a BAR write has no effect until it's brought back to D0",
    /// a real possibility for an onboard audio codec some BIOS setups
    /// let an operator disable entirely.
    pub fn debug_power_state(&self) -> Option<u8> {
        const CAPABILITY_ID_POWER_MANAGEMENT: u8 = 0x01;
        const PMCSR_OFFSET_FROM_CAP: u8 = 4;
        let cap = self.find_capability(CAPABILITY_ID_POWER_MANAGEMENT)?;
        let pmcsr = read_u16(self.bus, self.device, self.function, cap + PMCSR_OFFSET_FROM_CAP);
        Some((pmcsr & 0b11) as u8)
    }

    /// Forces this function into D0 (full power) via its PCI Power
    /// Management capability, if it has one. A real OS's PCI core does
    /// this unconditionally before any driver's own probe ever runs (Linux:
    /// `pci_set_power_state(pdev, PCI_D0)`); this kernel has no such core,
    /// so any driver relying on a memory BAR must do it explicitly instead
    /// of assuming BIOS already left the function in D0. Per the PCI Power
    /// Management spec, a function in D3hot does not respond to memory/IO
    /// space accesses at all -- on some chipsets (integrated
    /// southbridge/PCH functions especially) an attempted access while
    /// still in that state can hang the bus outright rather than cleanly
    /// completing with a master-abort, which is a plausible, real
    /// explanation for a hang that happens on real hardware but never
    /// under QEMU (which does not model PM D-states for its device
    /// models at all). Returns `true` if a D3hot-to-D0 transition was
    /// requested -- the caller must still wait out the PCI PM spec's
    /// required settle delay (typically several milliseconds) before
    /// touching this function's BARs -- or `false` if it has no PM
    /// capability (nothing to do) or was already in D0.
    pub fn force_power_state_d0(&self) -> bool {
        const CAPABILITY_ID_POWER_MANAGEMENT: u8 = 0x01;
        const PMCSR_OFFSET_FROM_CAP: u8 = 4;
        let Some(cap) = self.find_capability(CAPABILITY_ID_POWER_MANAGEMENT) else {
            return false;
        };
        let offset = cap + PMCSR_OFFSET_FROM_CAP;
        let pmcsr = read_u16(self.bus, self.device, self.function, offset);
        if pmcsr & 0b11 == 0 {
            return false;
        }
        write_u16(self.bus, self.device, self.function, offset, pmcsr & !0b11);
        true
    }

    /// Diagnostic-only: probes `index`'s BAR the same way `assign_io_bar`
    /// does (write all-1s, read back the size mask, restore the original
    /// value) but always returns the raw readback rather than trying to
    /// interpret it, for real-hardware debugging when `assign_io_bar`
    /// itself refuses. Safe to call on an already-assigned BAR too, since
    /// the original value is always restored.
    pub fn debug_probe_bar(&self, index: u8) -> Option<u32> {
        if index >= 6 {
            return None;
        }
        let offset = BAR0_OFFSET + index * 4;
        let original = read_u32(self.bus, self.device, self.function, offset);
        write_u32(self.bus, self.device, self.function, offset, 0xffff_ffff);
        let size_mask = read_u32(self.bus, self.device, self.function, offset);
        write_u32(self.bus, self.device, self.function, offset, original);
        Some(size_mask)
    }

    pub fn assign_io_bar(&self, index: u8, requested_base: u16) -> Option<u16> {
        if index >= 6 {
            return None;
        }
        let offset = BAR0_OFFSET + index * 4;
        let current = read_u32(self.bus, self.device, self.function, offset);
        if current != 0 {
            return None;
        }

        write_u32(self.bus, self.device, self.function, offset, 0xffff_ffff);
        let size_mask = read_u32(self.bus, self.device, self.function, offset);
        write_u32(self.bus, self.device, self.function, offset, 0);
        if size_mask & BAR_IS_IO_SPACE == 0 {
            return None;
        }
        let size = (!(size_mask & 0xffff_fffc)).wrapping_add(1) & 0xffff;
        if size == 0 || u32::from(requested_base) % size != 0 {
            return None;
        }

        write_u32(
            self.bus,
            self.device,
            self.function,
            offset,
            u32::from(requested_base) | BAR_IS_IO_SPACE,
        );
        let confirmed = read_u32(self.bus, self.device, self.function, offset);
        if confirmed & 0xffff_fffc != u32::from(requested_base) {
            return None;
        }
        Some(requested_base)
    }

    /// Reads a 32-bit memory BAR, returning its physical base address.
    /// 64-bit memory BARs are deliberately not handled yet because the
    /// kernel and its DMA allocator currently use 32-bit physical addresses.
    pub fn memory_bar(&self, index: u8) -> Option<u32> {
        if index >= 6 {
            return None;
        }
        let offset = BAR0_OFFSET + index * 4;
        let bar = read_u32(self.bus, self.device, self.function, offset);
        if bar & BAR_IS_IO_SPACE != 0 {
            return None;
        }
        let memory_type = (bar >> 1) & 0b11;
        if memory_type == 0b10 {
            return None;
        }
        let base = bar & 0xffff_fff0;
        (base != 0).then_some(base)
    }

    /// A memory BAR's physical base and size in bytes, accepting 64-bit BARs
    /// whose upper half is zero (xHCI controllers commonly advertise a 64-bit
    /// BAR even when it sits below 4 GiB; the kernel's `u32` physical addresses
    /// cannot use one that does not). The size comes from the usual probe --
    /// write all ones, read back the mask, restore -- done with memory decoding
    /// switched off in the command register, so the device never answers at a
    /// bogus address while the BAR holds the probe value.
    pub fn memory_bar_region(&self, index: u8) -> Option<(u32, u32)> {
        if index >= 6 {
            return None;
        }
        let offset = BAR0_OFFSET + index * 4;
        let original = read_u32(self.bus, self.device, self.function, offset);
        if original & BAR_IS_IO_SPACE != 0 {
            return None;
        }
        let is_64_bit = (original >> 1) & 0b11 == 0b10;
        if is_64_bit {
            if index >= 5 || read_u32(self.bus, self.device, self.function, offset + 4) != 0 {
                return None;
            }
        }
        let base = original & 0xffff_fff0;
        if base == 0 {
            return None;
        }
        let command = read_u32(self.bus, self.device, self.function, 0x04);
        write_u32(self.bus, self.device, self.function, 0x04, command & !0x7);
        write_u32(self.bus, self.device, self.function, offset, 0xffff_ffff);
        let mask = read_u32(self.bus, self.device, self.function, offset);
        write_u32(self.bus, self.device, self.function, offset, original);
        write_u32(self.bus, self.device, self.function, 0x04, command);
        let size = (!(mask & 0xffff_fff0)).wrapping_add(1);
        (size != 0).then_some((base, size))
    }

    /// Sets the I/O Space Enable (bit 0) and Bus Master Enable (bit 2)
    /// bits in the PCI command register (offset `0x04`), leaving every
    /// other bit as the firmware left it.
    pub fn enable_io_and_bus_master(&self) {
        const COMMAND_OFFSET: u8 = 0x04;
        const IO_SPACE_ENABLE: u32 = 1 << 0;
        const BUS_MASTER_ENABLE: u32 = 1 << 2;
        let command = read_u32(self.bus, self.device, self.function, COMMAND_OFFSET);
        write_u32(
            self.bus,
            self.device,
            self.function,
            COMMAND_OFFSET,
            command | IO_SPACE_ENABLE | BUS_MASTER_ENABLE,
        );
    }

    /// Sets Memory Space Enable (bit 1) and Bus Master Enable (bit 2).
    /// Intel e1000 exposes its device registers through a memory BAR, while
    /// its receive/transmit rings are still accessed by PCI bus mastering.
    pub fn enable_memory_and_bus_master(&self) {
        const COMMAND_OFFSET: u8 = 0x04;
        const MEMORY_SPACE_ENABLE: u32 = 1 << 1;
        const BUS_MASTER_ENABLE: u32 = 1 << 2;
        let command = read_u32(self.bus, self.device, self.function, COMMAND_OFFSET);
        write_u32(
            self.bus,
            self.device,
            self.function,
            COMMAND_OFFSET,
            command | MEMORY_SPACE_ENABLE | BUS_MASTER_ENABLE,
        );
    }

    /// Walks this function's PCI capability list (Status register bit 4,
    /// then the linked list starting at the Capabilities Pointer, offset
    /// `0x34`: each node is `[capability_id: u8][next_pointer: u8]`) for a
    /// capability matching `capability_id`. Returns the config-space
    /// offset of that capability's own structure (so its ID byte is at
    /// the returned offset, and its fields immediately follow), or `None`
    /// if this function has no capability list at all, or none matching.
    fn find_capability(&self, capability_id: u8) -> Option<u8> {
        let status = read_u16(self.bus, self.device, self.function, STATUS_OFFSET);
        if status & STATUS_CAPABILITIES_LIST == 0 {
            return None;
        }
        let mut pointer =
            read_u8(self.bus, self.device, self.function, CAPABILITIES_POINTER_OFFSET);
        for _ in 0..MAX_CAPABILITY_HOPS {
            if pointer == 0 {
                return None;
            }
            let id = read_u8(self.bus, self.device, self.function, pointer);
            if id == capability_id {
                return Some(pointer);
            }
            pointer = read_u8(self.bus, self.device, self.function, pointer + 1);
        }
        None
    }

    /// Enables Message Signaled Interrupts for this function, targeting
    /// `vector` on the local APIC identified by `destination_lapic_id`
    /// (see `lapic::id()`). Returns `None` -- with no config-space writes
    /// performed -- if this function has no MSI capability at all (an
    /// older device like ES1370, or any card exposing only legacy INTx);
    /// callers must keep their `interrupt_line()`/`register_pci_irq`
    /// fallback for that case, exactly as before this existed.
    ///
    /// Only the simplest case this project currently needs is
    /// implemented: one vector, a 32-bit message address. A 64-bit-
    /// capable device (Message Control bit 7) still works correctly with
    /// only the 32-bit address fields written -- every address this
    /// kernel ever targets (`0xfee00000`-range) fits in 32 bits regardless
    /// -- so its Message Upper Address register is simply zeroed rather
    /// than left holding whatever firmware put there. A device that
    /// *requires* (not just supports) the 64-bit fields isn't handled
    /// here. Per-vector masking (Message Control bit 8) and multi-vector
    /// allocation (bits 1-3) are left at their power-on defaults (masking
    /// off, one vector) -- no current consumer needs more, and MSI-X
    /// (capability ID `0x11`, a completely different, table-based
    /// mechanism) isn't implemented at all.
    pub fn enable_msi(&self, vector: u8, destination_lapic_id: u32) -> Option<()> {
        const MESSAGE_CONTROL_OFFSET: u8 = 0x02;
        const MESSAGE_ADDRESS_OFFSET: u8 = 0x04;
        const MESSAGE_UPPER_ADDRESS_OFFSET: u8 = 0x08;
        const MESSAGE_DATA_OFFSET_32BIT: u8 = 0x08;
        const MESSAGE_DATA_OFFSET_64BIT: u8 = 0x0c;
        const CONTROL_64BIT_CAPABLE: u16 = 1 << 7;
        const CONTROL_MSI_ENABLE: u16 = 1 << 0;
        // Largest offset any branch below writes to, relative to the
        // capability's own start; reject a (spec-violating) capability
        // sitting too close to the end of config space to safely address
        // rather than silently reading/writing past a 32-bit dword's
        // worth of unrelated bytes.
        const MAX_STRUCTURE_END: u8 = MESSAGE_DATA_OFFSET_64BIT + 2;

        let capability = self.find_capability(CAPABILITY_ID_MSI)?;
        if capability > u8::MAX - MAX_STRUCTURE_END {
            return None;
        }

        let control = read_u16(
            self.bus,
            self.device,
            self.function,
            capability + MESSAGE_CONTROL_OFFSET,
        );
        let data_offset = if control & CONTROL_64BIT_CAPABLE != 0 {
            write_u32(
                self.bus,
                self.device,
                self.function,
                capability + MESSAGE_UPPER_ADDRESS_OFFSET,
                0,
            );
            MESSAGE_DATA_OFFSET_64BIT
        } else {
            MESSAGE_DATA_OFFSET_32BIT
        };

        // Physical destination mode, no redirection hint: this single-core
        // kernel only ever has the one local APIC as a destination.
        let message_address = 0xfee0_0000u32 | (destination_lapic_id << 12);
        write_u32(
            self.bus,
            self.device,
            self.function,
            capability + MESSAGE_ADDRESS_OFFSET,
            message_address,
        );

        // Delivery mode 0 (fixed), vector in the low byte. MSI is always
        // edge-triggered per the PCI spec, so unlike a legacy PIC IRQ gate
        // there is no trigger-mode bit to set here at all.
        write_u16(
            self.bus,
            self.device,
            self.function,
            capability + data_offset,
            u16::from(vector),
        );

        write_u16(
            self.bus,
            self.device,
            self.function,
            capability + MESSAGE_CONTROL_OFFSET,
            control | CONTROL_MSI_ENABLE,
        );
        Some(())
    }

    /// Clears the MSI Enable bit, returning the function to its legacy INTx
    /// pin. Used when an MSI self-test shows the chipset never delivers the
    /// message, so a NIC can fall back to its legacy IRQ. `None` if there is
    /// no MSI capability.
    pub fn disable_msi(&self) -> Option<()> {
        const MESSAGE_CONTROL_OFFSET: u8 = 0x02;
        const CONTROL_MSI_ENABLE: u16 = 1 << 0;
        let capability = self.find_capability(CAPABILITY_ID_MSI)?;
        let control = read_u16(self.bus, self.device, self.function, capability + MESSAGE_CONTROL_OFFSET);
        write_u16(
            self.bus,
            self.device,
            self.function,
            capability + MESSAGE_CONTROL_OFFSET,
            control & !CONTROL_MSI_ENABLE,
        );
        Some(())
    }
}

/// Finds all PCI functions matching a vendor ID and one of its device IDs,
/// storing up to `out.len()` addresses in bus/device/function enumeration
/// order. This is intentionally bounded: the kernel has no heap, and the
/// supported NIC set is small enough for a fixed caller-owned array.
pub fn find_devices(
    vendor_id: u16,
    device_ids: &[u16],
    out: &mut [Option<PciAddress>],
) -> usize {
    let mut found = 0;
    for bus in 0..=255u8 {
        for device in 0..32u8 {
            let vendor = read_u16(bus, device, 0, 0x00);
            if vendor == 0xffff {
                continue;
            }

            let header_type = read_u8(bus, device, 0, 0x0e);
            let function_count = if header_type & HEADER_TYPE_MULTIFUNCTION != 0 {
                8
            } else {
                1
            };
            for function in 0..function_count {
                let function_vendor = if function == 0 {
                    vendor
                } else {
                    read_u16(bus, device, function, 0x00)
                };
                if function_vendor != vendor_id {
                    continue;
                }
                let function_device = read_u16(bus, device, function, 0x02);
                if device_ids.contains(&function_device) {
                    if found < out.len() {
                        out[found] = Some(PciAddress {
                            bus,
                            device,
                            function,
                        });
                    }
                    found += 1;
                    if found == out.len() {
                        return found;
                    }
                }
            }
        }
    }
    found
}

/// One PCI function's identifying configuration-space fields, as reported
/// by `scan_devices`. This is deliberately read-only/informational (unlike
/// `PciAddress`, which callers use to re-read BARs or flip command-register
/// bits) -- its only purpose is the `pci` shell command's device dump, used
/// to identify unknown hardware (e.g. reading off a device ID) before
/// writing a driver for it.
#[derive(Clone, Copy)]
pub struct PciDeviceInfo {
    pub bus: u8,
    pub device: u8,
    pub function: u8,
    pub vendor_id: u16,
    pub device_id: u16,
    pub class: u8,
    pub subclass: u8,
    pub prog_if: u8,
    pub header_type: u8,
    /// The raw BAR0 register value, undecoded: bit 0 distinguishes I/O-
    /// mapped (base in bits 31:2) from memory-mapped (base in bits 31:4,
    /// with bits 2:1 giving the memory type) space. Shown as a raw hex
    /// value rather than decoded, since a dump command's job is exposing
    /// what is actually on the bus, not guessing which BAR meaning a
    /// not-yet-identified device intends.
    pub bar0: u32,
    /// Whether this function advertises an MSI capability (ID `0x05`) at
    /// all -- not whether MSI is actually *enabled* for it right now (a
    /// driver may still be using its legacy `interrupt_line` even when
    /// this is `true`, e.g. because every MSI vector was already claimed
    /// by the time it initialized). See "MSI/MSI-X" in `AGENTS.md`.
    pub has_msi: bool,
    /// Revision ID (config offset `0x08`). Vendors reuse one device ID across
    /// silicon steppings -- Realtek's `10ec:8168` covers many chip versions --
    /// so this and `subsystem` help tell apart cards the ID alone cannot.
    pub revision: u8,
    /// Subsystem vendor (low 16 bits) and subsystem ID (high 16 bits), config
    /// offset `0x2c`: identifies the board maker's card/model. Only meaningful
    /// for header type 0; zero for bridges, whose `0x2c` is something else.
    pub subsystem: u32,
    /// Interrupt Line (config offset `0x3c`): the PIC input firmware says this
    /// function's interrupt is routed to (`0xff` = none). Whether the board
    /// really wires it there is exactly what `irqstat`'s PIC registers can show.
    pub interrupt_line: u8,
    /// Interrupt Pin (config offset `0x3d`): 0 = none, 1 = INTA#, 2 = INTB#...
    pub interrupt_pin: u8,
    /// PCI Command register (offset `0x04`). Bit 10 is "Interrupt Disable": a
    /// function with it set never asserts INTx, whatever its driver does.
    pub command: u16,
    /// PCI Status register (offset `0x06`). Bit 3 is "Interrupt Status": the
    /// function is asserting INTx right now -- a direct reading of whether a
    /// device is raising its interrupt line at all.
    pub status: u16,
}

/// Bounded to the largest device count this project has ever needed to
/// enumerate at once (a handful of onboard devices plus a multi-port add-on
/// card); like every other fixed-size PCI table here, this trades an
/// unlikely truncation on an exotic system with many devices for having no
/// heap at all.
pub const MAX_PCI_DEVICES: usize = 64;

/// Walks every PCI bus/device/function, recording each present function's
/// identifying fields into `out` in enumeration order. Stops once `out` is
/// full; returns the total number of functions found (which may exceed
/// `out.len()` if it was too small to hold them all, mirroring
/// `find_devices`/`find_serial_ports`'s own truncation behavior).
pub fn scan_devices(out: &mut [PciDeviceInfo]) -> usize {
    let mut found = 0;
    for bus in 0..=255u8 {
        for device in 0..32u8 {
            let vendor = read_u16(bus, device, 0, 0x00);
            if vendor == 0xffff {
                continue;
            }
            let header_type = read_u8(bus, device, 0, 0x0e);
            let function_count = if header_type & HEADER_TYPE_MULTIFUNCTION != 0 {
                8
            } else {
                1
            };
            for function in 0..function_count {
                let function_vendor = if function == 0 {
                    vendor
                } else {
                    read_u16(bus, device, function, 0x00)
                };
                if function_vendor == 0xffff {
                    continue;
                }
                let address = PciAddress {
                    bus,
                    device,
                    function,
                };
                let info = PciDeviceInfo {
                    bus,
                    device,
                    function,
                    vendor_id: function_vendor,
                    device_id: read_u16(bus, device, function, 0x02),
                    class: read_u8(bus, device, function, 0x0b),
                    subclass: read_u8(bus, device, function, 0x0a),
                    prog_if: read_u8(bus, device, function, 0x09),
                    header_type: read_u8(bus, device, function, 0x0e),
                    bar0: read_u32(bus, device, function, BAR0_OFFSET),
                    has_msi: address.find_capability(CAPABILITY_ID_MSI).is_some(),
                    revision: read_u8(bus, device, function, 0x08),
                    interrupt_line: read_u8(bus, device, function, 0x3c),
                    interrupt_pin: read_u8(bus, device, function, 0x3d),
                    command: read_u16(bus, device, function, 0x04),
                    status: read_u16(bus, device, function, 0x06),
                    subsystem: if read_u8(bus, device, function, 0x0e) & 0x7f == 0 {
                        read_u32(bus, device, function, 0x2c)
                    } else {
                        0
                    },
                };
                if let Some(slot) = out.get_mut(found) {
                    *slot = info;
                }
                found += 1;
                if found == out.len() {
                    return found;
                }
            }
        }
    }
    found
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
            let function_count = if header_type & HEADER_TYPE_MULTIFUNCTION != 0 {
                8
            } else {
                1
            };
            for function in 0..function_count {
                let function_vendor = if function == 0 {
                    vendor
                } else {
                    read_u16(bus, device, function, 0x00)
                };
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

/// Walks every PCI bus/device/function looking for UHCI (USB 1.1) host
/// controllers, matching by class/subclass/prog-if (`CLASS_SERIAL_BUS`/
/// `SUBCLASS_USB`/`PROG_IF_UHCI`) rather than vendor:device ID -- the
/// second such case in this file, alongside `find_serial_ports` above, and
/// for the identical reason: many different chipset vendors implement the
/// same standardized programming interface. Returns full `PciAddress`
/// values (unlike `find_serial_ports`'s bare I/O addresses), since a UHCI
/// controller -- unlike this project's port-I/O-only UART code -- also
/// needs `enable_io_and_bus_master()` (UHCI is DMA-capable, walking its
/// Frame List/Queue Heads/Transfer Descriptors by bus mastering) and
/// `interrupt_line()`.
pub fn find_usb_controllers(wanted_prog_if: u8, out: &mut [Option<PciAddress>]) -> usize {
    let mut count = 0;
    for bus in 0..=255u8 {
        for device in 0..32u8 {
            let vendor = read_u16(bus, device, 0, 0x00);
            if vendor == 0xffff {
                continue;
            }
            let header_type = read_u8(bus, device, 0, 0x0e);
            let function_count = if header_type & HEADER_TYPE_MULTIFUNCTION != 0 {
                8
            } else {
                1
            };
            for function in 0..function_count {
                let function_vendor = if function == 0 {
                    vendor
                } else {
                    read_u16(bus, device, function, 0x00)
                };
                if function_vendor == 0xffff {
                    continue;
                }
                let class = read_u8(bus, device, function, 0x0b);
                let subclass = read_u8(bus, device, function, 0x0a);
                let prog_if = read_u8(bus, device, function, 0x09);
                if class != CLASS_SERIAL_BUS || subclass != SUBCLASS_USB || prog_if != wanted_prog_if
                {
                    continue;
                }
                let Some(slot) = out.get_mut(count) else {
                    return count;
                };
                *slot = Some(PciAddress {
                    bus,
                    device,
                    function,
                });
                count += 1;
            }
        }
    }
    count
}

/// UHCI (USB 1.1) host controllers: see `find_usb_controllers`.
pub fn find_uhci_controllers(out: &mut [Option<PciAddress>]) -> usize {
    find_usb_controllers(PROG_IF_UHCI, out)
}

/// xHCI (USB 3.x) host controllers: see `find_usb_controllers`.
pub fn find_xhci_controllers(out: &mut [Option<PciAddress>]) -> usize {
    find_usb_controllers(PROG_IF_XHCI, out)
}
