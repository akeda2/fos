//! Test-signal generator for audio tapes: white and pink noise, sine, silence, optional on/off bursts, with a click-free
//! fade at every start, stop and burst edge. Levels are RMS dB re the RMS of a full-scale sine,
//! the same scale as the `ltas` analyzer, so a generated 0 dB sine is a full-scale sine and `-20` dB noise reads -20 dB in the `ltas`
//! total. Output is one mono f32 stream (`Generator::fill`); `pack` writes it into the device's frames.
//!
//! White noise is the sum of twelve uniform draws (near Gaussian, crest factor ~4 at the 99.99 % level) from a xorshift
//! generator with a fixed seed, so a run is repeatable. Pink noise filters that white with Paul Kellet's refined 7-pole
//! filter (-3 dB/octave within 0.05 dB between 10 Hz and 0.9 of Nyquist at 44.1/48 kHz; the poles do not move with the rate,
//! so at 96 kHz the slope holds but the lowest octaves are a little off). Samples are clipped at +-1.
#![cfg_attr(not(test), no_std)]

use dsp::math::{pow10, sin_cos64};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Silence,
    White,
    Pink,
    Sine,
}

impl Kind {
    pub const ALL: [Kind; 4] = [Kind::Silence, Kind::White, Kind::Pink, Kind::Sine];

    pub fn name(self) -> &'static str {
        match self {
            Kind::Silence => "off",
            Kind::White => "white",
            Kind::Pink => "pink",
            Kind::Sine => "sine",
        }
    }

    pub fn from_name(name: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.name() == name)
    }

    pub fn next(self) -> Kind {
        let index = Kind::ALL.iter().position(|&k| k == self).unwrap_or(0);
        Kind::ALL[(index + 1) % Kind::ALL.len()]
    }
}

/// RMS of Kellet's filter output for unit-variance white input, measured (Kellet's filter on this white source); dividing by it makes the
/// pink noise as loud as the white noise of the same setting.
const PINK_RMS: f32 = 3.034;

pub struct Generator {
    kind: Kind,
    /// The signal still being produced while it fades out after `set_kind(Silence)`.
    source: Kind,
    sample_rate: f32,
    level_db: f32,
    amplitude: f32,
    frequency: f32,
    rng: u32,
    pink: [f32; 7],
    phase: f64,
    /// 0..=1, moves towards `target_gain` by `fade_step` per sample.
    gain: f32,
    target_gain: f32,
    fade_step: f32,
    /// Burst: on for `on_samples`, off for `off_samples` (both 0 = continuous).
    on_samples: u32,
    off_samples: u32,
    burst_at: u32,
    burst_on: bool,
}

impl Generator {
    pub const fn new() -> Self {
        Self {
            kind: Kind::Silence,
            source: Kind::Silence,
            sample_rate: 48000.0,
            level_db: -20.0,
            amplitude: 0.1,
            frequency: 1000.0,
            rng: 0x2545_f491,
            pink: [0.0; 7],
            phase: 0.0,
            gain: 0.0,
            target_gain: 0.0,
            fade_step: 1.0,
            on_samples: 0,
            off_samples: 0,
            burst_at: 0,
            burst_on: true,
        }
    }

    pub fn set_rate(&mut self, sample_rate: u32) {
        self.sample_rate = sample_rate as f32;
        self.fade_step = 1.0 / (0.010 * self.sample_rate); // a 10 ms fade
        self.set_level_db(self.level_db);
    }

    pub fn set_kind(&mut self, kind: Kind) {
        self.kind = kind;
        if kind != Kind::Silence {
            self.source = kind;
        }
        self.target_gain = if kind == Kind::Silence { 0.0 } else { 1.0 };
        self.burst_at = 0;
        self.burst_on = true;
    }

    pub fn kind(&self) -> Kind {
        self.kind
    }

    /// RMS level in dB; 0 dB is a full-scale sine's RMS (so a sine at 0 dB just touches full scale; noise is clipped).
    pub fn set_level_db(&mut self, db: f32) {
        self.level_db = if db > 0.0 { 0.0 } else { db };
        // The unit of `amplitude` is RMS; a full-scale sine has RMS 0.7071, so 0 dB means 0.7071 RMS.
        self.amplitude = pow10(self.level_db / 20.0) * 0.707_106_8;
    }

