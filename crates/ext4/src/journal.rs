//! The jbd2 journal (docs/ext4.md, step 4): replay a journal Linux left behind, and journal the writer's metadata.
//!
//! `Journaled<D>` wraps a device. Metadata writes (`Device::write`) are *staged* in memory; `commit` makes them one
//! atomic transaction:
//!
//! 1. the descriptor block, the staged blocks and the commit block go to the journal;
//! 2. the journal superblock's `start` is set (one sector write: **this is the commit point**);
//! 3. the staged blocks are written to their home places (the checkpoint);
//! 4. `start` goes back to 0, the sequence number moves on.
//!
//! A power cut before 2 loses the transaction whole, one after 2 is replayed by the next mount (this crate's `open`, or
//! Linux). Because every transaction is checkpointed before the next starts, the journal holds at most one, which is
//! why no revoke records are ever written (revokes are only read, for journals Linux made). File data (`write_data`)
//! goes straight to the disk before the commit ("ordered" mode: a committed file never shows other data).
//!
//! `needs_recovery` (incompat 0x4) is set in the ext4 superblock before the first commit and cleared by `close`,
//! because Linux only replays a journal of a volume that says so. There is no write barrier in `Device`: a disk with a
//! volatile cache may reorder step 1 and 2; that narrows the guarantee, it does not remove the journal's use.
//!
//! Not handled, refused with `Unsupported`: journal checksums (v1, v2, v3), asynchronous commit, fast commit, an
//! external journal, a journal that is not one contiguous run on the volume.

use crate::{Device, Error, Fs, Mapping, Options, FLAG_EXTENTS, INCOMPAT_RECOVER, SECTOR, le32, MAX_BLOCK};

/// Blocks one transaction holds (more auto-commits, which only costs atomicity of that one huge operation).
pub const TX_BLOCKS: usize = 64;

const JMAGIC: u32 = 0xc03b_3998;
const T_DESCRIPTOR: u32 = 1;
const T_COMMIT: u32 = 2;
const T_SUPER_V2: u32 = 4;
const T_REVOKE: u32 = 5;
const F_ESCAPE: u16 = 1;
const F_SAME_UUID: u16 = 2;
const F_LAST: u16 = 8;
const JI_REVOKE: u32 = 1;
const JI_64BIT: u32 = 2;
const REVOKES: usize = 256;

fn be32(bytes: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn be16(bytes: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([bytes[at], bytes[at + 1]])
}

fn put_be32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_be_bytes());
}

/// The staged blocks of the transaction being built. Big (256 KiB), so it lives where the caller puts it (a static, or a
/// leaked box in tests) and `Journaled` borrows it for good.
pub struct Stage {
    numbers: [u64; TX_BLOCKS],
    blocks: [[u8; MAX_BLOCK]; TX_BLOCKS],
    count: usize,
}

impl Stage {
    pub const fn new() -> Self {
        Stage { numbers: [0; TX_BLOCKS], blocks: [[0; MAX_BLOCK]; TX_BLOCKS], count: 0 }
    }
}

impl Default for Stage {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy)]
struct Journal {
    /// Volume block holding the journal's block 0 (its superblock).
    start: u64,
    /// Journal blocks, and the first one that holds log (`s_first`).
    len: u32,
    first: u32,
    /// Sequence number of the next transaction.
    seq: u32,
    /// Bytes of a descriptor tag: 8, or 12 with the journal's 64-bit feature.
    tag: usize,
    uuid: [u8; 16],
    /// `needs_recovery` is set in the ext4 superblock.
    active: bool,
}

pub struct Journaled<D: Device> {
    dev: D,
    stage: &'static mut Stage,
    bs: usize,
    /// Blocks of the volume, to refuse a journal that names blocks outside it.
    limit: u64,
    journal: Option<Journal>,
}

