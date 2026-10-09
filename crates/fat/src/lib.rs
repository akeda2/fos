//! FAT32 and exFAT, for a recorder that must write files a PC can read.
//!
//! What it does, deliberately and only:
//!
//! - mounts a FAT32 or exFAT volume on 512-byte sectors (a partition or a whole-disk "superfloppy"; the caller
//!   gives it a `Device` whose sector 0 is the volume's first sector);
//! - looks files up by name in the **root directory** (case-insensitive, long names included) and reads them;
//! - creates one file at a time as a **single contiguous run of clusters** reserved up front, writes into it at
//!   sector-aligned offsets, and ends it at its true length, giving the unused tail back. That is exactly what a
//!   recording needs: no allocation work while the audio is flowing (exFAT sets "no FAT chain"; FAT32's chain is
//!   written once when the run is claimed), and the directory entry's size is the only thing that moves.
//!
//! What it does not do: creating subdirectories, renames, files that grow by chaining fragments, 4 KiB sectors, TexFAT, or
//! non-ASCII names. On FAT32 it can *enter* an existing subdirectory (`enter`: the loader's `\EFI\BOOT` on an EFI System
//! Partition) and swap a file's contents for another file's in one sector write (`replace`). A name with characters outside printable ASCII is refused rather than mangled.
//!
//! Power-cut behaviour: the directory entry's size only ever advances to what has been written (the caller passes
//! the length with each write), so after a cut the file holds everything up to its last length update. On FAT32 the
//! reserved run stays chained to the file, which `fsck` reports as a harmless size/chain mismatch and trims; on exFAT
//! the unused part of the run stays marked allocated in the bitmap until a check frees it.
#![cfg_attr(not(test), no_std)]

mod exfat;
mod format;
pub use format::{cluster_sectors, format_exfat};
mod fat32;
mod name;

pub const SECTOR: usize = 512;

/// A block device: whole sectors, addressed from the first sector of the volume.
pub trait Device {
    fn read(&mut self, lba: u64, buffer: &mut [u8]) -> Result<(), Error>;
    fn write(&mut self, lba: u64, buffer: &[u8]) -> Result<(), Error>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The device failed a read or a write.
    Device,
    /// Not a FAT32 or exFAT volume (or one this code cannot handle: other sector sizes, FAT12/16).
    NotFat,
    Unsupported,
    NotFound,
    /// No free run of clusters, or the root directory cannot grow.
    NoSpace,
    /// A name this code will not create (empty, too long, non-ASCII, or with a character FAT forbids).
    BadName,
    /// Structures that contradict themselves (a chain that loops, a bad cluster number).
    Corrupt,
    /// A write outside the reserved run.
    OutOfRange,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Fat32,
    ExFat,
}

/// A timestamp for new directory entries (the kernel has no clock yet, so callers pass a fixed one).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Time {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

impl Time {
    pub const DEFAULT: Time = Time { year: 2026, month: 1, day: 1, hour: 0, minute: 0, second: 0 };

    /// The FAT date (bits 15-9 year since 1980, 8-5 month, 4-0 day) and time (15-11 hour, 10-5 minute, 4-0 seconds/2).
    /// The inverse of `dos`; None for a stamp that is not a date (month 0, day 0, ...).
    pub(crate) fn from_dos(date: u16, time: u16) -> Option<Time> {
        let (month, day) = (((date >> 5) & 15) as u8, (date & 31) as u8);
        let (hour, minute, second) = ((time >> 11) as u8, ((time >> 5) & 63) as u8, ((time & 31) * 2) as u8);
        if month == 0 || month > 12 || day == 0 || hour > 23 || minute > 59 || second > 59 {
            return None;
        }
        Some(Time { year: 1980 + (date >> 9), month, day, hour, minute, second })
    }

    pub(crate) fn dos(&self) -> (u16, u16) {
        let date = ((self.year.saturating_sub(1980)) << 9) | (u16::from(self.month.clamp(1, 12)) << 5) | u16::from(self.day.clamp(1, 31));
        let time = (u16::from(self.hour.min(23)) << 11) | (u16::from(self.minute.min(59)) << 5) | u16::from(self.second.min(59) / 2);
        (date, time)
    }
}

