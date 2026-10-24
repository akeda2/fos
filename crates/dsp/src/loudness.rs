//! Loudness after ITU-R BS.1770-4 / EBU R128 for a stereo pair: K-weighting (a high shelf and a high-pass), the mean square of
//! 400 ms blocks (momentary, M) and of 3 s (short-term, S), the gated integrated loudness (I) and the loudness range (LRA, EBU
//! Tech 3342). Values are LUFS (LU for differences); silence reads `FLOOR_LUFS`.
//!
//! The measurement works on 100 ms sub-blocks: M is the mean of the last 4, S of the last 30, and every sub-block step adds
//! one gating block (400 ms, 75% overlap) to a histogram of 0.1 LU bins from -70 to +10 LUFS that remembers each bin's
//! energy, so I needs no per-block list however long the programme. The two gates are applied to the histogram when read.
//! Filter arithmetic is f64: a 38 Hz high-pass at 48 kHz loses its accuracy in f32.

use crate::math::{ln64, pow10_64, sin_cos64};
use core::f64::consts::PI;

pub const FLOOR_LUFS: f32 = -120.0;
const OFFSET: f64 = -0.691;
const BINS: usize = 800;
const LOWEST: f64 = -70.0;
const SUBS_SHORT: usize = 30;
const SUBS_MOMENTARY: usize = 4;

fn lufs(energy: f64) -> f64 {
    if energy <= 0.0 { -200.0 } else { OFFSET + 10.0 * ln64(energy) / core::f64::consts::LN_10 }
}

#[derive(Clone, Copy)]
struct Biquad {
    b: [f64; 3],
    a: [f64; 2],
    z: [f64; 2],
}

impl Biquad {
    fn process(&mut self, x: f64) -> f64 {
        let y = self.b[0] * x + self.z[0];
        self.z[0] = self.b[1] * x - self.a[0] * y + self.z[1];
        self.z[1] = self.b[2] * x - self.a[1] * y;
        y
    }
}

fn tan(x: f64) -> f64 {
    let (s, c) = sin_cos64(x);
    s / c
}

/// The two K-weighting stages for a sample rate (the BS.1770 analogue prototypes, bilinear transform).
fn k_weighting(rate: f64) -> [Biquad; 2] {
    let (f0, gain, q) = (1681.974450955533, 3.999843853973347, 0.7071752369554196);
    let k = tan(PI * f0 / rate);
    let vh = pow10_64(gain / 20.0);
    let vb = pow10_64(gain / 20.0 * 0.4996667741545416);
    let a0 = 1.0 + k / q + k * k;
    let shelf = Biquad {
        b: [(vh + vb * k / q + k * k) / a0, 2.0 * (k * k - vh) / a0, (vh - vb * k / q + k * k) / a0],
        a: [2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0],
        z: [0.0; 2],
    };
    let (f0, q) = (38.13547087602444, 0.5003270373238773);
    let k = tan(PI * f0 / rate);
    let a0 = 1.0 + k / q + k * k;
    let high_pass = Biquad { b: [1.0, -2.0, 1.0], a: [2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0], z: [0.0; 2] };
    [shelf, high_pass]
}

/// Mean energy and count per 0.1 LU bin of block loudness.
struct Histogram {
    count: [u32; BINS],
    energy: [f64; BINS],
}

impl Histogram {
    const fn new() -> Self {
        Self { count: [0; BINS], energy: [0.0; BINS] }
    }

    fn clear(&mut self) {
        self.count = [0; BINS];
        self.energy = [0.0; BINS];
    }

    fn bin(level: f64) -> usize {
        (((level - LOWEST) * 10.0) as usize).min(BINS - 1)
    }

    fn add(&mut self, energy: f64) {
        let level = lufs(energy);
        if level >= LOWEST {
            let bin = Self::bin(level);
            self.count[bin] += 1;
            self.energy[bin] += energy;
        }
    }

    /// Count and summed energy of the bins from `first` up.
    fn from(&self, first: usize) -> (u64, f64) {
        let mut count = 0u64;
        let mut energy = 0.0;
        for bin in first..BINS {
            count += self.count[bin] as u64;
            energy += self.energy[bin];
        }
        (count, energy)
    }

