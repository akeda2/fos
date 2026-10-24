//! A divide-down string machine in the manner of the Solina String Ensemble, as a `no_std`, allocation-free, hard-float library.
//! The tape owns the screen, touch and audio device; this crate turns MIDI messages and settings into stereo f32 frames.
//!
//! How it sounds like the original: there is one *top octave* of twelve free-running sawtooth generators (C8-B8 here), and
//! every lower octave of every note is a divide-by-two of the one above it, so all the Cs, all the C#s... of the keyboard are
//! phase-locked to each other and never beat, which is what gives these machines their hollow, locked sound (`saw`). A key does
//! not start an oscillator: all of them run all the time and a key only opens a gain, so a 61-key chord costs three table-free
//! saw reads per key. Each key has three envelopes: strings (slow attack and release, set by the two sliders), brass (a short
//! swell) and piano (a fast attack and a decay while held). The keys' 16', 8' and 4' registers are summed on buses, and the six
//! sections (Violin, Viola, Cello, Bass, Brass, Piano) are different mixes of them through different low-pass filters, each
//! with its own level slider.
//!
//! Chain: keys -> section buses -> section filters and levels -> ensemble chorus -> phaser -> reverb -> volume. The mod wheel
//! (controller 1) steers the phaser speed, the ensemble depth or both (`ModTarget`); controller 64 is the sustain pedal.
#![cfg_attr(not(test), no_std)]

use dsp::math;
use fx::{Ensemble, Phaser, Reverb};

pub const FIRST_KEY: u8 = 36;
pub const KEYS: usize = 61;
pub const SECTIONS: usize = 6;
pub const SECTION_NAMES: [&str; SECTIONS] = ["Violin", "Viola", "Cello", "Bass", "Brass", "Piano"];
/// MIDI note of the top octave's C: the octave that is generated; every other note is divided down from it.
const TOP: i32 = 108;
/// Low-pass cutoff of each section in Hz.
const CUTOFF: [f32; SECTIONS] = [6000.0, 3000.0, 1500.0, 500.0, 2500.0, 3500.0];
/// Gain per unit of saw amplitude: leaves room for a chord of several keys on several sections.
const HEADROOM: f32 = 0.04;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModTarget {
    Phaser,
    Ensemble,
    Both,
}

impl ModTarget {
    pub fn name(self) -> &'static str {
        ["Phaser", "Ensemble", "Both"][self as usize]
    }
}

pub struct Strings {
    rate: f32,
    counter: [u64; 12],
    step: [u32; 12],
    held: [bool; KEYS],
    sustained: [bool; KEYS],
    pedal: bool,
    strings_env: [f32; KEYS],
    brass_env: [f32; KEYS],
    piano_env: [f32; KEYS],
    piano_amp: [f32; KEYS],
    level: [f32; SECTIONS],
    attack: f32,
    release: f32,
    strings_attack: f32,
    strings_release: f32,
    brass_attack: f32,
    brass_release: f32,
    piano_attack: f32,
    piano_decay: f32,
    piano_release: f32,
    pole: [f32; SECTIONS],
    low: [[f32; 2]; SECTIONS],
    ensemble: Ensemble,
    phaser: Phaser,
    reverb: Reverb,
    volume: f32,
    mod_wheel: f32,
    mod_target: ModTarget,
    ensemble_mix: f32,
    phaser_mix: f32,
    phaser_speed: f32,
    ensemble_depth: f32,
}

fn time_constant(seconds: f32, rate: f32) -> f32 {
    1.0 - math::exp64(-1.0 / (seconds as f64 * rate as f64)) as f32
}

