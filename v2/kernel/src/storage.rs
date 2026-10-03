//! Storage bring-up: start the disk drivers, register their disks, and find *the boot
//! disk* without the firmware. The loader passed the GUID, start and size of the
//! payload partition it booted from (BootInfo); the kernel scans every disk its
//! drivers found for that partition, then mounts the FOSFS data partition on the same
//! disk. Boot-disk discovery is what lets a USB stick or a SATA disk be the root
//! filesystem without any BIOS help.

use fos_v2_abi::{BootInfo, FOSFS_TYPE_GUID, PAYLOAD_TYPE_GUID};

use crate::block::{self, Backend};
use crate::fosfs::{self, Volume};
use crate::{ahci, gpt, nvme};

static mut BOOT_VOLUME: Option<Volume> = None;
static mut BOOT_DISK: Option<usize> = None;
static mut PAYLOAD_START: Option<u64> = None;

pub fn boot_volume() -> Option<Volume> {
    unsafe { *core::ptr::addr_of!(BOOT_VOLUME) }
}

/// First sector of the payload partition on the boot disk (where the slot metadata and
/// the kernel slots live).
pub fn payload_start() -> Option<u64> {
    unsafe { *core::ptr::addr_of!(PAYLOAD_START) }
}

pub fn boot_disk() -> Option<usize> {
    unsafe { *core::ptr::addr_of!(BOOT_DISK) }
}

fn trimmed(model: &[u8]) -> &str {
    core::str::from_utf8(model).unwrap_or("?").trim()
}

pub fn init(info: &BootInfo) {
    let sata = ahci::init_all(|drive, controller, port| {
        let origin = format_origin("SATA", controller, Some(port));
        block::register(Backend::Ahci(drive), drive.sectors, drive.model, origin.as_str());
    });
    let nvme = nvme::init_all(|drive, controller| {
        let origin = format_origin("NVMe", controller, None);
        block::register(Backend::Nvme(drive), drive.sectors, drive.model, origin.as_str());
    });
    for index in 0..block::count() {
        let disk = block::get(index).unwrap();
        crate::println!(
            "disk{}: {} MiB, {} ({})",
            index + 1,
            disk.sectors / 2048,
            trimmed(&disk.model),
            core::str::from_utf8(&disk.origin[..disk.origin_length]).unwrap_or("?")
        );
    }
    if block::count() == 0 {
        crate::println!("Storage: no disks found by the drivers this kernel has ({sata} SATA, {nvme} NVMe).");
        return;
    }

    // The boot disk is the one holding the payload partition the loader read.
    let mut found = None;
    for index in 0..block::count() {
        let hit = gpt::find(index, &PAYLOAD_TYPE_GUID, |p| p.unique_guid == info.payload_guid && p.start == info.payload_start && p.sectors == info.payload_sectors);
        if hit.is_some() {
            found = Some(index);
            break;
        }
    }
    let Some(disk) = found else {
        crate::println!("Boot disk: not found among the disks above (the payload partition GUID matches none); no FOSFS.");
        return;
    };
    crate::println!("Boot disk: disk{} (its GPT holds the payload partition the loader booted from).", disk + 1);
    unsafe {
        BOOT_DISK = Some(disk);
        PAYLOAD_START = Some(info.payload_start);
    }
    match gpt::find(disk, &FOSFS_TYPE_GUID, |_| true) {
        Some(partition) => match Volume::mount(disk, partition.start, partition.sectors) {
            Ok(volume) => {
                let mut files = 0;
                let _ = volume.for_each(|_| files += 1);
                crate::println!("FOSFS: mounted on disk{} ({} MiB, {files} file(s)).", disk + 1, partition.sectors / 2048);
                unsafe { BOOT_VOLUME = Some(volume) };
            }
            Err(fosfs::Error::NotFormatted) => crate::println!("FOSFS: the data partition holds no FOSFS v3 volume."),
            Err(error) => crate::println!("FOSFS: cannot read the data partition ({error:?})."),
        },
        None => crate::println!("FOSFS: no FOSFS data partition on the boot disk."),
    }
}

/// "SATA c:p" or "NVMe c" without allocating.
struct Origin {
    bytes: [u8; 16],
    length: usize,
}
impl Origin {
    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.bytes[..self.length]).unwrap_or("")
    }
}
fn format_origin(kind: &str, controller: usize, port: Option<u8>) -> Origin {
    use core::fmt::Write;
    let mut origin = Origin { bytes: [0; 16], length: 0 };
    let _ = match port {
        Some(port) => write!(origin, "{kind} {controller}:{port}"),
        None => write!(origin, "{kind} {controller}"),
    };
    origin
}
impl core::fmt::Write for Origin {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        let room = self.bytes.len() - self.length;
        let take = text.len().min(room);
        self.bytes[self.length..self.length + take].copy_from_slice(&text.as_bytes()[..take]);
        self.length += take;
        Ok(())
    }
}
