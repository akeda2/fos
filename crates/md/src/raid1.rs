//! A RAID 1 (mirror) array assembled from md v1.x members, read-only.
//!
//! Assembly follows what the Linux kernel's md does: look for a superblock on every member, keep those of one array
//! (same `set_uuid`), take the highest event count as the truth and use only the members that reached it and whose slot
//! the freshest member's role table lists as active. A member with a lower count missed writes and is left out; one
//! flagged as still recovering is left out too. Reads come from the first usable member and fall back to the next when
//! one fails. Nothing is written, so an array in the middle of a resync (`dirty`) is read but never repaired here.

use crate::superblock::{self, Superblock, FEATURE_BAD_BLOCKS, FEATURE_BITMAP, FEATURE_RECOVERY_OFFSET, LEVEL_RAID1, READ_BYTES, ROLE_FAULTY};
use crate::{Error, Members, SECTOR};

pub const MAX_MEMBERS: usize = 8;

#[derive(Clone, Copy, Debug)]
struct Slot {
    member: usize,
    data_offset: u64,
}

#[derive(Debug)]
pub struct Array {
    slots: [Option<Slot>; MAX_MEMBERS],
    used: usize,
    sectors: u64,
    uuid: [u8; 16],
    name: [u8; 32],
    events: u64,
    raid_disks: u32,
    dirty: bool,
}

/// Finds the superblock on `member`, trying minor versions 1.2, 1.1 and 1.0 in the order mdadm prefers.
pub fn find_superblock<M: Members>(members: &mut M, member: usize) -> Option<(Superblock, [u8; READ_BYTES])> {
    let sectors = members.sectors(member);
    for minor in [2u8, 1, 0] {
        let Some(at) = superblock::super_sector(minor, sectors) else { continue };
        let mut block = [0u8; READ_BYTES];
        // a 1.0 block near the end may have fewer than 8 sectors after it
        let room = ((sectors.saturating_sub(at)) as usize).min(READ_BYTES / SECTOR);
        if room == 0 || members.read(member, at, &mut block[..room * SECTOR]).is_err() {
            continue;
        }
        if let Ok((sb, _)) = Superblock::parse(&block, minor) {
            if sb.super_offset == at {
                return Some((sb, block));
            }
        }
    }
    None
}

impl Array {
    /// Assembles the mirror made of the offered members. With `uuid`, only that array; without, the first one found.
    pub fn assemble<M: Members>(members: &mut M, uuid: Option<[u8; 16]>) -> Result<Array, Error> {
        let mut wanted = uuid;
        let mut found: [Option<(Superblock, [u8; READ_BYTES])>; MAX_MEMBERS] = core::array::from_fn(|_| None);
        let mut any = false;
        for member in 0..members.count().min(MAX_MEMBERS) {
            let Some((sb, block)) = find_superblock(members, member) else { continue };
            if wanted.is_some_and(|u| u != sb.set_uuid) {
                continue;
            }
            wanted = Some(sb.set_uuid);
            any = true;
            found[member] = Some((sb, block));
        }
        if !any {
            return Err(Error::NoMembers);
        }
        let (mut best, mut best_member) = (0u64, usize::MAX);
        for (member, entry) in found.iter().enumerate() {
            if let Some((sb, _)) = entry {
                if sb.level != LEVEL_RAID1 {
                    return Err(Error::Unsupported);
                }
                if sb.feature_map & !(FEATURE_BITMAP | FEATURE_RECOVERY_OFFSET | FEATURE_BAD_BLOCKS) != 0 {
                    return Err(Error::Unsupported);
                }
                if best_member == usize::MAX || sb.events > best {
                    best = sb.events;
                    best_member = member;
                }
            }
        }
        let (lead, lead_block) = found[best_member].as_ref().unwrap();
        let (_, roles) = Superblock::parse(lead_block, lead.minor)?;
        let mut array = Array {
            slots: [None; MAX_MEMBERS],
            used: 0,
            sectors: lead.size,
            uuid: lead.set_uuid,
            name: lead.set_name,
            events: best,
            raid_disks: lead.raid_disks,
            dirty: lead.resync_offset != superblock::SYNCED,
        };
        for (member, entry) in found.iter().enumerate() {
            let Some((sb, _)) = entry else { continue };
            let active = roles.get(sb.dev_number as usize).is_some_and(|role| role < ROLE_FAULTY && (role as u32) < lead.raid_disks);
            let in_step = sb.events == best && sb.feature_map & FEATURE_RECOVERY_OFFSET == 0;
            let fits = sb.data_offset.checked_add(sb.size).is_some_and(|end| end <= members.sectors(member));
            if active && in_step && fits && sb.size == lead.size {
                array.slots[array.used] = Some(Slot { member, data_offset: sb.data_offset });
                array.used += 1;
            }
        }
        if array.used == 0 {
            return Err(Error::Degraded);
        }
        Ok(array)
    }

