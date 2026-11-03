//! ext4 writing (docs/ext4.md, step 3): format a volume, create / write / truncate / remove / rename files and folders.
//!
//! Scope, on purpose: this writer keeps a volume consistent *without a journal and without checksums*, so it only
//! accepts volumes whose features it can maintain: `filetype`, `extents`, `64bit`, `flex_bg`, `sparse_super`,
//! `large_file`, `huge_file`, `dir_nlink`, `extra_isize`, and (as inherited from Linux-made volumes) `dir_index`,
//! `resize_inode`, `ext_attr`, a *clean* `has_journal`, and `metadata_csum` (with `metadata_csum_seed` and an empty
//! `orphan_file`), which is what `mkfs.ext4` makes today. `uninit_bg` alone (a checksum of the descriptors only) is
//! refused with `ReadOnly`; `Writer::format` leaves checksums off.
//!
//! Crash behaviour (no journal): the superblock is marked "not clean" at the first change and "clean" again by `flush`.
//! Data goes to disk before the extents that name it, the directory entry goes last when creating and first when
//! removing, so a power cut leaves leaked or half-removed objects that `e2fsck` repairs, never a file that shows
//! somebody else's data.
//!
//! Shape: every operation takes inode *numbers* and re-reads what it needs (the inode table block and the descriptor
//! block are cached), so a caller never works with a stale copy. A file's extents are held in an in-memory list while
//! one operation edits them (`MAX_EXTENTS`: the inode's four slots, or four leaf blocks under it, which holds files
//! up to hundreds of GiB even when fragmented into 128 MiB extents); deeper trees are read but not written.
//! `Writer` is about 100 KiB: put it in a `Box` or a static, not on a small stack.

use crate::crc::crc32c;
use crate::{Buffer, Device, Error, Fs, Inode, Journaled, Mapping, Stage, FLAG_EXTENTS, FLAG_INLINE_DATA, INCOMPAT_64BIT, INCOMPAT_EXTENTS, INCOMPAT_FILETYPE, INCOMPAT_FLEX_BG, MAGIC, MAX_BLOCK, ROOT, SECTOR, le16, le32};
use core::ops::{Deref, DerefMut};

const LEAVES: usize = 4;
const MAX_EXTENTS: usize = LEAVES * ((MAX_BLOCK - 12) / 12);
const MAX_EXTENT_LEN: u32 = 32768;
const LOG: usize = 64;
const INDEX_FL: u32 = 0x1000;
const NAME_MAX: usize = 255;
const LINKS_MAX: u16 = 65000;

const COMPAT_OK: u32 = 0x8 | 0x4 | 0x10 | 0x20 | 0x1000; // ext_attr, has_journal, resize_inode, dir_index, orphan_file
const RO_COMPAT_OK: u32 = 0x1 | 0x2 | 0x8 | 0x20 | 0x40 | 0x400; // sparse_super, large_file, huge_file, dir_nlink, extra_isize, metadata_csum
const INCOMPAT_OK: u32 = INCOMPAT_FILETYPE | INCOMPAT_EXTENTS | INCOMPAT_64BIT | INCOMPAT_FLEX_BG | 0x2000; // + csum_seed
const RO_METADATA_CSUM: u32 = 0x400;
const BG_INODE_UNINIT: u16 = 1;
const BG_BLOCK_UNINIT: u16 = 2;
/// Bytes at the end of a folder block holding the checksum under metadata_csum (a fake empty entry, then the sum).
const DIR_TAIL: usize = 12;

fn put16(bytes: &mut [u8], at: usize, value: u16) {
    bytes[at..at + 2].copy_from_slice(&value.to_le_bytes());
}

fn put32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn bit(bitmap: &[u8], index: usize) -> bool {
    bitmap[index >> 3] >> (index & 7) & 1 != 0
}

fn set_bit(bitmap: &mut [u8], index: usize, on: bool) {
    if on {
        bitmap[index >> 3] |= 1 << (index & 7);
    } else {
        bitmap[index >> 3] &= !(1 << (index & 7));
    }
}

/// The file-type byte of a folder entry for a mode.
fn kind_of(mode: u16) -> u8 {
    match mode & 0xf000 {
        0x8000 => 1,
        0x4000 => 2,
        0x2000 => 3,
        0x6000 => 4,
        0x1000 => 5,
        0xc000 => 6,
        0xa000 => 7,
        _ => 0,
    }
}

fn put32_be(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_be_bytes());
}

