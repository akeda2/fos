//! Streaming decoders for the media player. A decoder reads its file through a `Source` (a local FOSFS file, an SMB file,
//! a byte slice in the tests) in small pieces and produces blocks of samples; nothing here allocates or needs `std`.
//!
//! Today: FLAC (`flac`), written from the format specification in plain Rust: no C library, no floating point except the
//! final scaling to `f32`.
#![cfg_attr(not(test), no_std)]

pub mod flac;
pub mod resample;

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
