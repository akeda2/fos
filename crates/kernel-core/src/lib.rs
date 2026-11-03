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
pub mod ext4vol;
pub mod fatvol;
pub mod fosfs;
pub mod slot_report;
pub mod tapesync_client;
pub mod gpio_client;
pub mod gpt;
pub mod i2s;
pub mod line;
pub mod lineedit;
pub mod loopback;
pub mod mounts;
pub mod nic;
pub mod nicctl;
#[cfg(target_arch = "x86_64")]
pub mod nicpark;
pub mod nvme;
pub mod pci;
pub mod random;
pub mod rtl8139;
pub mod rtl8168;
pub mod screen;
pub mod tape_fs;
pub mod tape_vfs;
pub mod threecom;
pub mod usb_class;
pub mod usb_midi;
pub mod usb_pointer;
pub mod usb_serial;
pub mod usb_serial_pl2303;
pub mod usb_serial_ti;
pub mod usb_audio;
pub mod usb_hub;
pub mod usb_env;
pub mod usb_storage;
pub mod volumes;
pub mod xhci;
pub mod netdump;
#[cfg(target_arch = "x86_64")]
pub mod gpu;
#[cfg(target_arch = "x86_64")]
pub mod serialports;
#[cfg(target_arch = "x86_64")]
pub mod uart;
#[cfg(target_arch = "aarch64")]
pub mod virtio_blk;
#[cfg(target_arch = "x86_64")]
pub mod ohci;
#[cfg(target_arch = "x86_64")]
pub mod usb_hid;
#[cfg(target_arch = "aarch64")]
pub mod usb_kbd;
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