fn valid_name(name: &[u8]) -> Result<(), Error> {
    if name.is_empty() || name.len() > NAME_MAX || name == b"." || name == b".." || name.iter().any(|&byte| byte == b'/' || byte == 0) {
        return Err(Error::InvalidName);
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct Ext {
    l: u32,
    len: u32,
    p: u64,
}

/// What `Writer::format` is told.
#[derive(Clone, Copy, Debug)]
pub struct FormatOptions<'a> {
    /// The volume label, up to 16 bytes.
    pub label: &'a [u8],
    /// One inode per this many bytes (0: 65536). A file server of big files wants more, many tiny files fewer.
    pub bytes_per_inode: u32,
    /// The volume's UUID (the caller has the entropy).
    pub uuid: [u8; 16],
    /// Seconds since 1970 for the time stamps.
    pub time: u32,
    /// Give the volume a jbd2 journal (what Linux expects of ext4, and what makes a power cut harmless).
    pub journal: bool,
    /// Make the volume with `metadata_csum` (what Linux's mkfs.ext4 does; every metadata block is checksummed).
    pub checksums: bool,
}

pub struct Writer<D: Device> {
    fs: Fs<D>,
    exts: [Ext; MAX_EXTENTS],
    count: usize,
    leaves: [u64; LEAVES],
    leaf_count: usize,
    bm: [u8; MAX_BLOCK],
    /// Runs allocated by the operation in progress, given back if it fails.
    log: [(u64, u32); LOG],
    log_len: usize,
    now: u32,
    dirty: bool,
    /// metadata_csum: the volume's checksum seed (zero and unused without it).
    csum: bool,
    seed: u32,
}

impl<D: Device> Deref for Writer<D> {
    type Target = Fs<D>;
    fn deref(&self) -> &Fs<D> {
        &self.fs
    }
}

impl<D: Device> DerefMut for Writer<D> {
    fn deref_mut(&mut self) -> &mut Fs<D> {
        &mut self.fs
    }
}

impl<D: Device> Writer<D> {
    /// Takes a mounted volume for writing; `ReadOnly` if it has features this writer cannot keep consistent.
    pub fn new(mut fs: Fs<D>) -> Result<Self, Error> {
        let (csum, seed) = Self::checksum_setup(&mut fs)?;
        if fs.compat & !COMPAT_OK != 0 || fs.incompat & !INCOMPAT_OK != 0 || fs.ro_compat & !RO_COMPAT_OK != 0 {
            return Err(Error::ReadOnly);
        }
        if fs.compat & 0x4 != 0 && !fs.device.journaled() {
            return Err(Error::ReadOnly);
        }
        if fs.block_size != 4096 && fs.block_size != 2048 && fs.block_size != 1024 {
            return Err(Error::ReadOnly);
        }
        Ok(Writer {
            fs,
            exts: [Ext { l: 0, len: 0, p: 0 }; MAX_EXTENTS],
            count: 0,
            leaves: [0; LEAVES],
            leaf_count: 0,
            bm: [0; MAX_BLOCK],
            log: [(0, 0); LOG],
            log_len: 0,
            now: 0,
            dirty: false,
            csum,
            seed,
        })
    }

    /// Whether the volume has metadata_csum, and its seed: crc32c of the UUID, or the superblock's fixed seed.
    fn checksum_setup(fs: &mut Fs<D>) -> Result<(bool, u32), Error> {
        if fs.ro_compat & RO_METADATA_CSUM == 0 {
            return Ok((false, 0));
        }
        let mut sb = [0u8; 1024];
        fs.device.read(2, &mut sb)?;
        let seed = if fs.incompat & 0x2000 != 0 { le32(&sb, 0x270) } else { crc32c(!0, &sb[104..120]) };
        Ok((true, seed))
    }

    /// Like `new`, but builds the writer inside `slot`, so its ~40 KiB never lands on a small stack.
    pub fn new_in(slot: &mut core::mem::MaybeUninit<Self>, mut fs: Fs<D>) -> Result<&mut Self, Error> {
        if fs.compat & !COMPAT_OK != 0 || fs.incompat & !INCOMPAT_OK != 0 || fs.ro_compat & !RO_COMPAT_OK != 0 || !matches!(fs.block_size, 1024 | 2048 | 4096) || (fs.compat & 0x4 != 0 && !fs.device.journaled()) {
            return Err(Error::ReadOnly);
        }
        let (csum, seed) = Self::checksum_setup(&mut fs)?;
        let p = slot.as_mut_ptr();
        // SAFETY: every field is written once; the arrays are plain integers, all-zero is a valid value.
        unsafe {
            core::ptr::addr_of_mut!((*p).fs).write(fs);
            core::ptr::write_bytes(core::ptr::addr_of_mut!((*p).exts), 0, 1);
            core::ptr::addr_of_mut!((*p).count).write(0);
            core::ptr::write_bytes(core::ptr::addr_of_mut!((*p).leaves), 0, 1);
            core::ptr::addr_of_mut!((*p).leaf_count).write(0);
            core::ptr::write_bytes(core::ptr::addr_of_mut!((*p).bm), 0, 1);
            core::ptr::write_bytes(core::ptr::addr_of_mut!((*p).log), 0, 1);
            core::ptr::addr_of_mut!((*p).log_len).write(0);
            core::ptr::addr_of_mut!((*p).now).write(0);
            core::ptr::addr_of_mut!((*p).dirty).write(false);
            core::ptr::addr_of_mut!((*p).csum).write(csum);
            core::ptr::addr_of_mut!((*p).seed).write(seed);
            Ok(&mut *p)
        }
    }

    /// The time stamped on what changes from now on (seconds since 1970).
    pub fn set_time(&mut self, seconds: u32) {
        self.now = seconds;
    }

    /// Marks the volume clean again: the free counts and the state in the superblock. Call it when a batch of changes
    /// is done; until then the superblock says "not clean" so that an interrupted session is checked by Linux.
    pub fn flush(&mut self) -> Result<(), Error> {
        if self.dirty {
            self.write_super(true)?;
            self.dirty = false;
        }
        self.fs.device.close()
    }

    pub fn into_device(mut self) -> Result<D, Error> {
        self.flush()?;
        Ok(self.fs.into_device())
    }

    fn begin(&mut self) -> Result<(), Error> {
        if self.fs.device.pending() >= 32 {
            self.fs.device.commit()?;
        }
        if !self.dirty {
            self.write_super(false)?;
            self.dirty = true;
        }
        Ok(())
    }

    fn write_super(&mut self, clean: bool) -> Result<(), Error> {
        let mut sb = [0u8; 1024];
        self.fs.device.read(2, &mut sb)?;
        put32(&mut sb, 12, self.fs.free_blocks as u32);
        if self.fs.incompat & INCOMPAT_64BIT != 0 {
            put32(&mut sb, 344, (self.fs.free_blocks >> 32) as u32);
        }
        put32(&mut sb, 16, self.fs.free_inodes);
        put32(&mut sb, 48, self.now);
        let state = le16(&sb, 58);
        put16(&mut sb, 58, if clean { state | 1 } else { state & !1 });
        if self.csum {
            let sum = crc32c(!0, &sb[..1020]);
            put32(&mut sb, 1020, sum);
        }
        self.fs.device.write(2, &sb)
    }

    // ---- raw blocks, descriptors, bitmaps ----

    fn lba(&self, block: u64) -> Result<u64, Error> {
        if block >= self.fs.blocks {
            return Err(Error::Corrupt);
        }
        Ok(block * (self.fs.block_size / SECTOR) as u64)
    }

    /// Writes the cached `Data` block of a file (not journaled: file data goes straight to the disk).
    fn write_file_block(&mut self, block: u64) -> Result<(), Error> {
        let lba = self.lba(block)?;
        let size = self.fs.block_size;
        self.fs.device.write_data(lba, &self.fs.data[..size])
    }

    fn write_buffer(&mut self, block: u64, which: Buffer) -> Result<(), Error> {
        let lba = self.lba(block)?;
        let size = self.fs.block_size;
        let fs = &mut self.fs;
        let buffer = match which {
            Buffer::Gdt => &fs.gdt,
            Buffer::Itable => &fs.itable,
            Buffer::Node => &fs.node,
            Buffer::Data => &fs.data,
        };
        fs.device.write(lba, &buffer[..size])
    }

    /// Makes the cached descriptor block the one holding `group`'s descriptor; returns its offset in the block.
    fn load_gdt(&mut self, group: u32) -> Result<usize, Error> {
        if group >= self.fs.groups {
            return Err(Error::Corrupt);
        }
        let byte = group as usize * self.fs.desc_size;
        let block = u64::from(self.fs.first_data_block) + 1 + (byte / self.fs.block_size) as u64;
        if self.fs.gdt_at != block {
            self.fs.gdt_at = u64::MAX;
            self.fs.read_block(block, Buffer::Gdt)?;
            self.fs.gdt_at = block;
        }
        Ok(byte % self.fs.block_size)
    }

    /// Writes the cached descriptor block back; `group`'s descriptor (in it) gets its checksum first.
    fn store_gdt(&mut self, group: u32) -> Result<(), Error> {
        if self.csum {
            let size = self.fs.desc_size;
            let at = (group as usize * size) % self.fs.block_size;
            put16(&mut self.fs.gdt, at + 0x1e, 0);
            let sum = crc32c(crc32c(self.seed, &group.to_le_bytes()), &self.fs.gdt[at..at + size]);
            put16(&mut self.fs.gdt, at + 0x1e, sum as u16);
        }
        let block = self.fs.gdt_at;
        self.write_buffer(block, Buffer::Gdt)
    }

    fn gdt_value(&self, at: usize, lo: usize, hi: usize) -> u32 {
        let low = u32::from(le16(&self.fs.gdt, at + lo));
        if self.fs.desc_size >= 64 { low | u32::from(le16(&self.fs.gdt, at + hi)) << 16 } else { low }
    }

    fn set_gdt_value(&mut self, at: usize, lo: usize, hi: usize, value: u32) {
        put16(&mut self.fs.gdt, at + lo, value as u16);
        if self.fs.desc_size >= 64 {
            put16(&mut self.fs.gdt, at + hi, (value >> 16) as u16);
        }
    }

    /// Adds to a group's free-block, free-inode and folder counts (and the superblock totals).
    fn adjust_group(&mut self, group: u32, blocks: i64, inodes: i64, dirs: i64) -> Result<(), Error> {
        let at = self.load_gdt(group)?;
        for (lo, hi, delta) in [(12, 0x2c, blocks), (14, 0x2e, inodes), (16, 0x30, dirs)] {
            if delta != 0 {
                let value = (i64::from(self.gdt_value(at, lo, hi)) + delta).clamp(0, i64::from(u32::MAX)) as u32;
                self.set_gdt_value(at, lo, hi, value);
            }
        }
        self.fs.free_blocks = (self.fs.free_blocks as i64 + blocks).max(0) as u64;
        self.fs.free_inodes = (i64::from(self.fs.free_inodes) + inodes).max(0) as u32;
        self.store_gdt(group)
    }

    fn bitmap_block(&mut self, group: u32, inode_map: bool) -> Result<u64, Error> {
        let at = self.load_gdt(group)?;
        let (lo, hi) = if inode_map { (4, 0x24) } else { (0, 0x20) };
        let mut block = u64::from(le32(&self.fs.gdt, at + lo));
        if self.fs.desc_size >= 64 {
            block |= u64::from(le32(&self.fs.gdt, at + hi)) << 32;
        }
        Ok(block)
    }

    /// Reads a group's bitmap into `bm`. Under metadata_csum a group flagged BLOCK_UNINIT / INODE_UNINIT has no valid
    /// bitmap on disk (Linux creates it on first use), so it is built here: all free for inodes, and for blocks only
    /// the group's own metadata (superblock and descriptor copies, and bitmaps and inode table if they lie inside).
    fn load_bitmap(&mut self, group: u32, inode_map: bool, block: u64) -> Result<(), Error> {
        let size = self.fs.block_size;
        if self.csum {
            let at = self.load_gdt(group)?;
            let flags = le16(&self.fs.gdt, at + 0x12);
            if inode_map && flags & BG_INODE_UNINIT != 0 {
                self.bm[..size].fill(0);
                return Ok(());
            }
            if !inode_map && flags & BG_BLOCK_UNINIT != 0 {
                return self.synthesize_block_bitmap(group);
            }
        }
        let lba = self.lba(block)?;
        self.fs.device.read(lba, &mut self.bm[..size])
    }

    fn synthesize_block_bitmap(&mut self, group: u32) -> Result<(), Error> {
        let size = self.fs.block_size;
        self.bm[..size].fill(0);
        let start = u64::from(self.fs.first_data_block) + u64::from(group) * u64::from(self.fs.blocks_per_group);
        let total = self.group_blocks(group);
        let backup = group == 0 || self.fs.ro_compat & 1 == 0 || is_sparse_group(group);
        if backup {
            let mut sb = [0u8; 1024];
            self.fs.device.read(2, &mut sb)?;
            let table = (self.fs.groups as usize * self.fs.desc_size).div_ceil(size);
            for index in 0..(1 + table + usize::from(le16(&sb, 0xce))).min(total) {
                set_bit(&mut self.bm, index, true);
            }
        }
        let at = self.load_gdt(group)?;
        let mut itable = u64::from(le32(&self.fs.gdt, at + 8));
        let (mut block_map, mut inode_map) = (u64::from(le32(&self.fs.gdt, at)), u64::from(le32(&self.fs.gdt, at + 4)));
        if self.fs.desc_size >= 64 {
            itable |= u64::from(le32(&self.fs.gdt, at + 0x28)) << 32;
            block_map |= u64::from(le32(&self.fs.gdt, at + 0x20)) << 32;
            inode_map |= u64::from(le32(&self.fs.gdt, at + 0x24)) << 32;
        }
        let itable_blocks = (self.fs.inodes_per_group as usize * self.fs.inode_size).div_ceil(size) as u64;
        let mut mark = |block: u64| {
            if block >= start && block < start + total as u64 {
                set_bit(&mut self.bm, (block - start) as usize, true);
            }
        };
        mark(block_map);
        mark(inode_map);
        for block in itable..itable + itable_blocks {
            mark(block);
        }
        for index in total..size * 8 {
            set_bit(&mut self.bm, index, true);
        }
        Ok(())
    }

    /// Writes a group's bitmap. Under metadata_csum the checksum (and "initialised" flag) goes into the cached
    /// descriptor, which the caller stores right after (`adjust_group`).
    fn store_bitmap(&mut self, group: u32, inode_map: bool, block: u64) -> Result<(), Error> {
        let lba = self.lba(block)?;
        let size = self.fs.block_size;
        self.fs.device.write(lba, &self.bm[..size])?;
        if self.csum {
            let bytes = if inode_map { self.fs.inodes_per_group } else { self.fs.blocks_per_group } as usize / 8;
            let sum = crc32c(self.seed, &self.bm[..bytes]);
            let at = self.load_gdt(group)?;
            let (lo, hi) = if inode_map { (0x1a, 0x3a) } else { (0x18, 0x38) };
            put16(&mut self.fs.gdt, at + lo, sum as u16);
            if self.fs.desc_size >= 64 {
                put16(&mut self.fs.gdt, at + hi, (sum >> 16) as u16);
            }
            let flags = le16(&self.fs.gdt, at + 0x12);
            put16(&mut self.fs.gdt, at + 0x12, flags & !if inode_map { BG_INODE_UNINIT } else { BG_BLOCK_UNINIT });
        }
        Ok(())
    }

    fn group_blocks(&self, group: u32) -> usize {
        let start = u64::from(self.fs.first_data_block) + u64::from(group) * u64::from(self.fs.blocks_per_group);
        (self.fs.blocks - start).min(u64::from(self.fs.blocks_per_group)) as usize
    }

    /// Takes up to `max` contiguous free blocks, the first free run at or after `goal` (wrapping round the groups);
    /// returns (first block, how many).
    fn alloc_run(&mut self, goal: u64, max: u32) -> Result<(u64, u32), Error> {
        let first = u64::from(self.fs.first_data_block);
        let per_group = u64::from(self.fs.blocks_per_group);
        let groups = self.fs.groups;
        let goal = goal.clamp(first, self.fs.blocks - 1);
        let start_group = ((goal - first) / per_group) as u32;
        for step in 0..=groups {
            let group = (start_group + step) % groups;
            let at = self.load_gdt(group)?;
            if self.gdt_value(at, 12, 0x2c) == 0 {
                continue;
            }
            let from = if step == 0 { ((goal - first) % per_group) as usize } else { 0 };
            let bitmap = self.bitmap_block(group, false)?;
            self.load_bitmap(group, false, bitmap)?;
            let total = self.group_blocks(group);
            let Some(begin) = (from..total).find(|&index| !bit(&self.bm, index)) else { continue };
            let mut end = begin;
            while end < total && (end - begin) < max as usize && !bit(&self.bm, end) {
                set_bit(&mut self.bm, end, true);
                end += 1;
            }
            self.store_bitmap(group, false, bitmap)?;
            self.adjust_group(group, -((end - begin) as i64), 0, 0)?;
            return Ok((first + u64::from(group) * per_group + begin as u64, (end - begin) as u32));
        }
        Err(Error::NoSpace)
    }

    fn free_run(&mut self, start: u64, count: u32) -> Result<(), Error> {
        let first = u64::from(self.fs.first_data_block);
        let per_group = u64::from(self.fs.blocks_per_group);
        let (mut block, mut left) = (start, u64::from(count));
        if start < first || start + left > self.fs.blocks {
            return Err(Error::Corrupt);
        }
        while left > 0 {
            let group = ((block - first) / per_group) as u32;
            let offset = ((block - first) % per_group) as usize;
            let take = left.min(per_group - offset as u64) as usize;
            let bitmap = self.bitmap_block(group, false)?;
            self.load_bitmap(group, false, bitmap)?;
            let mut freed = 0;
            for index in offset..offset + take {
                if bit(&self.bm, index) {
                    set_bit(&mut self.bm, index, false);
                    freed += 1;
                }
            }
            self.store_bitmap(group, false, bitmap)?;
            self.adjust_group(group, freed, 0, 0)?;
            block += take as u64;
            left -= take as u64;
        }
        Ok(())
    }

    fn alloc_inode(&mut self, near_group: u32, is_dir: bool) -> Result<u32, Error> {
        let per_group = self.fs.inodes_per_group;
        for step in 0..self.fs.groups {
            let group = (near_group + step) % self.fs.groups;
            let at = self.load_gdt(group)?;
            if self.gdt_value(at, 14, 0x2e) == 0 {
                continue;
            }
            let bitmap = self.bitmap_block(group, true)?;
            self.load_bitmap(group, true, bitmap)?;
            let Some(index) = (0..per_group as usize).find(|&index| !bit(&self.bm, index)) else { continue };
            set_bit(&mut self.bm, index, true);
            self.store_bitmap(group, true, bitmap)?;
            if self.csum {
                // Slots past the highest used one are "unused" (Linux skips them when scanning).
                let at = self.load_gdt(group)?;
                let unused = self.gdt_value(at, 0x1c, 0x32).min(per_group - (index as u32 + 1));
                self.set_gdt_value(at, 0x1c, 0x32, unused);
            }
            self.adjust_group(group, 0, -1, i64::from(is_dir))?;
            return Ok(group * per_group + index as u32 + 1);
        }
        Err(Error::NoSpace)
    }

    fn free_inode(&mut self, number: u32, was_dir: bool) -> Result<(), Error> {
        let per_group = self.fs.inodes_per_group;
        let group = (number - 1) / per_group;
        let bitmap = self.bitmap_block(group, true)?;
        self.load_bitmap(group, true, bitmap)?;
        set_bit(&mut self.bm, ((number - 1) % per_group) as usize, false);
        self.store_bitmap(group, true, bitmap)?;
        self.adjust_group(group, 0, 1, -i64::from(was_dir))
    }

    // ---- inodes ----

    fn inode_location(&mut self, number: u32) -> Result<(u64, usize), Error> {
        if number == 0 || number > self.fs.inodes {
            return Err(Error::Corrupt);
        }
        let index = u64::from(number - 1);
        let group = (index / u64::from(self.fs.inodes_per_group)) as u32;
        let at = self.load_gdt(group)?;
        let mut table = u64::from(le32(&self.fs.gdt, at + 8));
        if self.fs.desc_size >= 64 {
            table |= u64::from(le32(&self.fs.gdt, at + 0x28)) << 32;
        }
        let within = (index % u64::from(self.fs.inodes_per_group)) as usize * self.fs.inode_size;
        Ok((table + (within / self.fs.block_size) as u64, within % self.fs.block_size))
    }

    /// Read-modify-write of one inode record.
    fn update_inode(&mut self, number: u32, change: impl FnOnce(&mut [u8])) -> Result<(), Error> {
        let (block, offset) = self.inode_location(number)?;
        self.fs.itable_at = u64::MAX;
        self.fs.read_block(block, Buffer::Itable)?;
        let size = self.fs.inode_size;
        change(&mut self.fs.itable[offset..offset + size]);
        if self.csum {
            let seed = self.inode_seed_of(number, &self.fs.itable[offset..offset + size]);
            let raw = &mut self.fs.itable[offset..offset + size];
            let wide = size > 128 && le16(raw, 128) >= 4;
            put16(raw, 0x7c, 0);
            if wide {
                put16(raw, 0x82, 0);
            }
            let sum = crc32c(seed, raw);
            put16(raw, 0x7c, sum as u16);
            if wide {
                put16(raw, 0x82, (sum >> 16) as u16);
            }
        }
        self.write_buffer(block, Buffer::Itable)?;
        self.fs.itable_at = block;
        Ok(())
    }

    /// The checksum seed of one inode (also the seed of its extent blocks and folder blocks): the volume seed, the
    /// inode number and the inode's generation.
    fn inode_seed_of(&self, number: u32, raw: &[u8]) -> u32 {
        crc32c(crc32c(self.seed, &number.to_le_bytes()), &raw[0x64..0x68])
    }

    fn inode_seed(&mut self, number: u32) -> Result<u32, Error> {
        let generation = self.read_raw(number, 0x64)?;
        Ok(crc32c(crc32c(self.seed, &number.to_le_bytes()), &generation.to_le_bytes()))
    }

    fn read_raw(&mut self, number: u32, at: usize) -> Result<u32, Error> {
        let (block, offset) = self.inode_location(number)?;
        if self.fs.itable_at != block {
            self.fs.itable_at = u64::MAX;
            self.fs.read_block(block, Buffer::Itable)?;
            self.fs.itable_at = block;
        }
        Ok(le32(&self.fs.itable, offset + at))
    }

    fn init_inode(&mut self, number: u32, mode: u16, links: u16, size: u64, root: &[u8; 60], blocks: u64) -> Result<(), Error> {
        let (now, inode_size) = (self.now, self.fs.inode_size);
        self.update_inode(number, |raw| {
            raw.fill(0);
            put16(raw, 0, mode);
            put32(raw, 4, size as u32);
            for at in [8, 12, 16] {
                put32(raw, at, now);
            }
            put16(raw, 26, links);
            put32(raw, 28, blocks as u32);
            put32(raw, 32, FLAG_EXTENTS);
            raw[40..100].copy_from_slice(root);
            put32(raw, 108, (size >> 32) as u32);
            put16(raw, 116, (blocks >> 32) as u16);
            if inode_size > 128 {
                put16(raw, 128, 32.min(inode_size - 128) as u16);
                if inode_size >= 152 {
                    put32(raw, 144, now);
                }
            }
        })
    }

    fn touch(&mut self, number: u32, flags_clear: u32) -> Result<(), Error> {
        let now = self.now;
        self.update_inode(number, |raw| {
            put32(raw, 12, now);
            put32(raw, 16, now);
            let flags = le32(raw, 32);
            put32(raw, 32, flags & !flags_clear);
        })
    }

    fn add_links(&mut self, number: u32, delta: i32) -> Result<(), Error> {
        self.update_inode(number, |raw| {
            let links = i32::from(le16(raw, 26)) + delta;
            put16(raw, 26, links.clamp(0, i32::from(u16::MAX)) as u16);
        })
    }

    fn empty_root() -> [u8; 60] {
        let mut root = [0u8; 60];
        put16(&mut root, 0, 0xf30a);
        put16(&mut root, 4, 4);
        root
    }

    // ---- extents ----

    fn max_extents(&self) -> usize {
        LEAVES * ((self.fs.block_size - 12) / 12)
    }

    fn parse_leaf(bytes: &[u8], exts: &mut [Ext], count: &mut usize, max: usize) -> Result<(), Error> {
        if le16(bytes, 0) != 0xf30a || le16(bytes, 6) != 0 {
            return Err(Error::Corrupt);
        }
        let entries = usize::from(le16(bytes, 2));
        if 12 + entries * 12 > bytes.len() || *count + entries > max {
            return Err(Error::TooFragmented);
        }
        for index in 0..entries {
            let entry = &bytes[12 + index * 12..24 + index * 12];
            let raw = u32::from(le16(entry, 4));
            if raw > 32768 {
                return Err(Error::Unsupported);
            }
            let ext = Ext { l: le32(entry, 0), len: raw, p: u64::from(le16(entry, 6)) << 32 | u64::from(le32(entry, 8)) };
            if ext.len == 0 || (*count > 0 && u64::from(exts[*count - 1].l) + u64::from(exts[*count - 1].len) > u64::from(ext.l)) {
                return Err(Error::Corrupt);
            }
            exts[*count] = ext;
            *count += 1;
        }
        Ok(())
    }

    fn load_extents(&mut self, inode: &Inode) -> Result<(), Error> {
        if inode.flags & FLAG_EXTENTS == 0 || inode.flags & FLAG_INLINE_DATA != 0 {
            return Err(Error::Unsupported);
        }
        let max = self.max_extents();
        self.count = 0;
        self.leaf_count = 0;
        let root = inode.block;
        if le16(&root, 0) != 0xf30a {
            return Err(Error::Corrupt);
        }
        match le16(&root, 6) {
            0 => Self::parse_leaf(&root, &mut self.exts, &mut self.count, 4)?,
            1 => {
                let entries = usize::from(le16(&root, 2));
                if entries > LEAVES {
                    return Err(Error::Corrupt);
                }
                for index in 0..entries {
                    let entry = &root[12 + index * 12..24 + index * 12];
                    let leaf = u64::from(le32(entry, 4)) | u64::from(le16(entry, 8)) << 32;
                    self.fs.read_block(leaf, Buffer::Node)?;
                    let size = self.fs.block_size;
                    Self::parse_leaf(&self.fs.node[..size], &mut self.exts, &mut self.count, max)?;
                    self.leaves[index] = leaf;
                    self.leaf_count = index + 1;
                }
            }
            _ => return Err(Error::Unsupported),
        }
        Ok(())
    }

    fn ext_find(&self, logical: u64) -> Option<usize> {
        self.exts[..self.count].iter().position(|ext| logical >= u64::from(ext.l) && logical < u64::from(ext.l) + u64::from(ext.len))
    }

    fn ext_remove(&mut self, index: usize) {
        self.exts.copy_within(index + 1..self.count, index);
        self.count -= 1;
    }

    /// Adds a mapped run, merging it with a neighbour it continues.
    fn ext_insert(&mut self, l: u32, len: u32, p: u64) -> Result<(), Error> {
        let index = self.exts[..self.count].partition_point(|ext| ext.l <= l);
        if index > 0 {
            let before = self.exts[index - 1];
            if before.l + before.len == l && before.p + u64::from(before.len) == p && before.len + len <= MAX_EXTENT_LEN {
                self.exts[index - 1].len += len;
                if index < self.count {
                    let after = self.exts[index];
                    let joined = self.exts[index - 1];
                    if joined.l + joined.len == after.l && joined.p + u64::from(joined.len) == after.p && joined.len + after.len <= MAX_EXTENT_LEN {
                        self.exts[index - 1].len += after.len;
                        self.ext_remove(index);
                    }
                }
                return Ok(());
            }
        }
        if index < self.count {
            let after = self.exts[index];
            if l + len == after.l && p + u64::from(len) == after.p && len + after.len <= MAX_EXTENT_LEN {
                self.exts[index] = Ext { l, len: len + after.len, p };
                return Ok(());
            }
        }
        if self.count >= self.max_extents() {
            return Err(Error::TooFragmented);
        }
        self.exts.copy_within(index..self.count, index + 1);
        self.exts[index] = Ext { l, len, p };
        self.count += 1;
        Ok(())
    }

    /// Writes `exts[..count]` into the inode (and leaf blocks), sets its block count, and gives back leaf blocks that
    /// are no longer needed.
    fn store_extents(&mut self, number: u32) -> Result<(), Error> {
        let capacity = (self.fs.block_size - 12) / 12;
        let count = self.count;
        let data_blocks: u64 = self.exts[..count].iter().map(|ext| u64::from(ext.len)).sum();
        let needed = if count <= 4 { 0 } else { count.div_ceil(capacity) };
        if needed > LEAVES {
            return Err(Error::TooFragmented);
        }
        let old_leaves = self.leaf_count;
        let mut root = Self::empty_root();
        if needed == 0 {
            put16(&mut root, 2, count as u16);
            for (index, ext) in self.exts[..count].iter().enumerate() {
                let at = 12 + index * 12;
                put32(&mut root, at, ext.l);
                put16(&mut root, at + 4, ext.len as u16);
                put16(&mut root, at + 6, (ext.p >> 32) as u16);
                put32(&mut root, at + 8, ext.p as u32);
            }
        } else {
            for index in self.leaf_count..needed {
                let (block, _) = self.alloc_run(self.exts[0].p, 1)?;
                self.leaves[index] = block;
                self.leaf_count = index + 1;
            }
            put16(&mut root, 2, needed as u16);
            put16(&mut root, 6, 1);
            for leaf in 0..needed {
                let (from, to) = (leaf * capacity, count.min((leaf + 1) * capacity));
                let size = self.fs.block_size;
                self.fs.node[..size].fill(0);
                put16(&mut self.fs.node, 0, 0xf30a);
                put16(&mut self.fs.node, 2, (to - from) as u16);
                put16(&mut self.fs.node, 4, capacity as u16);
                for (index, ext) in self.exts[from..to].iter().enumerate() {
                    let at = 12 + index * 12;
                    put32(&mut self.fs.node, at, ext.l);
                    put16(&mut self.fs.node, at + 4, ext.len as u16);
                    put16(&mut self.fs.node, at + 6, (ext.p >> 32) as u16);
                    put32(&mut self.fs.node, at + 8, ext.p as u32);
                }
                let block = self.leaves[leaf];
                if self.csum {
                    let tail = 12 + capacity * 12;
                    let sum = crc32c(self.inode_seed(number)?, &self.fs.node[..tail]);
                    put32(&mut self.fs.node, tail, sum);
                }
                self.write_buffer(block, Buffer::Node)?;
                let at = 12 + leaf * 12;
                put32(&mut root, at, self.exts[from].l);
                put32(&mut root, at + 4, block as u32);
                put16(&mut root, at + 8, (block >> 32) as u16);
            }
        }
        let sectors = (data_blocks + needed as u64) * (self.fs.block_size / SECTOR) as u64;
        if sectors > u64::from(u32::MAX) && self.fs.ro_compat & 0x8 == 0 {
            return Err(Error::Invalid);
        }
        self.update_inode(number, |raw| {
            raw[40..100].copy_from_slice(&root);
            put32(raw, 28, sectors as u32);
            put16(raw, 116, (sectors >> 32) as u16);
        })?;
        for index in needed..old_leaves {
            let block = self.leaves[index];
            self.free_run(block, 1)?;
        }
        self.leaf_count = needed;
        Ok(())
    }

    /// Maps every unmapped block of `first..=last`, allocating runs next to the file's earlier blocks.
    fn fill_holes(&mut self, number: u32, first: u64, last: u64) -> Result<(), Error> {
        let home = u64::from(self.fs.first_data_block) + u64::from((number - 1) / self.fs.inodes_per_group) * u64::from(self.fs.blocks_per_group);
        let mut at = first;
        while at <= last {
            if let Some(index) = self.ext_find(at) {
                at = u64::from(self.exts[index].l) + u64::from(self.exts[index].len);
                continue;
            }
            let next = self.exts[..self.count].iter().map(|ext| u64::from(ext.l)).find(|&l| l > at).unwrap_or(u64::MAX);
            let hole_end = (last + 1).min(next);
            while at < hole_end {
                let goal = self.exts[..self.count].iter().rev().find(|ext| u64::from(ext.l) < at).map_or(home, |ext| ext.p + u64::from(ext.len));
                if self.log_len == LOG {
                    return Err(Error::TooFragmented);
                }
                let (block, got) = self.alloc_run(goal, (hole_end - at).min(u64::from(MAX_EXTENT_LEN)) as u32)?;
                self.log[self.log_len] = (block, got);
                self.log_len += 1;
                self.ext_insert(at as u32, got, block)?;
                at += u64::from(got);
            }
        }
        Ok(())
    }

    /// Undoes the allocations of a failed operation (nothing was stored yet).
    fn rollback(&mut self, inode: &Inode) {
        for index in 0..self.log_len {
            let (block, count) = self.log[index];
            let _ = self.free_run(block, count);
        }
        self.log_len = 0;
        let _ = self.load_extents(inode);
    }

    fn physical(&self, logical: u64) -> Option<(u64, u64)> {
        let ext = self.exts[self.ext_find(logical)?];
        let skip = logical - u64::from(ext.l);
        Some((ext.p + skip, u64::from(ext.len) - skip))
    }

    // ---- files ----

    fn checked(&mut self, number: u32) -> Result<Inode, Error> {
        self.fs.inode(number)
    }

    /// Writes `data` at `offset` of the file, allocating blocks (a gap before `offset` stays a hole) and extending its
    /// size. Returns the bytes written.
    pub fn write(&mut self, number: u32, offset: u64, data: &[u8]) -> Result<usize, Error> {
        let inode = self.checked(number)?;
        if inode.is_dir() {
            return Err(Error::IsADirectory);
        }
        if !inode.is_file() {
            return Err(Error::Invalid);
        }
        if data.is_empty() {
            return Ok(0);
        }
        self.begin()?;
        self.load_extents(&inode)?;
        let size = self.fs.block_size as u64;
        let end = offset + data.len() as u64;
        let (first, last) = (offset / size, (end - 1) / size);
        if last >= u64::from(u32::MAX) {
            return Err(Error::Invalid);
        }
        let fresh_first = self.ext_find(first).is_none();
        let fresh_last = self.ext_find(last).is_none();
        self.log_len = 0;
        let result = self.fill_holes(number, first, last).and_then(|()| self.put_data(offset, data, first, last, fresh_first, fresh_last));
        if let Err(error) = result {
            self.rollback(&inode);
            return Err(error);
        }
        if let Err(error) = self.store_extents(number) {
            self.rollback(&inode);
            return Err(error);
        }
        self.log_len = 0;
        let new_size = inode.size.max(end);
        let now = self.now;
        self.update_inode(number, |raw| {
            put32(raw, 4, new_size as u32);
            put32(raw, 108, (new_size >> 32) as u32);
            put32(raw, 12, now);
            put32(raw, 16, now);
        })?;
        Ok(data.len())
    }

    fn put_data(&mut self, offset: u64, data: &[u8], first: u64, last: u64, fresh_first: bool, fresh_last: bool) -> Result<(), Error> {
        let size = self.fs.block_size as u64;
        let mut done = 0usize;
        while done < data.len() {
            let position = offset + done as u64;
            let logical = position / size;
            let skip = (position % size) as usize;
            let remaining = data.len() - done;
            let (block, run) = self.physical(logical).ok_or(Error::Corrupt)?;
            if skip == 0 && remaining as u64 >= size {
                let whole = run.min(remaining as u64 / size);
                let lba = self.lba(block)?;
                if block + whole > self.fs.blocks {
                    return Err(Error::Corrupt);
                }
                self.fs.device.write_data(lba, &data[done..done + (whole * size) as usize])?;
                done += (whole * size) as usize;
            } else {
                let fresh = (logical == first && fresh_first) || (logical == last && fresh_last);
                if fresh {
                    self.fs.data[..size as usize].fill(0);
                } else {
                    self.fs.read_block(block, Buffer::Data)?;
                }
                let part = (size as usize - skip).min(remaining);
                self.fs.data[skip..skip + part].copy_from_slice(&data[done..done + part]);
                self.write_file_block(block)?;
                done += part;
            }
        }
        Ok(())
    }

    /// Sets the file's size. Larger: the new part is a hole. Smaller: blocks past it are freed.
    pub fn truncate(&mut self, number: u32, new_size: u64) -> Result<(), Error> {
        let inode = self.checked(number)?;
        if !inode.is_file() {
            return Err(Error::Invalid);
        }
        self.begin()?;
        let now = self.now;
        if new_size >= inode.size {
            return self.update_inode(number, |raw| {
                put32(raw, 4, new_size as u32);
                put32(raw, 108, (new_size >> 32) as u32);
                put32(raw, 12, now);
                put32(raw, 16, now);
            });
        }
        self.load_extents(&inode)?;
        let size = self.fs.block_size as u64;
        if new_size % size != 0 {
            let logical = new_size / size;
            if let Some((block, _)) = self.physical(logical) {
                self.fs.read_block(block, Buffer::Data)?;
                self.fs.data[(new_size % size) as usize..size as usize].fill(0);
                self.write_file_block(block)?;
            }
        }
        let cut = new_size.div_ceil(size);
        let old_count = self.count;
        let mut straddler = None;
        let mut keep = 0;
        while keep < old_count && u64::from(self.exts[keep].l) < cut {
            keep += 1;
        }
        if keep > 0 {
            let last = self.exts[keep - 1];
            if u64::from(last.l) + u64::from(last.len) > cut {
                let kept = (cut - u64::from(last.l)) as u32;
                straddler = Some((last.p + u64::from(kept), last.len - kept));
                self.exts[keep - 1].len = kept;
            }
        }
        self.count = keep;
        let tail_start = keep;
        // The inode names fewer blocks first, then the blocks are given back.
        self.store_extents(number)?;
        self.update_inode(number, |raw| {
            put32(raw, 4, new_size as u32);
            put32(raw, 108, (new_size >> 32) as u32);
            put32(raw, 12, now);
            put32(raw, 16, now);
        })?;
        if let Some((block, count)) = straddler {
            self.free_run(block, count)?;
        }
        for index in tail_start..old_count {
            let ext = self.exts[index];
            self.free_run(ext.p, ext.len)?;
        }
        self.count = keep;
        Ok(())
    }

    // ---- folders ----

    fn dir_block(&mut self, dir: &Inode, logical: u64) -> Result<u64, Error> {
        match self.fs.map(dir, logical)? {
            Mapping::Data { block, .. } => Ok(block),
            _ => Err(Error::Corrupt),
        }
    }

    /// Folder blocks end in a checksum under metadata_csum; entries live in the first `size - DIR_TAIL` bytes.
    fn dir_limit(&self) -> usize {
        self.fs.block_size - if self.csum { DIR_TAIL } else { 0 }
    }

    /// Under metadata_csum a hash-tree folder has checksummed index blocks we do not maintain, so before its first
    /// change it becomes a linear folder: the root block's `..` entry takes back the index space, index nodes become
    /// empty folder blocks (leaves are plain folder blocks already), and the index flag goes.
    fn dir_unindex(&mut self, number: u32, dir: &Inode) -> Result<(), Error> {
        if !self.csum || dir.flags & INDEX_FL == 0 {
            return Ok(());
        }
        let size = self.fs.block_size;
        let limit = self.dir_limit();
        for logical in 0..dir.size / size as u64 {
            let block = self.dir_block(dir, logical)?;
            self.fs.read_block(block, Buffer::Data)?;
            if logical == 0 {
                if le16(&self.fs.data, 4) != 12 || self.fs.data[16 + 2] != 2 || &self.fs.data[20..22] != b".." {
                    return Err(Error::Corrupt);
                }
                put16(&mut self.fs.data, 16, (limit - 12) as u16);
                self.fs.data[24..size].fill(0);
            } else if le32(&self.fs.data, 0) == 0 && usize::from(le16(&self.fs.data, 4)) == size {
                self.fs.data[..size].fill(0);
                put16(&mut self.fs.data, 4, limit as u16);
            } else {
                continue;
            }
            self.dir_seal(number)?;
            self.write_buffer(block, Buffer::Data)?;
        }
        self.update_inode(number, |raw| {
            let flags = le32(raw, 32);
            put32(raw, 32, flags & !INDEX_FL);
        })
    }

    /// Writes the end-of-block checksum entry of the folder block in `fs.data`.
    fn dir_seal(&mut self, dir_number: u32) -> Result<(), Error> {
        if !self.csum {
            return Ok(());
        }
        let size = self.fs.block_size;
        let seed = self.inode_seed(dir_number)?;
        let sum = crc32c(seed, &self.fs.data[..size - DIR_TAIL]);
        let tail = &mut self.fs.data[size - DIR_TAIL..size];
        tail.fill(0);
        put16(tail, 4, DIR_TAIL as u16);
        tail[7] = 0xde;
        put32(tail, 8, sum);
        Ok(())
    }

    /// Adds `name -> number` to a folder, in free space of an existing block or in a new one.
    fn dir_add(&mut self, dir_number: u32, name: &[u8], number: u32, kind: u8) -> Result<(), Error> {
        let dir = self.checked(dir_number)?;
        self.dir_unindex(dir_number, &dir)?;
        let size = self.fs.block_size;
        let limit = self.dir_limit();
        let need = 8 + ((name.len() + 3) & !3);
        let place = |data: &mut [u8], at: usize| {
            put32(data, at, number);
            data[at + 6] = name.len() as u8;
            data[at + 7] = kind;
            data[at + 8..at + 8 + name.len()].copy_from_slice(name);
        };
        for logical in 0..dir.size / size as u64 {
            let block = self.dir_block(&dir, logical)?;
            self.fs.read_block(block, Buffer::Data)?;
            let mut at = 0;
            while at < limit {
                let record = usize::from(le16(&self.fs.data, at + 4));
                if record < 8 || record % 4 != 0 || at + record > limit {
                    return Err(Error::Corrupt);
                }
                let used = if le32(&self.fs.data, at) == 0 { 0 } else { 8 + ((usize::from(self.fs.data[at + 6]) + 3) & !3) };
                if record - used >= need {
                    let at_new = at + used;
                    if used > 0 {
                        put16(&mut self.fs.data, at + 4, used as u16);
                    }
                    put16(&mut self.fs.data, at_new + 4, (record - used) as u16);
                    place(&mut self.fs.data, at_new);
                    self.dir_seal(dir_number)?;
                    self.write_buffer(block, Buffer::Data)?;
                    return self.touch(dir_number, INDEX_FL);
                }
                at += record;
            }
        }
        // No room: the folder grows by one block.
        self.load_extents(&dir)?;
        let logical = dir.size / size as u64;
        let goal = self.exts[..self.count].last().map_or(0, |ext| ext.p + u64::from(ext.len));
        let (block, _) = self.alloc_run(goal, 1)?;
        if let Err(error) = self.ext_insert(logical as u32, 1, block).and_then(|()| self.store_extents(dir_number)) {
            let _ = self.free_run(block, 1);
            let _ = self.load_extents(&dir);
            return Err(error);
        }
        self.fs.data[..size].fill(0);
        put16(&mut self.fs.data, 4, limit as u16);
        place(&mut self.fs.data, 0);
        self.dir_seal(dir_number)?;
        self.write_buffer(block, Buffer::Data)?;
        let (new_size, now) = (dir.size + size as u64, self.now);
        self.update_inode(dir_number, |raw| {
            put32(raw, 4, new_size as u32);
            put32(raw, 12, now);
            put32(raw, 16, now);
            let flags = le32(raw, 32);
            put32(raw, 32, flags & !INDEX_FL);
        })
    }

    /// Takes `name` out of a folder; returns what it named.
    fn dir_remove(&mut self, dir_number: u32, name: &[u8]) -> Result<Option<u32>, Error> {
        let dir = self.checked(dir_number)?;
        self.dir_unindex(dir_number, &dir)?;
        let size = self.fs.block_size;
        let limit = self.dir_limit();
        for logical in 0..dir.size / size as u64 {
            let block = self.dir_block(&dir, logical)?;
            self.fs.read_block(block, Buffer::Data)?;
            let (mut at, mut before) = (0, None::<usize>);
            while at < limit {
                let record = usize::from(le16(&self.fs.data, at + 4));
                if record < 8 || record % 4 != 0 || at + record > limit {
                    return Err(Error::Corrupt);
                }
                let length = usize::from(self.fs.data[at + 6]);
                let found = le32(&self.fs.data, at);
                if found != 0 && &self.fs.data[at + 8..at + 8 + length] == name {
                    match before {
                        Some(previous) => {
                            let merged = usize::from(le16(&self.fs.data, previous + 4)) + record;
                            put16(&mut self.fs.data, previous + 4, merged as u16);
                        }
                        None => put32(&mut self.fs.data, at, 0),
                    }
                    self.dir_seal(dir_number)?;
                    self.write_buffer(block, Buffer::Data)?;
                    self.touch(dir_number, INDEX_FL)?;
                    return Ok(Some(found));
                }
                before = Some(at);
                at += record;
            }
        }
        Ok(None)
    }

    fn dir_is_empty(&mut self, dir: &Inode) -> Result<bool, Error> {
        let mut empty = true;
        self.fs.list(dir, &mut |entry| {
            if entry.name != b"." && entry.name != b".." {
                empty = false;
            }
            !empty
        })?;
        Ok(empty)
    }

    fn start_check(&mut self, parent: u32, name: &[u8]) -> Result<Inode, Error> {
        valid_name(name)?;
        let dir = self.checked(parent)?;
        if !dir.is_dir() {
            return Err(Error::NotADirectory);
        }
        if self.fs.lookup(&dir, name)?.is_some() {
            return Err(Error::Exists);
        }
        self.begin()?;
        Ok(dir)
    }

    /// Makes an empty file `name` in folder `parent` (inode numbers) with the permission bits `permissions`.
    pub fn create(&mut self, parent: u32, name: &[u8], permissions: u16) -> Result<Inode, Error> {
        self.start_check(parent, name)?;
        let number = self.alloc_inode((parent - 1) / self.fs.inodes_per_group, false)?;
        let made = self.init_inode(number, 0x8000 | (permissions & 0o7777), 1, 0, &Self::empty_root(), 0).and_then(|()| self.dir_add(parent, name, number, 1));
        if let Err(error) = made {
            let _ = self.free_inode(number, false);
            return Err(error);
        }
        self.fs.inode(number)
    }

    pub fn mkdir(&mut self, parent: u32, name: &[u8], permissions: u16) -> Result<Inode, Error> {
        let dir = self.start_check(parent, name)?;
        if dir.links >= LINKS_MAX {
            return Err(Error::TooManyLinks);
        }
        let group = (parent - 1) / self.fs.inodes_per_group;
        let number = self.alloc_inode(group, true)?;
        let home = u64::from(self.fs.first_data_block) + u64::from(group) * u64::from(self.fs.blocks_per_group);
        let block = match self.alloc_run(home, 1) {
            Ok((block, _)) => block,
            Err(error) => {
                let _ = self.free_inode(number, true);
                return Err(error);
            }
        };
        let size = self.fs.block_size;
        let made = (|| {
            let mut root = Self::empty_root();
            put16(&mut root, 2, 1);
            put16(&mut root, 16, 1);
            put32(&mut root, 20, block as u32);
            put16(&mut root, 18, (block >> 32) as u16);
            // The inode first: a folder block's checksum depends on the inode's generation.
            self.init_inode(number, 0x4000 | (permissions & 0o7777), 2, size as u64, &root, (size / SECTOR) as u64)?;
            self.fs.data[..size].fill(0);
            put32(&mut self.fs.data, 0, number);
            put16(&mut self.fs.data, 4, 12);
            self.fs.data[6] = 1;
            self.fs.data[7] = 2;
            self.fs.data[8] = b'.';
            put32(&mut self.fs.data, 12, parent);
            let limit = self.dir_limit();
            put16(&mut self.fs.data, 16, (limit - 12) as u16);
            self.fs.data[18] = 2;
            self.fs.data[19] = 2;
            self.fs.data[20] = b'.';
            self.fs.data[21] = b'.';
            self.dir_seal(number)?;
            self.write_buffer(block, Buffer::Data)?;
            self.dir_add(parent, name, number, 2)?;
            self.add_links(parent, 1)
        })();
        if let Err(error) = made {
            let _ = self.free_run(block, 1);
            let _ = self.free_inode(number, true);
            return Err(error);
        }
        self.fs.inode(number)
    }

    /// Removes a file, a link, a symbolic link or an empty folder from `parent`.
    pub fn remove(&mut self, parent: u32, name: &[u8]) -> Result<(), Error> {
        let dir = self.checked(parent)?;
        let target = self.fs.lookup(&dir, name)?.ok_or(Error::NotFound)?;
        if target.is_dir() && !self.dir_is_empty(&target)? {
            return Err(Error::NotEmpty);
        }
        self.begin()?;
        if !target.is_dir() && target.links > 1 {
            self.dir_remove(parent, name)?;
            return self.add_links(target.number, -1);
        }
        let fast_link = target.fast_target().is_some();
        if (!fast_link && target.flags & FLAG_EXTENTS == 0) || target.flags & FLAG_INLINE_DATA != 0 || self.read_raw(target.number, 104)? != 0 {
            return Err(Error::Unsupported);
        }
        if !fast_link {
            self.load_extents(&target)?;
        }
        self.dir_remove(parent, name)?;
        if !fast_link {
            for index in 0..self.count {
                let ext = self.exts[index];
                self.free_run(ext.p, ext.len)?;
            }
            for index in 0..self.leaf_count {
                let block = self.leaves[index];
                self.free_run(block, 1)?;
            }
        }
        let now = self.now.max(1);
        let root = Self::empty_root();
        self.update_inode(target.number, |raw| {
            put16(raw, 26, 0);
            put32(raw, 4, 0);
            put32(raw, 108, 0);
            put32(raw, 28, 0);
            put16(raw, 116, 0);
            put32(raw, 20, now);
            raw[40..100].copy_from_slice(&root);
        })?;
        self.free_inode(target.number, target.is_dir())?;
        if target.is_dir() {
            self.add_links(parent, -1)?;
        }
        self.leaf_count = 0;
        self.count = 0;
        Ok(())
    }

    /// Moves or renames; an existing target of the same kind is replaced (an empty folder for a folder).
    pub fn rename(&mut self, from_dir: u32, from: &[u8], to_dir: u32, to: &[u8]) -> Result<(), Error> {
        valid_name(to)?;
        let source_dir = self.checked(from_dir)?;
        let target_dir = self.checked(to_dir)?;
        if !target_dir.is_dir() {
            return Err(Error::NotADirectory);
        }
        let source = self.fs.lookup(&source_dir, from)?.ok_or(Error::NotFound)?;
        if from_dir == to_dir && from == to {
            return Ok(());
        }
        if source.is_dir() {
            // A folder cannot go into itself or below itself: walk `..` up from the new parent.
            let mut current = to_dir;
            for _ in 0..4096 {
                if current == source.number {
                    return Err(Error::Invalid);
                }
                if current == ROOT {
                    break;
                }
                let folder = self.checked(current)?;
                current = self.fs.lookup(&folder, b"..")?.ok_or(Error::Corrupt)?.number;
            }
            if target_dir.links >= LINKS_MAX && from_dir != to_dir {
                return Err(Error::TooManyLinks);
            }
        }
        self.begin()?;
        if let Some(existing) = self.fs.lookup(&target_dir, to)? {
            if existing.number == source.number {
                self.dir_remove(from_dir, from)?;
                return Ok(());
            }
            match (source.is_dir(), existing.is_dir()) {
                (false, false) | (true, true) => self.remove(to_dir, to)?,
                (_, true) => return Err(Error::IsADirectory),
                (true, false) => return Err(Error::NotADirectory),
            }
        }
        self.dir_add(to_dir, to, source.number, kind_of(source.mode))?;
        self.dir_remove(from_dir, from)?;
        if source.is_dir() && from_dir != to_dir {
            self.dir_unindex(source.number, &source)?;
            let block = self.dir_block(&source, 0)?;
            self.fs.read_block(block, Buffer::Data)?;
            if self.fs.data[12 + 6] != 2 || &self.fs.data[20..22] != b".." {
                return Err(Error::Corrupt);
            }
            put32(&mut self.fs.data, 12, to_dir);
            self.dir_seal(source.number)?;
            self.write_buffer(block, Buffer::Data)?;
            self.add_links(from_dir, -1)?;
            self.add_links(to_dir, 1)?;
        }
        Ok(())
    }
}

impl<D: Device> Writer<Journaled<D>> {
    /// Makes a new ext4 volume (4 KiB blocks, no checksums: see the module notes; a journal if `options.journal`) in
    /// `sectors` sectors of the device, with an empty root folder and `lost+found`, and returns it ready for writing.
    /// Everything on the device's first `sectors` sectors is lost. The usual sparse superblock backups are written, so
    /// a dead first sector does not end the volume.
    pub fn format(mut device: D, sectors: u64, options: &FormatOptions, stage: &'static mut Stage) -> Result<Self, Error> {
        format_raw(&mut device, sectors, options)?;
        let mut writer = Writer::new(crate::open(device, stage)?)?;
        writer.set_time(options.time);
        let root = writer.fs.root()?.number;
        writer.mkdir(root, b"lost+found", 0o700)?;
        writer.flush()?;
        Ok(writer)
    }
}

const FORMAT_BLOCK: usize = 4096;
const BLOCKS_PER_GROUP: u64 = 32768;
const FORMAT_INODE: usize = 256;

fn is_power_of(mut value: u64, base: u64) -> bool {
    if value == 0 {
        return false;
    }
    while value % base == 0 {
        value /= base;
    }
    value == 1
}

/// Block groups that carry a copy of the superblock and descriptors (`sparse_super`).
fn has_backup(group: u64) -> bool {
    group <= 1 || is_power_of(group, 3) || is_power_of(group, 5) || is_power_of(group, 7)
}

struct Geometry {
    blocks: u64,
    groups: u64,
    gdt_blocks: u64,
    inodes_per_group: u64,
    itable_blocks: u64,
}

impl Geometry {
    /// (first block, block bitmap, inode bitmap, inode table, blocks in the group, blocks of metadata)
    fn group(&self, group: u64) -> (u64, u64, u64, u64, u64, u64) {
        let start = group * BLOCKS_PER_GROUP;
        let copies = if has_backup(group) { 1 + self.gdt_blocks } else { 0 };
        let (block_map, inode_map, table) = (start + copies, start + copies + 1, start + copies + 2);
        let total = (self.blocks - start).min(BLOCKS_PER_GROUP);
        (start, block_map, inode_map, table, total, copies + 2 + self.itable_blocks)
    }
}

/// Writes the empty volume (no `lost+found` yet: that needs a mounted `Writer`).
pub fn format_raw<D: Device>(device: &mut D, sectors: u64, options: &FormatOptions) -> Result<(), Error> {
    let ratio = if options.bytes_per_inode == 0 { 65536 } else { u64::from(options.bytes_per_inode).max(1024) };
    let mut blocks = sectors / (FORMAT_BLOCK / SECTOR) as u64;
    let geometry = loop {
        let groups = blocks.div_ceil(BLOCKS_PER_GROUP);
        if groups == 0 {
            return Err(Error::Invalid);
        }
        let in_group = blocks.min(BLOCKS_PER_GROUP);
        let inodes = ((in_group * FORMAT_BLOCK as u64 / ratio).min(8 * FORMAT_BLOCK as u64).div_ceil(16) * 16).max(64);
        let candidate = Geometry { blocks, groups, gdt_blocks: (groups * 32).div_ceil(FORMAT_BLOCK as u64), inodes_per_group: inodes, itable_blocks: inodes * FORMAT_INODE as u64 / FORMAT_BLOCK as u64 };
        let (_, _, _, _, last_total, last_meta) = candidate.group(groups - 1);
        if last_total >= last_meta + 32 {
            break candidate;
        }
        if groups == 1 {
            return Err(Error::Invalid);
        }
        blocks = (groups - 1) * BLOCKS_PER_GROUP;
    };
    let (groups, inodes_per_group) = (geometry.groups, geometry.inodes_per_group);
    let (_, _, _, _, total0, meta0_blocks) = geometry.group(0);
    // The journal follows the root folder's block in group 0 (an eighth of the volume at most).
    let mut jblocks = if options.journal { (blocks / 64).clamp(1024, 16384) } else { 0 };
    // Linux refuses a journal under 1024 blocks, so a volume under 32 MiB goes without.
    if jblocks > blocks / 8 || total0 < meta0_blocks + 1 + jblocks + 32 {
        jblocks = 0;
    }
    let used0 = 1 + jblocks;
    let mut free_blocks = 0u64;
    for group in 0..groups {
        let (_, _, _, _, total, meta) = geometry.group(group);
        free_blocks += total - meta - if group == 0 { used0 } else { 0 };
    }
    let free_inodes = groups * inodes_per_group - 10;

    // The old superblock goes first, so a half-written volume is not mistaken for ext4.
    device.write(0, &[0u8; FORMAT_BLOCK])?;

    let mut block = [0u8; FORMAT_BLOCK];
    let zero = [0u8; 4 * FORMAT_BLOCK];
    let lba = |block: u64| block * (FORMAT_BLOCK / SECTOR) as u64;
    let block_bitmap = |group: u64, block: &mut [u8; FORMAT_BLOCK]| {
        let (_, _, _, _, total, meta) = geometry.group(group);
        block.fill(0);
        for index in 0..(meta as usize + if group == 0 { used0 as usize } else { 0 }) {
            set_bit(block, index, true);
        }
        for index in total as usize..8 * FORMAT_BLOCK {
            set_bit(block, index, true);
        }
    };
    let inode_bitmap = |group: u64, block: &mut [u8; FORMAT_BLOCK]| {
        block.fill(0);
        if group == 0 {
            for index in 0..10 {
                set_bit(block, index, true);
            }
        }
        for index in inodes_per_group as usize..8 * FORMAT_BLOCK {
            set_bit(block, index, true);
        }
    };
    let seed = crc32c(!0, &options.uuid);
    for group in 0..groups {
        let (start, block_map, inode_map, table, _, _) = geometry.group(group);
        block_bitmap(group, &mut block);
        device.write(lba(block_map), &block)?;
        inode_bitmap(group, &mut block);
        device.write(lba(inode_map), &block)?;
        let mut done = 0;
        while done < geometry.itable_blocks {
            let take = (geometry.itable_blocks - done).min(4);
            device.write(lba(table + done), &zero[..take as usize * FORMAT_BLOCK])?;
            done += take;
        }
        let _ = start;
    }

    // The root folder: inode 2 and one block holding `.` and `..`.
    let (_, _, _, table0, _, meta0) = geometry.group(0);
    let root_block = meta0;
    block.fill(0);
    put32(&mut block, 0, 2);
    put16(&mut block, 4, 12);
    block[6] = 1;
    block[7] = 2;
    block[8] = b'.';
    put32(&mut block, 12, 2);
    let dir_end = FORMAT_BLOCK - if options.checksums { DIR_TAIL } else { 0 };
    put16(&mut block, 16, (dir_end - 12) as u16);
    block[18] = 2;
    block[19] = 2;
    block[20] = b'.';
    block[21] = b'.';
    if options.checksums {
        // The checksum entry ending a folder block; inode 2 has generation 0.
        let sum = crc32c(crc32c(crc32c(seed, &2u32.to_le_bytes()), &0u32.to_le_bytes()), &block[..dir_end]);
        put16(&mut block, dir_end + 4, DIR_TAIL as u16);
        block[dir_end + 7] = 0xde;
        put32(&mut block, dir_end + 8, sum);
    }
    device.write(lba(root_block), &block)?;
    if jblocks > 0 {
        let mut done = 0;
        while done < jblocks {
            let take = (jblocks - done).min(4);
            device.write(lba(root_block + 1 + done), &zero[..take as usize * FORMAT_BLOCK])?;
            done += take;
        }
        block.fill(0);
        put32_be(&mut block, 0, 0xc03b_3998);
        put32_be(&mut block, 4, 4);
        put32_be(&mut block, 12, FORMAT_BLOCK as u32);
        put32_be(&mut block, 16, jblocks as u32);
        put32_be(&mut block, 20, 1);
        put32_be(&mut block, 24, 1);
        put32_be(&mut block, 40, 0);
        put32_be(&mut block, 64, 1);
        block[48..64].copy_from_slice(&options.uuid);
        device.write(lba(root_block + 1), &block)?;
    }
    block.fill(0);
    {
        let raw = &mut block[FORMAT_INODE..2 * FORMAT_INODE];
        put16(raw, 0, 0x4000 | 0o755);
        put32(raw, 4, FORMAT_BLOCK as u32);
        for at in [8, 12, 16, 144] {
            put32(raw, at, options.time);
        }
        put16(raw, 26, 2);
        put32(raw, 28, (FORMAT_BLOCK / SECTOR) as u32);
        put32(raw, 32, FLAG_EXTENTS);
        put16(raw, 40, 0xf30a);
        put16(raw, 42, 1);
        put16(raw, 44, 4);
        put16(raw, 52 + 4, 1);
        put32(raw, 40 + 20, root_block as u32);
        put16(raw, 128, 32);
    }
    if jblocks > 0 {
        let raw = &mut block[7 * FORMAT_INODE..8 * FORMAT_INODE];
        put16(raw, 0, 0o100600);
        put32(raw, 4, (jblocks * FORMAT_BLOCK as u64) as u32);
        for at in [8, 12, 16, 144] {
            put32(raw, at, options.time);
        }
        put16(raw, 26, 1);
        put32(raw, 28, (jblocks * (FORMAT_BLOCK / SECTOR) as u64) as u32);
        put32(raw, 32, FLAG_EXTENTS);
        put16(raw, 40, 0xf30a);
        put16(raw, 42, 1);
        put16(raw, 44, 4);
        put16(raw, 52 + 4, jblocks as u16);
        put32(raw, 40 + 20, (root_block + 1) as u32);
        put16(raw, 128, 32);
    }
    let journal_map: [u8; 60] = block[7 * FORMAT_INODE + 40..7 * FORMAT_INODE + 100].try_into().unwrap();
    if options.checksums {
        for number in [2u32, 8] {
            let raw = &mut block[(number as usize - 1) * FORMAT_INODE..number as usize * FORMAT_INODE];
            if raw[0] == 0 && raw[1] == 0 {
                continue;
            }
            let sum = crc32c(crc32c(crc32c(seed, &number.to_le_bytes()), &0u32.to_le_bytes()), raw);
            put16(raw, 0x7c, sum as u16);
            put16(raw, 0x82, (sum >> 16) as u16);
        }
    }
    device.write(lba(table0), &block)?;

    // The descriptor blocks, primary and backups, computed group by group.
    let superblock = |group: u64| {
        let mut sb = [0u8; 1024];
        put32(&mut sb, 0, (groups * inodes_per_group) as u32);
        put32(&mut sb, 4, blocks as u32);
        put32(&mut sb, 12, free_blocks as u32);
        put32(&mut sb, 16, free_inodes as u32);
        put32(&mut sb, 24, 2);
        put32(&mut sb, 28, 2);
        put32(&mut sb, 32, BLOCKS_PER_GROUP as u32);
        put32(&mut sb, 36, BLOCKS_PER_GROUP as u32);
        put32(&mut sb, 40, inodes_per_group as u32);
        put32(&mut sb, 48, options.time);
        put16(&mut sb, 54, 0xffff);
        put16(&mut sb, 56, MAGIC);
        put16(&mut sb, 58, 1);
        put16(&mut sb, 60, 1);
        put32(&mut sb, 64, options.time);
        put32(&mut sb, 76, 1);
        put32(&mut sb, 84, 11);
        put16(&mut sb, 88, FORMAT_INODE as u16);
        put16(&mut sb, 90, group as u16);
        put32(&mut sb, 92, 0);
        put32(&mut sb, 96, INCOMPAT_FILETYPE | INCOMPAT_EXTENTS);
        put32(&mut sb, 100, 0x1 | 0x2 | 0x8 | 0x20 | 0x40 | if options.checksums { RO_METADATA_CSUM } else { 0 });
        if jblocks > 0 {
            put32(&mut sb, 224, 8);
            put32(&mut sb, 92, 0x4);
            sb[253] = 1;
            sb[268..328].copy_from_slice(&journal_map);
            put32(&mut sb, 332, (jblocks * FORMAT_BLOCK as u64) as u32);
        }
        sb[104..120].copy_from_slice(&options.uuid);
        let label = &options.label[..options.label.len().min(16)];
        sb[120..120 + label.len()].copy_from_slice(label);
        sb[236..252].copy_from_slice(&options.uuid);
        sb[252] = 1;
        put32(&mut sb, 264, options.time);
        put16(&mut sb, 348, 32);
        put16(&mut sb, 350, 32);
        put32(&mut sb, 352, 1);
        if options.checksums {
            sb[373] = 1; // s_checksum_type: crc32c
            let sum = crc32c(!0, &sb[..1020]);
            put32(&mut sb, 1020, sum);
        }
        sb
    };
    let mut scratch = [0u8; FORMAT_BLOCK];
    for index in 0..geometry.gdt_blocks {
        block.fill(0);
        for slot in 0..(FORMAT_BLOCK / 32) as u64 {
            let group = index * (FORMAT_BLOCK / 32) as u64 + slot;
            if group >= groups {
                break;
            }
            let (_, block_map, inode_map, table, total, meta) = geometry.group(group);
            let raw = &mut block[slot as usize * 32..slot as usize * 32 + 32];
            put32(raw, 0, block_map as u32);
            put32(raw, 4, inode_map as u32);
            put32(raw, 8, table as u32);
            put16(raw, 12, (total - meta - if group == 0 { used0 } else { 0 }) as u16);
            put16(raw, 14, (inodes_per_group - if group == 0 { 10 } else { 0 }) as u16);
            put16(raw, 16, u16::from(group == 0));
            if options.checksums {
                block_bitmap(group, &mut scratch);
                put16(raw, 0x18, crc32c(seed, &scratch[..BLOCKS_PER_GROUP as usize / 8]) as u16);
                inode_bitmap(group, &mut scratch);
                put16(raw, 0x1a, crc32c(seed, &scratch[..inodes_per_group as usize / 8]) as u16);
                let sum = crc32c(crc32c(seed, &(group as u32).to_le_bytes()), raw);
                put16(raw, 0x1e, sum as u16);
            }
        }
        device.write(lba(1 + index), &block)?;
        for group in 1..groups {
            if has_backup(group) {
                device.write(lba(group * BLOCKS_PER_GROUP + 1 + index), &block)?;
            }
        }
    }
    for group in 1..groups {
        if has_backup(group) {
            block.fill(0);
            block[..1024].copy_from_slice(&superblock(group));
            device.write(lba(group * BLOCKS_PER_GROUP), &block)?;
        }
    }
    block.fill(0);
    block[1024..2048].copy_from_slice(&superblock(0));
    device.write(0, &block)
}

/// With sparse_super only groups 0, 1 and powers of 3, 5 and 7 carry a copy of the superblock and descriptors.
fn is_sparse_group(group: u32) -> bool {
    if group <= 1 {
        return true;
    }
    [3u32, 5, 7].iter().any(|&base| {
        let mut power = base;
        while power < group {
            power = power.saturating_mul(base);
        }
        power == group
    })
}
