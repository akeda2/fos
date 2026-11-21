//! Peak files: the lowest and highest sample of each input over every `frames_per_peak` frames of a take, written by the
//! Recorder beside each audio file (`Konsert_take007_..._in01-08.peaks` beside `..._in01-08.flac`), so the Play page
//! shows a take's overview at once instead of reading the whole take.
//!
//! The Recorder collects them while it records (`PeakLog`, in memory; a long take makes them coarser rather than running
//! out of room) and writes the files when the take stops, after the audio files are closed. A take cut short (power,
//! crash) has none, and the Play page reads the audio instead.
//!
//! The file, all little-endian:
//!
//! ```text
//! 0   "FOSPEAKS"
//! 8   u32 version (1)
//! 12  u32 channels (the audio file's)
//! 16  u32 sample rate
//! 20  u32 frames per peak
//! 24  u64 peaks
//! 32  u64 frames (the audio file's)
//! 40  zeros up to 64
//! 64  peaks x channels x (i16 lowest, i16 highest), sample values scaled to 32767
//! ```

pub const MAGIC: &[u8; 8] = b"FOSPEAKS";
pub const VERSION: u32 = 1;
pub const HEADER: usize = 64;
/// Frames per peak at the start of a take (about 11 ms at 48 kHz); doubled each time the log fills.
pub const FIRST_FRAMES_PER_PEAK: u32 = 512;
/// A cell nothing was folded into: lowest above highest.
pub const EMPTY: u32 = pack(i16::MAX, i16::MIN);

pub const fn pack(low: i16, high: i16) -> u32 {
    (low as u16 as u32) | ((high as u16 as u32) << 16)
}

pub fn unpack(cell: u32) -> (i16, i16) {
    (cell as u16 as i16, (cell >> 16) as u16 as i16)
}

fn scale(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * 32767.0) as i16
}

fn merge(a: u32, b: u32) -> u32 {
    let ((al, ah), (bl, bh)) = (unpack(a), unpack(b));
    pack(al.min(bl), ah.max(bh))
}

/// A peak file's header.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Header {
    pub channels: u32,
    pub rate: u32,
    pub frames_per_peak: u32,
    pub peaks: u64,
    pub frames: u64,
}

impl Header {
    pub fn write(&self, out: &mut [u8; HEADER]) {
        out.fill(0);
        out[..8].copy_from_slice(MAGIC);
        out[8..12].copy_from_slice(&VERSION.to_le_bytes());
        out[12..16].copy_from_slice(&self.channels.to_le_bytes());
        out[16..20].copy_from_slice(&self.rate.to_le_bytes());
        out[20..24].copy_from_slice(&self.frames_per_peak.to_le_bytes());
        out[24..32].copy_from_slice(&self.peaks.to_le_bytes());
        out[32..40].copy_from_slice(&self.frames.to_le_bytes());
    }

    /// The header from a file's first bytes and its length: None unless it is one this reader knows and the file holds
    /// every peak it says.
    pub fn parse(head: &[u8], file_bytes: u64) -> Option<Header> {
        let u32_at = |at: usize| Some(u32::from_le_bytes(head.get(at..at + 4)?.try_into().ok()?));
        let u64_at = |at: usize| Some(u64::from_le_bytes(head.get(at..at + 8)?.try_into().ok()?));
        if head.get(..8)? != MAGIC || u32_at(8)? != VERSION {
            return None;
        }
        let header = Header { channels: u32_at(12)?, rate: u32_at(16)?, frames_per_peak: u32_at(20)?, peaks: u64_at(24)?, frames: u64_at(32)? };
        let body = header.peaks.checked_mul(u64::from(header.channels) * 4)?;
        let sane = (1..=8).contains(&header.channels) && header.frames_per_peak > 0 && header.peaks == header.frames.div_ceil(u64::from(header.frames_per_peak));
        (sane && file_bytes >= HEADER as u64 + body).then_some(header)
    }

    /// Where peak `peak` starts in the file.
    pub fn offset(&self, peak: u64) -> u64 {
        HEADER as u64 + peak * u64::from(self.channels) * 4
    }
}

/// A peak file's name beside its audio file: the same name with `.peaks` for the extension.
pub fn file_name(audio: &str) -> Option<crate::naming::FileName> {
    let stem = audio.rsplit_once('.')?.0;
    let mut name = crate::naming::FileName::from(stem);
    let _ = core::fmt::Write::write_str(&mut name, ".peaks");
    (name.as_str().len() == stem.len() + 6).then_some(name)
}

