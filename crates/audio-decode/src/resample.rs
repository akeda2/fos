//! A streaming stereo sample-rate converter: cubic (Catmull-Rom) interpolation between input frames.
//!
//! The ratio is kept as the integers `in_rate` and `out_rate`, so the output position never drifts however long a
//! track is. Cubic interpolation is clean for the common 44.1 kHz to 48 kHz step (the imaging it leaves is far below the
//! music) but it has no anti-alias filter, so downsampling by a large factor would alias; a file whose rate the device
//! offers is not resampled at all.

pub struct Resampler {
    in_rate: i64,
    out_rate: i64,
    /// The input frames around the interval being read: `history[1]` to `history[2]` is that interval.
    history: [[f32; 2]; 4],
    /// Where the next output sample falls in the interval, in 1/`out_rate` of an input frame (negative: not yet).
    phase: i64,
    started: bool,
}

impl Resampler {
    pub const fn new(in_rate: u32, out_rate: u32) -> Self {
        Resampler { in_rate: in_rate as i64, out_rate: out_rate as i64, history: [[0.0; 2]; 4], phase: 0, started: false }
    }

    pub fn reset(&mut self) {
        self.started = false;
        self.phase = 0;
    }

    /// Takes the next input frame and calls `emit` for every output frame that now falls before it.
    pub fn push(&mut self, frame: [f32; 2], emit: &mut impl FnMut([f32; 2])) {
        if self.started {
            self.history = [self.history[1], self.history[2], self.history[3], frame];
            self.phase -= self.out_rate;
        } else {
            // The interpolated interval trails the newest frame by two: the first output (frame 0 itself) comes with the third push.
            self.history = [frame; 4];
            self.phase = 2 * self.out_rate;
            self.started = true;
        }
        while self.phase < self.out_rate {
            if self.phase >= 0 {
                let t = self.phase as f32 / self.out_rate as f32;
                let [a, b, c, d] = self.history;
                let channel = |i: usize| {
                    let (a, b, c, d) = (a[i], b[i], c[i], d[i]);
                    b + 0.5 * t * (c - a + t * (2.0 * a - 5.0 * b + 4.0 * c - d + t * (3.0 * (b - c) + d - a)))
                };
                emit([channel(0), channel(1)]);
            }
            self.phase += self.in_rate;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(in_rate: u32, out_rate: u32, input: &[[f32; 2]]) -> std::vec::Vec<[f32; 2]> {
        let mut resampler = Resampler::new(in_rate, out_rate);
        let mut out = std::vec::Vec::new();
        for frame in input {
            resampler.push(*frame, &mut |sample| out.push(sample));
        }
        out
    }

    #[test]
    fn same_rate_passes_through() {
        let input: std::vec::Vec<[f32; 2]> = (0..50).map(|i| [i as f32, -(i as f32)]).collect();
        let out = run(48000, 48000, &input);
        assert_eq!(out.len(), 48); // the last two input frames are only the interpolation lookahead
        assert!(out.iter().zip(&input).all(|(a, b)| (a[0] - b[0]).abs() < 1e-4 && (a[1] - b[1]).abs() < 1e-4));
    }

    #[test]
    fn count_follows_the_ratio_and_a_ramp_stays_a_ramp() {
        let input: std::vec::Vec<[f32; 2]> = (0..4410).map(|i| [i as f32 / 4410.0, 0.0]).collect();
        let out = run(44100, 48000, &input);
        assert!((out.len() as i64 - 4800).abs() <= 2, "{}", out.len());
        // A straight line is reproduced exactly by cubic interpolation: sample n sits at n * 44100/48000 of the ramp.
        for (n, sample) in out.iter().enumerate().skip(4).take(4000) {
            assert!((sample[0] - n as f32 * 44100.0 / 48000.0 / 4410.0).abs() < 1e-4, "{n}");
        }
    }
}
