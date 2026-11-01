//! Monkey's Audio (`.ape`) decoder: mono or stereo, 8, 16 or 24 bits, files written by version 3.99 and later
//! (format versions 3990 to 3999; every encoder since 2004 writes them), all five compression levels.
//!
//! A file is a descriptor (`MAC `, version, the sizes of what follows), a 24-byte header (compression level, blocks per
//! frame, frame count, rate, bits, channels), a seek table (the byte offset of every frame), an optional stored WAV header and the
//! frames. Frames are independent: each one starts with a CRC and flags, then a range-coded stream, and the filters and
//! predictors start from zero in every frame, which is what makes seeking cheap (start at a frame, decode and drop
//! blocks up to the target).
//!
//! The coding of a frame, in the order the decoder undoes it:
//! 1. **Entropy**: a range coder reads, per sample and channel, an "overflow" symbol from a fixed 64-symbol model and the
//!    low part as a uniform number below an adaptive pivot (`ksum / 32`); the pair is folded into a signed value.
//! 2. **Neural-net filters** (levels above "fast"): up to three sign-sign LMS filters of 16 to 1280 taps on the
//!    previous outputs, applied one after the other.
//! 3. **Predictor**: a first-order filter, four adaptive taps on the channel's own last output's differences and five on
//!    the other channel's (stereo), all sign-sign LMS again.
//! 4. **Stereo**: the stored channels are mid and side-ish (`X`, `Y`); left is `Y - X/2`, right is `left + X`.
//!
//! The stream is a sequence of 32-bit words each stored little-endian but read most significant byte first, so the
//! bytes are read in reverse within every aligned group of four; a frame can start in the middle of a word (the seek
//! table counts bytes), which is why `start_frame` aligns down and skips.
//!
//! The frame CRC (of the decoded PCM, as little-endian bytes) is checked at the end of every frame: a mismatch is
//! counted in `crc_errors`, not an error, so a damaged frame plays as noise instead of stopping the track.
//!
//! Not supported: files before 3.99 (a different range coder and, before 3.95, different predictors), more than two
//! channels. The 24-bit path (a second predictor pass that works out whether the encoder used 32- or 64-bit
//! arithmetic) is written from the format but has not been run against a real 24-bit file.
//!
//! Like FLAC the decoder is large (two channels of `MAX_BLOCK` samples and the filter memory): keep it in a `static`,
//! build it with `Decoder::new` and call `open`.

use crate::tags::Tags;
use crate::{Decode, Error, Source};

/// Samples per channel decoded per `next_block`.
pub const MAX_BLOCK: usize = 4096;
/// Bytes fetched from the source at a time (a multiple of 4: the stream is read in words).
const INPUT: usize = 32768;

/// The range coder renormalises when the range falls to this or below.
const BOTTOM: u32 = 1 << 23;
const MODEL_ELEMENTS: u32 = 64;

/// Start of each symbol's interval in a 16-bit range, and its width.
const COUNTS: [u16; 22] = [
    0, 19578, 36160, 48417, 56323, 60899, 63265, 64435, 64971, 65232, 65351, 65416, 65447, 65466, 65476, 65482, 65485, 65488, 65490, 65491, 65492,
    65493,
];
const COUNTS_DIFF: [u16; 21] = [19578, 16582, 12257, 7906, 4576, 2366, 1170, 536, 261, 119, 65, 31, 19, 10, 6, 3, 3, 2, 1, 1, 1];

/// Filter taps and fixed-point fraction bits per filter slot, by compression level (1000 to 5000, so index 0..=4).
const FILTER_ORDERS: [[usize; 3]; 5] = [[0, 0, 0], [16, 0, 0], [64, 0, 0], [32, 256, 0], [16, 256, 1280]];
const FILTER_SHIFTS: [[u32; 3]; 5] = [[0, 0, 0], [11, 0, 0], [11, 0, 0], [10, 13, 0], [11, 13, 15]];

/// Samples of history a filter keeps beyond its taps before it moves the window back to the start of its buffer.
const FILTER_HISTORY: usize = 512;
const PREDICTOR_HISTORY: usize = 512;
/// The predictor reads up to this many entries behind its position.
const PREDICTOR_SIZE: usize = 50;
const PREDICTOR_ORDER: usize = 8;
const Y_DELAY_A: usize = 18 + PREDICTOR_ORDER * 4;
const Y_DELAY_B: usize = 18 + PREDICTOR_ORDER * 3;
const X_DELAY_A: usize = 18 + PREDICTOR_ORDER * 2;
const X_DELAY_B: usize = 18 + PREDICTOR_ORDER;
const Y_ADAPT_A: usize = 18;
const X_ADAPT_A: usize = 14;
const Y_ADAPT_B: usize = 10;
const X_ADAPT_B: usize = 5;

/// Frame flag bits.
const FLAG_MONO_SILENCE: u32 = 1;
const FLAG_STEREO_SILENCE: u32 = 3;
const FLAG_PSEUDO_STEREO: u32 = 4;

const CRC_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut bit = 0;
        while bit < 8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            bit += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
};

