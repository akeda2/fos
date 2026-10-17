//! Long-term average spectrum analysis, the engine behind the `ltas` tape and reusable by other audio tapes.
//!
//! Samples (mono, f32 in -1..1) go into `Analyzer::push`. Every `hop` samples the last `n` are windowed, transformed
//! (`dsp::fft`), turned into one-sided mean-square per bin, weighted (A/C/Z) and summed into fractional-octave bands (or kept
//! as FFT bins), and fed to an `Accumulator` that keeps the live, exponential, linear (the LTAS) and peak views at once.
//!
//! Levels are dB re a full-scale sine (a sine of amplitude 1.0 reads 0 dB in the band that holds it) plus the calibration
//! offset. Band power is the sum of each bin's power times the part of the bin inside the band, so noise-like and tonal
//! signals both come out right (FFT-bin outputs are normalised for noise, so a tone reads about 2-3 dB low there) and a band narrower than a bin is not simply missed (`Analyzer::band_is_resolved` says when a
//! band is narrower than two bins and so only approximate).
#![cfg_attr(not(test), no_std)]

pub mod average;
pub mod bands;
pub mod csv;
pub mod weighting;

use dsp::fft::Plan;
use dsp::math::{log10, pow10};
use dsp::window::{self, Kind};

pub use average::{Accumulator, Mode};
pub use bands::Resolution;
pub use weighting::Weighting;

pub const LOWEST_HZ: f32 = 20.0;
pub const HIGHEST_HZ: f32 = 20000.0;
const FLOOR_DB: f32 = -200.0;

#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub sample_rate: u32,
    /// FFT length, a power of two.
    pub n: usize,
    /// New samples between frames; `n / 2` is 50 % overlap.
    pub hop: usize,
    pub window: Kind,
    pub resolution: Resolution,
    pub weighting: Weighting,
    /// Added to every level, so that a measured sine of known level can be made to read right.
    pub calibration_db: f32,
    /// Time constant of the exponential view.
    pub tau_s: f32,
    /// Run the AVX2/FMA build of the FFT (only when `fp_level` says so).
    pub avx2: bool,
}

impl Config {
    pub fn new(sample_rate: u32) -> Self {
        Self { sample_rate, n: 8192, hop: 4096, window: Kind::Hann, resolution: Resolution::Third, weighting: Weighting::Z, calibration_db: 0.0, tau_s: 1.0, avx2: false }
    }
}

/// `N` is the largest FFT length (buffers are sized for it); the output count never exceeds `N / 2 + 1`.
pub struct Analyzer<const N: usize> {
    plan: Plan<N>,
    config: Config,
    ring: [f32; N],
    window: [f32; N],
    re: [f32; N],
    im: [f32; N],
    power: [f32; N],
    gain: [f32; N],
    out_lo: [f32; N],
    out_centre: [f32; N],
    out_hi: [f32; N],
    scratch: [f32; N],
    accumulator: Accumulator<N>,
    outputs: usize,
    pos: usize,
    filled: usize,
    since_frame: usize,
    scale: f32,
    ready: bool,
}

impl<const N: usize> Analyzer<N> {
    pub const fn new() -> Self {
        Self {
            plan: Plan::empty(),
            config: Config { sample_rate: 48000, n: 0, hop: 0, window: Kind::Hann, resolution: Resolution::Third, weighting: Weighting::Z, calibration_db: 0.0, tau_s: 1.0, avx2: false },
            ring: [0.0; N],
            window: [0.0; N],
            re: [0.0; N],
            im: [0.0; N],
            power: [0.0; N],
            gain: [0.0; N],
            out_lo: [0.0; N],
            out_centre: [0.0; N],
            out_hi: [0.0; N],
            scratch: [0.0; N],
            accumulator: Accumulator::new(),
            outputs: 0,
            pos: 0,
            filled: 0,
            since_frame: 0,
            scale: 0.0,
            ready: false,
        }
    }

