//! PCI configuration space, through the legacy port mechanism (0xCF8 address, 0xCFC
//! data). It works on every x86 machine and needs no ACPI table; the memory-mapped
//! ECAM window (MCFG) is only needed for the extended space above offset 0xFF, which
//! nothing here reads yet.

use crate::cpu;

const ADDRESS_PORT: u16 = 0xcf8;
const DATA_PORT: u16 = 0xcfc;
pub const MAX_DEVICES: usize = 128;

#[derive(Clone, Copy)]
pub struct Device {
    pub bus: u8,
    pub device: u8,
    pub function: u8,
    pub vendor: u16,
    pub id: u16,
    pub class: u8,
    pub subclass: u8,
    pub interface: u8,
    pub revision: u8,
}

static mut DEVICES: [Option<Device>; MAX_DEVICES] = [None; MAX_DEVICES];
static mut COUNT: usize = 0;

fn address(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    0x8000_0000 | (u32::from(bus) << 16) | (u32::from(device) << 11) | (u32::from(function) << 8) | u32::from(offset & 0xfc)
}

pub fn read32(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    cpu::outl(ADDRESS_PORT, address(bus, device, function, offset));
    cpu::inl(DATA_PORT)
}

pub fn write32(bus: u8, device: u8, function: u8, offset: u8, value: u32) {
    cpu::outl(ADDRESS_PORT, address(bus, device, function, offset));
    cpu::outl(DATA_PORT, value);
}

impl Device {
    pub fn read32(&self, offset: u8) -> u32 {
        read32(self.bus, self.device, self.function, offset)
    }
    pub fn write32(&self, offset: u8, value: u32) {
        write32(self.bus, self.device, self.function, offset, value)
    }

    /// Turns on memory decoding and bus mastering (DMA), the two things a NIC driver
    /// needs from the command register (offset 4).
    pub fn enable_memory_and_bus_master(&self) {
        let command = self.read32(4);
        self.write32(4, (command & 0xffff) | 0x6);
    }

    /// Offset of capability `id` in the device's capability list, if it has one.
    pub fn find_capability(&self, id: u8) -> Option<u8> {
        if self.read32(4) >> 16 & (1 << 4) == 0 {
            return None; // status bit 4: no capability list
        }
        let mut offset = (self.read32(0x34) & 0xfc) as u8;
        for _ in 0..48 {
            if offset < 0x40 {
                return None;
            }
            let header = self.read32(offset);
            if header & 0xff == u32::from(id) {
                return Some(offset);
            }
            offset = ((header >> 8) & 0xfc) as u8;
        }
        None
    }

    /// Wakes a function a firmware left in power state D3 (it would not answer MMIO).
    pub fn force_power_state_d0(&self) {
        if let Some(capability) = self.find_capability(0x01) {
            let control = self.read32(capability + 4);
            if control & 3 != 0 {
                self.write32(capability + 4, control & !3);
                crate::apic::wait_ticks(1); // D3hot -> D0 needs 10 ms
            }
        }
    }

    /// Base address and size of memory BAR `index`, or None for an I/O or absent BAR.
    /// A 64-bit BAR takes two slots. Sizing writes all-ones and reads back which bits
    /// stuck, with decoding switched off while the BAR holds that bogus value.
    pub fn memory_bar(&self, index: u8) -> Option<(u64, u64)> {
        let offset = 0x10 + index * 4;
        let low = self.read32(offset);
        if low & 1 == 1 || low == 0 && index > 5 {
            return None;
        }
        let is_64_bit = (low >> 1) & 3 == 2;
        let command = self.read32(4);
        self.write32(4, command & 0xffff & !0x7);
        self.write32(offset, 0xffff_ffff);
        let size_low = self.read32(offset);
        self.write32(offset, low);
        let (base, mask) = if is_64_bit {
            let high = self.read32(offset + 4);
            self.write32(offset + 4, 0xffff_ffff);
            let size_high = self.read32(offset + 4);
            self.write32(offset + 4, high);
            (u64::from(low & !0xf) | u64::from(high) << 32, u64::from(size_low & !0xf) | u64::from(size_high) << 32)
        } else {
            (u64::from(low & !0xf), u64::from(size_low & !0xf) | 0xffff_ffff_0000_0000)
        };
        self.write32(4, command & 0xffff);
        if base == 0 || mask == 0 {
            return None;
        }
        Some((base, (!mask).wrapping_add(1)))
    }
}

/// Walks every bus/device/function once and remembers what answered.
pub fn scan() -> usize {
    unsafe {
        COUNT = 0;
        let devices = &mut *core::ptr::addr_of_mut!(DEVICES);
        for bus in 0..=255u8 {
            for device in 0..32u8 {
                if read32(bus, device, 0, 0) & 0xffff == 0xffff {
                    continue;
                }
                let header = (read32(bus, device, 0, 0x0c) >> 16) as u8;
                let functions = if header & 0x80 != 0 { 8 } else { 1 };
                for function in 0..functions {
                    let ids = read32(bus, device, function, 0);
                    if ids & 0xffff == 0xffff || COUNT == MAX_DEVICES {
                        continue;
                    }
                    let class = read32(bus, device, function, 8);
                    devices[COUNT] = Some(Device {
                        bus,
                        device,
                        function,
                        vendor: ids as u16,
                        id: (ids >> 16) as u16,
                        class: (class >> 24) as u8,
                        subclass: (class >> 16) as u8,
                        interface: (class >> 8) as u8,
                        revision: class as u8,
                    });
                    COUNT += 1;
                }
            }
        }
        COUNT
    }
}

pub fn devices() -> impl Iterator<Item = Device> {
    unsafe { core::slice::from_raw_parts(core::ptr::addr_of!(DEVICES) as *const Option<Device>, COUNT).iter().flatten().copied() }
}

pub fn find(vendor: u16, ids: &[u16]) -> Option<Device> {
    devices().find(|device| device.vendor == vendor && ids.contains(&device.id))
}

pub fn class_name(class: u8, subclass: u8) -> &'static str {
    match (class, subclass) {
        (0x01, 0x01) => "IDE controller",
        (0x01, 0x06) => "SATA controller",
        (0x01, 0x08) => "NVMe controller",
        (0x01, _) => "storage controller",
        (0x02, _) => "network controller",
        (0x03, _) => "display controller",
        (0x04, 0x03) => "HD audio",
        (0x04, _) => "multimedia",
        (0x06, 0x00) => "host bridge",
        (0x06, 0x01) => "ISA bridge",
        (0x06, 0x04) => "PCI bridge",
        (0x06, _) => "bridge",
        (0x0c, 0x03) => "USB controller",
        (0x0c, 0x05) => "SMBus",
        (0x0c, _) => "serial bus controller",
        _ => "other",
    }
}
