//! A pair of all-pass filters whose outputs stay 90 degrees apart over the audio band: the "j" every phase matrix needs
//! (SQ and QS decoding, Dolby Surround's surround channel). Both outputs have the same magnitude as the input.
//!
//! Each path is four first-order all-passes in z^-2, `y[n] = k (x[n] + y[n-2]) - x[n-2]`, with the coefficients of Olli
//! Niemitalo's 8-section polyphase Hilbert pair (yehar.com, "Hilbert transform" IIR); the in-phase path also gets one
//! sample of delay. Measured with the test below at 48 kHz: the two outputs differ by 90 degrees within +-1.5 degrees from
//! 60 Hz to 20 kHz, and the error is about the same relative to the sample rate (so at 96 kHz the good range starts near
//! 120 Hz; the bass of a matrix-encoded record is mono anyway). The state is `f64`: the first sections have poles very close to
//! the unit circle, and `f32` state would add noise at low frequencies.

const PATH_A: [f64; 4] = [0.6923878, 0.9360654322959, 0.9882295226860, 0.9987488452737];
const PATH_B: [f64; 4] = [0.4021921162426, 0.8561710882420, 0.9722909545651, 0.9952884791278];

#[derive(Clone, Copy)]
struct Section {
    k: f64,
    x1: f64,
    x2: f64,
    y1: f64,
    y2: f64,
}

impl Section {
    const fn new(a: f64) -> Self {
        Self { k: a * a, x1: 0.0, x2: 0.0, y1: 0.0, y2: 0.0 }
    }

    fn step(&mut self, x: f64) -> f64 {
        let y = self.k * (x + self.y2) - self.x2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }
}

/// One signal in, two out: `(same, lead)`. `lead` is `same` advanced by 90 degrees (as `j * same` for a signal
/// `e^(j w t)`), and both are delayed alike (a few milliseconds of group delay at the bass, nothing above 1 kHz).
#[derive(Clone, Copy)]
pub struct Quadrature {
    a: [Section; 4],
    b: [Section; 4],
    hold: f64,
}

impl Quadrature {
    pub const fn new() -> Self {
        Self {
            a: [Section::new(PATH_A[0]), Section::new(PATH_A[1]), Section::new(PATH_A[2]), Section::new(PATH_A[3])],
            b: [Section::new(PATH_B[0]), Section::new(PATH_B[1]), Section::new(PATH_B[2]), Section::new(PATH_B[3])],
            hold: 0.0,
        }
    }

    pub fn process(&mut self, x: f32) -> (f32, f32) {
        let mut a = x as f64;
        let mut b = a;
        for section in &mut self.a {
            a = section.step(a);
        }
        for section in &mut self.b {
            b = section.step(b);
        }
        let same = self.hold;
        self.hold = a;
        (same as f32, b as f32)
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }
}

impl Default for Quadrature {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Complex amplitude of `x` at `hz`, from a whole number of cycles of the last second.
    fn amplitude(x: &[f32], hz: f32, rate: f32) -> (f64, f64) {
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (n, &v) in x.iter().enumerate() {
            let phase = 2.0 * core::f64::consts::PI * hz as f64 * n as f64 / rate as f64;
            re += v as f64 * phase.cos();
            im -= v as f64 * phase.sin();
        }
        (re, im)
    }

    #[test]
    fn outputs_are_a_quarter_turn_apart_with_equal_level() {
        let rate = 48_000.0;
        for hz in [60.0, 100.0, 250.0, 1000.0, 4000.0, 10_000.0, 16_000.0, 20_000.0] {
            let mut q = Quadrature::new();
            let (mut same, mut lead) = (Vec::new(), Vec::new());
            for n in 0..96_000 {
                let (s, l) = q.process((2.0 * core::f32::consts::PI * hz * n as f32 / rate).sin());
                if n >= 48_000 {
                    same.push(s);
                    lead.push(l);
                }
            }
            // 48000 samples hold a whole number of cycles of every frequency above
            let (sr, si) = amplitude(&same, hz, rate);
            let (lr, li) = amplitude(&lead, hz, rate);
            let turn = (li.atan2(lr) - si.atan2(sr)).to_degrees().rem_euclid(360.0);
            let level = (lr.hypot(li) / sr.hypot(si)).log10() * 20.0;
            assert!((turn - 90.0).abs() < 1.5, "{hz} Hz: {turn} degrees");
            assert!(level.abs() < 0.05, "{hz} Hz: {level} dB");
        }
    }
}
