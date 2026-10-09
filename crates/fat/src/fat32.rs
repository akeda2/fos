//! FAT32: mounting, the root directory (short and long names), and the one-run-at-a-time writer.

use crate::name::{self, Short};
use crate::{Device, Error, Entry, Fs, Kind, SECTOR, Stream, Time, Visit};

const ATTR_LONG_NAME: u8 = 0x0f;
const ATTR_ARCHIVE: u8 = 0x20;
const ATTR_DIRECTORY: u8 = 0x10;
const ATTR_VOLUME_ID: u8 = 0x08;
const EOC: u32 = 0x0fff_ffff;
const MAX_FILE: u64 = 0xffff_ffff;
/// Where in the root directory an entry set was found: its sector, byte offset and the number of entries.
#[derive(Clone, Copy)]
struct Location {
    lba: u64,
    offset: usize,
    entries: usize,
}

fn le16(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}
fn le32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

impl<D: Device> Fs<D> {
    pub(crate) fn mount_fat32(&mut self, boot: &[u8; SECTOR]) -> Result<(), Error> {
        let bytes_per_sector = le16(boot, 11);
        let spc = u32::from(boot[13]);
        let reserved = u64::from(le16(boot, 14));
        let fats = u32::from(boot[16]);
        let root_entries = le16(boot, 17);
        let fat16_size = le16(boot, 22);
        let total = if le16(boot, 19) != 0 { u64::from(le16(boot, 19)) } else { u64::from(le32(boot, 32)) };
        let fat_sectors = le32(boot, 36);
        if root_entries != 0 || fat16_size != 0 || fat_sectors == 0 {
            return Err(Error::NotFat); // FAT12/16
        }
        if bytes_per_sector != SECTOR as u16 {
            return Err(Error::Unsupported);
        }
        if spc == 0 || !spc.is_power_of_two() || fats == 0 || fats > 2 {
            return Err(Error::NotFat);
        }
        let data_lba = reserved + u64::from(fats) * u64::from(fat_sectors);
        if total <= data_lba {
            return Err(Error::NotFat);
        }
        let clusters = ((total - data_lba) / u64::from(spc)) as u32;
        if clusters < 65_525 {
            return Err(Error::NotFat);
        }
        self.kind = Kind::Fat32;
        self.spc = spc;
        self.data_lba = data_lba;
        self.clusters = clusters;
        self.fat_lba = reserved;
        self.fat_sectors = fat_sectors;
        self.fats = fats;
        self.root = le32(boot, 44);
        self.dir = self.root;
        self.fsinfo = u32::from(le16(boot, 48));
        if self.root < 2 || self.root > clusters + 1 {
            return Err(Error::NotFat);
        }
        Ok(())
    }

    /// Visits every file and directory of the root directory (long names assembled) until `visit` returns true.
    pub(crate) fn fat32_walk(&mut self, visit: &mut dyn FnMut(&Entry) -> bool) -> Result<(), Error> {
        let mut long = [0u8; 64];
        let mut long_length = 0usize;
        let mut long_checksum = 0u8;
        let mut long_entries = 0usize;
        let mut long_valid = false;
        self.root_sectors(|lba, sector| {
            for slot in 0..SECTOR / 32 {
                let entry = &sector[slot * 32..slot * 32 + 32];
                if entry[0] == 0 {
                    return Visit::Done(()); // end of directory
                }
                if entry[0] == 0xe5 {
                    long_valid = false;
                    continue;
                }
                if entry[11] & 0x3f == ATTR_LONG_NAME {
                    let sequence = entry[0];
                    if sequence & 0x40 != 0 {
                        long = [0; 64];
                        long_length = 0;
                        long_checksum = entry[13];
                        long_entries = 0;
                        long_valid = true;
                    }
                    let index = usize::from(sequence & 0x1f).wrapping_sub(1);
                    if !long_valid || index >= 5 || entry[13] != long_checksum {
                        long_valid = false;
                        continue;
                    }
                    long_entries += 1;
                    for (position, at) in [1usize, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30].into_iter().enumerate() {
                        let unit = le16(entry, at);
                        let target = index * 13 + position;
                        if unit == 0 {
                            long_length = long_length.max(target);
                            break;
                        }
                        if unit != 0xffff && target < 64 {
                            long[target] = if unit < 0x80 { unit as u8 } else { b'?' };
                            long_length = long_length.max(target + 1);
                        }
                    }
                    continue;
                }
                if entry[11] & ATTR_VOLUME_ID != 0 {
                    long_valid = false;
                    continue;
                }
                let mut short_name = [0u8; 11];
                short_name.copy_from_slice(&entry[..11]);
                let has_long = long_valid && name::short_checksum(&short_name) == long_checksum;
                let mut text = [0u8; 13];
                let length = name::short_text(&short_name, entry[12], &mut text);
                let mut found = Entry::new(if has_long { &long[..long_length] } else { &text[..length] });
                found.size = u64::from(le32(entry, 28));
                found.first_cluster = (u32::from(le16(entry, 20)) << 16) | u32::from(le16(entry, 26));
                found.is_directory = entry[11] & ATTR_DIRECTORY != 0;
                found.modified = Time::from_dos(le16(entry, 24), le16(entry, 22));
                found.set_lba = lba;
                found.set_offset = slot * 32;
                found.set_entries = if has_long { long_entries + 1 } else { 1 };
                long_valid = false;
                if visit(&found) {
                    return Visit::Done(());
                }
            }
            Visit::Continue
        })?;
        Ok(())
    }

