//! A three-voice ensemble chorus in the manner of the string machines (Solina, Roland RS): the mono input is delayed by three
//! taps of one delay line whose delay times are swept by slow and fast oscillators 120 degrees apart, so the voices never line
//! up and the sound thickens and shimmers without a recognisable vibrato. The taps are spread over the stereo field.

use crate::{Delay, Sine, phase_step};

const LINE: usize = 4096;
/// Where the voices sit in the stereo field (left weight; the right is the mirror image).
const PAN: [f32; 3] = [0.8, 0.5, 0.2];

pub struct Ensemble {
    sine: Sine,
    line: Delay<LINE>,
    centre: f32,
    swing: f32,
    slow: u32,
    fast: u32,
    slow_step: u32,
    fast_step: u32,
    depth: f32,
    mix: f32,
}

impl Ensemble {
    pub fn new(sample_rate: u32) -> Self {
        let mut ensemble = Ensemble { sine: Sine::new(), line: Delay::new(), centre: 0.0, swing: 0.0, slow: 0, fast: 0, slow_step: 0, fast_step: 0, depth: 1.0, mix: 0.0 };
        ensemble.set_sample_rate(sample_rate);
        ensemble
    }

    pub fn set_sample_rate(&mut self, sample_rate: u32) {
        let rate = sample_rate.clamp(8000, 192000) as f32;
        self.centre = 0.007 * rate;
        self.swing = 0.0025 * rate;
        self.slow_step = phase_step(0.55, rate);
        self.fast_step = phase_step(6.3, rate);
        self.line.clear();
    }

    /// How far the delay times swing, 0..=1 (1 is about +-2.5 ms from the 7 ms centre).
    pub fn set_depth(&mut self, depth: f32) {
        self.depth = depth.clamp(0.0, 1.0);
    }

    /// The ensemble's level, 0..=1 (0 is off).
    pub fn set_mix(&mut self, mix: f32) {
        self.mix = mix.clamp(0.0, 1.0);
    }

    pub fn mix(&self) -> f32 {
        self.mix
    }

    pub fn depth(&self) -> f32 {
        self.depth
    }

    /// One mono sample in, one stereo frame out.
    pub fn process(&mut self, sample: f32) -> (f32, f32) {
        if self.mix == 0.0 {
            return (sample, sample);
        }
        self.line.push(sample);
        self.slow = self.slow.wrapping_add(self.slow_step);
        self.fast = self.fast.wrapping_add(self.fast_step);
        let (mut left, mut right) = (0.0, 0.0);
        for (voice, pan) in PAN.iter().enumerate() {
            let offset = (voice as u32).wrapping_mul(u32::MAX / 3);
            let wobble = 0.8 * self.sine.at(self.slow.wrapping_add(offset)) + 0.2 * self.sine.at(self.fast.wrapping_add(offset));
            let delay = (self.centre + self.swing * self.depth * wobble).clamp(1.0, (LINE - 2) as f32);
            let tap = self.line.read(delay);
            left += tap * pan;
            right += tap * (1.0 - pan);
        }
        let dry = 1.0 - 0.5 * self.mix;
        let wet = 0.4 * self.mix;
        (sample * dry + left * wet, sample * dry + right * wet)
    }
}
