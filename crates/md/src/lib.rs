//! Linux software RAID (md, the arrays `mdadm` makes) for fos, as `no_std` code with no allocation and host tests.
//!
//! The point is recovery: an array fos assembles is the same bytes a Linux live USB assembles with
//! `mdadm --assemble --scan`, and the filesystem on it (ext4) is plain ext4. [`superblock`] reads and writes the v1.x
//! superblock (v1.0 at the end of the member, v1.1 at its start, v1.2 4 KiB in); [`raid1`] assembles members into a
//! mirror and reads from it. The caller supplies the members through [`Members`], so the same code serves the kernel's
//! block layer and host tests over byte vectors.
//!
//! Stage one is read-only RAID 1. Writes (which must bump the event count and the superblock in the right order),
//! RAID 0/10 and RAID 5/6 come later; see `docs/md.md`.
#![cfg_attr(not(test), no_std)]

pub mod raid1;
pub mod superblock;

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
    /// A read beyond the array's end, or a buffer that is not whole sectors.
    Range,
}

/// The disks an array is assembled from, numbered by the caller from 0.
pub trait Members {
    /// How many members the caller offers.
    fn count(&self) -> usize;
    /// The member's size in 512-byte sectors.
    fn sectors(&self, member: usize) -> u64;
    /// Reads whole sectors from `member`, addressed from its first sector.
    fn read(&mut self, member: usize, lba: u64, buffer: &mut [u8]) -> Result<(), Error>;
}
