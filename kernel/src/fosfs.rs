//! FOSFS version 3: a deliberately small flat filesystem, byte-compatible with v1's
//! (so the same host tools and the same volumes work): one superblock sector, eight
//! directory sectors of 64 fixed 64-byte entries, and files stored as one contiguous
//! run of sectors each, placed first-fit. Names are up to 16 lowercase letters,
//! digits, `.`, `_` or `-`.

use crate::block;

const MAGIC: &[u8; 8] = b"FOSFS3\0\0";
const VERSION: u16 = 3;
const DIRECTORY_SECTORS: u64 = 8;
const DATA_START: u64 = 9;
const ENTRY_SIZE: usize = 64;
const ENTRIES_PER_SECTOR: usize = 512 / ENTRY_SIZE;
const MAX_ENTRIES: usize = DIRECTORY_SECTORS as usize * ENTRIES_PER_SECTOR;
pub const MAX_NAME: usize = 16;
/// Where the volume label lives in the superblock: 16 bytes, zero padded, written by `label`
/// (v1's FOSFS leaves these bytes alone, and a v1 `format` clears them).
const LABEL_OFFSET: usize = 64;

/// A volume label: the same characters as a file name, up to 16 of them. It names a volume
/// in a way that survives disks being numbered differently from one boot to the next.
#[derive(Clone, Copy)]
pub struct Label {
    bytes: [u8; MAX_NAME],
    length: usize,
}

impl Label {
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.length]
    }
    pub fn is_empty(&self) -> bool {
        self.length == 0
    }
}

/// A mounted volume: a disk and the partition on it.
#[derive(Clone, Copy)]
pub struct Volume {
    pub disk: usize,
    pub base: u64,
    pub sectors: u64,
}

#[derive(Clone, Copy)]
pub struct Entry {
    directory_lba: u64,
    index: usize,
    start: u64,
    pub length: u64,
    sector_count: u64,
    name: [u8; MAX_NAME],
    name_length: usize,
}

