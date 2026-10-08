//! Streaming one take into a file through a `Sink`.
//!
//! The recorder owns the file layout (header first, then the audio exactly as the device delivered it) and the
//! write discipline; the `Sink` is whatever stores the bytes (FOSFS's reserved extent today, FAT32 and exFAT files
//! next). A sink's rules are what make this careful:
//!
//! - writes go to the volume in whole `chunk`-sized blocks (a multiple of the 512-byte sector), so every block but the
//!   last starts and ends on a sector boundary;
//! - the file's *length* and its *header* only move together, at the first block, at a `checkpoint` and at the end, so
//!   a power cut or reset leaves a file whose header matches its length and holds everything up to the last
//!   checkpoint;
//! - the first sector (header plus the first audio) is kept in memory and rewritten at checkpoints, so the header
//!   can follow the take without reading anything back.
//!
//! When the sink's file is full (`Sink::open` reports its largest size), `push` takes what fits and returns how much
//! it consumed; the caller finishes this file and begins the next one with the rest (a split, as FAT32's 4 GiB
//! limit needs).

use crate::wav::{self, Format};

pub const SECTOR: usize = 512;

/// Where a take is stored.
pub trait Sink {
    /// Creates the file `name` with room for at least `reserve` bytes (0 = as much as possible). Returns the
    /// largest size this file can reach, or a negative error code.
    fn open(&mut self, name: &[u8], reserve: u64) -> Result<u64, i32>;
    /// Writes `data` at byte `offset` (a multiple of 512; the last sector is padded) and, if `length` is not 0,
    /// makes that the file's length.
    fn write(&mut self, offset: u64, data: &[u8], length: u64) -> Result<(), i32>;
    /// Ends the file with `length` bytes, giving back any reserved space it did not use.
    fn close(&mut self, length: u64) -> Result<(), i32>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Error(pub i32);

pub struct Recorder<'a, S: Sink> {
    sink: &'a mut S,
    staging: &'a mut [u8],
    format: Format,
    header_len: usize,
    /// The largest file size the sink offered.
    limit: u64,
    /// Bytes of `staging` in use, and the file offset of its first byte.
    filled: usize,
    offset: u64,
    /// Audio bytes accepted so far (not counting the header).
    audio: u64,
    first_sector: [u8; SECTOR],
    have_first: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Summary {
    /// The file's final length, header included.
    pub bytes: u64,
    pub frames: u64,
}

impl<'a, S: Sink> Recorder<'a, S> {
    /// Opens the file and writes nothing yet. `staging` is the block buffer: a multiple of 512 bytes, large enough
    /// to hold a header and a sector. `reserve` is the size the sink should make room for (header included; 0 = as
    /// much as possible).
    pub fn begin(sink: &'a mut S, staging: &'a mut [u8], format: Format, name: &[u8], reserve: u64) -> Result<Recorder<'a, S>, Error> {
        assert!(staging.len() % SECTOR == 0 && staging.len() >= 2 * SECTOR);
        let header_len = wav::header_len(&format);
        let limit = sink.open(name, reserve).map_err(Error)?;
        staging[..header_len].fill(0); // the header is written when the first block goes out
        Ok(Recorder { sink, staging, format, header_len, limit, filled: header_len, offset: 0, audio: 0, first_sector: [0; SECTOR], have_first: false })
    }

    /// The largest size the sink offered for this file (header included).
    pub fn capacity(&self) -> u64 {
        self.limit
    }

    /// Audio bytes accepted so far.
    pub fn audio_bytes(&self) -> u64 {
        self.audio
    }

    /// Bytes of audio that still fit in this file.
    pub fn room(&self) -> u64 {
        self.limit.saturating_sub(self.offset + self.filled as u64)
    }

    /// Appends audio (whole frames). Returns how many bytes were taken: fewer than `data.len()` only when the file
    /// is full.
    pub fn push(&mut self, data: &[u8]) -> Result<usize, Error> {
        let mut taken = 0;
        while taken < data.len() {
            let room = (self.staging.len() - self.filled).min(self.room().min(usize::MAX as u64) as usize);
            if room == 0 {
                if self.filled == self.staging.len() {
                    self.flush(false)?;
                    continue;
                }
                break; // the file is full
            }
            let count = room.min(data.len() - taken);
            self.staging[self.filled..self.filled + count].copy_from_slice(&data[taken..taken + count]);
            self.filled += count;
            self.audio += count as u64;
            taken += count;
            if self.filled == self.staging.len() {
                self.flush(false)?;
            }
        }
        Ok(taken)
    }

    /// Writes out the staged bytes. At offset 0 the block starts with the header; its first sector is kept.
    fn flush(&mut self, last: bool) -> Result<(), Error> {
        if self.filled == 0 {
            return Ok(());
        }
        if self.offset == 0 {
            wav::write_header(self.staging, &self.format, self.audio);
            if self.filled >= SECTOR {
                self.first_sector.copy_from_slice(&self.staging[..SECTOR]);
                self.have_first = true;
            }
        }
        // The first block sets the length (it carries its own header); later blocks leave it alone until a checkpoint
        // or the end, so the length never runs ahead of the header.
        let length = if self.offset == 0 || last { self.offset + self.filled as u64 } else { 0 };
        self.sink.write(self.offset, &self.staging[..self.filled], length).map_err(Error)?;
        self.offset += self.filled as u64;
        self.filled = 0;
        Ok(())
    }

