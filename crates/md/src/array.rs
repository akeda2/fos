//! An md array assembled from v1.x members: RAID 1 (mirror), RAID 0 (stripe) and RAID 10 in the "near" layout.
//!
//! Assembly follows what the Linux kernel's md does: look for a superblock on every member, keep those of one array
//! (same `set_uuid`), take the highest event count as the truth and use only the members that reached it and whose slot
//! the freshest member's role table lists as active. A member with a lower count missed writes and is left out; one
//! flagged as still recovering is left out too. Reads come from the first usable member and fall back to the next when
//! one fails. An array in the middle of a resync (`dirty`) is read but not repaired by reading.
//!
//! RAID 0 and RAID 10 spread the array over `chunk` sectors at a time: chunk `k` of the array is copy `c` (0..`near`) at
//! position `p = k * near + c`, which is member role `p % raid_disks`, row `p / raid_disks`, so physical sector
//! `row * chunk + offset` on that member (the same arithmetic as Linux's `raid10.c`; RAID 0 is `near = 1`). A RAID 0
//! has no redundancy and no resync, so it is never marked dirty and a member that fails a write fails the write. RAID 10
//! needs one copy of every chunk: a member that fails is dropped as long as that holds.

use crate::superblock::{self, Superblock, FEATURE_BAD_BLOCKS, FEATURE_BITMAP, FEATURE_RECOVERY_OFFSET, LEVEL_RAID0, LEVEL_RAID1, LEVEL_RAID10, READ_BYTES, ROLE_FAULTY, SYNCED};
use crate::{Error, Members, SECTOR};

pub const MAX_MEMBERS: usize = 8;

/// A member in use: where its data starts and its superblock as read, patched in place when the array writes.
struct Slot {
    member: usize,
    data_offset: u64,
    dev_number: u32,
    /// The position in the array (what the role table says for `dev_number`); the stripe arithmetic uses it.
    role: u16,
    super_sector: u64,
    block: [u8; READ_BYTES],
}

