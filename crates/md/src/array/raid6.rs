//! RAID 6 for `Array`: two syndromes per stripe row, P (the XOR of the data) and Q (the sum of g^j times the data over
//! GF(2^8), `crate::gf`), so any two members of a row can be lost.
//!
//! Where things sit (Linux `raid5_compute_sector`, level 6): P is on the member that RAID 5 would give parity to, Q on the
//! next one (wrapping). The data disks, in the order that gives their syndrome index j, are the members that follow Q
//! around the ring: role (q + 1 + j) % raid_disks. Symmetric layouts also put chunk i of the row there; asymmetric ones
//! number the chunks over the members in order, skipping P and Q (so chunk order and syndrome order differ).
//! Everything works on one row's piece at a time: all members are read into `Bufs`, whatever is missing is rebuilt from
//! the rest, and P and Q are recomputed from the data.

use super::*;
use crate::gf;

/// Sectors per piece: the buffers hold one piece of every member.
pub(super) const PIECE6: u64 = 8;
const PBYTES: usize = PIECE6 as usize * SECTOR;
type Bufs = [[u8; PBYTES]; MAX_MEMBERS];

impl Array {
    /// The sectors left in the chunk, the data role, the P and Q roles and the member sector of array sector `v`.
    pub(super) fn locate6(&self, v: u64) -> (u64, u16, u16, u16, u64) {
        let n = self.raid_disks as u64;
        let (k, offset) = (v / self.chunk, v % self.chunk);
        let (row, i) = (k / (n - 2), k % (n - 2));
        let (p, q) = self.parity_roles6(row);
        let data = if self.layout & 2 != 0 {
            (p as u64 + 2 + i) % n
        } else if p as u64 == n - 1 {
            i + 1
        } else if i >= p as u64 {
            i + 2
        } else {
            i
        };
        (self.chunk - offset, data as u16, p, q, row * self.chunk + offset)
    }

    pub(super) fn parity_roles6(&self, row: u64) -> (u16, u16) {
        let n = self.raid_disks as u64;
        let p = if self.layout & 1 != 0 { row % n } else { n - 1 - row % n };
        (p as u16, ((p + 1) % n) as u16)
    }

    /// The syndrome index of data role `role` when Q is on `q`.
    fn syndrome_index(&self, role: usize, q: usize) -> usize {
        let n = self.raid_disks as usize;
        (role + n - (q + 1) % n) % n
    }

    /// Reads every member's piece at member sector `at`. Returns the roles that are missing (absent, or failed now:
    /// those are added to `failed`); more than two is an error.
    fn load6<M: Members>(&self, members: &mut M, at: u64, bytes: usize, bufs: &mut Bufs, failed: &mut u32) -> Result<u32, Error> {
        let mut missing = 0u32;
        for role in 0..self.raid_disks as u16 {
            match self.usable(role, *failed) {
                Some((member, base)) if members.read(member, base + at, &mut bufs[role as usize][..bytes]).is_ok() => {}
                Some(_) => {
                    *failed |= 1 << role;
                    missing |= 1 << role;
                }
                None => missing |= 1 << role,
            }
        }
        if missing.count_ones() > 2 { Err(Error::Device) } else { Ok(missing) }
    }

    /// P and Q over the data roles of the row, leaving out the roles in `skip`.
    fn syndromes(&self, bufs: &Bufs, p: usize, q: usize, skip: &[usize], bytes: usize, psum: &mut [u8], qsum: &mut [u8]) {
        psum[..bytes].fill(0);
        qsum[..bytes].fill(0);
        for role in (0..self.raid_disks as usize).filter(|r| *r != p && *r != q && !skip.contains(r)) {
            for (a, b) in psum[..bytes].iter_mut().zip(&bufs[role][..bytes]) {
                *a ^= b;
            }
            gf::mul_add(&mut qsum[..bytes], &bufs[role][..bytes], gf::pow(self.syndrome_index(role, q)));
        }
    }