    /// The mean energy of the blocks above the absolute gate and `relative` LU below the mean of those: None without any.
    fn gated(&self, relative: f64) -> Option<(usize, u64, f64)> {
        let (count, energy) = self.from(0);
        if count == 0 {
            return None;
        }
        let threshold = lufs(energy / count as f64) + relative;
        let edge = (threshold - LOWEST) * 10.0;
        // the first bin whose lower edge is at or above the threshold
        let first = if edge <= 0.0 { 0 } else if edge as usize as f64 >= edge { edge as usize } else { edge as usize + 1 };
        let (kept, kept_energy) = self.from(first.min(BINS));
        (kept > 0).then_some((first, kept, kept_energy))
    }
}

pub struct Loudness {
    filters: [[Biquad; 2]; 2],
    sub_length: usize,
    sub_fill: usize,
    sub_energy: f64,
    ring: [f64; SUBS_SHORT],
    ring_next: usize,
    ring_len: usize,
    blocks: Histogram,
    short_terms: Histogram,
    momentary_energy: f64,
    short_energy: f64,
    /// A new 100 ms step has completed since the caller last looked (`take_step`).
    step: bool,
}

impl Loudness {
    pub fn new(rate: u32) -> Self {
        let k = k_weighting(rate as f64);
        Self {
            filters: [k, k],
            sub_length: (rate / 10).max(1) as usize,
            sub_fill: 0,
            sub_energy: 0.0,
            ring: [0.0; SUBS_SHORT],
            ring_next: 0,
            ring_len: 0,
            blocks: Histogram::new(),
            short_terms: Histogram::new(),
            momentary_energy: 0.0,
            short_energy: 0.0,
            step: false,
        }
    }

    /// Clears the integrated value, the range and the windows (the filters keep running).
    pub fn reset(&mut self) {
        self.blocks.clear();
        self.short_terms.clear();
        self.ring = [0.0; SUBS_SHORT];
        self.ring_len = 0;
        self.ring_next = 0;
        self.sub_fill = 0;
        self.sub_energy = 0.0;
        self.momentary_energy = 0.0;
        self.short_energy = 0.0;
    }

    /// Feeds sample pairs (-1..1 full scale) as they arrive.
    pub fn feed(&mut self, left: &[f32], right: &[f32]) {
        for (&l, &r) in left.iter().zip(right) {
            let mut energy = 0.0;
            for (channel, sample) in [l, r].into_iter().enumerate() {
                let mut x = sample as f64;
                for stage in &mut self.filters[channel] {
                    x = stage.process(x);
                }
                energy += x * x;
            }
            self.sub_energy += energy;
            self.sub_fill += 1;
            if self.sub_fill == self.sub_length {
                self.finish_sub();
            }
        }
    }

    fn finish_sub(&mut self) {
        self.ring[self.ring_next] = self.sub_energy / self.sub_length as f64;
        self.ring_next = (self.ring_next + 1) % SUBS_SHORT;
        self.ring_len = (self.ring_len + 1).min(SUBS_SHORT);
        self.sub_fill = 0;
        self.sub_energy = 0.0;
        self.momentary_energy = self.mean_of_last(SUBS_MOMENTARY);
        self.short_energy = self.mean_of_last(SUBS_SHORT);
        if self.ring_len >= SUBS_MOMENTARY {
            self.blocks.add(self.momentary_energy);
        }
        if self.ring_len >= SUBS_SHORT {
            self.short_terms.add(self.short_energy);
        }
        self.step = true;
    }

    /// The mean energy of the last `n` sub-blocks (fewer while the window is still filling counts the missing as silence).
    fn mean_of_last(&self, n: usize) -> f64 {
        let mut sum = 0.0;
        for back in 1..=n.min(self.ring_len) {
            sum += self.ring[(self.ring_next + SUBS_SHORT - back) % SUBS_SHORT];
        }
        sum / n as f64
    }

    /// True once per completed 100 ms step: the caller samples `short_term` for a history at its own pace.
    pub fn take_step(&mut self) -> bool {
        core::mem::replace(&mut self.step, false)
    }

    fn reading(energy: f64) -> f32 {
        if energy <= 0.0 { FLOOR_LUFS } else { (lufs(energy) as f32).max(FLOOR_LUFS) }
    }

    /// Momentary loudness, the last 400 ms.
    pub fn momentary(&self) -> f32 {
        Self::reading(self.momentary_energy)
    }

    /// Short-term loudness, the last 3 s.
    pub fn short_term(&self) -> f32 {
        Self::reading(self.short_energy)
    }

