//! The CPU-specific half of the server kernel. Each architecture offers the same hooks (`halt`, `early`, `init_memory`,
//! `init_interrupts`, `init_machine`, `wait_for_interrupt`, `ticks`, ...) and modules under their short names
//! (`crate::cpu`, `crate::apic` or `crate::gic`, ...); everything outside this directory is shared.

#[cfg(target_arch = "x86_64")]
mod x86_64;
#[cfg(target_arch = "x86_64")]
pub use x86_64::*;

#[cfg(target_arch = "aarch64")]
mod aarch64;
#[cfg(target_arch = "aarch64")]
pub use aarch64::*;
