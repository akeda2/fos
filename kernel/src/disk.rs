use crate::boot_info::{read_u32, BootInfo};

const SECTOR_SIZE: usize = 512;

#[repr(align(16))]
struct AlignedSector([u8; SECTOR_SIZE]);

static mut SECTOR: AlignedSector = AlignedSector([0; SECTOR_SIZE]);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    OutOfRange,
    Unavailable,
    FirmwareFailure,
}

pub fn sector_count(info: &BootInfo) -> u32 {
    read_u32(core::ptr::addr_of!(info.disk_sector_count))
}

pub fn data_partition_lba(info: &BootInfo) -> u32 {
    read_u32(core::ptr::addr_of!(info.data_partition_lba))
}

pub fn data_partition_sectors(info: &BootInfo) -> u32 {
    read_u32(core::ptr::addr_of!(info.data_partition_sectors))
}

pub fn read_sector<T>(info: &BootInfo, lba: u32, operation: impl FnOnce(&[u8; SECTOR_SIZE]) -> T) -> Result<T, Error> {
    transfer(info, lba, 0)?;
    let sector = unsafe { &*core::ptr::addr_of!(SECTOR.0) };
    Ok(operation(sector))
}

pub fn write_sector(info: &BootInfo, lba: u32, operation: impl FnOnce(&mut [u8; SECTOR_SIZE])) -> Result<(), Error> {
    let sector = unsafe { &mut *core::ptr::addr_of_mut!(SECTOR.0) };
    operation(sector);
    transfer(info, lba, 1)
}

fn transfer(info: &BootInfo, lba: u32, direction: u32) -> Result<(), Error> {
    if lba >= sector_count(info) {
        return Err(Error::OutOfRange);
    }
    let address = read_u32(core::ptr::addr_of!(info.bios_disk_io));
    if address == 0 {
        return Err(Error::Unavailable);
    }
    let routine: unsafe extern "C" fn(u32, u32, u32) -> u32 =
        unsafe { core::mem::transmute(address as usize) };
    let buffer = core::ptr::addr_of!(SECTOR) as u32;
    if unsafe { routine(direction, lba, buffer) } == 0 {
        Ok(())
    } else {
        Err(Error::FirmwareFailure)
    }
}
