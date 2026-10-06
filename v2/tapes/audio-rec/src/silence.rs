//! The trigger that starts a take on sound and ends it on silence.
//!
//! It is fed the loudest sample magnitude of each block of audio (any channel, or a chosen few) and the block's
//! length in frames, and keeps two timers: how long the level has been above the threshold and how long below. A
//! take starts when the level has been above for `sound_ms` and stops when it has been below for `silence_ms`. With
//! pre-roll the start can be late by `sound_ms` without losing anything, so `sound_ms` can be long enough not to be
//! fooled by a click. Magnitudes use the meter's scale (2^31 = full scale), so the threshold is in dBFS.

use audio_wire::meter;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    /// Below this level (dBFS tenths, e.g. -500 for -50 dB) is silence.
    pub threshold_tenths: i32,
    /// How long the level must stay above the threshold to count as sound.
    pub sound_ms: u32,
    /// How long it must stay below it to count as silence.
    pub silence_ms: u32,
}

impl Config {
    pub const DEFAULT: Config = Config { threshold_tenths: -500, sound_ms: 100, silence_ms: 5_000 };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    None,
    /// Sound has lasted `sound_ms`: start a take.
    Sound,
    /// Silence has lasted `silence_ms`: end it.
    Silence,
}

pub struct Detector {
    config: Config,
    rate: u32,
    /// Whether the last event reported was `Sound` (so a `Silence` can only follow it, and vice versa).
    sounding: bool,
    above_frames: u64,
    below_frames: u64,
}

impl Detector {
    pub fn new(config: Config, rate: u32) -> Detector {
        Detector { config, rate: rate.max(1), sounding: false, above_frames: 0, below_frames: 0 }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn set_config(&mut self, config: Config) {
        self.config = config;
    }

    pub fn sounding(&self) -> bool {
        self.sounding
    }

    /// Forgets the timers and starts from silence.
    pub fn reset(&mut self) {
        self.sounding = false;
        self.above_frames = 0;
        self.below_frames = 0;
    }

    /// Tells the detector the take was started or stopped by other means (the operator), so that the next event is
    /// the opposite one.
    pub fn force(&mut self, sounding: bool) {
        self.sounding = sounding;
        self.above_frames = 0;
        self.below_frames = 0;
    }

    fn frames(&self, ms: u32) -> u64 {
        u64::from(ms) * u64::from(self.rate) / 1000
    }

    /// Feeds one block: its peak magnitude and its length in frames.
    pub fn feed(&mut self, peak: u32, frames: u32) -> Event {
        let loud = meter::db_tenths(peak) > self.config.threshold_tenths;
        let frames = u64::from(frames);
        if loud {
            self.above_frames += frames;
            self.below_frames = 0;
        } else {
            self.below_frames += frames;
            self.above_frames = 0;
        }
        if !self.sounding && self.above_frames >= self.frames(self.config.sound_ms).max(1) {
            self.sounding = true;
            self.above_frames = 0;
            return Event::Sound;
        }
        if self.sounding && self.below_frames >= self.frames(self.config.silence_ms).max(1) {
            self.sounding = false;
            self.below_frames = 0;
            return Event::Silence;
        }
        Event::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOUD: u32 = 0x1000_0000; // -18 dBFS
    const QUIET: u32 = 0x0001_0000; // about -90 dBFS

    #[test]
    fn starts_after_sound_and_stops_after_silence() {
        let config = Config { threshold_tenths: -500, sound_ms: 100, silence_ms: 1000 };
        let mut detector = Detector::new(config, 48_000);
        let block = 480; // 10 ms
        for _ in 0..50 {
            assert_eq!(detector.feed(QUIET, block), Event::None);
        }
        for step in 0..10 {
            let event = detector.feed(LOUD, block);
            assert_eq!(event, if step == 9 { Event::Sound } else { Event::None });
        }
        assert!(detector.sounding());
        // A short gap does not end it.
        for _ in 0..50 {
            assert_eq!(detector.feed(QUIET, block), Event::None);
        }
        assert_eq!(detector.feed(LOUD, block), Event::None);
        for step in 0..100 {
            let event = detector.feed(QUIET, block);
            assert_eq!(event, if step == 99 { Event::Silence } else { Event::None });
        }
        assert!(!detector.sounding());
    }

    #[test]
    fn a_click_is_not_sound() {
        let mut detector = Detector::new(Config { threshold_tenths: -500, sound_ms: 100, silence_ms: 1000 }, 48_000);
        for _ in 0..20 {
            assert_eq!(detector.feed(LOUD, 48), Event::None); // 1 ms
            for _ in 0..10 {
                assert_eq!(detector.feed(QUIET, 480), Event::None);
            }
        }
    }

    #[test]
    fn forced_state_ends_with_silence() {
        let mut detector = Detector::new(Config { threshold_tenths: -500, sound_ms: 100, silence_ms: 200 }, 48_000);
        detector.force(true);
        assert_eq!(detector.feed(QUIET, 4800), Event::None);
        assert_eq!(detector.feed(QUIET, 4800), Event::Silence);
    }
}