/// A root directory entry, as `list` and `find` see it.
#[derive(Clone, Copy)]
pub struct Entry {
    name: [u8; 64],
    name_length: usize,
    pub size: u64,
    pub first_cluster: u32,
    pub contiguous: bool,
    pub is_directory: bool,
    /// The last-modified stamp, if the entry holds a valid one.
    pub modified: Option<Time>,
    /// Where the entry set starts (sector, byte offset) and how many entries it has.
    pub(crate) set_lba: u64,
    pub(crate) set_offset: usize,
    pub(crate) set_entries: usize,
}

impl Entry {
    pub(crate) fn new(name: &[u8]) -> Entry {
        let mut stored = [0u8; 64];
        let length = name.len().min(64);
        stored[..length].copy_from_slice(&name[..length]);
        Entry { name: stored, name_length: length, size: 0, first_cluster: 0, contiguous: false, is_directory: false, modified: None, set_lba: 0, set_offset: 0, set_entries: 0 }
    }

    /// The name as ASCII (characters outside it are shown as `?`).
    pub fn name(&self) -> &[u8] {
        &self.name[..self.name_length]
    }

    pub fn file(&self) -> File {
        File { size: self.size, first_cluster: self.first_cluster, contiguous: self.contiguous }
    }
}

/// A file found by `find`: enough to read it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct File {
    pub size: u64,
    pub first_cluster: u32,
    /// The clusters are consecutive (exFAT's "no FAT chain"); otherwise they are followed through the FAT.
    pub contiguous: bool,
}

/// A file being written: one reserved run of clusters and where its directory entry is.
#[derive(Clone, Copy, Debug)]
pub struct Stream {
    pub(crate) first_cluster: u32,
    pub(crate) clusters: u32,
    /// Bytes the run can hold (capped for FAT32 at 4 GiB - 1).
    pub(crate) capacity: u64,
    /// The sector holding the file's directory entries, the byte offset of the entry set in it and its length in
    /// entries (the short entry is the last on FAT32, the first on exFAT).
    pub(crate) entry_lba: u64,
    pub(crate) entry_offset: usize,
    pub(crate) entries: usize,
}

impl Stream {
    /// The largest length this file can reach.
    pub fn capacity(&self) -> u64 {
        self.capacity
    }
}

pub struct Fs<D: Device> {
    pub(crate) device: D,
    pub(crate) kind: Kind,
    /// Sectors per cluster and the first sector of cluster 2.
    pub(crate) spc: u32,
    pub(crate) data_lba: u64,
    /// Number of clusters (cluster numbers run from 2 to `clusters + 1`).
    pub(crate) clusters: u32,
    pub(crate) fat_lba: u64,
    pub(crate) fat_sectors: u32,
    pub(crate) fats: u32,
    pub(crate) root: u32,
    /// The directory every lookup, listing and creation works on: the root until `enter` says otherwise.
    pub(crate) dir: u32,
    /// FAT32: the FSInfo sector (0 = none). exFAT: the allocation bitmap's first cluster and length in bytes.
    pub(crate) fsinfo: u32,
    pub(crate) bitmap_cluster: u32,
    pub(crate) bitmap_bytes: u64,
    pub(crate) time: Time,
    /// The FAT sector last read (a walk down a long chain reads each FAT sector once).
    pub(crate) fat_cache_lba: u64,
    pub(crate) fat_cache: [u8; SECTOR],
}

impl<D: Device> Fs<D> {
    pub fn mount(mut device: D) -> Result<Fs<D>, Error> {
        let mut boot = [0u8; SECTOR];
        device.read(0, &mut boot)?;
        if boot[510] != 0x55 || boot[511] != 0xaa {
            return Err(Error::NotFat);
        }
        let mut fs = Fs {
            device, kind: Kind::Fat32, spc: 0, data_lba: 0, clusters: 0, fat_lba: 0, fat_sectors: 0, fats: 0, root: 0, dir: 0, fsinfo: 0, bitmap_cluster: 0, bitmap_bytes: 0, time: Time::DEFAULT,
            fat_cache_lba: u64::MAX, fat_cache: [0; SECTOR],
        };
        if &boot[3..11] == b"EXFAT   " {
            fs.mount_exfat(&boot)?;
        } else {
            fs.mount_fat32(&boot)?;
        }
        Ok(fs)
    }

