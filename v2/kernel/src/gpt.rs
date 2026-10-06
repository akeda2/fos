//! Reading a disk's GUID Partition Table: find a partition by its type GUID (and
//! optionally its unique GUID). Only the primary table is read; the loader and the
//! image builder write both copies.

use fos_v2_abi::FOSFS_TYPE_GUID;

use crate::block;

#[derive(Clone, Copy)]
pub struct Partition {
    pub unique_guid: [u8; 16],
    pub start: u64,
    pub sectors: u64,
}

/// Calls `visit` for every partition of `type_guid` on `disk`, until it returns true.
pub fn find(disk: usize, type_guid: &[u8; 16], mut accept: impl FnMut(&Partition) -> bool) -> Option<Partition> {
    let mut header = [0u8; 512];
    block::read(disk, 1, &mut header).ok()?;
    if &header[0..8] != b"EFI PART" {
        return None;
    }
    let entries_lba = u64::from_le_bytes(header[72..80].try_into().unwrap());
    let entry_count = u32::from_le_bytes(header[80..84].try_into().unwrap()) as usize;
    let entry_size = u32::from_le_bytes(header[84..88].try_into().unwrap()) as usize;
    if !(128..=512).contains(&entry_size) || entry_count == 0 || entry_count > 256 {
        return None;
    }
    // One sector at a time: entries never straddle a sector for sizes that divide 512.
    let per_sector = 512 / entry_size;
    let mut sector = [0u8; 512];
    for index in 0..entry_count {
        if index % per_sector == 0 {
            block::read(disk, entries_lba + (index / per_sector) as u64, &mut sector).ok()?;
        }
        let entry = &sector[(index % per_sector) * entry_size..][..entry_size];
        if &entry[0..16] != type_guid {
            continue;
        }
        let first = u64::from_le_bytes(entry[32..40].try_into().unwrap());
        let last = u64::from_le_bytes(entry[40..48].try_into().unwrap());
        let partition = Partition { unique_guid: entry[16..32].try_into().unwrap(), start: first, sectors: last.saturating_sub(first) + 1 };
        if accept(&partition) {
            return Some(partition);
        }
    }
    None
}

/// What a disk's first sectors say it holds, for `format` to decide with.
pub enum Summary {
    /// Sectors 0 and 1 are all zero: nothing has ever been put on it.
    Blank,
    /// A GPT: its first FOSFS partition (if any) and how many other partitions it lists.
    Gpt { fosfs: Option<Partition>, other: usize },
    /// Something else (an MBR, a FAT or ext boot sector, ...).
    Unknown,
}