/// Mounts the volume for writing: replays a journal that holds committed transactions, then wraps the device so that the
/// writer's metadata is journaled (volumes without a journal are written straight through).
pub fn open<D: Device>(device: D, stage: &'static mut Stage) -> Result<Fs<Journaled<D>>, Error> {
    let mut probe = Fs::mount_with(device, Options { ignore_journal: true })?;
    let mut sb = [0u8; 1024];
    probe.device.read(2, &mut sb)?;
    let wanted = probe.compat & 0x4 != 0;
    let mut journal = None;
    if wanted {
        let number = le32(&sb, 224);
        if number == 0 {
            return Err(Error::Unsupported); // an external journal
        }
        let inode = probe.inode(number)?;
        if inode.flags & FLAG_EXTENTS == 0 {
            return Err(Error::Unsupported);
        }
        let bs = probe.block_size;
        let len = (inode.size / bs as u64) as u32;
        let mut logical = 0u64;
        let mut start = 0u64;
        while logical < u64::from(len) {
            match probe.map(&inode, logical)? {
                Mapping::Data { block, blocks } => {
                    if logical == 0 {
                        start = block;
                    } else if block != start + logical {
                        return Err(Error::Unsupported);
                    }
                    logical += blocks;
                }
                _ => return Err(Error::Corrupt),
            }
        }
        if start == 0 || start + u64::from(len) > probe.blocks || len < TX_BLOCKS as u32 + 8 {
            return Err(Error::Corrupt);
        }
        journal = Some(Journal { start, len, first: 1, seq: 1, tag: 8, uuid: [0; 16], active: false });
    }
    let (bs, limit, recover) = (probe.block_size, probe.blocks, probe.incompat & INCOMPAT_RECOVER != 0);
    let mut dev = Journaled { dev: probe.into_device(), stage, bs, limit, journal };
    dev.stage.count = 0;
    if dev.journal.is_some() {
        dev.load_super()?;
        if dev.read_journal_start()? != 0 {
            dev.replay()?;
        }
        if recover {
            dev.set_recover(false)?;
        }
    } else if recover {
        return Err(Error::NeedsRecovery);
    }
    Fs::mount(dev)
}

impl<D: Device> Journaled<D> {
    fn spb(&self) -> u64 {
        (self.bs / SECTOR) as u64
    }

    fn jlba(&self, position: u32) -> u64 {
        (self.journal.as_ref().map_or(0, |journal| journal.start) + u64::from(position)) * self.spb()
    }

    fn slot(&self, block: u64) -> Option<usize> {
        self.stage.numbers[..self.stage.count].iter().position(|&number| number == block)
    }

    /// Reads the journal superblock into the in-memory description (and validates it).
    fn load_super(&mut self) -> Result<(), Error> {
        let mut sb = [0u8; 1024];
        self.dev.read(self.jlba(0), &mut sb)?;
        if be32(&sb, 0) != JMAGIC || be32(&sb, 4) != T_SUPER_V2 || be32(&sb, 12) as usize != self.bs {
            return Err(Error::Unsupported);
        }
        let (compat, incompat, ro_compat) = (be32(&sb, 36), be32(&sb, 40), be32(&sb, 44));
        if compat != 0 || ro_compat != 0 || incompat & !(JI_REVOKE | JI_64BIT) != 0 || be32(&sb, 64) > 1 {
            return Err(Error::Unsupported);
        }
        let journal = self.journal.as_mut().ok_or(Error::Corrupt)?;
        let maxlen = be32(&sb, 16);
        let first = be32(&sb, 20);
        if maxlen > journal.len || first == 0 || first >= maxlen || maxlen < TX_BLOCKS as u32 + 8 {
            return Err(Error::Corrupt);
        }
        journal.len = maxlen;
        journal.first = first;
        journal.seq = be32(&sb, 24);
        journal.tag = if incompat & JI_64BIT != 0 { 12 } else { 8 };
        journal.uuid.copy_from_slice(&sb[48..64]);
        Ok(())
    }

