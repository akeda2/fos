//! Storage bring-up: start the SATA and NVMe drivers, register their disks, and find the boot disk without the firmware: it
//! is the disk holding the payload partition the loader booted from (GUID, start and size come in `BootInfo`). The FOSFS
//! data partition of that disk is then mounted through `kernel-core`. Same logic as the embedded kernel's `storage.rs`.

use core::fmt::Write;
use fosfor_abi::{BootInfo, FOSFS_TYPE_GUID, PAYLOAD_TYPE_GUID};
use kernel_core::{ahci, fosfs, gpt, nvme, volumes};

use crate::block::{self, Backend};

pub use kernel_core::volumes::{boot_volume, volume_labelled, volume_on, VolumeError};
pub use crate::slots::boot_slot;

/// First sector of the payload partition on the boot disk (slot metadata, then the kernel slots).
pub fn payload_start() -> Option<u64> {
    crate::slots::payload().map(|(_, start)| start)
}
pub use crate::debug::pet;

static mut BOOT_DISK: Option<usize> = None;

pub fn boot_disk() -> Option<usize> {
    unsafe { *core::ptr::addr_of!(BOOT_DISK) }
}

fn trimmed(model: &[u8]) -> &str {
    core::str::from_utf8(model).unwrap_or("?").trim()
}

/// "SATA c:p" or "NVMe c" without allocating.
struct Origin {
    bytes: [u8; 16],
    length: usize,
}

impl Origin {
    fn new(kind: &str, controller: usize, port: Option<u8>) -> Origin {
        let mut origin = Origin { bytes: [0; 16], length: 0 };
        let _ = match port {
            Some(port) => write!(origin, "{kind} {controller}:{port}"),
            None => write!(origin, "{kind} {controller}"),
        };
        origin
    }
    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.bytes[..self.length]).unwrap_or("")
    }
}

impl Write for Origin {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        let take = text.len().min(self.bytes.len() - self.length);
        self.bytes[self.length..self.length + take].copy_from_slice(&text.as_bytes()[..take]);
        self.length += take;
        Ok(())
    }
}

pub fn init(info: &BootInfo) {
    let sata = ahci::init_all(|drive, controller, port| {
        let origin = Origin::new("SATA", controller, Some(port));
        block::register(Backend::Ahci(drive), drive.sectors, drive.model, origin.as_str());
    });
    let nvme = nvme::init_all(|drive, controller| {
        let origin = Origin::new("NVMe", controller, None);
        block::register(Backend::Nvme(drive), drive.sectors, drive.model, origin.as_str());
    });
    if block::count() == 0 {
        println!("Storage: no disks found ({sata} SATA, {nvme} NVMe).");
        return;
    }
    find_boot_disk(info);
}

/// Looks for the disk with the payload partition among the registered disks and mounts its FOSFS volume. `main` calls it a
/// second time after `usb::init` when the machine boots from a USB stick (piano), which only exists once USB is up.
pub fn find_boot_disk(info: &BootInfo) {
    if boot_disk().is_some() || block::count() == 0 {
        return;
    }
    let (guid, start, sectors) = (info.payload_guid, info.payload_start, info.payload_sectors);
    let found = (0..block::count())
        .find(|&index| gpt::find(index, &PAYLOAD_TYPE_GUID, |p| p.unique_guid == guid && p.start == start && p.sectors == sectors).is_some());
    let found = found.map(block::move_to_front);
    for index in 0..block::count() {
        let disk = block::get(index).unwrap();
        println!("disk{}: {} MiB, {} ({})", index + 1, disk.sectors / 2048, trimmed(&disk.model), core::str::from_utf8(&disk.origin[..disk.origin_length]).unwrap_or("?"));
    }
    let Some(disk) = found else {
        println!("Boot disk: not found among the disks above (the payload partition GUID matches none); no FOSFS.");
        return;
    };
    println!("Boot disk: disk{} (its GPT holds the payload partition the loader booted from).", disk + 1);
    unsafe { BOOT_DISK = Some(disk) };
    match gpt::find(disk, &FOSFS_TYPE_GUID, |_| true) {
        Some(partition) => match fosfs::Volume::mount(disk, partition.start, partition.sectors) {
            Ok(volume) => {
                let mut files = 0;
                let _ = volume.for_each(|_| files += 1);
                println!("FOSFS: mounted on disk{} ({} MiB, {files} file(s)).", disk + 1, partition.sectors / 2048);
                volumes::set_boot_volume(volume);
            }
            Err(fosfs::Error::NotFormatted) => println!("FOSFS: the data partition holds no FOSFS volume."),
            Err(error) => println!("FOSFS: cannot read the data partition ({error:?})."),
        },
        None => println!("FOSFS: no FOSFS data partition on the boot disk."),
    }
}

/// Mounts the boot disk's FOSFS volume again (after `grow` changed its size).
pub fn remount_boot() {
    if let Some(disk) = boot_disk() {
        if let Some(partition) = gpt::find(disk, &FOSFS_TYPE_GUID, |_| true) {
            if let Ok(volume) = fosfs::Volume::mount(disk, partition.start, partition.sectors) {
                volumes::set_boot_volume(volume);
            }
        }
    }
}
