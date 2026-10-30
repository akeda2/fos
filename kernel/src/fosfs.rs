//! FOSFS versions 3 and 4: a deliberately small flat filesystem. Both have one superblock sector, then a
//! directory of fixed-size entries, then data; files are one contiguous run of sectors each, placed first-fit,
//! and the directory is the only allocation record. They differ in size limits (`docs/fosfs-v4.md`):
//!
//! * v3 (magic `FOSFS3`): eight directory sectors of 64 entries of 64 bytes; names up to 16 lowercase letters,
//!   digits, `.`, `_` or `-`. Byte-compatible with v1.
//! * v4 (magic `FOSFS4`): 128-byte entries and a directory sized from the volume (one entry per 4 MiB, 256 to
//!   4096 entries); names up to 48 letters of either case, digits, `.`, `_`, `-` or `+`, compared case-sensitively.
//!
//! The entry's first 28 bytes are the same in both: used flag, name length, start sector (4), length in bytes
//! (12), sector count (20); the name follows at 28. The rest of a v4 entry is zero and reserved (more extents).

use crate::block;

const MAGIC3: &[u8; 8] = b"FOSFS3\0\0";
const MAGIC4: &[u8; 8] = b"FOSFS4\0\0";
const DIRECTORY_SECTORS_V3: u64 = 8;
const ENTRY_SIZE_V3: usize = 64;
const ENTRY_SIZE_V4: usize = 128;
const MAX_NAME_V3: usize = 16;
/// The longest name or label any version holds; callers size their buffers with it.
pub const MAX_NAME: usize = 48;
/// The most entries a directory has (v4 at 16 GiB and up); sizes the scratch table.
const MAX_ENTRIES: usize = 4096;
const MIN_ENTRIES_V4: u64 = 256;
/// Sectors per entry of directory growth: one entry per 4 MiB of volume.
const SECTORS_PER_ENTRY_V4: u64 = 8192;
/// Where the volume label lives in the superblock: up to 16 bytes (v4: 48), zero padded, written by `label`
/// (v1's FOSFS leaves these bytes alone, and a v1 `format` clears them).
const LABEL_OFFSET: usize = 64;
/// Directory sectors read per disk request while scanning.
const CHUNK_SECTORS: usize = 16;

/// A volume label: the same characters as a file name, up to 16 of them (v4: 48). It names a volume
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
    version: u8,
    directory_sectors: u64,
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

/// A file name any FOSFS version could hold: up to 48 letters, digits, `.`, `_`, `-` or `+`. A v3 volume applies
/// the stricter `valid_name_v3` itself, so a caller may check with this and let the volume refuse the rest.
pub fn valid_name(name: &[u8]) -> bool {
    !name.is_empty() && name.len() <= MAX_NAME && name.iter().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'+'))
}

/// The v3 rule: up to 16 lowercase letters, digits, `.`, `_` or `-`.
pub fn valid_name_v3(name: &[u8]) -> bool {
    !name.is_empty() && name.len() <= MAX_NAME_V3 && name.iter().all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'))
}

/// A volume label: the v3 name rule, so it stays lowercase and fits every version.
pub fn valid_label(label: &[u8]) -> bool {
    valid_name_v3(label)
}

fn get_u64(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

/// Scratch for extent searches and `check`: (start, end, name hash) of every file. Static because a v4 directory
/// can hold 4096 entries (96 KiB), too much for the stack; the shell is single-threaded and no caller nests.
static mut SCRATCH: [(u64, u64, u64); MAX_ENTRIES] = [(0, 0, 0); MAX_ENTRIES];

fn hash(name: &[u8]) -> u64 {
    name.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3))
}

/// The directory size a new v4 volume of `sectors` sectors gets.
fn directory_sectors_v4(sectors: u64) -> u64 {
    let entries = (sectors / SECTORS_PER_ENTRY_V4).clamp(MIN_ENTRIES_V4, MAX_ENTRIES as u64);
    entries * ENTRY_SIZE_V4 as u64 / 512
}

impl Volume {
    fn read_sector(&self, lba: u64) -> Result<[u8; 512], Error> {
        let mut sector = [0u8; 512];
        block::read(self.disk, self.base + lba, &mut sector)?;
        Ok(sector)
    }

    fn entry_size(&self) -> usize {
        if self.version == 4 { ENTRY_SIZE_V4 } else { ENTRY_SIZE_V3 }
    }

    fn entries_per_sector(&self) -> usize {
        512 / self.entry_size()
    }

