//! A vowel filter that doubles as a wah-wah: a low-pass and two formant band-passes whose frequencies follow one number,
//! the openness (0 a dark "u", 1 the top of the range, about an "a" by default). The settings shared by every instance
//! (`WahShape`: where it rests, the pedal, Q and range) are worked out once a block; each instance (`Wah`, a voice of the
//! electric piano or the string machine's sum) keeps only its filter state and adds its own lift to the openness, so a
//! pedal moves notes that are already ringing.
//!
//! The formants start from the vowel charts (men's voices): "u" about 300 and 870 Hz, "a" 750 and 1250. The first rises to
//! `top` (Range: 600 Hz to 2.4 kHz) as `open (0.4 + 0.6 open)`, slowly at first, as a pedal's sweep sounds even; the second
//! goes from 2.9 to 1.6 times the first. The low-pass above them follows (at least 2.2 times the second), so an open vowel
//! is brighter too (all of them below 9 kHz and 0.4 times the rate). Q sharpens both peaks (1.5 to 12) and, past a third of its travel, turns down the unfiltered sound
//! beside them, as a real wah keeps little of it: a soft vowel at low Q, a squeal at high Q and range.

/// A state-variable filter (trapezoidal integrators, Zavalishin's TPT form): stable while `g` and `k` move every sample.
#[derive(Clone, Copy)]
pub struct Svf {
    ic1: f32,
    ic2: f32,
}

impl Svf {
    pub const ZERO: Svf = Svf { ic1: 0.0, ic2: 0.0 };

    /// One sample through the filter, `g` the tan of pi times the cutoff over the rate, `k` the damping (1/Q): (band, low).
    pub fn tick(&mut self, x: f32, g: f32, k: f32) -> (f32, f32) {
        let a1 = 1.0 / (1.0 + g * (g + k));
        let (a2, a3) = (g * a1, g * g * a1);
        let v3 = x - self.ic2;
        let v1 = a1 * self.ic1 + a2 * v3;
        let v2 = self.ic2 + a2 * self.ic1 + a3 * v3;
        self.ic1 = 2.0 * v1 - self.ic1;
        self.ic2 = 2.0 * v2 - self.ic2;
        (v1, v2)
    }
}

/// `Svf`'s `g` for a cutoff of `hz` without a tangent: three terms of its series, close enough below a fifth of the rate
/// and still rising (a filter that moves every sample cannot afford `tan`).
pub fn fast_g(hz: f32, rate: f32) -> f32 {
    let w = core::f32::consts::PI * hz / rate;
    let w2 = w * w;
    w * (1.0 + w2 * (1.0 / 3.0 + w2 * (2.0 / 15.0)))
}

/// The settings every `Wah` of an instrument shares, for one block.
#[derive(Clone, Copy)]
pub struct WahShape {
    /// Where the vowel rests before an instance adds its lift: the vowel setting, opened further by the pedal.
    base: f32,
    /// The first formant at full openness, Hz.
    top: f32,
    /// The damping (1/Q) of the two formants.
    k1: f32,
    k2: f32,
    /// How much of the unfiltered sound is left beside the peaks.
    dry: f32,
    /// How far the peaks stand out.
    gain: f32,
    rate: f32,
}

impl WahShape {
    /// `vowel` where it rests (0 "u" to 1 the top), `pedal` how far a pedal opens it beyond that (1: past the top),
    /// `q` and `range` the sliders, all 0..=1.
    pub fn new(vowel: f32, pedal: f32, q: f32, range: f32, rate: f32) -> Self {
        let (vowel, pedal, q, range) = (vowel.clamp(0.0, 1.0), pedal.clamp(0.0, 1.0), q.clamp(0.0, 1.0), range.clamp(0.0, 1.0));
        let k1 = 1.0 / (1.5 * dsp::math::pow10(0.90309 * q));
        WahShape {
            base: vowel + (1.2 - vowel) * pedal,
            top: 600.0 * dsp::math::pow10(0.60206 * range),
            k1,
            k2: 0.8 * k1,
            dry: 1.0 - 0.9 * ((q - 0.3) / 0.7).max(0.0),
            gain: 0.5 + 1.25 * q,
            rate,
        }
    }

    /// Where the vowel rests now, before any lift.
    pub fn base(&self) -> f32 {
        self.base
    }
}

/// One instance's filter state.
#[derive(Clone, Copy)]
pub struct Wah {
    low: Svf,
    f1: Svf,
    f2: Svf,
}

impl Wah {
    pub const ZERO: Wah = Wah { low: Svf::ZERO, f1: Svf::ZERO, f2: Svf::ZERO };

    /// One sample: `lift` is added to the shape's openness (a hit, an envelope), `emphasis` scales the peaks (1 by default).
    pub fn tick(&mut self, x: f32, lift: f32, emphasis: f32, shape: &WahShape) -> f32 {
        let open = (shape.base + lift).clamp(0.0, 1.2);
        let f1 = 300.0 + (shape.top - 300.0) * open * (0.4 + 0.6 * open);
        let ceiling = (0.4 * shape.rate).min(9000.0);
        let f1 = f1.min(ceiling);
        let f2 = (f1 * (2.9 - 1.3 * open.min(1.0))).min(ceiling);
        let cutoff = (1400.0 + 3000.0 * open).max(2.2 * f2).min(ceiling);
        let low = self.low.tick(x, fast_g(cutoff, shape.rate), 1.2).1;
        let band1 = self.f1.tick(low, fast_g(f1, shape.rate), shape.k1).0 * shape.k1;
        let band2 = self.f2.tick(low, fast_g(f2, shape.rate), shape.k2).0 * shape.k2;
        shape.dry * low + shape.gain * emphasis * (band1 + 0.6 * band2)
    }
}
