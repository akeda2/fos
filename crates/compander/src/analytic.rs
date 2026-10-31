//! A ripple-free amplitude estimate. Blackmer's patent (US 3,789,143) gets an RMS value that does not wobble at low
//! frequencies by feeding the signal through a network with two outputs 90 degrees apart and summing their squares: for a
//! sine, sin^2 + cos^2 is constant. Here the network is a pair of all-pass chains (a Hilbert pair, 8 poles), whose phase
//! difference stays near 90 degrees from a few tens of hertz up to nearly Nyquist.

/// Coefficients of the two chains. Each stage is H(z) = (a^2 - z^-2) / (1 - a^2 z^-2); the first chain also sees its
/// input delayed by one sample (Olli Niemitalo's elliptic design, 8 poles).
const CHAIN_A: [f32; 4] = [0.6923878, 0.936_065_4, 0.988_229_5, 0.998_748_85];
const CHAIN_B: [f32; 4] = [0.402_192_1, 0.856_171_1, 0.972_290_95, 0.995_288_5];

#[derive(Clone, Copy, Default)]
struct Stage {
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

impl Stage {
    fn run(&mut self, a: f32, x: f32) -> f32 {
        let a2 = a * a;
        let y = a2 * (x + self.y2) - self.x2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }
}

/// The quadrature pair; `power` gives amplitude squared of whatever goes in.
#[derive(Clone, Copy, Default)]
pub struct Analytic {
    a: [Stage; 4],
    b: [Stage; 4],
    delayed: f32,
}

impl Analytic {
    /// The two 90-degree-apart outputs for input `x`.
    pub fn pair(&mut self, x: f32) -> (f32, f32) {
        let mut a = self.delayed;
        self.delayed = x;
        for (stage, coefficient) in self.a.iter_mut().zip(CHAIN_A) {
            a = stage.run(coefficient, a);
        }
        let mut b = x;
        for (stage, coefficient) in self.b.iter_mut().zip(CHAIN_B) {
            b = stage.run(coefficient, b);
        }
        (a, b)
    }

    /// Squared amplitude: the peak value squared for a sine (so mean-square is half of it).
    pub fn power(&mut self, x: f32) -> f32 {
        let (a, b) = self.pair(x);
        a * a + b * b
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Peak-to-peak spread of sin^2 + cos^2 for a sine of `hz`, in dB, once settled.
    fn ripple_db(hz: f64, rate: f64) -> f64 {
        let mut analytic = Analytic::default();
        let (mut low, mut high) = (f64::MAX, 0.0f64);
        let samples = (rate * 1.5) as usize;
        for n in 0..samples {
            let x = (2.0 * core::f64::consts::PI * hz * n as f64 / rate).sin() as f32;
            let power = analytic.power(x) as f64;
            if n > samples / 2 {
                low = low.min(power);
                high = high.max(power);
            }
        }
        10.0 * (high / low).log10()
    }

    #[test]
    fn quadrature_holds_over_the_audio_band() {
        for hz in [30.0, 60.0, 100.0, 1000.0, 5000.0, 15000.0, 20000.0] {
            println!("{hz} Hz: ripple {:.3} dB", ripple_db(hz, 48000.0));
        }
        for hz in [60.0, 100.0, 1000.0, 5000.0, 15000.0] {
            assert!(ripple_db(hz, 48000.0) < 0.15, "{hz} Hz");
        }
    }
}
