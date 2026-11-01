//! The platform services layer: everything a driver or the shell may ask of the machine underneath, in one place.
//!
//! Drivers say `crate::cpu`, `crate::paging`, `crate::timer`, `crate::msi`, `crate::pci_config` and `crate::memory`
//! (`main.rs` re-exports this module's names at the crate root), and `crate::println!` for output. Anything else under
//! `arch/` (APIC and I/O APIC routing, GIC, serial and PL011 registers, ACPI, descriptor tables, CPU power) is private to the
//! architecture and to the entry files; `tools/check_hal.py` lists the places that still reach past this layer and fails on
//! new ones, so the list only shrinks.
//!
//! Why: the plan for a second, larger kernel (`docs/big-kernel.md`) shares the drivers between two cores. The core
//! provides this surface (memory, mapping, time, port and register access, interrupt routing, logging); the drivers do not
//! know which core they run in. Today there is one provider per architecture and the names are plain re-exports, so nothing
//! changes at run time. Stage 1b turns this module into a trait implemented by each core.
//!
//! The surface, as counted in the drivers (2026-11-01):
//! - memory: `allocate`, `allocate_contiguous`, `free`, `dma_pool`, `reserve`, `stats`
//! - paging: `map_device`, `map_uncached`, `map_framebuffer`, `is_mapped`, `set_executable`
//! - time: `ticks`, `micros`, `wait_ticks`, `busy_wait_ms`, `TICK_HZ`
//! - cpu: port I/O, MSRs, `cpuid`, interrupt save/restore, `sleep`, `halt`
//! - interrupts: `msi::allocate`, `attach_intx`, `claim`, `release`

#[cfg(target_arch = "x86_64")]
pub use crate::arch::x86_64::{cpu, msi, paging, pci_config, timer};

#[cfg(target_arch = "aarch64")]
pub use crate::arch::aarch64::{cpu, msi, paging, pci_config, timer};

/// The local APIC id MSI messages are addressed to (x86-64): the CPU that takes device interrupts.
#[cfg(target_arch = "x86_64")]
pub fn interrupt_target() -> u32 {
    crate::apic::id()
}

/// The first GSI and one past the last that the I/O APIC serves, for probing legacy INTx candidates (x86-64).
#[cfg(target_arch = "x86_64")]
pub fn gsi_range() -> (u32, u32) {
    crate::ioapic::gsi_range()
}
