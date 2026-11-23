//! A file as an exported disk: the file's sectors, wherever its file system put them, served as sectors 0.. of a
//! disk of their own.
//!
//! The kernel asks the file system once, at export time, where the file lies and hands the answer over as a list of
//! [`Run`]s in file order: so many sectors at this disk address, or so many that read as zeros (a hole, or an ext4 extent
//! that was allocated and never written). [`FileDisk`] then turns each read or write of the export into reads and writes
//! of the underlying disk, split where a request crosses from one run into the next. No file-system code runs per frame,
//! so serving a file costs what serving a disk does.
//!
//! The list is a snapshot: the kernel keeps other writers off those sectors while the export lasts
//! (docs/block-sharing.md "A file as an export"). Zero runs have no sectors behind them, so writes to them are refused;
//! the kernel exports a file with any writable only when it has none.

use crate::{Disk, SECTOR};

/// Where `sectors` sectors of the file, starting at file sector `start`, are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Run {
    pub start: u64,
    pub sectors: u64,
    pub place: Place,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place {
    /// On the disk from this sector on.
    Disk(u64),
    /// Nowhere: reads as zeros, cannot be written.
    Zeros,
}

impl Run {
    fn end(&self) -> u64 {
        self.start + self.sectors
    }
}

/// Adds `sectors` sectors at `place` to the end of `runs`' last run when they continue it (the next disk sector, or zeros
/// after zeros); false when they start a new run, which the caller then appends. A fragmented file's map stays as short
/// as the file system allows: ext4 stores at most 32768 blocks per extent, so a large file that lies in one piece comes as
/// many extents that merge here.
pub fn extend(last: Option<&mut Run>, sectors: u64, place: Place) -> bool {
    let Some(last) = last else { return false };
    let continues = match (last.place, place) {
        (Place::Disk(at), Place::Disk(next)) => at + last.sectors == next,
        (Place::Zeros, Place::Zeros) => true,
        _ => false,
    };
    if continues {
        last.sectors += sectors;
    }
    continues
}

/// Whether `runs` describe sectors 0.. of a file with no gaps and cover at least `sectors` of them.
pub fn covers(runs: &[Run], sectors: u64) -> bool {
    let mut next = 0;
    for run in runs {
        if run.start != next || run.sectors == 0 {
            return false;
        }
        next = run.end();
    }
    next >= sectors
}

/// How many sectors of the first `sectors` lie in zero runs.
pub fn zero_sectors(runs: &[Run], sectors: u64) -> u64 {
    runs.iter()
        .filter(|run| run.place == Place::Zeros && run.start < sectors)
        .map(|run| run.end().min(sectors) - run.start)
        .sum()
}

/// The first `sectors` sectors of a file whose map is `runs`, on `disk`.
pub struct FileDisk<'a, D: Disk> {
    disk: &'a mut D,
    runs: &'a [Run],
    sectors: u64,
}

impl<'a, D: Disk> FileDisk<'a, D> {
    /// `None` when `runs` does not cover `sectors` (see [`covers`]) or a run lies past the end of `disk`.
    pub fn new(disk: &'a mut D, runs: &'a [Run], sectors: u64) -> Option<FileDisk<'a, D>> {
        let size = disk.sectors();
        let inside = runs.iter().all(|run| match run.place {
            Place::Disk(at) => at.checked_add(run.sectors).is_some_and(|end| end <= size),
            Place::Zeros => true,
        });
        (covers(runs, sectors) && inside).then_some(FileDisk { disk, runs, sectors })
    }

    /// The run holding file sector `lba` (which the caller has checked is below `sectors`).
    fn run_at(&self, lba: u64) -> &Run {
        let index = self.runs.partition_point(|run| run.end() <= lba);
        &self.runs[index]
    }

    /// Calls `each(place, offset in bytes, sectors)` for the pieces of the request `lba`, `bytes`; false at once if
    /// `each` does, or if the request is not whole sectors inside the file.
    fn split(&mut self, lba: u64, bytes: usize, each: &mut dyn FnMut(&mut D, Place, usize, usize) -> bool) -> bool {
        let count = (bytes / SECTOR) as u64;
        if bytes % SECTOR != 0 || lba.checked_add(count).is_none_or(|end| end > self.sectors) {
            return false;
        }
        let (mut at, mut done) = (lba, 0usize);
        while done < bytes {
            let run = *self.run_at(at);
            let skip = at - run.start;
            let take = (run.sectors - skip).min(((bytes - done) / SECTOR) as u64) as usize;
            let place = match run.place {
                Place::Disk(first) => Place::Disk(first + skip),
                Place::Zeros => Place::Zeros,
            };
            if !each(self.disk, place, done, take * SECTOR) {
                return false;
            }
            at += take as u64;
            done += take * SECTOR;
        }
        true
    }
}

impl<D: Disk> Disk for FileDisk<'_, D> {
    fn sectors(&self) -> u64 {
        self.sectors
    }

    fn read(&mut self, lba: u64, buffer: &mut [u8]) -> bool {
        let length = buffer.len();
        self.split(lba, length, &mut |disk, place, offset, bytes| {
            let piece = &mut buffer[offset..offset + bytes];
            match place {
                Place::Disk(at) => disk.read(at, piece),
                Place::Zeros => {
                    piece.fill(0);
                    true
                }
            }
        })
    }

    /// Refused as a whole when any of it falls in a zero run: nothing is written then. (Checked first, so a write is
    /// never half done because of the map; a disk error halfway is the disk's, as for a whole-disk export.)
    fn write(&mut self, lba: u64, data: &[u8]) -> bool {
        let mut writable = true;
        if !self.split(lba, data.len(), &mut |_, place, _, _| {
            writable &= place != Place::Zeros;
            true
        }) || !writable
        {
            return false;
        }
        self.split(lba, data.len(), &mut |disk, place, offset, bytes| match place {
            Place::Disk(at) => disk.write(at, &data[offset..offset + bytes]),
            Place::Zeros => false,
        })
    }

    fn flush(&mut self) -> bool {
        self.disk.flush()
    }
}
