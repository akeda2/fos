//! The exponential sine sweep (Farina) and its deconvolution. Playing the sweep through the room and recording it gives
//! `recording = sweep * impulse response`; dividing the spectra gives the impulse response back, with far more energy behind it
//! than a balloon's pop (the whole sweep, not one click) and with the loudspeaker's harmonic distortion landing *before* the
//! response (negative time), so it never lands in the decay.
//!
//! The division is regularised: `H = R conj(S) / (|S|^2 + eps)`, eps a fraction of the largest `|S|^2`, so frequencies the sweep
//! does not cover (below `f1`, above `f2`) come out as silence instead of amplified noise.

use dsp::fft::Plan;
use dsp::math::{exp64, ln64, sin_cos64};

/// How much below the strongest bin of the sweep's spectrum the regularisation sits (power, 1e-5 = -50 dB).
const EPSILON: f32 = 1e-5;

/// A sweep from `f1` to `f2` Hz lasting `samples` samples at `rate`.
#[derive(Clone, Copy)]
pub struct Sweep {
    pub rate: u32,
    pub f1: f32,
    pub f2: f32,
    pub samples: usize,
}

impl Sweep {
    pub fn seconds(&self) -> f32 {
        self.samples as f32 / self.rate as f32
    }

    /// Sample `i` (zero outside the sweep), `level` the amplitude. The first and last 10 ms are faded (smoothstep) so the
    /// loudspeaker does not click.
    pub fn sample(&self, i: usize, level: f32) -> f32 {
        if i >= self.samples {
            return 0.0;
        }
        let t = i as f64 / self.rate as f64;
        let total = self.samples as f64 / self.rate as f64;
        let k = ln64(self.f2 as f64 / self.f1 as f64);
        let phase = 2.0 * core::f64::consts::PI * self.f1 as f64 * total / k * (exp64(t * k / total) - 1.0);
        let fade = (self.rate as usize / 100).max(1);
        let edge = i.min(self.samples - 1 - i);
        let gain = if edge < fade {
            let x = edge as f32 / fade as f32;
            x * x * (3.0 - 2.0 * x)
        } else {
            1.0
        };
        level * gain * sin_cos64(phase).0 as f32
    }

    /// Writes the whole sweep (at `level`) into `out`, which must hold `samples`; the rest of `out` is zeroed.
    pub fn fill(&self, out: &mut [f32], level: f32) {
        for (i, v) in out.iter_mut().enumerate() {
            *v = self.sample(i, level);
        }
    }
}

/// Deconvolves in place. `rec_*` hold the recording and `ref_*` the played sweep, each zero-padded to `plan.len()` samples (the
/// imaginary parts zero). On return `rec_re` is the impulse response, its sample 0 aligned with the start of the sweep (a
/// device latency or the distance to the room's speaker shows as the position of the first big sample). The transform
/// is circular: pad so that recording length + the response you want to keep fits in `plan.len()`.
pub fn deconvolve<const MAX: usize>(plan: &Plan<MAX>, avx2: bool, rec_re: &mut [f32], rec_im: &mut [f32], ref_re: &mut [f32], ref_im: &mut [f32]) {
    let n = plan.len();
    plan.forward(rec_re, rec_im, avx2);
    plan.forward(ref_re, ref_im, avx2);
    let strongest = ref_re.iter().zip(ref_im.iter()).fold(0.0f32, |m, (r, i)| m.max(r * r + i * i));
    let epsilon = strongest * EPSILON;
    for k in 0..n {
        let (sr, si) = (ref_re[k], ref_im[k]);
        let (rr, ri) = (rec_re[k], rec_im[k]);
        let scale = 1.0 / (sr * sr + si * si + epsilon);
        // R * conj(S), then conjugated for the inverse-by-forward trick
        rec_re[k] = (rr * sr + ri * si) * scale;
        rec_im[k] = -(ri * sr - rr * si) * scale;
    }
    plan.forward(rec_re, rec_im, avx2);
    let norm = 1.0 / n as f32;
    for v in rec_re.iter_mut() {
        *v *= norm;
    }
}
