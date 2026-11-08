//! The md v1.x superblock (`struct mdp_superblock_1` of the Linux kernel, `include/uapi/linux/raid/md_p.h`).
//!
//! Little-endian, 256 bytes of header and then one `u16` role per slot (`max_dev` of them), the whole thing checksummed
//! by summing 32-bit words with the checksum field taken as zero. Where it sits depends on the minor version:
//! 1.0 near the end of the member (the last 8 KiB, 4 KiB aligned, so boot code at the start still works), 1.1 at sector
//! 0, 1.2 at sector 8. `super_offset` inside the block says where the writer put it, which lets a reader notice a
//! superblock found at the wrong place (a copied or truncated disk).

use crate::{Error, SECTOR};

pub const MAGIC: u32 = 0xa92b_4efc;
/// The fixed part, before the role table.
pub const HEADER: usize = 256;
/// Roles: a slot number below `max_dev`, or one of these.
pub const ROLE_SPARE: u16 = 0xffff;
pub const ROLE_FAULTY: u16 = 0xfffe;
/// `resync_offset` of an array whose members are all in step.
pub const SYNCED: u64 = u64::MAX;

/// `feature_map` bits this code knows. Anything else set means the array uses something it does not understand.
pub const FEATURE_BITMAP: u32 = 1;
pub const FEATURE_RECOVERY_OFFSET: u32 = 2;
pub const FEATURE_RESHAPE_ACTIVE: u32 = 4;
pub const FEATURE_BAD_BLOCKS: u32 = 8;

pub const LEVEL_LINEAR: i32 = -1;
pub const LEVEL_RAID0: i32 = 0;
pub const LEVEL_RAID1: i32 = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Superblock {
    /// Minor version, by where it was found: 0, 1 or 2.
    pub minor: u8,
    pub feature_map: u32,
    pub set_uuid: [u8; 16],
    pub set_name: [u8; 32],
    pub ctime: u64,
    pub level: i32,
    pub layout: u32,
    /// Sectors used on each member.
    pub size: u64,
    /// Chunk size in sectors (striped levels).
    pub chunk_sectors: u32,
    pub raid_disks: u32,
    pub bitmap_offset: i32,
    /// First sector of data on this member.
    pub data_offset: u64,
    /// Sectors of data on this member.
    pub data_size: u64,
    pub super_offset: u64,
    pub recovery_offset: u64,
    /// This member's slot (its index in the role table).
    pub dev_number: u32,
    pub device_uuid: [u8; 16],
    pub devflags: u8,
    pub utime: u64,
    /// Bumped by every superblock update; the member with the highest count is the freshest.
    pub events: u64,
    pub resync_offset: u64,
    pub max_dev: u32,
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}
fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

/// The sector a superblock of minor version `minor` sits at on a member of `sectors` sectors; None for a bad minor or a
/// member too small.
pub fn super_sector(minor: u8, sectors: u64) -> Option<u64> {
    match minor {
        0 => sectors.checked_sub(16).map(|s| s & !7),
        1 => Some(0),
        2 => Some(8),
        _ => None,
    }
}

/// The checksum of a superblock whose bytes (header and roles) are `block`, `sb_csum` counted as zero.
pub fn checksum(block: &[u8]) -> u32 {
    let max_dev = u32_at(block, 220) as usize;
    let size = HEADER + max_dev * 2;
    let mut sum: u64 = 0;
    let mut at = 0;
    while at + 4 <= size {
        let word = if at == 216 { 0 } else { u32_at(block, at) };
        sum += word as u64;
        at += 4;
    }
    if at < size {
        sum += u16::from_le_bytes([block[at], block[at + 1]]) as u64;
    }
    ((sum & 0xffff_ffff) + (sum >> 32)) as u32
}