    fn read_journal_start(&mut self) -> Result<u32, Error> {
        let mut sb = [0u8; SECTOR];
        self.dev.read(self.jlba(0), &mut sb)?;
        Ok(be32(&sb, 28))
    }

    fn write_journal_super(&mut self, start: u32, seq: u32) -> Result<(), Error> {
        let mut sb = [0u8; 1024];
        let lba = self.jlba(0);
        self.dev.read(lba, &mut sb)?;
        put_be32(&mut sb, 24, seq);
        put_be32(&mut sb, 28, start);
        self.dev.write(lba, &sb)
    }

    /// Sets or clears `needs_recovery` in the ext4 superblock on the disk (and in a staged copy of it).
    fn set_recover(&mut self, on: bool) -> Result<(), Error> {
        let mut sb = [0u8; 1024];
        self.dev.read(2, &mut sb)?;
        let flags = le32(&sb, 96);
        let new = if on { flags | INCOMPAT_RECOVER } else { flags & !INCOMPAT_RECOVER };
        sb[96..100].copy_from_slice(&new.to_le_bytes());
        self.dev.write(2, &sb)?;
        if let Some(slot) = self.slot(1024 / self.bs as u64) {
            let at = 1024 % self.bs + 96;
            self.stage.blocks[slot][at..at + 4].copy_from_slice(&new.to_le_bytes());
        }
        if let Some(journal) = self.journal.as_mut() {
            journal.active = on;
        }
        Ok(())
    }

    /// Walks the tags of a descriptor block; returns how many there are.
    fn tags(&self, block: &[u8], mut each: impl FnMut(usize, u64, u16)) -> usize {
        let tag = self.journal.as_ref().map_or(8, |journal| journal.tag);
        let (mut at, mut count) = (12, 0);
        while at + tag <= self.bs {
            let mut number = u64::from(be32(block, at));
            if tag == 12 {
                number |= u64::from(be32(block, at + 8)) << 32;
            }
            let flags = be16(block, at + 6);
            each(count, number, flags);
            count += 1;
            at += tag;
            if flags & F_SAME_UUID == 0 {
                at += 16;
            }
            if flags & F_LAST != 0 {
                break;
            }
        }
        count
    }

    fn wrap(&self, position: u32) -> u32 {
        let journal = self.journal.as_ref().unwrap();
        if position >= journal.len { position - (journal.len - journal.first) } else { position }
    }

