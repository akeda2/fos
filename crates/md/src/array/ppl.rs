//! The RAID 5 partial parity log (PPL), Linux's way to close the write hole without a journal device.
//!
//! A write that updates one chunk of a stripe changes data and parity in two separate member writes; a power cut
//! between them leaves the parity not matching the data, and the day a member dies, the data rebuilt from that parity is
//! wrong, even for chunks the write never touched. The *partial parity* of a write is the XOR of the data the write did
//! not touch (the old parity with the old data taken out). Written to the log first, it makes the parity recomputable
//! whichever of the data writes landed: parity = (the touched data as it is now) XOR (the partial parity).
//!
//! Layout (Linux `drivers/md/raid5-ppl.c`, `include/uapi/linux/raid/md_p.h`): every member has a log area of `size`
//! sectors at `offset` sectors from its superblock (both in the superblock, in the bytes the bitmap offset uses; the
//! two never go together). A log is a 4 KiB header, followed by the partial parity of its entries in order. The header:
//! 512 bytes of 0xff, signature (CRC-32C of the array UUID), 4 zero bytes, generation (u64), entry count, CRC-32C of the
//! header with this field zero, and up to 148 entries of 24 bytes: array sector of the new data, partial parity length,
//! data length, the role holding the parity, CRC-32C of the partial parity. An entry means "the parity of these sectors
//! was being updated": `data_size / pp_size` consecutive data chunks, the same `pp_size`-byte range in each, from the
//! chunk holding `data_sector`. The log lives on the member that holds the parity of the stripe it describes.
//!
//! This code logs one piece (a stretch of one chunk, one data member) at a time, and always at the start of the area:
//! one header with one entry, rewritten for every piece, with a higher generation. It reads whatever Linux wrote (the
//! MULTIPLE_PPLS feature, a ring of headers, included): the newest valid header (a CRC-32C-clean one of this array whose generation does not fall)
//! wins. At the start of a write session the whole area is zeroed and an empty header written, so no old header can be
//! mistaken for a newer one (Linux does the same when it starts an array).
//!
//! Recovery runs on a dirty array (`recover_ppl`): each entry whose partial parity checks out has the parity of its
//! range recomputed, unless one of its data members is missing (that stripe stays as unsafe as without a log). If every
//! entry was fine and there was at least one, the array counts as clean again and needs no resync, like in Linux.

use super::*;
use crate::crc32c;
use crate::superblock::{FEATURE_MULTIPLE_PPLS, FEATURE_PPL};

const HEADER: usize = 4096;
const HEADER_SECTORS: u64 = (HEADER / SECTOR) as u64;
const RESERVED: usize = 512;
const SIGNATURE_AT: usize = RESERVED;
const GENERATION_AT: usize = RESERVED + 8;
const COUNT_AT: usize = RESERVED + 16;
const CHECKSUM_AT: usize = RESERVED + 20;
const ENTRIES_AT: usize = RESERVED + 24;
const ENTRY: usize = 24;
const MAX_ENTRIES: usize = (HEADER - ENTRIES_AT) / ENTRY;
const PP_BYTES: usize = PIECE as usize * SECTOR;

/// Where the logs are and the generation the next header gets.
pub(super) struct Ppl {
    pub offset: i64,
    pub size: u64,
    pub signature: u32,
    pub generation: u64,
}

/// What `recover_ppl` found.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PplReport {
    /// Entries whose partial parity was intact (applied, or skipped because a data member is missing).
    pub entries: u32,
    /// Logs without a valid header and entries that did not check out: the array is not trusted clean after those.
    pub mismatched: u32,
}