impl Superblock {
    /// Reads the superblock from `block` (at least the header and the role table: pass `READ_BYTES`) and returns it
    /// with the role table borrowed from `block`.
    pub fn parse(block: &[u8], minor: u8) -> Result<(Superblock, Roles<'_>), Error> {
        if block.len() < HEADER || u32_at(block, 0) != MAGIC {
            return Err(Error::NotMd);
        }
        if u32_at(block, 4) != 1 {
            return Err(Error::Unsupported);
        }
        let max_dev = u32_at(block, 220);
        let end = HEADER + max_dev as usize * 2;
        if max_dev > 1920 || block.len() < end {
            return Err(Error::NotMd);
        }
        if checksum(block) != u32_at(block, 216) {
            return Err(Error::NotMd);
        }
        let sb = Superblock {
            minor,
            feature_map: u32_at(block, 8),
            set_uuid: block[16..32].try_into().unwrap(),
            set_name: block[32..64].try_into().unwrap(),
            ctime: u64_at(block, 64),
            level: u32_at(block, 72) as i32,
            layout: u32_at(block, 76),
            size: u64_at(block, 80),
            chunk_sectors: u32_at(block, 88),
            raid_disks: u32_at(block, 92),
            bitmap_offset: u32_at(block, 96) as i32,
            data_offset: u64_at(block, 128),
            data_size: u64_at(block, 136),
            super_offset: u64_at(block, 144),
            recovery_offset: u64_at(block, 152),
            dev_number: u32_at(block, 160),
            device_uuid: block[168..184].try_into().unwrap(),
            devflags: block[184],
            utime: u64_at(block, 192),
            events: u64_at(block, 200),
            resync_offset: u64_at(block, 208),
            max_dev,
        };
        Ok((sb, Roles { bytes: &block[HEADER..end] }))
    }

    /// The array's name as text (what `mdadm --name` set, usually `host:name`), up to the first NUL.
    pub fn name(&self) -> &str {
        let end = self.set_name.iter().position(|&b| b == 0).unwrap_or(32);
        core::str::from_utf8(&self.set_name[..end]).unwrap_or("")
    }

    /// Writes the header and the role table into `out` (zeroed first), with the checksum. Returns the bytes used.
    /// `out` must hold `HEADER + 2 * roles.len()` bytes; `roles.len()` becomes `max_dev`.
    pub fn build(&self, roles: &[u16], out: &mut [u8]) -> usize {
        let size = HEADER + roles.len() * 2;
        out[..size].fill(0);
        let put32 = |out: &mut [u8], at: usize, v: u32| out[at..at + 4].copy_from_slice(&v.to_le_bytes());
        let put64 = |out: &mut [u8], at: usize, v: u64| out[at..at + 8].copy_from_slice(&v.to_le_bytes());
        put32(out, 0, MAGIC);
        put32(out, 4, 1);
        put32(out, 8, self.feature_map);
        out[16..32].copy_from_slice(&self.set_uuid);
        out[32..64].copy_from_slice(&self.set_name);
        put64(out, 64, self.ctime);
        put32(out, 72, self.level as u32);
        put32(out, 76, self.layout);
        put64(out, 80, self.size);
        put32(out, 88, self.chunk_sectors);
        put32(out, 92, self.raid_disks);
        put32(out, 96, self.bitmap_offset as u32);
        put64(out, 128, self.data_offset);
        put64(out, 136, self.data_size);
        put64(out, 144, self.super_offset);
        put64(out, 152, self.recovery_offset);
        put32(out, 160, self.dev_number);
        out[168..184].copy_from_slice(&self.device_uuid);
        out[184] = self.devflags;
        put64(out, 192, self.utime);
        put64(out, 200, self.events);
        put64(out, 208, self.resync_offset);
        put32(out, 220, roles.len() as u32);
        for (i, role) in roles.iter().enumerate() {
            out[HEADER + i * 2..HEADER + i * 2 + 2].copy_from_slice(&role.to_le_bytes());
        }
        let sum = checksum(out);
        put32(out, 216, sum);
        size
    }
}

/// The role table: slot `n` holds the role of the member that has `dev_number == n`.
#[derive(Clone, Copy)]
pub struct Roles<'a> {
    bytes: &'a [u8],
}

impl Roles<'_> {
    pub fn len(&self) -> usize {
        self.bytes.len() / 2
    }
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
    /// The role of slot `slot`: an active position (0, 1, ...), `ROLE_SPARE` or `ROLE_FAULTY`.
    pub fn get(&self, slot: usize) -> Option<u16> {
        let at = slot * 2;
        self.bytes.get(at..at + 2).map(|b| u16::from_le_bytes([b[0], b[1]]))
    }
}

/// A superblock read takes this many sectors: 4 KiB holds the header and the 1920 slots `parse` accepts.
pub const READ_SECTORS: usize = 8;
pub const READ_BYTES: usize = READ_SECTORS * SECTOR;
