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

    /// Checks the superblock; `Err(NotFormatted)` if this is not a FOSFS v3 volume of this size.
    pub fn mount(disk: usize, base: u64, sectors: u64) -> Result<Volume, Error> {
        let volume = Volume { disk, base, sectors };
        let sector = volume.read_sector(0)?;
        let valid = &sector[..8] == MAGIC
            && u16::from_le_bytes([sector[8], sector[9]]) == VERSION
            && u16::from_le_bytes([sector[10], sector[11]]) as usize == ENTRY_SIZE
            && u32::from_le_bytes(sector[12..16].try_into().unwrap()) == 1
            && u32::from_le_bytes(sector[16..20].try_into().unwrap()) == DIRECTORY_SECTORS as u32
            && get_u64(&sector, 20) == DATA_START
            && get_u64(&sector, 28) == sectors.saturating_sub(DATA_START);
        if valid { Ok(volume) } else { Err(Error::NotFormatted) }
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

    /// Creates or replaces a file. An existing file is rewritten in place when it fits
    /// its old extent, otherwise moved to a free extent.
    pub fn write(&self, name: &[u8], data: &[u8]) -> Result<(), Error> {
        if !valid_name(name) {
            return Err(Error::InvalidName);
        }
        let existing = self.find(name)?;
        let needed = data.len().div_ceil(512) as u64;
        let (slot, start) = match existing {
            Some(entry) if needed <= entry.sector_count => ((entry.directory_lba, entry.index), entry.start),
            Some(entry) => ((entry.directory_lba, entry.index), self.free_extent(needed, Some(&entry))?.ok_or(Error::DataFull)?),
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
