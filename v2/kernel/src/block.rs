//! The block-device layer: every disk any driver found, behind one read/write
//! interface by 512-byte sector. Drivers register disks here; the rest of the kernel
//! (GPT, FOSFS) only knows `read`/`write` by disk number.

use crate::{ahci, usb_storage};

pub const MAX_DISKS: usize = 8;

#[derive(Clone, Copy)]
pub enum Backend {
    Ahci(ahci::Drive),
    /// An index into `usb_storage`'s device table.
    Usb(usize),
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

pub fn count() -> usize {
    unsafe { COUNT }
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
        Backend::Ahci(drive) => {
            for (index, chunk) in buffer.chunks_mut(ahci::MAX_SECTORS * 512).enumerate() {
                drive.read(lba + (index * ahci::MAX_SECTORS) as u64, chunk).map_err(|_| Error::Device)?;
            }
            Ok(())
        }
        Backend::Usb(device) => usb_storage::read(device, lba, buffer).map_err(|_| Error::Device),
    }
}

/// Writes whole sectors to `lba`.
pub fn write(disk: usize, lba: u64, buffer: &[u8]) -> Result<(), Error> {
    let info = get(disk).ok_or(Error::NoSuchDisk)?;
    if buffer.len() % 512 != 0 || lba + (buffer.len() / 512) as u64 > info.sectors {
        return Err(Error::OutOfRange);
    }
    match info.backend {
        Backend::Ahci(drive) => {
            for (index, chunk) in buffer.chunks(ahci::MAX_SECTORS * 512).enumerate() {
                drive.write(lba + (index * ahci::MAX_SECTORS) as u64, chunk).map_err(|_| Error::Device)?;
            }
            Ok(())
        }
        Backend::Usb(device) => usb_storage::write(device, lba, buffer).map_err(|_| Error::Device),
    }
}
