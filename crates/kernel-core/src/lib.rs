//! Kernel code shared by every core (docs/big-kernel.md, stage 1b). Modules reach the machine only through `fos_hal`; the
//! re-exports below let them keep saying `crate::block`, `crate::clock` and `crate::println!` as they did inside the kernel.

#![no_std]

pub use fos_hal::{block, clock, cpu, debug, memory, paging, pci_config, print, println, tasks, timer};

/// Interrupt vectors, plus programming a PCI device to raise one (the message comes from the platform).
pub mod msi {
    pub use fos_hal::msi::*;

    pub fn enable_msi(device: &crate::pci::Device, vector: u8) -> bool {
        let (address, data) = fos_hal::msi::message(vector);
        device.enable_msi_message(address, data)
    }

    pub fn enable_msix(device: &crate::pci::Device, vector: u8) -> bool {
        let (address, data) = fos_hal::msi::message(vector);
        device.enable_msix_message(address, data)
    }
}

/// Interrupt routing facts, where the shared code expects them (`crate::hal::...`).
pub mod hal {
    pub use fos_hal::{gsi_range, interrupt_target};
}

pub mod ahci;
pub mod audio;
pub mod e1000;
pub mod fatvol;
pub mod fosfs;
pub mod gpt;
pub mod i2s;
pub mod lineedit;
pub mod loopback;
pub mod mounts;
pub mod nic;
pub mod nvme;
pub mod pci;
pub mod rtl8139;
pub mod rtl8168;
pub mod tape_fs;
pub mod threecom;
pub mod usb_class;
pub mod usb_audio;
pub mod usb_hub;
pub mod usb_storage;
pub mod volumes;
pub mod xhci;
#[cfg(target_arch = "x86_64")]
pub mod ehci;
#[cfg(target_arch = "x86_64")]
pub mod hda;
#[cfg(not(target_arch = "x86_64"))]
#[path = "hda_none.rs"]
pub mod hda;
#[cfg(not(target_arch = "x86_64"))]
#[path = "ehci_none.rs"]
pub mod ehci;
