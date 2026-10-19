//! PCI configuration space through ECAM: each function's 4 KiB of config registers is memory at
//! `base + (bus << 20 | device << 15 | function << 12 | offset)`.
//!
//! Where the window is comes from the ACPI MCFG table (`init`): QEMU `virt` with 64-bit address space puts it at
//! 0x4010000000, the low 0x3f000000 only when "highmem" is off, so a hard-coded base would be wrong half the time.
//! The Pi 4's PCIe controller has no ECAM: it has its own config-access scheme, behind these same two functions. Only bus 0 is mapped (1 MiB): it holds every device on QEMU `virt`, and mapping 256 MiB in 4 KiB pages
//! would cost 2 MiB of page tables.
//! Reads of an absent function return all ones, as on x86.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

static ECAM: AtomicU64 = AtomicU64::new(0);
static MAPPED: AtomicBool = AtomicBool::new(false);

// Raspberry Pi 4 (BCM2711): the PCIe controller has no ECAM. Config space of the root port is at the controller base, every
// other function is reached by writing its bus/device/function to EXT_CFG_INDEX and reading the 4 KiB EXT_CFG_DATA window
// (register offsets from Linux's pcie-brcmstb driver). The firmware (edk2) has already trained the link and set up the
// windows, which `init` only reads: the outbound window maps a CPU address range onto PCI memory space (the VL805's BAR0 is
// a PCI address, 0xf8000000, seen by the CPU at 0x6_0000_0000), the inbound window maps PCI address 0 to RAM address 0.
const BRCM_BASE: u64 = 0xfd50_0000;
const BRCM_SIZE: u64 = 0xa000;
const BRCM_ID: u32 = 0x2711_14e4;
const EXT_CFG_DATA: u64 = 0x8000;
const EXT_CFG_INDEX: u64 = 0x9000;
const WIN0_PCI_LO: u64 = 0x400c;
const WIN0_PCI_HI: u64 = 0x4010;
const WIN0_BASE_LIMIT: u64 = 0x4070;
const WIN0_BASE_HI: u64 = 0x4080;
const WIN0_LIMIT_HI: u64 = 0x4084;
/// DMA pool for devices behind the controller, in frames: the VL805 is not cache-coherent with the CPU.
const DMA_POOL_FRAMES: u64 = 1024;

static BRCM: AtomicBool = AtomicBool::new(false);
static WINDOW_PCI: AtomicU64 = AtomicU64::new(0);
static WINDOW_CPU: AtomicU64 = AtomicU64::new(0);
static WINDOW_SIZE: AtomicU64 = AtomicU64::new(0);

fn brcm(offset: u64) -> *mut u32 {
    (BRCM_BASE + offset) as *mut u32
}

/// Reads the ECAM base from the ACPI MCFG table and remembers it; failing that, recognises the Pi 4's controller. Returns
/// the window base and what kind it is, or None when neither exists.
pub fn init(rsdp: u64) -> Option<(u64, &'static str)> {
    if let Some(base) = super::acpi::mcfg_base(rsdp) {
        ECAM.store(base, Ordering::Relaxed);
        return Some((base, "ECAM window (ACPI MCFG)"));
    }
    crate::paging::map_device(BRCM_BASE, BRCM_SIZE);
    if unsafe { brcm(0).read_volatile() } != BRCM_ID {
        return None;
    }
    let read = |offset| u64::from(unsafe { brcm(offset).read_volatile() });
    let (base_limit, base_hi, limit_hi) = (read(WIN0_BASE_LIMIT), read(WIN0_BASE_HI), read(WIN0_LIMIT_HI));
    let (first_mb, last_mb) = (base_hi << 12 | (base_limit >> 4 & 0xfff), limit_hi << 12 | (base_limit >> 20 & 0xfff));
    WINDOW_CPU.store(first_mb << 20, Ordering::Relaxed);
    WINDOW_SIZE.store((last_mb + 1).saturating_sub(first_mb) << 20, Ordering::Relaxed);
    WINDOW_PCI.store(read(WIN0_PCI_HI) << 32 | (read(WIN0_PCI_LO) & 0xfff0_0000), Ordering::Relaxed);
    BRCM.store(true, Ordering::Relaxed);
    // Memory a device behind it writes must not sit in CPU caches: carve a non-cacheable pool for DMA allocations.
    crate::memory::dma_pool(DMA_POOL_FRAMES);
    Some((BRCM_BASE, "Broadcom BCM2711 controller (firmware-trained link)"))
}

/// The CPU address of a PCI memory address: the outbound window's translation, identity outside it (and on ECAM systems).
pub fn cpu_address(pci: u64) -> u64 {
    let (window, size) = (WINDOW_PCI.load(Ordering::Relaxed), WINDOW_SIZE.load(Ordering::Relaxed));
    if BRCM.load(Ordering::Relaxed) && pci >= window && pci < window + size {
        pci - window + WINDOW_CPU.load(Ordering::Relaxed)
    } else {
        pci
    }
}

fn register(bus: u8, device: u8, function: u8, offset: u8) -> Option<*mut u32> {
    if BRCM.load(Ordering::Relaxed) {
        // Bus 0 holds only the root port; bus 1 only the one device below it.
        let offset = u64::from(offset & 0xfc);
        return match (bus, device, function) {
            (0, 0, 0) => Some(brcm(offset)),
            (1, 0, _) => {
                unsafe { brcm(EXT_CFG_INDEX).write_volatile(u32::from(bus) << 20 | u32::from(device) << 15 | u32::from(function) << 12) };
                Some(brcm(EXT_CFG_DATA + offset))
            }
            _ => None,
        };
    }
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
