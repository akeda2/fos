//! PCI configuration space through the legacy port mechanism: the address to 0xCF8, then the dword at 0xCFC. The two ports
//! are a pair, so with several CPUs one access must not interleave with another: a lock covers each address/data exchange.

use crate::cpu;
use crate::sync::SpinLock;

const ADDRESS_PORT: u16 = 0xcf8;
const DATA_PORT: u16 = 0xcfc;

static LOCK: SpinLock<()> = SpinLock::new(());

fn address(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    0x8000_0000 | (u32::from(bus) << 16) | (u32::from(device) << 11) | (u32::from(function) << 8) | u32::from(offset & 0xfc)
}

pub fn read32(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    let _guard = LOCK.lock();
    cpu::outl(ADDRESS_PORT, address(bus, device, function, offset));
    cpu::inl(DATA_PORT)
}

pub fn write32(bus: u8, device: u8, function: u8, offset: u8, value: u32) {
    let _guard = LOCK.lock();
    cpu::outl(ADDRESS_PORT, address(bus, device, function, offset));
    cpu::outl(DATA_PORT, value);
}
