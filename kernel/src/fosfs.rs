use crate::boot_info::BootInfo;
use crate::console;
use crate::disk;
use crate::{print, println};

const MAGIC: &[u8; 8] = b"FOSFS1\0\0";
const SECTOR_SIZE: usize = 512;
const DIRECTORY_SECTORS: u32 = 4;
const DATA_START: u32 = 5;
const ENTRY_SIZE: usize = 32;
const ENTRIES_PER_SECTOR: usize = SECTOR_SIZE / ENTRY_SIZE;
const MAX_NAME_LENGTH: usize = 16;

#[derive(Clone, Copy)]
struct FileEntry {
    directory_lba: u32,
    index: usize,
    start_sector: u32,
    length: u32,
    sector_count: u32,
    name: [u8; MAX_NAME_LENGTH],
    name_length: usize,
}

pub fn format(info: &BootInfo) -> Result<(), disk::Error> {
    let base = disk::data_partition_lba(info);
    let sectors = disk::data_partition_sectors(info);
    disk::write_sector(info, base, |sector| {
        sector.fill(0);
        sector[..8].copy_from_slice(MAGIC);
        put_u16(sector, 8, 1);
        put_u16(sector, 10, 32);
        put_u32(sector, 12, 1);
        put_u32(sector, 16, DIRECTORY_SECTORS);
        put_u32(sector, 20, DATA_START);
        put_u32(sector, 24, sectors - DATA_START);
    })?;
    for sector in 0..DIRECTORY_SECTORS {
        disk::write_sector(info, base + 1 + sector, |contents| contents.fill(0))?;
    }
    Ok(())
}

pub fn list(info: &BootInfo) -> Result<(), disk::Error> {
    validate(info)?;
    let mut found = false;
    for_each_entry(info, |entry| {
        found = true;
        print_name(&entry.name[..entry.name_length]);
        println!("  {} bytes, {} sector(s)", entry.length, entry.sector_count);
    })?;
    if !found {
        println!("FOSFS is empty.");
    }
    Ok(())
}

pub fn cat(info: &BootInfo, name: &[u8]) -> Result<bool, disk::Error> {
    validate(info)?;
    let Some(entry) = find_entry(info, name)? else {
        return Ok(false);
    };
    let base = disk::data_partition_lba(info);
    let mut remaining = entry.length as usize;
    for sector in 0..entry.sector_count {
        disk::read_sector(info, base + entry.start_sector + sector, |contents| {
            let length = remaining.min(SECTOR_SIZE);
            console::write_bytes(&contents[..length]);
            remaining -= length;
        })?;
    }
    if entry.length != 0 {
        println!();
    }
    Ok(true)
}

pub fn write(info: &BootInfo, name: &[u8], text: &[u8]) -> Result<bool, disk::Error> {
    if !valid_name(name) || text.len() > SECTOR_SIZE {
        return Ok(false);
    }
    validate(info)?;
    let base = disk::data_partition_lba(info);
    let existing = find_entry(info, name)?;
    let (directory_lba, index, start_sector) = if let Some(entry) = existing {
        (entry.directory_lba, entry.index, entry.start_sector)
    } else {
        let Some((directory_lba, index)) = find_free_slot(info)? else {
            return Ok(false);
        };
        (directory_lba, index, next_free_data_sector(info)?)
    };
    if start_sector >= disk::data_partition_sectors(info) {
        return Ok(false);
    }
    disk::write_sector(info, base + start_sector, |contents| {
        contents.fill(0);
        contents[..text.len()].copy_from_slice(text);
    })?;
    let mut directory = read_sector_copy(info, directory_lba)?;
    let offset = index * ENTRY_SIZE;
    directory[offset..offset + ENTRY_SIZE].fill(0);
    directory[offset] = 1;
    directory[offset + 1] = name.len() as u8;
    put_u32(&mut directory, offset + 4, start_sector);
    put_u32(&mut directory, offset + 8, text.len() as u32);
    put_u32(&mut directory, offset + 12, 1);
    directory[offset + 16..offset + 16 + name.len()].copy_from_slice(name);
    disk::write_sector(info, directory_lba, |contents| *contents = directory)?;
    Ok(true)
}