    /// The first data sector: the superblock and the directory come before it.
    fn data_start(&self) -> u64 {
        1 + self.directory_sectors
    }

    fn max_name(&self) -> usize {
        if self.version == 4 { MAX_NAME } else { MAX_NAME_V3 }
    }

    /// Whether this version can hold `name` (v3: lowercase only, 16 characters).
    pub fn accepts_name(&self, name: &[u8]) -> bool {
        if self.version == 4 { valid_name(name) } else { valid_name_v3(name) }
    }

    /// 3 or 4.
    pub fn version(&self) -> u8 {
        self.version
    }

    /// Calls `visit(lba, index, entry bytes)` for every directory slot, in order, until it returns false. The
    /// directory is read `CHUNK_SECTORS` sectors per request, which matters on USB with 4096 entries.
    fn scan(&self, mut visit: impl FnMut(u64, usize, &[u8]) -> bool) -> Result<(), Error> {
        let (size, per_sector) = (self.entry_size(), self.entries_per_sector());
        let mut chunk = [0u8; CHUNK_SECTORS * 512];
        let mut done = 0;
        while done < self.directory_sectors {
            let count = (self.directory_sectors - done).min(CHUNK_SECTORS as u64) as usize;
            block::read(self.disk, self.base + 1 + done, &mut chunk[..count * 512])?;
            for sector in 0..count {
                for index in 0..per_sector {
                    let at = sector * 512 + index * size;
                    if !visit(1 + done + sector as u64, index, &chunk[at..at + size]) {
                        return Ok(());
                    }
                }
            }
            done += count as u64;
        }
        Ok(())
    }

    /// Checks the superblock; `Err(NotFormatted)` if this is not a FOSFS v3 or v4 volume that fits the `sectors` of its partition.
    /// The volume may be smaller than its partition (the partition was grown first, `grow` is about to enlarge the volume,
    /// or it was interrupted between the two): the superblock's size is the volume's size.
    pub fn mount(disk: usize, base: u64, sectors: u64) -> Result<Volume, Error> {
        let mut volume = Volume { disk, base, sectors, version: 3, directory_sectors: DIRECTORY_SECTORS_V3 };
        let sector = volume.read_sector(0)?;
        let recorded = get_u64(&sector, 28);
        let version = match &sector[..8] {
            m if m == MAGIC3 => 3,
            m if m == MAGIC4 => 4,
            _ => return Err(Error::NotFormatted),
        };
        let (entry_size, directory_sectors) = (u16::from_le_bytes([sector[10], sector[11]]) as usize, u64::from(u32::from_le_bytes(sector[16..20].try_into().unwrap())));
        let expected_entry = if version == 4 { ENTRY_SIZE_V4 } else { ENTRY_SIZE_V3 };
        let geometry = if version == 4 { (1..=MAX_ENTRIES as u64 * 128 / 512).contains(&directory_sectors) } else { directory_sectors == DIRECTORY_SECTORS_V3 };
        let data_start = 1 + directory_sectors;
        let valid = u16::from_le_bytes([sector[8], sector[9]]) == u16::from(version)
            && entry_size == expected_entry
            && u32::from_le_bytes(sector[12..16].try_into().unwrap()) == 1
            && geometry
            && get_u64(&sector, 20) == data_start
            && recorded >= 8
            && recorded <= sectors.saturating_sub(data_start);
        if !valid {
            return Err(Error::NotFormatted);
        }
        volume.version = version;
        volume.directory_sectors = directory_sectors;
        volume.sectors = data_start + recorded;
        Ok(volume)
    }

    /// Enlarges the volume to `sectors` sectors of its (grown) partition: only the size in the superblock changes, since
    /// files are extents addressed from the start of the volume and the new space is simply free (a v4 directory keeps
    /// the size it was formatted with). Returns the volume as it is now. The partition must already be at least that big.
    pub fn grow(&self, sectors: u64) -> Result<Volume, Error> {
        if sectors <= self.sectors {
            return Ok(*self);
        }
        let mut sector = self.read_sector(0)?;
        sector[28..36].copy_from_slice(&(sectors - self.data_start()).to_le_bytes());
        block::write(self.disk, self.base, &sector)?;
        Volume::mount(self.disk, self.base, sectors)
    }

    /// Writes an empty FOSFS v3 volume; see `format_version`.
    pub fn format(disk: usize, base: u64, sectors: u64, label: &[u8]) -> Result<Volume, Error> {
        Volume::format_version(disk, base, sectors, label, 3)
    }

