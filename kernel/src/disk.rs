use crate::boot_info::{read_u32, read_u64, BootInfo};
use crate::interrupts;

const SECTOR_SIZE: usize = 512;

/// The BIOS hard-disk drive-number range this trampoline accepts (`0x80`
/// through `0xff`); floppy drive numbers (`0x00`/`0x01`) are out of scope
/// here and would need their own, separate trampoline entry.
const FIRST_SECONDARY_DRIVE: u8 = 0x81;
/// An arbitrarily generous bound on how many additional (non-boot) disks
/// are probed for at boot -- comfortably more than this lab's machines
/// have BIOS drive letters for, and cheap to raise later if that changes.
const MAX_SECONDARY_DISKS: usize = 4;

#[repr(align(16))]
struct AlignedSector([u8; SECTOR_SIZE]);

static mut SECTOR: AlignedSector = AlignedSector([0; SECTOR_SIZE]);
static mut GPT_DATA_LBA: u64 = 0;
static mut GPT_DATA_SECTORS: u64 = 0;
static mut GPT_VALID: bool = false;

#[derive(Clone, Copy)]
struct SecondaryDisk {
    drive_number: u8,
    sector_count: u64,
}

static mut SECONDARY_DISKS: [Option<SecondaryDisk>; MAX_SECONDARY_DISKS] =
    [None, None, None, None];
static mut SECONDARY_COUNT: usize = 0;

/// Identifies which physical disk an FOSFS operation targets. `Boot` is the
/// disk the machine started from (GPT-discovered FOSFS partition, as
/// before); `Secondary` selects one of the additional disks found by
/// [`discover_secondary_disks`], addressed by a 0-based index into that
/// list (`disk2` in shell command syntax is index 0, `disk3` is index 1,
/// and so on).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Device {
    Boot,
    Secondary(usize),
}

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

/// Reads one entry of the secondary-disk table without ever materializing a
/// shared reference to the whole `static mut` array (disallowed under the
/// 2024 edition's `static_mut_refs` lint) -- a single-element raw-pointer
/// read/copy is sufficient since `SecondaryDisk` is `Copy`.
fn secondary_disk(index: usize) -> Option<SecondaryDisk> {
    if index >= MAX_SECONDARY_DISKS {
        return None;
    }
    unsafe { core::ptr::addr_of!(SECONDARY_DISKS[index]).read() }
}

/// The BIOS drive number a [`Device`] should be addressed by.
fn drive_number(info: &BootInfo, device: Device) -> Option<u8> {
    match device {
        Device::Boot => Some(info.boot_drive),
        Device::Secondary(index) => secondary_disk(index).map(|disk| disk.drive_number),
    }
}

/// How many sectors are addressable on `device`, for bounds-checking reads
/// and writes.
pub fn device_sector_count(info: &BootInfo, device: Device) -> u64 {
    match device {
        Device::Boot => sector_count(info),
        Device::Secondary(index) => {
            secondary_disk(index).map_or(0, |disk| disk.sector_count)
        }
    }
}

/// Where FOSFS's own data lives on `device`. The boot disk keeps its
/// GPT-discovered (or loader-provided) partition offset; a secondary disk
/// has no partition table at all -- it is formatted as one flat FOSFS
/// volume starting at LBA 0, spanning the whole disk, since it is never
/// booted from and so has no need to also be a standards-compliant
/// bootable image (see AGENTS.md's "A second (non-boot) disk" section).
pub fn device_data_partition_lba(info: &BootInfo, device: Device) -> u64 {
    match device {
        Device::Boot => data_partition_lba(info),
        Device::Secondary(_) => 0,
    }
}

pub fn device_data_partition_sectors(info: &BootInfo, device: Device) -> u64 {
    match device {
        Device::Boot => data_partition_sectors(info),
        Device::Secondary(_) => device_sector_count(info, device),
    }
}

/// How many additional (non-boot) disks were found by
/// [`discover_secondary_disks`].
pub fn secondary_count() -> usize {
    unsafe { SECONDARY_COUNT }
}

