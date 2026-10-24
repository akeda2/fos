//! A spring reverb in the manner of the tank in an amplifier or a Space Echo: the signal runs down a spring and comes back from
//! the far end, again and again, each trip a little quieter. Two things make it a spring and not a room. The spring is
//! dispersive: low frequencies travel more slowly than high ones, so a click comes back as a falling chirp (the "drip" and
//! "boing"); that is a cascade of first-order allpass filters, which changes timing and nothing else. And there is only one
//! path, so the echoes come as a regular train a few tens of milliseconds apart, not a diffuse cloud. Two springs of slightly
//! different length make the left and right channels.

use crate::Delay;
use dsp::math;

const SECTIONS: usize = 40;
const SPRINGS: usize = 2;
/// One-way travel time of each spring, seconds, and its dispersion (the allpass coefficient: more negative, more chirp).
const TRAVEL: [f32; SPRINGS] = [0.029, 0.034];
const DISPERSION: [f32; SPRINGS] = [-0.65, -0.60];
/// 192 kHz x 40 ms.
const CAP: usize = 7680;
const WET_GAIN: f32 = 0.9;

/// A cascade of first-order allpass sections: `y = a x + z`, `z = x - a y`. With a negative `a` the group delay is largest at
/// low frequencies (about 4.6 samples per section at 400 Hz for a = -0.65, 1.9 at 4 kHz).
pub(crate) struct Dispersion {
    a: f32,
    z: [f32; SECTIONS],
}

impl Dispersion {
    pub(crate) const fn new(a: f32) -> Self {
        Dispersion { a, z: [0.0; SECTIONS] }
    }

    #[inline]
    pub(crate) fn process(&mut self, mut x: f32) -> f32 {
        for z in self.z.iter_mut() {
            let y = self.a * x + *z;
            *z = x - self.a * y;
            x = y;
        }
        x
    }

    fn clear(&mut self) {
        self.z = [0.0; SECTIONS];
    }
}

pub struct Spring {
    line: [Delay<CAP>; SPRINGS],
    chain: [Dispersion; SPRINGS],
    /// The one-pole low-pass inside each loop (a spring loses its highs on every trip) and the input's DC/rumble blocker.
    damp: [f32; SPRINGS],
    block: f32,
    length: [f32; SPRINGS],
    damp_coef: f32,
    block_coef: f32,
    feedback: f32,
    decay: f32,
    tone: f32,
    mix: f32,
}

impl Spring {
    pub fn new(sample_rate: u32) -> Self {
        let mut spring = Spring {
            line: [Delay::new(), Delay::new()],
            chain: [Dispersion::new(DISPERSION[0]), Dispersion::new(DISPERSION[1])],
            damp: [0.0; SPRINGS],
            block: 0.0,
            length: [0.0; SPRINGS],
            damp_coef: 0.0,
            block_coef: 0.0,
            feedback: 0.0,
            decay: 0.5,
            tone: 0.5,
            mix: 0.0,
        };
        spring.set_sample_rate(sample_rate);
        spring
    }

    pub fn set_sample_rate(&mut self, sample_rate: u32) {
        let rate = sample_rate.clamp(8000, 192000) as f32;
        for index in 0..SPRINGS {
            // The allpass chain adds its own delay; the line makes up the rest of the travel time.
            self.length[index] = (TRAVEL[index] * rate - 4.0 * SECTIONS as f32).clamp(8.0, (CAP - 2) as f32);
            self.line[index].clear();
            self.chain[index].clear();
        }
        self.block_coef = 1.0 - math::exp64((-2.0 * core::f64::consts::PI * 150.0 / rate as f64) as f64) as f32;
        self.damp = [0.0; SPRINGS];
        self.block = 0.0;
        self.set_tone(self.tone);
        self.set_decay(self.decay);
    }

    /// How long the springs ring, 0..=1 (about 0.4 s to 3 s).
    pub fn set_decay(&mut self, decay: f32) {
        self.decay = decay.clamp(0.0, 1.0);
        self.feedback = 0.35 + 0.5 * self.decay;
    }

    /// How bright the return is, 0..=1 (a loop low-pass from 1.5 kHz to 6 kHz).
    pub fn set_tone(&mut self, tone: f32) {
        self.tone = tone.clamp(0.0, 1.0);
        let hz = 1500.0 + 4500.0 * self.tone;
        self.damp_coef = 1.0 - math::exp64(-2.0 * core::f64::consts::PI * hz as f64 / self.rate() as f64) as f32;
    }

    /// The reverb's level, 0..=1 (0 is off and costs nothing).
    pub fn set_mix(&mut self, mix: f32) {
        self.mix = mix.clamp(0.0, 1.0);
    }

    pub fn mix(&self) -> f32 {
        self.mix
    }

    pub fn decay(&self) -> f32 {
        self.decay
    }

    pub fn tone(&self) -> f32 {
        self.tone
    }

    // The sample rate is only needed to turn a cutoff into a coefficient; it is recovered from the travel time.
    fn rate(&self) -> f32 {
        (self.length[0] + 4.0 * SECTIONS as f32) / TRAVEL[0]
    }

    /// One stereo frame in, one out: the dry signal plus the springs' return.
    pub fn process(&mut self, left: f32, right: f32) -> (f32, f32) {
        if self.mix == 0.0 {
            return (left, right);
        }
        let mono = 0.5 * (left + right);
        self.block += self.block_coef * (mono - self.block);
        let input = mono - self.block;
        let mut out = [0.0; SPRINGS];
        for index in 0..SPRINGS {
            let arrived = self.chain[index].process(self.line[index].read(self.length[index]));
            self.damp[index] += self.damp_coef * (arrived - self.damp[index]);
            out[index] = self.damp[index];
            // Soft-limited so a loud input at full decay rings but cannot run away.
            let fed = input + self.feedback * self.damp[index];
            self.line[index].push(fed / (1.0 + 0.25 * fed.abs()));
        }
        let wet = self.mix * WET_GAIN;
        (left + out[0] * wet, right + out[1] * wet)
    }
}
