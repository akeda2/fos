//! Averaging of band power over frames. All four views are kept at once (they are cheap), the tape chooses which to show.

use dsp::math::exp64;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// The latest frame.
    Live,
    /// Exponential average with a time constant (`Config::tau_ms`).
    Exp,
    /// Linear (equal-weight) average of every frame since reset: the LTAS.
    Linear,
    /// Highest power seen since reset.
    Peak,
}

impl Mode {
    pub const ALL: [Mode; 4] = [Mode::Live, Mode::Exp, Mode::Linear, Mode::Peak];

    pub fn name(self) -> &'static str {
        match self {
            Mode::Live => "live",
            Mode::Exp => "exp",
            Mode::Linear => "ltas",
            Mode::Peak => "peak",
        }
    }

    pub fn from_name(name: &str) -> Option<Mode> {
        Mode::ALL.into_iter().find(|m| m.name() == name)
    }
}

pub struct Accumulator<const M: usize> {
    live: [f32; M],
    exp: [f32; M],
    peak: [f32; M],
    sum: [f64; M],
    frames: u64,
    alpha: f32,
    len: usize,
}

impl<const M: usize> Accumulator<M> {
    pub const fn new() -> Self {
        Self { live: [0.0; M], exp: [0.0; M], peak: [0.0; M], sum: [0.0; M], frames: 0, alpha: 1.0, len: 0 }
    }

    /// Clears everything and sets the number of outputs and the exponential average's time constant: `tau_s` seconds with
    /// frames arriving every `frame_s` seconds.
    pub fn configure(&mut self, len: usize, frame_s: f32, tau_s: f32) {
        assert!(len <= M);
        self.len = len;
        self.alpha = if tau_s <= 0.0 { 1.0 } else { (1.0 - exp64(-(frame_s as f64) / tau_s as f64)) as f32 };
        self.reset();
    }

    pub fn reset(&mut self) {
        self.live[..self.len].fill(0.0);
        self.exp[..self.len].fill(0.0);
        self.peak[..self.len].fill(0.0);
        self.sum[..self.len].fill(0.0);
        self.frames = 0;
    }

    pub fn frames(&self) -> u64 {
        self.frames
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn update(&mut self, power: &[f32]) {
        let first = self.frames == 0;
        for i in 0..self.len {
            let p = power[i];
            self.live[i] = p;
            self.exp[i] = if first { p } else { self.exp[i] + self.alpha * (p - self.exp[i]) };
            if p > self.peak[i] {
                self.peak[i] = p;
            }
            self.sum[i] += p as f64;
        }
        self.frames += 1;
    }

    /// The chosen view as power per output (zeros before the first frame).
    pub fn get(&self, mode: Mode, out: &mut [f32]) {
        let n = self.len.min(out.len());
        match mode {
            Mode::Live => out[..n].copy_from_slice(&self.live[..n]),
            Mode::Exp => out[..n].copy_from_slice(&self.exp[..n]),
            Mode::Peak => out[..n].copy_from_slice(&self.peak[..n]),
            Mode::Linear => {
                let scale = if self.frames == 0 { 0.0 } else { 1.0 / self.frames as f64 };
                for i in 0..n {
                    out[i] = (self.sum[i] * scale) as f32;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn views_differ_as_they_should() {
        let mut acc = Accumulator::<4>::new();
        acc.configure(2, 0.1, 0.1);
        for p in [[1.0, 4.0], [3.0, 0.0], [2.0, 2.0]] {
            acc.update(&p);
        }
        let mut out = [0.0; 2];
        acc.get(Mode::Live, &mut out);
        assert_eq!(out, [2.0, 2.0]);
        acc.get(Mode::Peak, &mut out);
        assert_eq!(out, [3.0, 4.0]);
        acc.get(Mode::Linear, &mut out);
        assert_eq!(out, [2.0, 2.0]);
        acc.get(Mode::Exp, &mut out);
        // alpha = 1 - e^-1 = 0.632: the first frame seeds, later ones pull towards the input.
        assert!(out[0] > 1.0 && out[0] < 3.0);
        assert!(acc.frames() == 3);
        acc.reset();
        acc.get(Mode::Linear, &mut out);
        assert_eq!(out, [0.0, 0.0]);
    }
}
