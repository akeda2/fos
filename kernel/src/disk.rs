use crate::boot_info::{BootInfo, read_u32};
use crate::interrupts;

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

pub fn read_sector<T>(
    info: &BootInfo,
    lba: u32,
    operation: impl FnOnce(&[u8; SECTOR_SIZE]) -> T,
) -> Result<T, Error> {
    transfer(info, lba, 0)?;
    let sector = unsafe { &*core::ptr::addr_of!(SECTOR.0) };
    Ok(operation(sector))
}

pub fn write_sector(
    info: &BootInfo,
    lba: u32,
    operation: impl FnOnce(&mut [u8; SECTOR_SIZE]),
) -> Result<(), Error> {
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
    // The trampoline drops into real mode to run genuine BIOS code. BIOS
    // expects both the traditional real-mode IVT at address 0 (our own IDT
    // holds 8-byte protected-mode gate descriptors at a different address,
    // which would be misread as garbage 4-byte real-mode entries) and the
    // PIC's BIOS-conventional vector mapping (IRQ0 -> INT 08h, whose
    // real-mode handler advances the BDA tick counter that BIOS
    // disk-timeout logic polls). Restore both for the duration of the call,
    // with interrupts disabled around each switch so nothing can be
    // misdelivered mid-reprogramming.
    unsafe {
        core::arch::asm!("cli", options(nomem, nostack));
    }
    interrupts::use_real_mode_idt();
    interrupts::use_bios_pic_mapping();
    unsafe {
        core::arch::asm!("sti", options(nomem, nostack));
    }
    let result = unsafe { routine(direction, lba, buffer) };
    unsafe {
        core::arch::asm!("cli", options(nomem, nostack));
    }
    interrupts::use_kernel_idt();
    interrupts::use_kernel_pic_mapping();
    unsafe {
        core::arch::asm!("sti", options(nomem, nostack));
    }
    if result == 0 {
        Ok(())
    } else {
        Err(Error::FirmwareFailure)
    }
}