    pub fn kind(&self) -> Kind {
        self.kind
    }

    /// The time stamped on files created from now on.
    pub fn set_time(&mut self, time: Time) {
        self.time = time;
    }

    pub fn cluster_bytes(&self) -> u64 {
        u64::from(self.spc) * SECTOR as u64
    }

    /// The volume label is not read; this is the number of clusters (for a free-space estimate by callers).
    pub fn cluster_count(&self) -> u32 {
        self.clusters
    }

    pub fn into_device(self) -> D {
        self.device
    }

    /// Makes the subdirectory `name` of the current directory the current one: lookups, listings, `create` and `remove` then
    /// work in it. FAT32 only (an EFI System Partition is FAT32 by specification). `enter_root` goes back.
    pub fn enter(&mut self, name: &[u8]) -> Result<(), Error> {
        if self.kind != Kind::Fat32 {
            return Err(Error::Unsupported);
        }
        match self.entry(name)? {
            Some(entry) if entry.is_directory => {
                // A subdirectory's ".." entry says cluster 0 for the root; a named entry never does, but be safe.
                self.dir = if entry.first_cluster < 2 { self.root } else { entry.first_cluster };
                Ok(())
            }
            Some(_) => Err(Error::NotFound),
            None => Err(Error::NotFound),
        }
    }

    pub fn enter_root(&mut self) {
        self.dir = self.root;
    }

    /// Makes the file `target` hold what the file `source` holds, in one sector write: the directory entry of `target` is
    /// pointed at `source`'s clusters (and takes its size and time), so after a power cut the volume has either the old
    /// contents or the new, never half of each. Only then is `source`'s own entry dropped (its clusters now belong to
    /// `target`) and `target`'s old clusters freed. A cut between the steps leaves two entries sharing one chain, or a
    /// leaked old chain, which a file system check tidies up; `forget_alias` finishes the first case. FAT32 only.
    pub fn replace(&mut self, target: &[u8], source: &[u8]) -> Result<(), Error> {
        if self.kind != Kind::Fat32 {
            return Err(Error::Unsupported);
        }
        let old = self.entry(target)?.ok_or(Error::NotFound)?;
        let new = self.entry(source)?.ok_or(Error::NotFound)?;
        if old.is_directory || new.is_directory || new.first_cluster < 2 || new.size == 0 {
            return Err(Error::Unsupported);
        }
        self.fat32_replace(&old, &new)
    }

    /// Finishes an interrupted `replace`: if the file `alias` and the file `of` share their first cluster, `alias`'s entry is
    /// dropped without freeing the clusters (they belong to `of`). False if there is nothing to do.
    pub fn forget_alias(&mut self, alias: &[u8], of: &[u8]) -> Result<bool, Error> {
        if self.kind != Kind::Fat32 {
            return Err(Error::Unsupported);
        }
        let (Some(alias), Some(of)) = (self.entry(alias)?, self.entry(of)?) else { return Ok(false) };
        if alias.first_cluster < 2 || alias.first_cluster != of.first_cluster {
            return Ok(false);
        }
        self.fat32_drop_entry(&alias)?;
        Ok(true)
    }

    /// Looks `name` up in the current directory (the root until `enter`).
    pub fn find(&mut self, name: &[u8]) -> Result<Option<File>, Error> {
        Ok(self.entry(name)?.map(|entry| entry.file()))
    }

    /// The root directory entry called `name` (any case), or None.
    pub fn entry(&mut self, name: &[u8]) -> Result<Option<Entry>, Error> {
        let mut found = None;
        self.walk(&mut |entry| {
            if crate::name::eq_ignore_case(entry.name(), name) {
                found = Some(*entry);
                return true;
            }
            false
        })?;
        Ok(found)
    }