    pub(crate) fn fat32_remove(&mut self, file: &Entry) -> Result<(), Error> {
        // Free the chain, then mark the entries deleted.
        self.fat32_free_chain(file.first_cluster)?;
        self.fat32_drop_entry(file)
    }

    /// Marks a file's directory entries deleted (0xE5), leaving its clusters as they are.
    pub(crate) fn fat32_drop_entry(&mut self, file: &Entry) -> Result<(), Error> {
        let mut sector = [0u8; SECTOR];
        self.device.read(file.set_lba, &mut sector)?;
        let last = file.set_offset / 32;
        let first = last.saturating_sub(file.set_entries - 1);
        for slot in first..=last {
            sector[slot * 32] = 0xe5;
        }
        self.device.write(file.set_lba, &sector)
    }

    /// Frees the clusters of the chain starting at `first` (consecutive clusters in one pass over the FAT).
    pub(crate) fn fat32_free_chain(&mut self, first: u32) -> Result<(), Error> {
        let mut cluster = first;
        let (mut run_start, mut run_length) = (cluster, 0u32);
        let mut guard = 0u32;
        while cluster >= 2 && cluster <= self.clusters + 1 {
            let next = self.chain_next(cluster)?;
            run_length += 1;
            if next != Some(cluster + 1) {
                self.fat_set_range(run_start, run_length, |_| 0)?;
                run_length = 0;
                if let Some(next) = next {
                    run_start = next;
                }
            }
            match next {
                Some(next) => cluster = next,
                None => break,
            }
            guard += 1;
            if guard > self.clusters {
                return Err(Error::Corrupt);
            }
        }
        Ok(())
    }

    /// See `Fs::replace`: the one-sector switch, then the clean-up.
    pub(crate) fn fat32_replace(&mut self, old: &Entry, new: &Entry) -> Result<(), Error> {
        let mut sector = [0u8; SECTOR];
        self.device.read(old.set_lba, &mut sector)?;
        let at = old.set_offset; // the short entry (the last of the set)
        let (date, time) = self.time.dos();
        sector[at + 22..at + 24].copy_from_slice(&time.to_le_bytes());
        sector[at + 24..at + 26].copy_from_slice(&date.to_le_bytes());
        sector[at + 18..at + 20].copy_from_slice(&date.to_le_bytes());
        sector[at + 20..at + 22].copy_from_slice(&((new.first_cluster >> 16) as u16).to_le_bytes());
        sector[at + 26..at + 28].copy_from_slice(&(new.first_cluster as u16).to_le_bytes());
        sector[at + 28..at + 32].copy_from_slice(&(new.size as u32).to_le_bytes());
        self.device.write(old.set_lba, &sector)?; // the switch: this one sector decides which contents the name has
        self.fat32_drop_entry(new)?;
        if old.first_cluster >= 2 && old.first_cluster != new.first_cluster {
            self.fat32_free_chain(old.first_cluster)?;
        }
        self.invalidate_fsinfo()
    }

