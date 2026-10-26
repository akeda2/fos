//! FLAC decoder (RFC 9639): mono or stereo, 8 to 24 bits, any sample rate, block sizes up to `MAX_BLOCK`.
//!
//! The stream is `fLaC`, metadata blocks (STREAMINFO is the one we need; the rest is skipped), then frames. A frame is a
//! header (block size, rate, channel layout), one subframe per channel (constant, verbatim, fixed polynomial predictor or
//! LPC, each followed by a Rice-coded residual) and a CRC-16. Channels may be stored as left/side, side/right or mid/side
//! and are turned back into left/right here.
//!
//! Frames are decoded one at a time from a small refill buffer, so memory use does not depend on the file or the
//! frame size. The frame CRCs are not checked: a transport that corrupts data (TCP does not) would show as noise, and a
//! frame header that does not parse is an error. Seeking is not implemented yet except `rewind`.
//!
//! The decoder is large (two channels of `MAX_BLOCK` samples): keep it in a `static`, build it with `Decoder::new` and
//! call `open`.

use crate::{Error, Source};

/// The longest block, in samples per channel. Streams from encoders that stay inside the FLAC "subset" use at most 4608
/// (up to 48 kHz) or 16384.
pub const MAX_BLOCK: usize = 16384;
/// Bytes fetched from the source at a time.
const INPUT: usize = 32768;

pub struct Decoder<S: Source> {
    source: S,
    buffer: [u8; INPUT],
    position: usize,
    end: usize,
    next_offset: u64,
    accumulator: u64,
    bits: u32,
    data_start: u64,
    pub sample_rate: u32,
    pub channels: u32,
    pub bits_per_sample: u32,
    /// 0 when the encoder did not know (a stream).
    pub total_samples: u64,
    /// The decoded block: `samples[channel][..n]`, left/right already restored. Valid after `next_block` returned `n`.
    pub samples: [[i32; MAX_BLOCK]; 2],
}

impl<S: Source> Decoder<S> {
    pub const fn new(source: S) -> Self {
        Decoder {
            source,
            buffer: [0; INPUT],
            position: 0,
            end: 0,
            next_offset: 0,
            accumulator: 0,
            bits: 0,
            data_start: 0,
            sample_rate: 0,
            channels: 0,
            bits_per_sample: 0,
            total_samples: 0,
            samples: [[0; MAX_BLOCK]; 2],
        }
    }

    pub fn source(&mut self) -> &mut S {
        &mut self.source
    }

    /// Reads the signature and the metadata blocks; leaves the decoder at the first frame.
    pub fn open(&mut self) -> Result<(), Error> {
        self.position = 0;
        self.end = 0;
        self.next_offset = 0;
        self.bits = 0;
        let mut signature = [0u8; 4];
        for byte in &mut signature {
            *byte = self.byte()?;
        }
        if &signature != b"fLaC" {
            return Err(Error::Format("not a FLAC file (no fLaC signature)"));
        }
        let mut have_streaminfo = false;
        loop {
            let last = self.read_bits(1)? == 1;
            let kind = self.read_bits(7)?;
            let length = self.read_bits(24)? as u64;
            if kind == 0 {
                if length < 34 {
                    return Err(Error::Format("a short STREAMINFO block"));
                }
                self.read_bits(16)?; // minimum block size
                self.read_bits(16)?; // maximum block size
                self.read_bits(24)?; // minimum frame size
                self.read_bits(24)?; // maximum frame size
                self.sample_rate = self.read_bits(20)?;
                self.channels = self.read_bits(3)? + 1;
                self.bits_per_sample = self.read_bits(5)? + 1;
                self.total_samples = ((self.read_bits(4)? as u64) << 32) | self.read_bits(32)? as u64;
                self.skip(16 + (length - 34))?; // the MD5 signature and anything after
                have_streaminfo = true;
            } else {
                self.skip(length)?;
            }
            if last {
                break;
            }
        }
        if !have_streaminfo {
            return Err(Error::Format("no STREAMINFO block"));
        }
        if self.channels > 2 {
            return Err(Error::Unsupported("more than two channels"));
        }
        if self.bits_per_sample > 24 || self.bits_per_sample < 4 {
            return Err(Error::Unsupported("sample size above 24 bits"));
        }
        if self.sample_rate == 0 {
            return Err(Error::Format("a sample rate of zero"));
        }
        self.data_start = self.next_offset - (self.end - self.position) as u64;
        Ok(())
    }