    /// Writes an empty FOSFS volume of `version` (3 or 4) over the `sectors` sectors at `base` of `disk`: the
    /// superblock (magic, version, geometry, optional label) and zeroed directory sectors. The data area is
    /// not touched (it is unreachable until a file is written). Destroys whatever volume was there; callers confirm first.
    pub fn format_version(disk: usize, base: u64, sectors: u64, label: &[u8], version: u8) -> Result<Volume, Error> {
        let (magic, entry_size, directory_sectors) = match version {
            3 => (MAGIC3, ENTRY_SIZE_V3, DIRECTORY_SECTORS_V3),
            4 => (MAGIC4, ENTRY_SIZE_V4, directory_sectors_v4(sectors)),
            _ => return Err(Error::NotFormatted),
        };
        let data_start = 1 + directory_sectors;
        if sectors < data_start + 8 {
            return Err(Error::TooSmall);
        }
        if !label.is_empty() && !valid_label(label) {
            return Err(Error::InvalidName);
        }
        let mut sector = [0u8; 512];
        sector[..8].copy_from_slice(magic);
        sector[8..10].copy_from_slice(&u16::from(version).to_le_bytes());
        sector[10..12].copy_from_slice(&(entry_size as u16).to_le_bytes());
        sector[12..16].copy_from_slice(&1u32.to_le_bytes());
        sector[16..20].copy_from_slice(&(directory_sectors as u32).to_le_bytes());
        sector[20..28].copy_from_slice(&data_start.to_le_bytes());
        sector[28..36].copy_from_slice(&(sectors - data_start).to_le_bytes());
        sector[LABEL_OFFSET..LABEL_OFFSET + label.len()].copy_from_slice(label);
        // Directory first, superblock last: a half-finished format is not a valid volume.
        let zeros = [0u8; CHUNK_SECTORS * 512];
        let mut done = 0;
        while done < directory_sectors {
            let count = (directory_sectors - done).min(CHUNK_SECTORS as u64) as usize;
            block::write(disk, base + 1 + done, &zeros[..count * 512])?;
            done += count as u64;
        }
        block::write(disk, base, &sector)?;
        Volume::mount(disk, base, sectors)
    }

    /// The volume's label (empty if it has none).
    pub fn label(&self) -> Result<Label, Error> {
        let sector = self.read_sector(0)?;
        let field = &sector[LABEL_OFFSET..LABEL_OFFSET + self.max_name()];
        let length = field.iter().position(|&byte| byte == 0).unwrap_or(field.len());
        let mut label = Label { bytes: [0; MAX_NAME], length };
        label.bytes[..length].copy_from_slice(&field[..length]);
        // A field holding anything but a valid name (junk in a volume some other tool made) counts as no label.
        if length != 0 && !valid_label(&label.bytes[..length]) {
            return Ok(Label { bytes: [0; MAX_NAME], length: 0 });
        }
        Ok(label)
    }

    /// The length in bytes of file `name`, or `None` if there is no such file.
    pub fn file_length(&self, name: &[u8]) -> Result<Option<u64>, Error> {
        if !self.accepts_name(name) {
            return Err(Error::InvalidName);
        }
        Ok(self.find(name)?.map(|entry| entry.length))
    }

    /// Sets the label; an empty slice clears it.
    pub fn set_label(&self, label: &[u8]) -> Result<(), Error> {
        if !label.is_empty() && !(valid_label(label) && label.len() <= self.max_name()) {
            return Err(Error::InvalidName);
        }
        let mut sector = self.read_sector(0)?;
        sector[LABEL_OFFSET..LABEL_OFFSET + self.max_name()].fill(0);
        sector[LABEL_OFFSET..LABEL_OFFSET + label.len()].copy_from_slice(label);
        block::write(self.disk, self.base, &sector)?;
        Ok(())
    }

    /// Whether the slot bytes hold a plausible file: (start, length, sector count, name length), or `None` for an
    /// empty slot or a damaged entry.
    fn parse(&self, slot: &[u8]) -> Option<(u64, u64, u64, usize)> {
        if slot[0] != 1 {
            return None;
        }
        let name_length = usize::from(slot[1]);
        let (start, length, sector_count) = (get_u64(slot, 4), get_u64(slot, 12), get_u64(slot, 20));
        let good = name_length != 0
            && name_length <= self.max_name()
            && start >= self.data_start()
            && start.saturating_add(sector_count) <= self.sectors
            && length <= sector_count.saturating_mul(512);
        good.then_some((start, length, sector_count, name_length))
    }

