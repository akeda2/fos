//! The block-device layer: every disk any driver found, behind one read/write
//! interface by 512-byte sector. Drivers register disks here; the rest of the kernel
//! (GPT, FOSFS) only knows `read`/`write` by disk number.

#[cfg(target_arch = "x86_64")]
use crate::{ahci, nvme, usb_storage};

pub const MAX_DISKS: usize = 8;

#[derive(Clone, Copy)]
pub enum Backend {
    #[cfg(target_arch = "x86_64")]
    Ahci(ahci::Drive),
    #[cfg(target_arch = "x86_64")]
    Nvme(nvme::Drive),
    /// An index into `usb_storage`'s device table.
    #[cfg(target_arch = "x86_64")]
    Usb(usize),
    #[cfg(target_arch = "aarch64")]
    Virtio(crate::virtio_blk::Drive),
    #[cfg(target_arch = "aarch64")]
    Sd(crate::sdhci::Drive),
}

#[derive(Clone, Copy)]
pub struct Disk {
    pub backend: Backend,
    pub sectors: u64,
    pub model: [u8; 40],
    /// Where it came from, for display ("SATA 0:3").
    pub origin: [u8; 16],
    pub origin_length: usize,
}

#[derive(Debug, Clone, Copy)]
pub enum Error {
    NoSuchDisk,
    OutOfRange,
    Device,
}

static mut DISKS: [Option<Disk>; MAX_DISKS] = [None; MAX_DISKS];
static mut COUNT: usize = 0;

pub fn register(backend: Backend, sectors: u64, model: [u8; 40], origin: &str) -> Option<usize> {
    unsafe {
        if COUNT == MAX_DISKS {
            return None;
        }
        let mut name = [0u8; 16];
        let length = origin.len().min(16);
        name[..length].copy_from_slice(&origin.as_bytes()[..length]);
        (*core::ptr::addr_of_mut!(DISKS))[COUNT] = Some(Disk { backend, sectors, model, origin: name, origin_length: length });
        COUNT += 1;
        Some(COUNT - 1)
    }
}

/// Moves disk `index` to the front (disk number 1), keeping every other disk in its
/// order behind it, and returns its new index (0). The boot disk is always `disk1` this
/// way, whatever order the drivers happened to find the disks in (USB sticks by root port,
/// then SATA, then NVMe), so scripts and files that name `disk1` keep meaning the same disk.
/// Only meant for start-up, before anything has remembered a disk index.
pub fn move_to_front(index: usize) -> usize {
    unsafe {
        if index < COUNT {
            (&mut *core::ptr::addr_of_mut!(DISKS))[..=index].rotate_right(1);
            return 0;
        }
    }
    index
}

pub fn count() -> usize {
    unsafe { COUNT }
}

/// The disk number of the USB mass-storage device in `usb_storage`'s slot `slot` (disk numbers move around when the boot
/// disk is put first, so the slot is the stable identity).
#[cfg(target_arch = "x86_64")]
pub fn usb_disk(slot: usize) -> Option<usize> {
    (0..count()).find(|&index| matches!(get(index), Some(Disk { backend: Backend::Usb(found), .. }) if found == slot))
}

pub fn get(index: usize) -> Option<Disk> {
    unsafe { (*core::ptr::addr_of!(DISKS)).get(index).copied().flatten() }
}

/// Reads whole sectors (`buffer.len()` a multiple of 512) from `lba`.
pub fn read(disk: usize, lba: u64, buffer: &mut [u8]) -> Result<(), Error> {
    let info = get(disk).ok_or(Error::NoSuchDisk)?;
    if buffer.len() % 512 != 0 || lba + (buffer.len() / 512) as u64 > info.sectors {
        return Err(Error::OutOfRange);
    }
    match info.backend {
        #[cfg(target_arch = "x86_64")]
        Backend::Ahci(drive) => {
            for (index, chunk) in buffer.chunks_mut(ahci::MAX_SECTORS * 512).enumerate() {
                drive.read(lba + (index * ahci::MAX_SECTORS) as u64, chunk).map_err(|_| Error::Device)?;
            }
            Ok(())
        }
        #[cfg(target_arch = "x86_64")]
        Backend::Nvme(drive) => {
            for (index, chunk) in buffer.chunks_mut(nvme::MAX_SECTORS * 512).enumerate() {
                drive.read(lba + (index * nvme::MAX_SECTORS) as u64, chunk).map_err(|_| Error::Device)?;
            }
            Ok(())
        }
        #[cfg(target_arch = "x86_64")]
        Backend::Usb(device) => usb_storage::read(device, lba, buffer).map_err(|_| Error::Device),
        #[cfg(target_arch = "aarch64")]
        Backend::Virtio(drive) => drive.read(lba, buffer).map_err(|_| Error::Device),
        #[cfg(target_arch = "aarch64")]
        Backend::Sd(drive) => drive.read(lba, buffer).map_err(|_| Error::Device),
    }
}

/// Writes whole sectors to `lba`.
pub fn write(disk: usize, lba: u64, buffer: &[u8]) -> Result<(), Error> {
    let info = get(disk).ok_or(Error::NoSuchDisk)?;
    if buffer.len() % 512 != 0 || lba + (buffer.len() / 512) as u64 > info.sectors {
        return Err(Error::OutOfRange);
    }
    match info.backend {
        #[cfg(target_arch = "x86_64")]
        Backend::Ahci(drive) => {
            for (index, chunk) in buffer.chunks(ahci::MAX_SECTORS * 512).enumerate() {
                drive.write(lba + (index * ahci::MAX_SECTORS) as u64, chunk).map_err(|_| Error::Device)?;
            }
            Ok(())
        }
        #[cfg(target_arch = "x86_64")]
        Backend::Nvme(drive) => {
            for (index, chunk) in buffer.chunks(nvme::MAX_SECTORS * 512).enumerate() {
                drive.write(lba + (index * nvme::MAX_SECTORS) as u64, chunk).map_err(|_| Error::Device)?;
            }
            Ok(())
        }
        #[cfg(target_arch = "x86_64")]
        Backend::Usb(device) => usb_storage::write(device, lba, buffer).map_err(|_| Error::Device),
        #[cfg(target_arch = "aarch64")]
        Backend::Virtio(drive) => drive.write(lba, buffer).map_err(|_| Error::Device),
        #[cfg(target_arch = "aarch64")]
        Backend::Sd(drive) => drive.write(lba, buffer).map_err(|_| Error::Device),
    }
}