    /// Back to the first frame.
    pub fn rewind(&mut self) {
        self.position = 0;
        self.end = 0;
        self.bits = 0;
        self.next_offset = self.data_start;
    }

    /// Decodes the next frame into `samples`; returns its length in samples per channel, `None` at the end of the file.
    pub fn next_block(&mut self) -> Result<Option<usize>, Error> {
        if self.bits == 0 && self.position == self.end && !self.refill()? {
            return Ok(None);
        }
        if self.read_bits(14)? != 0x3FFE {
            return Err(Error::Format("lost frame sync"));
        }
        self.read_bits(1)?; // reserved
        self.read_bits(1)?; // fixed or variable block size: only the number that follows differs
        let block_code = self.read_bits(4)?;
        let rate_code = self.read_bits(4)?;
        let assignment = self.read_bits(4)?;
        let size_code = self.read_bits(3)?;
        self.read_bits(1)?; // reserved
        self.read_utf8()?; // frame or sample number
        let block = match block_code {
            1 => 192,
            2..=5 => 576 << (block_code - 2),
            6 => self.read_bits(8)? as usize + 1,
            7 => self.read_bits(16)? as usize + 1,
            8..=15 => 256 << (block_code - 8),
            _ => return Err(Error::Format("a reserved block size code")),
        };
        match rate_code {
            12 => {
                self.read_bits(8)?;
            }
            13 | 14 => {
                self.read_bits(16)?;
            }
            _ => {}
        }
        self.read_bits(8)?; // header CRC-8
        let bits = match size_code {
            0 => self.bits_per_sample,
            1 => 8,
            2 => 12,
            4 => 16,
            5 => 20,
            6 => 24,
            _ => return Err(Error::Unsupported("this sample size")),
        };
        let channels = if assignment < 8 { assignment as usize + 1 } else { 2 };
        if channels != self.channels as usize {
            return Err(Error::Unsupported("the channel count changes in the stream"));
        }
        if block > MAX_BLOCK {
            return Err(Error::Unsupported("a block longer than 16384 samples"));
        }
        for channel in 0..channels {
            // The side channel of a stereo pair has one bit more.
            let extra = match (assignment, channel) {
                (8, 1) | (9, 0) | (10, 1) => 1,
                _ => 0,
            };
            self.subframe(channel, block, bits + extra)?;
        }
        self.align();
        self.read_bits(16)?; // frame CRC-16
        let [left, right] = &mut self.samples;
        match assignment {
            8 => {
                for i in 0..block {
                    right[i] = left[i].wrapping_sub(right[i]);
                }
            }
            9 => {
                for i in 0..block {
                    left[i] = left[i].wrapping_add(right[i]);
                }
            }
            10 => {
                for i in 0..block {
                    let side = right[i] as i64;
                    let mid = ((left[i] as i64) << 1) | (side & 1);
                    left[i] = ((mid + side) >> 1) as i32;
                    right[i] = ((mid - side) >> 1) as i32;
                }
            }
            _ => {}
        }
        Ok(Some(block))
    }

    /// Writes `count` samples starting at `start` of the current block to `out` as interleaved stereo `f32` in [-1, 1)
    /// (a mono file is duplicated). `out` must hold `2 * count` values.
    pub fn to_f32_stereo(&self, start: usize, count: usize, out: &mut [f32]) {
        let scale = 1.0 / (1u32 << (self.bits_per_sample - 1)) as f32;
        let left = &self.samples[0][start..start + count];
        let right = if self.channels == 2 { &self.samples[1][start..start + count] } else { left };
        for i in 0..count {
            out[2 * i] = left[i] as f32 * scale;
            out[2 * i + 1] = right[i] as f32 * scale;
        }
    }

