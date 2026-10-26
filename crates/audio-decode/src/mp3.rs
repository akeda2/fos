//! MP3 (MPEG-1/2/2.5 Layer III, and Layer I/II) through `nanomp3-core`, a safe `no_std` Rust port of minimp3. This
//! wrapper only feeds it from a `Source`: it keeps a 16 KiB window of the file (the decoder wants several frames
//! ahead), skips tags, and hands out one frame (1152 or 576 samples per channel) at a time as `f32`. Gapless trimming
//! of the encoder delay is not done.
use nanomp3_core::{DecodeError, Decoder as Core, MAX_SAMPLES_PER_FRAME};

use crate::tags::Tags;
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
    pub tags: Tags,
    /// Samples per channel handed out so far (after a seek: the estimated start).
    decoded: u64,
    /// Where the audio starts (after an ID3v2 tag).
    data_start: u64,
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
            tags: Tags::new(),
            decoded: 0,
            data_start: 0,
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
        self.decoded = 0;
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
                    self.decoded += info.samples_produced as u64;
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
        self.read_tags();
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

    /// Title, artist and album from an ID3v2 tag at the start, else from an ID3v1 tag in the last 128 bytes. Best effort:
    /// a tag that cannot be read leaves the fields empty and never stops the file from playing.
    fn read_tags(&mut self) {
        self.tags.clear();
        self.data_start = 0;
        let _ = self.id3v2();
        if self.tags.is_empty() {
            let _ = self.id3v1();
        }
    }

    fn id3v2(&mut self) -> Result<(), Error> {
        let mut head = [0u8; 10];
        if self.source.read_at(0, &mut head)? < 10 || &head[..3] != b"ID3" {
            return Ok(());
        }
        let version = head[3];
        let synchsafe = |b: &[u8]| b.iter().fold(0u64, |n, &x| (n << 7) | (x & 0x7F) as u64);
        let end = 10 + synchsafe(&head[6..10]);
        self.data_start = end;
        // Frame header: 6 bytes (id 3, size 3) in v2.2, else 10 (id 4, size 4, flags 2).
        let (id_len, header) = if version == 2 { (3, 6) } else { (4, 10) };
        let mut at = 10u64;
        if head[5] & 0x40 != 0 && version >= 3 {
            // An extended header: its size (synchsafe in v2.4, plain in v2.3, not counting these four bytes).
            let mut size = [0u8; 4];
            self.source.read_at(at, &mut size)?;
            at += if version == 4 { synchsafe(&size) } else { u32::from_be_bytes(size) as u64 + 4 };
        }
        while at + header as u64 <= end {
            let mut frame = [0u8; 10];
            if self.source.read_at(at, &mut frame[..header])? < header || frame[0] == 0 {
                break;
            }
            let size = match (version, &frame[id_len..header]) {
                (2, b) => u32::from_be_bytes([0, b[0], b[1], b[2]]) as u64,
                (4, b) => synchsafe(&b[..4]),
                (_, b) => u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as u64,
            };
            let body = at + header as u64;
            let field = match &frame[..id_len] {
                b"TIT2" | b"TT2" => Some(0),
                b"TPE1" | b"TP1" => Some(1),
                b"TALB" | b"TAL" => Some(2),
                _ => None,
            };
            if let (Some(which), true) = (field, size > 1) {
                let mut data = [0u8; 200];
                let keep = ((size - 1) as usize).min(data.len());
                let mut encoding = [0u8; 1];
                self.source.read_at(body, &mut encoding)?;
                let got = self.source.read_at(body + 1, &mut data[..keep])?;
                let data = &data[..got];
                let text = match which {
                    0 => &mut self.tags.title,
                    1 => &mut self.tags.artist,
                    _ => &mut self.tags.album,
                };
                match encoding[0] {
                    0 => text.set_latin1(data),
                    1 => text.set_utf16(data, false),
                    2 => text.set_utf16(data, true),
                    _ => text.set_utf8(data.split(|&b| b == 0).next().unwrap_or(&[])),
                }
            }
            at = body + size;
        }
        Ok(())
    }

    /// ID3v1: "TAG", then 30 bytes each of title, artist and album, padded with NULs or spaces.
    fn id3v1(&mut self) -> Result<(), Error> {
        let size = self.source.size();
        if size < 128 {
            return Ok(());
        }
        let mut tag = [0u8; 128];
        if self.source.read_at(size - 128, &mut tag)? < 128 || &tag[..3] != b"TAG" {
            return Ok(());
        }
        for (text, range) in [(&mut self.tags.title, 3..33), (&mut self.tags.artist, 33..63), (&mut self.tags.album, 63..93)] {
            text.set_latin1(&tag[range]);
        }
        Ok(())
    }

    /// Samples per channel before the block `next_block` returns next.
    pub fn position(&self) -> u64 {
        self.decoded - if self.pending { self.block as u64 } else { 0 }
    }

    /// Jumps to the byte that the bitrate says holds `target`: exact for constant bitrate, an estimate for VBR. The
    /// decoder resynchronises on the next frame; the first frames may be dropped while its bit reservoir refills.
    pub fn seek(&mut self, target: u64) -> Result<u64, Error> {
        let size = self.source.size();
        let target = if self.total_samples > 0 { target.min(self.total_samples.saturating_sub(1)) } else { 0 };
        let byte = if self.total_samples > 0 { (size as u128 * target as u128 / self.total_samples as u128) as u64 } else { 0 };
        self.core = Core::new();
        self.position = 0;
        self.end = 0;
        self.at_end = false;
        self.pending = false;
        self.next_offset = byte.max(self.data_start).min(size);
        self.decoded = target;
        Ok(target)
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

    fn tags(&self) -> &Tags {
        &self.tags
    }

    fn position(&self) -> u64 {
        Decoder::position(self)
    }

    fn seek(&mut self, sample: u64) -> Result<u64, Error> {
        Decoder::seek(self, sample)
    }

    fn format_name(&self) -> &'static str {
        "MP3"
    }
}
