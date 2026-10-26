//! Streaming decoders for the media player. A decoder reads its file through a `Source` (a local FOSFS file, an SMB file,
//! a byte slice in the tests) in small pieces and produces blocks of samples; nothing here allocates or needs `std`.
//!
//! Today: WAV (`wav`), FLAC (`flac`), written from the format specification in plain Rust: no C library, no floating point except the
//! final scaling to `f32`; and MP3 (`mp3`), through the `nanomp3-core` crate. Both implement `Decode`.
#![cfg_attr(not(test), no_std)]

pub mod flac;
pub mod mp3;
pub mod resample;
pub mod tags;
pub mod wav;

/// What the player needs from any decoder: blocks of audio, converted to stereo `f32` on request.
pub trait Decode {
    fn open(&mut self) -> Result<(), Error>;
    /// Decodes the next block and returns its length in samples per channel; `None` at the end.
    fn next_block(&mut self) -> Result<Option<usize>, Error>;
    /// `count` samples from `start` of the current block as interleaved stereo `f32` in [-1, 1] (mono is duplicated).
    fn to_f32_stereo(&self, start: usize, count: usize, out: &mut [f32]);
    fn sample_rate(&self) -> u32;
    fn channels(&self) -> u32;
    /// 0 for a lossy format.
    fn bits_per_sample(&self) -> u32;
    /// 0 when unknown.
    fn total_samples(&self) -> u64;
    /// Title, artist and album found in the file's metadata (empty when there are none); valid after `open`.
    fn tags(&self) -> &tags::Tags;
    /// Samples per channel before the block `next_block` returns next.
    fn position(&self) -> u64;
    /// Moves to the block holding `sample` (per channel) and returns that block's first sample; exact for FLAC, an
    /// estimate from the bitrate for MP3.
    fn seek(&mut self, sample: u64) -> Result<u64, Error>;
    /// "FLAC", "MP3", "WAV"...
    fn format_name(&self) -> &'static str;
}

/// Random-access bytes of one file.
pub trait Source {
    /// The file's length.
    fn size(&self) -> u64;
    /// Reads up to `out.len()` bytes at `offset`; 0 means the end of the file.
    fn read_at(&mut self, offset: u64, out: &mut [u8]) -> Result<usize, SourceError>;
}

/// A source could not deliver (the network failed, the disk read failed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Source,
    /// The data is not what the format allows (what was wrong).
    Format(&'static str),
    /// Valid, but beyond what this decoder does (what).
    Unsupported(&'static str),
}

impl Error {
    pub fn describe(&self) -> &'static str {
        match self {
            Error::Source => "the file could not be read",
            Error::Format(why) | Error::Unsupported(why) => why,
        }
    }
}

impl From<SourceError> for Error {
    fn from(_: SourceError) -> Self {
        Error::Source
    }
}

/// A whole file in memory, for tests and small clips.
impl Source for &[u8] {
    fn size(&self) -> u64 {
        self.len() as u64
    }

    fn read_at(&mut self, offset: u64, out: &mut [u8]) -> Result<usize, SourceError> {
        let start = (offset as usize).min(self.len());
        let count = out.len().min(self.len() - start);
        out[..count].copy_from_slice(&self[start..start + count]);
        Ok(count)
    }
}