    fn subframe(&mut self, channel: usize, block: usize, bits: u32) -> Result<(), Error> {
        if self.read_bits(1)? != 0 {
            return Err(Error::Format("a subframe header with its padding bit set"));
        }
        let kind = self.read_bits(6)?;
        let wasted = if self.read_bits(1)? == 1 { self.read_unary()? + 1 } else { 0 };
        if wasted >= bits {
            return Err(Error::Format("more wasted bits than the sample has"));
        }
        let bits = bits - wasted;
        match kind {
            0 => {
                let value = self.read_signed(bits)?;
                self.samples[channel][..block].fill(value);
            }
            1 => {
                for i in 0..block {
                    self.samples[channel][i] = self.read_signed(bits)?;
                }
            }
            8..=12 => {
                let order = (kind - 8) as usize;
                self.warmup(channel, order, block, bits)?;
                self.residual(channel, order, block)?;
                let x = &mut self.samples[channel];
                for i in order..block {
                    let p = |k: usize| x[i - k] as i64;
                    let prediction = match order {
                        0 => 0,
                        1 => p(1),
                        2 => 2 * p(1) - p(2),
                        3 => 3 * p(1) - 3 * p(2) + p(3),
                        _ => 4 * p(1) - 6 * p(2) + 4 * p(3) - p(4),
                    };
                    x[i] = (x[i] as i64 + prediction) as i32;
                }
            }
            32..=63 => {
                let order = (kind - 31) as usize;
                self.warmup(channel, order, block, bits)?;
                let precision = self.read_bits(4)? + 1;
                if precision == 16 {
                    return Err(Error::Format("an invalid LPC precision"));
                }
                let shift = self.read_signed(5)?;
                if shift < 0 {
                    return Err(Error::Format("a negative LPC shift"));
                }
                let mut coefficients = [0i32; 32];
                for coefficient in coefficients.iter_mut().take(order) {
                    *coefficient = self.read_signed(precision)?;
                }
                self.residual(channel, order, block)?;
                let x = &mut self.samples[channel];
                for i in order..block {
                    let mut sum = 0i64;
                    for (k, coefficient) in coefficients[..order].iter().enumerate() {
                        sum += *coefficient as i64 * x[i - 1 - k] as i64;
                    }
                    x[i] = (x[i] as i64 + (sum >> shift)) as i32;
                }
            }
            _ => return Err(Error::Format("a reserved subframe type")),
        }
        if wasted > 0 {
            for sample in &mut self.samples[channel][..block] {
                *sample <<= wasted;
            }
        }
        Ok(())
    }

    fn warmup(&mut self, channel: usize, order: usize, block: usize, bits: u32) -> Result<(), Error> {
        if order > block {
            return Err(Error::Format("a predictor order above the block size"));
        }
        for i in 0..order {
            self.samples[channel][i] = self.read_signed(bits)?;
        }
        Ok(())
    }

    /// Reads the Rice-coded residual into `samples[channel][order..block]`.
    fn residual(&mut self, channel: usize, order: usize, block: usize) -> Result<(), Error> {
        let method = self.read_bits(2)?;
        if method > 1 {
            return Err(Error::Format("a reserved residual coding method"));
        }
        let parameter_bits = 4 + method;
        let escape = (1 << parameter_bits) - 1;
        let partition_order = self.read_bits(4)?;
        let partitions = 1usize << partition_order;
        if block % partitions != 0 || (block >> partition_order) < order {
            return Err(Error::Format("a bad residual partition order"));
        }
        let mut at = order;
        for partition in 0..partitions {
            let count = (block >> partition_order) - if partition == 0 { order } else { 0 };
            let parameter = self.read_bits(parameter_bits)?;
            if parameter == escape {
                let width = self.read_bits(5)?;
                for _ in 0..count {
                    self.samples[channel][at] = if width == 0 { 0 } else { self.read_signed(width)? };
                    at += 1;
                }
            } else {
                for _ in 0..count {
                    let quotient = self.read_unary()?;
                    let remainder = if parameter == 0 { 0 } else { self.read_bits(parameter)? };
                    let folded = (quotient << parameter) | remainder;
                    self.samples[channel][at] = ((folded >> 1) as i32) ^ -((folded & 1) as i32);
                    at += 1;
                }
            }
        }
        Ok(())
    }

