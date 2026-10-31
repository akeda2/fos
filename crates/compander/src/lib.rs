//! A dbx-style compander: a wide-band 2:1 compressor on record and the exactly complementary 1:2 expander on playback, with
//! the true-RMS, log-domain level detector of Blackmer's patent (US 3,789,143, expired). One engine does both directions and
//! both families: `Params::type_i()` for flat professional media, `Params::type_ii()` for cassettes and noisy media (a
//! band-limited, HF-weighted detector). Everything is public data so a tape can offer other curves.
//!
//! Signal flow (per channel, one sample at a time, `no_std`, allocation-free, hard-float, `dsp::math` for log/pow):
//!
//! ```text
//! encode:  x -> pre-emphasis -> p --+--> gain(-(1-1/r)(L-pivot) dB) -> y        L = detector(p)
//! decode:  y --+--> gain(+(r-1)(L-pivot) dB) -> z -> de-emphasis -> x           L = detector(y)
//! ```
//!
//! Both gains are computed from the level of the signal they act on, so decode(encode(x)) is the identity for a steady
//! signal whatever the pivot and ratio are; on fast changes the two detectors see slightly different signals, which is the
//! same tracking error the analogue system has. `L` is RMS in dB relative to full scale (a sine of amplitude 1.0 is
//! -3 dBFS). The detector runs: side-chain filters -> quadrature (ripple-free) power -> short smoothing -> dB -> a slew
//! limit that follows rises quickly and falls at `fall_db_per_s` (the patent's 60 dB/s).
//!
//! The corner frequencies come from published sources (`docs/compander.md`): the Peissig et al. DAFx'04 paper for the
//! pre/de-emphasis and detector curves, the dbx 140X manual for what differs between the types. They are provisional:
//! `Params` is where to tune them against real dbx material.
#![cfg_attr(not(test), no_std)]

pub mod analytic;
pub mod tapeeq;

use analytic::Analytic;
use dsp::math;

/// What a `Compander` does to the samples that go through it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Samples pass untouched.
    Bypass,
    /// Record side: pre-emphasis then 2:1 compression.
    Encode,
    /// Playback side: 1:2 expansion then de-emphasis.
    Decode,
}

/// The tuning of one compander family.
#[derive(Clone, Copy, Debug)]
pub struct Params {
    /// Compression ratio on encode, expansion ratio on decode (dbx: 2).
    pub ratio: f32,
    /// RMS level, in dB relative to full scale, where the gain is unity. Encoded peaks end up about 6 dB lower than the
    /// source with 0 dBFS in, which is what the dbx manual says of its encoder.
    pub pivot_db: f32,
    /// Quietest level the detector reports; below it the gain stops growing (dbx handles about 115 dB).
    pub floor_db: f32,
    /// Audio-path pre-emphasis: (zero Hz, pole Hz). A boost of pole/zero above the pole; the decoder applies the inverse.
    pub emphasis: Option<(f32, f32)>,
    /// Extra emphasis in the detector's side chain only: (zero Hz, pole Hz).
    pub detector_emphasis: Option<(f32, f32)>,
    /// The detector ignores what is below this (Hz).
    pub detector_high_pass: Option<f32>,
    /// ... and above this (Hz).
    pub detector_low_pass: Option<f32>,
    /// Smoothing of the power estimate, milliseconds (kept short: the quadrature network already removes the ripple).
    pub smooth_ms: f32,
    /// How fast the level estimate may fall, dB per second.
    pub fall_db_per_s: f32,
    /// How fast it may rise, dB per second.
    pub rise_db_per_s: f32,
}

impl Params {
    /// dbx Type I: for media with flat response and good headroom (reel-to-reel). The detector hears the whole spectrum.
    pub fn type_i() -> Params {
        Params {
            ratio: 2.0,
            pivot_db: -12.0,
            floor_db: -120.0,
            emphasis: Some((370.0, 1590.0)),
            detector_emphasis: None,
            detector_high_pass: None,
            detector_low_pass: None,
            smooth_ms: 0.3,
            fall_db_per_s: 60.0,
            rise_db_per_s: 1_000_000.0,
        }
    }

