//! A FLAC file written through a `Sink`, a block at a time.
//!
//! The write discipline is the recorder's (`audio_rec::recorder`): blocks go out in whole sectors; the file's length and the
//! STREAMINFO in its first sector move together (at the first block, at `checkpoint` and at `finish`), and the first sector
//! is kept in memory so STREAMINFO can be rewritten without reading anything back. The encoder's frames are 4096 samples
//! (`flac_enc::BLOCK`); samples are collected until a block is full.
//!
//! About 360 KiB with all its buffers: keep one in a `static`.

use audio_rec::recorder::{SECTOR, Sink};
use flac_enc::{BLOCK, Encoder, Info, max_frame_bytes};

/// Most channels a file can have here (5.1).
pub const MAX_CHANNELS: usize = 6;
/// Bytes collected before a write to the disk.
pub const STAGE: usize = 128 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The sink refused (its negative code).
    Sink(i32),
    /// Channels or bit depth the encoder does not take, or tags too long for the header.
    Format,
    /// The file reached the size the sink offered.
    Full,
    /// `begin` was not called (or `finish` has been).
    NotOpen,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Summary {
    pub bytes: u64,
    pub samples: u64,
}

pub struct FlacFile {
    encoder: Encoder,
    channels: usize,
    block: [[i32; BLOCK]; MAX_CHANNELS],
    fill: usize,
    frame: [u8; max_frame_bytes(MAX_CHANNELS)],
    stage: [u8; STAGE],
    staged: usize,
    offset: u64,
    limit: u64,
    first: [u8; SECTOR],
    have_first: bool,
    open: bool,
}

impl FlacFile {
    pub const fn new() -> FlacFile {
        FlacFile {
            encoder: Encoder::new(Info { sample_rate: 48000, channels: 2, bits: 16 }),
            channels: 2,
            block: [[0; BLOCK]; MAX_CHANNELS],
            fill: 0,
            frame: [0; max_frame_bytes(MAX_CHANNELS)],
            stage: [0; STAGE],
            staged: 0,
            offset: 0,
            limit: 0,
            first: [0; SECTOR],
            have_first: false,
            open: false,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Samples per channel taken so far (including those still waiting for a full block).
    pub fn samples(&self) -> u64 {
        self.encoder.total_samples() + self.fill as u64
    }

    /// Bytes handed to the sink so far.
    pub fn bytes(&self) -> u64 {
        self.offset + self.staged as u64
    }

    /// Creates the file and writes the header (`tags` as Vorbis comments) into the first block; nothing reaches the disk yet.
    /// `reserve` is the size the sink should make room for (0 = as much as possible).
    pub fn begin(&mut self, sink: &mut impl Sink, name: &[u8], reserve: u64, info: Info, tags: &[(&str, &str)]) -> Result<(), Error> {
        if info.channels == 0 || info.channels as usize > MAX_CHANNELS {
            return Err(Error::Format);
        }
        let encoder = Encoder::new(info);
        let header = encoder.write_header(&mut self.stage, tags).map_err(|_| Error::Format)?;
        self.limit = sink.open(name, reserve).map_err(Error::Sink)?;
        self.encoder = encoder;
        self.channels = info.channels as usize;
        self.fill = 0;
        self.staged = header;
        self.offset = 0;
        self.have_first = false;
        self.open = true;
        Ok(())
    }

    /// Appends samples: `chans[c]` is channel `c`, all the same length (any length; blocks form as they fill).
    pub fn push(&mut self, sink: &mut impl Sink, chans: &[&[i32]]) -> Result<(), Error> {
        if !self.open {
            return Err(Error::NotOpen);
        }
        if chans.len() != self.channels || chans.iter().any(|c| c.len() != chans[0].len()) {
            return Err(Error::Format);
        }
        let mut done = 0;
        while done < chans[0].len() {
            let count = (BLOCK - self.fill).min(chans[0].len() - done);
            for (channel, samples) in chans.iter().enumerate() {
                self.block[channel][self.fill..self.fill + count].copy_from_slice(&samples[done..done + count]);
            }
            self.fill += count;
            done += count;
            if self.fill == BLOCK {
                self.emit(sink)?;
            }
        }
        Ok(())
    }

    fn emit(&mut self, sink: &mut impl Sink) -> Result<(), Error> {
        let n = self.fill;
        self.fill = 0;
        let refs: [&[i32]; MAX_CHANNELS] = core::array::from_fn(|c| &self.block[c][..if c < self.channels { n } else { 0 }]);
        let length = self.encoder.encode_frame(&refs[..self.channels], &mut self.frame).map_err(|_| Error::Format)?;
        if self.offset + self.staged as u64 + length as u64 > self.limit {
            return Err(Error::Full);
        }
        let mut taken = 0;
        while taken < length {
            let count = (STAGE - self.staged).min(length - taken);
            self.stage[self.staged..self.staged + count].copy_from_slice(&self.frame[taken..taken + count]);
            self.staged += count;
            taken += count;
            if self.staged == STAGE {
                self.flush(sink, false)?;
            }
        }
        Ok(())
    }

    fn flush(&mut self, sink: &mut impl Sink, last: bool) -> Result<(), Error> {
        if self.staged == 0 {
            return Ok(());
        }
        if self.offset == 0 {
            if last {
                self.encoder.patch_header(&mut self.stage[..42]);
            }
            if self.staged >= SECTOR {
                self.first.copy_from_slice(&self.stage[..SECTOR]);
                self.have_first = !last;
            }
        }
        // The first block sets the length along with its header; later ones leave it alone until a checkpoint or the end.
        let length = if self.offset == 0 || last { self.offset + self.staged as u64 } else { 0 };
        sink.write(self.offset, &self.stage[..self.staged], length).map_err(Error::Sink)?;
        self.offset += self.staged as u64;
        self.staged = 0;
        Ok(())
    }

    /// Brings STREAMINFO and the file length up to date so that a reset now keeps what has been written. One sector; call
    /// every second or two.
    pub fn checkpoint(&mut self, sink: &mut impl Sink) -> Result<(), Error> {
        if !self.open || !self.have_first {
            return Ok(());
        }
        let mut first = self.first;
        self.encoder.patch_header(&mut first[..42]);
        sink.write(0, &first, self.offset).map_err(Error::Sink)
    }

    /// Encodes the last partial block, writes the rest and the final STREAMINFO, and closes the file.
    pub fn finish(&mut self, sink: &mut impl Sink) -> Result<Summary, Error> {
        if !self.open {
            return Err(Error::NotOpen);
        }
        if self.fill > 0 {
            self.emit(sink)?;
        }
        self.flush(sink, true)?;
        let total = self.offset;
        if self.have_first {
            let mut first = self.first;
            self.encoder.patch_header(&mut first[..42]);
            sink.write(0, &first, total).map_err(Error::Sink)?;
        }
        sink.close(total).map_err(Error::Sink)?;
        self.open = false;
        Ok(Summary { bytes: total, samples: self.encoder.total_samples() })
    }

    /// Gives the file up after an error: closes the sink as it stands.
    pub fn abandon(&mut self, sink: &mut impl Sink) {
        if self.open {
            let _ = sink.close(self.offset);
            self.open = false;
        }
    }
}

impl Default for FlacFile {
    fn default() -> Self {
        Self::new()
    }
}