fn crc_update(crc: u32, byte: u8) -> u32 {
    CRC_TABLE[((crc ^ byte as u32) & 0xFF) as usize] ^ (crc >> 8)
}

/// -1 for a positive value, 1 for a negative one, 0 for zero: the sign the adaptive filters subtract.
fn sign(value: i32) -> i32 {
    (value < 0) as i32 - (value > 0) as i32
}

#[derive(Clone, Copy)]
struct Rice {
    k: u32,
    ksum: u32,
}

impl Rice {
    const fn new() -> Self {
        Rice { k: 10, ksum: (1 << 10) * 16 }
    }

    /// Adapts to the value just decoded: `k` follows the log of the running mean.
    fn update(&mut self, x: u32) {
        let limit = if self.k != 0 { 1u32 << (self.k + 4) } else { 0 };
        self.ksum = self.ksum.wrapping_add((x.wrapping_add(1) / 2).wrapping_sub(self.ksum.wrapping_add(16) >> 5));
        if self.ksum < limit {
            self.k -= 1;
        } else if self.ksum >= (1u32 << (self.k + 5)) && self.k < 24 {
            self.k += 1;
        }
    }
}

/// One adaptive filter of `O` taps at most (`H` = `O` + history): `delay` holds the last outputs clipped to 16 bits,
/// `adapt` the matching adaptation steps, both addressed as a window of `order` entries ending at `pos`.
struct Nn<const O: usize, const H: usize> {
    coeffs: [i16; O],
    delay: [i16; H],
    adapt: [i16; H],
    pos: usize,
    avg: u32,
}

impl<const O: usize, const H: usize> Nn<O, H> {
    const fn new() -> Self {
        Nn { coeffs: [0; O], delay: [0; H], adapt: [0; H], pos: 0, avg: 0 }
    }

    fn reset(&mut self, order: usize) {
        self.coeffs[..order].fill(0);
        self.delay[..order].fill(0);
        self.adapt[..order].fill(0);
        self.pos = order;
        self.avg = 0;
    }

    /// Undoes the filter in place: each value is the residual plus the prediction from the last `order` outputs.
    fn apply(&mut self, order: usize, shift: u32, data: &mut [i32]) {
        for value in data.iter_mut() {
            if self.pos == H {
                self.delay.copy_within(H - order..H, 0);
                self.adapt.copy_within(H - order..H, 0);
                self.pos = order;
            }
            let p = self.pos;
            let step = sign(*value) as i16;
            let coeffs = &mut self.coeffs[..order];
            let delay = &self.delay[p - order..p];
            let adapt = &self.adapt[p - order..p];
            let mut dot = 0i32;
            for i in 0..order {
                dot = dot.wrapping_add(coeffs[i] as i32 * delay[i] as i32);
                coeffs[i] = coeffs[i].wrapping_add(step.wrapping_mul(adapt[i]));
            }
            let mut result = ((dot as i64 + (1i64 << (shift - 1))) >> shift) as i32;
            result = result.wrapping_add(*value);
            *value = result;
            self.delay[p] = result.clamp(-32768, 32767) as i16;

            let magnitude = result.unsigned_abs();
            self.adapt[p] = if magnitude != 0 {
                let big = (magnitude as u64 > self.avg as u64 * 3) as u32 + (magnitude > self.avg.wrapping_add(self.avg / 3)) as u32;
                (sign(result) * (8 << big)) as i16
            } else {
                0
            };
            self.avg = self.avg.wrapping_add((magnitude.wrapping_sub(self.avg) as i32 / 16) as u32);
            self.adapt[p - 1] >>= 1;
            self.adapt[p - 2] >>= 1;
            self.adapt[p - 8] >>= 1;
            self.pos += 1;
        }
    }
}

/// The last stage: predicts each value from the previous ones of its own channel (`A`) and of the other (`B`).
/// 64-bit throughout so the 24-bit "wide" mode can use it; the narrow mode wraps its sums at 32 bits like the
/// encoder's integers do.
#[derive(Clone)]
struct Predictor {
    buf: [i64; PREDICTOR_HISTORY + PREDICTOR_SIZE],
    pos: usize,
    last_a: [i64; 2],
    filter_a: [i64; 2],
    filter_b: [i64; 2],
    coeffs_a: [[i64; 4]; 2],
    coeffs_b: [[i64; 5]; 2],
}

impl Predictor {
    const fn new() -> Self {
        Predictor {
            buf: [0; PREDICTOR_HISTORY + PREDICTOR_SIZE],
            pos: 0,
            last_a: [0; 2],
            filter_a: [0; 2],
            filter_b: [0; 2],
            coeffs_a: [[360, 317, -109, 98]; 2],
            coeffs_b: [[0; 5]; 2],
        }
    }

    fn reset(&mut self) {
        self.buf[..PREDICTOR_SIZE].fill(0);
        self.pos = 0;
        self.last_a = [0; 2];
        self.filter_a = [0; 2];
        self.filter_b = [0; 2];
        self.coeffs_a = [[360, 317, -109, 98]; 2];
        self.coeffs_b = [[0; 5]; 2];
    }

