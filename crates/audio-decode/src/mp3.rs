//! MP3 (MPEG-1/2/2.5 Layer III, and Layer I/II) through `nanomp3-core`, a safe `no_std` Rust port of minimp3. This
//! wrapper only feeds it from a `Source`: it keeps a 16 KiB window of the file (the decoder wants several frames
//! ahead), skips tags, and hands out one frame (1152 or 576 samples per channel) at a time as `f32`. Gapless trimming
//! of the encoder delay is not done.
use nanomp3_core::{DecodeError, Decoder as Core, MAX_SAMPLES_PER_FRAME};

use crate::{Decode, Error, Source};

const INPUT: usize = 16384;
/// Refill when fewer bytes than this remain in the window.
const LOW_WATER: usize = 8192;

pub struct Decoder<S: Source> {
    source: S,
    core: Core,
    buffer: [u8; INPUT],
    position: usize,
    end: usize,
    next_offset: u64,
    at_end: bool,
    pcm: [f32; MAX_SAMPLES_PER_FRAME],
    /// Samples per channel of the block `pcm` holds.
    block: usize,
    /// A frame decoded by `open` and not yet handed out.
    pending: bool,
    pub sample_rate: u32,
    pub channels: u32,
    /// Estimated from the file size and the first frame's bitrate (exact for constant bitrate).
    pub total_samples: u64,
}

impl<S: Source> Decoder<S> {
    pub const fn new(source: S) -> Self {
        Decoder {
            source,
            core: Core::new(),
            buffer: [0; INPUT],
            position: 0,
            end: 0,
            next_offset: 0,
            at_end: false,
            pcm: [0.0; MAX_SAMPLES_PER_FRAME],
            block: 0,
            pending: false,
            sample_rate: 0,
            channels: 0,
            total_samples: 0,
        }
    }

    pub fn source(&mut self) -> &mut S {
        &mut self.source
    }

    fn restart(&mut self) {
        self.core = Core::new();
        self.position = 0;
        self.end = 0;
        self.next_offset = 0;
        self.at_end = false;
        self.pending = false;
        self.block = 0;
    }

    fn refill(&mut self) -> Result<(), Error> {
        if self.at_end || self.end - self.position >= LOW_WATER {
            return Ok(());
        }
        self.buffer.copy_within(self.position..self.end, 0);
        self.end -= self.position;
        self.position = 0;
        while self.end < INPUT {
            let got = self.source.read_at(self.next_offset, &mut self.buffer[self.end..])?;
            if got == 0 {
                self.at_end = true;
                break;
            }
            self.end += got;
            self.next_offset += got as u64;
        }
        Ok(())
    }

    /// Decodes up to the next frame that carries audio; `None` at the end of the file.
    fn frame(&mut self) -> Result<Option<usize>, Error> {
        loop {
            self.refill()?;
            if self.position == self.end {
                return Ok(None);
            }
            let (used, result) = self.core.decode(&self.buffer[self.position..self.end], &mut self.pcm);
            // Always step over what was consumed; a decoder that consumed nothing must not stall us.
            self.position += if used == 0 { 1 } else { used }.min(self.end - self.position);
            match result {
                Ok(info) if info.samples_produced > 0 => {
                    self.sample_rate = info.sample_rate;
                    self.channels = info.channels.num() as u32;
                    self.block = info.samples_produced;
                    return Ok(Some(info.samples_produced));
                }
                Ok(_) | Err(DecodeError::ReservoirUnavailable(_)) | Err(DecodeError::Corrupt(_)) => {}
                Err(DecodeError::NoFrame) => {
                    if self.at_end && self.position >= self.end {
                        return Ok(None);
                    }
                }
                Err(_) => return Err(Error::Unsupported("an MPEG audio layer this decoder was built without")),
            }
        }
    }

    /// Finds the first audio frame (skipping tags) to learn the rate and channels, and estimates the length.
    pub fn open(&mut self) -> Result<(), Error> {
        self.restart();
        let size = self.source.size();
        match self.frame()? {
            Some(_) => self.pending = true,
            None => return Err(Error::Format("no MPEG audio frame found")),
        }
        // The bitrate of the first frame stands for the file; the position of that frame is the tag's length.
        let mut probe = Core::new();
        let mut scratch = [0f32; MAX_SAMPLES_PER_FRAME];
        let bitrate = match probe.decode(&self.buffer[..self.end.min(INPUT)], &mut scratch) {
            (_, Ok(info)) | (_, Err(DecodeError::ReservoirUnavailable(info))) => info.bitrate,
            _ => 0,
        };
        self.total_samples = if bitrate > 0 { size * 8 / (bitrate as u64 * 1000) * self.sample_rate as u64 } else { 0 };
        Ok(())
    }

    pub fn rewind(&mut self) {
        let _ = self.open();
    }
}

impl<S: Source> Decode for Decoder<S> {
    fn open(&mut self) -> Result<(), Error> {
        Decoder::open(self)
    }

    fn next_block(&mut self) -> Result<Option<usize>, Error> {
        if self.pending {
            self.pending = false;
            return Ok(Some(self.block));
        }
        self.frame()
    }

    fn to_f32_stereo(&self, start: usize, count: usize, out: &mut [f32]) {
        for i in 0..count {
            let at = start + i;
            if self.channels == 2 {
                out[2 * i] = self.pcm[2 * at];
                out[2 * i + 1] = self.pcm[2 * at + 1];
            } else {
                out[2 * i] = self.pcm[at];
                out[2 * i + 1] = self.pcm[at];
            }
        }
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u32 {
        self.channels
    }

    fn bits_per_sample(&self) -> u32 {
        0
    }

    fn total_samples(&self) -> u64 {
        self.total_samples
    }
}
