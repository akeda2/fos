//! A phono equaliser: the playback curves of 78 rpm and microgroove records (`curves`: RIAA, IEC, Columbia LP, NAB, AES, Decca,
//! London, Teldec, RCA, CCIR, the early European and American 78s), a mono switch, polarity switches, a rumble filter, an
//! adjustable bass boost (0-12 dB at 60-150 Hz, with a Q), a trim and peak meters. `no_std`, allocation-free, hard-float (f64 inside); one `Phono` per stream, fed blocks of f32 frames.
//!
//! The signal path is: polarity (invert left, right or both) -> mono (left + right, which cancels the vertical rumble of a
//! stereo cartridge playing a mono record) -> the curve, one first-order lead-lag for the bass turnover and shelf and one pole
//! for the treble cut -> the rumble filter -> the bass boost -> the trim. The curve is normalised to 0 dB at 1 kHz, so the bass comes out up to
//! 20 dB louder than the midrange: watch the output peak and turn the trim down when it clips.
//!
//! The sections keep the analogue poles and match the analogue gain at Nyquist (bass turnover and shelf) and at 0.35 of the
//! sample rate (treble cut), so the digital curve stays within 0.15 dB of the analogue one up to 16 kHz at 48 kHz
//! (`response_db` is the digital curve, `curve_db` the analogue one; the tests pin the difference).
#![cfg_attr(not(test), no_std)]

pub mod curves;
mod filter;

pub use curves::{CURVES, Curve, Groove, find};
use dsp::math::{pow10_64, sqrt64};
use filter::{Biquad, Section};

const SECTIONS: usize = 5;
/// The bass pole of a curve without a shelf, in seconds (10 Hz): far enough below the audio band to change nothing above 30 Hz.
const NO_SHELF_TAU: f64 = 1.0 / (2.0 * core::f64::consts::PI * 10.0);
const IEC_TAU: f64 = 7950e-6;
const REFERENCE_HZ: f64 = 1000.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Polarity {
    Normal,
    InvertLeft,
    InvertRight,
    InvertBoth,
}

impl Polarity {
    pub fn next(self) -> Polarity {
        match self {
            Polarity::Normal => Polarity::InvertLeft,
            Polarity::InvertLeft => Polarity::InvertRight,
            Polarity::InvertRight => Polarity::InvertBoth,
            Polarity::InvertBoth => Polarity::Normal,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Polarity::Normal => "normal",
            Polarity::InvertLeft => "invert L",
            Polarity::InvertRight => "invert R",
            Polarity::InvertBoth => "invert L+R",
        }
    }

    /// The word a settings file uses.
    pub fn key(self) -> &'static str {
        match self {
            Polarity::Normal => "normal",
            Polarity::InvertLeft => "left",
            Polarity::InvertRight => "right",
            Polarity::InvertBoth => "both",
        }
    }

    pub fn parse(text: &str) -> Option<Polarity> {
        match text {
            "normal" | "off" | "none" => Some(Polarity::Normal),
            "left" | "l" | "invert-left" => Some(Polarity::InvertLeft),
            "right" | "r" | "invert-right" => Some(Polarity::InvertRight),
            "both" | "lr" | "invert" | "invert-both" => Some(Polarity::InvertBoth),
            _ => None,
        }
    }
}

/// The rumble (subsonic) filter after the curve.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Rumble {
    Off,
    /// 12 dB per octave Butterworth at this many Hz (10..=100).
    Steep(f32),
}

/// The adjustable bass boost after the curve: a peaking filter (what a turntable's bass "warmth" control does).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct BassBoost {
    /// 0..=12 dB; 0 is off.
    pub gain_db: f32,
    /// Centre frequency, 60..=150 Hz.
    pub hz: f32,
    /// Width: 0.3 (broad, about three octaves) to 3 (narrow, a third of an octave).
    pub q: f32,
}

impl BassBoost {
    pub const OFF: BassBoost = BassBoost { gain_db: 0.0, hz: 100.0, q: 0.7 };

    fn clamped(self) -> BassBoost {
        BassBoost { gain_db: self.gain_db.clamp(0.0, 12.0), hz: self.hz.clamp(60.0, 150.0), q: self.q.clamp(0.3, 3.0) }
    }
}