    /// Replays the committed transactions in the log (Linux's three passes: find the end, collect the revokes, copy).
    fn replay(&mut self) -> Result<(), Error> {
        let journal = self.journal.ok_or(Error::Corrupt)?;
        let start = self.read_journal_start()?;
        let mut revokes = [(0u64, 0u32); REVOKES];
        let mut revoked = 0usize;
        let mut buffer = [0u8; MAX_BLOCK];
        let mut data = [0u8; MAX_BLOCK];
        let bs = self.bs;
        let mut end = journal.seq;
        for pass in 1..=3 {
            let (mut sequence, mut position) = (journal.seq, start);
            for _ in 0..journal.len {
                if pass > 1 && sequence >= end {
                    break;
                }
                let lba = self.jlba(position);
                self.dev.read(lba, &mut buffer[..bs])?;
                if be32(&buffer, 0) != JMAGIC || be32(&buffer, 8) != sequence {
                    break;
                }
                match be32(&buffer, 4) {
                    T_DESCRIPTOR => {
                        let mut tags = [(0u64, 0u16); TX_BLOCKS * 2];
                        let mut stored = 0;
                        let count = self.tags(&buffer, |index, number, flags| {
                            if index < tags.len() {
                                tags[index] = (number, flags);
                                stored = index + 1;
                            }
                        });
                        if count > stored {
                            return Err(Error::Unsupported);
                        }
                        if pass == 3 {
                            for (index, &(number, flags)) in tags[..count].iter().enumerate() {
                                let hit = revokes[..revoked].iter().any(|&(block, at)| block == number && at >= sequence);
                                if hit {
                                    continue;
                                }
                                if number >= self.limit {
                                    return Err(Error::Corrupt);
                                }
                                self.dev.read(self.jlba(self.wrap(position + 1 + index as u32)), &mut data[..bs])?;
                                if flags & F_ESCAPE != 0 {
                                    put_be32(&mut data, 0, JMAGIC);
                                }
                                self.dev.write(number * self.spb(), &data[..bs])?;
                            }
                        }
                        position = self.wrap(position + 1 + count as u32);
                    }
                    T_COMMIT => {
                        sequence += 1;
                        if pass == 1 {
                            end = sequence;
                        }
                        position = self.wrap(position + 1);
                    }
                    T_REVOKE => {
                        if pass == 2 {
                            let used = be32(&buffer, 12) as usize;
                            let width = if journal.tag == 12 { 8 } else { 4 };
                            let mut at = 16;
                            while at + width <= used.min(bs) {
                                let mut number = u64::from(be32(&buffer, at));
                                if width == 8 {
                                    number = number << 32 | u64::from(be32(&buffer, at + 4));
                                }
                                match revokes[..revoked].iter().position(|&(block, _)| block == number) {
                                    Some(index) => revokes[index].1 = revokes[index].1.max(sequence),
                                    None if revoked < REVOKES => {
                                        revokes[revoked] = (number, sequence);
                                        revoked += 1;
                                    }
                                    None => return Err(Error::Unsupported),
                                }
                                at += width;
                            }
                        }
                        position = self.wrap(position + 1);
                    }
                    _ => break,
                }
            }
        }
        self.write_journal_super(0, end)?;
        if let Some(journal) = self.journal.as_mut() {
            journal.seq = end;
        }
        Ok(())
    }

    /// Makes the staged blocks durable as one transaction (see the module notes). Nothing staged: nothing happens.
    fn commit_stage(&mut self) -> Result<(), Error> {
        let count = self.stage.count;
        if count == 0 {
            return Ok(());
        }
        let journal = self.journal.ok_or(Error::Corrupt)?;
        if !journal.active {
            self.set_recover(true)?;
        }
        let (bs, seq, first) = (self.bs, journal.seq, journal.first);
        let mut block = [0u8; MAX_BLOCK];
        put_be32(&mut block, 0, JMAGIC);
        put_be32(&mut block, 4, T_DESCRIPTOR);
        put_be32(&mut block, 8, seq);
        let mut at = 12;
        for index in 0..count {
            let number = self.stage.numbers[index];
            let escape = be32(&self.stage.blocks[index], 0) == JMAGIC;
            let mut flags = if escape { F_ESCAPE } else { 0 };
            if index > 0 {
                flags |= F_SAME_UUID;
            }
            if index + 1 == count {
                flags |= F_LAST;
            }
            put_be32(&mut block, at, number as u32);
            block[at + 6..at + 8].copy_from_slice(&flags.to_be_bytes());
            if journal.tag == 12 {
                put_be32(&mut block, at + 8, (number >> 32) as u32);
            }
            at += journal.tag;
            if index == 0 {
                block[at..at + 16].copy_from_slice(&journal.uuid);
                at += 16;
            }
        }
        self.dev.write(self.jlba(first), &block[..bs])?;
        for index in 0..count {
            block[..bs].copy_from_slice(&self.stage.blocks[index][..bs]);
            if be32(&block, 0) == JMAGIC {
                put_be32(&mut block, 0, 0);
            }
            let lba = self.jlba(first + 1 + index as u32);
            self.dev.write(lba, &block[..bs])?;
        }
        block[..bs].fill(0);
        put_be32(&mut block, 0, JMAGIC);
        put_be32(&mut block, 4, T_COMMIT);
        put_be32(&mut block, 8, seq);
        let lba = self.jlba(first + 1 + count as u32);
        self.dev.write(lba, &block[..bs])?;
        self.write_journal_super(first, seq)?; // the commit point
        for index in 0..count {
            let lba = self.stage.numbers[index] * self.spb();
            self.dev.write(lba, &self.stage.blocks[index][..bs])?;
        }
        self.write_journal_super(0, seq.wrapping_add(1))?;
        if let Some(journal) = self.journal.as_mut() {
            journal.seq = seq.wrapping_add(1);
        }
        self.stage.count = 0;
        Ok(())
    }