    /// Calls `visit` for every file and directory in the root directory, in directory order.
    pub fn list(&mut self, mut visit: impl FnMut(&Entry)) -> Result<(), Error> {
        self.walk(&mut |entry| {
            visit(entry);
            false
        })
    }

    /// Removes the file `name` (its clusters are freed). False if there is no such file.
    pub fn remove(&mut self, name: &[u8]) -> Result<bool, Error> {
        match self.entry(name)? {
            Some(entry) if !entry.is_directory => {
                self.remove_file(&entry)?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// (free bytes in all, bytes of the longest free run) -- the second is what one recording can use. exFAT only: on
    /// FAT32 this would mean reading the whole FAT, which a slow stick makes take a long time, so it says `Unsupported`.
    pub fn free_space(&mut self) -> Result<(u64, u64), Error> {
        match self.kind {
            Kind::ExFat => self.exfat_free_space(),
            Kind::Fat32 => Err(Error::Unsupported),
        }
    }

    /// Walks the root directory until `visit` returns true.
    fn walk(&mut self, visit: &mut dyn FnMut(&Entry) -> bool) -> Result<(), Error> {
        match self.kind {
            Kind::Fat32 => self.fat32_walk(visit),
            Kind::ExFat => self.exfat_walk(visit),
        }
    }

    /// Reads from `file` at byte `offset` into `buffer`; returns the bytes read (0 at the end).
    pub fn read(&mut self, file: &File, offset: u64, buffer: &mut [u8]) -> Result<usize, Error> {
        if offset >= file.size {
            return Ok(0);
        }
        let wanted = buffer.len().min((file.size - offset) as usize);
        let mut done = 0;
        let cluster_bytes = self.cluster_bytes();
        let mut cluster = file.first_cluster;
        // Walk to the cluster holding `offset`.
        for _ in 0..offset / cluster_bytes {
            cluster = self.next_cluster(cluster, file.contiguous)?;
        }
        let mut within = offset % cluster_bytes;
        let mut sector_buffer = [0u8; SECTOR];
        while done < wanted {
            let lba = self.cluster_lba(cluster) + within / SECTOR as u64;
            let in_sector = (within % SECTOR as u64) as usize;
            let count = (SECTOR - in_sector).min(wanted - done);
            if in_sector == 0 && count == SECTOR {
                self.device.read(lba, &mut buffer[done..done + SECTOR])?;
            } else {
                self.device.read(lba, &mut sector_buffer)?;
                buffer[done..done + count].copy_from_slice(&sector_buffer[in_sector..in_sector + count]);
            }
            done += count;
            within += count as u64;
            if within == cluster_bytes && done < wanted {
                cluster = self.next_cluster(cluster, file.contiguous)?;
                within = 0;
            }
        }
        Ok(done)
    }

    /// Creates `name` (replacing a file of that name) with one contiguous run big enough for `reserve` bytes, or
    /// the biggest run there is when `reserve` is 0 (never more than the format allows). The file's length is 0 until
    /// `write` sets it. Returns the stream and the most it can hold.
    pub fn create(&mut self, name: &[u8], reserve: u64) -> Result<Stream, Error> {
        if let Some(existing) = self.entry(name)? {
            if existing.is_directory {
                return Err(Error::BadName);
            }
            self.remove_file(&existing)?;
        }
        match self.kind {
            Kind::Fat32 => self.fat32_create(name, reserve),
            Kind::ExFat => self.exfat_create(name, reserve),
        }
    }

    /// Writes `data` at byte `offset` (a multiple of 512) inside the run; the last sector is padded with zeros. If
    /// `length` is not 0 it becomes the file's length (it may only grow while writing; `finish` sets the final one).
    pub fn write(&mut self, stream: &Stream, offset: u64, data: &[u8], length: u64) -> Result<(), Error> {
        if offset % SECTOR as u64 != 0 || offset + data.len() as u64 > stream.clusters as u64 * self.cluster_bytes() {
            return Err(Error::OutOfRange);
        }
        if length > stream.capacity {
            return Err(Error::OutOfRange);
        }
        let lba = self.cluster_lba(stream.first_cluster) + offset / SECTOR as u64;
        let whole = data.len() / SECTOR * SECTOR;
        if whole > 0 {
            self.device.write(lba, &data[..whole])?;
        }
        if whole < data.len() {
            let mut tail = [0u8; SECTOR];
            tail[..data.len() - whole].copy_from_slice(&data[whole..]);
            self.device.write(lba + (whole / SECTOR) as u64, &tail)?;
        }
        if length != 0 {
            self.set_length(stream, length)?;
        }
        Ok(())
    }

    /// Ends the file at `length` bytes and frees the rest of the run. A length of 0 removes the file.
    pub fn finish(&mut self, stream: &Stream, name: &[u8], length: u64) -> Result<(), Error> {
        match self.kind {
            Kind::Fat32 => self.fat32_finish(stream, name, length),
            Kind::ExFat => self.exfat_finish(stream, name, length),
        }
    }

    fn set_length(&mut self, stream: &Stream, length: u64) -> Result<(), Error> {
        match self.kind {
            Kind::Fat32 => self.fat32_set_length(stream, length),
            Kind::ExFat => self.exfat_set_length(stream, length),
        }
    }

    fn remove_file(&mut self, entry: &Entry) -> Result<(), Error> {
        match self.kind {
            Kind::Fat32 => self.fat32_remove(entry),
            Kind::ExFat => self.exfat_remove(entry),
        }
    }

    pub(crate) fn cluster_lba(&self, cluster: u32) -> u64 {
        self.data_lba + u64::from(cluster - 2) * u64::from(self.spc)
    }

    /// The cluster after `cluster` in a file's run: the next number for a contiguous file, else the FAT's answer.
    fn next_cluster(&mut self, cluster: u32, contiguous: bool) -> Result<u32, Error> {
        if contiguous {
            return Ok(cluster + 1);
        }
        let next = self.fat_get(cluster)?;
        if next < 2 || next > self.clusters + 1 {
            return Err(Error::Corrupt);
        }
        Ok(next)
    }

    /// One FAT entry (both formats use 32-bit entries; FAT32 keeps only 28 bits of them).
    pub(crate) fn fat_get(&mut self, cluster: u32) -> Result<u32, Error> {
        let byte = u64::from(cluster) * 4;
        let lba = self.fat_lba + byte / SECTOR as u64;
        if self.fat_cache_lba != lba {
            self.device.read(lba, &mut self.fat_cache)?;
            self.fat_cache_lba = lba;
        }
        let at = (byte % SECTOR as u64) as usize;
        let sector = &self.fat_cache;
        let value = u32::from_le_bytes([sector[at], sector[at + 1], sector[at + 2], sector[at + 3]]);
        Ok(if self.kind == Kind::Fat32 { value & 0x0fff_ffff } else { value })
    }

    /// Sets FAT entries `first..first + count` to `value(cluster)` in every copy of the FAT (FAT32 leaves the top 4
    /// bits of each entry as they were, as the specification asks).
    pub(crate) fn fat_set_range(&mut self, first: u32, count: u32, mut value: impl FnMut(u32) -> u32) -> Result<(), Error> {
        self.fat_cache_lba = u64::MAX;
        let mut cluster = first;
        let end = first + count;
        let mut sector = [0u8; SECTOR];
        while cluster < end {
            let byte = u64::from(cluster) * 4;
            let index = byte / SECTOR as u64;
            self.device.read(self.fat_lba + index, &mut sector)?;
            while cluster < end && u64::from(cluster) * 4 / SECTOR as u64 == index {
                let at = (u64::from(cluster) * 4 % SECTOR as u64) as usize;
                let new = value(cluster);
                let old = u32::from_le_bytes([sector[at], sector[at + 1], sector[at + 2], sector[at + 3]]);
                let merged = if self.kind == Kind::Fat32 { (old & 0xf000_0000) | (new & 0x0fff_ffff) } else { new };
                sector[at..at + 4].copy_from_slice(&merged.to_le_bytes());
                cluster += 1;
            }
            for copy in 0..self.fats {
                self.device.write(self.fat_lba + u64::from(copy) * u64::from(self.fat_sectors) + index, &sector)?;
            }
        }
        Ok(())
    }

    /// Zeroes one cluster (a new directory block).
    pub(crate) fn zero_cluster(&mut self, cluster: u32) -> Result<(), Error> {
        let zeros = [0u8; SECTOR];
        let lba = self.cluster_lba(cluster);
        for sector in 0..self.spc {
            self.device.write(lba + u64::from(sector), &zeros)?;
        }
        Ok(())
    }
}

/// What a sector visitor decided.
pub(crate) enum Visit<T> {
    Continue,
    Done(T),
    /// The visitor changed the sector: write it back and go on.
    Rewrite,
}

impl<D: Device> Fs<D> {
    /// The next cluster of a directory (or file) chain, or None at its end.
    pub(crate) fn chain_next(&mut self, cluster: u32) -> Result<Option<u32>, Error> {
        let next = self.fat_get(cluster)?;
        let end = if self.kind == Kind::Fat32 { 0x0fff_fff8 } else { 0xffff_fff8 };
        if next >= end {
            Ok(None)
        } else if next < 2 || next > self.clusters + 1 {
            Err(Error::Corrupt)
        } else {
            Ok(Some(next))
        }
    }

    /// Runs `visit` over every sector of the root directory in order until it returns `Done`.
    pub(crate) fn root_sectors<T>(&mut self, mut visit: impl FnMut(u64, &mut [u8; SECTOR]) -> Visit<T>) -> Result<Option<T>, Error> {
        let mut cluster = self.dir;
        let mut guard = 0u32;
        let mut sector = [0u8; SECTOR];
        loop {
            for index in 0..self.spc {
                let lba = self.cluster_lba(cluster) + u64::from(index);
                self.device.read(lba, &mut sector)?;
                match visit(lba, &mut sector) {
                    Visit::Continue => {}
                    Visit::Done(value) => return Ok(Some(value)),
                    Visit::Rewrite => self.device.write(lba, &sector)?,
                }
            }
            match self.chain_next(cluster)? {
                Some(next) => cluster = next,
                None => return Ok(None),
            }
            guard += 1;
            if guard > self.clusters {
                return Err(Error::Corrupt);
            }
        }
    }

    /// Chains one more zeroed cluster (already claimed by the caller in whatever allocation record the format has)
    /// onto the root directory and returns its number.
    pub(crate) fn extend_root(&mut self, cluster: u32) -> Result<(), Error> {
        self.zero_cluster(cluster)?;
        let end = if self.kind == Kind::Fat32 { 0x0fff_ffff } else { 0xffff_ffff };
        self.fat_set_range(cluster, 1, |_| end)?;
        let mut last = self.dir;
        while let Some(next) = self.chain_next(last)? {
            last = next;
        }
        self.fat_set_range(last, 1, |_| cluster)
    }

    /// `count` consecutive free entries inside one sector of the root directory, as (sector, byte offset) -- or None
    /// when every sector is full. `free(first_byte)` says whether an entry is unused. A sector whose free tail is
    /// too short has its end-of-directory marker (0x00) replaced by `unused` so that the entries placed in a later
    /// sector are still reached.
    pub(crate) fn find_free_entries(&mut self, count: usize, free: impl Fn(u8) -> bool, unused: u8) -> Result<Option<(u64, usize)>, Error> {
        self.root_sectors(|lba, sector| {
            let mut run = 0;
            let mut first_marker = None;
            for slot in 0..SECTOR / 32 {
                let first = sector[slot * 32];
                if free(first) {
                    run += 1;
                    if first == 0 && first_marker.is_none() {
                        first_marker = Some(slot);
                    }
                    if run == count {
                        return Visit::Done((lba, (slot + 1 - count) * 32));
                    }
                } else {
                    run = 0;
                    first_marker = None;
                }
            }
            // The tail after the last used entry is free and the entries after an end marker with it; if they cannot
            // hold `count`, retire the marker.
            match first_marker {
                Some(marker) => {
                    for slot in marker..SECTOR / 32 {
                        sector[slot * 32] = unused;
                    }
                    Visit::Rewrite
                }
                None => Visit::Continue,
            }
        })
    }
}