    fn advance(&mut self) {
        self.pos += 1;
        if self.pos == PREDICTOR_HISTORY {
            self.buf.copy_within(PREDICTOR_HISTORY..PREDICTOR_HISTORY + PREDICTOR_SIZE, 0);
            self.pos = 0;
        }
    }

    /// One value of channel `f` (0 = Y, 1 = X); the other channel's latest output feeds the B part.
    #[allow(clippy::too_many_arguments)]
    fn update(&mut self, decoded: i32, f: usize, delay_a: usize, delay_b: usize, adapt_a: usize, adapt_b: usize, wide: bool) -> i32 {
        let b = self.pos;
        let buf = &mut self.buf;
        buf[b + delay_a] = self.last_a[f];
        buf[b + adapt_a] = sign(buf[b + delay_a] as i32) as i64;
        buf[b + delay_a - 1] = buf[b + delay_a].wrapping_sub(buf[b + delay_a - 1]);
        buf[b + adapt_a - 1] = sign(buf[b + delay_a - 1] as i32) as i64;
        let ca = &self.coeffs_a[f];
        let mut prediction_a = 0i64;
        for k in 0..4 {
            prediction_a = prediction_a.wrapping_add(buf[b + delay_a - k].wrapping_mul(ca[k]));
        }

        // The other channel's last output, through a scaled first-order filter.
        buf[b + delay_b] = self.filter_a[f ^ 1].wrapping_sub(self.filter_b[f].wrapping_mul(31) >> 5);
        buf[b + adapt_b] = sign(buf[b + delay_b] as i32) as i64;
        buf[b + delay_b - 1] = buf[b + delay_b].wrapping_sub(buf[b + delay_b - 1]);
        buf[b + adapt_b - 1] = sign(buf[b + delay_b - 1] as i32) as i64;
        self.filter_b[f] = self.filter_a[f ^ 1];
        let cb = &self.coeffs_b[f];
        let mut prediction_b = 0i64;
        for k in 0..5 {
            prediction_b = prediction_b.wrapping_add(buf[b + delay_b - k].wrapping_mul(cb[k]));
        }

        self.last_a[f] = if wide {
            decoded as i64 + (prediction_a.wrapping_add(prediction_b >> 1) >> 10)
        } else {
            let (a, b) = (prediction_a as i32 as i64, prediction_b as i32 as i64);
            (decoded as u32).wrapping_add(((a + (b >> 1)) as i32 >> 10) as u32) as i32 as i64
        };
        self.filter_a[f] = self.last_a[f].wrapping_add(self.filter_a[f].wrapping_mul(31) >> 5);

        let step = sign(decoded) as i64;
        for k in 0..4 {
            self.coeffs_a[f][k] = self.coeffs_a[f][k].wrapping_add(buf[b + adapt_a - k].wrapping_mul(step));
        }
        for k in 0..5 {
            self.coeffs_b[f][k] = self.coeffs_b[f][k].wrapping_add(buf[b + adapt_b - k].wrapping_mul(step));
        }
        self.filter_a[f] as i32
    }

    /// A mono frame: only the A part.
    fn mono(&mut self, data: &mut [i32]) {
        let mut current = self.last_a[0] as i32;
        for value in data.iter_mut() {
            let a = *value;
            let b = self.pos;
            let buf = &mut self.buf;
            buf[b + Y_DELAY_A] = current as i64;
            buf[b + Y_DELAY_A - 1] = buf[b + Y_DELAY_A].wrapping_sub(buf[b + Y_DELAY_A - 1]);
            let mut prediction = 0i64;
            for k in 0..4 {
                prediction = prediction.wrapping_add(buf[b + Y_DELAY_A - k].wrapping_mul(self.coeffs_a[0][k]));
            }
            current = a.wrapping_add((prediction as i32) >> 10);
            buf[b + Y_ADAPT_A] = sign(buf[b + Y_DELAY_A] as i32) as i64;
            buf[b + Y_ADAPT_A - 1] = sign(buf[b + Y_DELAY_A - 1] as i32) as i64;
            let step = sign(a) as i64;
            for k in 0..4 {
                self.coeffs_a[0][k] = self.coeffs_a[0][k].wrapping_add(buf[b + Y_ADAPT_A - k].wrapping_mul(step));
            }
            self.advance();
            self.filter_a[0] = (current as i64).wrapping_add(self.filter_a[0].wrapping_mul(31) >> 5);
            *value = self.filter_a[0] as i32;
        }
        self.last_a[0] = current as i64;
    }
}

/// Runs the stereo predictor over a block. With `check` it stops, returning true, at the first sample whose left or
/// right output leaves 24 bits (a sign that the 32-bit arithmetic overflowed).
fn stereo_pass(p: &mut Predictor, d0: &mut [i32], d1: &mut [i32], wide: bool, check: bool) -> bool {
    for i in 0..d0.len() {
        let a0 = p.update(d0[i], 0, Y_DELAY_A, Y_DELAY_B, Y_ADAPT_A, Y_ADAPT_B, wide);
        let a1 = p.update(d1[i], 1, X_DELAY_A, X_DELAY_B, X_ADAPT_A, X_ADAPT_B, wide);
        d0[i] = a0;
        d1[i] = a1;
        if check {
            let left = a1.wrapping_sub(a0 / 2);
            let right = left.wrapping_add(a0);
            if left.unsigned_abs().max(right.unsigned_abs()) > 1 << 23 {
                return true;
            }
        }
        p.advance();
    }
    false
}

