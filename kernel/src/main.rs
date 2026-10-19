//! fos v2 kernel, milestone M3: PCI, an e1000 NIC and the network terminal (on top of
//! M2, memory and interrupts).
//!
//! On top of M1 (loader handoff, own GDT, serial and framebuffer console) the kernel
//! now owns the machine's memory and its interrupts:
//!
//! * a physical page allocator built from the firmware's memory map;
//! * its own page tables with NX, read-only text and unmapped guard pages;
//! * a TSS-backed IDT, so a fault is reported in words (and a stack overflow, which
//!   takes down the normal stack, still gets a clean report on a stack of its own);
//! * ACPI tables, the local APIC and a calibrated 100 Hz timer tick;
//! * a clean power-off through the UEFI runtime services.
//!
//! See docs/design.md, section 11.
#![no_std]
#![no_main]

// `println!` goes to `print` (defined by the entry file of each architecture).
#[macro_export]
macro_rules! println {
    () => { $crate::print(format_args!("\n")) };
    ($($argument:tt)*) => { $crate::print(format_args!("{}\n", format_args!($($argument)*))) };
}

// One crate root, two entry files: each is spliced in here, so `crate::println!`, `crate::print` and the module paths
// look the same to every driver. The x86-64 file is the whole kernel as it was; the aarch64 one is the Raspberry Pi
// port, growing module by module (docs/aarch64-plan.md).
#[cfg(target_arch = "x86_64")]
include!("main_x86_64.rs");
#[cfg(target_arch = "aarch64")]
include!("main_aarch64.rs");
#[cfg(target_arch = "aarch64")]
pub use arch::aarch64::timer;
#[cfg(target_arch = "aarch64")]
pub use arch::aarch64::{cpu, paging};
