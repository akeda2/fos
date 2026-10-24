//! A stereo phaser: six first-order allpass stages swept by a slow oscillator (the right channel a quarter turn behind the left,
//! so the notches wander across the stereo image), with feedback around the chain, mixed with the dry sound. The sweep runs
//! exponentially from 300 Hz upward; `depth` sets how far (1 is a little over three octaves).

use crate::{Sine, phase_step};
use dsp::math;

const STAGES: usize = 6;
const LOW_HZ: f32 = 300.0;
/// log10 of the ratio between the top and the bottom of a full sweep (about 11.7: 3.5 kHz over 300 Hz).
const SWEEP_LOG10: f32 = 1.07;
/// The coefficients follow the oscillator every this many samples: a tan per sample would cost more than the stages.
const CONTROL: u32 = 16;

struct Channel {
    state: [f32; STAGES],
    last: f32,
    coefficient: f32,
}

impl Channel {
    const fn new() -> Self {
        Channel { state: [0.0; STAGES], last: 0.0, coefficient: 0.0 }
    }

    fn process(&mut self, sample: f32, feedback: f32) -> f32 {
        let mut x = sample + feedback * self.last;
        for state in &mut self.state {
            let y = self.coefficient * x + *state;
            *state = x - self.coefficient * y;
            x = y;
        }
        self.last = x;
        x
    }
}

pub struct Phaser {
    rate: f32,
    sine: Sine,
    lfo: u32,
    step: u32,
    count: u32,
    depth: f32,
    feedback: f32,
    mix: f32,
    left: Channel,
    right: Channel,
}

impl Phaser {
    pub fn new(sample_rate: u32) -> Self {
        let mut phaser = Phaser { rate: 48000.0, sine: Sine::new(), lfo: 0, step: 0, count: 0, depth: 0.8, feedback: 0.5, mix: 0.0, left: Channel::new(), right: Channel::new() };
        phaser.set_sample_rate(sample_rate);
        phaser.set_speed(0.4);
        phaser
    }

    pub fn set_sample_rate(&mut self, sample_rate: u32) {
        self.rate = sample_rate.clamp(8000, 192000) as f32;
        self.left = Channel::new();
        self.right = Channel::new();
        self.update();
    }

    /// The sweep speed in Hz (0.05 to 10).
    pub fn set_speed(&mut self, hz: f32) {
        self.step = phase_step(hz.clamp(0.05, 10.0), self.rate);
    }

    /// How far the notches sweep, 0..=1.
    pub fn set_depth(&mut self, depth: f32) {
        self.depth = depth.clamp(0.0, 1.0);
    }

    /// Feedback around the chain, 0..=0.95: more makes the notches sharper and the sound more vocal.
    pub fn set_feedback(&mut self, feedback: f32) {
        self.feedback = feedback.clamp(0.0, 0.95);
    }

    /// The phaser's level, 0..=1 (0 is off).
    pub fn set_mix(&mut self, mix: f32) {
        self.mix = mix.clamp(0.0, 1.0);
    }

    pub fn mix(&self) -> f32 {
        self.mix
    }

    pub fn depth(&self) -> f32 {
        self.depth
    }

    pub fn feedback(&self) -> f32 {
        self.feedback
    }

    fn update(&mut self) {
        let coefficient = |phase: u32| {
            let sweep = 0.5 + 0.5 * self.sine.at(phase);
            let hz = (LOW_HZ * math::pow10(SWEEP_LOG10 * self.depth * sweep)).min(0.45 * self.rate);
            let angle = core::f32::consts::PI * hz / self.rate;
            let tangent = math::sin(angle) / math::cos(angle);
            (tangent - 1.0) / (tangent + 1.0)
        };
        self.left.coefficient = coefficient(self.lfo);
        self.right.coefficient = coefficient(self.lfo.wrapping_add(1 << 30));
    }

    /// One stereo frame in, one out.
    pub fn process(&mut self, left: f32, right: f32) -> (f32, f32) {
        if self.mix == 0.0 {
            return (left, right);
        }
        self.lfo = self.lfo.wrapping_add(self.step);
        self.count += 1;
        if self.count >= CONTROL {
            self.count = 0;
            self.update();
        }
        let dry = 1.0 - 0.5 * self.mix;
        let wet = 0.5 * self.mix;
        (left * dry + self.left.process(left, self.feedback) * wet, right * dry + self.right.process(right, self.feedback) * wet)
    }
}
