//! Making a new array (`mdadm --create`): fresh v1.2 superblocks and an internal write-intent bitmap on blank members.
//!
//! The layout is the one mdadm 4.x writes for a whole-disk member, so a Linux live USB assembles the result with
//! `mdadm --assemble --scan`:
//!
//! * sector 8: the superblock (minor version 1.2), `super_offset` 8, one role per member in slots 0..n and spare roles
//!   after them (`MAX_DEV` slots, as room for later members);
//! * sector 16 (`bitmap_offset` 8): the bitmap, header and bits, every bit set: nothing is in step yet;
//! * sector `DATA_OFFSET` (1 MiB): the data. Linux's own default is 128 MiB, which leaves room to reshape; this code
//!   does not reshape and keeps the smaller gap, which Linux reads the same way;
//! * the last `TAIL` sectors (1 MiB) stay out of the data and are zeroed: a GPT's backup header and a v1.0 superblock
//!   live there, and Linux uses a backup GPT when the primary is gone, so a stale one would show partitions on a member.
//!
//! The head (sectors 0 to `DATA_OFFSET`, with an old partition table and any older superblock) is zeroed too, and so
//! is the first MiB of every member's data: the array's first sectors then read as zeros instead of an old file
//! system's superblock, and the parity of those rows is right already. Superblocks go out last, after a flush, so a
//! create cut short leaves members without one, or a dirty array missing members, which assembly refuses.
//!
//! The superblocks say `resync_offset` 0: the parity (RAID 5, 6) or the second copy (RAID 1) is not computed yet. The
//! caller assembles the array and runs the resync (`Array::begin_resync`), as Linux starts one after `mdadm --create`;
//! until it finishes the array counts as dirty. Levels: 1, 5 and 6, the ones the bitmap and the resync serve.

use crate::bitmap;
use crate::superblock::{self, Superblock, FEATURE_BITMAP, LEVEL_RAID1, LEVEL_RAID5, LEVEL_RAID6, ROLE_SPARE};
use crate::{Error, Members, SECTOR};

/// Where the superblock sits (minor 1.2).
pub const SUPER_SECTOR: u64 = 8;
/// The bitmap's start, counted from the superblock (Linux and mdadm put it 4 KiB after it).
pub const BITMAP_OFFSET: i32 = 8;
/// First data sector on each member (1 MiB).
pub const DATA_OFFSET: u64 = 2048;
/// Sectors left out at the end of each member (1 MiB): the backup GPT and a v1.0 superblock's place, zeroed.
pub const TAIL: u64 = 2048;
/// Data sectors zeroed at the start of each member.
pub const ZERO_DATA: u64 = 2048;
/// Role-table slots: room for later members without moving the bitmap (the table must fit the 4 KiB before it).
pub const MAX_DEV: usize = 384;
/// The default chunk (stripe unit) of RAID 5 and 6: 512 KiB, mdadm's default.
pub const DEFAULT_CHUNK: u32 = 1024;
/// RAID 5 and 6 layout: left-symmetric, mdadm's default.
pub const LEFT_SYMMETRIC: u32 = 2;
/// Seconds the Linux bitmap daemon waits between clearing passes (mdadm's default).
const DAEMON_SLEEP: u32 = 5;
/// The smallest bitmap chunk (64 MiB, what mdadm picks for large arrays); doubled until the bits fit `bitmap::CAP`.
const BITMAP_CHUNK_MIN: u64 = 64 << 20;

/// What to make.
pub struct Spec {
    /// 1, 5 or 6.
    pub level: i32,
    /// Chunk size in sectors (RAID 5 and 6; ignored for RAID 1). A power of two, at least 8.
    pub chunk_sectors: u32,
    /// `host:name`, as `mdadm --name` sets it (at most 32 bytes; longer is cut).
    pub name: [u8; 32],
    /// The array's uuid, and each member's (one per member, in member order): random, from the caller.
    pub uuid: [u8; 16],
    /// Seconds since 1970.
    pub now: u64,
}