impl Strings {
    /// A silent machine at `sample_rate` Hz: strings sections on, brass and piano off, ensemble and a little reverb.
    pub fn new(sample_rate: u32) -> Self {
        let mut strings = Strings {
            rate: 48000.0,
            counter: [0; 12],
            step: [0; 12],
            held: [false; KEYS],
            sustained: [false; KEYS],
            pedal: false,
            strings_env: [0.0; KEYS],
            brass_env: [0.0; KEYS],
            piano_env: [0.0; KEYS],
            piano_amp: [0.0; KEYS],
            level: [0.8, 0.6, 0.6, 0.0, 0.0, 0.0],
            attack: 0.3,
            release: 0.4,
            strings_attack: 0.0,
            strings_release: 0.0,
            brass_attack: 0.0,
            brass_release: 0.0,
            piano_attack: 0.0,
            piano_decay: 0.0,
            piano_release: 0.0,
            pole: [0.0; SECTIONS],
            low: [[0.0; 2]; SECTIONS],
            ensemble: Ensemble::new(sample_rate),
            phaser: Phaser::new(sample_rate),
            reverb: Reverb::new(),
            volume: 0.7,
            mod_wheel: 0.35,
            mod_target: ModTarget::Both,
            ensemble_mix: 0.7,
            phaser_mix: 0.0,
            phaser_speed: 0.4,
            ensemble_depth: 0.7,
        };
        strings.set_sample_rate(sample_rate);
        strings.set_reverb(0.25);
        strings
    }

    pub fn set_sample_rate(&mut self, sample_rate: u32) {
        let rate = sample_rate.clamp(8000, 192000) as f32;
        self.rate = rate;
        for pc in 0..12 {
            let note = (TOP + pc as i32 - 69) as f64;
            let hz = 440.0 * math::pow10_64(note / 12.0 * core::f64::consts::LOG10_2);
            self.step[pc] = (hz / rate as f64 * 4294967296.0) as u32;
        }
        for (pole, cutoff) in self.pole.iter_mut().zip(CUTOFF) {
            *pole = 1.0 - math::exp64(-2.0 * core::f64::consts::PI * cutoff as f64 / rate as f64) as f32;
        }
        self.brass_attack = time_constant(0.06, rate);
        self.brass_release = time_constant(0.12, rate);
        self.piano_attack = time_constant(0.002, rate);
        // The piano's amplitude falls by 60 dB in 3 s while held and in 0.25 s after release.
        self.piano_decay = math::exp64(-6.9078 / (3.0 * rate as f64)) as f32;
        self.piano_release = math::exp64(-6.9078 / (0.25 * rate as f64)) as f32;
        self.ensemble.set_sample_rate(sample_rate);
        self.phaser.set_sample_rate(sample_rate);
        self.reverb.set_sample_rate(rate);
        self.update_times();
        self.update_effects();
    }

    fn update_times(&mut self) {
        self.strings_attack = time_constant(0.01 * math::pow10(2.0 * self.attack), self.rate);
        self.strings_release = time_constant(0.05 * math::pow10(1.8 * self.release), self.rate);
    }

    fn update_effects(&mut self) {
        let wheel = self.mod_wheel;
        self.phaser_speed = if self.mod_target != ModTarget::Ensemble { 0.25 * math::pow10(1.3802 * wheel) } else { 0.4 };
        self.ensemble_depth = if self.mod_target != ModTarget::Phaser { 0.4 + 0.6 * wheel } else { 0.8 };
        self.phaser.set_speed(self.phaser_speed);
        self.phaser.set_depth(0.8);
        self.phaser.set_feedback(0.5);
        self.phaser.set_mix(self.phaser_mix);
        self.ensemble.set_depth(self.ensemble_depth);
        self.ensemble.set_mix(self.ensemble_mix);
    }

    /// The sawtooth, -1..1, of MIDI note `note` (24..=108), divided down from the top octave's generator of its pitch class.
    fn saw(&self, note: i32) -> f32 {
        let pc = note.rem_euclid(12) as usize;
        let octaves = (TOP + pc as i32 - note) / 12;
        let phase = (self.counter[pc] >> octaves) as u32;
        (phase >> 8) as f32 * (2.0 / 16777216.0) - 1.0
    }