    /// Fills in the buffers of the roles in `missing` (at most two) from the others.
    fn reconstruct6(&self, bufs: &mut Bufs, p: usize, q: usize, missing: u32, bytes: usize) -> Result<(), Error> {
        let n = self.raid_disks as usize;
        let mut lost = [usize::MAX; 2];
        let mut count = 0;
        for role in (0..n).filter(|r| *r != p && *r != q && missing & (1 << r) != 0) {
            lost[count] = role;
            count += 1;
        }
        let (p_gone, q_gone) = (missing & (1 << p) != 0, missing & (1 << q) != 0);
        let (mut psum, mut qsum, mut work) = ([0u8; PBYTES], [0u8; PBYTES], [0u8; PBYTES]);
        match count {
            0 => {}
            1 => {
                let x = lost[0];
                self.syndromes(bufs, p, q, &[x], bytes, &mut psum, &mut qsum);
                if !p_gone {
                    for i in 0..bytes {
                        work[i] = bufs[p][i] ^ psum[i];
                    }
                } else if !q_gone {
                    for i in 0..bytes {
                        work[i] = bufs[q][i] ^ qsum[i];
                    }
                    gf::mul_in_place(&mut work[..bytes], gf::inv(gf::pow(self.syndrome_index(x, q))));
                } else {
                    return Err(Error::Device);
                }
                bufs[x][..bytes].copy_from_slice(&work[..bytes]);
            }
            _ => {
                if p_gone || q_gone {
                    return Err(Error::Device);
                }
                let (x, y) = (lost[0], lost[1]);
                self.syndromes(bufs, p, q, &[x, y], bytes, &mut psum, &mut qsum);
                // Pxy = Dx + Dy, Qxy = g^x Dx + g^y Dy  =>  Dx = (Qxy + g^y Pxy) / (g^x + g^y)
                let (gx, gy) = (gf::pow(self.syndrome_index(x, q)), gf::pow(self.syndrome_index(y, q)));
                for i in 0..bytes {
                    psum[i] ^= bufs[p][i];
                    qsum[i] ^= bufs[q][i];
                }
                work[..bytes].copy_from_slice(&qsum[..bytes]);
                gf::mul_add(&mut work[..bytes], &psum[..bytes], gy);
                gf::mul_in_place(&mut work[..bytes], gf::inv(gx ^ gy));
                for i in 0..bytes {
                    bufs[y][i] = psum[i] ^ work[i];
                }
                bufs[x][..bytes].copy_from_slice(&work[..bytes]);
            }
        }
        if p_gone || q_gone {
            self.syndromes(bufs, p, q, &[], bytes, &mut psum, &mut qsum);
            if p_gone {
                bufs[p][..bytes].copy_from_slice(&psum[..bytes]);
            }
            if q_gone {
                bufs[q][..bytes].copy_from_slice(&qsum[..bytes]);
            }
        }
        Ok(())
    }

    pub(super) fn read_raid6<M: Members>(&self, members: &mut M, lba: u64, buffer: &mut [u8]) -> Result<(), Error> {
        let count = (buffer.len() / SECTOR) as u64;
        let mut done = 0;
        while done < count {
            let (run, data, p, q, at) = self.locate6(lba + done);
            let run = run.min(count - done).min(PIECE);
            let piece = &mut buffer[done as usize * SECTOR..(done + run) as usize * SECTOR];
            let direct = self.usable(data, 0).is_some_and(|(member, base)| members.read(member, base + at, piece).is_ok());
            if !direct {
                let mut bufs: Bufs = [[0; PBYTES]; MAX_MEMBERS];
                let mut sub = 0;
                while sub < run {
                    let length = (run - sub).min(PIECE6);
                    let bytes = length as usize * SECTOR;
                    let mut failed = 1u32 << data;
                    let missing = self.load6(members, at + sub, bytes, &mut bufs, &mut failed)?;
                    self.reconstruct6(&mut bufs, p as usize, q as usize, missing, bytes)?;
                    piece[sub as usize * SECTOR..sub as usize * SECTOR + bytes].copy_from_slice(&bufs[data as usize][..bytes]);
                    sub += length;
                }
            }
            done += run;
        }
        Ok(())
    }

    /// Writes a RAID 6 array one piece at a time. With data, P and Q all there the syndromes are updated from the old
    /// and new data (three reads); otherwise the row is read whole, what is missing rebuilt, and P and Q recomputed.
    pub(super) fn write_raid6<M: Members>(&mut self, members: &mut M, lba: u64, buffer: &[u8], now: u64) -> Result<(), Error> {
        let count = (buffer.len() / SECTOR) as u64;
        let (mut done, mut failed, mut lost) = (0, 0u32, false);
        while done < count {
            let (run, data, p, q, at) = self.locate6(lba + done);
            let run = run.min(count - done).min(PIECE6);
            let new = &buffer[done as usize * SECTOR..(done + run) as usize * SECTOR];
            if self.write_piece6(members, data, p, q, at, new, &mut failed).is_err() {
                lost = true;
                break;
            }
            done += run;
        }
        if self.drop_roles(members, failed, now)? {
            lost = true;
        }
        if lost { Err(Error::Device) } else { Ok(()) }
    }

