//! Analysis windows and the constants that turn FFT bins into calibrated levels.
//!
//! For a window w of N points: `coherent_gain` = sum(w)/N (a sine of amplitude A peaks at A*gain*N/2 in the FFT) and
//! `power_sum` = sum(w^2) (noise power per bin is sigma^2 * power_sum). A noise-like measurement (LTAS of pink noise, music)
//! divides power by `power_sum * N`-style constants; a tone divides by the coherent gain squared. The caller picks.

use crate::math::cos;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Rect,
    Hann,
    Hamming,
    BlackmanHarris,
    FlatTop,
}

impl Kind {
    pub const ALL: [Kind; 5] = [Kind::Rect, Kind::Hann, Kind::Hamming, Kind::BlackmanHarris, Kind::FlatTop];

    pub fn name(self) -> &'static str {
        match self {
            Kind::Rect => "rect",
            Kind::Hann => "hann",
            Kind::Hamming => "hamming",
            Kind::BlackmanHarris => "blackman-harris",
            Kind::FlatTop => "flat-top",
        }
    }

    pub fn from_name(name: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|kind| kind.name() == name)
    }

    pub fn next(self) -> Kind {
        let index = Kind::ALL.iter().position(|&kind| kind == self).unwrap_or(0);
        Kind::ALL[(index + 1) % Kind::ALL.len()]
    }
}

/// Fills `out` with a periodic window (the right choice for spectrum analysis).
pub fn fill(kind: Kind, out: &mut [f32]) {
    let n = out.len() as f32;
    let terms: &[f32] = match kind {
        Kind::Rect => &[1.0],
        Kind::Hann => &[0.5, -0.5],
        Kind::Hamming => &[0.54, -0.46],
        Kind::BlackmanHarris => &[0.35875, -0.48829, 0.14128, -0.01168],
        Kind::FlatTop => &[0.215578948, -0.416631580, 0.277263158, -0.083578947, 0.006947368],
    };
    for (i, value) in out.iter_mut().enumerate() {
        let phase = 2.0 * core::f32::consts::PI * i as f32 / n;
        let mut sum = 0.0;
        for (m, a) in terms.iter().enumerate() {
            sum += a * cos(m as f32 * phase);
        }
        *value = sum;
    }
}

pub fn coherent_gain(window: &[f32]) -> f32 {
    window.iter().sum::<f32>() / window.len() as f32
}

pub fn power_sum(window: &[f32]) -> f32 {
    window.iter().map(|w| w * w).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gains_match_the_textbook_values() {
        let mut w = vec![0.0f32; 4096];
        fill(Kind::Hann, &mut w);
        assert!((coherent_gain(&w) - 0.5).abs() < 1e-5);
        assert!((power_sum(&w) / 4096.0 - 0.375).abs() < 1e-5);
        fill(Kind::BlackmanHarris, &mut w);
        assert!((coherent_gain(&w) - 0.35875).abs() < 1e-5);
        fill(Kind::FlatTop, &mut w);
        assert!((coherent_gain(&w) - 0.215578948).abs() < 1e-5);
        fill(Kind::Rect, &mut w);
        assert_eq!(coherent_gain(&w), 1.0);
    }

    #[test]
    fn names_round_trip_and_cycle() {
        for kind in Kind::ALL {
            assert_eq!(Kind::from_name(kind.name()), Some(kind));
        }
        assert_eq!(Kind::FlatTop.next(), Kind::Rect);
    }
}
