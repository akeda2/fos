//! A read cache under the volume code: `Cached<D>` keeps recently read 4 KiB units of the device in memory, so the
//! metadata a file server touches over and over (bitmaps, inode-table blocks, folder blocks, extent nodes) costs one
//! disk command, not one per use. Writes go straight through and update what they overlap, so the cache is never stale
//! and never holds data the disk does not.
//!
//! The unit is eight sectors aligned on the volume; reads of at most one unit that stay inside it fill and use a slot
//! (direct mapped: unit number modulo the slot count), anything else goes to the disk untouched. A file's data is read
//! in big runs and does not pass through the slots, so streaming a file does not push the metadata out.
//!
//! The memory is the caller's (`Cache::new` takes the slices, a static in the kernels; a server kernel can give it
//! megabytes, an embedded one a few dozen KiB). `Cached::new` empties it: a cache belongs to one volume at a time.

use crate::{Device, Error, SECTOR};

const UNIT_SECTORS: u64 = 8;
const UNIT: usize = UNIT_SECTORS as usize * SECTOR;
const EMPTY: u64 = u64::MAX;

/// Slot tags and data: `data.len()` must be `tags.len() * 4096`.
pub struct Cache {
    tags: &'static mut [u64],
    data: &'static mut [u8],
    pub hits: u64,
    pub misses: u64,
}

impl Cache {
    pub fn new(tags: &'static mut [u64], data: &'static mut [u8]) -> Self {
        assert!(data.len() >= tags.len() * UNIT && !tags.is_empty());
        Cache { tags, data, hits: 0, misses: 0 }
    }

    /// Forgets everything: for when the disk may have changed behind the cache's back.
    pub fn clear(&mut self) {
        self.tags.fill(EMPTY);
        self.hits = 0;
        self.misses = 0;
    }

    fn slot(&self, unit: u64) -> usize {
        (unit % self.tags.len() as u64) as usize
    }

    /// A read through the cache; `raw` reads the disk.
    pub fn read(&mut self, lba: u64, buffer: &mut [u8], mut raw: impl FnMut(u64, &mut [u8]) -> Result<(), Error>) -> Result<(), Error> {
        let sectors = (buffer.len() / SECTOR) as u64;
        let unit = lba / UNIT_SECTORS;
        if sectors == 0 || sectors > UNIT_SECTORS || (lba + sectors - 1) / UNIT_SECTORS != unit {
            return raw(lba, buffer);
        }
        let slot = self.slot(unit);
        if self.tags[slot] != unit {
            let at = slot * UNIT;
            if raw(unit * UNIT_SECTORS, &mut self.data[at..at + UNIT]).is_err() {
                // The unit may run past the end of the volume: ask for just what was wanted.
                return raw(lba, buffer);
            }
            self.tags[slot] = unit;
            self.misses += 1;
        } else {
            self.hits += 1;
        }
        let from = slot * UNIT + (lba - unit * UNIT_SECTORS) as usize * SECTOR;
        buffer.copy_from_slice(&self.data[from..from + buffer.len()]);
        Ok(())
    }

    /// Brings the slots a write overlaps up to date (they are only copied into when already filled).
    pub fn wrote(&mut self, lba: u64, buffer: &[u8]) {
        let end = lba + (buffer.len() / SECTOR) as u64;
        let mut unit = lba / UNIT_SECTORS;
        while unit * UNIT_SECTORS < end {
            let slot = self.slot(unit);
            if self.tags[slot] == unit {
                let (a, b) = ((unit * UNIT_SECTORS).max(lba), ((unit + 1) * UNIT_SECTORS).min(end));
                let to = slot * UNIT + (a - unit * UNIT_SECTORS) as usize * SECTOR;
                let from = (a - lba) as usize * SECTOR;
                let length = (b - a) as usize * SECTOR;
                self.data[to..to + length].copy_from_slice(&buffer[from..from + length]);
            }
            unit += 1;
        }
    }
}

pub struct Cached<D: Device> {
    dev: D,
    cache: &'static mut Cache,
}

impl<D: Device> Cached<D> {
    pub fn new(dev: D, cache: &'static mut Cache) -> Self {
        cache.clear();
        Cached { dev, cache }
    }

    pub fn into_inner(self) -> D {
        self.dev
    }

    pub fn stats(&self) -> (u64, u64) {
        (self.cache.hits, self.cache.misses)
    }
}

impl<D: Device> Device for Cached<D> {
    fn read(&mut self, lba: u64, buffer: &mut [u8]) -> Result<(), Error> {
        let dev = &mut self.dev;
        self.cache.read(lba, buffer, |l, b| dev.read(l, b))
    }

    fn write(&mut self, lba: u64, buffer: &[u8]) -> Result<(), Error> {
        self.dev.write(lba, buffer)?;
        self.cache.wrote(lba, buffer);
        Ok(())
    }

    fn write_data(&mut self, lba: u64, buffer: &[u8]) -> Result<(), Error> {
        self.dev.write_data(lba, buffer)?;
        self.cache.wrote(lba, buffer);
        Ok(())
    }

    fn sync(&mut self) -> Result<(), Error> {
        self.dev.sync()
    }
}
