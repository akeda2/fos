//! WAV (RIFF/WAVE): integer PCM at 8, 16, 24 and 32 bits and 32-bit float, mono or stereo, also in the
//! `WAVE_FORMAT_EXTENSIBLE` wrapper. Nothing is compressed, so seeking is exact. A data chunk that claims more than the
//! file holds (a recording cut short, or a streamed file with a length of 0 or all ones) plays to the end of the file.
use crate::tags::Tags;
use crate::{Decode, Error, Source};

/// Frames per block.
const BLOCK: usize = 1024;
const MAX_FRAME_BYTES: usize = 8;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Sample {
    U8,
    I16,
    I24,
    I32,
    F32,
}

pub struct Decoder<S: Source> {
    source: S,
    sample: Sample,
    channels: u32,
    sample_rate: u32,
    bits: u32,
    frame_bytes: usize,
    data_start: u64,
    frames: u64,
    /// The frame the next block starts at.
    next: u64,
    raw: [u8; BLOCK * MAX_FRAME_BYTES],
    pcm: [f32; 2 * BLOCK],
    tags: Tags,
}

impl<S: Source> Decoder<S> {
    pub const fn new(source: S) -> Self {
        Decoder {
            source,
            sample: Sample::I16,
            channels: 0,
            sample_rate: 0,
            bits: 0,
            frame_bytes: 0,
            data_start: 0,
            frames: 0,
            next: 0,
            raw: [0; BLOCK * MAX_FRAME_BYTES],
            pcm: [0.0; 2 * BLOCK],
            tags: Tags::new(),
        }
    }

    pub fn source(&mut self) -> &mut S {
        &mut self.source
    }

    fn read_exact(&mut self, offset: u64, out: &mut [u8]) -> Result<(), Error> {
        if self.source.read_at(offset, out)? < out.len() {
            return Err(Error::Format("the file ends inside the WAV header"));
        }
        Ok(())
    }

    /// Walks the chunks to `fmt ` and `data`.
    pub fn open(&mut self) -> Result<(), Error> {
        self.next = 0;
        self.tags.clear();
        let mut head = [0u8; 12];
        self.read_exact(0, &mut head)?;
        if &head[..4] == b"RF64" {
            return Err(Error::Unsupported("RF64 files (over 4 GiB)"));
        }
        if &head[..4] != b"RIFF" || &head[8..12] != b"WAVE" {
            return Err(Error::Format("not a WAV file (no RIFF/WAVE header)"));
        }
        let size = self.source.size();
        let mut at = 12u64;
        let mut have_format = false;
        while at + 8 <= size {
            let mut chunk = [0u8; 8];
            self.read_exact(at, &mut chunk)?;
            let length = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]) as u64;
            let body = at + 8;
            match &chunk[..4] {
                b"fmt " => {
                    let mut fmt = [0u8; 40];
                    let keep = (length as usize).min(fmt.len());
                    if keep < 16 {
                        return Err(Error::Format("a short fmt chunk"));
                    }
                    self.read_exact(body, &mut fmt[..keep])?;
                    self.format(&fmt[..keep])?;
                    have_format = true;
                }
                b"data" => {
                    if !have_format {
                        return Err(Error::Format("the data chunk comes before fmt"));
                    }
                    self.data_start = body;
                    let available = size - body;
                    let bytes = if length == 0 || length > available { available } else { length };
                    self.frames = bytes / self.frame_bytes as u64;
                    return Ok(());
                }
                b"LIST" if length >= 4 => self.list_info(body, length),
                _ => {}
            }
            at = body + length + (length & 1);
        }
        Err(Error::Format("no data chunk"))
    }

    fn format(&mut self, fmt: &[u8]) -> Result<(), Error> {
        let mut tag = u16::from_le_bytes([fmt[0], fmt[1]]);
        self.channels = u16::from_le_bytes([fmt[2], fmt[3]]) as u32;
        self.sample_rate = u32::from_le_bytes([fmt[4], fmt[5], fmt[6], fmt[7]]);
        let bits = u16::from_le_bytes([fmt[14], fmt[15]]) as u32;
        if tag == 0xFFFE && fmt.len() >= 26 {
            tag = u16::from_le_bytes([fmt[24], fmt[25]]); // the first two bytes of the sub-format GUID
        }
        self.bits = bits;
        self.sample = match (tag, bits) {
            (1, 8) => Sample::U8,
            (1, 16) => Sample::I16,
            (1, 24) => Sample::I24,
            (1, 32) => Sample::I32,
            (3, 32) => Sample::F32,
            _ => return Err(Error::Unsupported("this WAV sample format (only PCM 8/16/24/32 bit and 32-bit float)")),
        };
        if self.channels == 0 || self.channels > 2 {
            return Err(Error::Unsupported("more than two channels"));
        }
        if self.sample_rate == 0 {
            return Err(Error::Format("a sample rate of zero"));
        }
        self.frame_bytes = self.channels as usize * (bits as usize / 8);
        Ok(())
    }

    /// `LIST`/`INFO`: `INAM` is the title, `IART` the artist, `IPRD` the album (zero-terminated text, Latin-1 or UTF-8).
    fn list_info(&mut self, body: u64, length: u64) {
        let mut kind = [0u8; 4];
        if self.read_exact(body, &mut kind).is_err() || &kind != b"INFO" {
            return;
        }
        let (mut at, end) = (body + 4, body + length);
        while at + 8 <= end {
            let mut chunk = [0u8; 8];
            if self.read_exact(at, &mut chunk).is_err() {
                return;
            }
            let size = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]) as u64;
            let mut text = [0u8; 120];
            let keep = (size as usize).min(text.len());
            let got = self.source.read_at(at + 8, &mut text[..keep]).unwrap_or(0);
            let text = &text[..got];
            let text = &text[..text.iter().position(|&b| b == 0).unwrap_or(text.len())];
            let field = match &chunk[..4] {
                b"INAM" => Some(&mut self.tags.title),
                b"IART" => Some(&mut self.tags.artist),
                b"IPRD" => Some(&mut self.tags.album),
                _ => None,
            };
            if let Some(field) = field {
                field.set_utf8(text);
            }
            at += 8 + size + (size & 1);
        }
    }

    pub fn seek(&mut self, sample: u64) -> Result<u64, Error> {
        self.next = sample.min(self.frames);
        Ok(self.next)
    }

    pub fn next_block(&mut self) -> Result<Option<usize>, Error> {
        if self.next >= self.frames {
            return Ok(None);
        }
        let count = ((self.frames - self.next) as usize).min(BLOCK);
        let bytes = count * self.frame_bytes;
        let offset = self.data_start + self.next * self.frame_bytes as u64;
        if self.source.read_at(offset, &mut self.raw[..bytes])? < bytes {
            return Err(Error::Source);
        }
        let width = self.frame_bytes / self.channels as usize;
        for (value, raw) in self.pcm.iter_mut().zip(self.raw[..bytes].chunks_exact(width)) {
            *value = match self.sample {
                Sample::U8 => (raw[0] as f32 - 128.0) / 128.0,
                Sample::I16 => i16::from_le_bytes([raw[0], raw[1]]) as f32 / 32768.0,
                Sample::I24 => (i32::from_le_bytes([0, raw[0], raw[1], raw[2]]) >> 8) as f32 / 8_388_608.0,
                Sample::I32 => i32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) as f32 / 2_147_483_648.0,
                Sample::F32 => f32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]),
            };
        }
        self.next += count as u64;
        Ok(Some(count))
    }
}

