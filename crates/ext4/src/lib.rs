//! ext4 for fos, read-only: mount a volume Linux (or `mkfs.ext4`) made, walk its folders, read its files.
//!
//! Why: the data volume of a file server must be something any Linux machine can mount when the fos box dies
//! (docs/ext4.md). The driver is `no_std`, allocates nothing, and reaches the disk through `Device`, so the same
//! code runs on the embedded kernel, on the server kernel and in the host tests (images from `mkfs.ext4`).
//!
//! Handled: 1, 2 and 4 KiB blocks; 32- and 64-byte group descriptors (`64bit`), `flex_bg`; extent trees of any
//! depth and the old direct/indirect block maps (ext2/ext3 volumes); uninitialised extents (read as zeros) and
//! sparse files; folders: `lookup` walks the hash tree of an indexed folder, `list` scans linearly (which also reads
//! an indexed folder). Refused with a reason: a journal that needs recovery (`mount_with` can ignore it, then the
//! last transactions are missing), `meta_bg`, inline data, compression, encryption, case folding, other block sizes.
//! Checksums are not verified yet.
//!
//! Performance shape (for the server kernel): `map` returns whole contiguous runs and `read` hands every whole-block
//! run to the device in one call, straight into the caller's buffer, so a big file is a few large transfers. The
//! group-descriptor and inode-table blocks have a one-block cache each; a larger block cache belongs one layer up
//! where an allocator exists.
#![cfg_attr(not(test), no_std)]

mod cache;
mod crc;
mod dirhash;
mod journal;
mod write;
pub use cache::{Cache, Cached};
pub use journal::{open, Journaled, Stage};
pub use write::{format_raw, FormatOptions, Writer};

/// The device's sector: `Device` addresses in 512-byte sectors from the first sector of the volume.
pub const SECTOR: usize = 512;
const MAX_BLOCK: usize = 4096;
const MAGIC: u16 = 0xef53;
const ROOT: u32 = 2;

const INCOMPAT_FILETYPE: u32 = 0x2;
const INCOMPAT_RECOVER: u32 = 0x4;
const INCOMPAT_EXTENTS: u32 = 0x40;
const INCOMPAT_64BIT: u32 = 0x80;
const INCOMPAT_MMP: u32 = 0x100;
const INCOMPAT_FLEX_BG: u32 = 0x200;
const INCOMPAT_EA_INODE: u32 = 0x400;
const INCOMPAT_CSUM_SEED: u32 = 0x2000;
const INCOMPAT_LARGEDIR: u32 = 0x4000;
const INCOMPAT_INLINE_DATA: u32 = 0x8000;
const INCOMPAT_SUPPORTED: u32 = INCOMPAT_FILETYPE
    | INCOMPAT_RECOVER
    | INCOMPAT_EXTENTS
    | INCOMPAT_64BIT
    | INCOMPAT_MMP
    | INCOMPAT_FLEX_BG
    | INCOMPAT_EA_INODE
    | INCOMPAT_CSUM_SEED
    | INCOMPAT_LARGEDIR
    | INCOMPAT_INLINE_DATA;

const FLAG_EXTENTS: u32 = 0x8_0000;
const FLAG_INLINE_DATA: u32 = 0x1000_0000;

