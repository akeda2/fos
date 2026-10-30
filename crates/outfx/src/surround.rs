//! The surround engine of the phono tape: stereo (`Lt Rt`) in, the channels of a quad layout out. One struct holds the
//! decoders and the steering so the tape only picks settings.
//!
//! * `Sq` and `Qs`: the passive matrix decoders (`sq`, `qs`), for records and files made with those encoders.
//! * `Ambience`: the passive stereo-to-surround matrix of `upmix` (side signal, dulled and delayed, to the rears), for
//!   ordinary stereo such as classical and organ recordings.
//! * `logic`: optional steering (`Preset::Music` or `Film`) on the decoded SQ/QS channels. The QS matrix on plain stereo
//!   with Music steering is also a stereo upmix of its own (the "horseshoe" of Sansui's decoders).
//!
//! There is never a centre: the C slot of a 5.1 layout stays silent. With an LFE in the layout it carries the bass of the
//! two fronts (Linkwitz-Riley low-pass at `lfe_hz`); the fronts stay full range.
//! Channel order is the layout's: 4.0 is LF RF LB RB, 4.1 LF RF LFE LB RB, 5.1 LF RF C LFE LB RB.
use crate::logic::{Logic, Preset};
use crate::qs::QsDecoder;
use crate::sq::SqDecoder;
use crate::upmix::{Biquad, Layout, Upmix, UpmixSettings};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Off,
    Sq,
    Qs,
    Ambience,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::Sq => "sq",
            Mode::Qs => "qs",
            Mode::Ambience => "ambience",
        }
    }

    pub fn parse(text: &str) -> Option<Mode> {
        [Mode::Off, Mode::Sq, Mode::Qs, Mode::Ambience].into_iter().find(|mode| mode.name() == text)
    }

    pub fn next(self) -> Mode {
        match self {
            Mode::Off => Mode::Sq,
            Mode::Sq => Mode::Qs,
            Mode::Qs => Mode::Ambience,
            Mode::Ambience => Mode::Off,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct SurroundSettings {
    pub mode: Mode,
    /// Steering on the SQ and QS decoders; `None` is the plain matrix.
    pub logic: Option<Preset>,
    /// Mirror the rear pair (a source made with the opposite sign of the 90 degree shift, or a wrong-way cable).
    pub swap_rear: bool,
    /// 4.0, 4.1 or 5.1 (no centre in any).
    pub layout: Layout,
    pub lfe_hz: f32,
    /// `Ambience` only: rear level 0..1 and delay in ms.
    pub width: f32,
    pub delay_ms: f32,
}

impl SurroundSettings {
    pub const DEFAULT: SurroundSettings =
        SurroundSettings { mode: Mode::Off, logic: None, swap_rear: false, layout: Layout::Quad40, lfe_hz: 100.0, width: 0.7, delay_ms: 15.0 };

    /// Output channels the device must have for these settings (2 while off).
    pub fn channels(&self) -> usize {
        if self.mode == Mode::Off { 2 } else { self.layout.channels() }
    }
}

pub struct Surround {
    settings: SurroundSettings,
    sq: SqDecoder,
    qs: QsDecoder,
    logic: Logic,
    upmix: Upmix,
    lfe: [Biquad; 2],
}

impl Surround {
    /// For a `static` (the ambience delay line is 64 KiB); `tune` before use.
    pub const fn new() -> Surround {
        Surround {
            settings: SurroundSettings::DEFAULT,
            sq: SqDecoder::new(),
            qs: QsDecoder::new(),
            logic: Logic::new(),
            upmix: Upmix::new(),
            lfe: [Upmix::IDLE; 2],
        }
    }

    pub fn settings(&self) -> &SurroundSettings {
        &self.settings
    }

    /// Applies new settings keeping the running state where the mode did not change, so a button can be pressed while
    /// the music plays. A change of mode starts the new decoder from silence.
    pub fn tune(&mut self, rate: u32, settings: SurroundSettings) {
        self.apply(rate, settings, settings.mode != self.settings.mode);
    }

    /// Like `tune`, but always starts the decoders from silence: the same input then gives the same output (a second pass
    /// over a file).
    pub fn restart(&mut self, rate: u32) {
        let settings = self.settings;
        self.apply(rate, settings, true);
    }

    fn apply(&mut self, rate: u32, settings: SurroundSettings, mode_changed: bool) {
        self.settings = settings;
        self.sq.swap_rear = settings.swap_rear;
        self.qs.swap_rear = settings.swap_rear;
        let preset = settings.logic.unwrap_or(Preset::Music);
        self.logic.tune(rate, preset);
        let ambience = UpmixSettings { width: settings.width, lfe_hz: settings.lfe_hz, delay_ms: settings.delay_ms, center: false, layout: Layout::Surround51, bass_mgmt: false };
        if mode_changed {
            self.sq.reset();
            self.qs.reset();
            self.logic.reset();
            self.upmix.configure(rate, ambience);
        } else {
            self.upmix.tune(rate, ambience);
        }
        let lfe = Biquad::new(settings.lfe_hz.clamp(40.0, 200.0) as f64, rate as f64, core::f64::consts::FRAC_1_SQRT_2);
        self.lfe = [lfe, lfe];
    }

    /// One stereo frame to the channels of the layout (the first `layout.channels()` of the result). In `Off` mode the
    /// frame comes back as it is in the first two.
    pub fn process(&mut self, frame: [f32; 2]) -> [f32; 6] {
        let quad = match self.settings.mode {
            Mode::Off => return [frame[0], frame[1], 0.0, 0.0, 0.0, 0.0],
            Mode::Sq | Mode::Qs => {
                let decoded = if self.settings.mode == Mode::Sq { self.sq.process(frame) } else { self.qs.process(frame) };
                if self.settings.logic.is_some() { self.logic.process(decoded) } else { decoded }
            }
            Mode::Ambience => {
                let six = self.upmix.process(frame);
                [six[0], six[1], six[4], six[5]]
            }
        };
        let bass = 0.5 * (quad[0] + quad[1]);
        let first = self.lfe[0].process(bass as f64);
        let lfe = self.lfe[1].process(first) as f32;
        self.settings.layout.pick([quad[0], quad[1], 0.0, lfe, quad[2], quad[3]])
    }
}

impl Default for Surround {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(settings: SurroundSettings, frames: &[[f32; 2]]) -> Vec<[f32; 6]> {
        let mut surround = Box::new(Surround::new());
        surround.tune(48_000, settings);
        frames.iter().map(|&f| surround.process(f)).collect()
    }

    fn tone(hz: f32, n: usize) -> Vec<f32> {
        (0..n).map(|i| 0.4 * (core::f32::consts::TAU * hz * i as f32 / 48_000.0).sin()).collect()
    }

    fn power(out: &[[f32; 6]], channel: usize) -> f32 {
        let tail = &out[out.len() / 2..];
        tail.iter().map(|f| f[channel] * f[channel]).sum::<f32>() / tail.len() as f32
    }

    #[test]
    fn off_passes_stereo_through() {
        let out = run(SurroundSettings::DEFAULT, &[[0.25, -0.5]; 4]);
        assert!(out.iter().all(|f| *f == [0.25, -0.5, 0.0, 0.0, 0.0, 0.0]));
    }

    #[test]
    fn layouts_place_channels_and_never_feed_a_centre() {
        let frames: Vec<[f32; 2]> = tone(1000.0, 9600).iter().map(|&x| [x, -x]).collect();
        for (layout, rear) in [(Layout::Quad40, [2, 3]), (Layout::Quad41, [3, 4]), (Layout::Surround51, [4, 5])] {
            let out = run(SurroundSettings { mode: Mode::Qs, layout, ..SurroundSettings::DEFAULT }, &frames);
            assert!(power(&out, rear[0]) > 1e-3 || power(&out, rear[1]) > 1e-3, "{layout:?} rears silent");
            if layout == Layout::Surround51 {
                assert_eq!(power(&out, 2), 0.0, "centre must stay silent");
            }
            for frame in &out {
                assert!(frame[layout.channels()..].iter().all(|&v| v == 0.0));
            }
        }
    }

    #[test]
    fn swap_rear_exchanges_the_rear_channels_in_every_decoder() {
        let frames: Vec<[f32; 2]> = tone(700.0, 9600).iter().map(|&x| [0.9 * x, 0.3 * x]).collect();
        for mode in [Mode::Sq, Mode::Qs] {
            for logic in [None, Some(Preset::Film)] {
                let base = SurroundSettings { mode, logic, ..SurroundSettings::DEFAULT };
                let (a, b) = (run(base, &frames), run(SurroundSettings { swap_rear: true, ..base }, &frames));
                for (x, y) in a.iter().zip(&b) {
                    assert!((x[2] - y[3]).abs() < 1e-6 && (x[3] - y[2]).abs() < 1e-6, "{mode:?} {logic:?}");
                    assert!((x[0] - y[0]).abs() < 1e-6);
                }
            }
        }
    }

    #[test]
    fn lfe_carries_the_bass_of_the_fronts_only() {
        for (hz, loud) in [(40.0, true), (2000.0, false)] {
            let frames: Vec<[f32; 2]> = tone(hz, 19_200).iter().map(|&x| [x, x]).collect();
            let out = run(SurroundSettings { mode: Mode::Qs, layout: Layout::Quad41, ..SurroundSettings::DEFAULT }, &frames);
            assert_eq!(power(&out, 2) > 0.01, loud, "{hz} Hz LFE power {}", power(&out, 2));
        }
    }

    #[test]
    fn changing_mode_while_running_is_safe_and_ambience_works() {
        let mut surround = Box::new(Surround::new());
        let mut settings = SurroundSettings { mode: Mode::Ambience, ..SurroundSettings::DEFAULT };
        surround.tune(48_000, settings);
        let side: Vec<[f32; 2]> = tone(500.0, 9600).iter().map(|&x| [x, -x]).collect();
        let out: Vec<[f32; 6]> = side.iter().map(|&f| surround.process(f)).collect();
        assert!(power(&out, 2) > 1e-3, "ambience feeds the rears from the side signal");
        for mode in [Mode::Sq, Mode::Qs, Mode::Off, Mode::Ambience] {
            settings.mode = mode;
            surround.tune(48_000, settings);
            for &f in &side[..500] {
                assert!(surround.process(f).iter().all(|v| v.is_finite()));
            }
        }
    }
}
