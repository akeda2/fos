//! The block-device layer: every disk the drivers found, by 512-byte sector. The same shape as the embedded kernel's
//! `block.rs` (SATA, NVMe and USB mass storage). Registration happens on the boot CPU
//! during start-up; reads and writes go straight to the drivers.

use kernel_core::{ahci, nvme};

pub const MAX_DISKS: usize = 16;

#[derive(Clone, Copy)]
pub enum Backend {
    Ahci(ahci::Drive),
    Nvme(nvme::Drive),
    /// An index into `usb_storage`'s device table.
    Usb(usize),
    /// The array `mdvol` assembled: the disk is virtual, its sectors come from the members.
    Md,
}

#[derive(Clone, Copy)]
pub struct Disk {
    pub backend: Backend,
    pub sectors: u64,
    pub model: [u8; 40],
    pub origin: [u8; 16],
    pub origin_length: usize,
}

#[derive(Debug, Clone, Copy)]
pub enum Error {
    NoSuchDisk,
    OutOfRange,
    Device,
    /// The disk cannot do it (TRIM on a disk without it).
    Unsupported,
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

/// Makes disk `index` disk number 1, the others keeping their order behind it. Start-up only.
pub fn move_to_front(index: usize) -> usize {
    unsafe {
        if index < COUNT {
            (&mut *core::ptr::addr_of_mut!(DISKS))[..=index].rotate_right(1);
            return 0;
        }
    }
    index
}

/// Makes the (stopped) array's disk `index` live again with a new size and name. False when `index` is not an md disk.
pub fn set_md(index: usize, sectors: u64, model: [u8; 40]) -> bool {
    unsafe {
        match (*core::ptr::addr_of_mut!(DISKS)).get_mut(index) {
            Some(Some(disk)) if matches!(disk.backend, Backend::Md) => {
                disk.sectors = sectors;
                disk.model = model;
                true
            }
            _ => false,
        }
    }
}

pub fn count() -> usize {
    unsafe { COUNT }
}

/// The disk number of the USB mass-storage device in `usb_storage`'s slot `slot` (disk numbers move when the boot disk is put
/// first, so the slot is the stable identity).
pub fn usb_disk(slot: usize) -> Option<usize> {
    (0..count()).find(|&index| matches!(get(index), Some(Disk { backend: Backend::Usb(found), .. }) if found == slot))
}

pub fn get(index: usize) -> Option<Disk> {
    unsafe { (*core::ptr::addr_of!(DISKS)).get(index).copied().flatten() }
}

fn checked(disk: usize, lba: u64, length: usize) -> Result<Disk, Error> {
    let info = get(disk).ok_or(Error::NoSuchDisk)?;
    if length % 512 != 0 || lba + (length / 512) as u64 > info.sectors {
        return Err(Error::OutOfRange);
    }
    Ok(info)
}

/// Reads whole sectors (`buffer.len()` a multiple of 512) from `lba`.
pub fn read(disk: usize, lba: u64, buffer: &mut [u8]) -> Result<(), Error> {
    match checked(disk, lba, buffer.len())?.backend {
        Backend::Ahci(drive) => {
            for (index, chunk) in buffer.chunks_mut(ahci::MAX_SECTORS * 512).enumerate() {
                drive.read(lba + (index * ahci::MAX_SECTORS) as u64, chunk).map_err(|_| Error::Device)?;
            }
        }
        Backend::Nvme(drive) => {
            for (index, chunk) in buffer.chunks_mut(nvme::MAX_SECTORS * 512).enumerate() {
                drive.read(lba + (index * nvme::MAX_SECTORS) as u64, chunk).map_err(|_| Error::Device)?;
            }
        }
        Backend::Usb(device) => crate::usb::storage_read(device, lba, buffer).map_err(|_| Error::Device)?,
        Backend::Md => crate::mdvol::read(lba, buffer)?,
    }
    Ok(())
}

/// Writes whole sectors to `lba`.
pub fn write(disk: usize, lba: u64, buffer: &[u8]) -> Result<(), Error> {
    write_with(disk, lba, buffer, true)
}

/// Writes without flushing a SATA drive's cache (`flush` does); the other drivers ignore the difference.
pub fn write_deferred(disk: usize, lba: u64, buffer: &[u8]) -> Result<(), Error> {
    write_with(disk, lba, buffer, false)
}

/// Makes the writes so far durable (SATA: FLUSH CACHE).
pub fn flush(disk: usize) -> Result<(), Error> {
    match checked(disk, 0, 0)?.backend {
        Backend::Ahci(drive) => drive.flush().map_err(|_| Error::Device),
        Backend::Md => crate::mdvol::flush(),
        _ => Ok(()),
    }
}

fn write_with(disk: usize, lba: u64, buffer: &[u8], flush: bool) -> Result<(), Error> {
    match checked(disk, lba, buffer.len())?.backend {
        Backend::Ahci(drive) => {
            for (index, chunk) in buffer.chunks(ahci::MAX_SECTORS * 512).enumerate() {
                let at = lba + (index * ahci::MAX_SECTORS) as u64;
                if flush { drive.write(at, chunk) } else { drive.write_deferred(at, chunk) }.map_err(|_| Error::Device)?;
            }
        }
        Backend::Nvme(drive) => {
            for (index, chunk) in buffer.chunks(nvme::MAX_SECTORS * 512).enumerate() {
                drive.write(lba + (index * nvme::MAX_SECTORS) as u64, chunk).map_err(|_| Error::Device)?;
            }
        }
        Backend::Usb(device) => crate::usb::storage_write(device, lba, buffer).map_err(|_| Error::Device)?,
        Backend::Md => crate::mdvol::write(lba, buffer)?,
    }
    Ok(())
}

/// TRIM: tells the disk these (start, sectors) ranges are free space. Contents read back afterwards are undefined.
/// SATA and NVMe SSDs only; `Unsupported` otherwise (USB mass storage for now).
pub fn discard(disk: usize, ranges: &[(u64, u64)]) -> Result<(), Error> {
    let info = get(disk).ok_or(Error::NoSuchDisk)?;
    if ranges.iter().any(|&(lba, sectors)| lba.checked_add(sectors).map_or(true, |end| end > info.sectors)) {
        return Err(Error::OutOfRange);
    }
    match info.backend {
        Backend::Ahci(drive) => drive.discard(ranges).map_err(|error| if error == ahci::Error::Unsupported { Error::Unsupported } else { Error::Device }),
        Backend::Nvme(drive) => drive.discard(ranges).map_err(|error| if error == nvme::Error::Unsupported { Error::Unsupported } else { Error::Device }),
        Backend::Usb(_) | Backend::Md => Err(Error::Unsupported),
    }
}