    /// Integrated loudness with the absolute (-70 LUFS) and relative (-10 LU) gates; None until a block passes both.
    pub fn integrated(&self) -> Option<f32> {
        let (_, count, energy) = self.blocks.gated(-10.0)?;
        Some(lufs(energy / count as f64) as f32)
    }

    /// Loudness range in LU (10th to 95th percentile of the short-term values above the -20 LU relative gate); None until
    /// 3 s of programme have been measured.
    pub fn range(&self) -> Option<f32> {
        let (first, count, _) = self.short_terms.gated(-20.0)?;
        let at = |fraction: f64| -> f64 {
            let target = (count as f64 * fraction) as u64;
            let mut seen = 0u64;
            for bin in first..BINS {
                seen += self.short_terms.count[bin] as u64;
                if seen > target {
                    return LOWEST + bin as f64 / 10.0;
                }
            }
            LOWEST + (BINS - 1) as f64 / 10.0
        };
        Some((at(0.95) - at(0.10)) as f32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::vec::Vec;

    fn sine(rate: u32, seconds: f32, hz: f32, db: f32) -> Vec<f32> {
        let amplitude = pow10_64(db as f64 / 20.0);
        (0..(rate as f32 * seconds) as usize).map(|n| (amplitude * sin_cos64(2.0 * PI * hz as f64 * n as f64 / rate as f64).0) as f32).collect()
    }

    #[test]
    fn k_weighting_matches_the_published_48k_coefficients() {
        let [shelf, high_pass] = k_weighting(48_000.0);
        let near = |a: f64, b: f64| (a - b).abs() < 1e-9;
        assert!(near(shelf.b[0], 1.53512485958697) && near(shelf.b[1], -2.69169618940638) && near(shelf.b[2], 1.19839281085285));
        assert!(near(shelf.a[0], -1.69065929318241) && near(shelf.a[1], 0.73248077421585));
        assert!(near(high_pass.a[0], -1.99004745483398) && near(high_pass.a[1], 0.99007225036621));
    }

    #[test]
    fn a_stereo_sine_at_minus_23_reads_minus_23() {
        // EBU Tech 3341 test 1: stereo 997 Hz, -23 dBFS peak is -23.0 LUFS in M, S and I.
        for rate in [44_100, 48_000, 96_000] {
            let tone = sine(rate, 25.0, 997.0, -23.0);
            let mut m = Loudness::new(rate);
            m.feed(&tone, &tone);
            assert!((m.momentary() + 23.0).abs() < 0.1, "{rate} M {}", m.momentary());
            assert!((m.short_term() + 23.0).abs() < 0.1, "{rate} S {}", m.short_term());
            assert!((m.integrated().unwrap() + 23.0).abs() < 0.1, "{rate} I {:?}", m.integrated());
            assert!(m.range().unwrap() < 0.2);
        }
    }

    #[test]
    fn the_gates_ignore_quiet_passages_and_silence_reads_nothing() {
        // 20 s at -20 LUFS, then 20 s at -40 (more than 10 LU lower): I stays at -20.
        let mut m = Loudness::new(48_000);
        assert_eq!(m.integrated(), None);
        let loud = sine(48_000, 20.0, 997.0, -20.0);
        let quiet = sine(48_000, 20.0, 997.0, -40.0);
        m.feed(&loud, &loud);
        m.feed(&quiet, &quiet);
        assert!((m.integrated().unwrap() + 20.0).abs() < 0.1, "{:?}", m.integrated());
        let silence = std::vec![0.0f32; 48_000];
        let mut s = Loudness::new(48_000);
        s.feed(&silence, &silence);
        assert_eq!(s.momentary(), FLOOR_LUFS);
        assert_eq!(s.integrated(), None);
    }

    #[test]
    fn range_follows_a_step_and_reset_clears() {
        // 20 s at -33 then 20 s at -23: the range is about 10 LU (EBU Tech 3342 case 2 uses +-10 steps).
        let mut m = Loudness::new(48_000);
        let low = sine(48_000, 20.0, 997.0, -33.0);
        let high = sine(48_000, 20.0, 997.0, -23.0);
        m.feed(&low, &low);
        m.feed(&high, &high);
        let range = m.range().unwrap();
        assert!((range - 10.0).abs() < 0.5, "{range}");
        m.reset();
        assert_eq!(m.integrated(), None);
        assert_eq!(m.range(), None);
    }
}
