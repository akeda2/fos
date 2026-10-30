//! QS (Sansui "Regular Matrix", 1971) decoder: the passive 4-2-4 matrix. Stereo `Lt Rt` in, four channels out:
//! `LF RF LB RB`.
//!
//! The encoder (Ito and Ishida, US 3757047; the layout used by the open `quarkquad/qs` encoder) mixes each front with
//! 0.414 of the other front, each rear with 0.414 of the other rear shifted by 90 degrees, and normalises by
//! 1/sqrt(1 + 0.414^2) = 0.924 (`a`; `b` = 0.414 a = 0.383):
//!
//! ```text
//!   Lt = a LF + b RF - j (a LB + b RB)        Rt = b LF + a RF + j (b LB + a RB)
//! ```
//!
//! Decoding with the conjugate transpose gives the basic QS decoder:
//!
//! ```text
//!   LF = a Lt + b Rt            RF = b Lt + a Rt
//!   LB = j (a Lt - b Rt)        RB = j (b Lt - a Rt)
//! ```
//!
//! with `j` the 90 degree shift of `phase::Quadrature`. Diagonal pairs (LF/RB, RF/LB) separate completely, neighbours
//! only by 3 dB (QS's known trade for stable images and stereo compatibility); a mono signal lands in both fronts at full
//! level and in both rears about 8 dB lower (the plain matrix has no logic to take that away). `swap_rear` mirrors the rear pair for a source made with the opposite sign of `j`; only a
//! real QS record settles that, as it does for SQ.
use crate::phase::Quadrature;

/// 1 / sqrt(1 + (sqrt 2 - 1)^2)
const A: f32 = 0.923_879_5;
/// (sqrt 2 - 1) * A
const B: f32 = 0.382_683_43;

#[derive(Clone, Copy)]
pub struct QsDecoder {
    left: Quadrature,
    right: Quadrature,
    pub swap_rear: bool,
}

impl QsDecoder {
    pub const fn new() -> Self {
        Self { left: Quadrature::new(), right: Quadrature::new(), swap_rear: false }
    }

    /// `[Lt, Rt]` in, `[LF, RF, LB, RB]` out; every output is delayed by the same few samples of the phase network.
    pub fn process(&mut self, frame: [f32; 2]) -> [f32; 4] {
        let (lt, lt_j) = self.left.process(frame[0]);
        let (rt, rt_j) = self.right.process(frame[1]);
        let left_front = A * lt + B * rt;
        let right_front = B * lt + A * rt;
        let left_back = A * lt_j - B * rt_j;
        let right_back = B * lt_j - A * rt_j;
        if self.swap_rear { [left_front, right_front, right_back, left_back] } else { [left_front, right_front, left_back, right_back] }
    }

    pub fn reset(&mut self) {
        self.left.reset();
        self.right.reset();
    }
}

impl Default for QsDecoder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signal(n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| [220.0f32, 530.0, 1370.0, 3100.0, 6700.0].iter().map(|f| (2.0 * core::f32::consts::PI * f * i as f32 / 48_000.0).sin()).sum::<f32>() * 0.2)
            .collect()
    }

    /// The QS encoder for one corner, on the same phase network so the delays match.
    fn encode(corner: usize, x: &[f32]) -> Vec<[f32; 2]> {
        let mut q = Quadrature::new();
        x.iter()
            .map(|&v| {
                let (same, j) = q.process(v);
                match corner {
                    0 => [A * same, B * same],
                    1 => [B * same, A * same],
                    2 => [-A * j, B * j],
                    _ => [-B * j, A * j],
                }
            })
            .collect()
    }

    fn power_db(x: &[f32]) -> f32 {
        10.0 * (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32 + 1e-20).log10()
    }

    fn decode(frames: &[[f32; 2]]) -> [Vec<f32>; 4] {
        let mut decoder = QsDecoder::new();
        let mut out = [Vec::new(), Vec::new(), Vec::new(), Vec::new()];
        for &frame in frames {
            for (channel, v) in decoder.process(frame).into_iter().enumerate() {
                out[channel].push(v);
            }
        }
        out
    }

    #[test]
    fn each_corner_has_full_level_diagonal_null_and_3db_neighbours() {
        let source = signal(48_000);
        let reference = power_db(&source[24_000..]);
        // [LF, RF, LB, RB]; the neighbours of a corner are the adjacent speakers, the diagonal is the far one.
        for (corner, expected) in [(0, [0.0, -3.0, -3.0, -99.0]), (1, [-3.0, 0.0, -99.0, -3.0]), (2, [-3.0, -99.0, 0.0, -3.0]), (3, [-99.0, -3.0, -3.0, 0.0])] {
            let out = decode(&encode(corner, &source));
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
    fn mono_is_mostly_front_and_swap_rear_mirrors() {
        let source = signal(24_000);
        let mono: Vec<[f32; 2]> = source.iter().map(|&v| [v, v]).collect();
        let out = decode(&mono);
        let drop = power_db(&out[0][12_000..]) - power_db(&out[2][12_000..]);
        assert!((drop - 7.7).abs() < 0.5, "rear is {drop} dB below the front");
        let (mut a, mut b) = (QsDecoder::new(), QsDecoder::new());
        b.swap_rear = true;
        for frame in encode(2, &source) {
            let (x, y) = (a.process(frame), b.process(frame));
            assert_eq!([x[0], x[1], x[2], x[3]], [y[0], y[1], y[3], y[2]]);
        }
    }
}