    /// dbx Type II: for cassettes and other noisy, band-limited media. The same 2:1 companding, but the detector responds
    /// only between about 60 Hz and 10 kHz and weighs 1-8 kHz more, so response errors at the edges of the band and
    /// treble peaks do not make it mistrack. Not compatible with Type I.
    pub fn type_ii() -> Params {
        Params { detector_emphasis: Some((440.0, 4800.0)), detector_high_pass: Some(60.0), detector_low_pass: Some(10_000.0), ..Params::type_i() }
    }
}

fn tan(x: f32) -> f32 {
    let (s, c) = math::sin_cos64(x as f64);
    (s / c) as f32
}

/// A first-order section with bilinear coefficients, corners pre-warped to land where they were asked for.
#[derive(Clone, Copy, Default)]
struct First {
    b0: f32,
    b1: f32,
    a1: f32,
    x1: f32,
    y1: f32,
}

impl First {
    const IDENTITY: First = First { b0: 1.0, b1: 0.0, a1: 0.0, x1: 0.0, y1: 0.0 };

    /// (1 + s/wz) / (1 + s/wp): unity at DC, a boost of `pole_hz / zero_hz` at the top.
    fn shelf(zero_hz: f32, pole_hz: f32, rate: f32) -> First {
        let k = |hz: f32| 1.0 / tan(core::f32::consts::PI * hz / rate);
        let (kz, kp) = (k(zero_hz), k(pole_hz));
        First { b0: (1.0 + kz) / (1.0 + kp), b1: (1.0 - kz) / (1.0 + kp), a1: (1.0 - kp) / (1.0 + kp), x1: 0.0, y1: 0.0 }
    }

    /// The exact inverse of `shelf` with the same corners.
    fn unshelf(zero_hz: f32, pole_hz: f32, rate: f32) -> First {
        First::shelf(pole_hz, zero_hz, rate)
    }

    fn low_pass(hz: f32, rate: f32) -> First {
        let k = 1.0 / tan(core::f32::consts::PI * hz / rate);
        First { b0: 1.0 / (1.0 + k), b1: 1.0 / (1.0 + k), a1: (1.0 - k) / (1.0 + k), x1: 0.0, y1: 0.0 }
    }

    fn high_pass(hz: f32, rate: f32) -> First {
        let k = 1.0 / tan(core::f32::consts::PI * hz / rate);
        First { b0: k / (1.0 + k), b1: -k / (1.0 + k), a1: (1.0 - k) / (1.0 + k), x1: 0.0, y1: 0.0 }
    }

    fn run(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.b1 * self.x1 - self.a1 * self.y1;
        self.x1 = x;
        self.y1 = y;
        y
    }
}

/// The level detector: side chain, quadrature power, smoothing, dB, slew limit.
#[derive(Clone, Copy)]
struct Detector {
    high_pass: First,
    low_pass: First,
    emphasis: First,
    analytic: Analytic,
    smooth: f32,
    power: f32,
    level: f32,
    floor: f32,
    rise: f32,
    fall: f32,
}

impl Detector {
    fn new(params: &Params, rate: f32) -> Detector {
        Detector {
            high_pass: params.detector_high_pass.map_or(First::IDENTITY, |hz| First::high_pass(hz, rate)),
            low_pass: params.detector_low_pass.map_or(First::IDENTITY, |hz| First::low_pass(hz.min(rate * 0.45), rate)),
            emphasis: params.detector_emphasis.map_or(First::IDENTITY, |(zero, pole)| First::shelf(zero, pole, rate)),
            analytic: Analytic::default(),
            smooth: 1.0 - math::exp64(-1.0 / (params.smooth_ms.max(0.01) as f64 * 0.001 * rate as f64)) as f32,
            power: 0.0,
            level: params.floor_db,
            floor: params.floor_db,
            rise: params.rise_db_per_s / rate,
            fall: params.fall_db_per_s / rate,
        }
    }