impl Entry {
    pub fn name(&self) -> &[u8] {
        &self.name[..self.name_length]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Disk,
    NotFormatted,
    InvalidName,
    DirectoryFull,
    DataFull,
    TooBig,
    TooSmall,
}

impl From<block::Error> for Error {
    fn from(_: block::Error) -> Self {
        Error::Disk
    }
}

pub fn valid_name(name: &[u8]) -> bool {
    !name.is_empty() && name.len() <= MAX_NAME && name.iter().all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'))
}

fn get_u64(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

impl Volume {
    fn read_sector(&self, lba: u64) -> Result<[u8; 512], Error> {
        let mut sector = [0u8; 512];
        block::read(self.disk, self.base + lba, &mut sector)?;
        Ok(sector)
    }

    /// Checks the superblock; `Err(NotFormatted)` if this is not a FOSFS v3 volume that fits the `sectors` of its partition.
    /// The volume may be smaller than its partition (the partition was grown first, `grow` is about to enlarge the volume,
    /// or it was interrupted between the two): the superblock's size is the volume's size.
    pub fn mount(disk: usize, base: u64, sectors: u64) -> Result<Volume, Error> {
        let mut volume = Volume { disk, base, sectors };
        let sector = volume.read_sector(0)?;
        let recorded = get_u64(&sector, 28);
        let valid = &sector[..8] == MAGIC
            && u16::from_le_bytes([sector[8], sector[9]]) == VERSION
            && u16::from_le_bytes([sector[10], sector[11]]) as usize == ENTRY_SIZE
            && u32::from_le_bytes(sector[12..16].try_into().unwrap()) == 1
            && u32::from_le_bytes(sector[16..20].try_into().unwrap()) == DIRECTORY_SECTORS as u32
            && get_u64(&sector, 20) == DATA_START
            && recorded >= 8
            && recorded <= sectors.saturating_sub(DATA_START);
        if !valid {
            return Err(Error::NotFormatted);
        }
        volume.sectors = DATA_START + recorded;
        Ok(volume)
    }

    /// Enlarges the volume to `sectors` sectors of its (grown) partition: only the size in the superblock changes, since
    /// files are extents addressed from the start of the volume and the new space is simply free. Returns the volume as it is
    /// now. The partition must already be at least that big.
    pub fn grow(&self, sectors: u64) -> Result<Volume, Error> {
        if sectors <= self.sectors {
            return Ok(*self);
        }
        let mut sector = self.read_sector(0)?;
        sector[28..36].copy_from_slice(&(sectors - DATA_START).to_le_bytes());
        block::write(self.disk, self.base, &sector)?;
        Volume::mount(self.disk, self.base, sectors)
    }

    /// Writes an empty FOSFS v3 volume over the `sectors` sectors at `base` of `disk`: the
    /// superblock (magic, version, geometry, optional label) and eight zeroed directory
    /// sectors. The data area is not touched (it is unreachable until a file is written).
    /// Destroys whatever volume was there; callers confirm first.
    pub fn format(disk: usize, base: u64, sectors: u64, label: &[u8]) -> Result<Volume, Error> {
        if sectors < DATA_START + 8 {
            return Err(Error::TooSmall);
        }
        if !label.is_empty() && !valid_name(label) {
            return Err(Error::InvalidName);
        }
        let mut sector = [0u8; 512];
        sector[..8].copy_from_slice(MAGIC);
        sector[8..10].copy_from_slice(&VERSION.to_le_bytes());
        sector[10..12].copy_from_slice(&(ENTRY_SIZE as u16).to_le_bytes());
        sector[12..16].copy_from_slice(&1u32.to_le_bytes());
        sector[16..20].copy_from_slice(&(DIRECTORY_SECTORS as u32).to_le_bytes());
        sector[20..28].copy_from_slice(&DATA_START.to_le_bytes());
        sector[28..36].copy_from_slice(&(sectors - DATA_START).to_le_bytes());
        sector[LABEL_OFFSET..LABEL_OFFSET + label.len()].copy_from_slice(label);
        // Directory first, superblock last: a half-finished format is not a valid volume.
        block::write(disk, base + 1, &[0u8; DIRECTORY_SECTORS as usize * 512])?;
        block::write(disk, base, &sector)?;
        Volume::mount(disk, base, sectors)
    }

    /// The volume's label (empty if it has none).
    pub fn label(&self) -> Result<Label, Error> {
        let sector = self.read_sector(0)?;
        let field = &sector[LABEL_OFFSET..LABEL_OFFSET + MAX_NAME];
        let length = field.iter().position(|&byte| byte == 0).unwrap_or(MAX_NAME);
        let mut label = Label { bytes: [0; MAX_NAME], length };
        label.bytes[..length].copy_from_slice(&field[..length]);
        // A field holding anything but a valid name (junk in a volume some other tool made) counts as no label.
        if length != 0 && !valid_name(&label.bytes[..length]) {
            return Ok(Label { bytes: [0; MAX_NAME], length: 0 });
        }
        Ok(label)
    }

    /// The length in bytes of file `name`, or `None` if there is no such file.
    pub fn file_length(&self, name: &[u8]) -> Result<Option<u64>, Error> {
        if !valid_name(name) {
            return Err(Error::InvalidName);
        }
        Ok(self.find(name)?.map(|entry| entry.length))
    }

    /// Sets the label; an empty slice clears it.
    pub fn set_label(&self, label: &[u8]) -> Result<(), Error> {
        if !label.is_empty() && !valid_name(label) {
            return Err(Error::InvalidName);
        }
        let mut sector = self.read_sector(0)?;
        sector[LABEL_OFFSET..LABEL_OFFSET + MAX_NAME].fill(0);
        sector[LABEL_OFFSET..LABEL_OFFSET + label.len()].copy_from_slice(label);
        block::write(self.disk, self.base, &sector)?;
        Ok(())
    }

    /// Calls `visit` for every valid directory entry.
    pub fn for_each(&self, mut visit: impl FnMut(Entry)) -> Result<(), Error> {
        for sector_offset in 0..DIRECTORY_SECTORS {
            let lba = 1 + sector_offset;
            let contents = self.read_sector(lba)?;
            for index in 0..ENTRIES_PER_SECTOR {
                let at = index * ENTRY_SIZE;
                if contents[at] != 1 {
                    continue;
                }
                let name_length = usize::from(contents[at + 1]);
                let (start, length, sector_count) = (get_u64(&contents, at + 4), get_u64(&contents, at + 12), get_u64(&contents, at + 20));
                if name_length == 0
                    || name_length > MAX_NAME
                    || start < DATA_START
                    || start.saturating_add(sector_count) > self.sectors
                    || length > sector_count.saturating_mul(512)
                {
                    continue; // a damaged entry is skipped, not trusted
                }
                let mut name = [0u8; MAX_NAME];
                name.copy_from_slice(&contents[at + 28..at + 44]);
                visit(Entry { directory_lba: lba, index, start, length, sector_count, name, name_length });
            }
        }
        Ok(())
    }

    /// Reads the directory the way a repair tool would: reports every file's extent through `line`, and every problem
    /// (a damaged entry, extents that overlap, a name used twice). Returns (files, problems); nothing is changed. There is
    /// nothing to replay or roll back after a power cut, so this only makes visible what mount and `for_each` already
    /// skip over.
    pub fn check(&self, mut line: impl FnMut(core::fmt::Arguments)) -> Result<(usize, usize), Error> {
        let mut files = [(0u64, 0u64, [0u8; MAX_NAME], 0usize); MAX_ENTRIES];
        let (mut count, mut problems, mut slots) = (0, 0, 0);
        for sector_offset in 0..DIRECTORY_SECTORS {
            let contents = self.read_sector(1 + sector_offset)?;
            for index in 0..ENTRIES_PER_SECTOR {
                let at = index * ENTRY_SIZE;
                if contents[at] == 0 {
                    continue;
                }
                slots += 1;
                let name_length = usize::from(contents[at + 1]);
                let (start, length, sectors) = (get_u64(&contents, at + 4), get_u64(&contents, at + 12), get_u64(&contents, at + 20));
                let good = contents[at] == 1
                    && (1..=MAX_NAME).contains(&name_length)
                    && start >= DATA_START
                    && start.saturating_add(sectors) <= self.sectors
                    && length <= sectors.saturating_mul(512);
                if !good {
                    problems += 1;
                    line(format_args!("directory entry {} is damaged (ignored)", slots - 1));
                    continue;
                }
                let mut name = [0u8; MAX_NAME];
                name.copy_from_slice(&contents[at + 28..at + 44]);
                files[count] = (start, sectors, name, name_length);
                count += 1;
                line(format_args!(
                    "{:<16} {:>9} bytes at sector {}, {} sectors",
                    core::str::from_utf8(&name[..name_length]).unwrap_or("?"),
                    length,
                    start,
                    sectors
                ));
            }
        }
        for a in 0..count {
            for b in a + 1..count {
                let (x, y) = (&files[a], &files[b]);
                if x.3 == y.3 && x.2[..x.3] == y.2[..y.3] {
                    problems += 1;
                    line(format_args!("{} appears twice", core::str::from_utf8(&x.2[..x.3]).unwrap_or("?")));
                }
                if x.1 != 0 && y.1 != 0 && x.0 < y.0 + y.1 && y.0 < x.0 + x.1 {
                    problems += 1;
                    line(format_args!(
                        "{} and {} share sectors",
                        core::str::from_utf8(&x.2[..x.3]).unwrap_or("?"),
                        core::str::from_utf8(&y.2[..y.3]).unwrap_or("?")
                    ));
                }
            }
        }
        Ok((count, problems))
    }

    pub fn find(&self, name: &[u8]) -> Result<Option<Entry>, Error> {
        let mut result = None;
        self.for_each(|entry| {
            if result.is_none() && entry.name() == name {
                result = Some(entry);
            }
        })?;
        Ok(result)
    }

    /// Reads a whole file into `buffer`; returns its length, or None if absent. A
    /// buffer that is too small is an error rather than a silent truncation.
    pub fn read(&self, name: &[u8], buffer: &mut [u8]) -> Result<Option<usize>, Error> {
        let Some(entry) = self.find(name)? else { return Ok(None) };
        let length = entry.length as usize;
        if length > buffer.len() {
            return Err(Error::TooBig);
        }
        // Whole sectors go straight into the buffer; the last partial sector via a copy.
        let whole = length / 512 * 512;
        if whole > 0 {
            block::read(self.disk, self.base + entry.start, &mut buffer[..whole])?;
        }
        if length > whole {
            let tail = self.read_sector(entry.start + (whole / 512) as u64)?;
            buffer[whole..length].copy_from_slice(&tail[..length - whole]);
        }
        Ok(Some(length))
    }

    /// Creates or replaces a file. The new contents go to a free extent of their own and the single directory-sector
    /// write that points the entry at it is the commit, so a power cut leaves the old file or the new one, never a mix
    /// (there is no allocation map to repair: the directory is the only record of what is used). Only when the disk has
    /// no free run for the new copy is an existing file rewritten in place (or moved over its own old extent), and that
    /// case is not crash-safe.
    pub fn write(&self, name: &[u8], data: &[u8]) -> Result<(), Error> {
        if !valid_name(name) {
            return Err(Error::InvalidName);
        }
        let existing = self.find(name)?;
        let needed = data.len().div_ceil(512) as u64;
        let (slot, start) = match existing {
            Some(entry) => {
                let start = match self.free_extent(needed, None)? {
                    Some(fresh) => fresh,
                    None if needed <= entry.sector_count => entry.start,
                    None => self.free_extent(needed, Some(&entry))?.ok_or(Error::DataFull)?,
                };
                ((entry.directory_lba, entry.index), start)
            }
            None => (self.free_slot()?.ok_or(Error::DirectoryFull)?, self.free_extent(needed, None)?.ok_or(Error::DataFull)?),
        };
        // Data first, directory entry last: a failure in between leaves the old file intact.
        self.write_data(start, data)?;
        let mut directory = self.read_sector(slot.0)?;
        let at = slot.1 * ENTRY_SIZE;
        directory[at..at + ENTRY_SIZE].fill(0);
        directory[at] = 1;
        directory[at + 1] = name.len() as u8;
        directory[at + 4..at + 12].copy_from_slice(&start.to_le_bytes());
        directory[at + 12..at + 20].copy_from_slice(&(data.len() as u64).to_le_bytes());
        directory[at + 20..at + 28].copy_from_slice(&needed.to_le_bytes());
        directory[at + 28..at + 28 + name.len()].copy_from_slice(name);
        block::write(self.disk, self.base + slot.0, &directory)?;
        Ok(())
    }

    /// Reserves a file for streaming: one contiguous extent of `max_bytes` (the largest free extent when 0), with
    /// a directory entry of length 0 that `write_extent` grows. Replaces an existing file of that name. Returns the
    /// entry and the reserved capacity in bytes. Nothing is written to the extent, so the old contents show through
    /// until they are overwritten; that is why the length stays 0 until data has really been written.
    pub fn reserve(&self, name: &[u8], max_bytes: u64) -> Result<(Entry, u64), Error> {
        if !valid_name(name) {
            return Err(Error::InvalidName);
        }
        let existing = self.find(name)?;
        let slot = match &existing {
            Some(entry) => (entry.directory_lba, entry.index),
            None => self.free_slot()?.ok_or(Error::DirectoryFull)?,
        };
        let (start, count) = if max_bytes == 0 {
            self.largest_free_extent(existing.as_ref())?.ok_or(Error::DataFull)?
        } else {
            let count = max_bytes.div_ceil(512);
            (self.free_extent(count, existing.as_ref())?.ok_or(Error::DataFull)?, count)
        };
        let mut directory = self.read_sector(slot.0)?;
        let at = slot.1 * ENTRY_SIZE;
        directory[at..at + ENTRY_SIZE].fill(0);
        directory[at] = 1;
        directory[at + 1] = name.len() as u8;
        directory[at + 4..at + 12].copy_from_slice(&start.to_le_bytes());
        directory[at + 20..at + 28].copy_from_slice(&count.to_le_bytes());
        directory[at + 28..at + 28 + name.len()].copy_from_slice(name);
        block::write(self.disk, self.base + slot.0, &directory)?;
        let mut entry = Entry { directory_lba: slot.0, index: slot.1, start, length: 0, sector_count: count, name: [0; MAX_NAME], name_length: name.len() };
        entry.name[..name.len()].copy_from_slice(name);
        Ok((entry, count * 512))
    }

    /// Writes `data` into a reserved file at byte `offset` (a multiple of 512; a final partial sector is padded) and,
    /// when `file_length` is non-zero, records it as the file's length. The data goes first and the length last, so a
    /// reset keeps everything up to the previous length.
    pub fn write_extent(&self, entry: &Entry, offset: u64, data: &[u8], file_length: u64) -> Result<(), Error> {
        if offset % 512 != 0 || offset + data.len() as u64 > entry.sector_count * 512 {
            return Err(Error::TooBig);
        }
        self.write_data(entry.start + offset / 512, data)?;
        if file_length != 0 {
            let mut directory = self.read_sector(entry.directory_lba)?;
            let at = entry.index * ENTRY_SIZE;
            directory[at + 12..at + 20].copy_from_slice(&file_length.to_le_bytes());
            block::write(self.disk, self.base + entry.directory_lba, &directory)?;
        }
        Ok(())
    }

    /// Ends a streamed file: records its final length and gives the unused part of the reservation back, so the
    /// space a take reserved but did not fill is free again.
    pub fn trim_extent(&self, entry: &Entry, file_length: u64) -> Result<(), Error> {
        let sectors = file_length.div_ceil(512).min(entry.sector_count);
        let mut directory = self.read_sector(entry.directory_lba)?;
        let at = entry.index * ENTRY_SIZE;
        directory[at + 12..at + 20].copy_from_slice(&file_length.to_le_bytes());
        directory[at + 20..at + 28].copy_from_slice(&sectors.to_le_bytes());
        block::write(self.disk, self.base + entry.directory_lba, &directory)?;
        Ok(())
    }

    pub fn remove(&self, name: &[u8]) -> Result<bool, Error> {
        let Some(entry) = self.find(name)? else { return Ok(false) };
        let mut directory = self.read_sector(entry.directory_lba)?;
        directory[entry.index * ENTRY_SIZE..(entry.index + 1) * ENTRY_SIZE].fill(0);
        block::write(self.disk, self.base + entry.directory_lba, &directory)?;
        Ok(true)
    }

    fn write_data(&self, start: u64, data: &[u8]) -> Result<(), Error> {
        let whole = data.len() / 512 * 512;
        if whole > 0 {
            block::write(self.disk, self.base + start, &data[..whole])?;
        }
        if data.len() > whole {
            let mut tail = [0u8; 512];
            tail[..data.len() - whole].copy_from_slice(&data[whole..]);
            block::write(self.disk, self.base + start + (whole / 512) as u64, &tail)?;
        }
        Ok(())
    }

    fn free_slot(&self) -> Result<Option<(u64, usize)>, Error> {
        for sector_offset in 0..DIRECTORY_SECTORS {
            let contents = self.read_sector(1 + sector_offset)?;
            if let Some(index) = (0..ENTRIES_PER_SECTOR).find(|&index| contents[index * ENTRY_SIZE] == 0) {
                return Ok(Some((1 + sector_offset, index)));
            }
        }
        Ok(None)
    }

    /// (free bytes in all, bytes of the largest free run): a recording needs one contiguous run, so the second figure
    /// is what a take can use.
    pub fn free_space(&self) -> Result<(u64, u64), Error> {
        let mut used = 0u64;
        self.for_each(|entry| used += entry.sector_count)?;
        let total = self.sectors.saturating_sub(DATA_START).saturating_sub(used);
        let largest = self.largest_free_extent(None)?.map_or(0, |(_, count)| count);
        Ok((total * 512, largest * 512))
    }

    /// The largest run of free sectors (start, count), ignoring the extent of `replacing`.
    fn largest_free_extent(&self, replacing: Option<&Entry>) -> Result<Option<(u64, u64)>, Error> {
        let mut extents = [(0u64, 0u64); MAX_ENTRIES];
        let mut used = 0;
        self.for_each(|entry| {
            if replacing.is_some_and(|r| r.directory_lba == entry.directory_lba && r.index == entry.index) {
                return;
            }
            if used < extents.len() {
                extents[used] = (entry.start, entry.start + entry.sector_count);
                used += 1;
            }
        })?;
        let extents = &mut extents[..used];
        extents.sort_unstable();
        let (mut best, mut candidate) = ((0u64, 0u64), DATA_START);
        for &(start, end) in extents.iter() {
            if start > candidate && start - candidate > best.1 {
                best = (candidate, start - candidate);
            }
            candidate = candidate.max(end);
        }
        if self.sectors > candidate && self.sectors - candidate > best.1 {
            best = (candidate, self.sectors - candidate);
        }
        Ok((best.1 > 0).then_some(best))
    }

    /// First-fit search for `count` free sectors, ignoring the extent of `replacing`.
    fn free_extent(&self, count: u64, replacing: Option<&Entry>) -> Result<Option<u64>, Error> {
        if count == 0 {
            return Ok(Some(DATA_START));
        }
        let mut extents = [(0u64, 0u64); MAX_ENTRIES];
        let mut used = 0;
        self.for_each(|entry| {
            if replacing.is_some_and(|r| r.directory_lba == entry.directory_lba && r.index == entry.index) {
                return;
            }
            if used < extents.len() {
                extents[used] = (entry.start, entry.start + entry.sector_count);
                used += 1;
            }
        })?;
        let extents = &mut extents[..used];
        extents.sort_unstable();
        let mut candidate = DATA_START;
        for &(start, end) in extents.iter() {
            if start >= candidate + count {
                break;
            }
            candidate = candidate.max(end);
        }
        Ok((candidate + count <= self.sectors).then_some(candidate))
    }
}

/// `fsck`: prints where every file lives and what, if anything, is wrong with the directory.
pub fn report(volume: &Volume) {
    match volume.check(|text| crate::println!("{text}")) {
        Ok((files, 0)) => crate::println!("fsck: {files} file(s), directory consistent."),
        Ok((files, problems)) => crate::println!("fsck: {files} file(s), {problems} problem(s) (damaged entries are ignored, nothing is changed)."),
        Err(error) => crate::println!("fsck: {error:?}"),
    }
}
