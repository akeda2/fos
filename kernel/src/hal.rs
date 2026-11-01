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

/// True on a Raspberry Pi 4, recognised by its GIC-400 distributor address (aarch64).
#[cfg(target_arch = "aarch64")]
pub fn is_pi4() -> bool {
    crate::arch::aarch64::gic::bases().0 == 0xff84_1000
}

/// Routes interrupt `intid` to `handler` (level or edge triggered) on the boot CPU (aarch64).
#[cfg(target_arch = "aarch64")]
pub fn register_irq(intid: u32, level: bool, handler: fn(u32)) {
    crate::arch::aarch64::gic::register(intid, level, handler)
}

/// Interrupts taken so far, for drivers that wait for one (aarch64).
#[cfg(target_arch = "aarch64")]
pub fn irq_ticks() -> u64 {
    crate::arch::aarch64::gic::irq_ticks()
}

/// `println!`: formats the text and hands each piece to every console sink the core listed in `CONSOLE_SINKS` (entry
/// file), in order. Drivers only ever call this (through `println!`); they never know where the text ends up.
pub fn print(arguments: core::fmt::Arguments) {
    use core::fmt::Write;
    struct Sinks;
    impl Write for Sinks {
        fn write_str(&mut self, text: &str) -> core::fmt::Result {
            for sink in crate::CONSOLE_SINKS.iter() {
                sink(text);
            }
            Ok(())
        }
    }
    let _ = Sinks.write_fmt(arguments);
}
