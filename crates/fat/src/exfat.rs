//! exFAT: mounting, the root directory's file entry sets, the allocation bitmap, and the contiguous-run writer.
//!
//! A file here is an entry set: a File entry (0x85), a Stream Extension (0xC0) and File Name entries (0xC1), 15 UTF-16
//! characters each, with a checksum over the set. A recording is written as `NoFatChain` (flag 0x02 in the stream):
//! its clusters are consecutive, the FAT is not touched, and only the allocation bitmap says they are in use.

use crate::name;
use crate::{Device, Entry, Error, Fs, Kind, SECTOR, Stream, Time, Visit};

fn le16(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}
fn le32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}
fn le64(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

const NO_FAT_CHAIN: u8 = 0x02;
const ALLOCATION_POSSIBLE: u8 = 0x01;

/// The checksum over an entry set (the bytes 2 and 3 of the first entry, which hold it, are skipped).
fn set_checksum(set: &[u8]) -> u16 {
    let mut checksum = 0u16;
    for (index, &byte) in set.iter().enumerate() {
        if index == 2 || index == 3 {
            continue;
        }
        checksum = (checksum << 15 | checksum >> 1).wrapping_add(u16::from(byte));
    }
    checksum
}

impl<D: Device> Fs<D> {
    pub(crate) fn mount_exfat(&mut self, boot: &[u8; SECTOR]) -> Result<(), Error> {
        if boot[108] != 9 {
            return Err(Error::Unsupported); // not 512-byte sectors
        }
        if boot[110] != 1 {
            return Err(Error::Unsupported); // TexFAT
        }
        let cluster_shift = boot[109];
        if cluster_shift > 16 {
            return Err(Error::NotFat);
        }
        self.kind = Kind::ExFat;
        self.spc = 1 << cluster_shift;
        self.fat_lba = u64::from(le32(boot, 80));
        self.fat_sectors = le32(boot, 84);
        self.fats = 1;
        self.data_lba = u64::from(le32(boot, 88));
        self.clusters = le32(boot, 92);
        self.root = le32(boot, 96);
        self.dir = self.root;
        if self.root < 2 || self.root > self.clusters + 1 || self.clusters == 0 {
            return Err(Error::NotFat);
        }
        // The allocation bitmap is described by a directory entry (type 0x81) in the root directory.
        let found = self.root_sectors(|_, sector| {
            for slot in 0..SECTOR / 32 {
                let entry = &sector[slot * 32..slot * 32 + 32];
                if entry[0] == 0 {
                    return Visit::Done(None);
                }
                if entry[0] == 0x81 {
                    return Visit::Done(Some((le32(entry, 20), le64(entry, 24))));
                }
            }
            Visit::Continue
        })?;
        let Some(Some((cluster, bytes))) = found else { return Err(Error::NotFat) };
        if cluster < 2 || cluster > self.clusters + 1 || bytes < u64::from(self.clusters).div_ceil(8) {
            return Err(Error::NotFat);
        }
        self.bitmap_cluster = cluster;
        self.bitmap_bytes = bytes;
        Ok(())
    }

    /// The volume label (the type 0x83 entry of the root directory) as ASCII; `None` without one. Characters outside ASCII
    /// come back as `?`.
    pub(crate) fn exfat_label(&mut self) -> Result<Option<([u8; 11], usize)>, Error> {
        let found = self.root_sectors(|_, sector| {
            for slot in 0..SECTOR / 32 {
                let entry = &sector[slot * 32..slot * 32 + 32];
                match entry[0] {
                    0 => return Visit::Done(None),
                    0x83 => {
                        let count = usize::from(entry[1]).min(11);
                        let mut label = [0u8; 11];
                        for (index, byte) in label.iter_mut().enumerate().take(count) {
                            let unit = u16::from_le_bytes([entry[2 + index * 2], entry[3 + index * 2]]);
                            *byte = if (0x20..0x7f).contains(&unit) { unit as u8 } else { b'?' };
                        }
                        return Visit::Done(Some((label, count)));
                    }
                    _ => {}
                }
            }
            Visit::Continue
        })?;
        Ok(found.flatten())
    }

    /// Visits every file and directory of the root directory until `visit` returns true. Sets from other writers may
    /// cross sector boundaries; this walks them with a small state machine.
    pub(crate) fn exfat_walk(&mut self, visit: &mut dyn FnMut(&Entry) -> bool) -> Result<(), Error> {
        let mut remaining = 0usize; // secondary entries still to come in the current set
        let mut current = Entry::new(b"");
        let mut name_length = 0usize;
        let mut text = [0u8; 64];
        let mut length = 0usize;
        self.root_sectors(|lba, sector| {
            for slot in 0..SECTOR / 32 {
                let entry = &sector[slot * 32..slot * 32 + 32];
                let kind = entry[0];
                if remaining == 0 {
                    if kind == 0 {
                        return Visit::Done(());
                    }
                    if kind == 0x85 {
                        remaining = usize::from(entry[1]);
                        current = Entry::new(b"");
                        current.set_lba = lba;
                        current.set_offset = slot * 32;
                        current.set_entries = 1 + remaining;
                        current.is_directory = le16(entry, 4) & 0x10 != 0;
                        current.modified = Time::from_dos((le32(entry, 12) >> 16) as u16, le32(entry, 12) as u16);
                        name_length = 0;
                        length = 0;
                    }
                    continue;
                }
                remaining -= 1;
                match kind {
                    0xc0 => {
                        current.contiguous = entry[1] & NO_FAT_CHAIN != 0;
                        name_length = usize::from(entry[3]);
                        current.size = le64(entry, 24);
                        current.first_cluster = le32(entry, 20);
                    }
                    0xc1 => {
                        for index in 0..15 {
                            let unit = le16(entry, 2 + index * 2);
                            if length < 64 {
                                text[length] = if unit < 0x80 { unit as u8 } else { b'?' };
                                length += 1;
                            }
                        }
                    }
                    _ => {}
                }
                if remaining == 0 {
                    let mut found = current;
                    let shown = name_length.min(length);
                    found.name = Entry::new(&text[..shown]).name;
                    found.name_length = shown;
                    if visit(&found) {
                        return Visit::Done(());
                    }
                }
            }
            Visit::Continue
        })?;
        Ok(())
    }

    /// Marks `count` clusters from `start` allocated or free in the bitmap.
    fn bitmap_mark(&mut self, start: u32, count: u32, allocated: bool) -> Result<(), Error> {
        let mut cluster = start;
        let end = start + count;
        let mut sector = [0u8; SECTOR];
        let bitmap_lba = self.cluster_lba(self.bitmap_cluster);
        while cluster < end {
            let bit = u64::from(cluster - 2);
            let index = bit / (SECTOR as u64 * 8);
            self.device.read(bitmap_lba + index, &mut sector)?;
            while cluster < end && u64::from(cluster - 2) / (SECTOR as u64 * 8) == index {
                let bit = (u64::from(cluster - 2) % (SECTOR as u64 * 8)) as usize;
                if allocated {
                    sector[bit / 8] |= 1 << (bit % 8);
                } else {
                    sector[bit / 8] &= !(1 << (bit % 8));
                }
                cluster += 1;
            }
            self.device.write(bitmap_lba + index, &sector)?;
        }
        Ok(())
    }

    /// The first run of at least `want` free clusters, else the longest run (`want` 0 asks for the longest).
    fn exfat_find_run(&mut self, want: u32) -> Result<(u32, u32), Error> {
        let (mut best_start, mut best_count) = (0u32, 0u32);
        let (mut run_start, mut run_count) = (0u32, 0u32);
        let bitmap_lba = self.cluster_lba(self.bitmap_cluster);
        let total_bytes = u64::from(self.clusters).div_ceil(8);
        let mut buffer = [0u8; 4096];
        let mut done_bytes = 0u64;
        let mut cluster = 2u32;
        while done_bytes < total_bytes {
            let sectors = (total_bytes - done_bytes).div_ceil(SECTOR as u64).min(8) as usize;
            self.device.read(bitmap_lba + done_bytes / SECTOR as u64, &mut buffer[..sectors * SECTOR])?;
            let bytes = (sectors * SECTOR).min((total_bytes - done_bytes) as usize);
            for &byte in &buffer[..bytes] {
                if byte == 0xff && cluster + 8 <= self.clusters + 2 {
                    if run_count > best_count {
                        (best_start, best_count) = (run_start, run_count);
                    }
                    run_count = 0;
                    cluster += 8;
                    continue;
                }
                for bit in 0..8 {
                    if cluster >= self.clusters + 2 {
                        break;
                    }
                    if byte & (1 << bit) == 0 {
                        if run_count == 0 {
                            run_start = cluster;
                        }
                        run_count += 1;
                        if want != 0 && run_count >= want {
                            return Ok((run_start, want));
                        }
                    } else {
                        if run_count > best_count {
                            (best_start, best_count) = (run_start, run_count);
                        }
                        run_count = 0;
                    }
                    cluster += 1;
                }
            }
            done_bytes += bytes as u64;
        }
        if run_count > best_count {
            (best_start, best_count) = (run_start, run_count);
        }
        Ok((best_start, best_count))
    }

    /// (free bytes in all, bytes of the longest free run) from the allocation bitmap.
    pub(crate) fn exfat_free_space(&mut self) -> Result<(u64, u64), Error> {
        let (mut free, mut longest, mut run) = (0u64, 0u64, 0u64);
        let bitmap_lba = self.cluster_lba(self.bitmap_cluster);
        let total_bytes = u64::from(self.clusters).div_ceil(8);
        let mut buffer = [0u8; 4096];
        let mut done_bytes = 0u64;
        let mut cluster = 0u32; // counted from the first cluster
        while done_bytes < total_bytes {
            let sectors = (total_bytes - done_bytes).div_ceil(SECTOR as u64).min(8) as usize;
            self.device.read(bitmap_lba + done_bytes / SECTOR as u64, &mut buffer[..sectors * SECTOR])?;
            let bytes = (sectors * SECTOR).min((total_bytes - done_bytes) as usize);
            for &byte in &buffer[..bytes] {
                for bit in 0..8 {
                    if cluster >= self.clusters {
                        break;
                    }
                    if byte & (1 << bit) == 0 {
                        free += 1;
                        run += 1;
                        longest = longest.max(run);
                    } else {
                        run = 0;
                    }
                    cluster += 1;
                }
            }
            done_bytes += bytes as u64;
        }
        let cluster_bytes = self.cluster_bytes();
        Ok((free * cluster_bytes, longest * cluster_bytes))
    }

    pub(crate) fn exfat_create(&mut self, name: &[u8], reserve: u64) -> Result<Stream, Error> {
        name::validate(name)?;
        let cluster_bytes = self.cluster_bytes();
        let want = if reserve == 0 { 0 } else { reserve.div_ceil(cluster_bytes).min(u64::from(self.clusters)) as u32 };
        let (start, count) = self.exfat_find_run(want)?;
        if count == 0 {
            return Err(Error::NoSpace);
        }
        self.bitmap_mark(start, count, true)?;
        let entries = 2 + name.len().div_ceil(15);
        let found = match self.exfat_free_entries(entries) {
            Ok(found) => found,
            Err(error) => {
                let _ = self.bitmap_mark(start, count, false);
                return Err(error);
            }
        };
        let (lba, offset) = found;
        // Size 0 and no cluster until the first length is set (with it, `set_length` points the entry at the run).
        self.exfat_write_set(lba, offset, name, 0x20, ALLOCATION_POSSIBLE | NO_FAT_CHAIN, 0, 0)?;
        Ok(Stream { first_cluster: start, clusters: count, capacity: u64::from(count) * cluster_bytes, entry_lba: lba, entry_offset: offset, entries })
    }

    /// Writes a complete entry set for `name` at (`lba`, `offset`): attributes `attributes` (0x20 archive, 0x10 directory),
    /// stream flags, first cluster and data length (valid length is the same), stamped with the current time.
    fn exfat_write_set(&mut self, lba: u64, offset: usize, name: &[u8], attributes: u16, flags: u8, first_cluster: u32, length: u64) -> Result<(), Error> {
        let (date, time) = self.time.dos();
        let stamp = (u32::from(date) << 16) | u32::from(time);
        let mut head = [0u8; 64];
        head[0] = 0x85;
        head[4..6].copy_from_slice(&attributes.to_le_bytes());
        for field in [8usize, 12, 16] {
            head[field..field + 4].copy_from_slice(&stamp.to_le_bytes());
        }
        head[32] = 0xc0;
        head[33] = flags;
        head[40..48].copy_from_slice(&length.to_le_bytes());
        head[52..56].copy_from_slice(&first_cluster.to_le_bytes());
        head[56..64].copy_from_slice(&length.to_le_bytes());
        self.exfat_put_set(lba, offset, &head, name)
    }

    /// Writes an entry set at (`lba`, `offset`) from `head` (the file entry and the stream extension, 64 bytes) and the
    /// name entries for `name`: the entry count, name length, hash and checksum are filled in here. The set must lie
    /// inside the sector (what `exfat_free_entries` and a set found by `exfat_walk` both guarantee for our names).
    fn exfat_put_set(&mut self, lba: u64, offset: usize, head: &[u8; 64], name: &[u8]) -> Result<(), Error> {
        let entries = 2 + name.len().div_ceil(15);
        if offset + entries * 32 > SECTOR {
            return Err(Error::Unsupported);
        }
        let mut sector = [0u8; SECTOR];
        self.device.read(lba, &mut sector)?;
        let set = &mut sector[offset..offset + entries * 32];
        set.fill(0);
        set[..64].copy_from_slice(head);
        set[1] = (entries - 1) as u8;
        set[35] = name.len() as u8;
        set[36..38].copy_from_slice(&name::exfat_hash(name).to_le_bytes());
        for (index, chunk) in name.chunks(15).enumerate() {
            let at = 64 + index * 32;
            set[at] = 0xc1;
            for (position, &byte) in chunk.iter().enumerate() {
                set[at + 2 + position * 2..at + 4 + position * 2].copy_from_slice(&u16::from(byte).to_le_bytes());
            }
        }
        let checksum = set_checksum(set);
        set[2..4].copy_from_slice(&checksum.to_le_bytes());
        self.device.write(lba, &sector)
    }

    /// A new directory: one zeroed cluster with its own end-of-chain mark, claimed in the bitmap, and only then the
    /// entry set that makes it visible (a cut before that leaks a cluster, which `fsck.exfat` repairs).
    pub(crate) fn exfat_mkdir(&mut self, name: &[u8]) -> Result<(), Error> {
        name::validate(name)?;
        if self.entry(name)?.is_some() {
            return Err(Error::Exists);
        }
        let (cluster, found) = self.exfat_find_run(1)?;
        if found == 0 {
            return Err(Error::NoSpace);
        }
        self.bitmap_mark(cluster, 1, true)?;
        let prepared = self.zero_cluster(cluster).and_then(|_| self.fat_set_range(cluster, 1, |_| 0xffff_ffff));
        let placed = prepared.and_then(|_| self.exfat_free_entries(2 + name.len().div_ceil(15)));
        let (lba, offset) = match placed {
            Ok(place) => place,
            Err(error) => {
                let _ = self.bitmap_mark(cluster, 1, false);
                return Err(error);
            }
        };
        let bytes = self.cluster_bytes();
        self.exfat_write_set(lba, offset, name, 0x10, ALLOCATION_POSSIBLE, cluster, bytes)
    }

    /// Turns the current contiguous (NoFatChain) directory into a FAT-chained one: the FAT is written first, then the flag
    /// in its entry set is cleared, so a cut in between leaves a directory that still reads as contiguous.
    pub(crate) fn exfat_unchain_dir(&mut self) -> Result<(), Error> {
        let Some((lba, offset, entries)) = self.dir_set else { return Err(Error::NoSpace) };
        let (first, count) = (self.dir, self.dir_clusters.max(1));
        self.fat_set_range(first, count, |cluster| if cluster == first + count - 1 { 0xffff_ffff } else { cluster + 1 })?;
        let mut sector = [0u8; SECTOR];
        self.device.read(lba, &mut sector)?;
        let set = &mut sector[offset..offset + entries * 32];
        set[33] &= !NO_FAT_CHAIN;
        let checksum = set_checksum(set);
        set[2..4].copy_from_slice(&checksum.to_le_bytes());
        self.device.write(lba, &sector)?;
        self.dir_contiguous = false;
        Ok(())
    }

    /// The current (non-root, FAT-chained) directory got another cluster: its own entry records the size, so add it.
    pub(crate) fn exfat_dir_grew(&mut self) -> Result<(), Error> {
        let Some((lba, offset, entries)) = self.dir_set else { return Ok(()) };
        let bytes = self.cluster_bytes();
        let mut sector = [0u8; SECTOR];
        self.device.read(lba, &mut sector)?;
        let set = &mut sector[offset..offset + entries * 32];
        let length = le64(set, 56) + bytes;
        set[40..48].copy_from_slice(&length.to_le_bytes());
        set[56..64].copy_from_slice(&length.to_le_bytes());
        let checksum = set_checksum(set);
        set[2..4].copy_from_slice(&checksum.to_le_bytes());
        self.device.write(lba, &sector)
    }

    /// Room for an entry set of `entries` entries in one sector of the root directory (growing it if full).
    fn exfat_free_entries(&mut self, entries: usize) -> Result<(u64, usize), Error> {
        if let Some(place) = self.find_free_entries(entries, |first| first & 0x80 == 0, 0x05)? {
            return Ok(place);
        }
        let (cluster, found) = self.exfat_find_run(1)?;
        if found == 0 {
            return Err(Error::NoSpace);
        }
        self.bitmap_mark(cluster, 1, true)?;
        self.extend_root(cluster)?;
        Ok((self.cluster_lba(cluster), 0))
    }

    pub(crate) fn exfat_set_length(&mut self, stream: &Stream, length: u64) -> Result<(), Error> {
        let mut sector = [0u8; SECTOR];
        self.device.read(stream.entry_lba, &mut sector)?;
        let set = &mut sector[stream.entry_offset..stream.entry_offset + stream.entries * 32];
        set[33] = ALLOCATION_POSSIBLE | NO_FAT_CHAIN;
        let (date, time) = self.time.dos();
        let stamp = (u32::from(date) << 16) | u32::from(time);
        set[12..16].copy_from_slice(&stamp.to_le_bytes()); // last modified
        set[16..20].copy_from_slice(&stamp.to_le_bytes()); // last accessed
        set[40..48].copy_from_slice(&length.to_le_bytes()); // valid data length
        set[52..56].copy_from_slice(&stream.first_cluster.to_le_bytes());
        set[56..64].copy_from_slice(&length.to_le_bytes()); // data length
        let checksum = set_checksum(set);
        set[2..4].copy_from_slice(&checksum.to_le_bytes());
        self.device.write(stream.entry_lba, &sector)
    }

    pub(crate) fn exfat_finish(&mut self, stream: &Stream, name: &[u8], length: u64) -> Result<(), Error> {
        let _ = name;
        let cluster_bytes = self.cluster_bytes();
        let needed = length.div_ceil(cluster_bytes) as u32;
        if needed > stream.clusters || length > stream.capacity {
            return Err(Error::OutOfRange);
        }
        if needed == 0 {
            // Nothing was recorded: free the run and delete the entries.
            self.bitmap_mark(stream.first_cluster, stream.clusters, false)?;
            return self.mark_deleted(stream.entry_lba, stream.entry_offset, stream.entries);
        }
        if needed < stream.clusters {
            self.bitmap_mark(stream.first_cluster + needed, stream.clusters - needed, false)?;
        }
        self.exfat_set_length(stream, length)
    }

    /// Clears the "in use" bit of every entry of a set (a deletion).
    fn mark_deleted(&mut self, lba: u64, offset: usize, entries: usize) -> Result<(), Error> {
        let mut lba = lba;
        let mut slot = offset / 32;
        let mut sector = [0u8; SECTOR];
        self.device.read(lba, &mut sector)?;
        for _ in 0..entries {
            if slot == SECTOR / 32 {
                self.device.write(lba, &sector)?;
                // Only inside the same cluster: a set crossing a cluster boundary is not handled.
                if (lba - self.data_lba) % u64::from(self.spc) == u64::from(self.spc) - 1 {
                    return Err(Error::Unsupported);
                }
                lba += 1;
                slot = 0;
                self.device.read(lba, &mut sector)?;
            }
            sector[slot * 32] &= 0x7f;
            slot += 1;
        }
        self.device.write(lba, &sector)
    }

    pub(crate) fn exfat_remove(&mut self, file: &Entry) -> Result<(), Error> {
        let cluster_bytes = self.cluster_bytes();
        let clusters = file.size.div_ceil(cluster_bytes) as u32;
        if file.first_cluster >= 2 && clusters > 0 {
            if file.contiguous {
                self.bitmap_mark(file.first_cluster, clusters, false)?;
            } else {
                let mut cluster = file.first_cluster;
                for _ in 0..clusters {
                    self.bitmap_mark(cluster, 1, false)?;
                    match self.chain_next(cluster)? {
                        Some(next) => cluster = next,
                        None => break,
                    }
                }
            }
        }
        self.mark_deleted(file.set_lba, file.set_offset, file.set_entries)
    }

    /// The file and stream-extension entries of a set (everything but the name), for moving it.
    fn exfat_head(&mut self, entry: &Entry) -> Result<[u8; 64], Error> {
        if entry.set_offset + entry.set_entries * 32 > SECTOR {
            return Err(Error::Unsupported);
        }
        let mut sector = [0u8; SECTOR];
        self.device.read(entry.set_lba, &mut sector)?;
        let mut head = [0u8; 64];
        head.copy_from_slice(&sector[entry.set_offset..entry.set_offset + 64]);
        Ok(head)
    }

    /// Renames `from` to `to` in the current directory. The same number of name entries rewrites the set in one sector
    /// write (a power cut leaves the old name or the new); otherwise the new set is written first and the old one
    /// deleted after, and a cut between leaves both names on one chain, which `fsck.exfat` tidies up. False when `to`
    /// is taken by another entry.
    pub(crate) fn exfat_rename(&mut self, from: &[u8], to: &[u8]) -> Result<bool, Error> {
        name::validate(to)?;
        let old = self.entry(from)?.ok_or(Error::NotFound)?;
        if self.entry(to)?.is_some_and(|other| (other.set_lba, other.set_offset) != (old.set_lba, old.set_offset)) {
            return Ok(false);
        }
        let head = self.exfat_head(&old)?;
        if 2 + to.len().div_ceil(15) == old.set_entries {
            self.exfat_put_set(old.set_lba, old.set_offset, &head, to)?;
        } else {
            let (lba, offset) = self.exfat_free_entries(2 + to.len().div_ceil(15))?;
            self.exfat_put_set(lba, offset, &head, to)?;
            self.mark_deleted(old.set_lba, old.set_offset, old.set_entries)?;
        }
        Ok(true)
    }

    /// The entry set of `name` for `exfat_put`, which writes it into another directory.
    pub(crate) fn exfat_park(&mut self, name: &[u8]) -> Result<Option<[u8; 64]>, Error> {
        match self.entry(name)? {
            Some(entry) => self.exfat_head(&entry).map(Some),
            None => Ok(None),
        }
    }

    /// Writes a parked entry set into the current directory as `name`; false if the name is taken.
    pub(crate) fn exfat_put(&mut self, head: &[u8; 64], name: &[u8]) -> Result<bool, Error> {
        name::validate(name)?;
        if self.entry(name)?.is_some() {
            return Ok(false);
        }
        let (lba, offset) = self.exfat_free_entries(2 + name.len().div_ceil(15))?;
        self.exfat_put_set(lba, offset, head, name)?;
        Ok(true)
    }

    /// Deletes the entry set of `name` but keeps its clusters (they now belong to the set `exfat_put` wrote).
    pub(crate) fn exfat_forget(&mut self, name: &[u8]) -> Result<bool, Error> {
        match self.entry(name)? {
            Some(entry) => self.mark_deleted(entry.set_lba, entry.set_offset, entry.set_entries).map(|()| true),
            None => Ok(false),
        }
    }

    /// Sets (or, with an empty `label`, removes) the volume label: the 0x83 entry of the root directory, up to 11 characters.
    /// It reuses the existing label entry (or a cleared one, type 0x03) and otherwise takes a free slot, so a change is
    /// one sector write. The root is entered for the change and the current directory restored.
    pub(crate) fn exfat_set_label(&mut self, label: &[u8]) -> Result<(), Error> {
        if label.len() > 11 || !label.iter().all(|&byte| (0x20..0x7f).contains(&byte) && !b"\"*/:<>?\\|".contains(&byte)) {
            return Err(Error::BadName);
        }
        let saved = (self.dir, self.dir_contiguous, self.dir_clusters, self.dir_set);
        self.enter_root();
        let outcome = self.exfat_write_label(label);
        (self.dir, self.dir_contiguous, self.dir_clusters, self.dir_set) = saved;
        outcome
    }

    fn exfat_write_label(&mut self, label: &[u8]) -> Result<(), Error> {
        let existing = self.root_sectors(|lba, sector| {
            for slot in 0..SECTOR / 32 {
                match sector[slot * 32] {
                    0 => return Visit::Done(None),
                    0x83 | 0x03 => return Visit::Done(Some((lba, slot * 32))),
                    _ => {}
                }
            }
            Visit::Continue
        })?;
        let (lba, offset) = match existing.flatten() {
            Some(place) => place,
            None if label.is_empty() => return Ok(()),
            None => self.exfat_free_entries(1)?,
        };
        let mut sector = [0u8; SECTOR];
        self.device.read(lba, &mut sector)?;
        let entry = &mut sector[offset..offset + 32];
        entry.fill(0);
        if label.is_empty() {
            entry[0] = 0x03;
        } else {
            entry[0] = 0x83;
            entry[1] = label.len() as u8;
            for (index, &byte) in label.iter().enumerate() {
                entry[2 + index * 2..4 + index * 2].copy_from_slice(&u16::from(byte).to_le_bytes());
            }
        }
        self.device.write(lba, &sector)
    }
}