    pub fn note_on(&mut self, note: u8) {
        if let Some(key) = Self::key(note) {
            self.held[key] = true;
            self.sustained[key] = false;
            self.piano_amp[key] = 1.0;
        }
    }

    pub fn note_off(&mut self, note: u8) {
        if let Some(key) = Self::key(note) {
            if self.held[key] && self.pedal {
                self.sustained[key] = true;
            }
            self.held[key] = false;
        }
    }

    fn key(note: u8) -> Option<usize> {
        (FIRST_KEY..FIRST_KEY + KEYS as u8).contains(&note).then(|| (note - FIRST_KEY) as usize)
    }

    pub fn is_held(&self, note: u8) -> bool {
        Self::key(note).is_some_and(|key| self.held[key])
    }

    pub fn held_count(&self) -> usize {
        self.held.iter().filter(|held| **held).count()
    }

    pub fn all_notes_off(&mut self) {
        self.held = [false; KEYS];
        self.sustained = [false; KEYS];
    }

    /// The sustain pedal: while down, released keys keep sounding; lifting it lets them go.
    pub fn set_sustain(&mut self, down: bool) {
        self.pedal = down;
        if !down {
            self.sustained = [false; KEYS];
        }
    }

    pub fn sustain(&self) -> bool {
        self.pedal
    }

    /// Handles one MIDI message: note on/off, controller 1 (mod wheel), 64 (sustain), 120 and 123 (all notes off).
    /// Returns whether it was one of those.
    pub fn midi(&mut self, message: &[u8]) -> bool {
        let [status, data1, data2] = match *message {
            [status, data1, data2] => [status, data1, data2],
            [status, data1] => [status, data1, 0],
            _ => return false,
        };
        match status & 0xf0 {
            0x90 if data2 > 0 => self.note_on(data1),
            0x90 | 0x80 => self.note_off(data1),
            0xb0 => match data1 {
                1 => self.set_mod_wheel(data2 as f32 / 127.0),
                64 => self.set_sustain(data2 >= 64),
                120 | 123 => self.all_notes_off(),
                _ => return false,
            },
            _ => return false,
        }
        true
    }

    pub fn level(&self, section: usize) -> f32 {
        self.level[section.min(SECTIONS - 1)]
    }

    pub fn set_level(&mut self, section: usize, level: f32) {
        self.level[section.min(SECTIONS - 1)] = level.clamp(0.0, 1.0);
    }

    /// Strings attack, 0..=1: 10 ms to 1 s (exponential).
    pub fn set_attack(&mut self, attack: f32) {
        self.attack = attack.clamp(0.0, 1.0);
        self.update_times();
    }

    pub fn attack(&self) -> f32 {
        self.attack
    }

    /// Strings release, 0..=1: 50 ms to 3 s (exponential).
    pub fn set_release(&mut self, release: f32) {
        self.release = release.clamp(0.0, 1.0);
        self.update_times();
    }

    pub fn release(&self) -> f32 {
        self.release
    }

    pub fn set_ensemble(&mut self, mix: f32) {
        self.ensemble_mix = mix.clamp(0.0, 1.0);
        self.update_effects();
    }

    pub fn ensemble(&self) -> f32 {
        self.ensemble_mix
    }

    pub fn set_phaser(&mut self, mix: f32) {
        self.phaser_mix = mix.clamp(0.0, 1.0);
        self.update_effects();
    }

    pub fn phaser(&self) -> f32 {
        self.phaser_mix
    }

    pub fn set_reverb(&mut self, mix: f32) {
        self.reverb.set_mix(mix);
    }

    pub fn reverb(&self) -> f32 {
        self.reverb.mix()
    }

    pub fn set_volume(&mut self, volume: f32) {
        self.volume = volume.clamp(0.0, 1.0);
    }

    pub fn volume(&self) -> f32 {
        self.volume
    }