fn le32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn le64(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

/// A header with at most one entry: `(data sector, partial parity, parity role)`.
fn header(signature: u32, generation: u64, entry: Option<(u64, &[u8], u16)>) -> [u8; HEADER] {
    let mut out = [0u8; HEADER];
    out[..RESERVED].fill(0xff);
    out[SIGNATURE_AT..SIGNATURE_AT + 4].copy_from_slice(&signature.to_le_bytes());
    out[GENERATION_AT..GENERATION_AT + 8].copy_from_slice(&generation.to_le_bytes());
    if let Some((data_sector, pp, parity)) = entry {
        out[COUNT_AT..COUNT_AT + 4].copy_from_slice(&1u32.to_le_bytes());
        let e = &mut out[ENTRIES_AT..ENTRIES_AT + ENTRY];
        e[0..8].copy_from_slice(&data_sector.to_le_bytes());
        e[8..12].copy_from_slice(&(pp.len() as u32).to_le_bytes());
        e[12..16].copy_from_slice(&(pp.len() as u32).to_le_bytes());
        e[16..20].copy_from_slice(&(parity as u32).to_le_bytes());
        e[20..24].copy_from_slice(&crc32c::crc32c(pp).to_le_bytes());
    }
    let sum = crc32c::crc32c(&out);
    out[CHECKSUM_AT..CHECKSUM_AT + 4].copy_from_slice(&sum.to_le_bytes());
    out
}

impl Array {
    /// Whether the array carries a partial parity log it trusts.
    pub fn has_ppl(&self) -> bool {
        self.ppl.is_some()
    }

    /// Reads the log placement from the superblocks and checks it: the same on every member, whole 4 KiB stripes of
    /// partial parity after the header (at least one piece's worth), clear of the data and the superblock, on the disk.
    /// Anything else leaves `ppl` unset, which keeps the array read-only (`start` refuses).
    pub(super) fn attach_ppl<M: Members>(&mut self, members: &mut M) {
        let mut found: Option<(i64, u64)> = None;
        for slot in self.slots[..self.used].iter().flatten() {
            let raw = u32::from_le_bytes(slot.block[96..100].try_into().unwrap());
            let offset = (raw & 0xffff) as u16 as i16 as i64;
            let data = (raw >> 16) as u64;
            let data = data.saturating_sub(HEADER_SECTORS) / 8 * 8;
            if data < PIECE || found.is_some_and(|(o, _)| o != offset) {
                return;
            }
            let size = data + HEADER_SECTORS;
            if !self.ppl_fits(slot.super_sector as i64 + offset, size, slot.data_offset, slot.super_sector, members.sectors(slot.member)) {
                return;
            }
            found = Some((offset, found.map_or(size, |(_, s)| s.min(size))));
        }
        if let Some((offset, size)) = found {
            let signature = crc32c::crc32c(&self.uuid);
            self.ppl = Some(Ppl { offset, size, signature, generation: 0 });
        }
    }

    fn ppl_fits(&self, start: i64, size: u64, data_offset: u64, super_sector: u64, disk: u64) -> bool {
        let end = start + size as i64;
        let header = (READ_BYTES / SECTOR) as i64;
        let (data_from, data_to) = (data_offset as i64, (data_offset + self.member_size) as i64);
        let super_from = super_sector as i64;
        start >= 0 && end <= disk as i64 && (end <= data_from || start >= data_to) && (end <= super_from || start >= super_from + header)
    }

    fn ppl_area(&self, super_sector: u64) -> Option<u64> {
        self.ppl.as_ref().map(|ppl| (super_sector as i64 + ppl.offset) as u64)
    }

    /// Zeroes a member's log area and writes an empty header, durably.
    fn ppl_clear_member<M: Members>(&self, members: &mut M, member: usize, super_sector: u64) -> Result<(), Error> {
        let (Some(ppl), Some(area)) = (self.ppl.as_ref(), self.ppl_area(super_sector)) else { return Ok(()) };
        let zeros = [0u8; PP_BYTES];
        let mut done = 0;
        while done < ppl.size {
            let run = (ppl.size - done).min(PIECE);
            members.write(member, area + done, &zeros[..run as usize * SECTOR])?;
            done += run;
        }
        members.write(member, area, &header(ppl.signature, 0, None))?;
        members.flush(member)
    }

    /// Starts a write session: every member's log is emptied before the array is marked in use.
    pub(super) fn ppl_start<M: Members>(&mut self, members: &mut M) -> Result<(), Error> {
        for index in 0..self.used {
            let Some((member, super_sector)) = self.slots[index].as_ref().map(|s| (s.member, s.super_sector)) else { continue };
            self.ppl_clear_member(members, member, super_sector)?;
        }
        if let Some(ppl) = self.ppl.as_mut() {
            ppl.generation = 0;
        }
        Ok(())
    }

    /// Prepares a member that is about to be rebuilt: its log must not hold anything from an earlier life.
    pub(super) fn ppl_add_member<M: Members>(&self, members: &mut M, member: usize, super_sector: u64, data_offset: u64) -> Result<(), Error> {
        let Some(ppl) = self.ppl.as_ref() else { return Ok(()) };
        let start = super_sector as i64 + ppl.offset;
        if !self.ppl_fits(start, ppl.size, data_offset, super_sector, members.sectors(member)) {
            return Err(Error::TooSmall);
        }
        self.ppl_clear_member(members, member, super_sector)
    }

    /// Logs the partial parity `pp` of a write to the data sectors from array sector `data_sector` on the member that
    /// holds the parity (`parity` is its role), and returns once it is on the medium: the data may be written after.
    pub(super) fn ppl_log<M: Members>(&mut self, members: &mut M, parity: u16, data_sector: u64, pp: &[u8]) -> Result<(), Error> {
        let (member, super_sector) = self.slot_of_role(parity).map(|s| (s.member, s.super_sector)).ok_or(Error::Device)?;
        let area = self.ppl_area(super_sector).ok_or(Error::Unsupported)?;
        let ppl = self.ppl.as_mut().ok_or(Error::Unsupported)?;
        if pp.is_empty() || pp.len() > PP_BYTES || pp.len() as u64 / SECTOR as u64 > ppl.size - HEADER_SECTORS {
            return Err(Error::Range);
        }
        ppl.generation += 1;
        let mut out = [0u8; HEADER + PP_BYTES];
        out[..HEADER].copy_from_slice(&header(ppl.signature, ppl.generation, Some((data_sector, pp, parity))));
        out[HEADER..HEADER + pp.len()].copy_from_slice(pp);
        members.write(member, area, &out[..HEADER + pp.len()])?;
        members.flush(member)
    }

    /// Logs partial parity `pp` for a write at array sector `v` when the array has a log and the parity member takes
    /// part (a rebuild target or a missing member has nothing to protect). False when the log write failed.
    pub(super) fn ppl_logged<M: Members>(&mut self, members: &mut M, parity: u16, v: u64, pp: &[u8]) -> bool {
        if self.ppl.is_none() || self.slot_of_role(parity).is_none() {
            return true;
        }
        self.ppl_log(members, parity, v, pp).is_ok()
    }

    /// The newest valid header in a member's log: where it sits (sectors from the area start) and its bytes. Linux
    /// scans the area header by header, each next one right after the previous header's partial parity.
    fn ppl_newest<M: Members>(&self, members: &mut M, member: usize, area: u64, size: u64, signature: u32) -> Result<Option<(u64, [u8; HEADER])>, Error> {
        let mut best: Option<(u64, [u8; HEADER])> = None;
        let mut offset = 0u64;
        let mut current = [0u8; HEADER];
        while offset + HEADER_SECTORS < size {
            members.read(member, area + offset, &mut current)?;
            let stored = le32(&current, CHECKSUM_AT);
            current[CHECKSUM_AT..CHECKSUM_AT + 4].fill(0);
            if crc32c::crc32c(&current) != stored || le32(&current, SIGNATURE_AT) != signature {
                break;
            }
            if best.as_ref().is_some_and(|(_, previous)| le64(previous, GENERATION_AT) > le64(&current, GENERATION_AT)) {
                break;
            }
            let count = le32(&current, COUNT_AT) as usize;
            if count > MAX_ENTRIES {
                break;
            }
            let skip: u64 = (0..count).map(|i| le32(&current, ENTRIES_AT + i * ENTRY + 8) as u64 / SECTOR as u64).sum();
            best = Some((offset, current));
            offset += HEADER_SECTORS + skip;
        }
        Ok(best)
    }

    /// Replays the logs of a dirty array: the parity of every logged range is recomputed from the data as it is now and
    /// the partial parity. Runs before the array serves anything (a degraded array would otherwise rebuild data from
    /// parity that no longer matches). When it succeeds for every entry and finds at least one, the array is clean: no
    /// resync is needed. Anything else leaves it dirty. A no-op on a clean array or one without a log.
    pub fn recover_ppl<M: Members>(&mut self, members: &mut M) -> Result<PplReport, Error> {
        let mut report = PplReport::default();
        let Some((offset, size, signature)) = self.ppl.as_ref().map(|p| (p.offset, p.size, p.signature)) else { return Ok(report) };
        if !self.dirty || self.writing {
            return Ok(report);
        }
        for index in 0..self.used {
            let Some((member, super_sector)) = self.slots[index].as_ref().map(|s| (s.member, s.super_sector)) else { continue };
            let area = (super_sector as i64 + offset) as u64;
            let Some((at, head)) = self.ppl_newest(members, member, area, size, signature)? else {
                report.mismatched += 1;
                continue;
            };
            let mut pp_at = area + at + HEADER_SECTORS;
            for i in 0..le32(&head, COUNT_AT) as usize {
                let e = ENTRIES_AT + i * ENTRY;
                let (data_sector, pp_size, data_size) = (le64(&head, e), le32(&head, e + 8) as u64, le32(&head, e + 12) as u64);
                let (parity, stored) = (le32(&head, e + 16), le32(&head, e + 20));
                if pp_size % SECTOR as u64 != 0 || at + HEADER_SECTORS + (pp_at - area - at - HEADER_SECTORS) + pp_size / SECTOR as u64 > size {
                    report.mismatched += 1;
                    break;
                }
                let mut crc = !0u32;
                let mut tmp = [0u8; PP_BYTES];
                let mut done = 0u64;
                while done < pp_size / SECTOR as u64 {
                    let run = (pp_size / SECTOR as u64 - done).min(PIECE);
                    members.read(member, pp_at + done, &mut tmp[..run as usize * SECTOR])?;
                    crc = crc32c::update(crc, &tmp[..run as usize * SECTOR]);
                    done += run;
                }
                if !crc != stored || !self.ppl_entry(members, member, pp_at, data_sector, pp_size, data_size, parity)? {
                    report.mismatched += 1;
                } else {
                    report.entries += 1;
                }
                pp_at += pp_size / SECTOR as u64;
            }
            members.flush(member)?;
        }
        if self.resync == 0 && report.entries > 0 && report.mismatched == 0 {
            self.dirty = false;
        }
        Ok(report)
    }

    /// Recomputes the parity of one entry's range. Returns false for an entry that makes no sense for this array.
    #[allow(clippy::too_many_arguments)]
    fn ppl_entry<M: Members>(&self, members: &mut M, log_member: usize, pp_at: u64, first: u64, pp_size: u64, data_size: u64, parity: u32) -> Result<bool, Error> {
        let (chunk, data_disks_all) = (self.chunk, self.raid_disks as u64 - 1);
        let (disks, strip, last) = if pp_size / (SECTOR as u64) < chunk {
            let disks = if pp_size > 0 { data_size / pp_size } else { data_disks_all };
            if disks == 0 {
                return Ok(false);
            }
            let strip = if pp_size > 0 { pp_size / SECTOR as u64 } else { data_size / SECTOR as u64 / disks };
            (disks, strip, first + (disks - 1) * chunk + strip)
        } else {
            (data_disks_all, chunk, first + data_size / SECTOR as u64)
        };
        if disks > data_disks_all || strip == 0 || strip > chunk || first + strip > self.sectors {
            return Ok(false);
        }
        let block = if first % 8 == 0 && last % 8 == 0 && strip % 8 == 0 { 8 } else { 1 };
        let bytes = block as usize * SECTOR;
        let mut i = 0;
        while i < strip {
            let (mut sum, mut tmp) = ([0u8; 8 * SECTOR], [0u8; 8 * SECTOR]);
            let (mut update, mut missing) = (false, false);
            for d in 0..disks {
                let r = first + i + d * chunk;
                if r >= last {
                    continue;
                }
                update = true;
                let (_, role, _, at) = self.locate5(r);
                let Some((member, base)) = self.usable(role, 0) else {
                    missing = true;
                    break;
                };
                members.read(member, base + at, &mut tmp[..bytes])?;
                xor(&mut sum[..bytes], &tmp[..bytes]);
            }
            if update && !missing {
                if pp_size > 0 {
                    members.read(log_member, pp_at + i, &mut tmp[..bytes])?;
                    xor(&mut sum[..bytes], &tmp[..bytes]);
                }
                let (_, _, role, at) = self.locate5(first + i);
                let Some(slot) = self.slot_of_role(role).filter(|s| role as u32 == parity && s.member == log_member) else { return Ok(false) };
                members.write(slot.member, slot.data_offset + at, &sum[..bytes])?;
            }
            i += block;
        }
        Ok(true)
    }

    /// Whether the superblocks say the array uses a log (whether or not this code could place it).
    pub(super) fn wants_ppl(features: u32) -> bool {
        features & (FEATURE_PPL | FEATURE_MULTIPLE_PPLS) != 0
    }
}