/// Highest input and output sample magnitudes of each channel since the last `take_peaks` (linear, 1.0 = full scale).
#[derive(Clone, Copy, Default, Debug)]
pub struct Peaks {
    pub input: [f32; 2],
    pub output: [f32; 2],
}

pub struct Phono {
    rate: f64,
    curve: usize,
    mono: bool,
    polarity: Polarity,
    rumble: Rumble,
    bass: BassBoost,
    trim_db: f32,
    sections: [Section; SECTIONS],
    gain: f64,
    chain: [[Biquad; SECTIONS]; 2],
    peaks: Peaks,
}

impl Phono {
    pub fn new(rate: u32) -> Phono {
        let mut phono = Phono {
            rate: rate as f64,
            curve: curves::RIAA,
            mono: false,
            polarity: Polarity::Normal,
            rumble: Rumble::Off,
            bass: BassBoost::OFF,
            trim_db: 0.0,
            sections: [Section::PASS; SECTIONS],
            gain: 1.0,
            chain: [[Biquad::new(Section::PASS); SECTIONS]; 2],
            peaks: Peaks::default(),
        };
        phono.rebuild();
        phono
    }

    pub fn curve(&self) -> usize {
        self.curve
    }
    pub fn mono(&self) -> bool {
        self.mono
    }
    pub fn polarity(&self) -> Polarity {
        self.polarity
    }
    pub fn rumble(&self) -> Rumble {
        self.rumble
    }
    pub fn trim_db(&self) -> f32 {
        self.trim_db
    }
    pub fn bass_boost(&self) -> BassBoost {
        self.bass
    }

    pub fn set_curve(&mut self, curve: usize) {
        self.curve = curve.min(CURVES.len() - 1);
        self.rebuild();
    }
    pub fn set_mono(&mut self, mono: bool) {
        self.mono = mono;
    }
    pub fn set_polarity(&mut self, polarity: Polarity) {
        self.polarity = polarity;
    }
    pub fn set_rumble(&mut self, rumble: Rumble) {
        self.rumble = match rumble {
            Rumble::Steep(hz) => Rumble::Steep(hz.clamp(10.0, 100.0)),
            off => off,
        };
        self.rebuild();
    }
    pub fn set_bass_boost(&mut self, bass: BassBoost) {
        self.bass = bass.clamped();
        self.rebuild();
    }
    pub fn set_trim_db(&mut self, db: f32) {
        self.trim_db = db.clamp(-24.0, 12.0);
        self.rebuild();
    }

    /// The analogue curve plus the rumble filter, as sections; the same list the response and the processing use.
    fn design(curve: &Curve, rumble: Rumble, bass: BassBoost, rate: f64) -> [Section; SECTIONS] {
        let mut out = [Section::PASS; SECTIONS];
        if curve.t2_us > 0.0 {
            let pole = if curve.t1_us > 0.0 { curve.t1_us as f64 * 1e-6 } else { NO_SHELF_TAU };
            out[0] = Section::lead_lag(curve.t2_us as f64 * 1e-6, pole, rate);
        }
        if curve.t3_us > 0.0 {
            out[1] = Section::low_pass(curve.t3_us as f64 * 1e-6, rate, 0.35 * rate);
        }
        if curve.iec_rumble {
            out[2] = Section::high_pass(IEC_TAU, rate);
        }
        if let Rumble::Steep(hz) = rumble {
            out[3] = Section::butterworth_high_pass(hz as f64, rate);
        }
        if bass.gain_db > 0.0 {
            out[4] = Section::peaking(bass.hz as f64, bass.q as f64, bass.gain_db as f64, rate);
        }
        out
    }

    fn rebuild(&mut self) {
        let curve = &CURVES[self.curve];
        self.sections = Self::design(curve, self.rumble, self.bass, self.rate);
        for chain in self.chain.iter_mut() {
            for (biquad, section) in chain.iter_mut().zip(self.sections.iter()) {
                biquad.section = *section;
            }
        }
        // Normalise the curve (and rumble filter) to 0 dB at 1 kHz, then the trim; the bass boost is not part of the reference,
        // so its gain is what it says at its centre frequency.
        let mut reference = self.sections;
        reference[4] = Section::PASS;
        let (re, im) = Self::combined(&reference, REFERENCE_HZ, self.rate);
        let at_reference = sqrt64(re * re + im * im);
        self.gain = pow10_64(self.trim_db as f64 / 20.0) / at_reference;
    }

