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

fn table_at<T: Copy>(address: u64, offset: u64) -> T {
    unsafe { core::ptr::read_unaligned((address + offset) as *const T) }
}

/// Finds the MCFG table from the ACPI RSDP (through the XSDT) and remembers the ECAM base of its first segment.
/// Returns that base, or None when the firmware describes no ECAM.
pub fn init(rsdp: u64) -> Option<u64> {
    if rsdp == 0 || table_at::<[u8; 8]>(rsdp, 0) != *b"RSD PTR " || table_at::<u8>(rsdp, 15) < 2 {
        return None;
    }
    let xsdt: u64 = table_at(rsdp, 24);
    let length: u32 = table_at(xsdt, 4);
    for entry in 0..(u64::from(length).saturating_sub(36) / 8) {
        let table: u64 = table_at(xsdt, 36 + entry * 8);
        if table_at::<[u8; 4]>(table, 0) == *b"MCFG" && table_at::<u32>(table, 4) >= 60 {
            let base: u64 = table_at(table, 44);
            ECAM.store(base, Ordering::Relaxed);
            return Some(base);
        }
    }
    None
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
