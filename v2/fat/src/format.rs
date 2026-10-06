//! Making an exFAT volume: the layout `mkfs.exfat` and Windows produce, with one FAT, an allocation bitmap, an up-case
//! table and an empty root directory.
//!
//! ```text
//! sector 0..11    main boot region: boot sector, 8 extended boot sectors, OEM parameters, reserved, checksum sector
//! sector 12..23   its backup copy
//! FatOffset       the FAT (one copy): entries 0 and 1, then chains for the bitmap, up-case table and root directory
//! ClusterHeap...  cluster 2: allocation bitmap, then the up-case table, then the root directory (one cluster)
//! ```
//!
//! The cluster size follows the volume size the way Windows chooses it (4 KiB up to 256 MiB, 32 KiB up to 32 GiB, 128 KiB
//! above), which also suits recording: a big cluster means few FAT/bitmap updates. The heap starts on a cluster boundary
//! (relative to the start of the volume; the partition itself should be 1 MiB aligned, as the kernel's GPT writer makes it).

use crate::{Device, Error, SECTOR, Time};

/// The up-case table (see `v2/tools/gen_upcase.py`).
const UPCASE: &[u8] = include_bytes!("upcase.bin");
const FAT_OFFSET: u64 = 128;

/// Sectors per cluster for a volume of `sectors` sectors.
pub fn cluster_sectors(sectors: u64) -> u32 {
    let megabytes = sectors / 2048;
    if megabytes <= 256 {
        8
    } else if megabytes <= 32 * 1024 {
        64
    } else {
        256
    }
}

fn checksum_step(checksum: u32, byte: u8) -> u32 {
    (checksum << 31 | checksum >> 1).wrapping_add(u32::from(byte))
}

/// Writes `count` zero sectors from `lba`, 64 KiB at a time.
fn zero<D: Device>(device: &mut D, lba: u64, count: u64) -> Result<(), Error> {
    let zeros = [0u8; 64 * 1024];
    let mut done = 0;
    while done < count {
        let sectors = (count - done).min((zeros.len() / SECTOR) as u64);
        device.write(lba + done, &zeros[..sectors as usize * SECTOR])?;
        done += sectors;
    }
    Ok(())
}