pub struct Decoder<S: Source> {
    source: S,
    /// Bytes before the `MAC ` signature (an ID3v2 tag).
    junk: u64,
    /// Where the seek table is, and where frame 0 is.
    table_offset: u64,
    first_frame: u64,
    table_entries: u32,
    /// Index into `FILTER_ORDERS`.
    filter_set: usize,
    blocks_per_frame: u32,
    final_blocks: u32,
    total_frames: u32,
    pub sample_rate: u32,
    pub channels: u32,
    pub bits_per_sample: u32,
    pub total_samples: u64,
    /// 1000 (fast) to 5000 (insane).
    pub compression_level: u16,
    /// Format version, 3990 and up.
    pub version: u16,
    pub tags: Tags,
    /// Frames whose decoded samples did not match the CRC stored in the file.
    pub crc_errors: u32,

    buffer: [u8; INPUT],
    buf_pos: usize,
    buf_end: usize,
    next_offset: u64,
    failed: bool,
    overrun: bool,

    /// The next frame to start, and the blocks still to decode in the current one.
    frame: u32,
    frame_left: u32,
    frame_flags: u32,
    frame_crc: u32,
    crc_state: u32,
    rc_low: u32,
    rc_range: u32,
    rc_help: u32,
    rc_buffer: u32,
    rice: [Rice; 2],
    short_filters: [Nn<64, { 64 + FILTER_HISTORY }>; 2],
    medium_filters: [Nn<256, { 256 + FILTER_HISTORY }>; 2],
    long_filters: [Nn<1280, { 1280 + FILTER_HISTORY }>; 2],
    predictor: Predictor,
    /// 24-bit files: -1 while undecided, 0 for 32-bit arithmetic, 1 for 64-bit.
    interim_mode: i8,
    interim: [[i32; MAX_BLOCK]; 2],

    /// Samples per channel up to the end of the last block `next_block` handed out.
    decoded: u64,
    block_len: usize,
    pending: bool,
    /// The decoded block: `samples[channel][..n]`. Valid after `next_block` returned `n`.
    pub samples: [[i32; MAX_BLOCK]; 2],
}

impl<S: Source> Decoder<S> {
    pub const fn new(source: S) -> Self {
        Decoder {
            source,
            junk: 0,
            table_offset: 0,
            first_frame: 0,
            table_entries: 0,
            filter_set: 0,
            blocks_per_frame: 0,
            final_blocks: 0,
            total_frames: 0,
            sample_rate: 0,
            channels: 0,
            bits_per_sample: 0,
            total_samples: 0,
            compression_level: 0,
            version: 0,
            tags: Tags::new(),
            crc_errors: 0,
            buffer: [0; INPUT],
            buf_pos: 0,
            buf_end: 0,
            next_offset: 0,
            failed: false,
            overrun: false,
            frame: 0,
            frame_left: 0,
            frame_flags: 0,
            frame_crc: 0,
            crc_state: 0,
            rc_low: 0,
            rc_range: 0,
            rc_help: 0,
            rc_buffer: 0,
            rice: [Rice::new(); 2],
            short_filters: [Nn::new(), Nn::new()],
            medium_filters: [Nn::new(), Nn::new()],
            long_filters: [Nn::new(), Nn::new()],
            predictor: Predictor::new(),
            interim_mode: 0,
            interim: [[0; MAX_BLOCK]; 2],
            decoded: 0,
            block_len: 0,
            pending: false,
            samples: [[0; MAX_BLOCK]; 2],
        }
    }

    pub fn source(&mut self) -> &mut S {
        &mut self.source
    }

