//! Message Signaled Interrupts on aarch64: same interface as the x86 `msi` module (`allocate`, `count`, `release`, and
//! `enable_msi`/`enable_msix` to program a device), two ways to deliver the message:
//!
//! - **Raspberry Pi 4**: the PCIe controller's own MSI block. A device writes 0x6540 | n to the target address
//!   0xF_FFFF_FFFC; the controller (BAR config at 0x4044/0x4048, data match at 0x404c, offsets from Linux's pcie-brcmstb)
//!   latches bit n in its MSI_INTR2 status (0x4500) and raises one GIC SPI, INTID 180 (SPI 148), for all 32 of them.
//! - **QEMU `virt`**: the GICv2m frame at 0x0802_0000. A device writes the SPI number to SETSPI_NS (+0x40); each vector
//!   is its own edge-triggered SPI from the range the frame's TYPER register (+0x08) reports.
//!
//! Handlers run in IRQ context and must only acknowledge the device and note that work is pending.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::{gic, paging, pci};

pub const COUNT: usize = 16;

const BRCM_BASE: u64 = 0xfd50_0000;
const BRCM_MSI_BAR_LO: u64 = 0x4044;
const BRCM_MSI_BAR_HI: u64 = 0x4048;
const BRCM_MSI_DATA: u64 = 0x404c;
const BRCM_MSI_INTR2: u64 = 0x4500;
const INTR2_STATUS: u64 = 0x0;
const INTR2_CLEAR: u64 = 0x8;
const INTR2_MASK_CLEAR: u64 = 0x14;
/// Above 4 GiB: the firmware's inbound window covers the whole first 4 GiB, and a message inside it would be taken for
/// a memory write instead of reaching the MSI block (Linux picks this address when its window is that large).
const BRCM_TARGET: u64 = 0xf_ffff_fffc;
const BRCM_DATA_MATCH: u32 = 0x6540;
const BRCM_SPI_INTID: u32 = 32 + 148;
const PI4_GIC_DISTRIBUTOR: u64 = 0xff84_1000;

const V2M_BASE: u64 = 0x0802_0000;
const V2M_TYPER: u64 = 0x008;
const V2M_SETSPI: u64 = 0x040;

#[derive(Clone, Copy, PartialEq)]
enum Backend {
    None,
    Brcm,
    V2m,
}

static mut BACKEND: Backend = Backend::None;
/// First SPI of the GICv2m range, and how many it has.
static V2M_FIRST: AtomicU32 = AtomicU32::new(0);
static V2M_COUNT: AtomicU32 = AtomicU32::new(0);
static mut HANDLERS: [Option<fn(usize)>; COUNT] = [None; COUNT];
static mut CONTEXTS: [usize; COUNT] = [0; COUNT];
static COUNTS: [AtomicU64; COUNT] = [const { AtomicU64::new(0) }; COUNT];

fn backend() -> Backend {
    unsafe { *core::ptr::addr_of!(BACKEND) }
}

fn brcm(offset: u64) -> *mut u32 {
    (BRCM_BASE + offset) as *mut u32
}

/// Finds the MSI controller this machine has and programs it. Call after the GIC and PCI config are up.
pub fn init() -> &'static str {
    if gic::bases().0 == PI4_GIC_DISTRIBUTOR {
        paging::map_device(BRCM_BASE, 0xa000);
        unsafe {
            brcm(BRCM_MSI_BAR_LO).write_volatile(BRCM_TARGET as u32 | 1); // bit 0: enable, size field 0 = 4 KiB window
            brcm(BRCM_MSI_BAR_HI).write_volatile((BRCM_TARGET >> 32) as u32);
            brcm(BRCM_MSI_DATA).write_volatile(0xffe0_0000 | BRCM_DATA_MATCH); // match data & 0xffe0 == 0x6540
            brcm(BRCM_MSI_INTR2 + INTR2_CLEAR).write_volatile(0xffff_ffff);
            brcm(BRCM_MSI_INTR2 + INTR2_MASK_CLEAR).write_volatile(0xffff_ffff);
            BACKEND = Backend::Brcm;
        }
        gic::register(BRCM_SPI_INTID, true, brcm_interrupt);
        return "BCM2711 PCIe MSI block (SPI 148)";
    }
    paging::map_device(V2M_BASE, 0x1000);
    let typer = unsafe { ((V2M_BASE + V2M_TYPER) as *const u32).read_volatile() };
    let (first, count) = (typer >> 16 & 0x3ff, typer & 0x3ff);
    if count == 0 || first < 32 {
        return "none";
    }
    V2M_FIRST.store(first, Ordering::Relaxed);
    V2M_COUNT.store(count.min(COUNT as u32), Ordering::Relaxed);
    unsafe { BACKEND = Backend::V2m };
    "GICv2m frame"
}

/// The 32 MSIs of the Pi's controller share one GIC line: clear what is latched, then run each handler.
fn brcm_interrupt(_intid: u32) {
    let status = unsafe { brcm(BRCM_MSI_INTR2 + INTR2_STATUS).read_volatile() };
    unsafe { brcm(BRCM_MSI_INTR2 + INTR2_CLEAR).write_volatile(status) };
    for slot in 0..COUNT {
        if status & (1 << slot) != 0 {
            fire(slot);
        }
    }
}

fn v2m_interrupt(intid: u32) {
    fire((intid - V2M_FIRST.load(Ordering::Relaxed)) as usize);
}

fn fire(slot: usize) {
    COUNTS[slot].fetch_add(1, Ordering::Relaxed);
    if let Some(handler) = unsafe { (*core::ptr::addr_of!(HANDLERS))[slot] } {
        handler(unsafe { CONTEXTS[slot] });
    }
}

/// Claims a free vector for `handler(context)`; None when the machine has no MSI controller or none is free.
pub fn allocate(handler: fn(usize), context: usize) -> Option<u8> {
    let limit = match backend() {
        Backend::None => return None,
        Backend::Brcm => COUNT,
        Backend::V2m => V2M_COUNT.load(Ordering::Relaxed) as usize,
    };
    unsafe {
        let handlers = &mut *core::ptr::addr_of_mut!(HANDLERS);
        let slot = handlers[..limit].iter().position(Option::is_none)?;
        handlers[slot] = Some(handler);
        CONTEXTS[slot] = context;
        COUNTS[slot].store(0, Ordering::Relaxed);
        if backend() == Backend::V2m {
            gic::register(V2M_FIRST.load(Ordering::Relaxed) + slot as u32, false, v2m_interrupt);
        }
        Some(slot as u8)
    }
}

pub fn release(vector: u8) {
    unsafe { (*core::ptr::addr_of_mut!(HANDLERS))[usize::from(vector)] = None };
}

/// How many messages arrived on `vector`.
pub fn count(vector: u8) -> u64 {
    COUNTS[usize::from(vector)].load(Ordering::Relaxed)
}

/// The (address, data) a device must write to raise `vector`.
fn message(vector: u8) -> (u64, u32) {
    match backend() {
        Backend::V2m => (V2M_BASE + V2M_SETSPI, V2M_FIRST.load(Ordering::Relaxed) + u32::from(vector)),
        _ => (BRCM_TARGET, BRCM_DATA_MATCH | u32::from(vector)),
    }
}

pub fn enable_msi(device: &pci::Device, vector: u8) -> bool {
    let (address, data) = message(vector);
    device.enable_msi_message(address, data)
}

pub fn enable_msix(device: &pci::Device, vector: u8) -> bool {
    let (address, data) = message(vector);
    device.enable_msix_message(address, data)
}
