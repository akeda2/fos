//! ext4 for fos, read-only: mount a volume Linux (or `mkfs.ext4`) made, walk its folders, read its files.
//!
//! Why: the data volume of a file server must be something any Linux machine can mount when the fos box dies
//! (docs/ext4.md). The driver is `no_std`, allocates nothing, and reaches the disk through `Device`, so the same
//! code runs on the embedded kernel, on the server kernel and in the host tests (images from `mkfs.ext4`).
//!
//! Handled: 1, 2 and 4 KiB blocks; 32- and 64-byte group descriptors (`64bit`), `flex_bg`; extent trees of any
//! depth and the old direct/indirect block maps (ext2/ext3 volumes); uninitialised extents (read as zeros) and
//! sparse files; folders by linear scan (an indexed folder stays readable that way; the hash-tree lookup is for the
//! performance work). Refused with a reason: a journal that needs recovery (`mount_with` can ignore it, then the
//! last transactions are missing), `meta_bg`, inline data, compression, encryption, case folding, other block sizes.
//! Checksums are not verified yet.
//!
//! Performance shape (for the server kernel): `map` returns whole contiguous runs and `read` hands every whole-block
//! run to the device in one call, straight into the caller's buffer, so a big file is a few large transfers. The
//! group-descriptor and inode-table blocks have a one-block cache each; a larger block cache belongs one layer up
//! where an allocator exists.
#![cfg_attr(not(test), no_std)]

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
    free_blocks: u64,
    inodes: u32,
    free_inodes: u32,
    inodes_per_group: u32,
    first_data_block: u32,
    inode_size: usize,
    desc_size: usize,
    groups: u32,
    incompat: u32,
    label: [u8; 16],
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
            free_blocks,
            inodes: le32(&sb, 0),
            free_inodes: le32(&sb, 16),
            inodes_per_group,
            first_data_block,
            inode_size,
            desc_size,
            groups,
            incompat,
            label,
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
        Ok(Inode { number, mode, size, flags: le32(raw, 32), links: le16(raw, 26), atime: le32(raw, 8), ctime: le32(raw, 12), mtime: le32(raw, 16), block: map })
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
        if !folder.is_dir() {
            return Err(Error::NotADirectory);
        }
        let size = self.block_size as u64;
        let mut logical = 0;
        while logical * size < folder.size {
            let (block, blocks) = match self.map(folder, logical)? {
                Mapping::Data { block, blocks } => (block, blocks),
                Mapping::Hole { blocks } | Mapping::Zeros { blocks } => {
                    logical += blocks;
                    continue;
                }
            };
            for step in 0..blocks.min((folder.size.div_ceil(size)).saturating_sub(logical)) {
                self.read_block(block + step, Buffer::Data)?;
                let bytes = &self.data[..self.block_size];
                let mut at = 0;
                while at + 8 <= bytes.len() {
                    let record = usize::from(le16(bytes, at + 4));
                    let name_length = usize::from(bytes[at + 6]);
                    if record < 8 + name_length || record % 4 != 0 || at + record > bytes.len() {
                        return Err(Error::Corrupt);
                    }
                    let inode = le32(bytes, at);
                    if inode != 0 && visit(&DirEntry { inode, kind: bytes[at + 7], name: &bytes[at + 8..at + 8 + name_length] }) {
                        return Ok(());
                    }
                    at += record;
                }
            }
            logical += blocks;
        }
        Ok(())
    }

    /// The entry called `name` in a folder (byte-exact, like Linux).
    pub fn lookup(&mut self, folder: &Inode, name: &[u8]) -> Result<Option<Inode>, Error> {
        let mut found = None;
        self.list(folder, &mut |entry| {
            if entry.name == name {
                found = Some(entry.inode);
                return true;
            }
            false
        })?;
        match found {
            Some(number) => self.inode(number).map(Some),
            None => Ok(None),
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
