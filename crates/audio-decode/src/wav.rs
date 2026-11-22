//! WAV (RIFF/WAVE, and RF64 for files over 4 GiB, EBU TECH 3306): integer PCM at 8, 16, 24 and 32 bits and 32-bit
//! float, plain or in the `WAVE_FORMAT_EXTENSIBLE` wrapper, up to `C` channels (two unless asked: `Decoder<S, 32>`).
//! Samples may sit left-justified in a wider slot than their bits (24 bits in 4 bytes, as `audio_rec::wav` writes what
//! a device delivers); the slot is the block align over the channels. Nothing is compressed, so seeking is exact. A data
//! chunk that claims more than the file holds (a recording cut short or still being written, or a streamed file with a
//! length of 0 or all ones) plays to the end of the file's whole frames.
use crate::tags::Tags;
use crate::{Decode, Error, Source};

/// Frames per block.
pub const BLOCK: usize = 1024;
/// The widest sample slot.
const MAX_SLOT: usize = 4;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Sample {
    /// 8-bit, unsigned (the only unsigned WAV width).
    U8,
    /// Signed, left-justified in the slot: read as the top bytes of an `i32`.
    Int,
    F32,
}

pub struct Decoder<S: Source, const C: usize = 2> {
    source: S,
    sample: Sample,
    channels: u32,
    sample_rate: u32,
    bits: u32,
    /// Bytes per sample in the file, and per frame.
    slot: usize,
    frame_bytes: usize,
    data_start: u64,
    frames: u64,
    /// The frame the next block starts at, and the one the current block started at.
    next: u64,
    at: u64,
    /// The block's bytes as read (`C` slots of `BLOCK` frames: flattened, interleaved), and its samples per channel.
    raw: [[u8; MAX_SLOT * BLOCK]; C],
    pcm: [[f32; BLOCK]; C],
    tags: Tags,
}

impl<S: Source, const C: usize> Decoder<S, C> {
    pub const fn new(source: S) -> Self {
        Decoder {
            source,
            sample: Sample::Int,
            channels: 0,
            sample_rate: 0,
            bits: 0,
            slot: 0,
            frame_bytes: 0,
            data_start: 0,
            frames: 0,
            next: 0,
            at: 0,
            raw: [[0; MAX_SLOT * BLOCK]; C],
            pcm: [[0.0; BLOCK]; C],
            tags: Tags::new(),
        }
    }

    pub fn source(&mut self) -> &mut S {
        &mut self.source
    }

    /// Sample `index` of channel `channel` of the block `next_block` returned, in [-1, 1].
    pub fn sample(&self, channel: usize, index: usize) -> f32 {
        self.pcm[channel][index]
    }

    /// The block's samples of channel `channel`.
    pub fn channel(&self, channel: usize) -> &[f32; BLOCK] {
        &self.pcm[channel]
    }

