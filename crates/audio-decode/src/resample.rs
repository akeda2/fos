//! A streaming stereo sample-rate converter: windowed-sinc interpolation (a Kaiser window, 16 zero crossings on each
//! side, about -90 dB stop band), with the low-pass cutoff placed just under the lower of the two Nyquist frequencies so
//! that downsampling does not alias. The kernel is tabulated once and read with linear interpolation.
//!
//! The ratio is kept as the integers `in_rate` and `out_rate`, so the output position never drifts however long a track
//! is. Every output frame is the kernel-weighted sum of the input frames around its time, divided by the sum of the
//! weights (exact unity gain at DC however the table rounds). The converter delays the signal by `half_width` input
//! frames; `finish` flushes them. Ratios above 8:1 downward are clamped to a cutoff of 1/8 (they would alias a little).
//! A file whose rate the device offers is not resampled at all.

use dsp::math::{sin_cos64, sqrt64};

/// Zero crossings of the sinc kept on each side of the centre.
const HALF: usize = 16;
/// Table entries per zero crossing.
const OVER: usize = 256;
const TABLE: usize = HALF * OVER + 2;
/// Input frames kept: twice the widest half-width (the cutoff is never below 1/8) plus a little.
const RING: usize = 512;
const BETA: f64 = 9.0;
/// Where the low-pass sits relative to the Nyquist frequency of the slower rate.
const CUTOFF: f32 = 0.97;

pub struct Resampler {
    table: [f32; TABLE],
    ready: bool,
    in_rate: i64,
    out_rate: i64,
    cutoff: f32,
    /// Input frames each side of an output time that the kernel reaches.
    half_width: i64,
    ring: [[f32; 2]; RING],
    /// Input frames pushed so far.
    count: i64,
    /// Time of the next output frame, in 1/`out_rate` of an input frame.
    position: i64,
}

/// Modified Bessel function of the first kind, order 0.
fn bessel_i0(x: f64) -> f64 {
    let (mut term, mut sum) = (1.0, 1.0);
    let quarter = x * x / 4.0;
    for k in 1..40 {
        term *= quarter / (k * k) as f64;
        sum += term;
    }
    sum
}

impl Resampler {
    /// An unconfigured converter (a `static` is the place for it: the kernel table is 16 KiB).
    pub const fn new() -> Self {
        Resampler {
            table: [0.0; TABLE],
            ready: false,
            in_rate: 1,
            out_rate: 1,
            cutoff: 1.0,
            half_width: 1,
            ring: [[0.0; 2]; RING],
            count: 0,
            position: 0,
        }
    }

    /// Sets the rates and starts over.
    pub fn configure(&mut self, in_rate: u32, out_rate: u32) {
        if !self.ready {
            let norm = bessel_i0(BETA);
            for (j, slot) in self.table.iter_mut().enumerate() {
                let u = j as f64 / OVER as f64;
                let x = core::f64::consts::PI * u;
                let sinc = if j == 0 { 1.0 } else { sin_cos64(x).0 / x };
                let edge = u / HALF as f64;
                let window = if edge >= 1.0 { 0.0 } else { bessel_i0(BETA * sqrt64(1.0 - edge * edge)) / norm };
                *slot = (sinc * window) as f32;
            }
            self.ready = true;
        }
        self.in_rate = in_rate as i64;
        self.out_rate = out_rate as i64;
        let ratio = (out_rate as f32 / in_rate as f32).min(1.0).max(0.125);
        self.cutoff = ratio * CUTOFF;
        self.half_width = (HALF as f32 / self.cutoff) as i64 + 1;
        self.reset();
    }

    pub fn reset(&mut self) {
        self.count = 0;
        self.position = 0;
    }

    fn frame(&self, index: i64) -> [f32; 2] {
        if index < 0 { [0.0; 2] } else { self.ring[index as usize % RING] }
    }

