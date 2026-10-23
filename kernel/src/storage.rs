//! Storage bring-up: start the disk drivers, register their disks, and find *the boot
//! disk* without the firmware. The loader passed the GUID, start and size of the
//! payload partition it booted from (BootInfo); the kernel scans every disk its
//! drivers found for that partition, then mounts the FOSFS data partition on the same
//! disk. Boot-disk discovery is what lets a USB stick or a SATA disk be the root
//! filesystem without any BIOS help.

use fosfor_abi::{BootInfo, FOSFS_TYPE_GUID, PAYLOAD_TYPE_GUID};

use crate::block::{self, Backend};
use crate::fosfs::{self, Volume};
use crate::gpt;
#[cfg(target_arch = "x86_64")]
use crate::{ahci, nvme};

static mut BOOT_VOLUME: Option<Volume> = None;
static mut BOOT_DISK: Option<usize> = None;
static mut PAYLOAD_START: Option<u64> = None;
static mut BOOT_SLOT: u32 = 0;

/// Tells the watchdog a long disk operation is still making progress (`debug` on x86, `crash` on the Pi).
pub fn pet() {
    #[cfg(target_arch = "x86_64")]
    crate::debug::pet();
    #[cfg(target_arch = "aarch64")]
    crate::crash::pet();
}

/// The A/B slot the running kernel was loaded from (`install` copies exactly that one).
pub fn boot_slot() -> u32 {
    unsafe { BOOT_SLOT }
}

pub fn boot_volume() -> Option<Volume> {
    unsafe { *core::ptr::addr_of!(BOOT_VOLUME) }
}

/// First sector of the payload partition on the boot disk (where the slot metadata and
/// the kernel slots live).
pub fn payload_start() -> Option<u64> {
    unsafe { *core::ptr::addr_of!(PAYLOAD_START) }
}

/// How a freshly attached disk is named in a message. Disk numbers are settled once the
/// boot disk is known (it becomes disk1), so a stick found before that cannot honestly be
/// given a number yet; `disks` lists the final numbering.
pub struct DiskNumber(pub usize);

impl core::fmt::Display for DiskNumber {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if boot_disk().is_some() {
            write!(formatter, "disk{}", self.0 + 1)
        } else {
            write!(formatter, "attached (numbered after the boot disk is found; see `disks`)")
        }
    }
}

/// Why a disk has no usable FOSFS volume.
#[derive(Debug)]
pub enum VolumeError {
    NoSuchDisk,
    /// The partition table could not be read at all (a failed read: the disk is gone or failing).
    DiskUnreadable,
    NoPartition,
    NotFormatted,
    Unreadable(fosfs::Error),
}

impl core::fmt::Display for VolumeError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            VolumeError::NoSuchDisk => write!(formatter, "no such disk (see `disks`)"),
            VolumeError::DiskUnreadable => write!(formatter, "cannot read the disk (it may have been unplugged)"),
            VolumeError::NoPartition => write!(formatter, "no FOSFS data partition (a GPT partition of the FOSFS type)"),
            VolumeError::NotFormatted => write!(formatter, "its data partition holds no FOSFS v3 volume"),
            VolumeError::Unreadable(error) => write!(formatter, "cannot read the data partition ({error:?})"),
        }
    }
}

/// Mounts the FOSFS data partition of disk number `number` (1-based, as `disks` lists).
pub fn volume_on(number: usize) -> Result<Volume, VolumeError> {
    if number == 0 || number > block::count() {
        return Err(VolumeError::NoSuchDisk);
    }
    let disk = number - 1;
    let partition = gpt::find(disk, &FOSFS_TYPE_GUID, |_| true).ok_or_else(|| {
        // `find` says None for "no such partition" and for a failed read alike; tell them apart.
        if block::read(disk, 1, &mut [0u8; 512]).is_err() { VolumeError::DiskUnreadable } else { VolumeError::NoPartition }
    })?;
    Volume::mount(disk, partition.start, partition.sectors).map_err(|error| match error {
        fosfs::Error::NotFormatted => VolumeError::NotFormatted,
        other => VolumeError::Unreadable(other),
    })
}

/// The volume holding the file `name`: the boot volume first, then the other disks in order (the same search v1 does for
/// `start`, `hostname`, `settings` and `debug`). A disk with no FOSFS volume is simply skipped.
pub fn volume_with_file(name: &[u8]) -> Option<Volume> {
    let has = |volume: &Volume| matches!(volume.file_length(name), Ok(Some(_)));
    if let Some(volume) = boot_volume().filter(|volume| has(volume)) {
        return Some(volume);
    }
    (1..=block::count()).find_map(|number| volume_on(number).ok().filter(|volume| has(volume)))
}