/// A block device: whole sectors, addressed from the first sector of the volume.
pub trait Device {
    fn read(&mut self, lba: u64, buffer: &mut [u8]) -> Result<(), Error>;
    /// Writes whole sectors. A read-only device keeps this default and `Writer` then fails with `ReadOnly`.
    fn write(&mut self, _lba: u64, _buffer: &[u8]) -> Result<(), Error> {
        Err(Error::ReadOnly)
    }
    /// File data: like `write`, but not journaled (metadata only is). Defaults to `write`.
    fn write_data(&mut self, lba: u64, buffer: &[u8]) -> Result<(), Error> {
        self.write(lba, buffer)
    }
    /// Makes the file data written with `write_data` durable (it may have stayed in the drive's cache). Called before the
    /// metadata that refers to it is committed.
    fn sync(&mut self) -> Result<(), Error> {
        Ok(())
    }
    /// Makes the metadata written so far durable as one transaction.
    fn commit(&mut self) -> Result<(), Error> {
        Ok(())
    }
    /// Commits, and leaves the volume marked clean (no journal replay needed).
    fn close(&mut self) -> Result<(), Error> {
        Ok(())
    }
    /// Metadata blocks staged and not committed.
    fn pending(&self) -> usize {
        0
    }
    /// True when metadata writes are journaled, which a volume with a journal needs from a writer.
    fn journaled(&self) -> bool {
        false
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The device failed a read.
    Device,
    /// No ext2/3/4 superblock.
    NotExt,
    /// A feature this driver does not handle (the volume is refused, or just this file).
    Unsupported,
    /// The journal holds transactions that were never applied; mount with `ignore_journal` to read what is on disk.
    NeedsRecovery,
    /// Structures that contradict themselves.
    Corrupt,
    NotADirectory,
    /// The device cannot be written, or the volume has features a writer cannot keep consistent (`metadata_csum`, ...).
    ReadOnly,
    Exists,
    NotFound,
    NoSpace,
    NotEmpty,
    IsADirectory,
    /// A file name that is empty, longer than 255 bytes, or holds `/` or NUL.
    InvalidName,
    /// The file's extents would not fit the inode plus four leaf blocks.
    TooFragmented,
    TooManyLinks,
    /// An impossible request (a folder moved into itself, ...).
    Invalid,
}

/// What `mount_with` may be told.
#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    /// Mount a volume whose journal was not replayed (the unclean-shutdown case) anyway.
    pub ignore_journal: bool,
}

/// An inode as the driver needs it. `block` is the inode's 60-byte map (an extent tree root, block pointers, or a fast
/// symbolic link's target).
#[derive(Clone, Copy, Debug)]
pub struct Inode {
    pub number: u32,
    pub mode: u16,
    pub size: u64,
    pub flags: u32,
    pub links: u16,
    pub atime: u32,
    pub ctime: u32,
    pub mtime: u32,
    pub uid: u32,
    pub gid: u32,
    block: [u8; 60],
}

impl Inode {
    pub fn is_dir(&self) -> bool {
        self.mode & 0xf000 == 0x4000
    }
    pub fn is_file(&self) -> bool {
        self.mode & 0xf000 == 0x8000
    }
    pub fn is_symlink(&self) -> bool {
        self.mode & 0xf000 == 0xa000
    }
    /// The permission bits (`rwxrwxrwx` plus setuid/setgid/sticky).
    pub fn permissions(&self) -> u16 {
        self.mode & 0o7777
    }
    /// The target of a fast symbolic link (up to 59 bytes, kept inside the inode); longer targets are read like a file.
    pub fn fast_target(&self) -> Option<&[u8]> {
        (self.is_symlink() && self.size < 60 && self.flags & FLAG_EXTENTS == 0 && self.flags & FLAG_INLINE_DATA == 0).then(|| &self.block[..self.size as usize])
    }
}

/// One folder entry as `list` hands it over. `kind` is the file-type byte (1 file, 2 folder, 7 symbolic link, ...; 0 on a
/// volume without the `filetype` feature).
pub struct DirEntry<'a> {
    pub inode: u32,
    pub kind: u8,
    pub name: &'a [u8],
}

/// Where a run of a file's blocks is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mapping {
    /// No blocks stored: reads as zeros, for `blocks` blocks (at least; the next mapped block ends it).
    Hole { blocks: u64 },
    /// `blocks` contiguous volume blocks starting at `block`.
    Data { block: u64, blocks: u64 },
    /// Allocated but never written (an uninitialised extent): reads as zeros.
    Zeros { blocks: u64 },
}