impl<S: Source> Decode for Decoder<S> {
    fn open(&mut self) -> Result<(), Error> {
        Decoder::open(self)
    }

    fn next_block(&mut self) -> Result<Option<usize>, Error> {
        Decoder::next_block(self)
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
        self.bits
    }

    fn total_samples(&self) -> u64 {
        self.frames
    }

    fn tags(&self) -> &Tags {
        &self.tags
    }

    fn position(&self) -> u64 {
        self.next
    }

    fn seek(&mut self, sample: u64) -> Result<u64, Error> {
        Decoder::seek(self, sample)
    }

    fn format_name(&self) -> &'static str {
        "WAV"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wav(tag: u16, bits: u16, channels: u16, data: &[u8], extra: &[u8]) -> Vec<u8> {
        let mut out = b"RIFF\0\0\0\0WAVE".to_vec();
        out.extend(b"fmt \x10\0\0\0");
        let frame = channels * bits / 8;
        for word in [tag, channels] {
            out.extend(word.to_le_bytes());
        }
        out.extend(48000u32.to_le_bytes());
        out.extend((48000 * frame as u32).to_le_bytes());
        out.extend(frame.to_le_bytes());
        out.extend(bits.to_le_bytes());
        out.extend(extra);
        out.extend(b"data");
        out.extend((data.len() as u32).to_le_bytes());
        out.extend(data);
        out
    }

    fn decode(file: &[u8]) -> Vec<f32> {
        let mut decoder = Box::new(Decoder::new(file));
        decoder.open().unwrap();
        let mut all = Vec::new();
        let mut out = [0f32; 2 * BLOCK];
        while let Some(n) = decoder.next_block().unwrap() {
            decoder.to_f32_stereo(0, n, &mut out);
            all.extend_from_slice(&out[..2 * n]);
        }
        all
    }

    #[test]
    fn sample_formats_scale_to_unit_range() {
        assert_eq!(decode(&wav(1, 16, 1, &[0x00, 0x80, 0xFF, 0x7F], b"")), [-1.0, -1.0, 32767.0 / 32768.0, 32767.0 / 32768.0]);
        assert_eq!(decode(&wav(1, 8, 2, &[0, 255], b"")), [-1.0, 127.0 / 128.0]);
        assert_eq!(decode(&wav(1, 24, 2, &[0, 0, 0x80, 0, 0, 0x40], b"")), [-1.0, 0.5]);
        assert_eq!(decode(&wav(1, 32, 1, &[0, 0, 0, 0x80], b"")), [-1.0, -1.0]);
        assert_eq!(decode(&wav(3, 32, 2, &[0, 0, 0x80, 0x3F, 0, 0, 0, 0xBF], b"")), [1.0, -0.5]);
    }

    #[test]
    fn a_data_chunk_longer_than_the_file_plays_to_its_end_and_seek_is_exact() {
        let mut file = wav(1, 16, 1, &[1, 0, 2, 0, 3, 0, 4, 0], b"");
        let at = file.len() - 12;
        file[at..at + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        let mut decoder = Box::new(Decoder::new(&file[..]));
        decoder.open().unwrap();
        assert_eq!(Decode::total_samples(&*decoder), 4);
        assert_eq!(Decode::seek(&mut *decoder, 2).unwrap(), 2);
        assert_eq!(decoder.next_block().unwrap(), Some(2));
        assert_eq!(decoder.next_block().unwrap(), None);
    }

    #[test]
    fn what_it_cannot_play_is_refused() {
        for file in [wav(1, 16, 6, &[0; 12], b""), wav(2, 4, 1, &[0; 4], b"")] {
            assert!(Decoder::new(&file[..]).open().is_err());
        }
    }
}
