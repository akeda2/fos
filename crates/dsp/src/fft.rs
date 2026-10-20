//! Radix-2 FFT, split real/imaginary arrays (so the butterflies vectorise), in f32.
//!
//! `Plan` holds the twiddle factors for sizes up to `MAX`, stage by stage: for a butterfly half-size `h` (1, 2, 4 ...) the
//! table has `h` consecutive entries at offset `h - 1`, which makes the inner loop a unit-stride walk. The same generic code is
//! compiled twice, once for the baseline (SSE2) and once with AVX2+FMA; `forward` picks one from the `avx2` flag, which the
//! tape takes from `tape_rt::fp_level() == FP_AVX2` (running the AVX2 build on a CPU without it would fault).

use crate::math::sin_cos64;

/// Twiddle tables for transforms of up to `MAX` points (a power of two; `Plan<16384>` is 128 KiB).
pub struct Plan<const MAX: usize> {
    cos: [f32; MAX],
    sin: [f32; MAX],
    n: usize,
}

impl<const MAX: usize> Plan<MAX> {
    /// A plan for nothing yet; `const` so a tape can keep it in a `static`. Call `init` before use.
    pub const fn empty() -> Self {
        Self { cos: [0.0; MAX], sin: [0.0; MAX], n: 0 }
    }

    /// Prepares for transforms of `n` points (a power of two, 2..=MAX); false when `n` is not one.
    pub fn init(&mut self, n: usize) -> bool {
        if n < 2 || n > MAX || !n.is_power_of_two() {
            return false;
        }
        let mut half = 1;
        while half < n {
            for k in 0..half {
                let (s, c) = sin_cos64(-core::f64::consts::PI * k as f64 / half as f64);
                self.cos[half - 1 + k] = c as f32;
                self.sin[half - 1 + k] = -s as f32; // stored as +sin; forward uses exp(-i...) = c - i s
            }
            half *= 2;
        }
        self.n = n;
        true
    }

    pub fn len(&self) -> usize {
        self.n
    }

    /// In-place forward transform of `re`/`im` (each exactly `len()` long); X[k] = sum x[n] e^{-2 pi i n k / N}, unscaled.
    pub fn forward(&self, re: &mut [f32], im: &mut [f32], avx2: bool) {
        assert!(re.len() == self.n && im.len() == self.n && self.n >= 2);
        // The AVX2 build exists on x86-64 only; elsewhere the flag is ignored (aarch64 NEON vectorises the generic code).
        #[cfg(target_arch = "x86_64")]
        if avx2 {
            // SAFETY: the caller promises AVX2 and FMA are present (fp_level).
            return unsafe { forward_avx2(self, re, im) };
        }
        let _ = avx2;
        forward_generic(self, re, im);
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,fma")]
unsafe fn forward_avx2<const MAX: usize>(plan: &Plan<MAX>, re: &mut [f32], im: &mut [f32]) {
    forward_generic(plan, re, im);
}

#[inline(always)]
fn forward_generic<const MAX: usize>(plan: &Plan<MAX>, re: &mut [f32], im: &mut [f32]) {
    let n = plan.n;
    let bits = n.trailing_zeros();
    for i in 0..n {
        let j = i.reverse_bits() >> (usize::BITS - bits);
        if j > i {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut half = 1;
    while half < n {
        let cos = &plan.cos[half - 1..2 * half - 1];
        let sin = &plan.sin[half - 1..2 * half - 1];
        for (block_re, block_im) in re.chunks_exact_mut(2 * half).zip(im.chunks_exact_mut(2 * half)) {
            let (lo_re, hi_re) = block_re.split_at_mut(half);
            let (lo_im, hi_im) = block_im.split_at_mut(half);
            for k in 0..half {
                let tr = cos[k] * hi_re[k] + sin[k] * hi_im[k];
                let ti = cos[k] * hi_im[k] - sin[k] * hi_re[k];
                hi_re[k] = lo_re[k] - tr;
                hi_im[k] = lo_im[k] - ti;
                lo_re[k] += tr;
                lo_im[k] += ti;
            }
        }
        half *= 2;
    }
}

/// |X[k]|^2 for k in 0..out.len() (typically N/2 + 1).
pub fn power(re: &[f32], im: &[f32], out: &mut [f32]) {
    for ((o, r), i) in out.iter_mut().zip(re).zip(im) {
        *o = r * r + i * i;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn avx2_here() -> bool {
        std::is_x86_feature_detected!("avx2") && std::is_x86_feature_detected!("fma")
    }

    fn reference(x: &[f64]) -> Vec<(f64, f64)> {
        let n = x.len();
        (0..n)
            .map(|k| {
                let (mut sr, mut si) = (0.0, 0.0);
                for (t, v) in x.iter().enumerate() {
                    let angle = -2.0 * std::f64::consts::PI * (t * k % n) as f64 / n as f64;
                    sr += v * angle.cos();
                    si += v * angle.sin();
                }
                (sr, si)
            })
            .collect()
    }

    #[test]
    fn matches_a_direct_dft_both_builds() {
        let n = 256;
        let signal: Vec<f64> = (0..n).map(|t| (t as f64 * 0.37).sin() + 0.5 * ((t * t % 17) as f64) - 3.0).collect();
        let expected = reference(&signal);
        let mut plan = Box::new(Plan::<16384>::empty());
        assert!(plan.init(n));
        for avx2 in [false, true] {
            if avx2 && !avx2_here() {
                continue;
            }
            let mut re: Vec<f32> = signal.iter().map(|&v| v as f32).collect();
            let mut im = vec![0.0f32; n];
            plan.forward(&mut re, &mut im, avx2);
            for k in 0..n {
                assert!((re[k] as f64 - expected[k].0).abs() < 1e-3, "avx2={avx2} re[{k}] {} vs {}", re[k], expected[k].0);
                assert!((im[k] as f64 - expected[k].1).abs() < 1e-3, "avx2={avx2} im[{k}]");
            }
        }
    }

    #[test]
    fn a_sine_lands_in_one_bin_at_large_size() {
        let n = 16384;
        let bin = 1000;
        let mut plan = Box::new(Plan::<16384>::empty());
        assert!(plan.init(n));
        let mut re: Vec<f32> = (0..n).map(|t| (2.0 * std::f64::consts::PI * (bin * t % n) as f64 / n as f64).sin() as f32).collect();
        let mut im = vec![0.0f32; n];
        plan.forward(&mut re, &mut im, avx2_here());
        let mut spectrum = vec![0.0f32; n / 2 + 1];
        power(&re, &im, &mut spectrum);
        let peak = (0..spectrum.len()).max_by(|&a, &b| spectrum[a].total_cmp(&spectrum[b])).unwrap();
        assert_eq!(peak, bin);
        assert!((spectrum[bin].sqrt() - n as f32 / 2.0).abs() < 1.0, "amplitude {}", spectrum[bin].sqrt());
        assert!(spectrum[bin + 5] < spectrum[bin] * 1e-9);
    }

    #[test]
    fn rejects_bad_sizes() {
        let mut plan = Box::new(Plan::<16384>::empty());
        assert!(!plan.init(1000) && !plan.init(1) && !plan.init(32768));
    }
}
