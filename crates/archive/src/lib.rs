//! What the phono tape needs to archive a record side, without hardware so it can be tested on the host:
//!
//! - `file`: a FLAC file written through an `audio_rec::recorder::Sink` (sector-aligned blocks, the header kept current at
//!   checkpoints so a reset keeps the take);
//! - `names`: the output file names from one user-chosen base name;
//! - `transcode`: the second pass, a raw Lt/Rt FLAC take decoded (SQ, QS, ambience) into a 4.0 / 4.1 / 5.1 FLAC whose
//!   bit depth comes from the noise floor of the whole take.
//!
//! The raw take is the master: it holds the samples exactly as the device delivered them. The decoded file is a
//! derived copy that can be made again with other settings.
#![cfg_attr(not(test), no_std)]

pub mod file;
pub mod names;
pub mod transcode;

pub use file::{Error, FlacFile, Summary};
pub use names::{Names, NameError};
pub use transcode::{Depth, Progress, Report, Transcoder};

/// A whole file in memory as a decoder source.
pub struct Bytes<'a>(pub &'a [u8]);

impl audio_decode::Source for Bytes<'_> {
    fn size(&self) -> u64 {
        self.0.len() as u64
    }

    fn read_at(&mut self, offset: u64, out: &mut [u8]) -> Result<usize, audio_decode::SourceError> {
        let start = (offset as usize).min(self.0.len());
        let count = out.len().min(self.0.len() - start);
        out[..count].copy_from_slice(&self.0[start..start + count]);
        Ok(count)
    }
}

#[cfg(test)]
mod tests;
