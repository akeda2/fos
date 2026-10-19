//! PCI configuration space through ECAM: each function's 4 KiB of config registers is memory at
//! `base + (bus << 20 | device << 15 | function << 12 | offset)`.
//!
//! Where the window is comes from the ACPI MCFG table (`init`): QEMU `virt` with 64-bit address space puts it at
//! 0x4010000000, the low 0x3f000000 only when "highmem" is off, so a hard-coded base would be wrong half the time.
//! The Pi 4's PCIe controller has no ECAM and needs its own config-access driver later, behind these same two
//! functions. Only bus 0 is mapped (1 MiB): it holds every device on QEMU `virt`, and mapping 256 MiB in 4 KiB pages
//! would cost 2 MiB of page tables.
//! Reads of an absent function return all ones, as on x86.

use core::sync::atomic::{AtomicBool, Ordering};

use core::sync::atomic::AtomicU64;

static ECAM: AtomicU64 = AtomicU64::new(0);
static MAPPED: AtomicBool = AtomicBool::new(false);

/// Reads the ECAM base from the ACPI MCFG table and remembers it. Returns that base, or None when the firmware
/// describes no ECAM.
pub fn init(rsdp: u64) -> Option<u64> {
    let base = super::acpi::mcfg_base(rsdp)?;
    ECAM.store(base, Ordering::Relaxed);
    Some(base)
}

fn register(bus: u8, device: u8, function: u8, offset: u8) -> Option<*mut u32> {
    let ecam = ECAM.load(Ordering::Relaxed);
    if bus != 0 || ecam == 0 {
        return None;
    }
    if !MAPPED.swap(true, Ordering::Relaxed) {
        crate::paging::map_device(ecam, 1 << 20);
    }
    Some((ecam + (u64::from(device) << 15 | u64::from(function) << 12 | u64::from(offset & 0xfc))) as *mut u32)
}

pub fn read32(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    match register(bus, device, function, offset) {
        Some(address) => unsafe { address.read_volatile() },
        None => 0xffff_ffff,
    }
}

pub fn write32(bus: u8, device: u8, function: u8, offset: u8, value: u32) {
    if let Some(address) = register(bus, device, function, offset) {
        unsafe { address.write_volatile(value) };
    }
}
