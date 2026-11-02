//! Kernel code shared by every core (docs/big-kernel.md, stage 1b). Modules reach the machine only through `fos_hal`; the
//! re-exports below let them keep saying `crate::block`, `crate::clock` and `crate::println!` as they did inside the kernel.

#![no_std]

pub use fos_hal::{block, clock, cpu, debug, memory, msi, paging, pci_config, print, println, timer};

/// Interrupt routing facts, where the shared code expects them (`crate::hal::...`).
pub mod hal {
    pub use fos_hal::{gsi_range, interrupt_target};
}

pub mod ahci;
pub mod e1000;
pub mod fatvol;
pub mod fosfs;
pub mod gpt;
pub mod lineedit;
pub mod mounts;
pub mod nic;
pub mod nvme;
pub mod pci;
pub mod rtl8139;
pub mod rtl8168;
pub mod tape_fs;
pub mod threecom;
pub mod volumes;
