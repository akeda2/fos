//! FOSFS versions 3 and 4: a deliberately small flat filesystem. Both have one superblock sector, then a
//! directory of fixed-size entries, then data; files are one contiguous run of sectors each, placed first-fit,
//! and the directory is the only allocation record. They differ in size limits (`docs/fosfs-v4.md`):
//!
//! * v3 (magic `FOSFS3`): eight directory sectors of 64 entries of 64 bytes; names up to 16 lowercase letters,
//!   digits, `.`, `_` or `-`. Byte-compatible with v1.
//! * v4 (magic `FOSFS4`): 128-byte entries and a directory sized from the volume (one entry per 4 MiB, 256 to
//!   4096 entries); names up to 48 letters of either case, digits, `.`, `_`, `-` or `+`, compared case-sensitively.
//!
//! A v3 entry: used flag, name length, start sector (4), length in bytes (12), sector count (20), name (28).
//!
//! A v4 entry (128 bytes) is its own layout: 0 kind (1 file, 2 folder), 1 name length, 2 extent count (0 to 3),
//! 4 CRC32 of the entry with that field zeroed, 8 length in bytes, 16 total sectors, 24 the slot number plus one of
//! the folder it lies in (0: the root), 32 modification time (Unix seconds of the local clock reading, 0 unknown),
//! 40 name, 88 up to three extents of (start sector u64, sector count u32). Folders are entries of the same flat
//! table: an empty folder exists, a rename is one entry write, each path component has its own 48 characters, and
//! a path is resolved by one scan of the table per component. An entry whose CRC is wrong is damaged: skipped
//! like an empty slot and reported by `fsck`.
//!
//! A v4 volume keeps a copy of the superblock in sector 1 (the directory starts at sector 2); the copy is written
//! first, and `mount` falls back to it when sector 0 is unreadable or invalid.

use crate::block;

const MAGIC3: &[u8; 8] = b"FOSFS3\0\0";
const MAGIC4: &[u8; 8] = b"FOSFS4\0\0";
const DIRECTORY_SECTORS_V3: u64 = 8;
const ENTRY_SIZE_V3: usize = 64;
const ENTRY_SIZE_V4: usize = 128;
const MAX_NAME_V3: usize = 16;
const KIND_FILE: u8 = 1;
const KIND_FOLDER: u8 = 2;
/// Where a v4 entry records its folder.
const PARENT_AT: usize = 24;
const CRC_AT: usize = 4;
const MTIME_AT: usize = 32;
const NAME_AT_V3: usize = 28;
const NAME_AT_V4: usize = 40;
const EXTENTS_AT: usize = 88;
/// A v4 file is at most this many extents; a write that needs more than the three biggest free runs fails.
const MAX_EXTENTS: usize = 3;
type Extent = (u64, u32);
/// The longest path: folders nest by name, so this only bounds a line the shell passes in.
const MAX_PATH: usize = 192;
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
    /// Sector 0 was unusable and the copy in sector 1 was mounted (v4).
    backup_used: bool,
}

#[derive(Clone, Copy)]
pub struct Entry {
    directory_lba: u64,
    /// The slot number plus one: what a child's `parent` holds.
    id: u32,
    parent: u32,
    pub is_dir: bool,
    index: usize,
    extents: [Extent; MAX_EXTENTS],
    extent_count: usize,
    pub length: u64,
    /// Sectors in all extents.
    sector_count: u64,
    /// Unix seconds of the local clock reading; 0 when unknown (v3, or the clock was unset).
    pub modified: u64,
    name: [u8; MAX_NAME],
    name_length: usize,
}

impl Entry {
    pub fn name(&self) -> &[u8] {
        &self.name[..self.name_length]
    }

    /// The first (for a stream: only) extent's start sector.
    fn start(&self) -> u64 {
        self.extents[0].0
    }
}

/// What a directory slot holds, decoded and validated.
struct Parsed {
    folder: bool,
    name_length: usize,
    length: u64,
    sectors: u64,
    extents: [Extent; MAX_EXTENTS],
    extent_count: usize,
    parent: u32,
    modified: u64,
}

/// What `encode` writes into a slot.
struct Spec<'a> {
    folder: bool,
    leaf: &'a [u8],
    parent: u32,
    length: u64,
    sectors: u64,
    extents: [Extent; MAX_EXTENTS],
    extent_count: usize,
    modified: u64,
}

const CRC_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut n = 0;
    while n < 256 {
        let mut crc = n as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 };
            bit += 1;
        }
        table[n] = crc;
        n += 1;
    }
    table
};

/// IEEE CRC-32 (what `zlib.crc32` computes) of a v4 entry with its CRC field read as zero.
fn entry_crc(slot: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for (at, &byte) in slot.iter().enumerate() {
        let byte = if (CRC_AT..CRC_AT + 4).contains(&at) { 0 } else { byte };
        crc = CRC_TABLE[((crc ^ u32::from(byte)) & 0xff) as usize] ^ (crc >> 8);
    }
    !crc
}

