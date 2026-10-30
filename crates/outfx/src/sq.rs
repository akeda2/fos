//! SQ (CBS, 1971) decoder: the passive 4-2-4 matrix, for records and files made with an SQ encoder. Stereo `Lt Rt` in, four
//! channels out: `LF RF LB RB`.
//!
//! ```text
//!   LF = Lt                     RF = Rt
//!   LB = 0.707 (j Lt - Rt)      RB = 0.707 (Lt - j Rt)
//! ```
//!
//! with `j` the 90 degree shift of `phase::Quadrature`. Encoded, the four corners are LF = (1, 0), RF = (0, 1), LB = (-0.707 j,
//! -0.707), RB = (0.707, 0.707 j); a centre-front sound is in phase on both channels, a centre-back sound is 90 degrees out of
//! phase. Without logic steering the fronts carry 3 dB of the rear channels, and a front sound leaks 3 dB into both rears; the
//! separation between the other pairs is what the phase network gives (30 dB and more). `swap_rear` mirrors the rear pair,
//! should a source be encoded with the opposite sign of `j`.
use crate::phase::Quadrature;

const SIDE: f32 = core::f32::consts::FRAC_1_SQRT_2;

#[derive(Clone, Copy)]
pub struct SqDecoder {
    left: Quadrature,
    right: Quadrature,
    pub swap_rear: bool,
}

impl SqDecoder {
    pub const fn new() -> Self {
        Self { left: Quadrature::new(), right: Quadrature::new(), swap_rear: false }
    }

    /// `[Lt, Rt]` in, `[LF, RF, LB, RB]` out; every output is delayed by the same few samples of the phase network.
    pub fn process(&mut self, frame: [f32; 2]) -> [f32; 4] {
        let (lt, lt_j) = self.left.process(frame[0]);
        let (rt, rt_j) = self.right.process(frame[1]);
        let left_back = SIDE * (lt_j - rt);
        let right_back = SIDE * (lt - rt_j);
        if self.swap_rear { [lt, rt, right_back, left_back] } else { [lt, rt, left_back, right_back] }
    }

    pub fn reset(&mut self) {
        self.left.reset();
        self.right.reset();
    }
}

impl Default for SqDecoder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sum of tones (no two harmonically related, so nothing lines up by accident), `n` samples at 48 kHz.
    fn signal(n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| [220.0f32, 530.0, 1370.0, 3100.0, 6700.0].iter().map(|f| (2.0 * core::f32::consts::PI * f * i as f32 / 48_000.0).sin()).sum::<f32>() * 0.2)
            .collect()
    }

    /// The SQ encoder for one corner: `Lt = a LF + b J(LB) ...`, using the same phase network so the delays match.
    fn encode(corner: usize, x: &[f32]) -> Vec<[f32; 2]> {
        let mut q = Quadrature::new();
        x.iter()
            .map(|&v| {
                let (same, j) = q.process(v);
                match corner {
                    0 => [same, 0.0],
                    1 => [0.0, same],
                    2 => [-SIDE * j, -SIDE * same],
                    _ => [SIDE * same, SIDE * j],
                }
            })
            .collect()
    }

    fn power_db(x: &[f32]) -> f32 {
        10.0 * (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32 + 1e-20).log10()
    }

    #[test]
    fn each_corner_comes_out_where_it_went_in() {
        let source = signal(48_000);
        let reference = power_db(&source[24_000..]);
        // [LF, RF, LB, RB] levels (dB re the source) for a sound encoded into each corner
        for (corner, expected) in [(0, [0.0, -99.0, -3.0, -3.0]), (1, [-99.0, 0.0, -3.0, -3.0]), (2, [-3.0, -3.0, 0.0, -99.0]), (3, [-3.0, -3.0, -99.0, 0.0])] {
            let mut decoder = SqDecoder::new();
            let mut out = [Vec::new(), Vec::new(), Vec::new(), Vec::new()];
            for frame in encode(corner, &source) {
                for (channel, v) in decoder.process(frame).into_iter().enumerate() {
                    out[channel].push(v);
                }
            }
            for channel in 0..4 {
                let level = power_db(&out[channel][24_000..]) - reference;
                if expected[channel] < -90.0 {
                    assert!(level < -25.0, "corner {corner} leaks {level} dB into channel {channel}");
                } else {
                    assert!((level - expected[channel]).abs() < 0.5, "corner {corner}, channel {channel}: {level} dB, expected {}", expected[channel]);
                }
            }
        }
    }

    #[test]
    fn swap_rear_mirrors_the_rear_pair() {
        let source = signal(24_000);
        let (mut a, mut b) = (SqDecoder::new(), SqDecoder::new());
        b.swap_rear = true;
        for frame in encode(2, &source) {
            let (x, y) = (a.process(frame), b.process(frame));
            assert_eq!([x[0], x[1], x[2], x[3]], [y[0], y[1], y[3], y[2]]);
        }
    }
}