    /// RMS level of the signal so far, dBFS.
    fn run(&mut self, x: f32) -> f32 {
        let side = self.emphasis.run(self.low_pass.run(self.high_pass.run(x)));
        self.power += self.smooth * (self.analytic.power(side) - self.power);
        // The quadrature power is amplitude squared; a sine's mean square is half of it.
        let db = (10.0 * math::log10(self.power.max(1e-20)) - 3.0103).max(self.floor);
        self.level = if db >= self.level { db.min(self.level + self.rise) } else { db.max(self.level - self.fall) };
        self.level
    }
}

/// One channel of compander.
#[derive(Clone, Copy)]
pub struct Compander {
    params: Params,
    mode: Mode,
    pre: First,
    de: First,
    detector: Detector,
    gain_db: f32,
}

impl Compander {
    pub fn new(params: Params, sample_rate: f32, mode: Mode) -> Compander {
        let (pre, de) = match params.emphasis {
            Some((zero, pole)) => (First::shelf(zero, pole, sample_rate), First::unshelf(zero, pole, sample_rate)),
            None => (First::IDENTITY, First::IDENTITY),
        };
        Compander { params, mode, pre, de, detector: Detector::new(&params, sample_rate), gain_db: 0.0 }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
    }

    /// The gain the last sample went through, dB (negative while compressing loud material, positive while expanding it).
    pub fn gain_db(&self) -> f32 {
        self.gain_db
    }

    /// The detector's level after the last sample, dBFS RMS.
    pub fn level_db(&self) -> f32 {
        self.detector.level
    }

    pub fn process(&mut self, x: f32) -> f32 {
        let Params { ratio, pivot_db, .. } = self.params;
        match self.mode {
            Mode::Bypass => x,
            Mode::Encode => {
                let p = self.pre.run(x);
                let level = self.detector.run(p);
                self.gain_db = -(1.0 - 1.0 / ratio) * (level - pivot_db);
                p * math::pow10(self.gain_db / 20.0)
            }
            Mode::Decode => {
                let level = self.detector.run(x);
                self.gain_db = (ratio - 1.0) * (level - pivot_db);
                self.de.run(x * math::pow10(self.gain_db / 20.0))
            }
        }
    }

