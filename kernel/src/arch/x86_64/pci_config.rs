//! PCI configuration space through the legacy port mechanism: write the address to 0xCF8, then read or write the dword at 0xCFC.

use super::cpu;

const ADDRESS_PORT: u16 = 0xcf8;
const DATA_PORT: u16 = 0xcfc;

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