/// The first volume, in disk order, whose label is `label`.
pub fn volume_labelled(label: &[u8]) -> Option<(usize, Volume)> {
    (1..=block::count()).find_map(|number| {
        let volume = volume_on(number).ok()?;
        (volume.label().ok()?.as_bytes() == label).then_some((number, volume))
    })
}

pub fn boot_disk() -> Option<usize> {
    unsafe { *core::ptr::addr_of!(BOOT_DISK) }
}

fn trimmed(model: &[u8]) -> &str {
    core::str::from_utf8(model).unwrap_or("?").trim()
}

/// What the loader said about the payload partition it booted from: GUID, first sector, size.
static mut PAYLOAD_IDENTITY: Option<([u8; 16], u64, u64)> = None;

pub fn init(info: &BootInfo) {
    unsafe {
        BOOT_SLOT = info.boot_slot;
        PAYLOAD_IDENTITY = Some((info.payload_guid, info.payload_start, info.payload_sectors));
    }
    #[cfg(target_arch = "x86_64")]
    let (sata, nvme) = {
        let sata = ahci::init_all(|drive, controller, port| {
            let origin = format_origin("SATA", controller, Some(port));
            block::register(Backend::Ahci(drive), drive.sectors, drive.model, origin.as_str());
        });
        let nvme = nvme::init_all(|drive, controller| {
            let origin = format_origin("NVMe", controller, None);
            block::register(Backend::Nvme(drive), drive.sectors, drive.model, origin.as_str());
        });
        (sata, nvme)
    };
    #[cfg(target_arch = "aarch64")]
    let (sata, nvme) = {
        crate::virtio_blk::init_all(|drive, index| {
            let origin = format_origin("virtio", index, None);
            block::register(Backend::Virtio(drive), drive.sectors, drive.model(), origin.as_str());
        });
        crate::sdhci::init_all(|drive, index| {
            let origin = format_origin("SD", index, None);
            block::register(Backend::Sd(drive), drive.sectors, drive.model(), origin.as_str());
        });
        (0, 0)
    };
    if block::count() == 0 {
        crate::println!("Storage: no disks found by the drivers this kernel has ({sata} SATA, {nvme} NVMe).");
    }
    locate_boot_disk(true);
}

/// Finds the boot disk among the registered disks, makes it disk1 and mounts its FOSFS volume. Returns true when the boot
/// disk is known (also when it was found earlier). Called at boot, again from the retry in `main` and from `usb rescan`: a
/// USB disk on some controllers is not ready when the first scan runs. `report` says what is missing; a quiet retry stays
/// silent about a boot disk that is still not there.
pub fn locate_boot_disk(report: bool) -> bool {
    if boot_disk().is_some() {
        return true;
    }
    let Some((guid, start, sectors)) = (unsafe { *core::ptr::addr_of!(PAYLOAD_IDENTITY) }) else { return false };
    if block::count() == 0 {
        return false;
    }

    // The boot disk is the one holding the payload partition the loader read. It becomes
    // disk1 whatever order the drivers found the disks in, so `disk1` always means "the
    // disk this system started from".
    let mut found = None;
    for index in 0..block::count() {
        let hit = gpt::find(index, &PAYLOAD_TYPE_GUID, |p| p.unique_guid == guid && p.start == start && p.sectors == sectors);
        if hit.is_some() {
            found = Some(index);
            break;
        }
    }
    if found.is_none() && !report {
        return false;
    }
    let found = found.map(|index| {
        if index != 0 {
            crate::println!("The boot disk was found as driver disk {}; renumbering it to disk1 (the others keep their order behind it).", index + 1);
        }
        block::move_to_front(index)
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
    let Some(disk) = found else {
        crate::println!("Boot disk: not found among the disks above (the payload partition GUID matches none); no FOSFS.");
        return false;
    };
    crate::println!("Boot disk: disk{} (its GPT holds the payload partition the loader booted from).", disk + 1);
    unsafe {
        BOOT_DISK = Some(disk);
        PAYLOAD_START = Some(start);
    }
    #[cfg(target_arch = "x86_64")]
    crate::debug::set_payload(disk, start);
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
    true
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

/// Mounts the boot disk's FOSFS volume again (after `grow` changed its size).
pub fn remount_boot() {
    if let Some(disk) = boot_disk() {
        if let Some(partition) = gpt::find(disk, &FOSFS_TYPE_GUID, |_| true) {
            if let Ok(volume) = Volume::mount(disk, partition.start, partition.sectors) {
                unsafe { BOOT_VOLUME = Some(volume) };
            }
        }
    }
}