    /// Finds a run of free clusters: the first one with at least `want` clusters, else the longest there is.
    fn find_run(&mut self, want: u32) -> Result<(u32, u32), Error> {
        let (mut best_start, mut best_count) = (0u32, 0u32);
        let (mut run_start, mut run_count) = (0u32, 0u32);
        let last = self.clusters + 2; // exclusive
        let mut cluster = 2u32;
        let mut buffer = [0u8; 4096];
        while cluster < last {
            let sector = u64::from(cluster) * 4 / SECTOR as u64;
            let count = ((self.fat_sectors as u64 - sector).min(8)) as usize;
            self.device.read(self.fat_lba + sector, &mut buffer[..count * SECTOR])?;
            let mut at = (u64::from(cluster) * 4 % SECTOR as u64) as usize;
            while at < count * SECTOR && cluster < last {
                let free = le32(&buffer, at) & 0x0fff_ffff == 0;
                if free {
                    if run_count == 0 {
                        run_start = cluster;
                    }
                    run_count += 1;
                    if run_count >= want {
                        return Ok((run_start, run_count.min(want)));
                    }
                } else {
                    if run_count > best_count {
                        (best_start, best_count) = (run_start, run_count);
                    }
                    run_count = 0;
                }
                cluster += 1;
                at += 4;
            }
        }
        if run_count > best_count {
            (best_start, best_count) = (run_start, run_count);
        }
        Ok((best_start, best_count))
    }

    pub(crate) fn fat32_create(&mut self, name: &[u8], reserve: u64) -> Result<Stream, Error> {
        name::validate(name)?;
        let cluster_bytes = self.cluster_bytes();
        let limit = MAX_FILE / cluster_bytes; // whole clusters whose total is a legal file size
        let want = if reserve == 0 { limit } else { reserve.div_ceil(cluster_bytes).min(limit) }.min(u64::from(self.clusters)) as u32;
        let (start, count) = self.find_run(want.max(1))?;
        if count == 0 {
            return Err(Error::NoSpace);
        }
        // Claim the run: one chain through all of it, ending in the end-of-chain mark.
        self.fat_set_range(start, count, |cluster| if cluster == start + count - 1 { EOC } else { cluster + 1 })?;
        let location = match self.add_entry(name, start) {
            Ok(location) => location,
            Err(error) => {
                let _ = self.fat_set_range(start, count, |_| 0);
                return Err(error);
            }
        };
        Ok(Stream { first_cluster: start, clusters: count, capacity: (u64::from(count) * cluster_bytes).min(MAX_FILE), entry_lba: location.lba, entry_offset: location.offset, entries: location.entries })
    }

    /// Writes the directory entries for a new, empty file whose run starts at `first`.
    fn add_entry(&mut self, name: &[u8], first: u32) -> Result<Location, Error> {
        let short = match name::exact_short(name) {
            Some(short) => short,
            None => self.unique_alias(name)?,
        };
        let long_entries = if short.needs_long { name.len().div_ceil(13) } else { 0 };
        let needed = long_entries + 1;
        let location = self.free_slots(needed)?;
        let checksum = name::short_checksum(&short.bytes);
        let (date, time) = self.time.dos();
        let mut sector = [0u8; SECTOR];
        self.device.read(location.lba, &mut sector)?;
        for index in 0..long_entries {
            // Highest sequence number first.
            let sequence = long_entries - index;
            let entry = &mut sector[location.offset + index * 32..location.offset + index * 32 + 32];
            entry.fill(0);
            entry[0] = sequence as u8 | if index == 0 { 0x40 } else { 0 };
            entry[11] = ATTR_LONG_NAME;
            entry[13] = checksum;
            for (position, at) in [1usize, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30].into_iter().enumerate() {
                let character = (sequence - 1) * 13 + position;
                let unit: u16 = match character.cmp(&name.len()) {
                    core::cmp::Ordering::Less => u16::from(name[character]),
                    core::cmp::Ordering::Equal => 0,
                    core::cmp::Ordering::Greater => 0xffff,
                };
                entry[at..at + 2].copy_from_slice(&unit.to_le_bytes());
            }
        }
        let at = location.offset + long_entries * 32;
        let entry = &mut sector[at..at + 32];
        entry.fill(0);
        entry[..11].copy_from_slice(&short.bytes);
        entry[11] = ATTR_ARCHIVE;
        entry[12] = short.case_flags;
        for field in [14usize, 22] {
            entry[field..field + 2].copy_from_slice(&time.to_le_bytes());
        }
        for field in [16usize, 18, 24] {
            entry[field..field + 2].copy_from_slice(&date.to_le_bytes());
        }
        let _ = first;
        // Size 0 and no cluster for now; `set_length` fills both in with the first block.
        self.device.write(location.lba, &sector)?;
        Ok(Location { lba: location.lba, offset: at, entries: needed })
    }