    fn read_exact(&mut self, offset: u64, out: &mut [u8]) -> Result<(), Error> {
        if read_full(&mut self.source, offset, out)? < out.len() {
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
        let rf64 = &head[..4] == b"RF64";
        if !(rf64 || &head[..4] == b"RIFF") || &head[8..12] != b"WAVE" {
            return Err(Error::Format("not a WAV file (no RIFF/WAVE header)"));
        }
        let size = self.source.size();
        let mut at = 12u64;
        let mut have_format = false;
        // RF64: the data chunk's real length is in `ds64` (riff size, data size, sample count), its own says all ones.
        let mut big_data = None;
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
                b"ds64" if rf64 && length >= 16 => {
                    let mut sizes = [0u8; 16];
                    self.read_exact(body, &mut sizes)?;
                    big_data = Some(u64::from_le_bytes(sizes[8..16].try_into().unwrap_or_default()));
                }
                b"data" => {
                    if !have_format {
                        return Err(Error::Format("the data chunk comes before fmt"));
                    }
                    self.data_start = body;
                    let length = if rf64 && length == u64::from(u32::MAX) { big_data.unwrap_or(0) } else { length };
                    let available = size.saturating_sub(body);
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
        let block_align = u16::from_le_bytes([fmt[12], fmt[13]]) as usize;
        let bits = u16::from_le_bytes([fmt[14], fmt[15]]) as u32;
        if tag == 0xFFFE && fmt.len() >= 26 {
            tag = u16::from_le_bytes([fmt[24], fmt[25]]); // the first two bytes of the sub-format GUID
        }
        if self.channels == 0 {
            return Err(Error::Format("no channels"));
        }
        if self.channels as usize > C {
            return Err(Error::Unsupported(if C == 2 { "more than two channels" } else { "more channels than this player reads" }));
        }
        if self.sample_rate == 0 {
            return Err(Error::Format("a sample rate of zero"));
        }
        // The slot: whole bytes for the bits, or wider when the block align says so (left-justified samples).
        let slot = (bits as usize).div_ceil(8).max(block_align / self.channels as usize);
        self.bits = bits;
        self.sample = match (tag, bits, slot) {
            (1, 8, 1) => Sample::U8,
            (1, 9..=32, 2..=4) if bits as usize <= slot * 8 => Sample::Int,
            (3, 32, 4) => Sample::F32,
            _ => return Err(Error::Unsupported("this WAV sample format (only PCM 8/16/24/32 bit and 32-bit float)")),
        };
        (self.slot, self.frame_bytes) = (slot, self.channels as usize * slot);
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

    /// Exact: the next block starts at frame `sample`.
    pub fn seek(&mut self, sample: u64) -> Result<u64, Error> {
        self.next = sample.min(self.frames);
        Ok(self.next)
    }

    /// The frame the block `next_block` returned last starts at.
    pub fn block_start(&self) -> u64 {
        self.at
    }

    pub fn next_block(&mut self) -> Result<Option<usize>, Error> {
        if self.next >= self.frames {
            return Ok(None);
        }
        let count = ((self.frames - self.next) as usize).min(BLOCK);
        let bytes = count * self.frame_bytes;
        let offset = self.data_start + self.next * self.frame_bytes as u64;
        // A file shorter than when it was opened (it cannot be, for a file that only grows) is a read error.
        if read_full(&mut self.source, offset, &mut self.raw.as_flattened_mut()[..bytes])? < bytes {
            return Err(Error::Source);
        }
        let (channels, slot) = (self.channels as usize, self.slot);
        for (index, frame) in self.raw.as_flattened()[..bytes].chunks_exact(self.frame_bytes).enumerate() {
            for (channel, bytes) in frame.chunks_exact(slot).take(channels).enumerate() {
                self.pcm[channel][index] = match self.sample {
                    Sample::U8 => (bytes[0] as f32 - 128.0) / 128.0,
                    Sample::Int => {
                        let mut word = [0u8; 4];
                        word[4 - slot..].copy_from_slice(bytes);
                        i32::from_le_bytes(word) as f32 / 2_147_483_648.0
                    }
                    Sample::F32 => f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
                };
            }
        }
        (self.at, self.next) = (self.next, self.next + count as u64);
        Ok(Some(count))
    }
}

/// Fills `out` from `offset`, in as many reads as the source needs (a source may give less than asked); how much it got
/// before the end of the file.
fn read_full(source: &mut impl Source, offset: u64, out: &mut [u8]) -> Result<usize, Error> {
    let mut got = 0;
    while got < out.len() {
        match source.read_at(offset + got as u64, &mut out[got..])? {
            0 => break,
            count => got += count,
        }
    }
    Ok(got)
}

impl<S: Source, const C: usize> Decode for Decoder<S, C> {
    fn open(&mut self) -> Result<(), Error> {
        Decoder::open(self)
    }

    fn next_block(&mut self) -> Result<Option<usize>, Error> {
        Decoder::next_block(self)
    }

    fn to_f32_stereo(&self, start: usize, count: usize, out: &mut [f32]) {
        for i in 0..count {
            let at = start + i;
            out[2 * i] = self.pcm[0][at];
            out[2 * i + 1] = self.pcm[if self.channels >= 2 { 1 } else { 0 }][at];
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
        let mut decoder = Box::new(Decoder::<_, 2>::new(file));
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
        let mut decoder = Box::new(Decoder::<_, 2>::new(&file[..]));
        decoder.open().unwrap();
        assert_eq!(Decode::total_samples(&*decoder), 4);
        assert_eq!(Decode::seek(&mut *decoder, 2).unwrap(), 2);
        assert_eq!(decoder.next_block().unwrap(), Some(2));
        assert_eq!(decoder.next_block().unwrap(), None);
    }

    /// A source that gives at most 100 bytes a read, as a disk read capped per call does.
    struct Trickle<'a>(&'a [u8]);

    impl Source for Trickle<'_> {
        fn size(&self) -> u64 {
            self.0.len() as u64
        }

        fn read_at(&mut self, offset: u64, out: &mut [u8]) -> Result<usize, crate::SourceError> {
            let length = out.len().min(100);
            self.0.read_at(offset, &mut out[..length])
        }
    }

    #[test]
    fn recorder_files_many_channels_wide_slots_and_rf64_are_read() {
        use audio_rec::wav::{Format, header_len, write_header};
        // 18 channels of 24 bits in 4-byte slots (extensible), as a multichannel device's take is written.
        let format = Format { channels: 18, rate: 96_000, slot: 4, bits: 24, float: false };
        let mut file = vec![0u8; header_len(&format)];
        let frames = 1500usize;
        write_header(&mut file, &format, (frames * 72) as u64);
        for frame in 0..frames {
            for channel in 0..18 {
                // Left-justified: channel c of frame f is (c + 1) / 32 of full scale, negative on odd frames.
                let value = ((channel as i64 + 1) << 26) * if frame % 2 == 1 { -1 } else { 1 };
                file.extend((value as i32).to_le_bytes());
            }
        }
        let mut decoder = Box::new(Decoder::<_, 32>::new(Trickle(&file)));
        decoder.open().unwrap();
        assert_eq!((Decode::channels(&*decoder), Decode::sample_rate(&*decoder), Decode::total_samples(&*decoder)), (18, 96_000, frames as u64));
        assert_eq!(decoder.next_block().unwrap(), Some(BLOCK), "short reads are joined");
        assert_eq!((decoder.sample(0, 0), decoder.sample(17, 1)), (1.0 / 32.0, -18.0 / 32.0));
        decoder.seek(1499).unwrap();
        assert_eq!((decoder.next_block().unwrap(), decoder.block_start()), (Some(1), 1499));
        assert_eq!(decoder.sample(3, 0), -4.0 / 32.0);
        // A take still being written: its header says more than is there; whole frames only.
        let short = &file[..header_len(&format) + 100 * 72 + 30];
        let mut decoder = Box::new(Decoder::<_, 32>::new(short));
        decoder.open().unwrap();
        assert_eq!(Decode::total_samples(&*decoder), 100);
        // Too many channels for the decoder asked for.
        assert!(Decoder::<_, 8>::new(&file[..]).open().is_err());

        // Past 4 GiB the header is RF64 and the length is in ds64 (the file here holds only the start of the data).
        let format = Format { channels: 2, rate: 48_000, slot: 4, bits: 32, float: true };
        let mut big = vec![0u8; header_len(&format)];
        write_header(&mut big, &format, 5u64 << 30);
        assert_eq!(&big[..4], b"RF64");
        big.extend([0.5f32.to_le_bytes(), (-0.25f32).to_le_bytes()].concat());
        let mut decoder = Box::new(Decoder::<_, 2>::new(&big[..]));
        decoder.open().unwrap();
        assert_eq!(Decode::total_samples(&*decoder), 1, "as much as the file holds");
        decoder.next_block().unwrap();
        assert_eq!((decoder.sample(0, 0), decoder.sample(1, 0)), (0.5, -0.25));
        // Plain 16-bit, as before.
        let format = Format { channels: 2, rate: 48_000, slot: 2, bits: 16, float: false };
        let mut plain = vec![0u8; header_len(&format)];
        write_header(&mut plain, &format, 4);
        plain.extend([(-16384i16).to_le_bytes(), 16384i16.to_le_bytes()].concat());
        let mut decoder = Box::new(Decoder::<_, 2>::new(&plain[..]));
        decoder.open().unwrap();
        decoder.next_block().unwrap();
        assert_eq!((decoder.sample(0, 0), decoder.sample(1, 0)), (-0.5, 0.5));
    }

    #[test]
    fn what_it_cannot_play_is_refused() {
        for file in [wav(1, 16, 6, &[0; 12], b""), wav(2, 4, 1, &[0; 4], b"")] {
            assert!(Decoder::<_, 2>::new(&file[..]).open().is_err());
        }
    }
}