fn seal(slot: &mut [u8]) {
    let crc = entry_crc(slot);
    slot[CRC_AT..CRC_AT + 4].copy_from_slice(&crc.to_le_bytes());
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
    /// A folder in the path does not exist (or is a file).
    NoDirectory,
    NotEmpty,
    /// The name is a folder where a file was wanted.
    IsDirectory,
    /// The name is a file where a folder was wanted.
    Exists,
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

/// A path any v4 volume can hold: names (`valid_name`) separated by single `/`, at most `MAX_PATH` bytes.
pub fn valid_path(path: &[u8]) -> bool {
    path.len() <= MAX_PATH && path.split(|&b| b == b'/').all(valid_name)
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

/// Splits `a/b/c` into (`a/b`, `c`); a bare name has an empty folder path.
fn split_path(path: &[u8]) -> (&[u8], &[u8]) {
    match path.iter().rposition(|&b| b == b'/') {
        Some(at) => (&path[..at], &path[at + 1..]),
        None => (&[], path),
    }
}

/// Scratch for extent searches and `check`: (start, end, name hash) of every extent. Static because a v4 directory
/// can hold 4096 entries of up to three extents (288 KiB), too much for the stack; the shell is single-threaded and
/// no caller nests.
static mut SCRATCH: [(u64, u64, u64); MAX_ENTRIES * MAX_EXTENTS] = [(0, 0, 0); MAX_ENTRIES * MAX_EXTENTS];

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

    /// The first directory sector: v4 keeps a copy of the superblock in sector 1.
    fn dir_start(&self) -> u64 {
        if self.version == 4 { 2 } else { 1 }
    }

    /// The first data sector: the superblock(s) and the directory come before it.
    fn data_start(&self) -> u64 {
        self.dir_start() + self.directory_sectors
    }

    fn name_at(&self) -> usize {
        if self.version == 4 { NAME_AT_V4 } else { NAME_AT_V3 }
    }

    fn max_name(&self) -> usize {
        if self.version == 4 { MAX_NAME } else { MAX_NAME_V3 }
    }

    /// Whether this version can hold `name` (v3: lowercase only, 16 characters).
    pub fn accepts_name(&self, name: &[u8]) -> bool {
        if self.version == 4 { valid_path(name) } else { valid_name_v3(name) }
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
            block::read(self.disk, self.base + self.dir_start() + done, &mut chunk[..count * 512])?;
            for sector in 0..count {
                for index in 0..per_sector {
                    let at = sector * 512 + index * size;
                    if !visit(self.dir_start() + done + sector as u64, index, &chunk[at..at + size]) {
                        return Ok(());
                    }
                }
            }
            done += count as u64;
        }
        Ok(())
    }

    /// Decodes a superblock sector for a partition of `sectors` sectors: (version, directory sectors, recorded size).
    fn parse_super(sector: &[u8], sectors: u64) -> Option<(u8, u64, u64)> {
        let version = match &sector[..8] {
            m if m == MAGIC3 => 3,
            m if m == MAGIC4 => 4,
            _ => return None,
        };
        let recorded = get_u64(sector, 28);
        let (entry_size, directory_sectors) = (u16::from_le_bytes([sector[10], sector[11]]) as usize, u64::from(u32::from_le_bytes(sector[16..20].try_into().unwrap())));
        let expected_entry = if version == 4 { ENTRY_SIZE_V4 } else { ENTRY_SIZE_V3 };
        let geometry = if version == 4 { (1..=MAX_ENTRIES as u64 * 128 / 512).contains(&directory_sectors) } else { directory_sectors == DIRECTORY_SECTORS_V3 };
        let data_start = if version == 4 { 2 } else { 1 } + directory_sectors;
        let valid = u16::from_le_bytes([sector[8], sector[9]]) == u16::from(version)
            && entry_size == expected_entry
            && u32::from_le_bytes(sector[12..16].try_into().unwrap()) == 1
            && geometry
            && get_u64(sector, 20) == data_start
            && recorded >= 8
            && recorded <= sectors.saturating_sub(data_start);
        valid.then_some((version, directory_sectors, recorded))
    }

    /// Checks the superblock; `Err(NotFormatted)` if this is not a FOSFS v3 or v4 volume that fits the `sectors` of its partition.
    /// The volume may be smaller than its partition (the partition was grown first, `grow` is about to enlarge the volume,
    /// or it was interrupted between the two): the superblock's size is the volume's size. A v4 volume whose sector 0 is
    /// bad is mounted from the copy in sector 1 (`backup_used`).
    pub fn mount(disk: usize, base: u64, sectors: u64) -> Result<Volume, Error> {
        let mut volume = Volume { disk, base, sectors, version: 3, directory_sectors: DIRECTORY_SECTORS_V3, backup_used: false };
        let primary = volume.read_sector(0)?;
        let (version, directory_sectors, recorded, backup_used) = match Volume::parse_super(&primary, sectors) {
            Some((version, directory_sectors, recorded)) => (version, directory_sectors, recorded, false),
            None => {
                let copy = volume.read_sector(1)?;
                match Volume::parse_super(&copy, sectors) {
                    Some((4, directory_sectors, recorded)) => (4, directory_sectors, recorded, true),
                    _ => return Err(Error::NotFormatted),
                }
            }
        };
        volume.version = version;
        volume.directory_sectors = directory_sectors;
        volume.backup_used = backup_used;
        volume.sectors = volume.data_start() + recorded;
        Ok(volume)
    }

    /// The superblock as it is now: sector 0, or the copy when that is what was mounted.
    fn read_super(&self) -> Result<[u8; 512], Error> {
        self.read_sector(u64::from(self.backup_used))
    }

    /// Writes a superblock: the copy first (v4), then sector 0, so a cut between them leaves one valid copy.
    fn write_super(&self, sector: &[u8; 512]) -> Result<(), Error> {
        if self.version == 4 {
            block::write(self.disk, self.base + 1, sector)?;
        }
        block::write(self.disk, self.base, sector)?;
        Ok(())
    }

    /// Enlarges the volume to `sectors` sectors of its (grown) partition: only the size in the superblock changes, since
    /// files are extents addressed from the start of the volume and the new space is simply free (a v4 directory keeps
    /// the size it was formatted with). Returns the volume as it is now. The partition must already be at least that big.
    pub fn grow(&self, sectors: u64) -> Result<Volume, Error> {
        if sectors <= self.sectors {
            return Ok(*self);
        }
        let mut sector = self.read_super()?;
        sector[28..36].copy_from_slice(&(sectors - self.data_start()).to_le_bytes());
        self.write_super(&sector)?;
        Volume::mount(self.disk, self.base, sectors)
    }

    /// Writes an empty FOSFS v3 volume; see `format_version`.
    pub fn format(disk: usize, base: u64, sectors: u64, label: &[u8]) -> Result<Volume, Error> {
        Volume::format_version(disk, base, sectors, label, 3)
    }

    /// Writes an empty FOSFS volume of `version` (3 or 4) over the `sectors` sectors at `base` of `disk`: the
    /// superblock (magic, version, geometry, optional label; v4 twice) and zeroed directory sectors. The data area is
    /// not touched (it is unreachable until a file is written). Destroys whatever volume was there; callers confirm first.
    pub fn format_version(disk: usize, base: u64, sectors: u64, label: &[u8], version: u8) -> Result<Volume, Error> {
        let (magic, entry_size, directory_sectors, dir_start) = match version {
            3 => (MAGIC3, ENTRY_SIZE_V3, DIRECTORY_SECTORS_V3, 1),
            4 => (MAGIC4, ENTRY_SIZE_V4, directory_sectors_v4(sectors), 2),
            _ => return Err(Error::NotFormatted),
        };
        let data_start = dir_start + directory_sectors;
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
        // Directory first, superblock last: a half-finished format is not a valid volume. Sector 1 of a v4 volume
        // is the superblock copy, so it is zeroed (invalid) until the copy is written below.
        let zeros = [0u8; CHUNK_SECTORS * 512];
        if version == 4 {
            block::write(disk, base + 1, &zeros[..512])?;
        }
        let mut done = 0;
        while done < directory_sectors {
            let count = (directory_sectors - done).min(CHUNK_SECTORS as u64) as usize;
            block::write(disk, base + dir_start + done, &zeros[..count * 512])?;
            done += count as u64;
        }
        if version == 4 {
            block::write(disk, base + 1, &sector)?;
        }
        block::write(disk, base, &sector)?;
        Volume::mount(disk, base, sectors)
    }

    /// The volume's label (empty if it has none).
    pub fn label(&self) -> Result<Label, Error> {
        let sector = self.read_super()?;
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
        Ok(self.find_file(name)?.map(|entry| entry.length))
    }

    /// Sets the label; an empty slice clears it.
    pub fn set_label(&self, label: &[u8]) -> Result<(), Error> {
        if !label.is_empty() && !(valid_label(label) && label.len() <= self.max_name()) {
            return Err(Error::InvalidName);
        }
        let mut sector = self.read_super()?;
        sector[LABEL_OFFSET..LABEL_OFFSET + self.max_name()].fill(0);
        sector[LABEL_OFFSET..LABEL_OFFSET + label.len()].copy_from_slice(label);
        self.write_super(&sector)
    }

    /// Decodes a slot, or `None` for an empty slot or a damaged entry (bad CRC, impossible extents, bad name length).
    fn parse(&self, slot: &[u8]) -> Option<Parsed> {
        let name_length = usize::from(slot[1]);
        if self.version == 3 {
            if slot[0] != KIND_FILE {
                return None;
            }
            let (start, length, sectors) = (get_u64(slot, 4), get_u64(slot, 12), get_u64(slot, 20));
            let good = name_length != 0
                && name_length <= MAX_NAME_V3
                && start >= self.data_start()
                && start.saturating_add(sectors) <= self.sectors
                && sectors <= u64::from(u32::MAX)
                && length <= sectors * 512;
            return good.then_some(Parsed { folder: false, name_length, length, sectors, extents: [(start, sectors as u32), (0, 0), (0, 0)], extent_count: 1, parent: 0, modified: 0 });
        }
        let folder = match slot[0] {
            KIND_FILE => false,
            KIND_FOLDER => true,
            _ => return None,
        };
        let extent_count = usize::from(slot[2]);
        if name_length == 0 || name_length > MAX_NAME || extent_count > MAX_EXTENTS || u32::from_le_bytes(slot[CRC_AT..CRC_AT + 4].try_into().unwrap()) != entry_crc(slot) {
            return None;
        }
        let (length, sectors) = (get_u64(slot, 8), get_u64(slot, 16));
        let mut extents = [(0, 0); MAX_EXTENTS];
        let mut total = 0u64;
        for (n, extent) in extents.iter_mut().enumerate().take(extent_count) {
            let at = EXTENTS_AT + n * 12;
            *extent = (get_u64(slot, at), u32::from_le_bytes(slot[at + 8..at + 12].try_into().unwrap()));
            if extent.0 < self.data_start() || extent.0.saturating_add(u64::from(extent.1)) > self.sectors {
                return None;
            }
            total += u64::from(extent.1);
        }
        if total != sectors || length > sectors * 512 || (folder && (extent_count != 0 || length != 0)) {
            return None;
        }
        Some(Parsed { folder, name_length, length, sectors, extents, extent_count, parent: u32::from_le_bytes(slot[PARENT_AT..PARENT_AT + 4].try_into().unwrap()), modified: get_u64(slot, MTIME_AT) })
    }

    /// Writes `spec` into `slot` (zeroed first), in this version's layout; v4 entries get their CRC.
    fn encode(&self, slot: &mut [u8], spec: &Spec) {
        slot.fill(0);
        slot[1] = spec.leaf.len() as u8;
        if self.version == 3 {
            // A v3 entry always has a start inside the data area, even when empty.
            let start = if spec.extent_count > 0 { spec.extents[0].0 } else { self.data_start() };
            slot[0] = KIND_FILE;
            slot[4..12].copy_from_slice(&start.to_le_bytes());
            slot[12..20].copy_from_slice(&spec.length.to_le_bytes());
            slot[20..28].copy_from_slice(&spec.sectors.to_le_bytes());
            slot[NAME_AT_V3..NAME_AT_V3 + spec.leaf.len()].copy_from_slice(spec.leaf);
            return;
        }
        slot[0] = if spec.folder { KIND_FOLDER } else { KIND_FILE };
        slot[2] = spec.extent_count as u8;
        slot[8..16].copy_from_slice(&spec.length.to_le_bytes());
        slot[16..24].copy_from_slice(&spec.sectors.to_le_bytes());
        slot[PARENT_AT..PARENT_AT + 4].copy_from_slice(&spec.parent.to_le_bytes());
        slot[MTIME_AT..MTIME_AT + 8].copy_from_slice(&spec.modified.to_le_bytes());
        slot[NAME_AT_V4..NAME_AT_V4 + spec.leaf.len()].copy_from_slice(spec.leaf);
        for (n, extent) in spec.extents.iter().enumerate().take(spec.extent_count) {
            let at = EXTENTS_AT + n * 12;
            slot[at..at + 8].copy_from_slice(&extent.0.to_le_bytes());
            slot[at + 8..at + 12].copy_from_slice(&extent.1.to_le_bytes());
        }
        seal(slot);
    }

    fn entry_from(&self, lba: u64, index: usize, slot: &[u8], parsed: &Parsed) -> Entry {
        let mut name = [0u8; MAX_NAME];
        let at = self.name_at();
        name[..parsed.name_length].copy_from_slice(&slot[at..at + parsed.name_length]);
        let id = ((lba - self.dir_start()) * self.entries_per_sector() as u64 + index as u64 + 1) as u32;
        Entry {
            directory_lba: lba,
            id,
            parent: parsed.parent,
            is_dir: parsed.folder,
            index,
            extents: parsed.extents,
            extent_count: parsed.extent_count,
            length: parsed.length,
            sector_count: parsed.sectors,
            modified: parsed.modified,
            name,
            name_length: parsed.name_length,
        }
    }

    /// Calls `visit` for every valid entry of every folder.
    fn for_each_all(&self, mut visit: impl FnMut(Entry)) -> Result<(), Error> {
        self.scan(|lba, index, slot| {
            // A damaged entry is skipped, not trusted.
            if let Some(parsed) = self.parse(slot) {
                visit(self.entry_from(lba, index, slot, &parsed));
            }
            true
        })
    }

    /// Calls `visit` for every valid entry in the root (files and, on v4, folders).
    pub fn for_each(&self, mut visit: impl FnMut(Entry)) -> Result<(), Error> {
        self.for_each_all(|entry| {
            if entry.parent == 0 {
                visit(entry);
            }
        })
    }

    /// Calls `visit` for every entry of the folder `path` (empty: the root); `NoDirectory` if there is no such folder.
    pub fn for_each_in(&self, path: &[u8], mut visit: impl FnMut(Entry)) -> Result<(), Error> {
        let parent = self.resolve_folder(path)?;
        self.for_each_all(|entry| {
            if entry.parent == parent {
                visit(entry);
            }
        })
    }

    /// The id of folder `path` (0 for the root, i.e. an empty path).
    fn resolve_folder(&self, path: &[u8]) -> Result<u32, Error> {
        let mut current = 0;
        if path.is_empty() {
            return Ok(0);
        }
        for component in path.split(|&b| b == b'/') {
            match self.find_child(current, component)? {
                Some(entry) if entry.is_dir => current = entry.id,
                _ => return Err(Error::NoDirectory),
            }
        }
        Ok(current)
    }

    fn find_child(&self, parent: u32, name: &[u8]) -> Result<Option<Entry>, Error> {
        let mut result = None;
        self.for_each_all(|entry| {
            if result.is_none() && entry.parent == parent && entry.name() == name {
                result = Some(entry);
            }
        })?;
        Ok(result)
    }

    /// Whether directory slot number `id` (as stored plus one) holds a folder.
    fn is_folder(&self, id: u32) -> Result<bool, Error> {
        let per_sector = self.entries_per_sector() as u32;
        let (lba, index) = (self.dir_start() + u64::from((id - 1) / per_sector), ((id - 1) % per_sector) as usize);
        if lba >= self.data_start() {
            return Ok(false);
        }
        let sector = self.read_sector(lba)?;
        let slot = &sector[index * self.entry_size()..(index + 1) * self.entry_size()];
        Ok(self.parse(slot).is_some_and(|parsed| parsed.folder))
    }

    /// Reads the directory the way a repair tool would: reports every file's extent through `line`, and every problem
    /// (a damaged entry, extents that overlap, a name used twice). Returns (files, problems); nothing is changed. There is
    /// nothing to replay or roll back after a power cut, so this only makes visible what mount and `for_each` already
    /// skip over.
    pub fn check(&self, mut line: impl FnMut(core::fmt::Arguments)) -> Result<(usize, usize), Error> {
        let table = unsafe { &mut *core::ptr::addr_of_mut!(SCRATCH) };
        let (mut rows, mut entries, mut problems) = (0, 0, 0);
        let mut failed = false;
        let per_sector = self.entries_per_sector() as u64;
        if self.backup_used {
            problems += 1;
            line(format_args!("the superblock in sector 0 is damaged; mounted from the copy in sector 1 (`label` rewrites both)"));
        }
        self.scan(|lba, index, slot| {
            if slot[0] == 0 {
                return true;
            }
            let Some(parsed) = self.parse(slot) else {
                problems += 1;
                line(format_args!("directory entry {} is damaged (ignored)", (lba - self.dir_start()) * per_sector + index as u64));
                return true;
            };
            if rows + parsed.extent_count.max(1) > table.len() {
                failed = true;
                return false;
            }
            let at = self.name_at();
            let name = &slot[at..at + parsed.name_length];
            // The name hash rides on the first row of an entry only (0 marks the others).
            let key = hash(name).wrapping_add(u64::from(parsed.parent).wrapping_mul(0x9e37_79b9_7f4a_7c15)) | 1;
            if parsed.extent_count == 0 {
                table[rows] = (0, 0, key);
                rows += 1;
            }
            for (n, &(start, count)) in parsed.extents[..parsed.extent_count].iter().enumerate() {
                table[rows] = (start, start + u64::from(count), if n == 0 { key } else { 0 });
                rows += 1;
            }
            entries += 1;
            let text = core::str::from_utf8(name).unwrap_or("?");
            if parsed.folder {
                line(format_args!("{text}/ (folder in {})", parsed.parent));
            } else if parsed.extent_count > 1 {
                line(format_args!("{:<16} {:>9} bytes, {} sectors in {} extents from sector {} (folder {})", text, parsed.length, parsed.sectors, parsed.extent_count, parsed.extents[0].0, parsed.parent));
            } else {
                line(format_args!("{:<16} {:>9} bytes at sector {}, {} sectors (folder {})", text, parsed.length, parsed.extents[0].0, parsed.sectors, parsed.parent));
            }
            if parsed.parent != 0 && !matches!(self.is_folder(parsed.parent), Ok(true)) {
                problems += 1;
                line(format_args!("{text} lies in folder {}, which does not exist (it is not listed)", parsed.parent));
            }
            true
        })?;
        if failed {
            return Err(Error::DirectoryFull);
        }
        let files = &mut table[..rows];
        files.sort_unstable();
        for pair in files.windows(2) {
            // Zero-length extents occupy nothing, so they cannot overlap.
            if pair[0].1 > pair[0].0 && pair[1].1 > pair[1].0 && pair[0].1 > pair[1].0 {
                problems += 1;
                line(format_args!("the extents at sectors {} and {} share sectors", pair[0].0, pair[1].0));
            }
        }
        files.sort_unstable_by_key(|file| file.2);
        for pair in files.windows(2) {
            if pair[0].2 != 0 && pair[0].2 == pair[1].2 {
                problems += 1;
                line(format_args!("a name appears twice (the files at sectors {} and {})", pair[0].0, pair[1].0));
            }
        }
        Ok((entries, problems))
    }

    /// The entry at `path` (a name, or `folder/name` on v4); `None` if there is none or a folder in the path is missing.
    pub fn find(&self, path: &[u8]) -> Result<Option<Entry>, Error> {
        let (folder, name) = split_path(path);
        let parent = match self.resolve_folder(folder) {
            Ok(parent) => parent,
            Err(Error::NoDirectory) => return Ok(None),
            Err(error) => return Err(error),
        };
        self.find_child(parent, name)
    }

    /// The file at `path`; a folder counts as absent.
    fn find_file(&self, path: &[u8]) -> Result<Option<Entry>, Error> {
        Ok(self.find(path)?.filter(|entry| !entry.is_dir))
    }

    /// Reads a whole file into `buffer`; returns its length, or None if absent. A
    /// buffer that is too small is an error rather than a silent truncation.
    pub fn read(&self, name: &[u8], buffer: &mut [u8]) -> Result<Option<usize>, Error> {
        let Some(entry) = self.find_file(name)? else { return Ok(None) };
        let length = entry.length as usize;
        if length > buffer.len() {
            return Err(Error::TooBig);
        }
        let mut done = 0;
        for &(start, count) in &entry.extents[..entry.extent_count] {
            let take = (count as usize * 512).min(length - done);
            self.read_data(start, &mut buffer[done..done + take])?;
            done += take;
        }
        Ok(Some(length))
    }

    /// Reads `out.len()` bytes starting at sector `start`: whole sectors go straight into `out`, a partial last
    /// sector via a copy.
    fn read_data(&self, start: u64, out: &mut [u8]) -> Result<(), Error> {
        let whole = out.len() / 512 * 512;
        if whole > 0 {
            block::read(self.disk, self.base + start, &mut out[..whole])?;
        }
        if out.len() > whole {
            let tail = self.read_sector(start + (whole / 512) as u64)?;
            let rest = out.len() - whole;
            out[whole..].copy_from_slice(&tail[..rest]);
        }
        Ok(())
    }

    /// Writes directory slot (`lba`, `index`) from `spec`.
    fn put_entry(&self, lba: u64, index: usize, spec: &Spec) -> Result<(), Error> {
        let mut directory = self.read_sector(lba)?;
        let at = index * self.entry_size();
        self.encode(&mut directory[at..at + self.entry_size()], spec);
        block::write(self.disk, self.base + lba, &directory)?;
        Ok(())
    }

    /// The modification time to record now (v3 has no field for it).
    fn now(&self) -> u64 {
        if self.version == 4 { crate::clock::unix_seconds() } else { 0 }
    }

    /// Creates or replaces a file. The new contents go to free extents of their own and the single directory-sector
    /// write that points the entry at them is the commit, so a power cut leaves the old file or the new one, never a mix
    /// (there is no allocation map to repair: the directory is the only record of what is used). A v4 file takes one
    /// free run when one fits, else up to three. Only when the disk has no room for the new copy next to the old one is
    /// the old one's space reused, and that case is not crash-safe.
    pub fn write(&self, name: &[u8], data: &[u8]) -> Result<(), Error> {
        if !self.accepts_name(name) {
            return Err(Error::InvalidName);
        }
        let (folder, leaf) = split_path(name);
        let parent = self.resolve_folder(folder)?;
        let existing = self.find_child(parent, leaf)?;
        if existing.as_ref().is_some_and(|entry| entry.is_dir) {
            return Err(Error::IsDirectory);
        }
        let needed = data.len().div_ceil(512) as u64;
        let (extents, extent_count) = match self.allocate(needed, None) {
            Err(Error::DataFull) if existing.is_some() => self.allocate(needed, existing.as_ref())?,
            other => other?,
        };
        let slot = match &existing {
            Some(entry) => (entry.directory_lba, entry.index),
            None => self.free_slot()?.ok_or(Error::DirectoryFull)?,
        };
        // Data first, directory entry last: a failure in between leaves the old file intact.
        let mut done = 0;
        for &(start, count) in &extents[..extent_count] {
            let take = (count as usize * 512).min(data.len() - done);
            self.write_data(start, &data[done..done + take])?;
            done += take;
        }
        self.put_entry(slot.0, slot.1, &Spec { folder: false, leaf, parent, length: data.len() as u64, sectors: needed, extents, extent_count, modified: self.now() })
    }

    /// Reserves a file for streaming: one contiguous extent of `max_bytes` (the largest free extent when 0), with
    /// a directory entry of length 0 that `write_extent` grows. Replaces an existing file of that name. Returns the
    /// entry and the reserved capacity in bytes. Nothing is written to the extent, so the old contents show through
    /// until they are overwritten; that is why the length stays 0 until data has really been written.
    pub fn reserve(&self, name: &[u8], max_bytes: u64) -> Result<(Entry, u64), Error> {
        if !self.accepts_name(name) {
            return Err(Error::InvalidName);
        }
        let (folder, leaf) = split_path(name);
        let parent = self.resolve_folder(folder)?;
        let existing = self.find_child(parent, leaf)?;
        if existing.as_ref().is_some_and(|entry| entry.is_dir) {
            return Err(Error::IsDirectory);
        }
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
        let count = count.min(u64::from(u32::MAX));
        let extents = [(start, count as u32), (0, 0), (0, 0)];
        let modified = self.now();
        self.put_entry(slot.0, slot.1, &Spec { folder: false, leaf, parent, length: 0, sectors: count, extents, extent_count: 1, modified })?;
        let id = ((slot.0 - self.dir_start()) * self.entries_per_sector() as u64 + slot.1 as u64 + 1) as u32;
        let mut entry = Entry { directory_lba: slot.0, id, parent, is_dir: false, index: slot.1, extents, extent_count: 1, length: 0, sector_count: count, modified, name: [0; MAX_NAME], name_length: leaf.len() };
        entry.name[..leaf.len()].copy_from_slice(leaf);
        Ok((entry, count * 512))
    }

    /// Writes `data` into a reserved file at byte `offset` (a multiple of 512; a final partial sector is padded) and,
    /// when `file_length` is non-zero, records it as the file's length. The data goes first and the length last, so a
    /// reset keeps everything up to the previous length.
    pub fn write_extent(&self, entry: &Entry, offset: u64, data: &[u8], file_length: u64) -> Result<(), Error> {
        if offset % 512 != 0 || offset + data.len() as u64 > entry.sector_count * 512 {
            return Err(Error::TooBig);
        }
        self.write_data(entry.start() + offset / 512, data)?;
        if file_length != 0 {
            let now = self.now();
            self.patch_slot(entry, |slot| {
                let at = if self.version == 4 { 8 } else { 12 };
                slot[at..at + 8].copy_from_slice(&file_length.to_le_bytes());
                if self.version == 4 && now != 0 {
                    slot[MTIME_AT..MTIME_AT + 8].copy_from_slice(&now.to_le_bytes());
                }
            })?;
        }
        Ok(())
    }

    /// Changes bytes of an existing entry's slot in place; a v4 entry that is still in use gets its CRC refreshed.
    fn patch_slot(&self, entry: &Entry, patch: impl FnOnce(&mut [u8])) -> Result<(), Error> {
        let mut directory = self.read_sector(entry.directory_lba)?;
        let at = entry.index * self.entry_size();
        let slot = &mut directory[at..at + self.entry_size()];
        patch(slot);
        if self.version == 4 && slot[0] != 0 {
            seal(slot);
        }
        block::write(self.disk, self.base + entry.directory_lba, &directory)?;
        Ok(())
    }

    /// Ends a streamed file: records its final length and gives the unused part of the reservation back, so the
    /// space a take reserved but did not fill is free again.
    pub fn trim_extent(&self, entry: &Entry, file_length: u64) -> Result<(), Error> {
        let sectors = file_length.div_ceil(512).min(entry.sector_count);
        let now = self.now();
        self.patch_slot(entry, |slot| {
            if self.version == 4 {
                slot[8..16].copy_from_slice(&file_length.to_le_bytes());
                slot[16..24].copy_from_slice(&sectors.to_le_bytes());
                slot[EXTENTS_AT + 8..EXTENTS_AT + 12].copy_from_slice(&(sectors as u32).to_le_bytes());
                if now != 0 {
                    slot[MTIME_AT..MTIME_AT + 8].copy_from_slice(&now.to_le_bytes());
                }
            } else {
                slot[12..20].copy_from_slice(&file_length.to_le_bytes());
                slot[20..28].copy_from_slice(&sectors.to_le_bytes());
            }
        })
    }

    /// Removes a file, or an empty folder (`NotEmpty` otherwise); false if there is no such entry.
    pub fn remove(&self, name: &[u8]) -> Result<bool, Error> {
        let Some(entry) = self.find(name)? else { return Ok(false) };
        if entry.is_dir {
            let mut busy = false;
            self.for_each_all(|other| busy |= other.parent == entry.id)?;
            if busy {
                return Err(Error::NotEmpty);
            }
        }
        self.patch_slot(&entry, |slot| slot.fill(0))?;
        Ok(true)
    }

    /// Makes folder `path` (its parent must exist); already being a folder is fine. v4 only.
    pub fn mkdir(&self, path: &[u8]) -> Result<(), Error> {
        if self.version != 4 || !valid_path(path) {
            return Err(Error::InvalidName);
        }
        let (folder, leaf) = split_path(path);
        let parent = self.resolve_folder(folder)?;
        match self.find_child(parent, leaf)? {
            Some(entry) if entry.is_dir => return Ok(()),
            Some(_) => return Err(Error::Exists),
            None => {}
        }
        let slot = self.free_slot()?.ok_or(Error::DirectoryFull)?;
        self.put_entry(slot.0, slot.1, &Spec { folder: true, leaf, parent, length: 0, sectors: 0, extents: [(0, 0); MAX_EXTENTS], extent_count: 0, modified: self.now() })
    }

    /// Makes every folder on the way to the file `path` that is missing (`a/b/c` makes `a` and `a/b`); a bare name
    /// has none. What a write under a mount point does, so that `config/phono.cfg` works on a root that has no
    /// `config` folder yet.
    pub fn make_parents(&self, path: &[u8]) -> Result<(), Error> {
        let (folder, _) = split_path(path);
        let mut end = 0;
        while end < folder.len() {
            end = folder[end..].iter().position(|&b| b == b'/').map_or(folder.len(), |at| end + at);
            self.mkdir(&folder[..end])?;
            end += 1;
        }
        Ok(())
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
        self.for_each_all(|entry| used += entry.sector_count)?;
        let total = self.sectors.saturating_sub(self.data_start()).saturating_sub(used);
        let largest = self.largest_free_extent(None)?.map_or(0, |(_, count)| count);
        Ok((total * 512, largest * 512))
    }

    /// The (start, end) extents of every file except `replacing`, sorted by start, in the static scratch table.
    fn sorted_extents(&self, replacing: Option<&Entry>) -> Result<&'static mut [(u64, u64, u64)], Error> {
        let table = unsafe { &mut *core::ptr::addr_of_mut!(SCRATCH) };
        let mut used = 0;
        self.for_each_all(|entry| {
            if replacing.is_some_and(|r| r.directory_lba == entry.directory_lba && r.index == entry.index) {
                return;
            }
            for &(start, count) in &entry.extents[..entry.extent_count] {
                if count > 0 && used < table.len() {
                    table[used] = (start, start + u64::from(count), 0);
                    used += 1;
                }
            }
        })?;
        let extents = &mut table[..used];
        extents.sort_unstable();
        Ok(extents)
    }

    /// The three largest runs of free sectors (start, count), biggest first, ignoring the extents of `replacing`;
    /// unused places hold (0, 0). Found while walking the sorted extents, so no list of all gaps is kept.
    fn largest_gaps(&self, replacing: Option<&Entry>) -> Result<[(u64, u64); MAX_EXTENTS], Error> {
        let extents = self.sorted_extents(replacing)?;
        let mut best = [(0u64, 0u64); MAX_EXTENTS];
        let mut offer = |start: u64, count: u64| {
            let mut item = (start, count);
            for place in best.iter_mut() {
                if item.1 > place.1 {
                    core::mem::swap(place, &mut item);
                }
            }
        };
        let mut candidate = self.data_start();
        for &(start, end, _) in extents.iter() {
            if start > candidate {
                offer(candidate, start - candidate);
            }
            candidate = candidate.max(end);
        }
        if self.sectors > candidate {
            offer(candidate, self.sectors - candidate);
        }
        Ok(best)
    }

    /// Finds room for `needed` sectors, ignoring the extents of `replacing`: one run when there is one (first fit), else
    /// on v4 the biggest runs, up to three, in start order. `DataFull` if even those are too small.
    fn allocate(&self, needed: u64, replacing: Option<&Entry>) -> Result<([Extent; MAX_EXTENTS], usize), Error> {
        let mut extents = [(0, 0); MAX_EXTENTS];
        if needed == 0 {
            return Ok((extents, 0));
        }
        if needed > u64::from(u32::MAX) {
            return Err(Error::TooBig);
        }
        if let Some(start) = self.free_extent(needed, replacing)? {
            extents[0] = (start, needed as u32);
            return Ok((extents, 1));
        }
        if self.version != 4 {
            return Err(Error::DataFull);
        }
        let (mut count, mut remaining) = (0, needed);
        for (start, size) in self.largest_gaps(replacing)? {
            if size == 0 || remaining == 0 {
                break;
            }
            let take = size.min(remaining);
            extents[count] = (start, take as u32);
            count += 1;
            remaining -= take;
        }
        if remaining > 0 {
            return Err(Error::DataFull);
        }
        extents[..count].sort_unstable();
        Ok((extents, count))
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