    /// (Re)configures and resets. False when `n` is not a power of two within the analyzer's size or `hop` is 0 or above `n`.
    pub fn configure(&mut self, config: Config) -> bool {
        if config.n > N || config.hop == 0 || config.hop > config.n || !self.plan.init(config.n) {
            self.ready = false;
            return false;
        }
        self.config = config;
        let n = config.n;
        window::fill(config.window, &mut self.window[..n]);
        let power_sum = window::power_sum(&self.window[..n]);
        // One-sided mean square per bin = 2 |X|^2 / (N * sum w^2); relative to a full-scale sine's 0.5, and calibrated.
        self.scale = 2.0 / (n as f32 * power_sum) / 0.5 * pow10(config.calibration_db / 10.0);
        let bin_hz = config.sample_rate as f32 / n as f32;
        for k in 0..=n / 2 {
            self.gain[k] = pow10(config.weighting.db(k as f32 * bin_hz) / 10.0);
        }
        let nyquist = config.sample_rate as f32 / 2.0;
        let top = if HIGHEST_HZ < nyquist { HIGHEST_HZ } else { nyquist };
        self.outputs = 0;
        match config.resolution.per_octave() {
            0 => {
                for k in 1..n / 2 {
                    self.out_lo[self.outputs] = (k as f32 - 0.5) * bin_hz;
                    self.out_centre[self.outputs] = k as f32 * bin_hz;
                    self.out_hi[self.outputs] = (k as f32 + 0.5) * bin_hz;
                    self.outputs += 1;
                }
            }
            per_octave => {
                let mut index = bands::first_band_at_or_above(per_octave, LOWEST_HZ);
                loop {
                    let (lo, centre, hi) = bands::band_edges(per_octave, index);
                    if centre > top || self.outputs >= N {
                        break;
                    }
                    self.out_lo[self.outputs] = lo;
                    self.out_centre[self.outputs] = centre;
                    self.out_hi[self.outputs] = if hi < nyquist { hi } else { nyquist };
                    self.outputs += 1;
                    index += 1;
                }
            }
        }
        let frame_s = config.hop as f32 / config.sample_rate as f32;
        self.accumulator.configure(self.outputs, frame_s, config.tau_s);
        self.pos = 0;
        self.filled = 0;
        self.since_frame = 0;
        self.ring[..n].fill(0.0);
        self.ready = true;
        true
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn reset(&mut self) {
        self.accumulator.reset();
    }

    pub fn frames(&self) -> u64 {
        self.accumulator.frames()
    }

    /// Number of output values (bands, or bins in FFT mode).
    pub fn outputs(&self) -> usize {
        self.outputs
    }

    pub fn centre_hz(&self, index: usize) -> f32 {
        self.out_centre[index]
    }

    pub fn edges_hz(&self, index: usize) -> (f32, f32) {
        (self.out_lo[index], self.out_hi[index])
    }

    /// False for a band narrower than two FFT bins: its level is an interpolation, not a measurement.
    pub fn band_is_resolved(&self, index: usize) -> bool {
        let bin_hz = self.config.sample_rate as f32 / self.config.n as f32;
        self.out_hi[index] - self.out_lo[index] >= 2.0 * bin_hz || self.config.resolution == Resolution::Fft
    }

    /// Feeds samples; returns how many frames were analysed (the tape redraws when it is above zero).
    pub fn push(&mut self, samples: &[f32]) -> usize {
        if !self.ready {
            return 0;
        }
        let n = self.config.n;
        let mut frames = 0;
        for &sample in samples {
            self.ring[self.pos] = sample;
            self.pos += 1;
            if self.pos == n {
                self.pos = 0;
            }
            if self.filled < n {
                self.filled += 1;
            }
            self.since_frame += 1;
            if self.filled == n && self.since_frame >= self.config.hop {
                self.since_frame = 0;
                self.analyse();
                frames += 1;
            }
        }
        frames
    }

    fn analyse(&mut self) {
        let n = self.config.n;
        let (oldest, newest) = self.ring[..n].split_at(self.pos);
        for (i, sample) in newest.iter().chain(oldest.iter()).enumerate() {
            self.re[i] = sample * self.window[i];
        }
        self.im[..n].fill(0.0);
        self.plan.forward(&mut self.re[..n], &mut self.im[..n], self.config.avx2);
        let half = n / 2;
        for k in 0..=half {
            let one_sided = if k == 0 || k == half { 0.5 } else { 1.0 };
            self.power[k] = (self.re[k] * self.re[k] + self.im[k] * self.im[k]) * self.gain[k] * self.scale * one_sided;
        }
        let bin_hz = self.config.sample_rate as f32 / n as f32;
        for o in 0..self.outputs {
            let (lo, hi) = (self.out_lo[o], self.out_hi[o]);
            if self.config.resolution == Resolution::Fft {
                self.scratch[o] = self.power[(self.out_centre[o] / bin_hz + 0.5) as usize];
                continue;
            }
            // Each bin k covers [(k - 1/2), (k + 1/2)] bins; add the share of its power that lies inside [lo, hi].
            let first = ((lo / bin_hz) as usize).saturating_sub(1);
            let last = (hi / bin_hz) as usize + 1;
            let mut sum = 0.0;
            for k in first..=last.min(half) {
                let bin_lo = (k as f32 - 0.5) * bin_hz;
                let bin_hi = (k as f32 + 0.5) * bin_hz;
                let overlap = (if hi < bin_hi { hi } else { bin_hi }) - (if lo > bin_lo { lo } else { bin_lo });
                if overlap > 0.0 {
                    sum += self.power[k] * overlap / bin_hz;
                }
            }
            self.scratch[o] = sum;
        }
        self.accumulator.update(&self.scratch[..self.outputs]);
    }

    /// The chosen view in dB (floored at -200 dB) into `out` (at least `outputs()` long).
    pub fn levels_db(&mut self, mode: Mode, out: &mut [f32]) {
        let n = self.outputs.min(out.len());
        self.accumulator.get(mode, &mut self.scratch[..n]);
        for i in 0..n {
            let db = 10.0 * log10(self.scratch[i]);
            out[i] = if db < FLOOR_DB { FLOOR_DB } else { db };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const N: usize = 8192;

    fn analyzer(config: Config) -> Box<Analyzer<N>> {
        let mut analyzer = Box::new(Analyzer::<N>::new());
        assert!(analyzer.configure(config));
        analyzer
    }

    fn sine(amplitude: f32, hz: f32, rate: u32, samples: usize) -> Vec<f32> {
        (0..samples).map(|t| amplitude * (2.0 * std::f64::consts::PI * hz as f64 * t as f64 / rate as f64).sin() as f32).collect()
    }

    /// xorshift, uniform in -1..1: deterministic white noise.
    fn white(samples: usize, seed: u32) -> Vec<f32> {
        let mut state = seed;
        (0..samples)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                (state as f32 / u32::MAX as f32) * 2.0 - 1.0
            })
            .collect()
    }

    fn peak_band(analyzer: &mut Analyzer<N>) -> (usize, f32) {
        let mut levels = vec![0.0; analyzer.outputs()];
        analyzer.levels_db(Mode::Linear, &mut levels);
        let index = (0..levels.len()).max_by(|&a, &b| levels[a].total_cmp(&levels[b])).unwrap();
        (index, levels[index])
    }

    #[test]
    fn band_layout_for_48k() {
        let analyzer = analyzer(Config::new(48000));
        assert_eq!(analyzer.outputs(), 31); // 20 Hz .. 20 kHz, 1/3 octave
        assert!((analyzer.centre_hz(0) - 19.95).abs() < 0.05);
        assert!((analyzer.centre_hz(30) - 19953.0).abs() < 30.0);
    }

    #[test]
    fn a_sine_reads_its_level_in_its_band_for_every_resolution_and_window() {
        for resolution in [Resolution::Third, Resolution::Sixth, Resolution::Twelfth] {
            for window in Kind::ALL {
                let mut config = Config::new(48000);
                config.resolution = resolution;
                config.window = window;
                let mut analyzer = analyzer(config);
                analyzer.push(&sine(0.5, 1000.0, 48000, 48000));
                let (index, level) = peak_band(&mut analyzer);
                assert!((analyzer.centre_hz(index) / 1000.0 - 1.0).abs() < 0.08, "{resolution:?} {window:?} peak at {}", analyzer.centre_hz(index));
                // amplitude 0.5 is -6.02 dB re a full-scale sine; allow the window's scalloping.
                assert!((level + 6.02).abs() < 0.8, "{resolution:?} {window:?}: {level}");
            }
        }
    }

    #[test]
    fn calibration_and_weighting_apply() {
        let mut config = Config::new(48000);
        config.calibration_db = 10.0;
        config.weighting = Weighting::A;
        let mut analyzer = analyzer(config);
        analyzer.push(&sine(0.5, 1000.0, 48000, 48000));
        let (_, level) = peak_band(&mut analyzer);
        assert!((level - 3.98).abs() < 0.8, "{level}"); // -6.02 + 10 + 0 (A at 1 kHz)
        let mut config = Config::new(48000);
        config.weighting = Weighting::A;
        let mut analyzer = self::analyzer(config);
        analyzer.push(&sine(0.5, 100.0, 48000, 48000));
        let (_, level) = peak_band(&mut analyzer);
        assert!((level - (-6.02 - 19.1)).abs() < 1.0, "{level}"); // A(100 Hz) = -19.1 dB
    }

    #[test]
    fn white_noise_is_flat_per_band_width_and_totals_its_power() {
        let mut config = Config::new(48000);
        config.resolution = Resolution::Twelfth;
        let mut analyzer = analyzer(config);
        let noise = white(48000 * 20, 12345);
        analyzer.push(&noise);
        let variance = noise.iter().map(|v| (*v as f64) * (*v as f64)).sum::<f64>() / noise.len() as f64;
        let mut levels = vec![0.0; analyzer.outputs()];
        analyzer.levels_db(Mode::Linear, &mut levels);
        // The bands tile 20 Hz..20 kHz of the 0..24 kHz the noise fills, so their total is that share of the variance.
        let (lo, _) = analyzer.edges_hz(0);
        let (_, hi) = analyzer.edges_hz(analyzer.outputs() - 1);
        let expected = variance * (hi - lo) as f64 / 24000.0 / 0.5;
        let total: f64 = levels.iter().map(|db| 10f64.powf(*db as f64 / 10.0)).sum();
        assert!((10.0 * (total / expected).log10()).abs() < 0.3, "total {total} expected {expected}");
        // and a band's power follows its width: the top octave carries ~half the power of the 20 kHz tiling.
        let spread = (levels[levels.len() - 1] - levels[levels.len() - 13] - 3.0).abs();
        assert!(spread < 1.5, "one octave up should be 3 dB more: {spread}");
    }

    #[test]
    fn fft_mode_gives_one_output_per_bin() {
        let mut config = Config::new(48000);
        config.resolution = Resolution::Fft;
        let mut analyzer = analyzer(config);
        assert_eq!(analyzer.outputs(), N / 2 - 1);
        analyzer.push(&sine(0.5, 1000.0, 48000, 48000));
        let (index, level) = peak_band(&mut analyzer);
        assert!((analyzer.centre_hz(index) - 1000.0).abs() < 6.0);
        // Bins are normalised for noise (power per bin): a tone reads 1/ENBW (-1.76 dB for Hann) low, plus up to 1.4 dB of
        // scalloping when it falls between bins.
        assert!((level + 6.02 + 1.76).abs() < 1.6 && level < -6.02, "{level}");
    }

    #[test]
    fn reset_and_bad_config() {
        let mut analyzer = analyzer(Config::new(48000));
        let frames = analyzer.push(&white(N * 3, 1));
        assert!(frames >= 4 && analyzer.frames() as usize == frames);
        analyzer.reset();
        assert_eq!(analyzer.frames(), 0);
        let mut bad = Config::new(48000);
        bad.n = 1000;
        assert!(!analyzer.configure(bad));
        assert_eq!(analyzer.push(&[0.0; 100]), 0);
        bad.n = 2 * N;
        assert!(!analyzer.configure(bad));
    }
}