fn le16(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn le32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

pub struct Fs<D: Device> {
    device: D,
    block_size: usize,
    blocks: u64,
    blocks_per_group: u32,
    free_blocks: u64,
    inodes: u32,
    free_inodes: u32,
    inodes_per_group: u32,
    first_data_block: u32,
    inode_size: usize,
    desc_size: usize,
    groups: u32,
    incompat: u32,
    compat: u32,
    ro_compat: u32,
    label: [u8; 16],
    /// `s_hash_seed`, the folder hash's seed.
    hash_seed: [u32; 4],
    /// `s_def_hash_version`, the hash a new index uses.
    def_hash: u8,
    /// The cached group-descriptor block and inode-table block (volume block numbers, `u64::MAX` for none).
    gdt_at: u64,
    gdt: [u8; MAX_BLOCK],
    itable_at: u64,
    itable: [u8; MAX_BLOCK],
    /// Extent-tree nodes and indirect blocks.
    node: [u8; MAX_BLOCK],
    /// Folder blocks and the unaligned ends of a read.
    data: [u8; MAX_BLOCK],
}

enum Step {
    Done(Mapping),
    Descend(u64),
}

impl<D: Device> Fs<D> {
    pub fn mount(device: D) -> Result<Self, Error> {
        Self::mount_with(device, Options::default())
    }

    pub fn mount_with(mut device: D, options: Options) -> Result<Self, Error> {
        let mut sb = [0u8; 1024];
        device.read(2, &mut sb)?;
        if le16(&sb, 56) != MAGIC {
            return Err(Error::NotExt);
        }
        let revision = le32(&sb, 76);
        let incompat = if revision >= 1 { le32(&sb, 96) } else { 0 };
        if incompat & !INCOMPAT_SUPPORTED != 0 {
            return Err(Error::Unsupported);
        }
        if incompat & INCOMPAT_RECOVER != 0 && !options.ignore_journal {
            return Err(Error::NeedsRecovery);
        }
        let log_block_size = le32(&sb, 24);
        if log_block_size > 2 {
            return Err(Error::Unsupported);
        }
        let block_size = 1024usize << log_block_size;
        let wide = incompat & INCOMPAT_64BIT != 0;
        let desc_size = if wide { usize::from(le16(&sb, 254)) } else { 32 };
        if desc_size < 32 || desc_size > block_size || !desc_size.is_power_of_two() {
            return Err(Error::Corrupt);
        }
        let inode_size = if revision >= 1 { usize::from(le16(&sb, 88)) } else { 128 };
        if inode_size < 128 || inode_size > block_size || !inode_size.is_power_of_two() {
            return Err(Error::Corrupt);
        }
        let blocks_per_group = le32(&sb, 32);
        let inodes_per_group = le32(&sb, 40);
        let first_data_block = le32(&sb, 20);
        let blocks = u64::from(le32(&sb, 4)) | if wide { u64::from(le32(&sb, 336)) << 32 } else { 0 };
        let free_blocks = u64::from(le32(&sb, 12)) | if wide { u64::from(le32(&sb, 344)) << 32 } else { 0 };
        if blocks_per_group == 0 || inodes_per_group == 0 || blocks <= u64::from(first_data_block) {
            return Err(Error::Corrupt);
        }
        let groups = (blocks - u64::from(first_data_block)).div_ceil(u64::from(blocks_per_group));
        let Ok(groups) = u32::try_from(groups) else { return Err(Error::Corrupt) };
        let mut label = [0u8; 16];
        label.copy_from_slice(&sb[120..136]);
        Ok(Fs {
            device,
            block_size,
            blocks,
            blocks_per_group,
            free_blocks,
            inodes: le32(&sb, 0),
            free_inodes: le32(&sb, 16),
            inodes_per_group,
            first_data_block,
            inode_size,
            desc_size,
            groups,
            incompat,
            compat: le32(&sb, 92),
            ro_compat: le32(&sb, 100),
            label,
            hash_seed: [le32(&sb, 0xec), le32(&sb, 0xf0), le32(&sb, 0xf4), le32(&sb, 0xf8)],
            def_hash: sb[0xfc],
            gdt_at: u64::MAX,
            gdt: [0; MAX_BLOCK],
            itable_at: u64::MAX,
            itable: [0; MAX_BLOCK],
            node: [0; MAX_BLOCK],
            data: [0; MAX_BLOCK],
        })
    }

    /// The volume label (up to 16 bytes, without the padding zeros).
    pub fn label(&self) -> &[u8] {
        let length = self.label.iter().position(|&byte| byte == 0).unwrap_or(16);
        &self.label[..length]
    }

    pub fn block_size(&self) -> usize {
        self.block_size
    }

    /// Size and free space in bytes, as the superblock last recorded them.
    pub fn space(&self) -> (u64, u64) {
        (self.blocks * self.block_size as u64, self.free_blocks * self.block_size as u64)
    }

    /// Inodes in total and free, as the superblock last recorded them.
    pub fn inode_counts(&self) -> (u32, u32) {
        (self.inodes, self.free_inodes)
    }

    /// Whether the `filetype` feature is on (folder entries carry the kind of the file).
    pub fn has_file_types(&self) -> bool {
        self.incompat & INCOMPAT_FILETYPE != 0
    }

    /// Gives the device back.
    pub fn into_device(self) -> D {
        self.device
    }

    fn read_block(&mut self, block: u64, which: Buffer) -> Result<(), Error> {
        let sectors = self.block_size / SECTOR;
        let lba = block.checked_mul(sectors as u64).ok_or(Error::Corrupt)?;
        if block >= self.blocks {
            return Err(Error::Corrupt);
        }
        let size = self.block_size;
        let buffer = match which {
            Buffer::Gdt => &mut self.gdt,
            Buffer::Itable => &mut self.itable,
            Buffer::Node => &mut self.node,
            Buffer::Data => &mut self.data,
        };
        self.device.read(lba, &mut buffer[..size])
    }

    pub fn root(&mut self) -> Result<Inode, Error> {
        self.inode(ROOT)
    }

    /// Reads inode `number` (1-based, as in folder entries).
    pub fn inode(&mut self, number: u32) -> Result<Inode, Error> {
        if number == 0 || number > self.inodes {
            return Err(Error::Corrupt);
        }
        let index = u64::from(number - 1);
        let group = (index / u64::from(self.inodes_per_group)) as u32;
        if group >= self.groups {
            return Err(Error::Corrupt);
        }
        let byte = group as usize * self.desc_size;
        let gdt_block = u64::from(self.first_data_block) + 1 + (byte / self.block_size) as u64;
        if self.gdt_at != gdt_block {
            self.gdt_at = u64::MAX;
            self.read_block(gdt_block, Buffer::Gdt)?;
            self.gdt_at = gdt_block;
        }
        let at = byte % self.block_size;
        let mut table = u64::from(le32(&self.gdt, at + 8));
        if self.desc_size >= 64 {
            table |= u64::from(le32(&self.gdt, at + 0x28)) << 32;
        }
        let within = (index % u64::from(self.inodes_per_group)) as usize * self.inode_size;
        let block = table + (within / self.block_size) as u64;
        if self.itable_at != block {
            self.itable_at = u64::MAX;
            self.read_block(block, Buffer::Itable)?;
            self.itable_at = block;
        }
        let raw = &self.itable[within % self.block_size..within % self.block_size + self.inode_size];
        let mode = le16(raw, 0);
        let mut size = u64::from(le32(raw, 4));
        if mode & 0xf000 == 0x8000 || mode & 0xf000 == 0x4000 {
            size |= u64::from(le32(raw, 108)) << 32;
        }
        let mut map = [0u8; 60];
        map.copy_from_slice(&raw[40..100]);
        Ok(Inode { number, mode, size, flags: le32(raw, 32), links: le16(raw, 26), atime: le32(raw, 8), ctime: le32(raw, 12), mtime: le32(raw, 16), uid: u32::from(le16(raw, 2)) | u32::from(le16(raw, 120)) << 16, gid: u32::from(le16(raw, 24)) | u32::from(le16(raw, 122)) << 16, block: map })
    }

    /// Where logical block `logical` of the file is: the run it starts, so one call covers as much as is contiguous.
    pub fn map(&mut self, inode: &Inode, logical: u64) -> Result<Mapping, Error> {
        if inode.flags & FLAG_INLINE_DATA != 0 {
            return Err(Error::Unsupported);
        }
        if inode.flags & FLAG_EXTENTS == 0 {
            return self.map_indirect(inode, logical);
        }
        let mut first = true;
        let mut limit = u64::MAX; // where the next sibling of the node being read begins
        for _ in 0..8 {
            let step = {
                let bytes: &[u8] = if first { &inode.block } else { &self.node[..self.block_size] };
                Self::extent_step(bytes, logical, &mut limit)?
            };
            match step {
                Step::Done(found) => return Ok(found),
                Step::Descend(block) => {
                    self.read_block(block, Buffer::Node)?;
                    first = false;
                }
            }
        }
        Err(Error::Corrupt)
    }

    fn extent_step(bytes: &[u8], logical: u64, limit: &mut u64) -> Result<Step, Error> {
        if le16(bytes, 0) != 0xf30a {
            return Err(Error::Corrupt);
        }
        let entries = usize::from(le16(bytes, 2));
        let depth = le16(bytes, 6);
        if 12 + entries * 12 > bytes.len() {
            return Err(Error::Corrupt);
        }
        let rest = |to: u64| Mapping::Hole { blocks: to.saturating_sub(logical).max(1) };
        if depth == 0 {
            for index in 0..entries {
                let entry = &bytes[12 + index * 12..24 + index * 12];
                let start = u64::from(le32(entry, 0));
                let raw = u64::from(le16(entry, 4));
                let length = if raw > 32768 { raw - 32768 } else { raw };
                let uninitialised = raw > 32768;
                if logical < start {
                    return Ok(Step::Done(rest(start)));
                }
                if logical < start + length {
                    let skip = logical - start;
                    let physical = (u64::from(le16(entry, 6)) << 32 | u64::from(le32(entry, 8))) + skip;
                    let blocks = length - skip;
                    return Ok(Step::Done(if uninitialised { Mapping::Zeros { blocks } } else { Mapping::Data { block: physical, blocks } }));
                }
            }
            return Ok(Step::Done(rest(*limit)));
        }
        let mut chosen = None;
        for index in 0..entries {
            let entry = &bytes[12 + index * 12..24 + index * 12];
            let start = u64::from(le32(entry, 0));
            if start > logical {
                if chosen.is_none() {
                    return Ok(Step::Done(rest(start)));
                }
                *limit = (*limit).min(start);
                break;
            }
            chosen = Some(u64::from(le32(entry, 4)) | u64::from(le16(entry, 8)) << 32);
        }
        match chosen {
            Some(block) => Ok(Step::Descend(block)),
            None => Ok(Step::Done(rest(*limit))),
        }
    }

    /// The ext2/ext3 map: 12 direct pointers, then single, double and triple indirect blocks.
    fn map_indirect(&mut self, inode: &Inode, logical: u64) -> Result<Mapping, Error> {
        let per = (self.block_size / 4) as u64;
        let pointer = |bytes: &[u8], index: u64| u64::from(le32(bytes, index as usize * 4));
        let found = |block: u64| if block == 0 { Mapping::Hole { blocks: 1 } } else { Mapping::Data { block, blocks: 1 } };
        if logical < 12 {
            return Ok(found(pointer(&inode.block, logical)));
        }
        let mut rest = logical - 12;
        let (mut levels, root) = if rest < per {
            (1, pointer(&inode.block, 12))
        } else if { rest -= per; rest < per * per } {
            (2, pointer(&inode.block, 13))
        } else {
            rest -= per * per;
            if rest >= per * per * per {
                return Err(Error::Corrupt);
            }
            (3, pointer(&inode.block, 14))
        };
        let mut block = root;
        while levels > 0 {
            if block == 0 {
                return Ok(Mapping::Hole { blocks: 1 });
            }
            self.read_block(block, Buffer::Node)?;
            let span = per.pow(levels - 1);
            block = pointer(&self.node, rest / span);
            rest %= span;
            levels -= 1;
        }
        Ok(found(block))
    }

    /// Reads up to `out.len()` bytes of the file from `offset`; returns how many (fewer at the end of the file).
    pub fn read(&mut self, inode: &Inode, offset: u64, out: &mut [u8]) -> Result<usize, Error> {
        if offset >= inode.size {
            return Ok(0);
        }
        let want = (out.len() as u64).min(inode.size - offset) as usize;
        let size = self.block_size as u64;
        let mut done = 0;
        while done < want {
            let position = offset + done as u64;
            let skip = (position % size) as usize;
            let remaining = (want - done) as u64;
            match self.map(inode, position / size)? {
                Mapping::Hole { blocks } | Mapping::Zeros { blocks } => {
                    let span = blocks.saturating_mul(size).saturating_sub(skip as u64).min(remaining) as usize;
                    out[done..done + span].fill(0);
                    done += span;
                }
                Mapping::Data { block, blocks } => {
                    let span = blocks.saturating_mul(size).saturating_sub(skip as u64).min(remaining);
                    if skip == 0 && span >= size {
                        let whole = (span / size * size) as usize;
                        let lba = block.checked_mul(size / SECTOR as u64).ok_or(Error::Corrupt)?;
                        if block + whole as u64 / size > self.blocks {
                            return Err(Error::Corrupt);
                        }
                        self.device.read(lba, &mut out[done..done + whole])?;
                        done += whole;
                    } else {
                        self.read_block(block, Buffer::Data)?;
                        let part = (size - skip as u64).min(remaining) as usize;
                        out[done..done + part].copy_from_slice(&self.data[skip..skip + part]);
                        done += part;
                    }
                }
            }
        }
        Ok(want)
    }

    /// Calls `visit` for every entry of a folder (including `.` and `..`) until it returns true.
    pub fn list(&mut self, folder: &Inode, visit: &mut dyn FnMut(&DirEntry) -> bool) -> Result<(), Error> {
        self.list_from(folder, 0, &mut |entry, _| visit(entry))
    }

    /// Like `list`, starting at a position an earlier call handed to `visit` (0 for the start): the position after the entry
    /// just visited, so a walk that goes on where it stopped costs one pass instead of one per entry. A position is the
    /// folder block in the high 32 bits and the byte offset in it in the low 32.
    pub fn list_from(&mut self, folder: &Inode, from: u64, visit: &mut dyn FnMut(&DirEntry, u64) -> bool) -> Result<(), Error> {
        if !folder.is_dir() {
            return Err(Error::NotADirectory);
        }
        let size = self.block_size as u64;
        let mut logical = from >> 32;
        let mut skip = (from & 0xffff_ffff) as usize;
        while logical * size < folder.size {
            let (block, blocks) = match self.map(folder, logical)? {
                Mapping::Data { block, blocks } => (block, blocks),
                Mapping::Hole { blocks } | Mapping::Zeros { blocks } => {
                    logical += blocks;
                    skip = 0;
                    continue;
                }
            };
            for step in 0..blocks.min((folder.size.div_ceil(size)).saturating_sub(logical)) {
                self.read_block(block + step, Buffer::Data)?;
                let bytes = &self.data[..self.block_size];
                let mut at = core::mem::take(&mut skip);
                while at + 8 <= bytes.len() {
                    let record = usize::from(le16(bytes, at + 4));
                    let name_length = usize::from(bytes[at + 6]);
                    if record < 8 + name_length || record % 4 != 0 || at + record > bytes.len() {
                        return Err(Error::Corrupt);
                    }
                    let inode = le32(bytes, at);
                    let after = ((logical + step) << 32) | (at + record) as u64;
                    if inode != 0 && visit(&DirEntry { inode, kind: bytes[at + 7], name: &bytes[at + 8..at + 8 + name_length] }, after) {
                        return Ok(());
                    }
                    at += record;
                }
            }
            logical += blocks;
        }
        Ok(())
    }

    /// The entry called `name` in a folder (byte-exact, like Linux). An indexed folder is searched through its hash
    /// tree (one block per level plus one leaf); anything unexpected in the index falls back to the linear scan.
    pub fn lookup(&mut self, folder: &Inode, name: &[u8]) -> Result<Option<Inode>, Error> {
        let found = match self.indexed(folder, name) {
            Some(result) => result?,
            None => self.scan(folder, name)?,
        };
        match found {
            Some(number) => self.inode(number).map(Some),
            None => Ok(None),
        }
    }

    fn scan(&mut self, folder: &Inode, name: &[u8]) -> Result<Option<u32>, Error> {
        let mut found = None;
        self.list(folder, &mut |entry| {
            if entry.name == name {
                found = Some(entry.inode);
                return true;
            }
            false
        })?;
        Ok(found)
    }

    /// Looks `name` up in the leaf block `logical` of a folder.
    fn leaf(&mut self, folder: &Inode, logical: u64, name: &[u8]) -> Result<Option<u32>, Error> {
        let Mapping::Data { block, .. } = self.map(folder, logical)? else { return Err(Error::Corrupt) };
        self.read_block(block, Buffer::Data)?;
        let bytes = &self.data[..self.block_size];
        let mut at = 0;
        while at + 8 <= bytes.len() {
            let record = usize::from(le16(bytes, at + 4));
            let length = usize::from(bytes[at + 6]);
            if record < 8 + length || record % 4 != 0 || at + record > bytes.len() {
                return Err(Error::Corrupt);
            }
            if le32(bytes, at) != 0 && &bytes[at + 8..at + 8 + length] == name {
                return Ok(Some(le32(bytes, at)));
            }
            at += record;
        }
        Ok(None)
    }

    /// The hash-tree search: `None` when the folder is not indexed or the index cannot be trusted (the caller scans
    /// linearly), else the result. The root block holds `.`, `..` and the root of the tree (hash version, number of
    /// index levels, then entries of (hash, block)); an index node is a fake empty entry followed by entries. Entry 0
    /// of a node covers every hash below entry 1's; its hash field holds the node's limit and count instead.
    fn indexed(&mut self, folder: &Inode, name: &[u8]) -> Option<Result<Option<u32>, Error>> {
        if folder.flags & 0x1000 == 0 || !folder.is_dir() || folder.size < 2 * self.block_size as u64 {
            return None;
        }
        let Mapping::Data { block, .. } = self.map(folder, 0).ok()? else { return None };
        self.read_block(block, Buffer::Data).ok()?;
        let root = &self.data[..self.block_size];
        let (version, info_length, mut levels) = (root[28], root[29], root[30]);
        if le16(root, 4) != 12 || root[20..22] != *b".." || info_length != 8 || levels > 3 {
            return None;
        }
        // `.` and `..` live in the root block, outside every leaf the hash can lead to.
        if name == b"." {
            return Some(Ok(Some(le32(root, 0))));
        }
        if name == b".." {
            return Some(Ok(Some(le32(root, 12))));
        }
        let hash = dirhash::hash(version, self.hash_seed, name)?;
        let blocks = folder.size / self.block_size as u64;
        let mut offset = 32;
        loop {
            let bytes = &self.data[..self.block_size];
            let count = usize::from(le16(bytes, offset + 2));
            if count == 0 || offset + count * 8 > self.block_size {
                return None;
            }
            // The last entry whose hash is at most ours (entry 0 has no hash of its own).
            let mut chosen = 0;
            while chosen + 1 < count && le32(bytes, offset + (chosen + 1) * 8) <= hash {
                chosen += 1;
            }
            let next = u64::from(le32(bytes, offset + chosen * 8 + 4) & 0x0fff_ffff);
            if next == 0 || next >= blocks {
                return None;
            }
            if levels == 0 {
                // Names whose hashes collide continue in the following leaves, marked by the hash's low bit; the
                // leaves are noted first because reading one reuses the buffer that holds the node.
                let mut leaves = [next, 0, 0, 0];
                let mut noted = 1;
                let mut index = chosen + 1;
                while index < count && le32(bytes, offset + index * 8) == hash | 1 && noted < leaves.len() {
                    leaves[noted] = u64::from(le32(bytes, offset + index * 8 + 4) & 0x0fff_ffff);
                    noted += 1;
                    index += 1;
                }
                // A chain that runs on past the node or past what we noted is left to the linear scan.
                let open = index == count || le32(bytes, offset + index * 8) == hash | 1;
                for &leaf in &leaves[..noted] {
                    if leaf == 0 || leaf >= blocks {
                        return None;
                    }
                    match self.leaf(folder, leaf, name) {
                        Ok(None) => {}
                        other => return Some(other),
                    }
                }
                return if open { None } else { Some(Ok(None)) };
            }
            levels -= 1;
            let Ok(Mapping::Data { block, .. }) = self.map(folder, next) else { return None };
            self.read_block(block, Buffer::Data).ok()?;
            // A node: a fake entry (inode 0, record length = the block), then the count/limit header.
            let bytes = &self.data[..self.block_size];
            if le32(bytes, 0) != 0 || usize::from(le16(bytes, 4)) != self.block_size {
                return None;
            }
            offset = 8;
        }
    }

    /// Follows `a/b/c` from the root (empty parts are skipped; symbolic links are not followed).
    pub fn resolve(&mut self, path: &[u8]) -> Result<Option<Inode>, Error> {
        let mut current = self.root()?;
        for part in path.split(|&byte| byte == b'/').filter(|part| !part.is_empty()) {
            if !current.is_dir() {
                return Ok(None);
            }
            match self.lookup(&current, part)? {
                Some(next) => current = next,
                None => return Ok(None),
            }
        }
        Ok(Some(current))
    }
}

#[derive(Clone, Copy)]
enum Buffer {
    Gdt,
    Itable,
    Node,
    Data,
}