/// The peaks of a take as it is recorded: `channels` inputs, a cell per input per peak in `cells` (the caller's memory).
/// When the cells are used up the peaks are merged in pairs and `frames_per_peak` doubles, so any length fits.
pub struct PeakLog<'a> {
    cells: &'a mut [u32],
    channels: usize,
    frames_per_peak: u64,
    /// Frames folded in, and peaks whose cells are set up (`EMPTY` until folded).
    frames: u64,
    ready: usize,
}

impl<'a> PeakLog<'a> {
    pub fn new(cells: &'a mut [u32]) -> Self {
        PeakLog { cells, channels: 0, frames_per_peak: u64::from(FIRST_FRAMES_PER_PEAK), frames: 0, ready: 0 }
    }

    /// Empties it for a take of `channels` inputs.
    pub fn start(&mut self, channels: usize) {
        self.channels = channels.max(1);
        (self.frames_per_peak, self.frames, self.ready) = (u64::from(FIRST_FRAMES_PER_PEAK), 0, 0);
    }

    fn capacity(&self) -> usize {
        self.cells.len() / self.channels.max(1)
    }

    /// Makes room for `frames` more frames: peaks set up to the last one they reach, merged in pairs when there is no
    /// room. Call once per block, before `fold`ing each input's samples of it.
    pub fn reserve(&mut self, frames: usize) {
        let needed = |log: &Self| (log.frames + frames as u64).div_ceil(log.frames_per_peak) as usize;
        while needed(self) > self.capacity() && self.capacity() >= 2 {
            self.halve();
        }
        let wanted = needed(self).min(self.capacity());
        let channels = self.channels;
        if wanted > self.ready {
            self.cells[self.ready * channels..wanted * channels].fill(EMPTY);
            self.ready = wanted;
        }
    }

    /// Merges the peaks in pairs: half as many, each twice as long.
    fn halve(&mut self) {
        let channels = self.channels;
        let pairs = self.ready.div_ceil(2);
        for peak in 0..pairs {
            for channel in 0..channels {
                let a = self.cells[2 * peak * channels + channel];
                let b = if 2 * peak + 1 < self.ready { self.cells[(2 * peak + 1) * channels + channel] } else { EMPTY };
                self.cells[peak * channels + channel] = merge(a, b);
            }
        }
        self.ready = pairs;
        self.frames_per_peak *= 2;
    }

    /// Folds input `channel`'s samples of the block `reserve` made room for.
    pub fn fold(&mut self, channel: usize, samples: &[f32]) {
        if channel >= self.channels {
            return;
        }
        let per = self.frames_per_peak;
        let mut index = 0;
        while index < samples.len() {
            let frame = self.frames + index as u64;
            let peak = (frame / per) as usize;
            if peak >= self.ready {
                break;
            }
            let end = (((peak as u64 + 1) * per - self.frames) as usize).min(samples.len());
            let cell = &mut self.cells[peak * self.channels + channel];
            let (mut low, mut high) = unpack(*cell);
            for &sample in &samples[index..end] {
                let value = scale(sample);
                (low, high) = (low.min(value), high.max(value));
            }
            *cell = pack(low, high);
            index = end;
        }
    }

    /// The block is folded in for every input: the frames move on.
    pub fn advance(&mut self, frames: usize) {
        self.frames += frames as u64;
    }

    pub fn frames(&self) -> u64 {
        self.frames
    }

    pub fn frames_per_peak(&self) -> u32 {
        self.frames_per_peak.min(u64::from(u32::MAX)) as u32
    }

    pub fn peaks(&self) -> u64 {
        self.frames.div_ceil(self.frames_per_peak)
    }

    /// The header of a file holding inputs `first..first + channels` of the log.
    pub fn header(&self, channels: usize, rate: u32) -> Header {
        Header { channels: channels as u32, rate, frames_per_peak: self.frames_per_peak(), peaks: self.peaks(), frames: self.frames }
    }

    /// Bytes `at..` of the body of the file of inputs `first..first + channels`, into `out` (whole cells); how many.
    pub fn body(&self, first: usize, channels: usize, at: u64, out: &mut [u8]) -> usize {
        let cell_bytes = 4 * channels;
        let total = self.peaks() as usize * cell_bytes;
        let (start, end) = ((at as usize).min(total), (at as usize + out.len() / 4 * 4).min(total));
        for (offset, chunk) in (start..end).step_by(4).zip(out.chunks_exact_mut(4)) {
            let (peak, channel) = (offset / cell_bytes, offset % cell_bytes / 4);
            let cell = if peak < self.ready { self.cells[peak * self.channels + first + channel] } else { EMPTY };
            chunk.copy_from_slice(&cell.to_le_bytes());
        }
        end - start
    }
}
