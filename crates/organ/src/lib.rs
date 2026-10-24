//! A drawbar organ engine in the Hammond B-3 mould, as a `no_std`, allocation-free, hard-float library (`dsp::math` for the
//! few elementary functions; build the tape for `x86_64-fos-tape-fp`). The tape owns the screen, the touch panel and the audio
//! device; this crate turns MIDI messages and control settings into stereo f32 frames.
//!
//! How it sounds like the original: there are 91 "tone wheels", one sine per semitone from C1 (wheel 1, 32.7 Hz) up. A key
//! never has an oscillator of its own: each of the nine drawbars takes the sine of a fixed wheel at a fixed interval above the
//! key (16' is an octave below, 5 1/3' a fifth above, 8' the key itself, then 4', 2 2/3', 2', 1 3/5', 1 1/3', 1'). Wheels
//! above the 91st fold back an octave, as the real instrument does. So a chord costs at most 91 sine lookups per sample,
//! however many keys are down: the keys only add to per-wheel gains, which are smoothed over a few milliseconds so a key does
//! not click by accident; the deliberate key click is added separately.
//!
//! Chain: wheels -> drawbar mix (+ percussion, + key click) -> scanner vibrato/chorus -> overdrive -> Leslie (stereo) -> volume.
#![cfg_attr(not(test), no_std)]

use dsp::math;

