//! Linux software RAID (md, the arrays `mdadm` makes) for fos, as `no_std` code with no allocation and host tests.
//!
//! The point is recovery: an array fos assembles is the same bytes a Linux live USB assembles with
//! `mdadm --assemble --scan`, and the filesystem on it (ext4) is plain ext4. [`superblock`] reads and writes the v1.x
//! superblock (v1.0 at the end of the member, v1.1 at its start, v1.2 4 KiB in); [`array`] assembles members into a
//! mirror and reads from it. The caller supplies the members through [`Members`], so the same code serves the kernel's
//! block layer and host tests over byte vectors.
//!
//! RAID 0, 1, 5, 6 and 10 (near layout) read and write (writes follow md's order: dirty mark, data, clean mark). An
//! internal write-intent bitmap ([`bitmap`]) is kept for RAID 1, 5 and 6; see `docs/md.md`.
#![cfg_attr(not(test), no_std)]

pub mod array;
pub mod bitmap;
pub mod crc32c;
pub mod gf;
pub mod superblock;

/// The mirror's old home: `Array` handles RAID 0 and 10 too now and lives in [`array`].
pub mod raid1 {
    pub use crate::array::*;
}

/// The sector size md addresses everything in; a member with larger physical sectors is still counted in 512 bytes.
pub const SECTOR: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// A member could not be read.
    Device,
    /// No md v1.x superblock where one belongs, or its checksum is wrong.
    NotMd,
    /// An md array this code cannot handle yet (level, metadata version, reshape in progress...).
    Unsupported,
    /// No member carries the wanted array.
    NoMembers,
    /// The array has fewer members than it needs to be read (or the members disagree about the array).
    Degraded,
    /// A read or write beyond the array's end, or a buffer that is not whole sectors.
    Range,
    /// The array is not open for writing (or is already).
    NotWritable,
    /// A resync or rebuild is already running, or there is nothing to rebuild.
    Busy,
    /// The member is too small to hold the array's data and its superblock.
    TooSmall,
}

/// The disks an array is assembled from, numbered by the caller from 0.
pub trait Members {
    /// How many members the caller offers.
    fn count(&self) -> usize;
    /// The member's size in 512-byte sectors.
    fn sectors(&self, member: usize) -> u64;
    /// Reads whole sectors from `member`, addressed from its first sector.
    fn read(&mut self, member: usize, lba: u64, buffer: &mut [u8]) -> Result<(), Error>;
    /// Writes whole sectors to `member`. Read-only users need not provide it.
    fn write(&mut self, _member: usize, _lba: u64, _buffer: &[u8]) -> Result<(), Error> {
        Err(Error::Unsupported)
    }
    /// Returns once everything written to `member` is on the medium (a cache flush).
    fn flush(&mut self, _member: usize) -> Result<(), Error> {
        Err(Error::Unsupported)
    }
}