/// Probes BIOS drives `0x81`, `0x82`, ... (up to [`MAX_SECONDARY_DISKS`])
/// for a second disk to use for config/log storage, alongside the boot
/// disk. Unlike the boot drive (handed to the loader for free in `DL` at
/// boot), nothing tells the kernel whether an additional disk is even
/// attached, so each candidate drive number is checked with the same
/// `AH=41h`/`AH=48h` BIOS Enhanced Disk Drive calls a real partitioning
/// tool would use, via the trampoline's new probe operation.
pub fn discover_secondary_disks(info: &BootInfo) {
    unsafe {
        SECONDARY_COUNT = 0;
    }
    for offset in 0..MAX_SECONDARY_DISKS as u8 {
        let drive_number = FIRST_SECONDARY_DRIVE + offset;
        let Some(sector_count) = probe_drive(info, drive_number) else {
            continue;
        };
        if sector_count == 0 {
            continue;
        }
        let count = unsafe {
            SECONDARY_DISKS[SECONDARY_COUNT] = Some(SecondaryDisk {
                drive_number,
                sector_count,
            });
            SECONDARY_COUNT += 1;
            SECONDARY_COUNT
        };
        crate::println!(
            "disk{}: found (BIOS drive 0x{:02x}, {} sectors).",
            count + 1,
            drive_number,
            sector_count
        );
    }
}

fn probe_drive(info: &BootInfo, drive_number: u8) -> Option<u64> {
    call_trampoline(info, 2, 0, drive_number).ok()?;
    let sector = unsafe { &*core::ptr::addr_of!(SECTOR.0) };
    Some(get_u64(sector, 0))
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
    device_read_sector(info, Device::Boot, lba, operation)
}

/// Same as [`read_sector`], but against an explicit [`Device`] rather than
/// always the boot disk -- the one addition FOSFS operations on a second
/// disk need.
pub fn device_read_sector<T>(
    info: &BootInfo,
    device: Device,
    lba: u64,
    operation: impl FnOnce(&[u8; SECTOR_SIZE]) -> T,
) -> Result<T, Error> {
    transfer(info, device, lba, 0)?;
    let sector = unsafe { &*core::ptr::addr_of!(SECTOR.0) };
    Ok(operation(sector))
}

pub fn device_write_sector(
    info: &BootInfo,
    device: Device,
    lba: u64,
    operation: impl FnOnce(&mut [u8; SECTOR_SIZE]),
) -> Result<(), Error> {
    let sector = unsafe { &mut *core::ptr::addr_of_mut!(SECTOR.0) };
    operation(sector);
    transfer(info, device, lba, 1)
}

fn transfer(info: &BootInfo, device: Device, lba: u64, direction: u32) -> Result<(), Error> {
    if lba >= device_sector_count(info, device) {
        return Err(Error::OutOfRange);
    }
    let Some(drive_number) = drive_number(info, device) else {
        return Err(Error::Unavailable);
    };
    call_trampoline(info, direction, lba, drive_number)
}

/// Invokes the `bios_disk_io` trampoline (`boot/stage2.S`) for one
/// operation against `drive_number`, using the shared [`SECTOR`] buffer as
/// its source/destination. `lba` is ignored by the probe operation
/// (`direction == 2`), which instead fills [`SECTOR`] with an 8-byte total
/// sector count on success.
fn call_trampoline(
    info: &BootInfo,
    direction: u32,
    lba: u64,
    drive_number: u8,
) -> Result<(), Error> {
    let address = read_u32(core::ptr::addr_of!(info.bios_disk_io));
    if address == 0 {
        return Err(Error::Unavailable);
    }
    let routine: unsafe extern "C" fn(u32, u32, u32, u32, u32) -> u32 =
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
    // misdelivered mid-reprogramming. Keep interrupts disabled until the
    // trampoline has actually switched to real mode; while the CPU is still
    // in protected mode, IDTR=0 points at 4-byte real-mode IVT entries, not
    // valid 8-byte protected-mode gates, so an IRQ in that small window
    // would triple-fault the machine. The trampoline enables interrupts only
    // after entering real mode and disables them again before returning.
    unsafe {
        core::arch::asm!("cli", options(nomem, nostack));
    }
    interrupts::use_real_mode_idt();
    interrupts::use_bios_pic_mapping();
    let result = unsafe {
        routine(
            direction,
            lba as u32,
            (lba >> 32) as u32,
            buffer,
            drive_number as u32,
        )
    };
    unsafe {
        core::arch::asm!("cli", options(nomem, nostack));
    }
    crate::gdt::restore_after_bios();
    interrupts::use_kernel_idt();
    interrupts::use_kernel_pic_mapping();
    // Some USB legacy-emulation BIOSes alter the keyboard scan-set/8042
    // translation state while handling INT 13h. Restore the Set 1 contract
    // before IRQ1 can resume delivering bytes to the shell decoder.
    crate::keyboard::restore_after_bios_disk_call();
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