    /// Reads the descriptor, the header and the tags; leaves the decoder at the first frame.
    pub fn open(&mut self) -> Result<(), Error> {
        self.tags.clear();
        self.crc_errors = 0;
        self.decoded = 0;
        self.pending = false;
        self.frame = 0;
        self.frame_left = 0;
        self.junk = self.id3v2_length()?;
        let mut head = [0u8; 76];
        if self.read_exact(self.junk, &mut head[..52])? < 52 || &head[..4] != b"MAC " {
            return Err(Error::Format("not a Monkey's Audio file (no MAC signature)"));
        }
        self.version = u16::from_le_bytes([head[4], head[5]]);
        if !(3990..=3999).contains(&self.version) {
            return Err(Error::Unsupported("Monkey's Audio files older than version 3.99"));
        }
        let first = head;
        let word0 = |at: usize| u32::from_le_bytes([first[at], first[at + 1], first[at + 2], first[at + 3]]);
        let descriptor = word0(8) as u64;
        let header_len = word0(12) as u64;
        let seek_table = word0(16) as u64;
        let wav_header = word0(20) as u64;
        if descriptor < 52 || header_len < 24 {
            return Err(Error::Format("a short Monkey's Audio header"));
        }
        if self.read_exact(self.junk + descriptor, &mut head[52..76])? < 24 {
            return Err(Error::Format("a short Monkey's Audio header"));
        }
        let word = |at: usize| u32::from_le_bytes([head[at], head[at + 1], head[at + 2], head[at + 3]]);
        let half = |at: usize| u16::from_le_bytes([head[at], head[at + 1]]);
        self.compression_level = half(52);
        self.blocks_per_frame = word(56);
        self.final_blocks = word(60);
        self.total_frames = word(64);
        self.bits_per_sample = half(68) as u32;
        self.channels = half(70) as u32;
        self.sample_rate = word(72);
        if self.compression_level % 1000 != 0 || !(1000..=5000).contains(&self.compression_level) {
            return Err(Error::Format("an unknown Monkey's Audio compression level"));
        }
        self.filter_set = (self.compression_level / 1000 - 1) as usize;
        if !matches!(self.bits_per_sample, 8 | 16 | 24) {
            return Err(Error::Unsupported("Monkey's Audio with other than 8, 16 or 24 bits"));
        }
        if !(1..=2).contains(&self.channels) {
            return Err(Error::Unsupported("Monkey's Audio with more than two channels"));
        }
        if self.total_frames == 0 || self.blocks_per_frame == 0 || self.sample_rate == 0 {
            return Err(Error::Format("a Monkey's Audio file without audio"));
        }
        self.table_entries = (seek_table / 4) as u32;
        if self.table_entries < self.total_frames {
            return Err(Error::Format("a seek table shorter than the frame count"));
        }
        self.table_offset = self.junk + descriptor + header_len;
        self.first_frame = self.table_offset + seek_table + wav_header;
        self.total_samples = self.final_blocks as u64 + self.blocks_per_frame as u64 * (self.total_frames as u64 - 1);
        self.interim_mode = if self.bits_per_sample == 24 { -1 } else { 0 };
        self.read_tags();
        Ok(())
    }

    /// Back to the first frame.
    pub fn rewind(&mut self) {
        self.decoded = 0;
        self.pending = false;
        self.frame = 0;
        self.frame_left = 0;
    }

    /// Decodes the next block (at most `MAX_BLOCK` samples, within one frame) into `samples`; `None` at the end.
    pub fn next_block(&mut self) -> Result<Option<usize>, Error> {
        if self.pending {
            self.pending = false;
            return Ok(Some(self.block_len));
        }
        if self.frame_left == 0 {
            if self.frame >= self.total_frames {
                return Ok(None);
            }
            self.start_frame(self.frame)?;
            self.frame += 1;
        }
        let count = self.decode_block()?;
        self.block_len = count;
        self.decoded += count as u64;
        Ok(Some(count))
    }

    /// Samples per channel before the block `next_block` returns next.
    pub fn position(&self) -> u64 {
        self.decoded - if self.pending { self.block_len as u64 } else { 0 }
    }