    /// Takes a blocks' staged copy out (a freed metadata block that is now file data).
    fn drop_staged(&mut self, block: u64) {
        if let Some(slot) = self.slot(block) {
            let last = self.stage.count - 1;
            self.stage.numbers[slot] = self.stage.numbers[last];
            let moved = self.stage.blocks[last];
            self.stage.blocks[slot] = moved;
            self.stage.count = last;
        }
    }
}

impl<D: Device> Device for Journaled<D> {
    fn read(&mut self, lba: u64, buffer: &mut [u8]) -> Result<(), Error> {
        self.dev.read(lba, buffer)?;
        let spb = self.spb();
        let sectors = (buffer.len() / SECTOR) as u64;
        for index in 0..self.stage.count {
            let from = self.stage.numbers[index] * spb;
            let (a, b) = (from.max(lba), (from + spb).min(lba + sectors));
            if a < b {
                let (src, dst) = (((a - from) as usize) * SECTOR, ((a - lba) as usize) * SECTOR);
                let length = ((b - a) as usize) * SECTOR;
                buffer[dst..dst + length].copy_from_slice(&self.stage.blocks[index][src..src + length]);
            }
        }
        Ok(())
    }

    fn write(&mut self, lba: u64, buffer: &[u8]) -> Result<(), Error> {
        if self.journal.is_none() {
            return self.dev.write(lba, buffer);
        }
        let spb = self.spb();
        let sectors = (buffer.len() / SECTOR) as u64;
        let mut block = lba / spb;
        while block * spb < lba + sectors {
            let (a, b) = ((block * spb).max(lba), ((block + 1) * spb).min(lba + sectors));
            let slot = match self.slot(block) {
                Some(slot) => slot,
                None => {
                    if self.stage.count == TX_BLOCKS {
                        self.commit_stage()?;
                    }
                    let slot = self.stage.count;
                    if b - a < spb {
                        let mut whole = [0u8; MAX_BLOCK];
                        self.read(block * spb, &mut whole[..self.bs])?;
                        self.stage.blocks[slot][..self.bs].copy_from_slice(&whole[..self.bs]);
                    }
                    self.stage.numbers[slot] = block;
                    self.stage.count += 1;
                    slot
                }
            };
            let (src, dst) = (((a - lba) as usize) * SECTOR, ((a - block * spb) as usize) * SECTOR);
            let length = ((b - a) as usize) * SECTOR;
            self.stage.blocks[slot][dst..dst + length].copy_from_slice(&buffer[src..src + length]);
            block += 1;
        }
        Ok(())
    }

    fn write_data(&mut self, lba: u64, buffer: &[u8]) -> Result<(), Error> {
        if self.journal.is_some() && self.stage.count > 0 {
            let spb = self.spb();
            for block in lba / spb..(lba + (buffer.len() / SECTOR) as u64).div_ceil(spb) {
                self.drop_staged(block);
            }
        }
        self.dev.write(lba, buffer)
    }

    fn commit(&mut self) -> Result<(), Error> {
        self.commit_stage()
    }

    fn close(&mut self) -> Result<(), Error> {
        self.commit_stage()?;
        if self.journal.as_ref().is_some_and(|journal| journal.active) {
            self.set_recover(false)?;
        }
        Ok(())
    }

    fn pending(&self) -> usize {
        self.stage.count
    }

    fn journaled(&self) -> bool {
        self.journal.is_some()
    }
}
