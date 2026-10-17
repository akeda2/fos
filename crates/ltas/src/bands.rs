//! Output resolutions: fractional-octave bands (ISO 266 base-ten: centres 1000 Hz * 10^(3k/(10b))) or raw FFT bins.

use dsp::math::{exp64, ln64};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Resolution {
    /// 1/3 octave bands.
    Third,
    /// 1/6 octave bands.
    Sixth,
    /// 1/12 octave bands.
    Twelfth,
    /// One output per FFT bin (DC excluded).
    Fft,
}

impl Resolution {
    pub const ALL: [Resolution; 4] = [Resolution::Third, Resolution::Sixth, Resolution::Twelfth, Resolution::Fft];

    pub fn name(self) -> &'static str {
        match self {
            Resolution::Third => "1/3",
            Resolution::Sixth => "1/6",
            Resolution::Twelfth => "1/12",
            Resolution::Fft => "fft",
        }
    }

    pub fn from_name(name: &str) -> Option<Resolution> {
        Resolution::ALL.into_iter().find(|r| r.name() == name)
    }

    pub fn next(self) -> Resolution {
        let index = Resolution::ALL.iter().position(|&r| r == self).unwrap_or(0);
        Resolution::ALL[(index + 1) % Resolution::ALL.len()]
    }

    /// Bands per octave, 0 for FFT bins.
    pub fn per_octave(self) -> u32 {
        match self {
            Resolution::Third => 3,
            Resolution::Sixth => 6,
            Resolution::Twelfth => 12,
            Resolution::Fft => 0,
        }
    }
}

/// Edges (Hz) of fractional-octave band `index` counted from 1 kHz (negative below): (lower, centre, upper).
pub fn band_edges(per_octave: u32, index: i32) -> (f32, f32, f32) {
    let ratio_log = 0.3 * core::f64::consts::LN_10; // ln of the base-ten octave ratio 10^(3/10)
    let step = ratio_log / per_octave as f64;
    let centre = 1000.0 * exp64(index as f64 * step);
    let half = step / 2.0;
    (
        (centre * exp64(-half)) as f32,
        centre as f32,
        (centre * exp64(half)) as f32,
    )
}

/// Index of the first band whose centre is at or above `hz` (1 % slack: the 19.95 Hz band is the nominal 20 Hz one).
pub fn first_band_at_or_above(per_octave: u32, hz: f32) -> i32 {
    let step = 0.3 * core::f64::consts::LN_10 / per_octave as f64;
    let exact = ln64(hz as f64 / 1000.0) / step;
    let mut index = exact as i32 - 2;
    while band_edges(per_octave, index).1 < hz * 0.99 {
        index += 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn third_octave_centres_are_the_iso_ones() {
        // Nominal ISO values are the rounded ones: 1000, 1250 (1259), 1600 (1585), 20 (19.95), 20000 (19953).
        let near = |a: f32, b: f32| (a / b - 1.0).abs() < 0.001;
        assert!(near(band_edges(3, 0).1, 1000.0));
        assert!(near(band_edges(3, 1).1, 1258.9));
        assert!(near(band_edges(3, -1).1, 794.3));
        assert!(near(band_edges(3, -17).1, 19.95));
        assert!(near(band_edges(3, 13).1, 19953.0));
        let (lo, c, hi) = band_edges(3, 0);
        assert!(near(lo, 891.25) && near(hi, 1122.0) && near(c, 1000.0));
    }

    #[test]
    fn sixth_octave_has_twice_the_bands() {
        assert!(((band_edges(6, 2).1 / band_edges(3, 1).1) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn first_band_search() {
        assert_eq!(first_band_at_or_above(3, 20.0), -17); // 19.95 Hz, within the tolerance of 20
        assert_eq!(first_band_at_or_above(3, 1000.0), 0);
        assert_eq!(first_band_at_or_above(3, 1013.0), 1);
    }
}
