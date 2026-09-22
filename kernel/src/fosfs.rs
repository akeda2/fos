use crate::boot_info::BootInfo;
use crate::console;
use crate::disk;
use crate::{print, println};

const MAGIC: &[u8; 8] = b"FOSFS3\0\0";
const VERSION: u16 = 3;
const SECTOR_SIZE: usize = 512;
const DIRECTORY_SECTORS: u64 = 8;
const DATA_START: u64 = 9;
const ENTRY_SIZE: usize = 64;
const ENTRIES_PER_SECTOR: usize = SECTOR_SIZE / ENTRY_SIZE;
const MAX_ENTRIES: usize = DIRECTORY_SECTORS as usize * ENTRIES_PER_SECTOR;
const MAX_NAME_LENGTH: usize = 16;

#[derive(Clone, Copy)]
struct FileEntry {
    directory_lba: u64,
    index: usize,
    start_sector: u64,
    length: u64,
    sector_count: u64,
    name: [u8; MAX_NAME_LENGTH],
    name_length: usize,
}

#[derive(Clone, Copy)]
struct Extent {
    start_sector: u64,
    sector_count: u64,
    directory_lba: u64,
    index: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WriteOutcome {
    Written,
    InvalidName,
    DirectoryFull,
    DataFull,
}

pub fn format(info: &BootInfo, device: disk::Device) -> Result<(), disk::Error> {
    let base = disk::device_data_partition_lba(info, device);
    let sectors = disk::device_data_partition_sectors(info, device);
    if sectors <= DATA_START {
        return Err(disk::Error::OutOfRange);
    }
    disk::device_write_sector(info, device, base, |sector| {
        sector.fill(0);
        sector[..8].copy_from_slice(MAGIC);
        put_u16(sector, 8, VERSION);
        put_u16(sector, 10, ENTRY_SIZE as u16);
        put_u32(sector, 12, 1);
        put_u32(sector, 16, DIRECTORY_SECTORS as u32);
        put_u64(sector, 20, DATA_START);
        put_u64(sector, 28, sectors - DATA_START);
    })?;
    for sector in 0..DIRECTORY_SECTORS {
        disk::device_write_sector(info, device, base + 1 + sector, |contents| {
            contents.fill(0)
        })?;
    }
    Ok(())
}

pub fn list(info: &BootInfo, device: disk::Device) -> Result<(), disk::Error> {
    validate(info, device)?;
    let mut found = false;
    for_each_entry(info, device, |entry| {
        found = true;
        print_name(&entry.name[..entry.name_length]);
        println!("  {} bytes, {} sector(s)", entry.length, entry.sector_count);
    })?;
    if !found {
        println!("FOSFS is empty.");
    }
    Ok(())
}

pub fn cat(info: &BootInfo, device: disk::Device, name: &[u8]) -> Result<bool, disk::Error> {
    validate(info, device)?;
    let Some(entry) = find_entry(info, device, name)? else {
        return Ok(false);
    };
    let base = disk::device_data_partition_lba(info, device);
    let mut remaining = entry.length;
    let mut sector = 0u64;
    while sector < entry.sector_count {
        let chunk_sectors =
            (entry.sector_count - sector).min(disk::MAX_TRANSFER_SECTORS as u64) as usize;
        disk::device_read_sectors(
            info,
            device,
            base + entry.start_sector + sector,
            chunk_sectors,
            |contents| {
                let length = remaining.min((chunk_sectors * SECTOR_SIZE) as u64) as usize;
                console::write_bytes(&contents[..length]);
                remaining -= length as u64;
            },
        )?;
        sector += chunk_sectors as u64;
    }
    if entry.length != 0 {
        println!();
    }
    Ok(true)
}

/// Returns a file's exact byte length, or `None` if the flat name is absent.
pub fn file_length(
    info: &BootInfo,
    device: disk::Device,
    name: &[u8],
) -> Result<Option<u64>, disk::Error> {
    validate(info, device)?;
    Ok(find_entry(info, device, name)?.map(|entry| entry.length))
}

/// Reads a whole file's raw bytes into `buffer`, returning its exact length,
/// or `None` if no such file exists. The caller must provide enough space for
/// the complete file; silently truncating a tape would corrupt its image.
pub fn read(
    info: &BootInfo,
    device: disk::Device,
    name: &[u8],
    buffer: &mut [u8],
) -> Result<Option<u64>, disk::Error> {
    validate(info, device)?;
    let Some(entry) = find_entry(info, device, name)? else {
        return Ok(None);
    };
    if entry.length > buffer.len() as u64 {
        return Err(disk::Error::OutOfRange);
    }
    let base = disk::device_data_partition_lba(info, device);
    let mut remaining = entry.length;
    let mut destination_offset = 0;
    let mut sector = 0u64;
    while sector < entry.sector_count {
        let chunk_sectors =
            (entry.sector_count - sector).min(disk::MAX_TRANSFER_SECTORS as u64) as usize;
        disk::device_read_sectors(
            info,
            device,
            base + entry.start_sector + sector,
            chunk_sectors,
            |contents| {
                let length = remaining.min((chunk_sectors * SECTOR_SIZE) as u64) as usize;
                buffer[destination_offset..destination_offset + length]
                    .copy_from_slice(&contents[..length]);
                destination_offset += length;
                remaining -= length as u64;
            },
        )?;
        sector += chunk_sectors as u64;
    }
    Ok(Some(entry.length))
}

pub fn write(
    info: &BootInfo,
    device: disk::Device,
    name: &[u8],
    text: &[u8],
) -> Result<WriteOutcome, disk::Error> {
    if !valid_name(name) {
        return Ok(WriteOutcome::InvalidName);
    }
    validate(info, device)?;
    let existing = find_entry(info, device, name)?;
    let required_sectors = sectors_for_length(text.len());
    let start_sector = if let Some(entry) = existing {
        if required_sectors <= entry.sector_count {
            entry.start_sector
        } else {
            let Some(start) = find_free_extent(
                info,
                device,
                required_sectors,
                Some(Extent {
                    start_sector: entry.start_sector,
                    sector_count: entry.sector_count,
                    directory_lba: entry.directory_lba,
                    index: entry.index,
                }),
            )?
            else {
                return Ok(WriteOutcome::DataFull);
            };
            start
        }
    } else {
        let Some((directory_lba, index)) = find_free_slot(info, device)? else {
            return Ok(WriteOutcome::DirectoryFull);
        };
        let Some(start_sector) = find_free_extent(info, device, required_sectors, None)? else {
            return Ok(WriteOutcome::DataFull);
        };
        write_file_data(info, device, start_sector, text)?;
        write_directory_entry(
            info,
            device,
            directory_lba,
            index,
            start_sector,
            text.len() as u64,
            required_sectors,
            name,
        )?;
        return Ok(WriteOutcome::Written);
    };

    write_file_data(info, device, start_sector, text)?;
    let entry = existing.unwrap();
    write_directory_entry(
        info,
        device,
        entry.directory_lba,
        entry.index,
        start_sector,
        text.len() as u64,
        required_sectors,
        name,
    )?;
    Ok(WriteOutcome::Written)
}

pub fn remove(info: &BootInfo, device: disk::Device, name: &[u8]) -> Result<bool, disk::Error> {
    validate(info, device)?;
    let Some(entry) = find_entry(info, device, name)? else {
        return Ok(false);
    };
    let mut directory = read_sector_copy(info, device, entry.directory_lba)?;
    directory[entry.index * ENTRY_SIZE..(entry.index + 1) * ENTRY_SIZE].fill(0);
    disk::device_write_sector(info, device, entry.directory_lba, |contents| {
        *contents = directory
    })?;
    Ok(true)
}

fn validate(info: &BootInfo, device: disk::Device) -> Result<(), disk::Error> {
    let base = disk::device_data_partition_lba(info, device);
    disk::device_read_sector(info, device, base, |sector| {
        &sector[..8] == MAGIC
            && get_u16(sector, 8) == VERSION
            && get_u16(sector, 10) == ENTRY_SIZE as u16
            && get_u32(sector, 12) == 1
            && get_u32(sector, 16) == DIRECTORY_SECTORS as u32
            && get_u64(sector, 20) == DATA_START
            && get_u64(sector, 28) == disk::device_data_partition_sectors(info, device) - DATA_START
    })
    .and_then(|valid| {
        if valid {
            Ok(())
        } else {
            Err(disk::Error::FirmwareFailure)
        }
    })
}

fn find_entry(
    info: &BootInfo,
    device: disk::Device,
    name: &[u8],
) -> Result<Option<FileEntry>, disk::Error> {
    let mut result = None;
    for_each_entry(info, device, |entry| {
        if result.is_none()
            && entry.name_length == name.len()
            && entry.name[..entry.name_length] == name[..]
        {
            result = Some(entry);
        }
    })?;
    Ok(result)
}

fn find_free_slot(
    info: &BootInfo,
    device: disk::Device,
) -> Result<Option<(u64, usize)>, disk::Error> {
    let base = disk::device_data_partition_lba(info, device);
    for sector_offset in 0..DIRECTORY_SECTORS {
        let lba = base + 1 + sector_offset;
        let contents = read_sector_copy(info, device, lba)?;
        for index in 0..ENTRIES_PER_SECTOR {
            if contents[index * ENTRY_SIZE] == 0 {
                return Ok(Some((lba, index)));
            }
        }
    }
    Ok(None)
}

fn find_free_extent(
    info: &BootInfo,
    device: disk::Device,
    sector_count: u64,
    ignored: Option<Extent>,
) -> Result<Option<u64>, disk::Error> {
    if sector_count == 0 {
        return Ok(Some(DATA_START));
    }
    let data_end = disk::device_data_partition_sectors(info, device);
    if sector_count > data_end.saturating_sub(DATA_START) {
        return Ok(None);
    }
    let mut extents = [Extent {
        start_sector: 0,
        sector_count: 0,
        directory_lba: 0,
        index: 0,
    }; MAX_ENTRIES];
    let mut extent_count = 0;
    for_each_entry(info, device, |entry| {
        if ignored.is_some_and(|ignored| {
            ignored.directory_lba == entry.directory_lba && ignored.index == entry.index
        }) {
            return;
        }
        if extent_count < extents.len() {
            extents[extent_count] = Extent {
                start_sector: entry.start_sector,
                sector_count: entry.sector_count,
                directory_lba: entry.directory_lba,
                index: entry.index,
            };
            extent_count += 1;
        }
    })?;

    let last_start = data_end - sector_count;
    let mut candidate = DATA_START;
    while candidate <= last_start {
        let candidate_end = candidate + sector_count;
        let overlaps = extents[..extent_count].iter().any(|extent| {
            let extent_end = extent.start_sector + extent.sector_count;
            candidate < extent_end && extent.start_sector < candidate_end
        });
        if !overlaps {
            return Ok(Some(candidate));
        }
        candidate += 1;
    }
    Ok(None)
}

fn write_file_data(
    info: &BootInfo,
    device: disk::Device,
    start_sector: u64,
    data: &[u8],
) -> Result<(), disk::Error> {
    let base = disk::device_data_partition_lba(info, device);
    let total_sectors = sectors_for_length(data.len());
    let mut offset = 0usize;
    let mut sector = 0u64;
    while sector < total_sectors {
        let chunk_sectors =
            (total_sectors - sector).min(disk::MAX_TRANSFER_SECTORS as u64) as usize;
        let chunk_bytes = (chunk_sectors * SECTOR_SIZE).min(data.len() - offset);
        let source = &data[offset..offset + chunk_bytes];
        disk::device_write_sectors(
            info,
            device,
            base + start_sector + sector,
            chunk_sectors,
            |contents| {
                contents[..source.len()].copy_from_slice(source);
            },
        )?;
        offset += chunk_bytes;
        sector += chunk_sectors as u64;
    }
    Ok(())
}

fn write_directory_entry(
    info: &BootInfo,
    device: disk::Device,
    directory_lba: u64,
    index: usize,
    start_sector: u64,
    length: u64,
    sector_count: u64,
    name: &[u8],
) -> Result<(), disk::Error> {
    let mut directory = read_sector_copy(info, device, directory_lba)?;
    let offset = index * ENTRY_SIZE;
    directory[offset..offset + ENTRY_SIZE].fill(0);
    directory[offset] = 1;
    directory[offset + 1] = name.len() as u8;
    put_u64(&mut directory, offset + 4, start_sector);
    put_u64(&mut directory, offset + 12, length);
    put_u64(&mut directory, offset + 20, sector_count);
    directory[offset + 28..offset + 28 + name.len()].copy_from_slice(name);
    disk::device_write_sector(info, device, directory_lba, |contents| *contents = directory)
}

fn for_each_entry(
    info: &BootInfo,
    device: disk::Device,
    mut operation: impl FnMut(FileEntry),
) -> Result<(), disk::Error> {
    let base = disk::device_data_partition_lba(info, device);
    let data_end = disk::device_data_partition_sectors(info, device);
    for sector_offset in 0..DIRECTORY_SECTORS {
        let lba = base + 1 + sector_offset;
        let contents = read_sector_copy(info, device, lba)?;
        for index in 0..ENTRIES_PER_SECTOR {
            let offset = index * ENTRY_SIZE;
            if contents[offset] != 1 {
                continue;
            }
            let name_length = usize::from(contents[offset + 1]);
            let start_sector = get_u64(&contents, offset + 4);
            let length = get_u64(&contents, offset + 12);
            let sector_count = get_u64(&contents, offset + 20);
            let end_sector = start_sector.saturating_add(sector_count);
            if name_length == 0
                || name_length > MAX_NAME_LENGTH
                || start_sector < DATA_START
                || end_sector > data_end
                || length > sector_count.saturating_mul(SECTOR_SIZE as u64)
            {
                continue;
            }
            let mut name = [0; MAX_NAME_LENGTH];
            name.copy_from_slice(&contents[offset + 28..offset + 44]);
            operation(FileEntry {
                directory_lba: lba,
                index,
                start_sector,
                length,
                sector_count,
                name,
                name_length,
            });
        }
    }
    Ok(())
}

fn read_sector_copy(
    info: &BootInfo,
    device: disk::Device,
    lba: u64,
) -> Result<[u8; SECTOR_SIZE], disk::Error> {
    disk::device_read_sector(info, device, lba, |contents| *contents)
}

fn sectors_for_length(length: usize) -> u64 {
    ((length + SECTOR_SIZE - 1) / SECTOR_SIZE) as u64
}

fn valid_name(name: &[u8]) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_LENGTH
        && name
            .iter()
            .all(|byte| matches!(*byte, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'))
}

fn print_name(name: &[u8]) {
    for &byte in name {
        print!("{}", byte as char);
    }
}

fn get_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn get_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn get_u64(bytes: &[u8], offset: usize) -> u64 {
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

fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}