    fn write_piece6<M: Members>(&mut self, members: &mut M, data: u16, p: u16, q: u16, at: u64, new: &[u8], failed: &mut u32) -> Result<(), Error> {
        let bytes = new.len();
        let (d, pi, qi) = (data as usize, p as usize, q as usize);
        let mut bufs: Bufs = [[0; PBYTES]; MAX_MEMBERS];
        let mut done = false;
        if let (Some(dm), Some(pm), Some(qm)) = (self.usable(data, *failed), self.usable(p, *failed), self.usable(q, *failed)) {
            let mut old = [0u8; PBYTES];
            if members.read(dm.0, dm.1 + at, &mut old[..bytes]).is_err() {
                *failed |= 1 << data;
            } else if members.read(pm.0, pm.1 + at, &mut bufs[pi][..bytes]).is_err() {
                *failed |= 1 << p;
            } else if members.read(qm.0, qm.1 + at, &mut bufs[qi][..bytes]).is_err() {
                *failed |= 1 << q;
            } else {
                for i in 0..bytes {
                    old[i] ^= new[i];
                    bufs[pi][i] ^= old[i];
                }
                gf::mul_add(&mut bufs[qi][..bytes], &old[..bytes], gf::pow(self.syndrome_index(d, qi)));
                bufs[d][..bytes].copy_from_slice(new);
                done = true;
            }
        }
        if !done {
            let missing = self.load6(members, at, bytes, &mut bufs, failed)?;
            self.reconstruct6(&mut bufs, pi, qi, missing, bytes)?;
            bufs[d][..bytes].copy_from_slice(new);
            let (mut psum, mut qsum) = ([0u8; PBYTES], [0u8; PBYTES]);
            self.syndromes(&bufs, pi, qi, &[], bytes, &mut psum, &mut qsum);
            bufs[pi][..bytes].copy_from_slice(&psum[..bytes]);
            bufs[qi][..bytes].copy_from_slice(&qsum[..bytes]);
        }
        let mut wrote = 0;
        for role in [data, p, q] {
            let buffer = &bufs[role as usize][..bytes];
            if let Some((member, base)) = self.usable(role, *failed) {
                match members.write(member, base + at, buffer) {
                    Ok(()) => wrote += 1,
                    Err(_) => *failed |= 1 << role,
                }
            } else if let Some((member, base)) = self.target(role) {
                if members.write(member, base + at, buffer).is_err() {
                    self.rebuild = None;
                }
            }
        }
        if wrote == 0 { Err(Error::Device) } else { Ok(()) }
    }

    /// Makes P and Q of one piece at member sector `m` agree with the data.
    pub(super) fn resync_piece6<M: Members>(&mut self, members: &mut M, m: u64, end: u64) -> Result<u64, Error> {
        let (p, q) = self.parity_roles6(m / self.chunk);
        let length = (self.chunk - m % self.chunk).min(end - m).min(PIECE6);
        let bytes = length as usize * SECTOR;
        let mut bufs: Bufs = [[0; PBYTES]; MAX_MEMBERS];
        if self.load6(members, m, bytes, &mut bufs, &mut 0)? != 0 {
            return Err(Error::Device);
        }
        let (mut psum, mut qsum) = ([0u8; PBYTES], [0u8; PBYTES]);
        self.syndromes(&bufs, p as usize, q as usize, &[], bytes, &mut psum, &mut qsum);
        for (role, sum) in [(p, &psum), (q, &qsum)] {
            if bufs[role as usize][..bytes] != sum[..bytes] {
                let (member, base) = self.usable(role, 0).ok_or(Error::Device)?;
                members.write(member, base + m, &sum[..bytes])?;
            }
        }
        Ok(m + length)
    }

    /// Fills the rebuild target's piece at member sector `m` from the other members.
    pub(super) fn rebuild_piece6<M: Members>(&mut self, members: &mut M, m: u64, end: u64) -> Result<u64, Error> {
        let (p, q) = self.parity_roles6(m / self.chunk);
        let length = (self.chunk - m % self.chunk).min(end - m).min(PIECE6);
        let bytes = length as usize * SECTOR;
        let (role, member, base) = self.rebuild.as_ref().map(|r| (r.slot.role, r.slot.member, r.slot.data_offset)).ok_or(Error::Busy)?;
        let mut bufs: Bufs = [[0; PBYTES]; MAX_MEMBERS];
        let done = self
            .load6(members, m, bytes, &mut bufs, &mut 0)
            .and_then(|missing| self.reconstruct6(&mut bufs, p as usize, q as usize, missing, bytes))
            .and_then(|()| members.write(member, base + m, &bufs[role as usize][..bytes]));
        match done {
            Ok(()) => Ok(m + length),
            Err(error) => {
                self.rebuild = None;
                Err(error)
            }
        }
    }
}
