//! Second-order sections in f64 (the bass poles sit at 0.999 of the unit circle at 48 kHz, where an f32 state would add
//! audible low-frequency noise) and their frequency response, for the picture on the touch screen and for the tests.

use dsp::math::{exp64, pow10_64, sin_cos64, sqrt64};

#[derive(Clone, Copy)]
pub struct Section {
    pub b: [f64; 3],
    pub a: [f64; 2],
}

impl Section {
    pub const PASS: Section = Section { b: [1.0, 0.0, 0.0], a: [0.0, 0.0] };

    /// `(1 + s*zero) / (1 + s*pole)`, times in seconds: the pole where the analogue one is (`exp(-1/(rate*pole))`, as an
    /// impulse-invariant design has it) and the zero placed so the gain is 1 at DC and equals the analogue gain at Nyquist. For a
    /// bass turnover and shelf that is exact across the band; the plain bilinear transform bends the top octaves.
    pub fn lead_lag(zero: f64, pole: f64, rate: f64) -> Section {
        let nyquist = core::f64::consts::PI * rate;
        let gain = sqrt64((1.0 + nyquist * nyquist * zero * zero) / (1.0 + nyquist * nyquist * pole * pole));
        Self::matched(exp64(-1.0 / (rate * pole)), gain * (1.0 + exp64(-1.0 / (rate * pole))))
    }

    /// `1 / (1 + s*tau)`: the pole as above, the zero placed so the gain equals the analogue gain at `match_hz`. Nyquist
    /// matching leaves 0.9 dB of error near 10 kHz at 48 kHz; matching at 0.35 of the sample rate keeps it under 0.15 dB to
    /// 16 kHz (the treble cut of a record is defined at 10 kHz, so that is where it must be right).
    pub fn low_pass(tau: f64, rate: f64, match_hz: f64) -> Section {
        let p = exp64(-1.0 / (rate * tau));
        let w = 2.0 * core::f64::consts::PI * match_hz;
        let wt = w / rate;
        let (_, c) = sin_cos64(wt);
        let target = (1.0 + p * p - 2.0 * p * c) / (1.0 + w * w * tau * tau);
        // b0 + b1 = 1 - p and |b0 + b1 e^-jw|^2 = target: a quadratic in b0.
        let s = 1.0 - p;
        let (qa, qb, qc) = (2.0 - 2.0 * c, -2.0 * s + 2.0 * s * c, s * s - target);
        let b0 = (-qb + sqrt64((qb * qb - 4.0 * qa * qc).max(0.0))) / (2.0 * qa);
        Section { b: [b0, s - b0, 0.0], a: [-p, 0.0] }
    }

    /// `(b0 + b1/z) / (1 - p/z)` with unity gain at DC; `b0 - b1` is the gain at Nyquist times `1 + p`.
    fn matched(p: f64, b0_minus_b1: f64) -> Section {
        let s = 1.0 - p;
        Section { b: [(s + b0_minus_b1) / 2.0, (s - b0_minus_b1) / 2.0, 0.0], a: [-p, 0.0] }
    }

    /// `s*tau / (1 + s*tau)`: the first-order high-pass of the IEC amendment (tau 7950 us, 20 Hz).
    pub fn high_pass(tau: f64, rate: f64) -> Section {
        let k = 2.0 * rate;
        let a0 = 1.0 + k * tau;
        Section { b: [k * tau / a0, -k * tau / a0, 0.0], a: [(1.0 - k * tau) / a0, 0.0] }
    }

    /// Second-order Butterworth high-pass at `hz` (12 dB per octave), corner pre-warped to be exact.
    pub fn butterworth_high_pass(hz: f64, rate: f64) -> Section {
        let (s, c) = sin_cos64(core::f64::consts::PI * hz / rate);
        let k = s / c;
        let root2 = core::f64::consts::SQRT_2;
        let norm = 1.0 / (1.0 + root2 * k + k * k);
        Section { b: [norm, -2.0 * norm, norm], a: [2.0 * (k * k - 1.0) * norm, (1.0 - root2 * k + k * k) * norm] }
    }

    /// A peaking filter (RBJ cookbook): `gain_db` at `hz`, 0 dB far away, `q` sets the width (bandwidth = hz / q).
    pub fn peaking(hz: f64, q: f64, gain_db: f64, rate: f64) -> Section {
        let a = pow10_64(gain_db / 40.0);
        let (s, c) = sin_cos64(2.0 * core::f64::consts::PI * hz / rate);
        let alpha = s / (2.0 * q);
        let a0 = 1.0 + alpha / a;
        Section { b: [(1.0 + alpha * a) / a0, -2.0 * c / a0, (1.0 - alpha * a) / a0], a: [-2.0 * c / a0, (1.0 - alpha / a) / a0] }
    }

    /// The complex response at `hz`.
    pub fn response(&self, hz: f64, rate: f64) -> (f64, f64) {
        let (s1, c1) = sin_cos64(2.0 * core::f64::consts::PI * hz / rate);
        let (s2, c2) = sin_cos64(4.0 * core::f64::consts::PI * hz / rate);
        let num = (self.b[0] + self.b[1] * c1 + self.b[2] * c2, -(self.b[1] * s1 + self.b[2] * s2));
        let den = (1.0 + self.a[0] * c1 + self.a[1] * c2, -(self.a[0] * s1 + self.a[1] * s2));
        let d = den.0 * den.0 + den.1 * den.1;
        ((num.0 * den.0 + num.1 * den.1) / d, (num.1 * den.0 - num.0 * den.1) / d)
    }
}

/// A section with its two state variables (transposed direct form II).
#[derive(Clone, Copy)]
pub struct Biquad {
    pub section: Section,
    z1: f64,
    z2: f64,
}

impl Biquad {
    pub const fn new(section: Section) -> Biquad {
        Biquad { section, z1: 0.0, z2: 0.0 }
    }

    pub fn reset(&mut self) {
        self.z1 = 0.0;
        self.z2 = 0.0;
    }

    #[inline]
    pub fn process(&mut self, x: f64) -> f64 {
        let s = &self.section;
        let y = s.b[0] * x + self.z1;
        self.z1 = s.b[1] * x - s.a[0] * y + self.z2;
        self.z2 = s.b[2] * x - s.a[1] * y;
        y
    }
}