    /// Calls `visit` for every valid directory entry.
    pub fn for_each(&self, mut visit: impl FnMut(Entry)) -> Result<(), Error> {
        self.scan(|lba, index, slot| {
            // A damaged entry is skipped, not trusted.
            if let Some((start, length, sector_count, name_length)) = self.parse(slot) {
                let mut name = [0u8; MAX_NAME];
                name[..name_length].copy_from_slice(&slot[28..28 + name_length]);
                visit(Entry { directory_lba: lba, index, start, length, sector_count, name, name_length });
            }
            true
        })
    }

    /// Reads the directory the way a repair tool would: reports every file's extent through `line`, and every problem
    /// (a damaged entry, extents that overlap, a name used twice). Returns (files, problems); nothing is changed. There is
    /// nothing to replay or roll back after a power cut, so this only makes visible what mount and `for_each` already
    /// skip over.
    pub fn check(&self, mut line: impl FnMut(core::fmt::Arguments)) -> Result<(usize, usize), Error> {
        let table = unsafe { &mut *core::ptr::addr_of_mut!(SCRATCH) };
        let (mut count, mut problems, mut slots) = (0, 0, 0);
        let mut failed = false;
        self.scan(|_, _, slot| {
            if slot[0] == 0 {
                return true;
            }
            slots += 1;
            let Some((start, length, sectors, name_length)) = self.parse(slot) else {
                problems += 1;
                line(format_args!("directory entry {} is damaged (ignored)", slots - 1));
                return true;
            };
            if count == table.len() {
                failed = true;
                return false;
            }
            let name = &slot[28..28 + name_length];
            table[count] = (start, start + sectors, hash(name));
            count += 1;
            line(format_args!("{:<16} {:>9} bytes at sector {}, {} sectors", core::str::from_utf8(name).unwrap_or("?"), length, start, sectors));
            true
        })?;
        if failed {
            return Err(Error::DirectoryFull);
        }
        let files = &mut table[..count];
        files.sort_unstable();
        for pair in files.windows(2) {
            // Zero-length reservations (empty files) occupy nothing, so they cannot overlap.
            if pair[0].1 > pair[0].0 && pair[1].1 > pair[1].0 && pair[0].1 > pair[1].0 {
                problems += 1;
                line(format_args!("the files at sectors {} and {} share sectors", pair[0].0, pair[1].0));
            }
        }
        files.sort_unstable_by_key(|file| file.2);
        for pair in files.windows(2) {
            if pair[0].2 == pair[1].2 {
                problems += 1;
                line(format_args!("a name appears twice (the files at sectors {} and {})", pair[0].0, pair[1].0));
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

    /// Writes directory slot (`lba`, `index`) after `fill` has set its bytes (zeroed first).
    fn put_slot(&self, lba: u64, index: usize, fill: impl FnOnce(&mut [u8])) -> Result<(), Error> {
        let mut directory = self.read_sector(lba)?;
        let at = index * self.entry_size();
        let slot = &mut directory[at..at + self.entry_size()];
        slot.fill(0);
        fill(slot);
        block::write(self.disk, self.base + lba, &directory)?;
        Ok(())
    }

    /// Creates or replaces a file. The new contents go to a free extent of their own and the single directory-sector
    /// write that points the entry at it is the commit, so a power cut leaves the old file or the new one, never a mix
    /// (there is no allocation map to repair: the directory is the only record of what is used). Only when the disk has
    /// no free run for the new copy is an existing file rewritten in place (or moved over its own old extent), and that
    /// case is not crash-safe.
    pub fn write(&self, name: &[u8], data: &[u8]) -> Result<(), Error> {
        if !self.accepts_name(name) {
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
        self.put_slot(slot.0, slot.1, |entry| {
            entry[0] = 1;
            entry[1] = name.len() as u8;
            entry[4..12].copy_from_slice(&start.to_le_bytes());
            entry[12..20].copy_from_slice(&(data.len() as u64).to_le_bytes());
            entry[20..28].copy_from_slice(&needed.to_le_bytes());
            entry[28..28 + name.len()].copy_from_slice(name);
        })
    }

    /// Reserves a file for streaming: one contiguous extent of `max_bytes` (the largest free extent when 0), with
    /// a directory entry of length 0 that `write_extent` grows. Replaces an existing file of that name. Returns the
    /// entry and the reserved capacity in bytes. Nothing is written to the extent, so the old contents show through
    /// until they are overwritten; that is why the length stays 0 until data has really been written.
    pub fn reserve(&self, name: &[u8], max_bytes: u64) -> Result<(Entry, u64), Error> {
        if !self.accepts_name(name) {
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
        self.put_slot(slot.0, slot.1, |entry| {
            entry[0] = 1;
            entry[1] = name.len() as u8;
            entry[4..12].copy_from_slice(&start.to_le_bytes());
            entry[20..28].copy_from_slice(&count.to_le_bytes());
            entry[28..28 + name.len()].copy_from_slice(name);
        })?;
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
            self.patch_slot(entry, |slot| slot[12..20].copy_from_slice(&file_length.to_le_bytes()))?;
        }
        Ok(())
    }

    /// Changes bytes of an existing entry's slot in place.
    fn patch_slot(&self, entry: &Entry, patch: impl FnOnce(&mut [u8])) -> Result<(), Error> {
        let mut directory = self.read_sector(entry.directory_lba)?;
        let at = entry.index * self.entry_size();
        patch(&mut directory[at..at + self.entry_size()]);
        block::write(self.disk, self.base + entry.directory_lba, &directory)?;
        Ok(())
    }

    /// Ends a streamed file: records its final length and gives the unused part of the reservation back, so the
    /// space a take reserved but did not fill is free again.
    pub fn trim_extent(&self, entry: &Entry, file_length: u64) -> Result<(), Error> {
        let sectors = file_length.div_ceil(512).min(entry.sector_count);
        self.patch_slot(entry, |slot| {
            slot[12..20].copy_from_slice(&file_length.to_le_bytes());
            slot[20..28].copy_from_slice(&sectors.to_le_bytes());
        })
    }

    pub fn remove(&self, name: &[u8]) -> Result<bool, Error> {
        let Some(entry) = self.find(name)? else { return Ok(false) };
        self.patch_slot(&entry, |slot| slot.fill(0))?;
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
        let mut found = None;
        self.scan(|lba, index, slot| {
            if slot[0] == 0 {
                found = Some((lba, index));
                return false;
            }
            true
        })?;
        Ok(found)
    }

    /// (free bytes in all, bytes of the largest free run): a recording needs one contiguous run, so the second figure
    /// is what a take can use.
    pub fn free_space(&self) -> Result<(u64, u64), Error> {
        let mut used = 0u64;
        self.for_each(|entry| used += entry.sector_count)?;
        let total = self.sectors.saturating_sub(self.data_start()).saturating_sub(used);
        let largest = self.largest_free_extent(None)?.map_or(0, |(_, count)| count);
        Ok((total * 512, largest * 512))
    }

    /// The (start, end) extents of every file except `replacing`, sorted by start, in the static scratch table.
    fn sorted_extents(&self, replacing: Option<&Entry>) -> Result<&'static mut [(u64, u64, u64)], Error> {
        let table = unsafe { &mut *core::ptr::addr_of_mut!(SCRATCH) };
        let mut used = 0;
        self.for_each(|entry| {
            if replacing.is_some_and(|r| r.directory_lba == entry.directory_lba && r.index == entry.index) {
                return;
            }
            if used < table.len() {
                table[used] = (entry.start, entry.start + entry.sector_count, 0);
                used += 1;
            }
        })?;
        let extents = &mut table[..used];
        extents.sort_unstable();
        Ok(extents)
    }

    /// The largest run of free sectors (start, count), ignoring the extent of `replacing`.
    fn largest_free_extent(&self, replacing: Option<&Entry>) -> Result<Option<(u64, u64)>, Error> {
        let extents = self.sorted_extents(replacing)?;
        let (mut best, mut candidate) = ((0u64, 0u64), self.data_start());
        for &(start, end, _) in extents.iter() {
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
            return Ok(Some(self.data_start()));
        }
        let extents = self.sorted_extents(replacing)?;
        let mut candidate = self.data_start();
        for &(start, end, _) in extents.iter() {
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
        Ok((files, 0)) => crate::println!("fsck: FOSFS v{}, {files} file(s), directory consistent.", volume.version()),
        Ok((files, problems)) => crate::println!("fsck: FOSFS v{}, {files} file(s), {problems} problem(s) (damaged entries are ignored, nothing is changed).", volume.version()),
        Err(error) => crate::println!("fsck: {error:?}"),
    }
}