    pub fn level_db(&self) -> f32 {
        self.level_db
    }

    pub fn set_frequency(&mut self, hz: f32) {
        self.frequency = hz;
    }

    pub fn frequency(&self) -> f32 {
        self.frequency
    }

    /// Bursts of `on_ms` of signal then `off_ms` of silence; both 0 turns them off.
    pub fn set_burst(&mut self, on_ms: u32, off_ms: u32) {
        self.on_samples = (on_ms as f32 * self.sample_rate / 1000.0) as u32;
        self.off_samples = (off_ms as f32 * self.sample_rate / 1000.0) as u32;
        self.burst_at = 0;
        self.burst_on = true;
    }

    fn uniform(&mut self) -> f32 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 17;
        self.rng ^= self.rng << 5;
        (self.rng >> 8) as f32 * (1.0 / 8388608.0) - 1.0 // -1..1
    }

    /// Unit-variance, approximately Gaussian.
    fn white(&mut self) -> f32 {
        let mut sum = 0.0;
        for _ in 0..12 {
            sum += self.uniform() * 0.5 + 0.5;
        }
        sum - 6.0
    }

    fn pink_sample(&mut self) -> f32 {
        let w = self.white();
        let b = &mut self.pink;
        b[0] = 0.99886 * b[0] + w * 0.0555179;
        b[1] = 0.99332 * b[1] + w * 0.0750759;
        b[2] = 0.96900 * b[2] + w * 0.1538520;
        b[3] = 0.86650 * b[3] + w * 0.3104856;
        b[4] = 0.55000 * b[4] + w * 0.5329522;
        b[5] = -0.7616 * b[5] - w * 0.0168980;
        let out = b[0] + b[1] + b[2] + b[3] + b[4] + b[5] + b[6] + w * 0.5362;
        b[6] = w * 0.115926;
        out
    }

    /// Fills `out` with mono samples in -1..1.
    pub fn fill(&mut self, out: &mut [f32]) {
        let step = 2.0 * core::f64::consts::PI * self.frequency as f64 / self.sample_rate as f64;
        for sample in out.iter_mut() {
            if self.on_samples > 0 {
                self.burst_at += 1;
                let limit = if self.burst_on { self.on_samples } else { self.off_samples.max(1) };
                if self.burst_at >= limit {
                    self.burst_at = 0;
                    self.burst_on = !self.burst_on;
                }
            }
            let wanted = if self.kind != Kind::Silence && self.burst_on { 1.0 } else { 0.0 };
            self.target_gain = wanted;
            if self.gain < self.target_gain {
                self.gain = (self.gain + self.fade_step).min(self.target_gain);
            } else if self.gain > self.target_gain {
                self.gain = (self.gain - self.fade_step).max(self.target_gain);
            }
            let raw = match self.source {
                Kind::Silence => 0.0,
                Kind::White => self.white() * self.amplitude,
                Kind::Pink => self.pink_sample() * self.amplitude / PINK_RMS,
                Kind::Sine => {
                    self.phase += step;
                    if self.phase > core::f64::consts::TAU {
                        self.phase -= core::f64::consts::TAU;
                    }
                    sin_cos64(self.phase).0 as f32 * self.amplitude * core::f32::consts::SQRT_2
                }
            };
            let value = raw * self.gain;
            *sample = if value > 1.0 { 1.0 } else if value < -1.0 { -1.0 } else { value };
        }
    }
}

/// A stereo test tone: the same sine on both channels with a phase offset and level difference on the right, so a
/// goniometer or correlation meter has something known to show without an input (offset 0 = mono, correlation +1; 90 degrees
/// = 0; 180 = opposite polarity, -1). `level_db` is the left channel's RMS dB (as `Generator`); `right_db` the right's
/// level relative to the left.
pub struct PairSine {
    sample_rate: f32,
    frequency: f32,
    left: f32,
    right: f32,
    phase_offset: f64,
    phase: f64,
}

impl PairSine {
    pub const fn new() -> Self {
        Self { sample_rate: 48000.0, frequency: 1000.0, left: 0.1, right: 0.1, phase_offset: 0.0, phase: 0.0 }
    }

    pub fn set_rate(&mut self, sample_rate: u32) {
        self.sample_rate = sample_rate as f32;
    }