pub struct Array {
    slots: [Option<Slot>; MAX_MEMBERS],
    used: usize,
    sectors: u64,
    uuid: [u8; 16],
    name: [u8; 32],
    events: u64,
    raid_disks: u32,
    dirty: bool,
    /// Open for writing: the superblocks say the array is in use (not clean) until `stop`.
    writing: bool,
    /// What the superblocks carry as `resync_offset`: 0 while in use, `SYNCED` when stopped clean.
    resync: u64,
    /// Slots (`dev_number`s) that failed since assembly; the survivors' role tables say so.
    faulty: [u32; MAX_MEMBERS],
    faulty_count: usize,
    features: u32,
    level: i32,
    /// Chunk size in sectors (RAID 0 and 10), and copies of each chunk (1 for RAID 0, `raid_disks` for RAID 1).
    chunk: u64,
    near: u32,
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

/// Chunk size, copies per chunk and array size in sectors for the freshest superblock; Unsupported for layouts this code
/// does not read (RAID 10 "far" and "offset", too many members, a RAID 0 or 10 with no chunk size).
fn geometry(lead: &Superblock) -> Result<(u64, u32, u64), Error> {
    let chunk = lead.chunk_sectors as u64;
    let disks = lead.raid_disks;
    if disks as usize > MAX_MEMBERS {
        return Err(Error::Unsupported);
    }
    match lead.level {
        LEVEL_RAID1 => Ok((0, disks, lead.size)),
        LEVEL_RAID0 if chunk > 0 && disks > 0 => Ok((chunk, 1, lead.size / chunk * chunk * disks as u64)),
        LEVEL_RAID10 => {
            let (near, far, offset) = (lead.layout & 0xff, (lead.layout >> 8) & 0xff, lead.layout & 0x1_0000);
            if chunk == 0 || far != 1 || offset != 0 || near == 0 || near > disks {
                return Err(Error::Unsupported);
            }
            Ok((chunk, near, lead.size / chunk * disks as u64 / near as u64 * chunk))
        }
        _ => Err(Error::Unsupported),
    }
}

fn gcd(a: u32, b: u32) -> u32 {
    if b == 0 { a } else { gcd(b, a % b) }
}

impl Array {
    /// Assembles the array made of the offered members. With `uuid`, only that array; without, the first one found.
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
                if !matches!(sb.level, LEVEL_RAID0 | LEVEL_RAID1 | LEVEL_RAID10) {
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
        let (chunk, near, sectors) = geometry(lead)?;
        let mut array = Array {
            slots: core::array::from_fn(|_| None),
            used: 0,
            sectors,
            uuid: lead.set_uuid,
            name: lead.set_name,
            events: best,
            raid_disks: lead.raid_disks,
            dirty: lead.level != LEVEL_RAID0 && lead.resync_offset != SYNCED,
            writing: false,
            resync: lead.resync_offset,
            faulty: [0; MAX_MEMBERS],
            faulty_count: 0,
            features: lead.feature_map,
            level: lead.level,
            chunk,
            near,
        };
        for (member, entry) in found.iter().enumerate() {
            let Some((sb, block)) = entry else { continue };
            let active = roles.get(sb.dev_number as usize).is_some_and(|role| role < ROLE_FAULTY && (role as u32) < lead.raid_disks);
            let in_step = sb.events == best && sb.feature_map & FEATURE_RECOVERY_OFFSET == 0;
            let fits = sb.data_offset.checked_add(sb.size).is_some_and(|end| end <= members.sectors(member));
            if active && in_step && fits && sb.size == lead.size {
                let super_sector = superblock::super_sector(sb.minor, members.sectors(member)).unwrap_or(0);
                let role = roles.get(sb.dev_number as usize).unwrap_or(0);
                array.slots[array.used] = Some(Slot { member, data_offset: sb.data_offset, dev_number: sb.dev_number, role, super_sector, block: *block });
                array.used += 1;
            }
        }
        if array.used == 0 || (array.level != LEVEL_RAID1 && !array.covered(array.role_mask())) {
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
    /// Whether the array is open for writing (between `start` and `stop`).
    pub fn is_writing(&self) -> bool {
        self.writing
    }
    /// 0, 1 or 10.
    pub fn level(&self) -> i32 {
        self.level
    }
    pub fn chunk_sectors(&self) -> u64 {
        self.chunk
    }

    /// The member numbers in use, in the order reads try them.
    pub fn used_members(&self) -> impl Iterator<Item = usize> + '_ {
        self.slots[..self.used].iter().flatten().map(|slot| slot.member)
    }

    /// The roles of the members in use, one bit each.
    fn role_mask(&self) -> u32 {
        self.slots[..self.used].iter().flatten().filter(|slot| slot.role < 32).fold(0, |mask, slot| mask | 1 << slot.role)
    }

    /// Whether every chunk has a copy among the members whose roles are in `mask`. The copies of a chunk are `near`
    /// consecutive roles starting at a multiple of `gcd(near, raid_disks)`, wrapping; a RAID 0 needs every member.
    fn covered(&self, mask: u32) -> bool {
        let n = self.raid_disks;
        if self.level == LEVEL_RAID1 {
            return mask != 0;
        }
        (0..n).step_by(gcd(self.near, n) as usize).all(|start| (0..self.near).any(|c| mask & (1 << ((start + c) % n)) != 0))
    }

    /// Where array sector `v` lives in a striped array: sectors left in its chunk, and each copy as
    /// (role, sector from the start of that member's data).
    fn locate(&self, v: u64) -> (u64, [(u16, u64); MAX_MEMBERS], usize) {
        let mut copies = [(0u16, 0u64); MAX_MEMBERS];
        let (k, offset) = (v / self.chunk, v % self.chunk);
        let n = self.raid_disks as u64;
        for c in 0..self.near as u64 {
            let position = k * self.near as u64 + c;
            copies[c as usize] = ((position % n) as u16, position / n * self.chunk + offset);
        }
        (self.chunk - offset, copies, self.near as usize)
    }

    fn slot_of_role(&self, role: u16) -> Option<&Slot> {
        self.slots[..self.used].iter().flatten().find(|slot| slot.role == role)
    }

    fn read_striped<M: Members>(&self, members: &mut M, lba: u64, buffer: &mut [u8]) -> Result<(), Error> {
        let count = (buffer.len() / SECTOR) as u64;
        let mut done = 0;
        while done < count {
            let (run, copies, n) = self.locate(lba + done);
            let run = run.min(count - done);
            let piece = &mut buffer[done as usize * SECTOR..(done + run) as usize * SECTOR];
            let mut last = Error::Device;
            let mut got = false;
            for &(role, at) in &copies[..n] {
                let Some(slot) = self.slot_of_role(role) else { continue };
                match members.read(slot.member, slot.data_offset + at, piece) {
                    Ok(()) => {
                        got = true;
                        break;
                    }
                    Err(error) => last = error,
                }
            }
            if !got {
                return Err(last);
            }
            done += run;
        }
        Ok(())
    }

    /// Writes a striped array. A member that fails is dropped when every chunk keeps a copy without it; otherwise it
    /// stays (a RAID 0 never drops one) and the write fails.
    fn write_striped<M: Members>(&mut self, members: &mut M, lba: u64, buffer: &[u8], now: u64) -> Result<(), Error> {
        let count = (buffer.len() / SECTOR) as u64;
        let (mut done, mut failed_roles, mut lost) = (0, 0u32, false);
        while done < count {
            let (run, copies, n) = self.locate(lba + done);
            let run = run.min(count - done);
            let piece = &buffer[done as usize * SECTOR..(done + run) as usize * SECTOR];
            let mut took = 0;
            for &(role, at) in &copies[..n] {
                let Some(slot) = self.slot_of_role(role) else { continue };
                match members.write(slot.member, slot.data_offset + at, piece) {
                    Ok(()) => took += 1,
                    Err(_) => failed_roles |= 1 << role,
                }
            }
            if took == 0 {
                lost = true;
                break;
            }
            done += run;
        }
        let mut failed = [false; MAX_MEMBERS];
        let mut mask = self.role_mask();
        for index in 0..self.used {
            let role = self.slots[index].as_ref().unwrap().role;
            if failed_roles & (1 << role) != 0 {
                let without = mask & !(1 << role);
                if self.covered(without) {
                    failed[index] = true;
                    mask = without;
                } else {
                    lost = true;
                }
            }
        }
        if self.drop_failed(&failed) {
            self.commit(members, now)?;
        }
        if lost { Err(Error::Device) } else { Ok(()) }
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
        if self.level != LEVEL_RAID1 {
            return self.read_striped(members, lba, buffer);
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

    /// Opens the array for writing: every member's superblock is rewritten with the event count one higher and
    /// `resync_offset` 0 ("not known to be in step"), and flushed, before any data write may follow. A crash after
    /// this makes Linux resync the whole array (there is no write-intent bitmap to narrow it); a clean `stop` marks it
    /// in step again. `now` is the time in seconds since 1970 (0 if unknown). Refused for arrays with a bitmap or a
    /// bad-block log, which this code would leave stale (create with `mdadm --bitmap=none`).
    pub fn start<M: Members>(&mut self, members: &mut M, now: u64) -> Result<(), Error> {
        if self.writing {
            return Err(Error::NotWritable);
        }
        if self.features & (FEATURE_BITMAP | FEATURE_BAD_BLOCKS) != 0 {
            return Err(Error::Unsupported);
        }
        if self.level == LEVEL_RAID0 {
            self.writing = true;
            return Ok(());
        }
        if !self.dirty {
            self.resync = 0;
        }
        self.writing = true;
        self.commit(members, now)
    }

    /// Writes whole sectors to every member in use. A member whose write fails is dropped: the others' superblocks
    /// are rewritten (flushed) saying so, with a higher event count, before this returns, so the dropped member can
    /// never be mistaken for current again. It fails only when no member took the data.
    pub fn write<M: Members>(&mut self, members: &mut M, lba: u64, buffer: &[u8], now: u64) -> Result<(), Error> {
        if !self.writing {
            return Err(Error::NotWritable);
        }
        if buffer.len() % SECTOR != 0 {
            return Err(Error::Range);
        }
        let count = (buffer.len() / SECTOR) as u64;
        if lba.checked_add(count).is_none_or(|end| end > self.sectors) {
            return Err(Error::Range);
        }
        if self.level != LEVEL_RAID1 {
            return self.write_striped(members, lba, buffer, now);
        }
        let mut failed = [false; MAX_MEMBERS];
        let mut done = 0;
        for (index, slot) in self.slots[..self.used].iter().flatten().enumerate() {
            match members.write(slot.member, slot.data_offset + lba, buffer) {
                Ok(()) => done += 1,
                Err(_) => failed[index] = true,
            }
        }
        if done == 0 {
            return Err(Error::Device);
        }
        if self.drop_failed(&failed) {
            self.commit(members, now)?;
        }
        Ok(())
    }

    /// Makes everything written so far durable on every member in use (members that fail to flush are dropped).
    pub fn flush<M: Members>(&mut self, members: &mut M, now: u64) -> Result<(), Error> {
        let mut failed = [false; MAX_MEMBERS];
        let mut done = 0;
        for (index, slot) in self.slots[..self.used].iter().flatten().enumerate() {
            match members.flush(slot.member) {
                Ok(()) => done += 1,
                Err(_) => failed[index] = true,
            }
        }
        if done == 0 {
            return Err(Error::Device);
        }
        if self.drop_failed(&failed) {
            self.commit(members, now)?;
        }
        Ok(())
    }

    /// Closes the array: flushes the data, then records "in step" (`resync_offset` back to `SYNCED`, one more event)
    /// in every superblock. An array that was already dirty when assembled stays dirty: nothing here resyncs it.
    pub fn stop<M: Members>(&mut self, members: &mut M, now: u64) -> Result<(), Error> {
        if !self.writing {
            return Err(Error::NotWritable);
        }
        self.flush(members, now)?;
        self.writing = false;
        if self.level == LEVEL_RAID0 {
            return Ok(());
        }
        if !self.dirty {
            self.resync = SYNCED;
        }
        self.commit(members, now)
    }

    /// Removes the members flagged in `failed` (indexes into the used slots) and remembers their slots as faulty.
    fn drop_failed(&mut self, failed: &[bool; MAX_MEMBERS]) -> bool {
        let mut any = false;
        for index in (0..self.used).rev() {
            if failed[index] {
                let slot = self.slots[index].take().unwrap();
                self.faulty[self.faulty_count] = slot.dev_number;
                self.faulty_count += 1;
                self.slots[index..self.used].rotate_left(1);
                self.used -= 1;
                any = true;
            }
        }
        any
    }

    /// Writes every used member's superblock with a higher event count, the current `resync_offset` and the faulty
    /// slots marked, and flushes it. A member that cannot take it is dropped, which needs another round (the survivors
    /// must record the loss), so this repeats until a round loses nobody.
    fn commit<M: Members>(&mut self, members: &mut M, now: u64) -> Result<(), Error> {
        loop {
            if self.used == 0 {
                return Err(Error::Device);
            }
            self.events += 1;
            let (events, resync, faulty, faulty_count) = (self.events, self.resync, self.faulty, self.faulty_count);
            let mut failed = [false; MAX_MEMBERS];
            for (index, slot) in self.slots[..self.used].iter_mut().flatten().enumerate() {
                let block = &mut slot.block;
                superblock::set_events(block, events);
                superblock::set_utime(block, now);
                superblock::set_resync(block, resync);
                let max_dev = u32::from_le_bytes(block[220..224].try_into().unwrap()) as usize;
                for &dev in &faulty[..faulty_count] {
                    if (dev as usize) < max_dev {
                        superblock::set_role(block, dev as usize, ROLE_FAULTY);
                    }
                }
                let sectors = superblock::reseal(block).div_ceil(SECTOR);
                let written = members.write(slot.member, slot.super_sector, &block[..sectors * SECTOR]);
                failed[index] = written.is_err() || members.flush(slot.member).is_err();
            }
            if !self.drop_failed(&failed) {
                return Ok(());
            }
        }
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

    /// Disks that take writes, can be told to fail them, and log every operation in order.
    struct Rw {
        disks: Disks,
        write_fail: Vec<bool>,
        log: Vec<String>,
    }

    impl Rw {
        fn new(disks: Vec<Vec<u8>>) -> Rw {
            let n = disks.len();
            Rw { disks: Disks(disks, vec![false; n]), write_fail: vec![false; n], log: Vec::new() }
        }
    }

    impl Members for Rw {
        fn count(&self) -> usize {
            self.disks.count()
        }
        fn sectors(&self, member: usize) -> u64 {
            self.disks.sectors(member)
        }
        fn read(&mut self, member: usize, lba: u64, buffer: &mut [u8]) -> Result<(), Error> {
            self.disks.read(member, lba, buffer)
        }
        fn write(&mut self, member: usize, lba: u64, buffer: &[u8]) -> Result<(), Error> {
            if self.write_fail[member] {
                return Err(Error::Device);
            }
            self.log.push(format!("w{member}@{lba}"));
            let start = lba as usize * SECTOR;
            self.disks.0[member][start..start + buffer.len()].copy_from_slice(buffer);
            Ok(())
        }
        fn flush(&mut self, member: usize) -> Result<(), Error> {
            self.log.push(format!("f{member}"));
            Ok(())
        }
    }

    fn super_of(rw: &mut Rw, member: usize) -> Superblock {
        find_superblock(rw, member).unwrap().0
    }

    fn pair() -> Rw {
        Rw::new(vec![member(2, 0, 10, &[0, 1], 1), member(2, 1, 10, &[0, 1], 1)])
    }

    #[test]
    fn a_write_marks_dirty_first_and_clean_last() {
        let mut rw = pair();
        let mut array = Array::assemble(&mut rw, None).unwrap();
        array.start(&mut rw, 1000).unwrap();
        for m in 0..2 {
            let sb = super_of(&mut rw, m);
            assert_eq!((sb.events, sb.resync_offset, sb.utime), (11, 0, 1000), "member {m}");
        }
        let first_data = rw.log.len();
        array.write(&mut rw, 5, &[0xab; 2 * SECTOR], 1001).unwrap();
        // the superblock writes and flushes (8 sectors in: 1.2) all precede the first data write
        assert!(rw.log[..first_data].iter().all(|op| !op.contains(&format!("@{DATA_OFFSET}")) && !op.contains("@2053")));
        assert_eq!(&rw.log[first_data..], &[format!("w0@{}", DATA_OFFSET + 5), format!("w1@{}", DATA_OFFSET + 5)]);
        array.stop(&mut rw, 1002).unwrap();
        for m in 0..2 {
            let sb = super_of(&mut rw, m);
            assert_eq!((sb.events, sb.resync_offset), (12, SYNCED), "member {m}");
        }
        // the data is on both members and a fresh assembly sees a clean, whole array
        let again = Array::assemble(&mut rw, None).unwrap();
        assert!(!again.dirty() && !again.degraded());
        let mut buf = [0u8; 2 * SECTOR];
        for m in 0..2 {
            rw.disks.0[m].len();
            again.read(&mut rw, 5, &mut buf).unwrap();
            assert!(buf.iter().all(|&b| b == 0xab));
        }
    }

    #[test]
    fn a_crash_between_start_and_stop_leaves_the_array_dirty() {
        let mut rw = pair();
        let mut array = Array::assemble(&mut rw, None).unwrap();
        array.start(&mut rw, 1).unwrap();
        array.write(&mut rw, 0, &[1; SECTOR], 1).unwrap();
        let after_crash = Array::assemble(&mut rw, None).unwrap();
        assert!(after_crash.dirty() && !after_crash.degraded());
        // writing to a dirty array keeps it dirty (this code never resyncs)
        let mut again = after_crash;
        again.start(&mut rw, 2).unwrap();
        again.stop(&mut rw, 3).unwrap();
        assert!(Array::assemble(&mut rw, None).unwrap().dirty());
    }

    #[test]
    fn a_failing_member_is_dropped_and_recorded_before_the_write_returns() {
        let mut rw = pair();
        let mut array = Array::assemble(&mut rw, None).unwrap();
        array.start(&mut rw, 1).unwrap();
        rw.write_fail[1] = true;
        array.write(&mut rw, 0, &[7; SECTOR], 2).unwrap();
        assert_eq!((array.present(), array.degraded()), (1, true));
        let sb = super_of(&mut rw, 0);
        assert_eq!(sb.events, 12);
        let block = find_superblock(&mut rw, 0).unwrap().1;
        let (_, roles) = Superblock::parse(&block, 2).unwrap();
        assert_eq!((roles.get(0), roles.get(1)), (Some(0), Some(ROLE_FAULTY)));
        array.stop(&mut rw, 3).unwrap();
        // the dropped member comes back later: its old superblock is stale, so it stays out
        rw.write_fail[1] = false;
        let again = Array::assemble(&mut rw, None).unwrap();
        assert_eq!((again.present(), again.events()), (1, 13));
        let mut buf = [0u8; SECTOR];
        again.read(&mut rw, 0, &mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 7));
    }

    #[test]
    fn losing_a_member_at_the_superblock_update_is_handled() {
        let mut rw = pair();
        let mut array = Array::assemble(&mut rw, None).unwrap();
        rw.write_fail[0] = true;
        array.start(&mut rw, 1).unwrap();
        assert_eq!(array.present(), 1);
        assert_eq!(array.used_members().collect::<Vec<_>>(), vec![1]);
        // member 0 never got the dirty mark; member 1 records it as faulty and is ahead by two events
        assert_eq!(super_of(&mut rw, 1).events, 12);
        rw.write_fail[1] = true;
        assert_eq!(array.write(&mut rw, 0, &[1; SECTOR], 2), Err(Error::Device));
    }

    #[test]
    fn writes_need_start_and_refuse_bitmaps() {
        let mut rw = pair();
        let mut array = Array::assemble(&mut rw, None).unwrap();
        assert_eq!(array.write(&mut rw, 0, &[0; SECTOR], 1), Err(Error::NotWritable));
        assert_eq!(array.stop(&mut rw, 1), Err(Error::NotWritable));
        array.start(&mut rw, 1).unwrap();
        assert_eq!(array.start(&mut rw, 1), Err(Error::NotWritable));
        assert_eq!(array.write(&mut rw, ARRAY_SECTORS, &[0; SECTOR], 1), Err(Error::Range));
        assert_eq!(array.write(&mut rw, 0, &[0; 100], 1), Err(Error::Range));

        let mut bitmapped = pair();
        for m in 0..2 {
            let mut block = sb(2, m as u32, 10);
            block.feature_map = superblock::FEATURE_BITMAP;
            block.super_offset = 8;
            block.build(&[0, 1], &mut bitmapped.disks.0[m][8 * SECTOR..]);
        }
        let mut array = Array::assemble(&mut bitmapped, None).unwrap();
        assert_eq!(array.start(&mut bitmapped, 1), Err(Error::Unsupported));
    }

    /// `disks` members of a striped array: `level`, `layout`, chunk of 8 sectors, member data 1024 sectors.
    fn striped(level: i32, layout: u32, disks: u32) -> Rw {
        let roles: Vec<u16> = (0..disks as u16).collect();
        let members = (0..disks)
            .map(|slot| {
                let mut disk = vec![0u8; DISK_SECTORS * SECTOR];
                let mut block = sb(2, slot, 10);
                block.level = level;
                block.layout = layout;
                block.chunk_sectors = 8;
                block.raid_disks = disks;
                block.max_dev = disks;
                block.super_offset = 8;
                block.build(&roles, &mut disk[8 * SECTOR..]);
                disk
            })
            .collect();
        Rw::new(members)
    }

    fn pattern(sectors: u64) -> Vec<u8> {
        (0..sectors as usize * SECTOR).map(|i| (i / SECTOR) as u8 ^ (i % 251) as u8).collect()
    }

    fn sector_of(rw: &Rw, member: usize, sector: u64) -> &[u8] {
        let at = (DATA_OFFSET + sector) as usize * SECTOR;
        &rw.disks.0[member][at..at + SECTOR]
    }

    #[test]
    fn raid0_stripes_chunks_across_members() {
        let mut rw = striped(LEVEL_RAID0, 0, 3);
        let mut array = Array::assemble(&mut rw, None).unwrap();
        assert_eq!((array.level(), array.sectors()), (LEVEL_RAID0, 3 * 1024));
        array.start(&mut rw, 1).unwrap();
        let data = pattern(60);
        array.write(&mut rw, 5, &data, 1).unwrap();
        // array sector v: chunk v/8 on member (v/8)%3, row (v/8)/3
        for v in 5..65u64 {
            let (k, off) = (v / 8, v % 8);
            let want = &data[(v - 5) as usize * SECTOR..][..SECTOR];
            assert_eq!(sector_of(&rw, (k % 3) as usize, k / 3 * 8 + off), want, "sector {v}");
        }
        let mut back = vec![0u8; 60 * SECTOR];
        array.read(&mut rw, 5, &mut back).unwrap();
        assert_eq!(back, data);
        let before = rw.log.len();
        array.stop(&mut rw, 2).unwrap();
        // no superblock traffic for a RAID 0: stopping only flushes
        assert!(rw.log[before..].iter().all(|op| op.starts_with('f')));
        assert_eq!(super_of(&mut rw, 0).events, 10);
    }

    #[test]
    fn raid0_needs_every_member_and_never_drops_one() {
        let mut rw = striped(LEVEL_RAID0, 0, 3);
        rw.disks.0.pop();
        assert_eq!(Array::assemble(&mut rw, None).err(), Some(Error::Degraded));
        let mut rw = striped(LEVEL_RAID0, 0, 2);
        let mut array = Array::assemble(&mut rw, None).unwrap();
        array.start(&mut rw, 1).unwrap();
        rw.write_fail[1] = true;
        assert_eq!(array.write(&mut rw, 0, &pattern(16), 1), Err(Error::Device));
        assert_eq!(array.present(), 2);
    }

    #[test]
    fn raid10_near2_puts_copies_on_neighbouring_members() {
        let mut rw = striped(LEVEL_RAID10, 0x102, 4);
        let mut array = Array::assemble(&mut rw, None).unwrap();
        assert_eq!(array.sectors(), 2 * 1024);
        array.start(&mut rw, 1).unwrap();
        let data = pattern(32);
        array.write(&mut rw, 0, &data, 1).unwrap();
        // chunk 0 on members 0 and 1, chunk 1 on 2 and 3, chunk 2 on 0 and 1 again one row down
        for (chunk, members, row) in [(0usize, [0, 1], 0u64), (1, [2, 3], 0), (2, [0, 1], 1), (3, [2, 3], 1)] {
            for m in members {
                assert_eq!(sector_of(&rw, m, row * 8), &data[chunk * 8 * SECTOR..][..SECTOR], "chunk {chunk} member {m}");
            }
        }
    }

    #[test]
    fn raid10_survives_one_loss_per_pair_and_refuses_a_lost_pair() {
        let mut rw = striped(LEVEL_RAID10, 0x102, 4);
        let mut array = Array::assemble(&mut rw, None).unwrap();
        array.start(&mut rw, 1).unwrap();
        let data = pattern(64);
        array.write(&mut rw, 0, &data, 1).unwrap();
        // member 1 fails: still whole thanks to member 0
        rw.write_fail[1] = true;
        array.write(&mut rw, 0, &data, 2).unwrap();
        assert_eq!((array.present(), array.degraded()), (3, true));
        // member 3 (the other pair) too
        rw.write_fail[3] = true;
        array.write(&mut rw, 0, &data, 3).unwrap();
        assert_eq!(array.present(), 2);
        // losing member 0 as well would lose chunks: the write fails and the member stays
        rw.write_fail[0] = true;
        assert_eq!(array.write(&mut rw, 0, &data, 4), Err(Error::Device));
        assert_eq!(array.present(), 2);
        let mut back = vec![0u8; 64 * SECTOR];
        array.read(&mut rw, 0, &mut back).unwrap();
        assert_eq!(back, data);

        // the survivors recorded the losses, so a fresh assemble leaves the dead members out
        let mut again = Array::assemble(&mut rw, None).unwrap();
        assert_eq!(again.present(), 2);
        let mut back = vec![0u8; 64 * SECTOR];
        again.read(&mut rw, 0, &mut back).unwrap();
        assert_eq!(back, data);
    }

    #[test]
    fn raid10_three_members_near2_and_unsupported_layouts() {
        let mut rw = striped(LEVEL_RAID10, 0x102, 3);
        let mut array = Array::assemble(&mut rw, None).unwrap();
        assert_eq!(array.sectors(), 1024 * 3 / 2 / 8 * 8);
        array.start(&mut rw, 1).unwrap();
        let data = pattern(24);
        array.write(&mut rw, 0, &data, 1).unwrap();
        // chunk 1 is positions 2 and 3: member 2 row 0, member 0 row 1
        assert_eq!(sector_of(&rw, 2, 0), &data[8 * SECTOR..][..SECTOR]);
        assert_eq!(sector_of(&rw, 0, 8), &data[8 * SECTOR..][..SECTOR]);
        rw.write_fail[0] = true;
        array.write(&mut rw, 0, &data, 2).unwrap();
        rw.write_fail[1] = true;
        assert_eq!(array.write(&mut rw, 0, &data, 3), Err(Error::Device));

        for layout in [0x201, 0x1_0102] {
            let mut rw = striped(LEVEL_RAID10, layout, 4);
            assert_eq!(Array::assemble(&mut rw, None).err(), Some(Error::Unsupported));
        }
    }
}
