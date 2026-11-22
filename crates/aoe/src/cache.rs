//! Fewer, larger disk transfers behind a [`Target`](crate::Target): read-ahead for sequential reads and gathering of
//! consecutive writes. One frame carries at most a few sectors (2 at MTU 1500), and each disk command has a fixed cost
//! (measured on the server kernel under KVM: 121 of the 153 us a frame took, docs/block-sharing.md), so one 64 KiB command
//! instead of 64 one-KiB commands is where the time is.
//!
//! [`Cache`] owns two caller-supplied buffers (no allocation here). [`Cache::on`] wraps a [`Disk`] for one batch of frames;
//! the wrapper is itself a `Disk`, so `Target::handle` does not change.
//!
//! - **Reads**: a read that starts where the previous one ended is sequential: the wrapper reads as much as the read
//!   buffer holds from there in one command and serves the following reads from memory. Any other read goes straight to
//!   the disk, so random reads do not pay for data nobody asked for.
//! - **Writes**: a write that continues the gathered run is appended to the write buffer and acknowledged; anything else
//!   writes the run out first. The run is also written out before a read that overlaps it, on FLUSH, and by
//!   [`Cached::settle`], which the caller must call before it lets go of the disk (the kernel does at the end of every
//!   batch, under its disk lock): acknowledged data never outlives the batch in this buffer. Acknowledging a write that
//!   is still in RAM is what a drive with a write cache does too; FLUSH CACHE is the durability point either way.
//! - **Coherence**: a write also updates the read buffer where they overlap. Writes by anyone else to the same disk are not
//!   seen, so the caller drops the read buffer ([`Cache::invalidate`]) whenever it goes idle or the export changes.
//! - A write run that fails when it is written out cannot fail the frames already acknowledged: it is counted
//!   (`write_errors`) and makes the next FLUSH fail, which is how the initiator hears of it.

use crate::{Disk, SECTOR};

#[derive(Default, Clone, Copy, Debug, PartialEq)]
pub struct CacheStats {
    /// Reads answered from the read buffer, and reads that filled it with one larger disk read.
    pub read_hits: u64,
    pub read_fills: u64,
    /// Disk commands issued: reads (fills and direct) and writes (runs and direct).
    pub disk_reads: u64,
    pub disk_writes: u64,
    /// Writes taken into a run.
    pub gathered: u64,
    /// Runs that failed when written out after their frames were acknowledged.
    pub write_errors: u64,
}

pub struct Cache<'a> {
    read: &'a mut [u8],
    read_start: u64,
    read_sectors: usize,
    /// Where the next read starts if the initiator reads on in order (`u64::MAX`: no read yet).
    next_read: u64,
    gather: &'a mut [u8],
    gather_start: u64,
    gather_sectors: usize,
    /// A run failed after its frames were acknowledged; the next flush reports it.
    failed: bool,
    pub stats: CacheStats,
}

impl<'a> Cache<'a> {
    /// `read` and `gather` are the read-ahead and write-gather buffers (whole sectors; either may be empty to turn that
    /// half off).
    pub fn new(read: &'a mut [u8], gather: &'a mut [u8]) -> Cache<'a> {
        let read_len = read.len() / SECTOR * SECTOR;
        let gather_len = gather.len() / SECTOR * SECTOR;
        Cache {
            read: &mut read[..read_len],
            read_start: 0,
            read_sectors: 0,
            next_read: u64::MAX,
            gather: &mut gather[..gather_len],
            gather_start: 0,
            gather_sectors: 0,
            failed: false,
            stats: CacheStats::default(),
        }
    }

    /// Forgets what the read buffer holds (the disk may have changed under it). The write run must have been settled.
    pub fn invalidate(&mut self) {
        self.read_sectors = 0;
        self.next_read = u64::MAX;
    }

    /// Sectors waiting in the write run (0 after `settle`).
    pub fn pending(&self) -> usize {
        self.gather_sectors
    }

    /// This cache in front of `disk`, for one batch of frames.
    pub fn on<'b, D: Disk + ?Sized>(&'b mut self, disk: &'b mut D) -> Cached<'b, 'a, D> {
        Cached { cache: self, disk }
    }
}

pub struct Cached<'b, 'a, D: Disk + ?Sized> {
    cache: &'b mut Cache<'a>,
    disk: &'b mut D,
}

