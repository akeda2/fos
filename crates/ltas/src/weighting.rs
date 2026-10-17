//! Frequency weightings (IEC 61672-1 analytic forms) as gains in dB.

use dsp::math::log10;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Weighting {
    /// Flat.
    Z,
    A,
    C,
}

impl Weighting {
    pub const ALL: [Weighting; 3] = [Weighting::Z, Weighting::A, Weighting::C];

    pub fn name(self) -> &'static str {
        match self {
            Weighting::Z => "z",
            Weighting::A => "a",
            Weighting::C => "c",
        }
    }

    pub fn from_name(name: &str) -> Option<Weighting> {
        Weighting::ALL.into_iter().find(|w| w.name() == name)
    }

    pub fn next(self) -> Weighting {
        let index = Weighting::ALL.iter().position(|&w| w == self).unwrap_or(0);
        Weighting::ALL[(index + 1) % Weighting::ALL.len()]
    }

    /// Gain in dB at `hz`.
    pub fn db(self, hz: f32) -> f32 {
        let f2 = (hz as f64) * (hz as f64);
        let (c1, c2, c3, c4) = (20.598997f64 * 20.598997, 107.65265f64 * 107.65265, 737.86223f64 * 737.86223, 12194.217f64 * 12194.217);
        match self {
            Weighting::Z => 0.0,
            Weighting::A => {
                let r = c4 * f2 * f2 / ((f2 + c1) * dsp::math::sqrt64((f2 + c2) * (f2 + c3)) * (f2 + c4));
                20.0 * log10(r as f32) + 2.0
            }
            Weighting::C => {
                let r = c4 * f2 / ((f2 + c1) * (f2 + c4));
                20.0 * log10(r as f32) + 0.062
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_standard_table() {
        // IEC 61672-1 table values (dB), tolerance 0.1 (the table is rounded to 0.1).
        for (hz, a, c) in [(63.0, -26.2, -0.8), (125.0, -16.1, -0.2), (1000.0, 0.0, 0.0), (4000.0, 1.0, -0.8), (8000.0, -1.1, -3.0), (16000.0, -6.7, -8.6)] {
            assert!((Weighting::A.db(hz) - a).abs() < 0.1, "A {hz}: {}", Weighting::A.db(hz));
            assert!((Weighting::C.db(hz) - c).abs() < 0.1, "C {hz}: {}", Weighting::C.db(hz));
        }
        assert_eq!(Weighting::Z.db(123.0), 0.0);
    }
}
