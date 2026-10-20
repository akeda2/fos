//! FAT32 and exFAT volumes on the disks the drivers found: the glue between the `fat` crate and `block`.
//!
//! A disk a PC formatted is found the way a PC finds it: a GPT partition of the "basic data" type, an MBR partition
//! of a FAT/exFAT type, or no partition table at all (a "superfloppy", which camera cards and some sticks are). The
//! first of those that mounts as FAT32 or exFAT is the disk's FAT volume. FOSFS stays the kernel's own format; a
//! disk that has a FOSFS partition is never treated as a FAT disk (see `tape.rs`'s `resolve_fat`).

use crate::{block, gpt};
use fat::{Device, Error};

/// The GPT type GUID for "basic data" partitions (what Windows, macOS and Linux put FAT32 and exFAT in), in the
/// on-disk mixed-endian byte order.
const BASIC_DATA_GUID: [u8; 16] = [0xa2, 0xa0, 0xd0, 0xeb, 0xe5, 0xb9, 0x33, 0x44, 0x87, 0xc0, 0x68, 0xb6, 0xb7, 0x26, 0x99, 0xc7];

/// The GPT type GUID of the EFI System Partition, in the on-disk mixed-endian byte order.
pub const ESP_GUID: [u8; 16] = [0x28, 0x73, 0x2a, 0xc1, 0x1f, 0xf8, 0xd2, 0x11, 0xba, 0x4b, 0x00, 0xa0, 0xc9, 0x3e, 0xc9, 0x3b];

/// A partition (or whole disk) as the `fat` crate's device: sector 0 is the volume's first sector.
#[derive(Clone, Copy)]
pub struct Partition {
    pub disk: usize,
    pub base: u64,
    pub sectors: u64,
}

impl Device for Partition {
    fn read(&mut self, lba: u64, buffer: &mut [u8]) -> Result<(), Error> {
        if lba + (buffer.len() / 512) as u64 > self.sectors {
            return Err(Error::Device);
        }
        block::read(self.disk, self.base + lba, buffer).map_err(|_| Error::Device)
    }
    fn write(&mut self, lba: u64, buffer: &[u8]) -> Result<(), Error> {
        if lba + (buffer.len() / 512) as u64 > self.sectors {
            return Err(Error::Device);
        }
        block::write(self.disk, self.base + lba, buffer).map_err(|_| Error::Device)
    }
}

/// Mounts the FAT volume of disk index `disk` (0-based), if it has one.
pub fn mount(disk: usize) -> Option<Fs> {
    let info = block::get(disk)?;
    let mut candidates = [Partition { disk, base: 0, sectors: 0 }; 5];
    let mut count = 0;
    // GPT basic-data partitions.
    let mut index = 0;
    while count < 4 {
        let mut seen = 0;
        let found = gpt::find(disk, &BASIC_DATA_GUID, |partition| {
            seen += 1;
            if seen > index {
                candidates[count] = Partition { disk, base: partition.start, sectors: partition.sectors };
                true
            } else {
                false
            }
        });
        if found.is_none() {
            break;
        }
        count += 1;
        index += 1;
    }
    // MBR partitions of a FAT type (0x0b/0x0c FAT32, 0x07 exFAT or NTFS: the boot sector decides).
    let mut sector = [0u8; 512];
    if block::read(disk, 0, &mut sector).is_ok() && sector[510] == 0x55 && sector[511] == 0xaa {
        for entry in 0..4 {
            let at = 446 + entry * 16;
            let kind = sector[at + 4];
            let start = u64::from(u32::from_le_bytes(sector[at + 8..at + 12].try_into().unwrap()));
            let length = u64::from(u32::from_le_bytes(sector[at + 12..at + 16].try_into().unwrap()));
            if matches!(kind, 0x0b | 0x0c | 0x07) && start > 0 && count < candidates.len() - 1 {
                candidates[count] = Partition { disk, base: start, sectors: length };
                count += 1;
            }
        }
    }
    // A volume that starts at sector 0.
    candidates[count] = Partition { disk, base: 0, sectors: info.sectors };
    count += 1;
    let mut fs = candidates[..count].iter().find_map(|&partition| fat::Fs::mount(partition).ok())?;
    stamp(&mut fs);
    Some(fs)
}

/// The boot disk's EFI System Partition (`esp/NAME`): on a Pi that is where `config.txt` and the firmware live, and the
/// loader is under `EFI/BOOT`. Only the root directory is reachable, like every FAT volume here.
pub fn mount_esp() -> Option<Fs> {
    let disk = crate::storage::boot_disk()?;
    let partition = gpt::find(disk, &ESP_GUID, |_| true)?;
    let mut fs = fat::Fs::mount(Partition { disk, base: partition.start, sectors: partition.sectors }).ok()?;
    stamp(&mut fs);
    Some(fs)
}

/// A mounted FAT volume on a disk.
pub type Fs = fat::Fs<Partition>;

/// The local time, or None while the clock is unset (files then get the fixed date).
fn now() -> Option<fat::Time> {
    crate::clock::now()
}

/// Gives `fs` the current local time for the files it creates or writes (a fixed date if the clock was never set).
pub fn stamp(fs: &mut Fs) {
    fs.set_time(now().unwrap_or(fat::Time::DEFAULT));
}

/// Why `format_exfat` did not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatError {
    NoSuchDisk,
    /// The boot disk is never formatted.
    BootDisk,
    BadLabel,
    /// Writing the partition table or the volume failed.
    Failed,
}

impl core::fmt::Display for FormatError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str(match self {
            FormatError::NoSuchDisk => "no such disk",
            FormatError::BootDisk => "that is the boot disk",
            FormatError::BadLabel => "a label is up to 11 printable ASCII characters, without \" * / : < > ? \\ |",
            FormatError::Failed => "writing to the disk failed",
        })
    }
}

/// Replaces disk index `disk`'s partition table with one GPT partition of exFAT and formats it, with the cluster size
/// `fat::format_exfat` picks for its size. Everything on the disk is lost. The shell's `format diskN exfat` and the tape
/// call `disk_format_exfat` both come here (each asks the user first). Returns the volume's size in MiB.
pub fn format_exfat(disk: usize, label: &[u8]) -> Result<u64, FormatError> {
    let info = block::get(disk).ok_or(FormatError::NoSuchDisk)?;
    if crate::storage::boot_disk() == Some(disk) {
        return Err(FormatError::BootDisk);
    }
    if label.len() > 11 || !label.iter().all(|&byte| (0x20..0x7f).contains(&byte) && !b"\"*/:<>?\\|".contains(&byte)) {
        return Err(FormatError::BadLabel);
    }
    let _ = info;
    let partition = gpt::write_single_partition(disk, &BASIC_DATA_GUID, "exFAT data").map_err(|_| FormatError::Failed)?;
    let mut device = Partition { disk, base: partition.start, sectors: partition.sectors };
    // A serial number that differs between volumes: the tick and microsecond counters, mixed.
    let serial = (crate::timer::micros() as u32) ^ (crate::timer::ticks() as u32).rotate_left(16);
    fat::format_exfat(&mut device, partition.sectors, partition.start, label, serial, now().unwrap_or(fat::Time::DEFAULT)).map_err(|_| FormatError::Failed)?;
    Ok(partition.sectors / 2048)
}
