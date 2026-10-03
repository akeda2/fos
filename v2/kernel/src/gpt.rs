//! Reading a disk's GUID Partition Table: find a partition by its type GUID (and
//! optionally its unique GUID). Only the primary table is read; the loader and the
//! image builder write both copies.

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