    /// Fetches the next piece of the file into the buffer; false at its end.
    fn refill(&mut self) -> Result<bool, Error> {
        let count = self.source.read_at(self.next_offset, &mut self.buffer)?;
        self.position = 0;
        self.end = count;
        self.next_offset += count as u64;
        Ok(count > 0)
    }

    fn byte(&mut self) -> Result<u8, Error> {
        if self.position == self.end && !self.refill()? {
            return Err(Error::Format("the file ends inside a frame"));
        }
        let byte = self.buffer[self.position];
        self.position += 1;
        Ok(byte)
    }

    /// Skips whole bytes (the bit position must be on a byte boundary, which it is between metadata fields).
    fn skip(&mut self, mut count: u64) -> Result<(), Error> {
        while count > 0 {
            if self.position == self.end && !self.refill()? {
                return Err(Error::Format("the file ends inside the metadata"));
            }
            let step = count.min((self.end - self.position) as u64);
            self.position += step as usize;
            count -= step;
        }
        Ok(())
    }

    /// Up to 32 bits, most significant first.
    fn read_bits(&mut self, count: u32) -> Result<u32, Error> {
        while self.bits < count {
            self.accumulator = (self.accumulator << 8) | self.byte()? as u64;
            self.bits += 8;
        }
        self.bits -= count;
        Ok(((self.accumulator >> self.bits) & ((1u64 << count) - 1)) as u32)
    }

    /// A two's-complement number of `count` bits (1 to 32).
    fn read_signed(&mut self, count: u32) -> Result<i32, Error> {
        let value = self.read_bits(count)?;
        let shift = 32 - count;
        Ok(((value << shift) as i32) >> shift)
    }

    /// Counts zero bits up to the next one bit (which is consumed).
    fn read_unary(&mut self) -> Result<u32, Error> {
        let mut zeros = 0u32;
        loop {
            if self.bits == 0 {
                self.accumulator = self.byte()? as u64;
                self.bits = 8;
            }
            let live = self.accumulator & ((1u64 << self.bits) - 1);
            if live == 0 {
                zeros += self.bits;
                self.bits = 0;
                if zeros > (1 << 24) {
                    return Err(Error::Format("a runaway unary code"));
                }
            } else {
                let top = 63 - live.leading_zeros();
                zeros += self.bits - 1 - top;
                self.bits = top;
                return Ok(zeros);
            }
        }
    }

    /// The frame header's UTF-8-style number (up to 36 bits).
    fn read_utf8(&mut self) -> Result<u64, Error> {
        let first = self.read_bits(8)?;
        let extra = match first {
            0x00..=0x7F => return Ok(first as u64),
            0xC0..=0xDF => 1,
            0xE0..=0xEF => 2,
            0xF0..=0xF7 => 3,
            0xF8..=0xFB => 4,
            0xFC..=0xFD => 5,
            0xFE => 6,
            _ => return Err(Error::Format("a bad frame number")),
        };
        let mut value = (first & (0x3F >> extra)) as u64;
        for _ in 0..extra {
            value = (value << 6) | (self.read_bits(8)? & 0x3F) as u64;
        }
        Ok(value)
    }

    fn align(&mut self) {
        self.bits -= self.bits % 8;
    }
}
