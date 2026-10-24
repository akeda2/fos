//! Audio effects for the instrument tapes, `no_std`, allocation-free and hard-float (`dsp::math` for the few elementary
//! functions): a rotary speaker, a stereo reverb, a spring reverb, a three-voice ensemble chorus, a phaser and an overdrive. Each effect is a
//! plain struct that owns its delay lines, is made for a sample rate, and is fed one frame at a time, so an instrument can put
//! them in whatever order it likes (the organ: ... overdrive -> rotary -> reverb; a string machine: ensemble -> phaser ->
//! reverb). Settings are 0..=1 unless a name says otherwise, and every effect with a mix of 0 is a bypass that costs nothing.
#![cfg_attr(not(test), no_std)]

use dsp::math;

mod ensemble;
mod phaser;
mod reverb;
mod rotary;
mod spring;

pub use ensemble::Ensemble;
pub use phaser::Phaser;
pub use reverb::Reverb;
pub use rotary::{Rotary, RotaryMode};
pub use spring::Spring;

const TABLE: usize = 1024;

/// One period of a sine, read by a 32-bit phase (the top 10 bits index, the rest interpolate): the shared low-frequency
/// oscillator of the effects. A phase advances by `phase_step(hz, rate)` per sample.
pub struct Sine {
    table: [f32; TABLE + 1],
}

impl Sine {
    pub fn new() -> Self {
        let mut table = [0.0; TABLE + 1];
        for (index, value) in table.iter_mut().enumerate() {
            *value = math::sin(index as f32 * (2.0 * core::f32::consts::PI / TABLE as f32));
        }
        Sine { table }
    }

    #[inline]
    pub fn at(&self, phase: u32) -> f32 {
        let index = (phase >> 22) as usize;
        let fraction = (phase & 0x003f_ffff) as f32 * (1.0 / 4194304.0);
        self.table[index] + (self.table[index + 1] - self.table[index]) * fraction
    }
}

impl Default for Sine {
    fn default() -> Self {
        Self::new()
    }
}

/// The phase increment per sample of an oscillator at `hz`.
pub fn phase_step(hz: f32, rate: f32) -> u32 {
    (hz as f64 / rate as f64 * 4294967296.0) as u32
}

/// A delay line read with linear interpolation.
pub struct Delay<const N: usize> {
    buffer: [f32; N],
    write: usize,
}

impl<const N: usize> Delay<N> {
    pub const fn new() -> Self {
        Delay { buffer: [0.0; N], write: 0 }
    }

    pub fn push(&mut self, value: f32) {
        self.buffer[self.write] = value;
        self.write = (self.write + 1) % N;
    }

    /// The sample `delay` samples ago (1.0 is the one pushed last), 1 <= delay < N - 1.
    pub fn read(&self, delay: f32) -> f32 {
        let whole = delay as usize;
        let fraction = delay - whole as f32;
        let newer = self.buffer[(self.write + N - whole) % N];
        let older = self.buffer[(self.write + N - whole - 1) % N];
        newer + (older - newer) * fraction
    }

    pub fn clear(&mut self) {
        self.buffer = [0.0; N];
    }
}

impl<const N: usize> Default for Delay<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// Soft-clipping overdrive: `amount` 0 is clean, 1 pushes the signal 18 dB into an `x / (1 + |x|)` curve. The output level
/// rises a little with the amount so it does not sound quieter when it saturates.
pub fn overdrive(sample: f32, amount: f32) -> f32 {
    if amount <= 0.0 {
        return sample;
    }
    let x = sample * (1.0 + amount * 7.0);
    x / (1.0 + x.abs()) * (1.0 + amount)
}

#[cfg(test)]
mod tests;