    /// Positions the stream so `next_block` returns the block that holds sample `target` and returns that block's
    /// first sample. Frames are independent, so this decodes (and drops) only the blocks of the target's frame
    /// before it: at most one frame, about 1.7 seconds at 44.1 kHz.
    pub fn seek(&mut self, target: u64) -> Result<u64, Error> {
        let target = target.min(self.total_samples.saturating_sub(1));
        let here = self.position();
        if self.pending && target >= here && target < self.decoded {
            return Ok(here);
        }
        let frame = ((target / self.blocks_per_frame as u64) as u32).min(self.total_frames - 1);
        self.pending = false;
        self.start_frame(frame)?;
        self.frame = frame + 1;
        let mut at = frame as u64 * self.blocks_per_frame as u64;
        while self.frame_left > 0 {
            let count = self.decode_block()?;
            if target < at + count as u64 {
                self.block_len = count;
                self.decoded = at + count as u64;
                self.pending = true;
                return Ok(at);
            }
            at += count as u64;
        }
        self.decoded = at;
        Ok(at)
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

    fn read_exact(&mut self, offset: u64, out: &mut [u8]) -> Result<usize, Error> {
        let mut got = 0;
        while got < out.len() {
            let n = self.source.read_at(offset + got as u64, &mut out[got..])?;
            if n == 0 {
                break;
            }
            got += n;
        }
        Ok(got)
    }

    /// The length of an ID3v2 tag at the start of the file (some taggers put one before the `MAC ` signature), else 0.
    fn id3v2_length(&mut self) -> Result<u64, Error> {
        let mut head = [0u8; 10];
        if self.read_exact(0, &mut head)? < 10 || &head[..3] != b"ID3" {
            return Ok(0);
        }
        let size = head[6..10].iter().fold(0u64, |size, &b| size << 7 | (b & 0x7F) as u64);
        Ok(10 + size + if head[5] & 0x10 != 0 { 10 } else { 0 })
    }

    /// Title, artist and album from the APEv2 tag at the end of the file (before an ID3v1 tag, if there is one).
    /// Best effort: a tag that does not parse leaves the fields empty.
    fn read_tags(&mut self) {
        let size = self.source.size();
        for end in [size, size.saturating_sub(128)] {
            if end >= 32 && self.apev2_tag(end).is_some() {
                return;
            }
        }
    }

    /// Reads the items of the APEv2 tag whose footer ends at `end`.
    fn apev2_tag(&mut self, end: u64) -> Option<()> {
        let mut footer = [0u8; 32];
        if self.read_exact(end - 32, &mut footer).ok()? < 32 || &footer[..8] != b"APETAGEX" {
            return None;
        }
        let word = |at: usize| u32::from_le_bytes([footer[at], footer[at + 1], footer[at + 2], footer[at + 3]]) as u64;
        let (tag_size, items) = (word(12), word(16).min(64));
        if tag_size < 32 || tag_size > end {
            return None;
        }
        let items_end = end - 32;
        let mut at = end - tag_size;
        for _ in 0..items {
            let mut head = [0u8; 72];
            let got = self.read_exact(at, &mut head[..(items_end - at).min(72) as usize]).ok()?;
            if got < 10 {
                break;
            }
            let length = u32::from_le_bytes([head[0], head[1], head[2], head[3]]) as u64;
            let key_len = head[8..got].iter().position(|&b| b == 0)?;
            let value_at = at + 8 + key_len as u64 + 1;
            if value_at + length > items_end {
                break;
            }
            let key = &head[8..8 + key_len];
            if ["title", "artist", "album"].iter().any(|name| key.eq_ignore_ascii_case(name.as_bytes())) {
                let mut value = [0u8; 256];
                let n = self.read_exact(value_at, &mut value[..length.min(256) as usize]).ok()?;
                self.tags.set_named(key, &value[..n]);
            }
            at = value_at + length;
        }
        Some(())
    }

    /// The file offset of frame `index`.
    fn frame_position(&mut self, index: u32) -> Result<u64, Error> {
        if index == 0 {
            return Ok(self.first_frame);
        }
        let mut entry = [0u8; 4];
        if self.read_exact(self.table_offset + 4 * index as u64, &mut entry)? < 4 {
            return Err(Error::Format("the seek table is cut short"));
        }
        Ok(u32::from_le_bytes(entry) as u64 + self.junk)
    }

    /// Reads the next chunk of the file into `buffer`, a whole number of words, each swapped so the bytes come out in
    /// stream order.
    fn refill(&mut self) -> bool {
        let mut n = match self.source.read_at(self.next_offset, &mut self.buffer) {
            Ok(n) => n,
            Err(_) => {
                self.failed = true;
                0
            }
        };
        n -= n % 4;
        if n == 0 {
            return false;
        }
        for word in self.buffer[..n].chunks_exact_mut(4) {
            word.reverse();
        }
        self.next_offset += n as u64;
        self.buf_pos = 0;
        self.buf_end = n;
        true
    }

    fn byte(&mut self) -> u8 {
        if self.buf_pos == self.buf_end && !self.refill() {
            self.overrun = true;
            return 0;
        }
        let byte = self.buffer[self.buf_pos];
        self.buf_pos += 1;
        byte
    }

    fn be32(&mut self) -> u32 {
        let mut value = 0;
        for _ in 0..4 {
            value = value << 8 | self.byte() as u32;
        }
        value
    }

    /// Positions the byte reader at frame `index` and reads its CRC and flags.
    fn start_frame(&mut self, index: u32) -> Result<(), Error> {
        let position = self.frame_position(index)?;
        let skip = position.checked_sub(self.first_frame).ok_or(Error::Format("a frame before the first one"))? & 3;
        self.next_offset = position - skip;
        self.buf_pos = 0;
        self.buf_end = 0;
        self.failed = false;
        self.overrun = false;
        if !self.refill() {
            return Err(if self.failed { Error::Source } else { Error::Format("the file ends before a frame") });
        }
        self.buf_pos = skip as usize;
        self.frame_left = if index + 1 == self.total_frames { self.final_blocks } else { self.blocks_per_frame };
        if self.frame_left == 0 {
            return Err(Error::Format("an empty frame"));
        }

        self.frame_crc = self.be32();
        self.frame_flags = 0;
        if self.frame_crc & 0x8000_0000 != 0 {
            self.frame_crc &= 0x7FFF_FFFF;
            self.frame_flags = self.be32();
        }
        self.crc_state = u32::MAX;
        self.rice = [Rice::new(); 2];
        self.predictor.reset();
        let orders = FILTER_ORDERS[self.filter_set];
        for ch in 0..2 {
            if orders[0] != 0 {
                self.short_filters[ch].reset(orders[0]);
            }
            if orders[1] != 0 {
                self.medium_filters[ch].reset(orders[1]);
            }
            if orders[2] != 0 {
                self.long_filters[ch].reset(orders[2]);
            }
        }
        // The first byte of the range-coded data is ignored; the next ones prime the decoder.
        self.byte();
        self.rc_buffer = self.byte() as u32;
        self.rc_low = self.rc_buffer >> 1;
        self.rc_range = 1 << 7;
        if self.failed {
            return Err(Error::Source);
        }
        if self.overrun {
            return Err(Error::Format("the file ends inside a frame"));
        }
        Ok(())
    }

    fn rc_normalize(&mut self) {
        while self.rc_range <= BOTTOM {
            let byte = self.byte() as u32;
            self.rc_buffer = (self.rc_buffer << 8).wrapping_add(byte);
            self.rc_low = (self.rc_low << 8) | ((self.rc_buffer >> 1) & 0xFF);
            self.rc_range <<= 8;
        }
    }

    /// The cumulative frequency of the next symbol out of `total`; `rc_update` then consumes the symbol.
    fn rc_culfreq(&mut self, total: u32) -> u32 {
        self.rc_normalize();
        self.rc_help = (self.rc_range / total.max(1)).max(1);
        self.rc_low / self.rc_help
    }

    fn rc_culshift(&mut self, bits: u32) -> u32 {
        self.rc_normalize();
        self.rc_help = (self.rc_range >> bits).max(1);
        self.rc_low / self.rc_help
    }

    fn rc_update(&mut self, width: u32, start: u32) {
        self.rc_low = self.rc_low.wrapping_sub(self.rc_help.wrapping_mul(start));
        self.rc_range = self.rc_help.wrapping_mul(width);
    }

    /// `bits` (at most 16) uniform bits.
    fn rc_bits(&mut self, bits: u32) -> u32 {
        let value = self.rc_culshift(bits);
        self.rc_update(1, value);
        value
    }

    /// One symbol of the fixed model; the last two intervals of the 16-bit range are 64 and above.
    fn rc_symbol(&mut self) -> u32 {
        let cf = self.rc_culshift(16);
        if cf > 65492 {
            self.rc_update(1, cf);
            return cf.wrapping_sub(65535).wrapping_add(63).min(MODEL_ELEMENTS - 1);
        }
        let mut symbol = 0;
        while COUNTS[symbol + 1] as u32 <= cf {
            symbol += 1;
        }
        self.rc_update(COUNTS_DIFF[symbol] as u32, COUNTS[symbol] as u32);
        symbol as u32
    }

    /// One signed residual for the channel with `rice` index `which`.
    fn decode_value(&mut self, which: usize) -> i32 {
        let pivot = (self.rice[which].ksum >> 5).max(1);
        let mut overflow = self.rc_symbol();
        if overflow == MODEL_ELEMENTS - 1 {
            overflow = self.rc_bits(16) << 16;
            overflow |= self.rc_bits(16);
        }
        let base = if pivot < 0x10000 {
            let base = self.rc_culfreq(pivot);
            self.rc_update(1, base);
            base
        } else {
            let (mut high, mut bits) = (pivot, 0);
            while high & !0xFFFF != 0 {
                high >>= 1;
                bits += 1;
            }
            let high = self.rc_culfreq(high + 1);
            self.rc_update(1, high);
            let low = self.rc_culfreq(1 << bits);
            self.rc_update(1, low);
            (high << bits) + low
        };
        let x = base.wrapping_add(overflow.wrapping_mul(pivot));
        self.rice[which].update(x);
        if x & 1 != 0 { (x >> 1).wrapping_add(1) as i32 } else { (x >> 1).wrapping_neg() as i32 }
    }

    /// Decodes up to `MAX_BLOCK` samples of the current frame into `samples`.
    fn decode_block(&mut self) -> Result<usize, Error> {
        let count = (self.frame_left as usize).min(MAX_BLOCK);
        let mono = self.channels == 1 || self.frame_flags & FLAG_PSEUDO_STEREO != 0;
        let silent = if mono { self.frame_flags & FLAG_STEREO_SILENCE != 0 } else { self.frame_flags & FLAG_STEREO_SILENCE == FLAG_STEREO_SILENCE };
        let _ = FLAG_MONO_SILENCE;
        self.samples[0][..count].fill(0);
        self.samples[1][..count].fill(0);
        if !silent {
            if mono {
                for i in 0..count {
                    self.samples[0][i] = self.decode_value(0);
                }
            } else {
                for i in 0..count {
                    self.samples[0][i] = self.decode_value(0);
                    self.samples[1][i] = self.decode_value(1);
                }
            }
            if self.failed {
                return Err(Error::Source);
            }
            if self.overrun {
                return Err(Error::Format("the file ends inside a frame"));
            }
            for ch in 0..if mono { 1 } else { 2 } {
                self.run_filters(ch, count);
            }
            if mono {
                self.predictor.mono(&mut self.samples[0][..count]);
                if self.channels == 2 {
                    let (first, second) = self.samples.split_at_mut(1);
                    second[0][..count].copy_from_slice(&first[0][..count]);
                }
            } else {
                self.predict_stereo(count);
                for i in 0..count {
                    let left = self.samples[1][i].wrapping_sub(self.samples[0][i] / 2);
                    self.samples[1][i] = left.wrapping_add(self.samples[0][i]);
                    self.samples[0][i] = left;
                }
            }
        }
        self.frame_left -= count as u32;
        self.update_crc(count);
        Ok(count)
    }

    fn run_filters(&mut self, ch: usize, count: usize) {
        let orders = FILTER_ORDERS[self.filter_set];
        let shifts = FILTER_SHIFTS[self.filter_set];
        let data = &mut self.samples[ch][..count];
        if orders[0] != 0 {
            self.short_filters[ch].apply(orders[0], shifts[0], data);
        }
        if orders[1] != 0 {
            self.medium_filters[ch].apply(orders[1], shifts[1], data);
        }
        if orders[2] != 0 {
            self.long_filters[ch].apply(orders[2], shifts[2], data);
        }
    }

    fn predict_stereo(&mut self, count: usize) {
        let (first, second) = self.samples.split_at_mut(1);
        let (d0, d1) = (&mut first[0][..count], &mut second[0][..count]);
        if self.interim_mode != -1 {
            stereo_pass(&mut self.predictor, d0, d1, self.interim_mode > 0, false);
            return;
        }
        // 24 bits, arithmetic width not known yet: run both and keep the one whose output stays in range.
        let mut wide = self.predictor.clone();
        let (i0, i1) = self.interim.split_at_mut(1);
        let (i0, i1) = (&mut i0[0][..count], &mut i1[0][..count]);
        i0.copy_from_slice(d0);
        i1.copy_from_slice(d1);
        if stereo_pass(&mut self.predictor, d0, d1, false, true) {
            self.interim_mode = 1;
        }
        if stereo_pass(&mut wide, i0, i1, true, true) {
            self.interim_mode = 0;
        }
        if self.interim_mode > 0 {
            d0.copy_from_slice(i0);
            d1.copy_from_slice(i1);
            self.predictor = wide;
        }
    }

    /// Accumulates the CRC of the block's PCM bytes, and checks it against the file's at the end of the frame.
    fn update_crc(&mut self, count: usize) {
        let mut crc = self.crc_state;
        for i in 0..count {
            for ch in 0..self.channels as usize {
                let value = self.samples[ch][i];
                match self.bits_per_sample {
                    8 => crc = crc_update(crc, (value.wrapping_add(0x80)) as u8),
                    16 => {
                        crc = crc_update(crc, value as u8);
                        crc = crc_update(crc, (value >> 8) as u8);
                    }
                    _ => {
                        crc = crc_update(crc, value as u8);
                        crc = crc_update(crc, (value >> 8) as u8);
                        crc = crc_update(crc, (value >> 16) as u8);
                    }
                }
            }
        }
        self.crc_state = crc;
        if self.frame_left == 0 && (!crc >> 1) != self.frame_crc {
            self.crc_errors += 1;
        }
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
        Decoder::to_f32_stereo(self, start, count, out)
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u32 {
        self.channels
    }

    fn bits_per_sample(&self) -> u32 {
        self.bits_per_sample
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
        "APE"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 8192 samples of the first frame of a real 3.99 "high" stereo file, cut off before the end of the frame (so its
    /// CRC cannot match) with a one-entry seek table and an APEv2 tag; the PCM CRCs are from ffmpeg's decoder.
    const FIXTURE: &[u8] = include_bytes!("../testdata/fixture.ape");

    fn crc32(bytes: impl Iterator<Item = u8>) -> u32 {
        !bytes.fold(u32::MAX, crc_update)
    }

    fn block_bytes(d: &Decoder<&[u8]>, n: usize) -> std::vec::Vec<u8> {
        (0..n).flat_map(|i| (0..2).flat_map(move |c| (d.samples[c][i] as i16).to_le_bytes())).collect()
    }

    #[test]
    fn decodes_like_ffmpeg() {
        let mut d = Box::new(Decoder::new(FIXTURE));
        d.open().unwrap();
        assert_eq!((d.sample_rate, d.channels, d.bits_per_sample, d.total_samples), (44100, 2, 16, 8192));
        assert_eq!(d.compression_level, 3000);
        assert_eq!((d.tags.title.as_str(), d.tags.artist.as_str()), ("Fixture", "Test"));
        let mut all = std::vec::Vec::new();
        while let Some(n) = d.next_block().unwrap() {
            all.extend(block_bytes(&d, n));
        }
        assert_eq!(all.len(), 8192 * 4);
        assert_eq!(crc32(all.iter().copied()), 0x969b_ed6c);
    }

    #[test]
    fn seek_returns_the_block_holding_the_sample() {
        let mut d = Box::new(Decoder::new(FIXTURE));
        d.open().unwrap();
        assert_eq!(d.seek(5000).unwrap(), 4096);
        let n = d.next_block().unwrap().unwrap();
        assert_eq!(n, 4096);
        assert_eq!(crc32(block_bytes(&d, n).into_iter()), 0xc46f_8a6b);
        assert_eq!(d.seek(u64::MAX).unwrap(), 4096);
        assert_eq!(d.position(), 4096);
    }

    #[test]
    fn rejects_old_versions_and_other_files() {
        let mut old = FIXTURE.to_vec();
        old[4] = 0x56; // 3990 -> 3926
        old[5] = 0x0F;
        assert!(matches!(Decoder::new(&old[..]).open(), Err(Error::Unsupported(_))));
        assert!(matches!(Decoder::new(&b"RIFF____WAVE"[..]).open(), Err(Error::Format(_))));
    }
}
