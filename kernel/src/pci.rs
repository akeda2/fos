//! PCI devices: the bus scan and the per-device helpers (BARs, capabilities, MSI). The configuration-space access itself
//! is `crate::pci_config`, supplied by the architecture: on x86 the legacy port mechanism (0xCF8 address, 0xCFC data), which
//! works on every PC and needs no ACPI table; the memory-mapped ECAM window (MCFG) is only needed for the extended space
//! above offset 0xFF, which nothing here reads yet.

use crate::pci_config;

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
    #[allow(dead_code)]
    pub revision: u8,
}

static mut DEVICES: [Option<Device>; MAX_DEVICES] = [None; MAX_DEVICES];
static mut COUNT: usize = 0;

pub fn read32(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    pci_config::read32(bus, device, function, offset)
}

pub fn write32(bus: u8, device: u8, function: u8, offset: u8, value: u32) {
    pci_config::write32(bus, device, function, offset, value)
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
                crate::timer::wait_ticks(1); // D3hot -> D0 needs 10 ms
            }
        }
    }

    /// Programs the MSI capability for one vector aimed at local APIC `apic_id` and turns it
    /// on (and turns the legacy INTx pin off). False if the device has no MSI capability.
    ///
    /// Capability layout (dword offsets from the capability): +0 id/next/message control,
    /// +4 address low, +8 address high (64-bit capable devices only), then the 16-bit
    /// message data (the vector). Control bit 0 enables, bits 4-6 choose how many vectors
    /// (0 = one), bit 7 says the device takes a 64-bit address.
    #[cfg(target_arch = "x86_64")]
    pub fn enable_msi(&self, vector: u8, apic_id: u32) -> bool {
        let Some(capability) = self.find_capability(0x05) else { return false };
        let header = self.read32(capability);
        let control = (header >> 16) as u16;
        let data_offset = if control & (1 << 7) != 0 {
            self.write32(capability + 8, 0); // upper address bits
            capability + 12
        } else {
            capability + 8
        };
        // Physical destination mode, fixed delivery, to one local APIC.
        self.write32(capability + 4, 0xfee0_0000 | (apic_id << 12));
        let data = self.read32(data_offset);
        self.write32(data_offset, (data & 0xffff_0000) | u32::from(vector));
        // Enable with a single vector (clear the multiple-message-enable bits).
        let enabled = (control & !(0x7 << 4)) | 1;
        self.write32(capability, (header & 0xffff) | (u32::from(enabled) << 16));
        self.write32(4, (self.read32(4) & 0xffff) | (1 << 10)); // command bit 10: INTx disable
        true
    }

    /// Programs MSI-X table entry 0 for one vector and enables MSI-X (the function mask is held
    /// while the entry is written). MSI-X keeps its message table in a device BAR instead of
    /// config space: the capability gives the table's BAR and offset. Returns false if the
    /// device has no MSI-X capability or its table BAR cannot be mapped.
    #[cfg(target_arch = "x86_64")]
    pub fn enable_msix(&self, vector: u8, apic_id: u32) -> bool {
        let Some(capability) = self.find_capability(0x11) else { return false };
        let header = self.read32(capability);
        let control = (header >> 16) as u16;
        let table = self.read32(capability + 4);
        let (bar, offset) = ((table & 7) as u8, u64::from(table & !7));
        let Some((base, size)) = self.memory_bar(bar) else { return false };
        crate::paging::map_device(base, size);
        // Enable + function-mask first, so nothing fires while entry 0 is half written.
        self.write32(capability, (header & 0xffff) | (u32::from(control | (1 << 15) | (1 << 14)) << 16));
        let entry = (base + offset) as *mut u32;
        unsafe {
            entry.write_volatile(0xfee0_0000 | (apic_id << 12)); // message address low
            entry.add(1).write_volatile(0); // address high
            entry.add(2).write_volatile(u32::from(vector)); // message data
            entry.add(3).write_volatile(0); // vector control: bit 0 = masked, clear it
        }
        self.write32(capability, (header & 0xffff) | (u32::from((control | (1 << 15)) & !(1 << 14)) << 16));
        self.write32(4, (self.read32(4) & 0xffff) | (1 << 10)); // INTx off
        true
    }

    #[cfg(target_arch = "x86_64")]
    pub fn disable_msix(&self) {
        if let Some(capability) = self.find_capability(0x11) {
            let header = self.read32(capability);
            self.write32(capability, (header & 0xffff) | (u32::from((header >> 16) as u16 & !(1 << 15)) << 16));
            self.write32(4, (self.read32(4) & 0xffff) & !(1 << 10));
        }
    }

    /// The legacy interrupt line register (config 0x3c): what the firmware programmed, which
    /// on APIC systems is only a hint (0 and 0xff mean "none").
    pub fn interrupt_line(&self) -> u8 {
        self.read32(0x3c) as u8
    }

    /// Turns MSI off again (and re-enables the INTx pin).
    #[cfg(target_arch = "x86_64")]
    pub fn disable_msi(&self) {
        if let Some(capability) = self.find_capability(0x05) {
            let header = self.read32(capability);
            self.write32(capability, (header & 0xffff) | (u32::from((header >> 16) as u16 & !1) << 16));
            self.write32(4, (self.read32(4) & 0xffff) & !(1 << 10));
        }
    }

    /// The port base of I/O BAR `index`, or None for a memory or absent BAR.
    pub fn io_bar(&self, index: u8) -> Option<u16> {
        let low = self.read32(0x10 + index * 4);
        (low & 1 == 1 && low & 0xfffc != 0).then_some((low & 0xfffc) as u16)
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