    pub fn set_frequency(&mut self, hz: f32) {
        self.frequency = hz;
    }

    /// Left RMS dB re a full-scale sine's RMS, and the right channel relative to it.
    pub fn set_level_db(&mut self, level_db: f32, right_db: f32) {
        self.left = pow10(level_db / 20.0);
        self.right = pow10((level_db + right_db) / 20.0);
    }

    pub fn set_phase_degrees(&mut self, degrees: f32) {
        self.phase_offset = degrees as f64 * core::f64::consts::PI / 180.0;
    }

    /// Fills both slices (equal length; the shorter counts) with samples in -1..1.
    pub fn fill(&mut self, left: &mut [f32], right: &mut [f32]) {
        let step = 2.0 * core::f64::consts::PI * self.frequency as f64 / self.sample_rate as f64;
        for (l, r) in left.iter_mut().zip(right.iter_mut()) {
            self.phase += step;
            if self.phase > core::f64::consts::TAU {
                self.phase -= core::f64::consts::TAU;
            }
            let (a, b) = (sin_cos64(self.phase).0 as f32, sin_cos64(self.phase + self.phase_offset).0 as f32);
            *l = (a * self.left * core::f32::consts::SQRT_2).clamp(-1.0, 1.0);
            *r = (b * self.right * core::f32::consts::SQRT_2).clamp(-1.0, 1.0);
        }
    }
}

impl Default for PairSine {
    fn default() -> Self {
        Self::new()
    }
}

/// Writes `mono` into `out` as interleaved frames: `channels` channels, each sample in a `slot`-byte little-endian signed slot
/// holding `bits` significant bits (left-justified, as USB audio does) or, with `float`, a 4-byte IEEE single, the signal on every channel selected in `mask` (bit 0
/// = channel 1) and zero elsewhere. Returns the bytes written (as many whole frames as fit).
pub fn pack(mono: &[f32], out: &mut [u8], channels: usize, slot: usize, bits: usize, float: bool, mask: u32) -> usize {
    let format = audio_sample::Format::new(slot, bits, float);
    let frame = channels * slot;
    if frame == 0 || !format.valid() {
        return 0;
    }
    let frames = mono.len().min(out.len() / frame);
    for (n, &sample) in mono.iter().take(frames).enumerate() {
        for channel in 0..channels {
            let at = n * frame + channel * slot;
            format.write(if mask >> channel & 1 != 0 { sample } else { 0.0 }, &mut out[at..at + slot]);
        }
    }
    frames * frame
}

#[cfg(test)]
mod tests {
    use super::*;
    use ltas::{Analyzer, Config, Mode, Resolution};

    #[test]
    fn pair_sine_levels_and_phase() {
        let mut pair = PairSine::new();
        pair.set_level_db(-6.0, -6.0);
        pair.set_phase_degrees(180.0);
        let (mut l, mut r) = (vec![0.0; 4800], vec![0.0; 4800]);
        pair.fill(&mut l, &mut r);
        let peak = |v: &[f32]| v.iter().fold(0.0f32, |m, &x| m.max(x.abs()));
        assert!((peak(&l) - 0.5012 * core::f32::consts::SQRT_2).abs() < 0.01);
        assert!((peak(&r) / peak(&l) - 0.5012).abs() < 0.01);
        assert!(l.iter().zip(&r).all(|(a, b)| a * b <= 1e-6));
    }

    fn generate(kind: Kind, db: f32, samples: usize) -> Vec<f32> {
        let mut generator = Generator::new();
        generator.set_rate(48000);
        generator.set_level_db(db);
        generator.set_kind(kind);
        let mut out = vec![0.0; samples];
        generator.fill(&mut out);
        out
    }

    fn rms(samples: &[f32]) -> f32 {
        (samples.iter().map(|v| (*v as f64) * (*v as f64)).sum::<f64>() / samples.len() as f64).sqrt() as f32
    }

    #[test]
    fn levels_are_rms_re_full_scale_sine() {
        let sine = generate(Kind::Sine, 0.0, 48000);
        let peak = sine.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!((peak - 1.0).abs() < 0.01, "peak {peak}");
        let white = generate(Kind::White, -20.0, 48000 * 5);
        assert!((20.0 * (rms(&white[2000..]) / 0.7071068).log10() + 20.0).abs() < 0.15);
        let pink = generate(Kind::Pink, -20.0, 48000 * 20);
        assert!((20.0 * (rms(&pink[48000..]) / 0.7071068).log10() + 20.0).abs() < 0.6, "pink {}", 20.0 * (rms(&pink[48000..]) / 0.7071068).log10());
    }