/// The shape a create gives, before anything is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plan {
    /// Data sectors used on each member.
    pub member_size: u64,
    /// The array's size in sectors.
    pub sectors: u64,
    /// The bitmap's chunk in sectors and its size on each member, in sectors.
    pub bitmap_chunk: u64,
    pub bitmap_sectors: u64,
}

/// The array members of these sizes (in sectors) would make, or why not: Unsupported for a level, member count or chunk
/// this code does not make, TooSmall when a member has no room for the head, tail and one chunk.
pub fn plan(spec: &Spec, member_sectors: &[u64]) -> Result<Plan, Error> {
    let disks = member_sectors.len() as u64;
    let (least, chunk) = match spec.level {
        LEVEL_RAID1 => (2, 8u64),
        LEVEL_RAID5 => (3, spec.chunk_sectors as u64),
        LEVEL_RAID6 => (4, spec.chunk_sectors as u64),
        _ => return Err(Error::Unsupported),
    };
    if disks < least || disks as usize > crate::array::MAX_MEMBERS || chunk < 8 || !chunk.is_power_of_two() {
        return Err(Error::Unsupported);
    }
    let smallest = member_sectors.iter().copied().min().unwrap_or(0);
    let usable = smallest.saturating_sub(DATA_OFFSET + TAIL) / chunk * chunk;
    if usable < ZERO_DATA.max(chunk) {
        return Err(Error::TooSmall);
    }
    let data_disks = match spec.level {
        LEVEL_RAID1 => 1,
        LEVEL_RAID5 => disks - 1,
        _ => disks - 2,
    };
    // the bitmap counts the resync cursor: array sectors for RAID 1, member sectors for RAID 5 and 6; both are `usable`
    let mut bitmap_chunk = BITMAP_CHUNK_MIN / SECTOR as u64;
    while bitmap::HEADER as u64 + usable.div_ceil(bitmap_chunk).div_ceil(8) > bitmap::CAP as u64 {
        bitmap_chunk *= 2;
    }
    let bitmap_sectors = (bitmap::HEADER as u64 + usable.div_ceil(bitmap_chunk).div_ceil(8)).div_ceil(SECTOR as u64);
    Ok(Plan { member_size: usable, sectors: usable * data_disks, bitmap_chunk, bitmap_sectors })
}

/// Zeroes `count` sectors of `member` from `lba`.
fn zero<M: Members>(members: &mut M, member: usize, lba: u64, count: u64) -> Result<(), Error> {
    static ZEROS: [u8; 64 * SECTOR] = [0; 64 * SECTOR];
    let mut done = 0;
    while done < count {
        let piece = (count - done).min(64);
        members.write(member, lba + done, &ZEROS[..piece as usize * SECTOR])?;
        done += piece;
    }
    Ok(())
}

/// The bitmap header and bits as mdadm writes them for a new array: every bit set (padding included), events level
/// with the superblocks, `sectors_reserved` the room up to the data.
fn bitmap_block(spec: &Spec, plan: &Plan, events: u64, out: &mut [u8]) {
    out.fill(0xff);
    out[..bitmap::HEADER].fill(0);
    let put32 = |out: &mut [u8], at: usize, v: u32| out[at..at + 4].copy_from_slice(&v.to_le_bytes());
    let put64 = |out: &mut [u8], at: usize, v: u64| out[at..at + 8].copy_from_slice(&v.to_le_bytes());
    put32(out, 0, bitmap::MAGIC);
    put32(out, 4, 4); // version
    out[8..24].copy_from_slice(&spec.uuid);
    put64(out, 24, events);
    put64(out, 32, events); // events_cleared
    put64(out, 40, plan.member_size); // sync_size
    put32(out, 48, 0); // state
    put32(out, 52, (plan.bitmap_chunk * SECTOR as u64) as u32);
    put32(out, 56, DAEMON_SLEEP);
    put32(out, 60, 0); // write_behind
    put32(out, 64, (DATA_OFFSET - SUPER_SECTOR - BITMAP_OFFSET as u64) as u32); // sectors_reserved
}

