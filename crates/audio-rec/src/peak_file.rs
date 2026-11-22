//! Peak files: the lowest and highest sample of each channel over every `frames_per_peak` frames of an audio file,
//! written beside it with the same name and `.peaks` for the extension (`take007_in01-08.peaks` beside
//! `take007_in01-08.flac`), so a player shows the file's overview at once instead of reading all of it. KakelFix's
//! Recorder writes them and its Play page reads them; any recorder or player can.
//!
//! A recorder collects them while it records (`PeakLog`, in memory; a long take makes them coarser rather than running
//! out of room) and writes the file when the take stops, after the audio file is closed. A take cut short (power,
//! crash) has none, and a player reads the audio instead. A `PeakLog` can hold more channels than one file: `header`
//! and `body` give the file of any run of them (a take split into files of a few channels each).
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

/// A peak file's extension.
pub const EXTENSION: &str = ".peaks";

/// A peak file's name beside audio file `audio`, written to `out` (the same name with `EXTENSION` for the extension);
/// false when `audio` has no extension or `out` had no room (a fixed-size name drops what does not fit).
pub fn file_name(audio: &str, out: &mut impl core::fmt::Write) -> bool {
    let Some((stem, _)) = audio.rsplit_once('.') else { return false };
    out.write_str(stem).is_ok() && out.write_str(EXTENSION).is_ok()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_log_keeps_peaks_and_gets_coarser_rather_than_run_out() {
        let mut cells = [0u32; 2 * 64];
        let mut log = PeakLog::new(&mut cells);
        log.start(2);
        // Two blocks that cross a peak boundary in the middle: 700 frames, then 400.
        let per = FIRST_FRAMES_PER_PEAK as usize;
        for (length, value) in [(700usize, 0.5f32), (400, -0.25)] {
            log.reserve(length);
            let left = [value; 700];
            let right = [-value; 700];
            log.fold(0, &left[..length]);
            log.fold(1, &right[..length]);
            log.advance(length);
        }
        assert_eq!((log.frames(), log.peaks(), log.frames_per_peak() as usize), (1100, 3, per));
        let mut body = [0u8; 3 * 2 * 4];
        assert_eq!(log.body(0, 2, 0, &mut body), body.len());
        let cell = |peak: usize, channel: usize| unpack(u32::from_le_bytes(body[(peak * 2 + channel) * 4..][..4].try_into().unwrap()));
        assert_eq!(cell(0, 0), (16383, 16383), "the first peak saw only the first block");
        assert_eq!(cell(1, 0), (-8191, 16383), "the second straddles both blocks");
        assert_eq!(cell(1, 1), (-16383, 8191));
        assert_eq!(cell(2, 0), (-8191, -8191));
        // Channel 1 alone, for a file of that channel.
        let mut one = [0u8; 3 * 4];
        assert_eq!(log.body(1, 1, 0, &mut one), one.len());
        assert_eq!(unpack(u32::from_le_bytes(one[4..8].try_into().unwrap())), (-16383, 8191));
        // Past the 64 peaks the cells hold, pairs merge: the frames per peak double, nothing is lost.
        log.reserve(per * 70);
        let ramp = [0.1f32; 512 * 70];
        log.fold(0, &ramp[..per * 70]);
        log.fold(1, &ramp[..per * 70]);
        log.advance(per * 70);
        assert_eq!(log.frames_per_peak() as usize, per * 2);
        assert!(log.peaks() <= 64);
        let mut merged = [0u8; 4];
        log.body(0, 1, 0, &mut merged);
        assert_eq!(unpack(u32::from_le_bytes(merged)), (-8191, 16383), "the first two peaks, merged");
        assert_ne!(u32::from_le_bytes(merged), EMPTY);
        // The header goes out and comes back; a file shorter than its peaks, or another file, is refused.
        let header = log.header(1, 48_000);
        let mut bytes = [0u8; HEADER];
        header.write(&mut bytes);
        let length = HEADER as u64 + header.peaks * 4;
        assert_eq!(Header::parse(&bytes, length), Some(header));
        assert_eq!(Header::parse(&bytes, length - 1), None);
        assert_eq!(Header::parse(b"RIFF\0\0\0\0WAVEfmt ", 1 << 20), None);
        assert_eq!(header.offset(2), HEADER as u64 + 8);
    }

    #[test]
    fn the_name_is_the_audio_file_s_with_peaks_for_the_extension() {
        let mut name = String::new();
        assert!(file_name("Konsert_take007_2026-11-20_143512_in01-08.flac", &mut name));
        assert_eq!(name, "Konsert_take007_2026-11-20_143512_in01-08.peaks");
        assert!(!file_name("no-extension", &mut String::new()));
    }
}