/// Formats the volume of `sectors` sectors that starts at sector 0 of `device` as exFAT. `partition_offset` is where the
/// volume starts on its disk (recorded in the boot sector, informational), `label` up to 11 ASCII characters (may be
/// empty), `serial` the volume serial number. Everything the volume's structures occupy is written; the data area is not
/// cleared. Returns the number of clusters.
pub fn format_exfat<D: Device>(device: &mut D, sectors: u64, partition_offset: u64, label: &[u8], serial: u32, _time: Time) -> Result<u32, Error> {
    if label.len() > 11 || !label.iter().all(|&byte| (0x20..0x7f).contains(&byte) && !b"\"*/:<>?\\|".contains(&byte)) {
        return Err(Error::BadName);
    }
    let spc = u64::from(cluster_sectors(sectors));
    let cluster_bytes = spc * SECTOR as u64;
    if sectors < 8192 || sectors > u64::from(u32::MAX) * 8 {
        return Err(Error::Unsupported);
    }
    // The FAT has an entry per cluster (plus two); size it for the most clusters the volume could have.
    let most = (sectors - FAT_OFFSET) / spc;
    let fat_length = ((most + 2) * 4).div_ceil(SECTOR as u64);
    let heap_offset = (FAT_OFFSET + fat_length).div_ceil(spc) * spc;
    if heap_offset >= sectors {
        return Err(Error::NoSpace);
    }
    let clusters = ((sectors - heap_offset) / spc).min(0xffff_fff5);
    if clusters < 16 {
        return Err(Error::NoSpace);
    }
    let clusters = clusters as u32;
    let bitmap_bytes = u64::from(clusters).div_ceil(8);
    let bitmap_clusters = bitmap_bytes.div_ceil(cluster_bytes) as u32;
    let upcase_clusters = (UPCASE.len() as u64).div_ceil(cluster_bytes) as u32;
    let bitmap_first = 2u32;
    let upcase_first = bitmap_first + bitmap_clusters;
    let root = upcase_first + upcase_clusters;
    let used = root - 1; // clusters 2..=root are in use
    let cluster_lba = |cluster: u32| heap_offset + u64::from(cluster - 2) * spc;

    // The FAT: zero it all, then the two reserved entries and the three chains.
    zero(device, FAT_OFFSET, fat_length)?;
    let mut fat = [0u8; SECTOR];
    let mut put = |entry: u32, value: u32, fat: &mut [u8; SECTOR]| fat[entry as usize * 4..entry as usize * 4 + 4].copy_from_slice(&value.to_le_bytes());
    // 100 clusters of chain fit in the first sector for any normal volume; longer ones (a huge bitmap) are written below.
    put(0, 0xffff_fff8, &mut fat);
    put(1, 0xffff_ffff, &mut fat);
    device.write(FAT_OFFSET, &fat)?;
    let mut chains = [(bitmap_first, bitmap_clusters), (upcase_first, upcase_clusters), (root, 1)];
    for (first, count) in chains.iter_mut() {
        let (first, count) = (*first, *count);
        let mut sector_index = u64::MAX;
        let mut sector = [0u8; SECTOR];
        for cluster in first..first + count {
            let value = if cluster + 1 == first + count { 0xffff_ffff } else { cluster + 1 };
            let index = u64::from(cluster) * 4 / SECTOR as u64;
            if index != sector_index {
                if sector_index != u64::MAX {
                    device.write(FAT_OFFSET + sector_index, &sector)?;
                }
                device.read(FAT_OFFSET + index, &mut sector)?;
                sector_index = index;
            }
            let at = (u64::from(cluster) * 4 % SECTOR as u64) as usize;
            sector[at..at + 4].copy_from_slice(&value.to_le_bytes());
        }
        device.write(FAT_OFFSET + sector_index, &sector)?;
    }

    // The allocation bitmap: clusters 2..=root are set.
    zero(device, cluster_lba(bitmap_first), u64::from(bitmap_clusters) * spc)?;
    let mut sector = [0u8; SECTOR];
    let mut remaining = used;
    let mut index = 0u64;
    while remaining > 0 {
        sector.fill(0);
        let bits = remaining.min(SECTOR as u32 * 8);
        for bit in 0..bits {
            sector[bit as usize / 8] |= 1 << (bit % 8);
        }
        device.write(cluster_lba(bitmap_first) + index, &sector)?;
        remaining -= bits;
        index += 1;
    }

    // The up-case table.
    let mut written = 0;
    let mut lba = cluster_lba(upcase_first);
    while written < UPCASE.len() {
        sector.fill(0);
        let count = (UPCASE.len() - written).min(SECTOR);
        sector[..count].copy_from_slice(&UPCASE[written..written + count]);
        device.write(lba, &sector)?;
        written += count;
        lba += 1;
    }
    let mut upcase_checksum = 0u32;
    for &byte in UPCASE {
        upcase_checksum = checksum_step(upcase_checksum, byte);
    }

    // The root directory: label, bitmap, up-case table.
    zero(device, cluster_lba(root), spc)?;
    sector.fill(0);
    let mut entry = 0;
    if !label.is_empty() {
        sector[0] = 0x83;
        sector[1] = label.len() as u8;
        for (index, &byte) in label.iter().enumerate() {
            sector[2 + index * 2..4 + index * 2].copy_from_slice(&u16::from(byte).to_le_bytes());
        }
        entry = 1;
    }
    let at = entry * 32;
    sector[at] = 0x81;
    sector[at + 20..at + 24].copy_from_slice(&bitmap_first.to_le_bytes());
    sector[at + 24..at + 32].copy_from_slice(&bitmap_bytes.to_le_bytes());
    let at = (entry + 1) * 32;
    sector[at] = 0x82;
    sector[at + 4..at + 8].copy_from_slice(&upcase_checksum.to_le_bytes());
    sector[at + 20..at + 24].copy_from_slice(&upcase_first.to_le_bytes());
    sector[at + 24..at + 32].copy_from_slice(&(UPCASE.len() as u64).to_le_bytes());
    device.write(cluster_lba(root), &sector)?;

    // The boot region, written last: a volume without a valid boot sector is not mounted by anything.
    let mut region = [0u8; 12 * SECTOR];
    let boot = &mut region[..SECTOR];
    boot[0..3].copy_from_slice(&[0xeb, 0x76, 0x90]);
    boot[3..11].copy_from_slice(b"EXFAT   ");
    boot[64..72].copy_from_slice(&partition_offset.to_le_bytes());
    boot[72..80].copy_from_slice(&sectors.to_le_bytes());
    boot[80..84].copy_from_slice(&(FAT_OFFSET as u32).to_le_bytes());
    boot[84..88].copy_from_slice(&(fat_length as u32).to_le_bytes());
    boot[88..92].copy_from_slice(&(heap_offset as u32).to_le_bytes());
    boot[92..96].copy_from_slice(&clusters.to_le_bytes());
    boot[96..100].copy_from_slice(&root.to_le_bytes());
    boot[100..104].copy_from_slice(&serial.to_le_bytes());
    boot[104..106].copy_from_slice(&0x0100u16.to_le_bytes()); // exFAT 1.00
    boot[108] = 9; // 512-byte sectors
    boot[109] = spc.trailing_zeros() as u8;
    boot[110] = 1; // one FAT
    boot[111] = 0x80;
    boot[112] = 0xff; // percent in use: unknown
    boot[120..510].fill(0xf4); // "boot code": halt
    boot[510] = 0x55;
    boot[511] = 0xaa;
    for extended in 1..9 {
        region[extended * SECTOR + 508..extended * SECTOR + 512].copy_from_slice(&0xaa55_0000u32.to_le_bytes());
    }
    // The checksum of sectors 0..=10 (not the volume flags and percent-in-use bytes), repeated through sector 11.
    let mut checksum = 0u32;
    for (index, &byte) in region[..11 * SECTOR].iter().enumerate() {
        if index == 106 || index == 107 || index == 112 {
            continue;
        }
        checksum = checksum_step(checksum, byte);
    }
    for slot in 0..SECTOR / 4 {
        region[11 * SECTOR + slot * 4..11 * SECTOR + slot * 4 + 4].copy_from_slice(&checksum.to_le_bytes());
    }
    device.write(12, &region)?; // the backup first
    device.write(0, &region)?;
    Ok(clusters)
}