/// Writes a new array over `members` (all of them, member `i` taking role `i`), erasing what they held at the head,
/// tail and start of the data. `device_uuids` gives each member's uuid. Returns the plan it followed. The array is
/// dirty until resynced.
pub fn create<M: Members>(members: &mut M, spec: &Spec, device_uuids: &[[u8; 16]]) -> Result<Plan, Error> {
    let count = members.count();
    if device_uuids.len() < count || count > crate::array::MAX_MEMBERS {
        return Err(Error::Unsupported);
    }
    let mut sizes = [0u64; crate::array::MAX_MEMBERS];
    for (member, size) in sizes[..count].iter_mut().enumerate() {
        *size = members.sectors(member);
    }
    let plan = plan(spec, &sizes[..count])?;
    // Erase first, everywhere, and make it durable before any superblock exists.
    for member in 0..count {
        zero(members, member, 0, DATA_OFFSET + ZERO_DATA)?;
        zero(members, member, sizes[member] - TAIL, TAIL)?;
    }
    for member in 0..count {
        members.flush(member)?;
    }
    let mut roles = [ROLE_SPARE; MAX_DEV];
    for (slot, role) in roles[..count].iter_mut().enumerate() {
        *role = slot as u16;
    }
    let events = 0;
    let time = spec.now & 0xff_ffff_ffff; // seconds in the low 40 bits, microseconds (0) above
    let mut map = [0u8; bitmap::CAP];
    let map_bytes = plan.bitmap_sectors as usize * SECTOR;
    bitmap_block(spec, &plan, events, &mut map[..map_bytes]);
    for member in 0..count {
        let sb = Superblock {
            minor: 2,
            feature_map: FEATURE_BITMAP,
            set_uuid: spec.uuid,
            set_name: spec.name,
            ctime: time,
            level: spec.level,
            layout: if spec.level == LEVEL_RAID1 { 0 } else { LEFT_SYMMETRIC },
            size: plan.member_size,
            chunk_sectors: if spec.level == LEVEL_RAID1 { 0 } else { spec.chunk_sectors },
            raid_disks: count as u32,
            bitmap_offset: BITMAP_OFFSET,
            data_offset: DATA_OFFSET,
            data_size: sizes[member] - DATA_OFFSET - TAIL,
            super_offset: SUPER_SECTOR,
            recovery_offset: 0,
            dev_number: member as u32,
            device_uuid: device_uuids[member],
            devflags: 0,
            utime: time,
            events,
            resync_offset: 0,
            max_dev: MAX_DEV as u32,
        };
        let mut block = [0u8; superblock::READ_BYTES];
        let used = sb.build(&roles, &mut block).div_ceil(SECTOR);
        members.write(member, (SUPER_SECTOR as i64 + BITMAP_OFFSET as i64) as u64, &map[..map_bytes])?;
        members.write(member, SUPER_SECTOR, &block[..used * SECTOR])?;
        members.flush(member)?;
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::array::{Array, Step};

    struct Disks(Vec<Vec<u8>>);

    impl Members for Disks {
        fn count(&self) -> usize {
            self.0.len()
        }
        fn sectors(&self, member: usize) -> u64 {
            (self.0[member].len() / SECTOR) as u64
        }
        fn read(&mut self, member: usize, lba: u64, buffer: &mut [u8]) -> Result<(), Error> {
            let at = lba as usize * SECTOR;
            buffer.copy_from_slice(self.0[member].get(at..at + buffer.len()).ok_or(Error::Range)?);
            Ok(())
        }
        fn write(&mut self, member: usize, lba: u64, buffer: &[u8]) -> Result<(), Error> {
            let at = lba as usize * SECTOR;
            self.0[member].get_mut(at..at + buffer.len()).ok_or(Error::Range)?.copy_from_slice(buffer);
            Ok(())
        }
        fn flush(&mut self, _member: usize) -> Result<(), Error> {
            Ok(())
        }
    }

    fn spec(level: i32) -> Spec {
        let mut name = [0u8; 32];
        name[..9].copy_from_slice(b"rack:tank");
        Spec { level, chunk_sectors: 128, name, uuid: [0x5a; 16], now: 1_790_000_000 }
    }

    /// Disks of `sectors` each, filled with a byte pattern (an old disk's contents).
    fn old_disks(count: usize, sectors: usize) -> Disks {
        Disks((0..count).map(|d| (0..sectors * SECTOR).map(|i| (i * 7 + d * 13 + 1) as u8).collect()).collect())
    }

    fn uuids(count: usize) -> Vec<[u8; 16]> {
        (0..count).map(|i| [i as u8 + 1; 16]).collect()
    }

    fn resync(array: &mut Array, disks: &mut Disks) {
        array.start(disks, 1).unwrap();
        array.begin_resync().unwrap();
        loop {
            match array.step(disks, 4096, 1).unwrap() {
                Step::Running(_) => {}
                Step::Done(_) => break,
                Step::Idle => panic!("the resync stopped"),
            }
        }
        array.stop(disks, 1).unwrap();
    }

    #[test]
    fn a_new_raid5_assembles_dirty_resyncs_and_then_holds_data() {
        let mut disks = old_disks(3, 8192 + 64);
        let plan = create(&mut disks, &spec(LEVEL_RAID5), &uuids(3)).unwrap();
        // 8256 - 2048 - 2048 = 4160 sectors, rounded down to 128-sector chunks
        assert_eq!(plan.member_size, 4096);
        assert_eq!(plan.sectors, 8192);
        let mut array = Array::assemble(&mut disks, None).unwrap();
        assert_eq!((array.sectors(), array.level(), array.present(), array.name()), (8192, 5, 3, "rack:tank"));
        assert!(array.dirty(), "parity is not computed until the resync");
        // the array's start reads as zeros, not an old file system
        let mut head = vec![0xeeu8; 4 * SECTOR];
        array.read(&mut disks, 0, &mut head).unwrap();
        assert!(head.iter().all(|&b| b == 0));
        resync(&mut array, &mut disks);
        // every stripe row XORs to zero: parity matches the old data left in the data area
        for row in 0..plan.member_size as usize {
            let at = (DATA_OFFSET as usize + row) * SECTOR;
            let x: Vec<u8> = (0..SECTOR).map(|i| disks.0[0][at + i] ^ disks.0[1][at + i] ^ disks.0[2][at + i]).collect();
            assert!(x.iter().all(|&b| b == 0), "row {row} parity");
        }
        // clean now: assembles clean, takes a write and reads it back, also with a member gone
        let mut array = Array::assemble(&mut disks, None).unwrap();
        assert!(!array.dirty());
        let data: Vec<u8> = (0..64 * SECTOR).map(|i| (i % 251) as u8).collect();
        array.start(&mut disks, 2).unwrap();
        array.write(&mut disks, 1000, &data, 2).unwrap();
        array.stop(&mut disks, 2).unwrap();
        disks.0[1].fill(0); // a member dies (its superblock too)
        let array = Array::assemble(&mut disks, None).unwrap();
        assert!(array.degraded());
        let mut back = vec![0u8; data.len()];
        array.read(&mut disks, 1000, &mut back).unwrap();
        assert_eq!(back, data);
    }

    #[test]
    fn the_head_and_tail_are_wiped_and_the_superblock_is_what_mdadm_writes() {
        let mut disks = old_disks(3, 8192);
        create(&mut disks, &spec(LEVEL_RAID5), &uuids(3)).unwrap();
        for disk in &disks.0 {
            assert!(disk[..SECTOR * 8].iter().all(|&b| b == 0), "MBR and primary GPT gone");
            assert!(disk[disk.len() - TAIL as usize * SECTOR..].iter().all(|&b| b == 0), "backup GPT and v1.0 place gone");
        }
        let (sb, block) = crate::array::find_superblock(&mut disks, 1).unwrap();
        let (_, roles) = Superblock::parse(&block, 2).unwrap();
        assert_eq!((sb.minor, sb.super_offset, sb.data_offset, sb.bitmap_offset, sb.layout, sb.chunk_sectors), (2, 8, 2048, 8, 2, 128));
        assert_eq!((sb.dev_number, sb.resync_offset, sb.feature_map, sb.max_dev), (1, 0, FEATURE_BITMAP, 384));
        assert_eq!((roles.get(0), roles.get(2), roles.get(3)), (Some(0), Some(2), Some(ROLE_SPARE)));
        assert_eq!(sb.device_uuid, [2; 16]);
        // the bitmap: version 4, every bit set, a power-of-two chunk, sync_size = the member's data size
        let map = &disks.0[1][16 * SECTOR..17 * SECTOR];
        assert_eq!(u32::from_le_bytes(map[0..4].try_into().unwrap()), bitmap::MAGIC);
        assert_eq!(u32::from_le_bytes(map[4..8].try_into().unwrap()), 4);
        assert_eq!(u64::from_le_bytes(map[40..48].try_into().unwrap()), sb.size);
        assert!(u32::from_le_bytes(map[52..56].try_into().unwrap()).is_power_of_two());
        assert_eq!(map[bitmap::HEADER], 0xff);
    }

    #[test]
    fn raid1_and_raid6_are_made_too_and_their_resync_makes_them_clean() {
        for (level, count) in [(LEVEL_RAID1, 2), (LEVEL_RAID6, 4)] {
            let mut disks = old_disks(count, 6144);
            create(&mut disks, &spec(level), &uuids(count)).unwrap();
            let mut array = Array::assemble(&mut disks, None).unwrap();
            assert!(array.dirty());
            resync(&mut array, &mut disks);
            assert!(!Array::assemble(&mut disks, None).unwrap().dirty(), "level {level}");
        }
    }

    #[test]
    fn unequal_members_use_the_smallest_and_bad_requests_are_refused() {
        let s = spec(LEVEL_RAID5);
        assert_eq!(plan(&s, &[10_000, 9_000, 12_000]).unwrap().member_size, (9_000 - 4096) / 128 * 128);
        assert_eq!(plan(&s, &[10_000, 10_000]), Err(Error::Unsupported), "RAID 5 wants three");
        assert_eq!(plan(&spec(LEVEL_RAID6), &[10_000; 3]), Err(Error::Unsupported), "RAID 6 wants four");
        assert_eq!(plan(&spec(0), &[10_000; 3]), Err(Error::Unsupported));
        assert_eq!(plan(&s, &[5_000; 3]), Err(Error::TooSmall));
        let odd = Spec { chunk_sectors: 100, ..spec(LEVEL_RAID5) };
        assert_eq!(plan(&odd, &[10_000; 3]), Err(Error::Unsupported));
    }

    #[test]
    fn a_large_member_gets_a_bitmap_chunk_that_fits() {
        // 256 GB members (rack's SSDs): 64 MiB chunks, about 3700 bits
        let p = plan(&spec(LEVEL_RAID5), &[500_118_192; 3]).unwrap();
        assert_eq!(p.bitmap_chunk, 131072);
        assert!(p.bitmap_sectors <= 2);
        // 20 TB: the chunk doubles until the bits fit in `bitmap::CAP`
        let p = plan(&spec(LEVEL_RAID5), &[39_063_650_304; 3]).unwrap();
        assert!(bitmap::HEADER as u64 + p.member_size.div_ceil(p.bitmap_chunk).div_ceil(8) <= bitmap::CAP as u64);
        assert!(p.bitmap_chunk.is_power_of_two());
    }
}