pub const FIRST_KEY: u8 = 36;
pub const KEYS: usize = 61;
pub const DRAWBARS: usize = 9;
pub const WHEELS: usize = 91;
/// The drawbar footages, longest first.
pub const FOOTAGES: [&str; DRAWBARS] = ["16'", "5 1/3'", "8'", "4'", "2 2/3'", "2'", "1 3/5'", "1 1/3'", "1'"];
/// Semitones above the key each drawbar sounds.
const INTERVAL: [i32; DRAWBARS] = [-12, 7, 0, 12, 19, 24, 28, 31, 36];
/// MIDI note of wheel 1 (C1).
const WHEEL_ONE: i32 = 24;
const TABLE: usize = 1024;
/// Gain per unit of drawbar amplitude: leaves room for a six-note chord with a few bars out.
const HEADROOM: f32 = 0.05;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Percussion {
    Off,
    Second,
    Third,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scanner {
    Off,
    V1,
    V2,
    V3,
    C1,
    C2,
    C3,
}

impl Scanner {
    pub fn name(self) -> &'static str {
        ["Off", "V1", "V2", "V3", "C1", "C2", "C3"][self as usize]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Leslie {
    /// No rotary speaker: the same mono signal on both channels.
    Off,
    /// Rotors spun down (they take a while, as the real ones do).
    Stop,
    Slow,
    Fast,
}

/// Drawbar registrations (16' .. 1') worth having a button for.
pub const PRESETS: [(&str, [u8; DRAWBARS]); 6] = [
    ("Full", [8, 8, 8, 8, 8, 8, 8, 8, 8]),
    ("Jazz", [8, 8, 8, 0, 0, 0, 0, 0, 0]),
    ("Gospel", [8, 8, 8, 8, 0, 0, 0, 0, 8]),
    ("Rock", [8, 8, 8, 0, 0, 0, 0, 0, 8]),
    ("Whiter", [0, 0, 8, 6, 8, 4, 0, 0, 0]),
    ("Flute", [0, 0, 8, 0, 4, 0, 0, 0, 0]),
];

/// Amplitude of a drawbar position 0..=8: 3 dB per step, 0 is off.
fn drawbar_amplitude(position: u8) -> f32 {
    if position == 0 { 0.0 } else { math::pow10(-0.15 * (8 - position.min(8)) as f32) }
}

/// The wheel (0-based) a key's drawbar takes, folded back an octave while it would be above the 91st.
fn wheel_for(key: u8, interval: i32) -> usize {
    let mut wheel = key as i32 + interval - WHEEL_ONE;
    while wheel >= WHEELS as i32 {
        wheel -= 12;
    }
    wheel.max(0) as usize
}

/// A delay line read with linear interpolation.
struct Delay<const N: usize> {
    buffer: [f32; N],
    write: usize,
}

impl<const N: usize> Delay<N> {
    const fn new() -> Self {
        Delay { buffer: [0.0; N], write: 0 }
    }

    fn push(&mut self, value: f32) {
        self.buffer[self.write] = value;
        self.write = (self.write + 1) % N;
    }

    /// The sample `delay` samples ago (1.0 is the one pushed last), 1 <= delay < N - 1.
    fn read(&self, delay: f32) -> f32 {
        let whole = delay as usize;
        let fraction = delay - whole as f32;
        let newer = self.buffer[(self.write + N - whole) % N];
        let older = self.buffer[(self.write + N - whole - 1) % N];
        newer + (older - newer) * fraction
    }
}

/// One Leslie rotor (horn or drum): its speed follows the target slowly; it modulates level and delay (Doppler) per ear.
struct Rotor {
    phase: u32,
    speed: f32,
    delay: Delay<64>,
}

impl Rotor {
    const fn new() -> Self {
        Rotor { phase: 0, speed: 0.0, delay: Delay::new() }
    }
}

pub struct Organ {
    sine: [f32; TABLE + 1],
    sample_rate: f32,
    drawbars: [u8; DRAWBARS],
    percussion: Percussion,
    percussion_fast: bool,
    percussion_soft: bool,
    scanner: Scanner,
    leslie: Leslie,
    drive: f32,
    volume: f32,
    click: f32,
    dirty: bool,
    held: [bool; KEYS],
    held_count: usize,
    phase: [u32; WHEELS],
    step: [u32; WHEELS],
    /// Target and smoothed gain of each wheel for the drawbars, and for the percussion harmonic.
    target: [f32; WHEELS],
    gain: [f32; WHEELS],
    percussion_target: [f32; WHEELS],
    percussion_gain: [f32; WHEELS],
    smoothing: f32,
    percussion_env: f32,
    percussion_decay: f32,
    click_env: f32,
    click_decay: f32,
    noise: u32,
    last_noise: f32,
    vibrato_delay: Delay<256>,
    vibrato_phase: u32,
    vibrato_step: u32,
    horn: Rotor,
    drum: Rotor,
    split: f32,
    low: f32,
}

impl Organ {
    /// A silent organ at `sample_rate` Hz with the "Full" registration and nothing else switched on.
    pub fn new(sample_rate: u32) -> Self {
        let mut sine = [0.0; TABLE + 1];
        for (index, value) in sine.iter_mut().enumerate() {
            *value = math::sin(index as f32 * (2.0 * core::f32::consts::PI / TABLE as f32));
        }
        let mut organ = Organ {
            sine,
            sample_rate: 48000.0,
            drawbars: PRESETS[0].1,
            percussion: Percussion::Off,
            percussion_fast: true,
            percussion_soft: false,
            scanner: Scanner::Off,
            leslie: Leslie::Off,
            drive: 0.0,
            volume: 0.7,
            click: 0.3,
            dirty: true,
            held: [false; KEYS],
            held_count: 0,
            phase: [0; WHEELS],
            step: [0; WHEELS],
            target: [0.0; WHEELS],
            gain: [0.0; WHEELS],
            percussion_target: [0.0; WHEELS],
            percussion_gain: [0.0; WHEELS],
            smoothing: 0.0,
            percussion_env: 0.0,
            percussion_decay: 0.0,
            click_env: 0.0,
            click_decay: 0.0,
            noise: 0x1234_5678,
            last_noise: 0.0,
            vibrato_delay: Delay::new(),
            vibrato_phase: 0,
            vibrato_step: 0,
            horn: Rotor::new(),
            drum: Rotor::new(),
            split: 0.0,
            low: 0.0,
        };
        organ.set_sample_rate(sample_rate);
        organ
    }

    pub fn set_sample_rate(&mut self, sample_rate: u32) {
        let rate = sample_rate.clamp(8000, 192000) as f32;
        self.sample_rate = rate;
        for wheel in 0..WHEELS {
            let note = (WHEEL_ONE + wheel as i32 - 69) as f64;
            let hz = 440.0 * math::pow10_64(note / 12.0 * core::f64::consts::LOG10_2);
            self.step[wheel] = (hz / rate as f64 * 4294967296.0) as u32;
        }
        self.smoothing = 1.0 - math::exp64(-1.0 / (0.003 * rate as f64)) as f32;
        self.click_decay = math::exp64(-1.0 / (0.0015 * rate as f64)) as f32;
        self.vibrato_step = (6.1 / rate as f64 * 4294967296.0) as u32;
        self.split = 1.0 - math::exp64(-2.0 * core::f64::consts::PI * 800.0 / rate as f64) as f32;
        self.set_percussion_decay();
    }

    fn set_percussion_decay(&mut self) {
        let seconds = if self.percussion_fast { 0.07 } else { 0.2 };
        self.percussion_decay = math::exp64(-1.0 / (seconds * self.sample_rate as f64)) as f32;
    }

    pub fn drawbars(&self) -> [u8; DRAWBARS] {
        self.drawbars
    }

    /// Sets drawbar `index` (0 = 16' .. 8 = 1') to position 0..=8.
    pub fn set_drawbar(&mut self, index: usize, position: u8) {
        if index < DRAWBARS {
            self.drawbars[index] = position.min(8);
            self.dirty = true;
        }
    }

    pub fn set_drawbars(&mut self, positions: [u8; DRAWBARS]) {
        for (index, &position) in positions.iter().enumerate() {
            self.set_drawbar(index, position);
        }
    }

    pub fn percussion(&self) -> Percussion {
        self.percussion
    }

    pub fn set_percussion(&mut self, percussion: Percussion) {
        self.percussion = percussion;
        self.dirty = true;
    }

    pub fn percussion_fast(&self) -> bool {
        self.percussion_fast
    }

    pub fn set_percussion_fast(&mut self, fast: bool) {
        self.percussion_fast = fast;
        self.set_percussion_decay();
    }

    pub fn percussion_soft(&self) -> bool {
        self.percussion_soft
    }

    pub fn set_percussion_soft(&mut self, soft: bool) {
        self.percussion_soft = soft;
    }

    pub fn scanner(&self) -> Scanner {
        self.scanner
    }

    pub fn set_scanner(&mut self, scanner: Scanner) {
        self.scanner = scanner;
    }

    pub fn leslie(&self) -> Leslie {
        self.leslie
    }

    pub fn set_leslie(&mut self, leslie: Leslie) {
        self.leslie = leslie;
    }

    /// Overdrive amount 0..=1 (0 is clean).
    pub fn set_drive(&mut self, drive: f32) {
        self.drive = drive.clamp(0.0, 1.0);
    }

    pub fn drive(&self) -> f32 {
        self.drive
    }

    /// Master volume 0..=1.
    pub fn set_volume(&mut self, volume: f32) {
        self.volume = volume.clamp(0.0, 1.0);
    }

    pub fn volume(&self) -> f32 {
        self.volume
    }

    /// Key click level 0..=1.
    pub fn set_click(&mut self, click: f32) {
        self.click = click.clamp(0.0, 1.0);
    }

    pub fn click(&self) -> f32 {
        self.click
    }

    /// Whether a key (MIDI note 36..=96) is down.
    pub fn is_held(&self, note: u8) -> bool {
        note.checked_sub(FIRST_KEY).and_then(|index| self.held.get(index as usize)).copied().unwrap_or(false)
    }

    pub fn held_count(&self) -> usize {
        self.held_count
    }

    pub fn note_on(&mut self, note: u8) {
        let Some(index) = note.checked_sub(FIRST_KEY).map(usize::from).filter(|&index| index < KEYS) else { return };
        if self.held[index] {
            return;
        }
        // The percussion fires only for the first key of a phrase, as on the original.
        if self.held_count == 0 {
            self.percussion_env = 1.0;
        }
        self.click_env = (self.click_env + 0.7).min(1.0);
        self.held[index] = true;
        self.held_count += 1;
        self.dirty = true;
    }

    pub fn note_off(&mut self, note: u8) {
        let Some(index) = note.checked_sub(FIRST_KEY).map(usize::from).filter(|&index| index < KEYS) else { return };
        if self.held[index] {
            self.held[index] = false;
            self.held_count -= 1;
            self.dirty = true;
        }
    }

    pub fn all_notes_off(&mut self) {
        self.held = [false; KEYS];
        self.held_count = 0;
        self.dirty = true;
    }

    /// Feeds one MIDI channel message (1-3 bytes; every channel is the same keyboard): note on/off, and "all notes off"
    /// (controllers 120 and 123). Returns whether it was one of those.
    pub fn midi(&mut self, message: &[u8]) -> bool {
        let [status, data1, data2] = match *message {
            [status, data1, data2] => [status, data1, data2],
            [status, data1] => [status, data1, 0],
            _ => return false,
        };
        match status & 0xf0 {
            0x90 if data2 > 0 => self.note_on(data1),
            0x90 | 0x80 => self.note_off(data1),
            0xb0 if data1 == 123 || data1 == 120 => self.all_notes_off(),
            _ => return false,
        }
        true
    }

    /// Rebuilds the per-wheel target gains from the held keys and the drawbars.
    fn retarget(&mut self) {
        self.target = [0.0; WHEELS];
        self.percussion_target = [0.0; WHEELS];
        let amplitudes: [f32; DRAWBARS] = core::array::from_fn(|index| {
            // Percussion takes over the 1' drawbar's job on the real instrument.
            if index == 8 && self.percussion != Percussion::Off { 0.0 } else { drawbar_amplitude(self.drawbars[index]) }
        });
        let harmonic = match self.percussion {
            Percussion::Off => None,
            Percussion::Second => Some(12),
            Percussion::Third => Some(19),
        };
        for (index, _) in self.held.iter().enumerate().filter(|(_, held)| **held) {
            let key = FIRST_KEY + index as u8;
            for drawbar in 0..DRAWBARS {
                self.target[wheel_for(key, INTERVAL[drawbar])] += amplitudes[drawbar];
            }
            if let Some(interval) = harmonic {
                self.percussion_target[wheel_for(key, interval)] += 1.0;
            }
        }
        self.dirty = false;
    }

    /// One Leslie rotor's two ear signals: level follows the rotor's facing, the delay swings by `swing` samples (Doppler).
    #[allow(clippy::too_many_arguments)]
    fn rotor(rotor: &mut Rotor, input: f32, target: f32, follow: f32, rate: f32, swing: f32, depth: f32, sine: &[f32; TABLE + 1]) -> (f32, f32) {
        rotor.speed += (target - rotor.speed) * follow;
        rotor.phase = rotor.phase.wrapping_add((rotor.speed * (4294967296.0 / rate)) as u32);
        let table = |phase: u32| {
            let index = (phase >> 22) as usize;
            let fraction = (phase & 0x003f_ffff) as f32 * (1.0 / 4194304.0);
            sine[index] + (sine[index + 1] - sine[index]) * fraction
        };
        rotor.delay.push(input);
        let delayed = rotor.delay.read(swing + 1.0 + swing * table(rotor.phase));
        let facing = table(rotor.phase.wrapping_add(1 << 30));
        (delayed * 0.7 * (1.0 + depth * facing), delayed * 0.7 * (1.0 - depth * facing))
    }

    /// Renders `frames.len()` stereo frames (left, right), each within -1..=1.
    pub fn render(&mut self, frames: &mut [[f32; 2]]) {
        if self.dirty {
            self.retarget();
        }
        let percussion_level = if self.percussion_soft { 0.6 } else { 1.5 };
        let scale = self.sample_rate / 48000.0;
        let (vibrato_depth, chorus) = match self.scanner {
            Scanner::Off => (0.0, false),
            Scanner::V1 | Scanner::C1 => (1.3 * scale, self.scanner == Scanner::C1),
            Scanner::V2 | Scanner::C2 => (2.5 * scale, self.scanner == Scanner::C2),
            Scanner::V3 | Scanner::C3 => (4.0 * scale, self.scanner == Scanner::C3),
        };
        let (horn_target, drum_target) = match self.leslie {
            Leslie::Off | Leslie::Stop => (0.0, 0.0),
            Leslie::Slow => (0.8, 0.66),
            Leslie::Fast => (6.7, 5.7),
        };
        // Time constants of the rotors' inertia: the horn is light, the drum heavy.
        let horn_follow = 1.0 / (0.7 * self.sample_rate);
        let drum_follow = 1.0 / (3.0 * self.sample_rate);
        let swing = scale;
        let drive_gain = 1.0 + self.drive * 7.0;
        for frame in frames.iter_mut() {
            let mut main = 0.0;
            let mut percussion = 0.0;
            for wheel in 0..WHEELS {
                self.phase[wheel] = self.phase[wheel].wrapping_add(self.step[wheel]);
                self.gain[wheel] += (self.target[wheel] - self.gain[wheel]) * self.smoothing;
                self.percussion_gain[wheel] += (self.percussion_target[wheel] - self.percussion_gain[wheel]) * self.smoothing;
                if self.gain[wheel] < 1e-6 && self.percussion_gain[wheel] < 1e-6 && self.target[wheel] == 0.0 && self.percussion_target[wheel] == 0.0 {
                    self.gain[wheel] = 0.0;
                    self.percussion_gain[wheel] = 0.0;
                    continue;
                }
                let phase = self.phase[wheel];
                let index = (phase >> 22) as usize;
                let low = self.sine[index];
                let value = low + (self.sine[index + 1] - low) * ((phase & 0x003f_ffff) as f32 * (1.0 / 4194304.0));
                main += self.gain[wheel] * value;
                percussion += self.percussion_gain[wheel] * value;
            }
            self.percussion_env *= self.percussion_decay;
            let mut sample = (main + percussion * self.percussion_env * percussion_level) * HEADROOM;
            // Key click: differentiated noise, so it is all treble, under a very short envelope.
            self.noise = self.noise.wrapping_mul(1664525).wrapping_add(1013904223);
            let noise = (self.noise >> 8) as f32 * (2.0 / 16777216.0) - 1.0;
            sample += (noise - self.last_noise) * 0.075 * self.click_env * self.click;
            self.last_noise = noise;
            self.click_env *= self.click_decay;

            if vibrato_depth > 0.0 {
                self.vibrato_phase = self.vibrato_phase.wrapping_add(self.vibrato_step);
                self.vibrato_delay.push(sample);
                let phase = self.vibrato_phase;
                let index = (phase >> 22) as usize;
                let wobble = self.sine[index] + (self.sine[index + 1] - self.sine[index]) * ((phase & 0x003f_ffff) as f32 * (1.0 / 4194304.0));
                let delayed = self.vibrato_delay.read(vibrato_depth + 1.0 + vibrato_depth * wobble);
                sample = if chorus { 0.5 * (sample + delayed) } else { delayed };
            }

            if self.drive > 0.0 {
                let x = sample * drive_gain;
                sample = x / (1.0 + x.abs()) * (1.0 + self.drive);
            }

            let (left, right) = if self.leslie == Leslie::Off {
                (sample, sample)
            } else {
                self.low += (sample - self.low) * self.split;
                let (low, high) = (self.low, sample - self.low);
                let (horn_left, horn_right) = Self::rotor(&mut self.horn, high, horn_target, horn_follow, self.sample_rate, 6.0 * swing, 0.6, &self.sine);
                let (drum_left, drum_right) = Self::rotor(&mut self.drum, low, drum_target, drum_follow, self.sample_rate, 3.0 * swing, 0.35, &self.sine);
                (horn_left + drum_left, horn_right + drum_right)
            };
            frame[0] = (left * self.volume).clamp(-1.0, 1.0);
            frame[1] = (right * self.volume).clamp(-1.0, 1.0);
        }
    }
}

#[cfg(test)]
mod tests;