impl<D: Disk + ?Sized> Cached<'_, '_, D> {
    /// Writes the gathered run out (not a disk flush). False if that failed; the failure is also kept for the next FLUSH.
    pub fn settle(&mut self) -> bool {
        let cache = &mut *self.cache;
        if cache.gather_sectors == 0 {
            return true;
        }
        let length = cache.gather_sectors * SECTOR;
        cache.stats.disk_writes += 1;
        let ok = self.disk.write(cache.gather_start, &cache.gather[..length]);
        if !ok {
            cache.stats.write_errors += 1;
            cache.failed = true;
        }
        cache.gather_sectors = 0;
        ok
    }

    fn overlaps_run(&self, lba: u64, sectors: usize) -> bool {
        let cache = &*self.cache;
        cache.gather_sectors != 0 && lba < cache.gather_start + cache.gather_sectors as u64 && cache.gather_start < lba + sectors as u64
    }
}

impl<D: Disk + ?Sized> Disk for Cached<'_, '_, D> {
    fn sectors(&self) -> u64 {
        self.disk.sectors()
    }

    fn read(&mut self, lba: u64, buffer: &mut [u8]) -> bool {
        let sectors = buffer.len() / SECTOR;
        if self.overlaps_run(lba, sectors) && !self.settle() {
            return false;
        }
        let total = self.disk.sectors();
        let cache = &mut *self.cache;
        let end = lba + sectors as u64;
        if cache.read_sectors != 0 && lba >= cache.read_start && end <= cache.read_start + cache.read_sectors as u64 {
            let at = (lba - cache.read_start) as usize * SECTOR;
            buffer.copy_from_slice(&cache.read[at..at + buffer.len()]);
            cache.stats.read_hits += 1;
            cache.next_read = end;
            return true;
        }
        let sequential = lba == cache.next_read;
        cache.next_read = end;
        let room = cache.read.len() / SECTOR;
        if !sequential || sectors > room {
            cache.stats.disk_reads += 1;
            return self.disk.read(lba, buffer);
        }
        let fill = (room as u64).min(total.saturating_sub(lba)) as usize;
        if fill < sectors {
            cache.stats.disk_reads += 1;
            return self.disk.read(lba, buffer);
        }
        cache.stats.disk_reads += 1;
        cache.read_sectors = 0;
        if !self.disk.read(lba, &mut cache.read[..fill * SECTOR]) {
            return false;
        }
        cache.read_start = lba;
        cache.read_sectors = fill;
        cache.stats.read_fills += 1;
        buffer.copy_from_slice(&cache.read[..buffer.len()]);
        true
    }

    fn write(&mut self, lba: u64, data: &[u8]) -> bool {
        let sectors = data.len() / SECTOR;
        {
            // Keep the read buffer what the disk will hold.
            let cache = &mut *self.cache;
            if cache.read_sectors != 0 {
                let (start, end) = (cache.read_start, cache.read_start + cache.read_sectors as u64);
                let (from, to) = (lba.max(start), (lba + sectors as u64).min(end));
                if from < to {
                    let source = (from - lba) as usize * SECTOR;
                    let target = (from - start) as usize * SECTOR;
                    let length = (to - from) as usize * SECTOR;
                    cache.read[target..target + length].copy_from_slice(&data[source..source + length]);
                }
            }
        }
        let room = self.cache.gather.len() / SECTOR;
        if sectors > room {
            self.settle();
            self.cache.stats.disk_writes += 1;
            return self.disk.write(lba, data);
        }
        let cache = &*self.cache;
        let continues = cache.gather_sectors != 0 && lba == cache.gather_start + cache.gather_sectors as u64 && cache.gather_sectors + sectors <= room;
        if !continues {
            self.settle();
            self.cache.gather_start = lba;
        }
        let cache = &mut *self.cache;
        let at = cache.gather_sectors * SECTOR;
        cache.gather[at..at + data.len()].copy_from_slice(data);
        cache.gather_sectors += sectors;
        cache.stats.gathered += 1;
        true
    }

    fn flush(&mut self) -> bool {
        let settled = self.settle();
        let failed = core::mem::replace(&mut self.cache.failed, false);
        self.disk.flush() && settled && !failed
    }
}
