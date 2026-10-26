//! Effects a player (or an effects tape) puts between the decoder and the device. They know nothing of the tape ABI:
//! frames in, frames out, one instance per stream. `no_std`, allocation-free.
//!
//! `Options` holds the settings and parses the `key=value` words of a command line, so every tape that offers these
//! effects spells them the same way.
#![cfg_attr(not(test), no_std)]

pub mod upmix;

pub use phono::BassBoost;
pub use upmix::{Upmix, UpmixSettings};
use phono::{Phono, curves};

/// Settings of the output effects; `Options::OFF` changes nothing.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Options {
    pub bass: BassBoost,
    /// 2 to 5.1 (`upmix=on`).
    pub upmix: bool,
    pub surround: UpmixSettings,
}

impl Options {
    pub const OFF: Options = Options { bass: BassBoost::OFF, upmix: false, surround: UpmixSettings::DEFAULT };

    /// Takes one `key=value` word: `bass=DB` (0-12), `bassfreq=HZ` (60-150), `bassq=Q` (0.3-3), `upmix=on|off`, `center=on|off`,
    /// `width=0..1`, `lfe=HZ` (40-200), `delay=MS` (0-40). `Ok(false)` when the key is not
    /// one of ours (the caller may know it), `Err` says what was wrong with the value.
    pub fn set(&mut self, key: &str, value: &str, parse: impl Fn(&str) -> Option<f32>) -> Result<bool, &'static str> {
        let number = |min: f32, max: f32| parse(value).filter(|v| *v >= min && *v <= max);
        match key {
            "bass" => self.bass.gain_db = number(0.0, 12.0).ok_or("bass= needs a boost in dB, 0 to 12")?,
            "bassfreq" => self.bass.hz = number(60.0, 150.0).ok_or("bassfreq= needs a frequency, 60 to 150 Hz")?,
            "bassq" => self.bass.q = number(0.3, 3.0).ok_or("bassq= needs a width, 0.3 to 3")?,
            "upmix" => self.upmix = switch(value).ok_or("upmix= needs on or off")?,
            "center" => self.surround.center = switch(value).ok_or("center= needs on or off")?,
            "width" => self.surround.width = number(0.0, 1.0).ok_or("width= needs the surround level, 0 to 1")?,
            "lfe" => self.surround.lfe_hz = number(40.0, 200.0).ok_or("lfe= needs a crossover frequency, 40 to 200 Hz")?,
            "delay" => self.surround.delay_ms = number(0.0, 40.0).ok_or("delay= needs the surround delay in ms, 0 to 40")?,
            _ => return Ok(false),
        }
        Ok(true)
    }

    pub fn is_off(&self) -> bool {
        self.bass.gain_db <= 0.0 && !self.upmix
    }
}

fn switch(text: &str) -> Option<bool> {
    match text {
        "on" | "yes" | "1" => Some(true),
        "off" | "no" | "0" => Some(false),
        _ => None,
    }
}

/// Bass boost: the peaking filter of the phono tape (`phono::Phono` on its flat curve), with the level lowered by the
/// boost so that a full-scale note at the centre frequency cannot clip.
pub struct Bass {
    filter: Phono,
}

impl Bass {
    /// `None` when the boost is 0 dB.
    pub fn new(rate: u32, bass: BassBoost) -> Option<Bass> {
        if bass.gain_db <= 0.0 {
            return None;
        }
        let mut filter = Phono::new(rate);
        filter.set_curve(curves::FLAT);
        filter.set_bass_boost(bass);
        filter.set_trim_db(-bass.gain_db);
        Some(Bass { filter })
    }

    /// A new boost without clearing the filter's state (a touch slider turned while the music plays); the caller makes sure
    /// `bass.gain_db` is above 0 (`new` says `None` otherwise).
    pub fn set(&mut self, bass: BassBoost) {
        self.filter.set_bass_boost(bass);
        self.filter.set_trim_db(-bass.gain_db);
    }

    pub fn process(&mut self, frame: [f32; 2]) -> [f32; 2] {
        let (left, right) = self.filter.process_frame(frame[0], frame[1]);
        [left, right]
    }

    /// Clears the filter state (after a seek).
    pub fn reset(&mut self) {
        self.filter.reset();
    }

    /// The gain at `hz` in dB, trim included.
    pub fn response_db(&self, hz: f32) -> f32 {
        self.filter.response_db(hz)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_parse_and_refuse() {
        let parse = |t: &str| t.parse::<f32>().ok();
        let mut o = Options::OFF;
        assert!(o.is_off());
        assert_eq!(o.set("bass", "6", parse), Ok(true));
        assert_eq!(o.set("bassfreq", "80", parse), Ok(true));
        assert_eq!(o.set("rate", "48000", parse), Ok(false));
        assert!(o.set("bass", "13", parse).is_err());
        assert!(o.set("bassq", "x", parse).is_err());
        assert_eq!((o.bass.gain_db, o.bass.hz, o.is_off()), (6.0, 80.0, false));
    }

    #[test]
    fn boost_is_unity_at_its_centre_and_cut_by_the_trim_elsewhere() {
        assert!(Bass::new(48000, BassBoost::OFF).is_none());
        let bass = Bass::new(48000, BassBoost { gain_db: 9.0, hz: 100.0, q: 0.7 }).unwrap();
        assert!(bass.response_db(100.0).abs() < 0.3, "{}", bass.response_db(100.0));
        assert!((bass.response_db(1000.0) + 9.0).abs() < 0.5, "{}", bass.response_db(1000.0));
    }

    #[test]
    fn a_tone_at_the_centre_keeps_its_level() {
        let mut bass = Bass::new(48000, BassBoost { gain_db: 9.0, hz: 100.0, q: 0.7 }).unwrap();
        let mut peak = 0.0f32;
        for n in 0..48000 {
            let x = (core::f32::consts::TAU * 100.0 * n as f32 / 48000.0).sin();
            let y = bass.process([x, x]);
            if n > 24000 {
                peak = peak.max(y[0].abs());
            }
        }
        assert!((peak - 1.0).abs() < 0.05, "{peak}");
    }
}