    /// The array's size in 512-byte sectors.
    pub fn sectors(&self) -> u64 {
        self.sectors
    }
    pub fn uuid(&self) -> [u8; 16] {
        self.uuid
    }
    /// `host:name` as given to `mdadm --name`.
    pub fn name(&self) -> &str {
        let end = self.name.iter().position(|&b| b == 0).unwrap_or(32);
        core::str::from_utf8(&self.name[..end]).unwrap_or("")
    }
    pub fn events(&self) -> u64 {
        self.events
    }
    /// Members in use now, and how many the array has when whole.
    pub fn present(&self) -> usize {
        self.used
    }
    pub fn expected(&self) -> usize {
        self.raid_disks as usize
    }
    pub fn degraded(&self) -> bool {
        self.used < self.raid_disks as usize
    }
    /// True when the freshest superblock says the members were not known to be in step (a resync was running or the
    /// array was not stopped cleanly).
    pub fn dirty(&self) -> bool {
        self.dirty
    }
    /// The member numbers in use, in the order reads try them.
    pub fn used_members(&self) -> impl Iterator<Item = usize> + '_ {
        self.slots[..self.used].iter().flatten().map(|slot| slot.member)
    }

    /// Reads whole sectors from the array, addressed from its first sector.
    pub fn read<M: Members>(&self, members: &mut M, lba: u64, buffer: &mut [u8]) -> Result<(), Error> {
        if buffer.len() % SECTOR != 0 {
            return Err(Error::Range);
        }
        let count = (buffer.len() / SECTOR) as u64;
        if lba.checked_add(count).is_none_or(|end| end > self.sectors) {
            return Err(Error::Range);
        }
        let mut last = Error::Device;
        for slot in self.slots[..self.used].iter().flatten() {
            match members.read(slot.member, slot.data_offset + lba, buffer) {
                Ok(()) => return Ok(()),
                Err(error) => last = error,
            }
        }
        Err(last)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::superblock::SYNCED;

    struct Disks(Vec<Vec<u8>>, Vec<bool>);

    impl Members for Disks {
        fn count(&self) -> usize {
            self.0.len()
        }
        fn sectors(&self, member: usize) -> u64 {
            (self.0[member].len() / SECTOR) as u64
        }
        fn read(&mut self, member: usize, lba: u64, buffer: &mut [u8]) -> Result<(), Error> {
            if self.1[member] {
                return Err(Error::Device);
            }
            let start = lba as usize * SECTOR;
            buffer.copy_from_slice(self.0[member].get(start..start + buffer.len()).ok_or(Error::Range)?);
            Ok(())
        }
    }

    const DISK_SECTORS: usize = 4096;
    const DATA_OFFSET: u64 = 2048;
    const ARRAY_SECTORS: u64 = 1024;

    fn name() -> [u8; 32] {
        let mut name = [0u8; 32];
        name[..8].copy_from_slice(b"fos:data");
        name
    }

    fn sb(minor: u8, slot: u32, events: u64) -> Superblock {
        Superblock {
            minor,
            feature_map: 0,
            set_uuid: [7; 16],
            set_name: name(),
            ctime: 1,
            level: LEVEL_RAID1,
            layout: 0,
            size: ARRAY_SECTORS,
            chunk_sectors: 0,
            raid_disks: 2,
            bitmap_offset: 0,
            data_offset: DATA_OFFSET,
            data_size: ARRAY_SECTORS,
            super_offset: 0,
            recovery_offset: 0,
            dev_number: slot,
            device_uuid: [slot as u8 + 1; 16],
            devflags: 0,
            utime: 2,
            events,
            resync_offset: SYNCED,
            max_dev: 2,
        }
    }

    /// A member holding minor `minor`'s superblock and a recognisable pattern as data.
    fn member(minor: u8, slot: u32, events: u64, roles: &[u16], seed: u8) -> Vec<u8> {
        let mut disk = vec![0u8; DISK_SECTORS * SECTOR];
        let mut block = sb(minor, slot, events);
        let at = superblock::super_sector(minor, DISK_SECTORS as u64).unwrap();
        block.super_offset = at;
        block.build(roles, &mut disk[at as usize * SECTOR..]);
        for sector in 0..ARRAY_SECTORS as usize {
            let start = (DATA_OFFSET as usize + sector) * SECTOR;
            disk[start..start + SECTOR].fill(seed.wrapping_add(sector as u8));
        }
        disk
    }

    #[test]
    fn checksum_round_trips_and_detects_damage() {
        let mut out = [0u8; 512];
        sb(2, 0, 5).build(&[0, 1], &mut out);
        let (parsed, roles) = Superblock::parse(&out, 2).unwrap();
        assert_eq!((parsed.events, parsed.raid_disks, roles.get(1)), (5, 2, Some(1)));
        assert_eq!(parsed.name(), "fos:data");
        out[100] ^= 1;
        assert_eq!(Superblock::parse(&out, 2).err(), Some(Error::NotMd));
    }

    #[test]
    fn assembles_a_mirror_in_every_minor_version() {
        for minor in [0u8, 1, 2] {
            let mut disks = Disks(vec![member(minor, 0, 9, &[0, 1], 1), member(minor, 1, 9, &[0, 1], 1)], vec![false; 2]);
            let array = Array::assemble(&mut disks, None).unwrap();
            assert_eq!((array.sectors(), array.present(), array.degraded(), array.dirty()), (ARRAY_SECTORS, 2, false, false));
            let mut buf = [0u8; 2 * SECTOR];
            array.read(&mut disks, 3, &mut buf).unwrap();
            assert!(buf[..SECTOR].iter().all(|&b| b == 4) && buf[SECTOR..].iter().all(|&b| b == 5), "minor {minor}");
            assert_eq!(array.read(&mut disks, ARRAY_SECTORS - 1, &mut buf), Err(Error::Range));
        }
    }

    #[test]
    fn a_stale_member_is_left_out() {
        let mut stale = member(2, 1, 8, &[0, 1], 99);
        stale[0] = 0;
        let mut disks = Disks(vec![member(2, 0, 9, &[0, 1], 1), stale], vec![false; 2]);
        let array = Array::assemble(&mut disks, None).unwrap();
        assert_eq!((array.present(), array.degraded()), (1, true));
        let mut buf = [0u8; SECTOR];
        array.read(&mut disks, 0, &mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 1));
    }

    #[test]
    fn a_member_marked_faulty_is_left_out() {
        let mut disks = Disks(vec![member(2, 0, 9, &[0, ROLE_FAULTY], 1), member(2, 1, 9, &[0, ROLE_FAULTY], 1)], vec![false; 2]);
        assert_eq!(Array::assemble(&mut disks, None).unwrap().present(), 1);
    }

    #[test]
    fn reads_fall_back_when_a_member_fails() {
        let mut disks = Disks(vec![member(2, 0, 9, &[0, 1], 1), member(2, 1, 9, &[0, 1], 1)], vec![false; 2]);
        let array = Array::assemble(&mut disks, None).unwrap();
        disks.1[0] = true;
        let mut buf = [0u8; SECTOR];
        array.read(&mut disks, 0, &mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 1));
        disks.1[1] = true;
        assert_eq!(array.read(&mut disks, 0, &mut buf), Err(Error::Device));
    }

    #[test]
    fn other_arrays_levels_and_blank_disks_are_not_picked() {
        let mut other = member(2, 0, 9, &[0, 1], 1);
        let mut block = sb(2, 0, 9);
        block.set_uuid = [9; 16];
        block.super_offset = 8;
        block.build(&[0, 1], &mut other[8 * SECTOR..]);
        let blank = vec![0u8; DISK_SECTORS * SECTOR];
        let mut disks = Disks(vec![blank.clone(), other, member(2, 0, 9, &[0, 1], 1)], vec![false; 3]);
        assert_eq!(Array::assemble(&mut disks, Some([7; 16])).unwrap().uuid(), [7; 16]);
        assert_eq!(Array::assemble(&mut Disks(vec![blank], vec![false]), None).err(), Some(Error::NoMembers));
        let mut raid5 = member(2, 0, 9, &[0, 1], 1);
        let mut block = sb(2, 0, 9);
        (block.level, block.super_offset) = (5, 8);
        block.build(&[0, 1], &mut raid5[8 * SECTOR..]);
        assert_eq!(Array::assemble(&mut Disks(vec![raid5], vec![false]), None).err(), Some(Error::Unsupported));
    }
}