    #[test]
    fn pink_is_flat_in_third_octaves_and_white_rises() {
        let mut config = Config::new(48000);
        config.resolution = Resolution::Third;
        let levels = |signal: &[f32]| {
            let mut analyzer = Box::new(Analyzer::<8192>::new());
            assert!(analyzer.configure(config));
            analyzer.push(signal);
            let mut db = vec![0.0; analyzer.outputs()];
            analyzer.levels_db(Mode::Linear, &mut db);
            (db, (0..analyzer.outputs()).map(|i| analyzer.centre_hz(i)).collect::<Vec<_>>())
        };
        let (pink, centres) = levels(&generate(Kind::Pink, -20.0, 48000 * 60));
        let inside: Vec<f32> = pink.iter().zip(&centres).filter(|(_, hz)| **hz >= 80.0 && **hz <= 16000.0).map(|(db, _)| *db).collect();
        let (low, high) = inside.iter().fold((f32::MAX, f32::MIN), |(l, h), v| (l.min(*v), h.max(*v)));
        assert!(high - low < 1.5, "pink third-octave spread {low}..{high}");
        let (white, _) = levels(&generate(Kind::White, -20.0, 48000 * 60));
        let slope = white[white.len() - 3] - white[10];
        assert!((slope - 10.0 * (centres[centres.len() - 3] / centres[10]).log10()).abs() < 1.5, "white slope {slope}");
    }

    #[test]
    fn sine_hits_its_frequency_and_fades_in_and_out() {
        let mut generator = Generator::new();
        generator.set_rate(48000);
        generator.set_frequency(1000.0);
        generator.set_level_db(-6.0);
        generator.set_kind(Kind::Sine);
        let mut out = vec![0.0; 48000];
        generator.fill(&mut out);
        assert!(out[0].abs() < 0.01 && out[5].abs() < 0.05, "fade in: {} {}", out[0], out[5]);
        assert!(out[100..].iter().fold(0.0f32, |m, v| m.max(v.abs())) > 0.49);
        generator.set_kind(Kind::Silence);
        let mut tail = vec![0.0; 1000];
        generator.fill(&mut tail);
        assert!(tail[999] == 0.0 && tail[..24].iter().any(|v| v.abs() > 0.2), "fade out");
        let largest_step = tail.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0f32, f32::max);
        assert!(largest_step < 0.08, "no click: {largest_step}");
    }

    #[test]
    fn bursts_alternate() {
        let mut generator = Generator::new();
        generator.set_rate(48000);
        generator.set_level_db(-10.0);
        generator.set_burst(100, 100);
        generator.set_kind(Kind::White);
        let mut out = vec![0.0; 48000 / 5 * 2];
        generator.fill(&mut out);
        assert!(rms(&out[500..4000]) > 0.1, "on");
        assert!(rms(&out[5500..9000]) < 0.001, "off");
        assert!(rms(&out[10100..13000]) > 0.1, "on again");
    }

    #[test]
    fn pack_formats() {
        let mono = [0.5f32, -0.5];
        let mut out = [0xffu8; 16];
        assert_eq!(pack(&mono, &mut out, 2, 2, 16, false, 0b01), 8);
        assert_eq!(&out[..8], &[0xff, 0x3f, 0, 0, 0x01, 0xc0, 0, 0]);
        let mut out = [0u8; 12];
        assert_eq!(pack(&mono, &mut out, 1, 3, 24, false, 1), 6);
        assert_eq!(&out[..3], &[0xff, 0xff, 0x3f]);
        assert_eq!(pack(&mono, &mut out, 1, 3, 32, false, 1), 0, "bits do not fit the slot");
        assert_eq!(pack(&mono, &mut out[..2], 1, 3, 24, false, 1), 0);
        let mut out = [0u8; 8];
        assert_eq!(pack(&[0.5, 2.0], &mut out, 1, 4, 32, true, 1), 8);
        assert_eq!(out, [0.5f32.to_le_bytes(), 2.0f32.to_le_bytes()].concat()[..], "float is written as it is");
    }
}