pub fn remove(info: &BootInfo, name: &[u8]) -> Result<bool, disk::Error> {
    validate(info)?;
    let Some(entry) = find_entry(info, name)? else {
        return Ok(false);
    };
    let mut directory = read_sector_copy(info, entry.directory_lba)?;
    directory[entry.index * ENTRY_SIZE..(entry.index + 1) * ENTRY_SIZE].fill(0);
    disk::write_sector(info, entry.directory_lba, |contents| *contents = directory)?;
    Ok(true)
}

fn validate(info: &BootInfo) -> Result<(), disk::Error> {
    let base = disk::data_partition_lba(info);
    disk::read_sector(info, base, |sector| {
        if &sector[..8] != MAGIC
            || get_u16(sector, 8) != 1
            || get_u16(sector, 10) != 32
            || get_u32(sector, 12) != 1
            || get_u32(sector, 16) != DIRECTORY_SECTORS
            || get_u32(sector, 20) != DATA_START
        {
            return false;
        }
        true
    })
    .and_then(|valid| if valid { Ok(()) } else { Err(disk::Error::FirmwareFailure) })
}

fn find_entry(info: &BootInfo, name: &[u8]) -> Result<Option<FileEntry>, disk::Error> {
    let mut result = None;
    for_each_entry(info, |entry| {
        if result.is_none() && entry.name_length == name.len() && entry.name[..entry.name_length] == name[..] {
            result = Some(entry);
        }
    })?;
    Ok(result)
}

fn find_free_slot(info: &BootInfo) -> Result<Option<(u32, usize)>, disk::Error> {
    let base = disk::data_partition_lba(info);
    for sector_offset in 0..DIRECTORY_SECTORS {
        let lba = base + 1 + sector_offset;
        let contents = read_sector_copy(info, lba)?;
        for index in 0..ENTRIES_PER_SECTOR {
            if contents[index * ENTRY_SIZE] == 0 {
                return Ok(Some((lba, index)));
            }
        }
    }
    Ok(None)
}

fn next_free_data_sector(info: &BootInfo) -> Result<u32, disk::Error> {
    let mut next = DATA_START;
    for_each_entry(info, |entry| {
        next = next.max(entry.start_sector.saturating_add(entry.sector_count));
    })?;
    Ok(next)
}

fn for_each_entry(info: &BootInfo, mut operation: impl FnMut(FileEntry)) -> Result<(), disk::Error> {
    let base = disk::data_partition_lba(info);
    for sector_offset in 0..DIRECTORY_SECTORS {
        let lba = base + 1 + sector_offset;
        let contents = read_sector_copy(info, lba)?;
        for index in 0..ENTRIES_PER_SECTOR {
            let offset = index * ENTRY_SIZE;
            if contents[offset] != 1 {
                continue;
            }
            let name_length = usize::from(contents[offset + 1]);
            if name_length == 0 || name_length > MAX_NAME_LENGTH {
                continue;
            }
            let mut name = [0; MAX_NAME_LENGTH];
            name.copy_from_slice(&contents[offset + 16..offset + 32]);
            operation(FileEntry {
                directory_lba: lba,
                index,
                start_sector: get_u32(&contents, offset + 4),
                length: get_u32(&contents, offset + 8),
                sector_count: get_u32(&contents, offset + 12),
                name,
                name_length,
            });
        }
    }
    Ok(())
}

fn read_sector_copy(info: &BootInfo, lba: u32) -> Result<[u8; SECTOR_SIZE], disk::Error> {
    disk::read_sector(info, lba, |contents| *contents)
}

fn valid_name(name: &[u8]) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_LENGTH
        && name.iter().all(|byte| {
            matches!(*byte, b'A'..=b'Z' | b'0'..=b'9' | b'.' | b'_' | b'-')
        })
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

fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}
