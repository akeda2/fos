//! A rotary speaker (Leslie): the signal is split at 800 Hz into a drum (bass) and a horn (treble). Each rotor's speed follows
//! a target slowly (the horn is light, the drum heavy, so they speed up and slow down at different rates, as the real ones
//! do) and turns the sound into two "ears": the level follows which way the rotor faces and the delay swings (the Doppler
//! shift of a source that moves toward and away from the listener).

use crate::{Delay, Sine, phase_step};
use dsp::math;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RotaryMode {
    /// No rotary speaker: the same mono signal on both channels.
    Off,
    /// Rotors spun down (they take a while, as the real ones do).
    Stop,
    Slow,
    Fast,
}

/// One rotor: its speed follows the target slowly; it modulates level and delay (Doppler) per ear.
struct Rotor {
    phase: u32,
    speed: f32,
    delay: Delay<64>,
}

impl Rotor {
    const fn new() -> Self {
        Rotor { phase: 0, speed: 0.0, delay: Delay::new() }
    }

    /// The rotor's two ear signals: level follows the rotor's facing, the delay swings by `swing` samples (Doppler).
    #[allow(clippy::too_many_arguments)]
    fn process(&mut self, input: f32, target: f32, follow: f32, rate: f32, swing: f32, depth: f32, sine: &Sine) -> (f32, f32) {
        self.speed += (target - self.speed) * follow;
        self.phase = self.phase.wrapping_add(phase_step(self.speed, rate));
        self.delay.push(input);
        let delayed = self.delay.read(swing + 1.0 + swing * sine.at(self.phase));
        let facing = sine.at(self.phase.wrapping_add(1 << 30));
        (delayed * 0.7 * (1.0 + depth * facing), delayed * 0.7 * (1.0 - depth * facing))
    }
}

pub struct Rotary {
    mode: RotaryMode,
    rate: f32,
    sine: Sine,
    horn: Rotor,
    drum: Rotor,
    split: f32,
    low: f32,
    horn_follow: f32,
    drum_follow: f32,
}

impl Rotary {
    pub fn new(sample_rate: u32) -> Self {
        let mut rotary = Rotary { mode: RotaryMode::Off, rate: 48000.0, sine: Sine::new(), horn: Rotor::new(), drum: Rotor::new(), split: 0.0, low: 0.0, horn_follow: 0.0, drum_follow: 0.0 };
        rotary.set_sample_rate(sample_rate);
        rotary
    }

    pub fn set_sample_rate(&mut self, sample_rate: u32) {
        let rate = sample_rate.clamp(8000, 192000) as f32;
        self.rate = rate;
        self.split = 1.0 - math::exp64(-2.0 * core::f64::consts::PI * 800.0 / rate as f64) as f32;
        // Time constants of the rotors' inertia: the horn is light (0.7 s), the drum heavy (3 s).
        self.horn_follow = 1.0 / (0.7 * rate);
        self.drum_follow = 1.0 / (3.0 * rate);
    }

    pub fn set_mode(&mut self, mode: RotaryMode) {
        self.mode = mode;
    }

    pub fn mode(&self) -> RotaryMode {
        self.mode
    }

    /// One mono sample in, one stereo frame out.
    pub fn process(&mut self, sample: f32) -> (f32, f32) {
        let (horn_target, drum_target) = match self.mode {
            RotaryMode::Off => return (sample, sample),
            RotaryMode::Stop => (0.0, 0.0),
            RotaryMode::Slow => (0.8, 0.66),
            RotaryMode::Fast => (6.7, 5.7),
        };
        let swing = self.rate / 48000.0;
        self.low += (sample - self.low) * self.split;
        let (low, high) = (self.low, sample - self.low);
        let (horn_left, horn_right) = self.horn.process(high, horn_target, self.horn_follow, self.rate, 6.0 * swing, 0.6, &self.sine);
        let (drum_left, drum_right) = self.drum.process(low, drum_target, self.drum_follow, self.rate, 3.0 * swing, 0.35, &self.sine);
        (horn_left + drum_left, horn_right + drum_right)
    }
}
