//! Choosing how many bits an archive needs, and getting audio down to that depth.
//!
//! A record's own noise floor is far above 24-bit quantisation, so the archive can keep fewer bits without losing
//! anything the record carried. `NoiseMeter` listens to a whole (finished) recording, per channel, and says where
//! its noise floor lies; `pick_bits` turns that into a depth; `Quantizer` rounds to it with TPDF dither.
//!
//! The quantised samples stay in a 24-bit container with the low bits zero, so the FLAC encoder's wasted-bits
//! detection makes the file as small as a true N-bit one, and every player still sees an ordinary 24-bit file.
//!
//! Levels are kept as log2 of mean square in half steps (1.505 dB each) so nothing here needs `libm`.

/// Samples measured per block. 1024 at 48 kHz is 21 ms: short enough to find the gaps between tracks and the
/// quiet run-in groove, long enough that the RMS of noise is steady.
pub const METER_BLOCK: usize = 1024;

const BINS: usize = 96;
/// dB per histogram bin: 10 log10(2) / 2.
const DB_PER_BIN: f32 = 1.505;
/// Level of bin 0 (mean square 1 in 24-bit sample units) in dB below full scale.
const BIN0_DBFS: f32 = -138.62;

/// A noise-floor estimate of one channel from a histogram of block levels. Feed it 24-bit-scale integer samples
/// (a full-scale sine peaks at ±8388607) in any chunk sizes.
pub struct NoiseMeter {
    hist: [u32; BINS],
    sum: u64,
    n: usize,
    blocks: u32,
}

impl NoiseMeter {
    pub const fn new() -> Self {
        NoiseMeter { hist: [0; BINS], sum: 0, n: 0, blocks: 0 }
    }

    pub fn feed(&mut self, samples: &[i32]) {
        for &s in samples {
            let v = s as i64;
            self.sum += (v * v) as u64;
            self.n += 1;
            if self.n == METER_BLOCK {
                self.close();
            }
        }
    }

    fn close(&mut self) {
        let e = self.sum / self.n as u64;
        self.sum = 0;
        self.n = 0;
        // Digital silence (a muted gap) says nothing about the record's noise.
        if e == 0 {
            return;
        }
        let l = 63 - e.leading_zeros();
        let half = if l > 0 { ((e >> (l - 1)) & 1) as usize } else { 0 };
        let bin = (2 * l as usize + half).min(BINS - 1);
        self.hist[bin] += 1;
        self.blocks += 1;
    }

    /// Blocks that held signal.
    pub fn blocks(&self) -> u32 {
        self.blocks
    }

    /// The bin below which `percent` of the non-silent blocks lie.
    fn percentile_bin(&self, percent: u32) -> Option<usize> {
        if self.blocks == 0 {
            return None;
        }
        let target = (self.blocks as u64 * percent as u64).div_ceil(100).max(1);
        let mut seen = 0u64;
        for (i, &c) in self.hist.iter().enumerate() {
            seen += c as u64;
            if seen >= target {
                return Some(i);
            }
        }
        None
    }

    /// The noise floor as a histogram bin: the 10th percentile of block levels. A record has music in most blocks, so
    /// the quietest tenth is the groove noise (or the room's, for a live capture).
    pub fn floor_bin(&self) -> Option<usize> {
        self.percentile_bin(10)
    }

    /// The same in dB relative to full scale.
    pub fn floor_dbfs(&self) -> Option<f32> {
        self.floor_bin().map(|b| b as f32 * DB_PER_BIN + BIN0_DBFS)
    }
}

impl Default for NoiseMeter {
    fn default() -> Self {
        Self::new()
    }
}

/// The fewest bits (`min..=24`) whose dithered quantisation noise sits `margin_db` below `floor_bin`. TPDF dither of
/// an N-bit grid has a noise power of (LSB/2)², which in 24-bit units is 2^(46-2N). `None` (nothing measured) gives 24.
pub fn pick_bits(floor_bin: Option<usize>, margin_db: u32, min: u32) -> u32 {
    let Some(floor) = floor_bin else { return 24 };
    let margin = ((margin_db * 1000 + 1504) / 1505) as isize;
    for n in min.clamp(8, 24)..=24 {
        let quant_bin = 2 * (46 - 2 * n as isize);
        if quant_bin + margin <= floor as isize {
            return n;
        }
    }
    24
}

/// Several channels' meters; the archive uses one depth for all, set by the noisiest requirement (the lowest
/// floor, since the quietest channel needs the finest grid).
pub fn lowest_floor(meters: &[NoiseMeter]) -> Option<usize> {
    meters.iter().filter_map(|m| m.floor_bin()).min()
}

/// Rounds samples to `bits` with TPDF dither (±1 LSB triangular), returning 24-bit-container integers whose low
/// `24 - bits` bits are zero. At 24 bits it only rounds and clips: the grid is already finer than any source.
pub struct Quantizer {
    bits: u32,
    rng: u32,
}

impl Quantizer {
    pub const fn new(bits: u32, seed: u32) -> Self {
        Quantizer { bits, rng: seed | 1 }
    }

    pub fn bits(&self) -> u32 {
        self.bits
    }

    fn next(&mut self) -> f32 {
        // xorshift32 -> [0, 1)
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.rng = x;
        (x >> 8) as f32 / 16_777_216.0
    }

    /// `x` in [-1, 1] full scale.
    pub fn quantize(&mut self, x: f32) -> i32 {
        let scale = (1u32 << (self.bits - 1)) as f32;
        let mut v = x * scale;
        if self.bits < 24 {
            v += self.next() - self.next();
        }
        let r = floor(v + 0.5);
        let max = scale - 1.0;
        let r = r.clamp(-scale, max);
        (r as i32) << (24 - self.bits)
    }

    /// Same for a 24-bit-scale integer (the exact value a 24-bit capture holds).
    pub fn quantize_i24(&mut self, s: i32) -> i32 {
        self.quantize(s as f32 / 8_388_608.0)
    }
}

fn floor(v: f32) -> f32 {
    let i = v as i32 as f32;
    if v < i { i - 1.0 } else { i }
}
