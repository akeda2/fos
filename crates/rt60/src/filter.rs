//! Zero-phase band filters for impulse responses. A band is a 2nd-order Butterworth high-pass at its lower edge and a 2nd-order
//! low-pass at its upper edge, run forward and then backward over the response: the forward-backward pass cancels the phase
//! (the decay is not shifted in time) and squares the magnitude, so the edges are -6 dB and the skirts fall at 24 dB per octave.
//! Everything is f64 inside: a 50 Hz band at 48 kHz is a pole very close to 1, where f32 states lose the filter.

use dsp::math::sin_cos64;

#[derive(Clone, Copy)]
struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    z1: f64,
    z2: f64,
}

impl Biquad {
    /// RBJ cookbook high-pass (`high` true) or low-pass at `hz`, Q = 1/sqrt(2) (Butterworth).
    fn new(rate: f64, hz: f64, high: bool) -> Self {
        let (s, c) = sin_cos64(2.0 * core::f64::consts::PI * hz / rate);
        let alpha = s / (2.0 * core::f64::consts::FRAC_1_SQRT_2);
        let a0 = 1.0 + alpha;
        let (b0, b1) = if high { ((1.0 + c) / 2.0, -(1.0 + c)) } else { ((1.0 - c) / 2.0, 1.0 - c) };
        Self { b0: b0 / a0, b1: b1 / a0, b2: b0 / a0, a1: -2.0 * c / a0, a2: (1.0 - alpha) / a0, z1: 0.0, z2: 0.0 }
    }

    fn process(&mut self, x: f64) -> f64 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }
}

/// Band-limits `x` in place to `low_hz..high_hz`, zero phase. A `high_hz` of 0 or above 45 % of the rate leaves the top open (a
/// high-pass only); a `low_hz` of 0 leaves the bottom open.
pub fn band(x: &mut [f32], rate: u32, low_hz: f32, high_hz: f32) {
    let rate = rate as f64;
    let mut stages = [Biquad::new(rate, 1.0, true); 2];
    let mut count = 0;
    if low_hz > 0.0 {
        stages[count] = Biquad::new(rate, low_hz as f64, true);
        count += 1;
    }
    if high_hz > 0.0 && (high_hz as f64) < 0.45 * rate {
        stages[count] = Biquad::new(rate, high_hz as f64, false);
        count += 1;
    }
    if count == 0 {
        return;
    }
    for pass in 0..2 {
        let mut chain = stages;
        let mut run = |sample: &mut f32| {
            let mut v = *sample as f64;
            for stage in &mut chain[..count] {
                v = stage.process(v);
            }
            *sample = v as f32;
        };
        if pass == 0 {
            x.iter_mut().for_each(&mut run);
        } else {
            x.iter_mut().rev().for_each(&mut run);
        }
    }
}
