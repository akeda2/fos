//! The write-intent bitmap md keeps beside the superblock (`mdadm --bitmap=internal`).
//!
//! Every member carries an identical copy: a 256-byte header at `super_sector + bitmap_offset`, then the bits (bit `k`
//! is bit `k % 8` of byte `k / 8`, little-endian). One bit stands for `chunk` sectors of the resync cursor space (array
//! sectors for RAID 1, sectors of one member for RAID 5 and 6). A bit is set, and flushed, before the first write into
//! its chunk; after a crash only the chunks whose bit is set need their copies compared. The bits past the last chunk
//! stay set, as mdadm leaves them.
//!
//! Header fields used (little-endian): magic 0, version 4, uuid 8, events 24, sync_size (sectors) 40, state 48,
//! chunk size in bytes 52. Verified against the bytes mdadm 4.5 writes.

use crate::{Error, Members, SECTOR};

pub const MAGIC: u32 = 0x6d74_6962;
pub const HEADER: usize = 256;
/// The most this code holds in memory: header and bits together.
pub const CAP: usize = 4 * 1024;
const STALE: u32 = 2;
const WRITE_ERROR: u32 = 4;

pub struct Bitmap {
    pub(crate) data: [u8; CAP],
    /// Sectors the header and the bits take on a member.
    pub(crate) sectors: usize,
    /// Cursor sectors one bit covers.
    pub(crate) chunk: u64,
    pub(crate) nbits: u64,
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}
fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

impl Bitmap {
    /// Reads and validates the bitmap at sector `at` of `member`. It must belong to the array (`uuid`), not be marked
    /// stale or failed, be at least as new as the array (`events`) and cover exactly `sync_size` cursor sectors; any
    /// other bitmap is something this code cannot vouch for, so the array stays read-only.
    pub fn load<M: Members>(members: &mut M, member: usize, at: u64, uuid: &[u8; 16], events: u64, sync_size: u64) -> Result<Bitmap, Error> {
        let mut bitmap = Bitmap { data: [0; CAP], sectors: 1, chunk: 0, nbits: 0 };
        members.read(member, at, &mut bitmap.data[..SECTOR])?;
        let head = &bitmap.data[..SECTOR];
        let chunk_bytes = u32_at(head, 52) as u64;
        let valid = u32_at(head, 0) == MAGIC
            && (3..=5).contains(&u32_at(head, 4))
            && head[8..24] == uuid[..]
            && u64_at(head, 24) >= events
            && u32_at(head, 48) & (STALE | WRITE_ERROR) == 0
            && u64_at(head, 40) == sync_size
            && chunk_bytes >= SECTOR as u64
            && chunk_bytes % SECTOR as u64 == 0;
        if !valid {
            return Err(Error::Unsupported);
        }
        bitmap.chunk = chunk_bytes / SECTOR as u64;
        bitmap.nbits = sync_size.div_ceil(bitmap.chunk);
        let total = HEADER as u64 + bitmap.nbits.div_ceil(8);
        if total > CAP as u64 {
            return Err(Error::Unsupported);
        }
        bitmap.sectors = (total as usize).div_ceil(SECTOR);
        if bitmap.sectors > 1 {
            members.read(member, at + 1, &mut bitmap.data[SECTOR..bitmap.sectors * SECTOR])?;
        }
        Ok(bitmap)
    }

    /// Adds the set bits of another member's copy (which must have the same shape).
    pub fn merge(&mut self, other: &Bitmap) -> Result<(), Error> {
        if other.chunk != self.chunk || other.nbits != self.nbits {
            return Err(Error::Unsupported);
        }
        for k in 0..self.nbits.div_ceil(8) as usize {
            self.data[HEADER + k] |= other.data[HEADER + k];
        }
        Ok(())
    }

    pub fn get(&self, bit: u64) -> bool {
        self.data[HEADER + (bit / 8) as usize] >> (bit % 8) & 1 != 0
    }

    /// Sets `bit`; true if it was clear.
    pub fn set(&mut self, bit: u64) -> bool {
        let byte = &mut self.data[HEADER + (bit / 8) as usize];
        let mask = 1 << (bit % 8);
        let was = *byte & mask != 0;
        *byte |= mask;
        !was
    }

    pub fn any(&self) -> bool {
        let (full, rest) = ((self.nbits / 8) as usize, self.nbits % 8);
        self.data[HEADER..HEADER + full].iter().any(|&b| b != 0) || (rest > 0 && self.data[HEADER + full] & ((1 << rest) - 1) != 0)
    }

    /// Clears every chunk's bit; the padding bits keep their value.
    pub fn clear(&mut self) {
        let (full, rest) = ((self.nbits / 8) as usize, self.nbits % 8);
        self.data[HEADER..HEADER + full].fill(0);
        if rest > 0 {
            self.data[HEADER + full] &= !((1u8 << rest) - 1);
        }
    }

    /// The first set bit at or after `from`.
    pub fn next_set(&self, from: u64) -> Option<u64> {
        (from..self.nbits).find(|&bit| self.get(bit))
    }

    pub fn set_events(&mut self, events: u64) {
        self.data[24..32].copy_from_slice(&events.to_le_bytes());
    }

    /// The region sector that holds `bit`.
    pub fn sector_of(&self, bit: u64) -> usize {
        (HEADER + (bit / 8) as usize) / SECTOR
    }
}
