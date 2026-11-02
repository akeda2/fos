//! The FOSFS volumes of the disks a core found: the boot volume (set by the core once it has mounted it) and the lookups by
//! disk number, by label and by "which volume holds this file". Disk numbers are one-based here, as `disks` lists them.

use crate::block;
use crate::fosfs::{self, Volume};
use crate::gpt;
use fosfor_abi::FOSFS_TYPE_GUID;

static mut BOOT_VOLUME: Option<Volume> = None;

/// Records the boot disk's mounted volume (after boot, and again after `grow` resized it).
pub fn set_boot_volume(volume: Volume) {
    unsafe { *core::ptr::addr_of_mut!(BOOT_VOLUME) = Some(volume) };
}

pub fn boot_volume() -> Option<Volume> {
    unsafe { *core::ptr::addr_of!(BOOT_VOLUME) }
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