    fn combined(sections: &[Section; SECTIONS], hz: f64, rate: f64) -> (f64, f64) {
        let (mut re, mut im) = (1.0, 0.0);
        for section in sections {
            let (r, i) = section.response(hz, rate);
            (re, im) = (re * r - im * i, re * i + im * r);
        }
        (re, im)
    }

    /// The digital response of the whole equaliser at `hz`, in dB (0 dB at 1 kHz plus the trim).
    pub fn response_db(&self, hz: f32) -> f32 {
        let (re, im) = Self::combined(&self.sections, hz as f64, self.rate);
        (20.0 * dsp::math::ln64(self.gain * sqrt64(re * re + im * im)) / core::f64::consts::LN_10) as f32
    }

    /// Clears the filter memory (after a gap in the signal, or a new input).
    pub fn reset(&mut self) {
        for chain in self.chain.iter_mut() {
            for biquad in chain.iter_mut() {
                biquad.reset();
            }
        }
    }

    pub fn take_peaks(&mut self) -> Peaks {
        core::mem::take(&mut self.peaks)
    }

    /// Equalises `left` and `right` in place (equal lengths).
    pub fn process(&mut self, left: &mut [f32], right: &mut [f32]) {
        for (l, r) in left.iter_mut().zip(right.iter_mut()) {
            (*l, *r) = self.process_frame(*l, *r);
        }
    }

    /// Equalises interleaved stereo frames in place.
    pub fn process_frames(&mut self, frames: &mut [[f32; 2]]) {
        for frame in frames.iter_mut() {
            let [l, r] = *frame;
            let (l, r) = self.process_frame(l, r);
            *frame = [l, r];
        }
    }

    /// One stereo frame through polarity, mono, the curve, the rumble filter, the bass boost and the trim.
    pub fn process_frame(&mut self, left: f32, right: f32) -> (f32, f32) {
        let (flip_left, flip_right) = match self.polarity {
            Polarity::Normal => (1.0, 1.0),
            Polarity::InvertLeft => (-1.0, 1.0),
            Polarity::InvertRight => (1.0, -1.0),
            Polarity::InvertBoth => (-1.0, -1.0),
        };
        self.peaks.input[0] = self.peaks.input[0].max(left.abs());
        self.peaks.input[1] = self.peaks.input[1].max(right.abs());
        let (x, y) = (left as f64 * flip_left, right as f64 * flip_right);
        let (a, b) = if self.mono {
            let m = self.run(0, 0.5 * (x + y));
            (m, m)
        } else {
            (self.run(0, x), self.run(1, y))
        };
        let (a, b) = (a as f32, b as f32);
        self.peaks.output[0] = self.peaks.output[0].max(a.abs());
        self.peaks.output[1] = self.peaks.output[1].max(b.abs());
        (a, b)
    }

    #[inline]
    fn run(&mut self, channel: usize, mut x: f64) -> f64 {
        for biquad in self.chain[channel].iter_mut() {
            x = biquad.process(x);
        }
        x * self.gain
    }
}

/// The analogue playback curve of `curve` in dB at `hz`, 0 dB at 1 kHz (what the tape draws as the reference line).
pub fn curve_db(curve: &Curve, hz: f32) -> f32 {
    let magnitude = |f: f64| {
        let w = 2.0 * core::f64::consts::PI * f;
        let mut power = 1.0;
        if curve.t2_us > 0.0 {
            let (t2, t1) = (curve.t2_us as f64 * 1e-6, if curve.t1_us > 0.0 { curve.t1_us as f64 * 1e-6 } else { NO_SHELF_TAU });
            power *= (1.0 + w * w * t2 * t2) / (1.0 + w * w * t1 * t1);
        }
        if curve.t3_us > 0.0 {
            let t3 = curve.t3_us as f64 * 1e-6;
            power /= 1.0 + w * w * t3 * t3;
        }
        if curve.iec_rumble {
            let t = IEC_TAU;
            power *= w * w * t * t / (1.0 + w * w * t * t);
        }
        power
    };
    (10.0 * dsp::math::ln64(magnitude(hz as f64) / magnitude(REFERENCE_HZ)) / core::f64::consts::LN_10) as f32
}

#[cfg(test)]
mod tests;
