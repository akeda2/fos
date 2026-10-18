//! Stereo measurements: the correlation coefficient and the L/R balance, smoothed with an exponential window.
//! Correlation is `sum(l*r) / sqrt(sum(l*l) * sum(r*r))` over that window: +1 for a mono signal, 0 for unrelated
//! channels (or one silent), -1 for opposite polarity. Hard-float tapes only (it uses `sqrt`).

use crate::math::sqrt;

#[derive(Clone, Copy)]
pub struct Correlator {
    alpha: f32,
    sll: f32,
    srr: f32,
    slr: f32,
}

impl Correlator {
    /// A window of about `tau_s` seconds at `rate` Hz.
    pub fn new(rate: u32, tau_s: f32) -> Self {
        let mut c = Self { alpha: 0.0, sll: 0.0, srr: 0.0, slr: 0.0 };
        c.set_window(rate, tau_s);
        c
    }

    pub fn set_window(&mut self, rate: u32, tau_s: f32) {
        self.alpha = 1.0 / (tau_s * rate as f32).max(1.0);
    }

    pub fn reset(&mut self) {
        self.sll = 0.0;
        self.srr = 0.0;
        self.slr = 0.0;
    }

    /// Feeds equally long slices (the shorter length counts).
    pub fn feed(&mut self, left: &[f32], right: &[f32]) {
        for (&l, &r) in left.iter().zip(right) {
            self.sll += self.alpha * (l * l - self.sll);
            self.srr += self.alpha * (r * r - self.srr);
            self.slr += self.alpha * (l * r - self.slr);
        }
    }

    /// -1..=1; 0 when either channel is (nearly) silent.
    pub fn correlation(&self) -> f32 {
        let product = self.sll * self.srr;
        if product < 1e-14 {
            return 0.0;
        }
        let value = self.slr / sqrt(product);
        if value > 1.0 { 1.0 } else if value < -1.0 { -1.0 } else { value }
    }

    /// -1 (only left) ..= +1 (only right) by RMS amplitude: `(r - l) / (r + l)`; 0 when silent.
    pub fn balance(&self) -> f32 {
        let (l, r) = (sqrt(self.sll), sqrt(self.srr));
        if l + r < 1e-7 { 0.0 } else { (r - l) / (r + l) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(l: impl Fn(usize) -> f32, r: impl Fn(usize) -> f32) -> Correlator {
        let mut c = Correlator::new(48_000, 0.1);
        let left: Vec<f32> = (0..48_000).map(&l).collect();
        let right: Vec<f32> = (0..48_000).map(&r).collect();
        c.feed(&left, &right);
        c
    }

    fn tone(i: usize, phase: f32) -> f32 {
        crate::math::sin(2.0 * core::f32::consts::PI * 1000.0 * i as f32 / 48_000.0 + phase)
    }

    #[test]
    fn mono_inverted_and_quadrature() {
        assert!(run(|i| tone(i, 0.0), |i| tone(i, 0.0)).correlation() > 0.99);
        assert!(run(|i| tone(i, 0.0), |i| -tone(i, 0.0)).correlation() < -0.99);
        assert!(run(|i| tone(i, 0.0), |i| tone(i, core::f32::consts::FRAC_PI_2)).correlation().abs() < 0.05);
    }

    #[test]
    fn silence_and_balance() {
        assert_eq!(run(|_| 0.0, |_| 0.0).correlation(), 0.0);
        assert_eq!(run(|_| 0.0, |_| 0.0).balance(), 0.0);
        assert!(run(|i| tone(i, 0.0), |_| 0.0).balance() < -0.99);
        assert!(run(|i| 0.5 * tone(i, 0.0), |i| tone(i, 0.0)).balance() > 0.3);
    }
}