    /// The mod wheel, 0..=1: phaser speed 0.25-6 Hz and/or ensemble depth 0.4-1, as `mod_target` says.
    pub fn set_mod_wheel(&mut self, wheel: f32) {
        self.mod_wheel = wheel.clamp(0.0, 1.0);
        self.update_effects();
    }

    pub fn mod_wheel(&self) -> f32 {
        self.mod_wheel
    }

    pub fn set_mod_target(&mut self, target: ModTarget) {
        self.mod_target = target;
        self.update_effects();
    }

    pub fn mod_target(&self) -> ModTarget {
        self.mod_target
    }

    /// The phaser's sweep speed in Hz now.
    pub fn phaser_speed(&self) -> f32 {
        self.phaser_speed
    }

    /// The ensemble's depth now, 0..=1.
    pub fn ensemble_depth(&self) -> f32 {
        self.ensemble_depth
    }

    /// Renders `frames.len()` stereo frames (left, right), each within -1..=1.
    pub fn render(&mut self, frames: &mut [[f32; 2]]) {
        for frame in frames.iter_mut() {
            for pc in 0..12 {
                self.counter[pc] = self.counter[pc].wrapping_add(self.step[pc] as u64);
            }
            // Buses: strings 16'/8'/4', brass 8'/4', piano 8'/4'.
            let mut bus = [0.0f32; 7];
            for key in 0..KEYS {
                let gate = self.held[key] || self.sustained[key];
                let target = if gate { 1.0 } else { 0.0 };
                if !gate && self.strings_env[key] < 1e-5 && self.brass_env[key] < 1e-5 && self.piano_env[key] < 1e-5 {
                    self.strings_env[key] = 0.0;
                    self.brass_env[key] = 0.0;
                    self.piano_env[key] = 0.0;
                    continue;
                }
                self.strings_env[key] += (target - self.strings_env[key]) * if gate { self.strings_attack } else { self.strings_release };
                self.brass_env[key] += (target - self.brass_env[key]) * if gate { self.brass_attack } else { self.brass_release };
                self.piano_amp[key] *= if gate { self.piano_decay } else { self.piano_release };
                self.piano_env[key] += (if gate || self.piano_amp[key] > 0.0 { self.piano_amp[key] } else { 0.0 } - self.piano_env[key]) * self.piano_attack;
                let note = FIRST_KEY as i32 + key as i32;
                // The 4' of the top keys would alias badly: fade it out over the top octave.
                let fade = ((97 - note) as f32 / 13.0).clamp(0.0, 1.0);
                let (low, mid, high) = (self.saw(note - 12), self.saw(note), self.saw(note + 12) * fade);
                let (s, b, p) = (self.strings_env[key], self.brass_env[key], self.piano_env[key]);
                bus[0] += s * low;
                bus[1] += s * mid;
                bus[2] += s * high;
                bus[3] += b * mid;
                bus[4] += b * high;
                bus[5] += p * mid;
                bus[6] += p * high;
            }
            let input = [
                0.6 * bus[1] + 0.8 * bus[2],
                bus[1],
                0.7 * bus[0] + 0.7 * bus[1],
                bus[0],
                0.7 * bus[3] + 0.5 * bus[4],
                0.7 * bus[5] + 0.4 * bus[6],
            ];
            let mut mono = 0.0;
            for section in 0..SECTIONS {
                let [first, second] = &mut self.low[section];
                *first += (input[section] - *first) * self.pole[section];
                *second += (*first - *second) * self.pole[section];
                mono += *second * self.level[section];
            }
            let (left, right) = self.ensemble.process(mono * HEADROOM);
            let (left, right) = self.phaser.process(left, right);
            let (left, right) = self.reverb.process(left, right);
            frame[0] = (left * self.volume).clamp(-1.0, 1.0);
            frame[1] = (right * self.volume).clamp(-1.0, 1.0);
        }
    }
}

#[cfg(test)]
mod tests;