    pub fn process_block(&mut self, samples: &mut [f32]) {
        for sample in samples {
            *sample = self.process(*sample);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f32 = 48_000.0;

    fn sine(hz: f64, db: f64, n: usize) -> f32 {
        (10f64.powf(db / 20.0) * 2f64.sqrt() * (2.0 * core::f64::consts::PI * hz * n as f64 / RATE as f64).sin()) as f32
    }

    /// Mean-square level in dB of `seconds` of the tone through `chain`, ignoring the first half.
    fn through(chain: &mut [Compander], hz: f64, db: f64, seconds: f32) -> f64 {
        let n = (RATE * seconds) as usize;
        let mut sum = 0.0;
        let mut count = 0usize;
        for i in 0..n {
            let mut v = sine(hz, db, i);
            for stage in chain.iter_mut() {
                v = stage.process(v);
            }
            if i >= n / 2 {
                sum += (v as f64) * (v as f64);
                count += 1;
            }
        }
        10.0 * (sum / count as f64).log10()
    }

    fn pair(params: Params) -> [Compander; 2] {
        [Compander::new(params, RATE, Mode::Encode), Compander::new(params, RATE, Mode::Decode)]
    }

    #[test]
    fn bypass_is_transparent() {
        let mut bypass = Compander::new(Params::type_i(), RATE, Mode::Bypass);
        for n in 0..1000 {
            let x = sine(440.0, -10.0, n);
            assert_eq!(bypass.process(x), x);
        }
    }

    #[test]
    fn encode_then_decode_is_transparent_for_steady_tones() {
        for (name, params) in [("I", Params::type_i()), ("II", Params::type_ii())] {
            for hz in [60.0, 200.0, 1000.0, 4000.0, 10_000.0] {
                for db in [-80.0, -50.0, -20.0, -6.0] {
                    let mut chain = pair(params);
                    let out = through(&mut chain, hz, db, 1.5);
                    let error = out - db;
                    println!("type {name} {hz:>6} Hz {db:>5} dBFS: {error:+.3} dB");
                    assert!(error.abs() < 0.3, "type {name} {hz} Hz {db} dBFS: {error:+.3} dB");
                }
            }
        }
    }

    #[test]
    fn encoder_compresses_two_to_one() {
        let mut quiet = [Compander::new(Params::type_i(), RATE, Mode::Encode)];
        let mut loud = [Compander::new(Params::type_i(), RATE, Mode::Encode)];
        let a = through(&mut quiet, 400.0, -60.0, 1.5);
        let b = through(&mut loud, 400.0, -20.0, 1.5);
        println!("40 dB more in gives {:.2} dB more out", b - a);
        assert!((b - a - 20.0).abs() < 0.5);
    }

    #[test]
    fn the_level_falls_at_the_patents_rate() {
        let mut detector = Compander::new(Params::type_i(), RATE, Mode::Decode);
        for n in 0..(RATE as usize) {
            detector.process(sine(1000.0, -10.0, n));
        }
        let start = detector.level_db();
        for _ in 0..(RATE as usize / 2) {
            detector.process(0.0);
        }
        let rate = (start - detector.level_db()) / 0.5;
        println!("fell {rate:.1} dB/s from {start:.1} dB");
        assert!((rate - 60.0).abs() < 3.0);
    }

    /// Distortion of the gain control on a low-frequency tone: the part of the output that is not the fundamental.
    fn low_frequency_distortion(params: Params, hz: f64, db: f64) -> f64 {
        let mut encoder = Compander::new(params, RATE, Mode::Encode);
        let n = (RATE as f64 * 2.0) as usize;
        let (mut s, mut c, mut total) = (0.0f64, 0.0f64, 0.0f64);
        let mut count = 0usize;
        // One second is a whole number of cycles for the whole-hertz tones used here.
        let window = RATE as usize;
        for i in 0..n {
            let y = encoder.process(sine(hz, db, i)) as f64;
            if i >= n - window {
                let phase = 2.0 * core::f64::consts::PI * hz * i as f64 / RATE as f64;
                s += y * phase.sin();
                c += y * phase.cos();
                total += y * y;
                count += 1;
            }
        }
        let fundamental = 2.0 * (s * s + c * c).sqrt() / count as f64;
        let rms = (total / count as f64).sqrt();
        let residual = (rms * rms - fundamental * fundamental / 2.0).max(0.0).sqrt();
        residual / (fundamental / 2f64.sqrt())
    }

    #[test]
    fn the_gain_does_not_modulate_low_tones() {
        for hz in [30.0, 50.0, 100.0] {
            let thd = low_frequency_distortion(Params::type_i(), hz, -20.0);
            println!("{hz} Hz: {:.3}% distortion", thd * 100.0);
            assert!(thd < 0.01, "{hz} Hz: {:.3}%", thd * 100.0);
        }
    }

    #[test]
    fn a_burst_survives_the_round_trip() {
        for params in [Params::type_i(), Params::type_ii()] {
            let mut chain = pair(params);
            let (mut peak_in, mut peak_out, mut worst) = (0.0f64, 0.0f64, 0.0f64);
            for i in 0..(RATE as usize / 2) {
                let on = (RATE as usize / 4..RATE as usize / 4 + 960).contains(&i);
                let x = if on { sine(1000.0, -10.0, i) } else { sine(1000.0, -90.0, i) };
                let mut v = x;
                for stage in chain.iter_mut() {
                    v = stage.process(v);
                }
                peak_in = peak_in.max(x.abs() as f64);
                peak_out = peak_out.max(v.abs() as f64);
                worst = worst.max((v - x).abs() as f64);
            }
            println!("burst: peak in {peak_in:.3}, out {peak_out:.3}, worst sample error {worst:.3}");
            // A burst out of near-silence overshoots for a few samples while the detector catches up (as the analogue
            // units do); the tape limits it. Bound it.
            assert!(peak_out / peak_in < 10.0);
        }
    }
}