pub fn inspect(disk: usize) -> Option<Summary> {
    let mut sectors = [0u8; 1024];
    block::read(disk, 0, &mut sectors).ok()?;
    if &sectors[512..520] != b"EFI PART" {
        return Some(if sectors.iter().all(|&byte| byte == 0) { Summary::Blank } else { Summary::Unknown });
    }
    let entries_lba = u64::from_le_bytes(sectors[512 + 72..512 + 80].try_into().unwrap());
    let entry_count = u32::from_le_bytes(sectors[512 + 80..512 + 84].try_into().unwrap()) as usize;
    let entry_size = u32::from_le_bytes(sectors[512 + 84..512 + 88].try_into().unwrap()) as usize;
    if !(128..=512).contains(&entry_size) || entry_count == 0 || entry_count > 256 {
        return Some(Summary::Unknown);
    }
    let per_sector = 512 / entry_size;
    let mut sector = [0u8; 512];
    let (mut fosfs, mut other) = (None, 0);
    for index in 0..entry_count {
        if index % per_sector == 0 {
            block::read(disk, entries_lba + (index / per_sector) as u64, &mut sector).ok()?;
        }
        let entry = &sector[(index % per_sector) * entry_size..][..entry_size];
        if entry[0..16].iter().all(|&byte| byte == 0) {
            continue; // an unused slot
        }
        if &entry[0..16] == &FOSFS_TYPE_GUID && fosfs.is_none() {
            let first = u64::from_le_bytes(entry[32..40].try_into().unwrap());
            let last = u64::from_le_bytes(entry[40..48].try_into().unwrap());
            fosfs = Some(Partition { unique_guid: entry[16..32].try_into().unwrap(), start: first, sectors: last.saturating_sub(first) + 1 });
        } else {
            other += 1;
        }
    }
    Some(Summary::Gpt { fosfs, other })
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

/// A random version-4 GUID. There is no hardware random source guaranteed on every machine,
/// so this mixes the time-stamp counter, the tick count and the caller's salt through a
/// splitmix64 generator: not secret, only meant to make two disks formatted here differ.
fn random_guid(salt: u64) -> [u8; 16] {
    let mut state = unsafe { core::arch::x86_64::_rdtsc() } ^ salt.rotate_left(32) ^ crate::apic::ticks().wrapping_mul(0x9e37_79b9_7f4a_7c15);
    let mut bytes = [0u8; 16];
    for half in bytes.chunks_mut(8) {
        state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = state;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        half.copy_from_slice(&(value ^ (value >> 31)).to_le_bytes());
    }
    bytes[7] = (bytes[7] & 0x0f) | 0x40; // version 4 (in the GPT's mixed-endian layout the field sits here)
    bytes[8] = (bytes[8] & 0x3f) | 0x80; // RFC 4122 variant
    bytes
}

static mut ENTRY_ARRAY: [u8; 128 * 128] = [0; 128 * 128];

/// Replaces the disk's partition table (and sector 0) with a protective MBR and a GPT holding
/// one FOSFS partition from sector 2048 to the last usable sector. Nothing else on the disk
/// is touched, but the old partitions become unreachable. Order: backup copy first, primary
/// header last, so an interrupted write never leaves a valid-looking table that is half new.
pub fn write_single_fosfs_partition(disk: usize) -> Result<Partition, block::Error> {
    write_single_partition(disk, &FOSFS_TYPE_GUID, "FOSFS data")
}

/// The same for a partition of any type (`exFAT data` uses the basic-data type GUID, which is what PCs expect).
pub fn write_single_partition(disk: usize, type_guid: &[u8; 16], name: &str) -> Result<Partition, block::Error> {
    let total = block::get(disk).ok_or(block::Error::NoSuchDisk)?.sectors;
    if total < 8192 {
        return Err(block::Error::OutOfRange);
    }
    let first = 2048u64;
    let last = total - 34; // the backup entry array (32 sectors) and header (1) follow it
    let partition = Partition { unique_guid: random_guid(total), start: first, sectors: last - first + 1 };
    let entries = unsafe { &mut *core::ptr::addr_of_mut!(ENTRY_ARRAY) };
    entries.fill(0);
    entries[0..16].copy_from_slice(type_guid);
    entries[16..32].copy_from_slice(&partition.unique_guid);
    entries[32..40].copy_from_slice(&first.to_le_bytes());
    entries[40..48].copy_from_slice(&last.to_le_bytes());
    for (index, unit) in name.encode_utf16().enumerate() {
        entries[56 + 2 * index..58 + 2 * index].copy_from_slice(&unit.to_le_bytes());
    }
    let entries_crc = crc32(entries);
    let disk_guid = random_guid(total ^ 0x5555);

    let header = |current: u64, backup: u64, entries_lba: u64| {
        let mut sector = [0u8; 512];
        sector[0..8].copy_from_slice(b"EFI PART");
        sector[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes()); // revision 1.0
        sector[12..16].copy_from_slice(&92u32.to_le_bytes()); // header size
        sector[24..32].copy_from_slice(&current.to_le_bytes());
        sector[32..40].copy_from_slice(&backup.to_le_bytes());
        sector[40..48].copy_from_slice(&34u64.to_le_bytes()); // first usable LBA
        sector[48..56].copy_from_slice(&(total - 34).to_le_bytes()); // last usable LBA
        sector[56..72].copy_from_slice(&disk_guid);
        sector[72..80].copy_from_slice(&entries_lba.to_le_bytes());
        sector[80..84].copy_from_slice(&128u32.to_le_bytes()); // entry count
        sector[84..88].copy_from_slice(&128u32.to_le_bytes()); // entry size
        sector[88..92].copy_from_slice(&entries_crc.to_le_bytes());
        let crc = crc32(&sector[..92]); // computed with the CRC field itself still zero
        sector[16..20].copy_from_slice(&crc.to_le_bytes());
        sector
    };
    let backup_entries = total - 33;
    block::write(disk, backup_entries, entries)?;
    block::write(disk, total - 1, &header(total - 1, 1, backup_entries))?;
    block::write(disk, 2, entries)?;
    let mut mbr = [0u8; 512];
    mbr[446 + 1..446 + 4].copy_from_slice(&[0x00, 0x02, 0x00]); // CHS of sector 1 (as the image builder writes it)
    mbr[446 + 4] = 0xee; // one partition of type "GPT protective"
    mbr[446 + 5..446 + 8].copy_from_slice(&[0xff, 0xff, 0xff]); // CHS end: beyond the CHS range
    mbr[446 + 8..446 + 12].copy_from_slice(&1u32.to_le_bytes());
    mbr[446 + 12..446 + 16].copy_from_slice(&(u32::try_from(total - 1).unwrap_or(u32::MAX)).to_le_bytes());
    mbr[510] = 0x55;
    mbr[511] = 0xaa;
    block::write(disk, 0, &mbr)?;
    block::write(disk, 1, &header(1, total - 1, 2))?;
    Ok(partition)
}
