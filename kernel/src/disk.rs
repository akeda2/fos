use crate::boot_info::{read_u32, read_u64, BootInfo};
use crate::interrupts;

const SECTOR_SIZE: usize = 512;

#[repr(align(16))]
struct AlignedSector([u8; SECTOR_SIZE]);

static mut SECTOR: AlignedSector = AlignedSector([0; SECTOR_SIZE]);
static mut GPT_DATA_LBA: u64 = 0;
static mut GPT_DATA_SECTORS: u64 = 0;
static mut GPT_VALID: bool = false;

const FOSFS_PARTITION_TYPE: [u8; 16] = [
    0x62, 0x73, 0x6f, 0x0f, 0x00, 0x00, 0x53, 0x4f, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x04,
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    OutOfRange,
    Unavailable,
    FirmwareFailure,
}

pub fn sector_count(info: &BootInfo) -> u64 {
    read_u64(core::ptr::addr_of!(info.disk_sector_count))
}

pub fn data_partition_lba(info: &BootInfo) -> u64 {
    if unsafe { GPT_VALID } {
        return unsafe { GPT_DATA_LBA };
    }
    read_u64(core::ptr::addr_of!(info.data_partition_lba))
}

pub fn data_partition_sectors(info: &BootInfo) -> u64 {
    if unsafe { GPT_VALID } {
        return unsafe { GPT_DATA_SECTORS };
    }
    read_u64(core::ptr::addr_of!(info.data_partition_sectors))
}

/// Discovers the FOSFS partition from the primary GPT, falling back to the
/// loader-provided geometry if an older image has no valid GPT.
pub fn initialize(info: &BootInfo) {
    let disk_sectors = sector_count(info);
    let Ok(valid) = read_sector(info, 1, |sector| {
        if &sector[..8] != b"EFI PART"
            || get_u32(sector, 8) != 0x0001_0000
            || get_u32(sector, 12) < 92
        {
            return None;
        }
        let entries_lba = get_u64(sector, 72);
        let entry_count = get_u32(sector, 80);
        let entry_size = get_u32(sector, 84);
        if entries_lba >= disk_sectors
            || entry_count == 0
            || entry_count > 128
            || entry_size < 128
            || entry_size > SECTOR_SIZE as u32
        {
            return None;
        }
        Some((entries_lba, entry_count, entry_size))
    }) else {
        return;
    };
    let Some((entries_lba, entry_count, entry_size)) = valid else {
        return;
    };

    for index in 0..entry_count as u64 {
        let byte_offset = index * entry_size as u64;
        let lba = entries_lba + byte_offset / SECTOR_SIZE as u64;
        let offset = (byte_offset % SECTOR_SIZE as u64) as usize;
        let Ok(entry) = read_sector(info, lba, |sector| {
            if offset + 48 > SECTOR_SIZE {
                return None;
            }
            if sector[offset..offset + 16] != FOSFS_PARTITION_TYPE {
                return None;
            }
            Some((get_u64(sector, offset + 32), get_u64(sector, offset + 40)))
        }) else {
            return;
        };
        let Some((first, last)) = entry else {
            continue;
        };
        if first == 0 || last < first || last >= disk_sectors {
            return;
        }
        unsafe {
            GPT_DATA_LBA = first;
            GPT_DATA_SECTORS = last - first + 1;
            GPT_VALID = true;
        }
        crate::println!(
            "GPT: discovered FOSFS partition at LBA {} + {} sectors.",
            first,
            last - first + 1
        );
        return;
    }
}

pub fn read_sector<T>(
    info: &BootInfo,
    lba: u64,
    operation: impl FnOnce(&[u8; SECTOR_SIZE]) -> T,
) -> Result<T, Error> {
    transfer(info, lba, 0)?;
    let sector = unsafe { &*core::ptr::addr_of!(SECTOR.0) };
    Ok(operation(sector))
}

pub fn write_sector(
    info: &BootInfo,
    lba: u64,
    operation: impl FnOnce(&mut [u8; SECTOR_SIZE]),
) -> Result<(), Error> {
    let sector = unsafe { &mut *core::ptr::addr_of_mut!(SECTOR.0) };
    operation(sector);
    transfer(info, lba, 1)
}

fn transfer(info: &BootInfo, lba: u64, direction: u32) -> Result<(), Error> {
    if lba >= sector_count(info) {
        return Err(Error::OutOfRange);
    }
    let address = read_u32(core::ptr::addr_of!(info.bios_disk_io));
    if address == 0 {
        return Err(Error::Unavailable);
    }
    let routine: unsafe extern "C" fn(u32, u32, u32, u32) -> u32 =
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
    let result = unsafe {
        routine(
            direction,
            lba as u32,
            (lba >> 32) as u32,
            buffer,
        )
    };
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

fn get_u32(bytes: &[u8; SECTOR_SIZE], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn get_u64(bytes: &[u8; SECTOR_SIZE], offset: usize) -> u64 {
    u64::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
        bytes[offset + 4],
        bytes[offset + 5],
        bytes[offset + 6],
        bytes[offset + 7],
    ])
}
