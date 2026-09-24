//! Minimal local APIC bring-up, used only to receive MSI-delivered
//! interrupts (see `pci::PciAddress::enable_msi`/`interrupts::register_msi_irq`).
//!
//! This is a deliberately narrow slice of what a local APIC can do,
//! layered *alongside* the existing 8259 PIC (`interrupts.rs`), never
//! replacing it: every legacy PIC-routed device (RTL8139, e1000, i350,
//! the NE2000-compatible clone, threecom, ES1370) keeps working through
//! `register_pci_irq` exactly as before this module existed. MSI itself
//! needs no I/O-APIC at all (it bypasses I/O-APIC redirection, delivered
//! straight to a vector this local APIC hands the CPU core), so none
//! exists here or anywhere else in this kernel. This module also never
//! programs the Local Vector Table's LINT0/LINT1 entries (which is how a
//! local APIC can additionally relay legacy-style INTx interrupts in
//! "virtual wire" mode) -- those stay masked/as firmware left them, so
//! the PIC retains full, independent control of every INTx line.

use crate::arch::x86::{rdmsr, wrmsr};
use crate::paging;

/// `IA32_APIC_BASE`: bits 12-35 give the LAPIC's physical MMIO base
/// (architecturally `0xFEE00000` unless firmware relocated it, which this
/// project never does and does not expect), bit 11 is the APIC Global
/// Enable flag.
const IA32_APIC_BASE_MSR: u32 = 0x1b;
const APIC_GLOBAL_ENABLE: u64 = 1 << 11;
const APIC_BASE_ADDRESS_MASK: u64 = 0xffff_f000;

/// The LAPIC's whole register window is one 4 KiB MMIO page, regardless of
/// how few registers this module actually touches.
const MMIO_BYTES: u32 = 0x1000;

const REG_ID: u32 = 0x20;
const REG_EOI: u32 = 0xb0;
const REG_SPURIOUS_VECTOR: u32 = 0xf0;
const SPURIOUS_SOFTWARE_ENABLE: u32 = 1 << 8;

/// Chosen to sit just past `interrupts::MSI_VECTOR_BASE`'s range and clear
/// of the PIC's own spurious vectors (`SPURIOUS_MASTER_VECTOR`/
/// `SPURIOUS_SLAVE_VECTOR`, `0x27`/`0x2f`) -- see `interrupts.rs` for the
/// full vector map. A genuine spurious LAPIC interrupt (the CPU vectors
/// here only when an interrupt was withdrawn before it could be
/// dispatched to a real handler) needs no EOI at all, unlike the PIC's
/// IRQ7/IRQ15 spurious handling, which still has to check the in-service
/// register first.
pub const SPURIOUS_VECTOR: u8 = 0x60;

static mut MMIO_BASE: u32 = 0;
static mut AVAILABLE: bool = false;

fn read_reg(offset: u32) -> u32 {
    unsafe { core::ptr::read_volatile((MMIO_BASE + offset) as *const u32) }
}

fn write_reg(offset: u32, value: u32) {
    unsafe { core::ptr::write_volatile((MMIO_BASE + offset) as *mut u32, value) }
}

/// CPUID.1:EDX bit 9 -- reuses `cpu::features()` rather than issuing a new
/// CPUID call, exactly like every other CPUID-gated feature in this
/// kernel (see `longmode64_shim.S`'s own leaf 0x80000001 check for the
/// long-mode equivalent).
fn cpu_reports_apic() -> bool {
    let (edx, _ecx) = crate::cpu::features();
    edx & (1 << 9) != 0
}

/// Detects and maps the local APIC, and confirms it is enabled. Safe to
/// call more than once (later calls just report the cached result); must
/// run before any call to `id()`/`send_eoi()`/`interrupts::register_msi_irq`.
/// Returns `false` (with no MMIO mapped and no register writes performed)
/// if this CPU reports no local APIC at all -- every real chipset since
/// the Pentium era has one, and so does every CPU model QEMU emulates
/// (even single-core, even the default `i440fx` machine type), so this is
/// a defensive check rather than an expected failure path.
pub fn initialize() -> bool {
    if unsafe { AVAILABLE } {
        return true;
    }
    if !cpu_reports_apic() {
        return false;
    }

    let apic_base_msr = unsafe { rdmsr(IA32_APIC_BASE_MSR) };
    let base = (apic_base_msr & APIC_BASE_ADDRESS_MASK) as u32;
    if paging::map_mmio(base, MMIO_BYTES).is_err() {
        return false;
    }
    unsafe {
        MMIO_BASE = base;
    }

    // QEMU (and every real chipset) already sets the Global Enable bit at
    // reset; set it defensively rather than assume that, since nothing
    // below works if it's clear.
    if apic_base_msr & APIC_GLOBAL_ENABLE == 0 {
        unsafe {
            wrmsr(IA32_APIC_BASE_MSR, apic_base_msr | APIC_GLOBAL_ENABLE);
        }
    }

    write_reg(
        REG_SPURIOUS_VECTOR,
        u32::from(SPURIOUS_VECTOR) | SPURIOUS_SOFTWARE_ENABLE,
    );

    unsafe {
        AVAILABLE = true;
    }
    true
}

/// Whether `initialize()` has already found and mapped a working local
/// APIC. Callers (e.g. `hda.rs`, deciding whether to attempt MSI at all)
/// should call `initialize()` first -- this only reports its result.
pub fn is_available() -> bool {
    unsafe { AVAILABLE }
}

/// This local APIC's own ID -- the only "destination" an MSI message ever
/// needs to name on a single-core machine like this one. Reads the ID
/// register's bits 24-31; no APIC-ID discovery infrastructure beyond this
/// one register is needed for that reason.
pub fn id() -> u32 {
    read_reg(REG_ID) >> 24
}

/// Acknowledges an MSI-delivered interrupt. This is a genuinely different
/// mechanism from the PIC's own `send_eoi` (`interrupts.rs`) -- writing
/// any value (conventionally 0) to this register, rather than an `outb`
/// command sequence -- and the two must never be mixed: an MSI-delivered
/// interrupt is never also PIC-routed, so its handler must call this and
/// only this.
pub fn send_eoi() {
    write_reg(REG_EOI, 0);
}