    /// Takes the next input frame and calls `emit` for every output frame that can now be completed.
    pub fn push(&mut self, frame: [f32; 2], emit: &mut impl FnMut([f32; 2])) {
        self.ring[self.count as usize % RING] = frame;
        self.count += 1;
        loop {
            let base = self.position / self.out_rate;
            if base + self.half_width > self.count - 1 {
                break;
            }
            let offset = (self.position % self.out_rate) as f32 / self.out_rate as f32;
            let (mut left, mut right, mut total) = (0.0f32, 0.0f32, 0.0f32);
            for i in base - self.half_width + 1..=base + self.half_width {
                let u = ((i - base) as f32 - offset).abs() * self.cutoff;
                if u >= HALF as f32 {
                    continue;
                }
                let at = u * OVER as f32;
                let j = at as usize;
                let weight = self.table[j] + (self.table[j + 1] - self.table[j]) * (at - j as f32);
                let [l, r] = self.frame(i);
                left += weight * l;
                right += weight * r;
                total += weight;
            }
            emit([left / total, right / total]);
            self.position += self.in_rate;
        }
    }

    /// Flushes the frames still inside the filter: call once after the last input frame.
    pub fn finish(&mut self, emit: &mut impl FnMut([f32; 2])) {
        for _ in 0..self.half_width {
            self.push([0.0; 2], emit);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(in_rate: u32, out_rate: u32, input: &[[f32; 2]]) -> Vec<[f32; 2]> {
        let mut resampler = Box::new(Resampler::new());
        resampler.configure(in_rate, out_rate);
        let mut out = Vec::new();
        for frame in input {
            resampler.push(*frame, &mut |sample| out.push(sample));
        }
        resampler.finish(&mut |sample| out.push(sample));
        out
    }

    fn sine(rate: u32, hz: f64, count: usize) -> Vec<[f32; 2]> {
        (0..count).map(|i| {
            let x = (2.0 * std::f64::consts::PI * hz * i as f64 / rate as f64).sin() as f32;
            [x, -x]
        }).collect()
    }

    fn peak(out: &[[f32; 2]]) -> f32 {
        out[out.len() / 4..out.len() * 3 / 4].iter().fold(0.0f32, |m, s| m.max(s[0].abs()))
    }

    #[test]
    fn length_follows_the_ratio() {
        for (from, to) in [(44100, 48000), (48000, 44100), (22050, 48000), (96000, 48000), (32000, 48000)] {
            let out = run(from, to, &sine(from, 1000.0, from as usize));
            assert!((out.len() as i64 - to as i64).abs() <= 1, "{from}->{to}: {}", out.len());
        }
    }

    #[test]
    fn a_tone_keeps_its_pitch_phase_and_level() {
        let out = run(44100, 48000, &sine(44100, 1000.0, 44100));
        for (n, sample) in out.iter().enumerate().skip(200).take(40000) {
            let want = (2.0 * std::f64::consts::PI * 1000.0 * n as f64 / 48000.0).sin() as f32;
            assert!((sample[0] - want).abs() < 2e-4, "{n}: {} vs {want}", sample[0]);
            assert_eq!(sample[1], -sample[0]);
        }
    }

    #[test]
    fn downsampling_removes_what_would_alias() {
        // 30 kHz at 96 kHz is above the 24 kHz Nyquist of the output: it must vanish, not fold down to 18 kHz.
        let gone = run(96000, 48000, &sine(96000, 30000.0, 96000));
        assert!(peak(&gone) < 0.003, "{}", peak(&gone));
        let kept = run(96000, 48000, &sine(96000, 10000.0, 96000));
        assert!((peak(&kept) - 1.0).abs() < 0.002, "{}", peak(&kept));
    }

    #[test]
    fn silence_in_silence_out_and_dc_is_exact() {
        let out = run(44100, 48000, &vec![[0.5, 0.5]; 5000]);
        assert!(out[100..4000].iter().all(|s| (s[0] - 0.5).abs() < 1e-6));
    }
}