    /// An alias `BASE~N.EXT` no existing entry uses.
    fn unique_alias(&mut self, name: &[u8]) -> Result<Short, Error> {
        for tail in 1..1000 {
            let candidate = name::alias(name, tail);
            let taken = self.root_sectors(|_, sector| {
                for slot in 0..SECTOR / 32 {
                    let entry = &sector[slot * 32..slot * 32 + 11];
                    if entry[0] == 0 {
                        return Visit::Done(false);
                    }
                    if entry[0] != 0xe5 && sector[slot * 32 + 11] & 0x3f != ATTR_LONG_NAME && entry == candidate.bytes {
                        return Visit::Done(true);
                    }
                }
                Visit::Continue
            })?;
            if taken != Some(true) {
                return Ok(candidate);
            }
        }
        Err(Error::NoSpace)
    }

    /// `count` consecutive free slots inside one sector of the root directory, growing it by a cluster if need be.
    fn free_slots(&mut self, count: usize) -> Result<Location, Error> {
        if let Some((lba, offset)) = self.find_free_entries(count, |first| first == 0 || first == 0xe5, 0xe5)? {
            return Ok(Location { lba, offset, entries: count });
        }
        // Full: chain one more cluster onto the directory.
        let (cluster, found) = self.find_run(1)?;
        if found == 0 {
            return Err(Error::NoSpace);
        }
        self.extend_root(cluster)?;
        Ok(Location { lba: self.cluster_lba(cluster), offset: 0, entries: count })
    }

    pub(crate) fn fat32_set_length(&mut self, stream: &Stream, length: u64) -> Result<(), Error> {
        let mut sector = [0u8; SECTOR];
        self.device.read(stream.entry_lba, &mut sector)?;
        let at = stream.entry_offset;
        sector[at + 28..at + 32].copy_from_slice(&(length as u32).to_le_bytes());
        let (date, time) = self.time.dos();
        sector[at + 22..at + 24].copy_from_slice(&time.to_le_bytes());
        sector[at + 24..at + 26].copy_from_slice(&date.to_le_bytes());
        sector[at + 18..at + 20].copy_from_slice(&date.to_le_bytes());
        // The entry points at the run from the first length on (until then the file is empty, with no cluster).
        sector[at + 20..at + 22].copy_from_slice(&((stream.first_cluster >> 16) as u16).to_le_bytes());
        sector[at + 26..at + 28].copy_from_slice(&(stream.first_cluster as u16).to_le_bytes());
        self.device.write(stream.entry_lba, &sector)
    }

    pub(crate) fn fat32_finish(&mut self, stream: &Stream, name: &[u8], length: u64) -> Result<(), Error> {
        let _ = name;
        let cluster_bytes = self.cluster_bytes();
        let needed = length.div_ceil(cluster_bytes) as u32;
        if needed > stream.clusters || length > stream.capacity {
            return Err(Error::OutOfRange);
        }
        if needed == 0 {
            // Nothing was recorded: remove the file altogether.
            self.fat_set_range(stream.first_cluster, stream.clusters, |_| 0)?;
            let mut sector = [0u8; SECTOR];
            self.device.read(stream.entry_lba, &mut sector)?;
            let last = stream.entry_offset / 32;
            for slot in (last + 1 - stream.entries)..=last {
                sector[slot * 32] = 0xe5;
            }
            return self.device.write(stream.entry_lba, &sector);
        }
        if needed < stream.clusters {
            // Cut the chain after the last needed cluster and free the rest.
            let last = stream.first_cluster + needed - 1;
            self.fat_set_range(last, 1, |_| EOC)?;
            self.fat_set_range(last + 1, stream.clusters - needed, |_| 0)?;
        }
        self.fat32_set_length(stream, length)?;
        self.invalidate_fsinfo()
    }

    /// The free-cluster count in FSInfo is no longer right; mark it unknown so that the next reader recounts.
    fn invalidate_fsinfo(&mut self) -> Result<(), Error> {
        if self.fsinfo == 0 || self.fsinfo == 0xffff {
            return Ok(());
        }
        let mut sector = [0u8; SECTOR];
        self.device.read(u64::from(self.fsinfo), &mut sector)?;
        if le32(&sector, 0) == 0x4161_5252 && le32(&sector, 484) == 0x6141_7272 {
            sector[488..496].fill(0xff);
            self.device.write(u64::from(self.fsinfo), &sector)?;
        }
        Ok(())
    }
}