    /// Brings the header and the file length up to date with what has been written, so that a reset now keeps the
    /// take so far. Cheap (one sector); call every second or two.
    pub fn checkpoint(&mut self) -> Result<(), Error> {
        if !self.have_first || self.offset <= self.header_len as u64 {
            return Ok(());
        }
        wav::write_header(&mut self.first_sector, &self.format, self.offset - self.header_len as u64);
        self.sink.write(0, &self.first_sector, self.offset).map_err(Error)
    }

    /// Writes the rest, the final header and the final length, and closes the file.
    pub fn finish(mut self) -> Result<Summary, Error> {
        self.flush(true)?;
        let total = self.offset;
        if self.have_first {
            wav::write_header(&mut self.first_sector, &self.format, total - self.header_len as u64);
            self.sink.write(0, &self.first_sector, total).map_err(Error)?;
        }
        self.sink.close(total).map_err(Error)?;
        Ok(Summary { bytes: total, frames: (total - self.header_len as u64) / self.format.frame_bytes().max(1) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sink in memory that remembers the length the way a file would.
    struct Memory {
        data: Vec<u8>,
        length: u64,
        capacity: u64,
        closed: Option<u64>,
    }

    impl Sink for Memory {
        fn open(&mut self, _name: &[u8], _reserve: u64) -> Result<u64, i32> {
            Ok(self.capacity)
        }
        fn write(&mut self, offset: u64, data: &[u8], length: u64) -> Result<(), i32> {
            assert_eq!(offset % 512, 0);
            let end = offset as usize + data.len();
            if self.data.len() < end {
                self.data.resize(end, 0);
            }
            self.data[offset as usize..end].copy_from_slice(data);
            if length != 0 {
                self.length = length;
            }
            Ok(())
        }
        fn close(&mut self, length: u64) -> Result<(), i32> {
            self.closed = Some(length);
            Ok(())
        }
    }

    fn data_len(file: &[u8]) -> u32 {
        let at = file.windows(4).position(|window| window == b"data").unwrap();
        u32::from_le_bytes(file[at + 4..at + 8].try_into().unwrap())
    }

    const FORMAT: Format = Format { channels: 2, rate: 48_000, slot: 2, bits: 16 };

    #[test]
    fn a_take_has_a_matching_header() {
        let mut sink = Memory { data: vec![], length: 0, capacity: 1 << 20, closed: None };
        let mut staging = vec![0u8; 4096];
        let mut recorder = Recorder::begin(&mut sink, &mut staging, FORMAT, b"take.wav", 0).unwrap();
        let audio: Vec<u8> = (0..10_000u32).map(|n| (n % 251) as u8).collect();
        assert_eq!(recorder.push(&audio).unwrap(), audio.len());
        let summary = recorder.finish().unwrap();
        assert_eq!(summary.bytes, 80 + 10_000);
        assert_eq!(summary.frames, 2500);
        assert_eq!(sink.closed, Some(80 + 10_000));
        assert_eq!(sink.length, 80 + 10_000);
        assert_eq!(data_len(&sink.data), 10_000);
        assert_eq!(&sink.data[80..80 + 10_000], &audio[..]);
    }

    #[test]
    fn a_checkpoint_keeps_the_take_so_far() {
        let mut sink = Memory { data: vec![], length: 0, capacity: 1 << 20, closed: None };
        let mut staging = vec![0u8; 4096];
        let mut recorder = Recorder::begin(&mut sink, &mut staging, FORMAT, b"take.wav", 0).unwrap();
        recorder.push(&[7u8; 10_000]).unwrap();
        recorder.checkpoint().unwrap();
        // Power cut here: three blocks of 4096 are written (the fourth is still staged), the header matches.
        drop(recorder);
        assert_eq!(sink.length, 8192);
        assert_eq!(data_len(&sink.data), 8192 - 80);
        assert_eq!(sink.closed, None);
    }

    #[test]
    fn a_full_file_takes_what_fits_and_reports_it() {
        let mut sink = Memory { data: vec![], length: 0, capacity: 5000, closed: None };
        let mut staging = vec![0u8; 1024];
        let mut recorder = Recorder::begin(&mut sink, &mut staging, FORMAT, b"take.wav", 0).unwrap();
        let taken = recorder.push(&[1u8; 20_000]).unwrap();
        assert_eq!(taken, 5000 - 80);
        assert_eq!(recorder.room(), 0);
        let summary = recorder.finish().unwrap();
        assert_eq!(summary.bytes, 5000);
        assert_eq!(data_len(&sink.data), 5000 - 80);
    }

    #[test]
    fn a_short_take_still_has_a_header() {
        let mut sink = Memory { data: vec![], length: 0, capacity: 1 << 20, closed: None };
        let mut staging = vec![0u8; 1024];
        let mut recorder = Recorder::begin(&mut sink, &mut staging, FORMAT, b"take.wav", 0).unwrap();
        recorder.push(&[3u8; 100]).unwrap();
        recorder.finish().unwrap();
        assert_eq!(data_len(&sink.data), 100);
        assert_eq!(sink.length, 180);
    }
}
