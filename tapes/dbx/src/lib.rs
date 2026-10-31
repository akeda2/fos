//! The `dbx` tape's logic without a screen: the stereo compander unit (two `compander::Compander`s, meters, mode and type
//! switching) and the touch layout. `main.rs` draws and runs it; this part is testable on the host.
#![cfg_attr(not(test), no_std)]

use compander::tapeeq::{Conversion, Direction, Speed, Standard};
use compander::{Compander, Mode, Params};
use core::fmt::Write;
use dsp::math::{log10, pow10, sin};
use instrument_rt::Rect;
use noise::Generator;
use tape_rt::Line;

pub const GLYPH_H: i32 = gfx::GLYPH_HEIGHT as i32;

/// Which dbx family the unit follows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    I,
    II,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::I => "Type I",
            Kind::II => "Type II",
        }
    }

    pub fn params(self) -> Params {
        match self {
            Kind::I => Params::type_i(),
            Kind::II => Params::type_ii(),
        }
    }

    pub fn from_name(name: &str) -> Option<Kind> {
        match name {
            "1" | "i" | "I" => Some(Kind::I),
            "2" | "ii" | "II" => Some(Kind::II),
            _ => None,
        }
    }
}

pub fn mode_from_name(name: &str) -> Option<Mode> {
    match name {
        "bypass" | "off" => Some(Mode::Bypass),
        "encode" | "record" | "rec" => Some(Mode::Encode),
        "decode" | "play" | "playback" => Some(Mode::Decode),
        _ => None,
    }
}

pub fn mode_name(mode: Mode) -> &'static str {
    match mode {
        Mode::Bypass => "BYPASS",
        Mode::Encode => "ENCODE",
        Mode::Decode => "DECODE",
    }
}

/// What a meter shows for a peak: dBFS, -120 for silence.
fn db(peak: f32) -> f32 {
    if peak < 1e-6 { -120.0 } else { 20.0 * log10(peak) }
}

/// The alignment tone: sent straight out, never through the compander (a dbx tape must not have its tones encoded).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Off,
    Hz1000,
    Hz10000,
    Hz100,
    /// Pink noise (equal energy per octave) for calibrating by spectrum; its RMS is that of a sine of the same level.
    Pink,
}

impl Tone {
    pub fn hz(self) -> f32 {
        match self {
            Tone::Off | Tone::Pink => 0.0,
            Tone::Hz1000 => 1000.0,
            Tone::Hz10000 => 10_000.0,
            Tone::Hz100 => 100.0,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Tone::Off => "off",
            Tone::Hz1000 => "1k",
            Tone::Hz10000 => "10k",
            Tone::Hz100 => "100",
            Tone::Pink => "pink",
        }
    }

    pub fn next(self) -> Tone {
        match self {
            Tone::Off => Tone::Hz1000,
            Tone::Hz1000 => Tone::Hz10000,
            Tone::Hz10000 => Tone::Hz100,
            Tone::Hz100 => Tone::Pink,
            Tone::Pink => Tone::Off,
        }
    }

    pub fn from_name(name: &str) -> Option<Tone> {
        [Tone::Off, Tone::Hz1000, Tone::Hz10000, Tone::Hz100, Tone::Pink].into_iter().find(|t| t.name() == name)
    }
}

/// Which signal the spectrum view analyses: what comes in (a tape's return) or what goes out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Input,
    Output,
}

impl Source {
    pub fn name(self) -> &'static str {
        match self {
            Source::Input => "in",
            Source::Output => "out",
        }
    }

    pub fn other(self) -> Source {
        if self == Source::Input { Source::Output } else { Source::Input }
    }
}

pub const TONE_MIN_DB: f32 = -40.0;
pub const TONE_MAX_DB: f32 = 0.0;
pub const TRIM_LIMIT_DB: f32 = 12.0;
pub const TRIM_STEP_DB: f32 = 0.5;

/// A stereo dbx unit: record (encode) in front of a tape machine, playback (decode) behind it.
///
/// Signal path per channel: Encode: input -> dbx encode -> tape EQ conversion for recording -> trim; Decode: input -> tape EQ
/// conversion for playback -> dbx decode -> trim; Bypass: input -> trim. The alignment tone replaces all of it (and the trim).
pub struct Unit {
    channels: [Compander; 2],
    eq: [Conversion; 2],
    kind: Kind,
    mode: Mode,
    rate: f32,
    fall: f32,
    /// The standard the machine's equalisers are aligned to, the standard the tape was (or is to be) recorded to, and the speed.
    machine: Standard,
    tape: Standard,
    speed: Speed,
    tone: Tone,
    tone_db: f32,
    tone_phase: f32,
    noise: Generator,
    spectrum: bool,
    source: Source,
    trim_db: f32,
    trim_gain: f32,
    in_peak: [f32; 2],
    out_peak: [f32; 2],
    /// Samples that would have been over full scale and were limited, per channel.
    pub clipped: [u32; 2],
}

impl Unit {
    pub fn new(rate: u32, kind: Kind, mode: Mode) -> Unit {
        let rate = rate as f32;
        let mut unit = Unit {
            channels: [Compander::new(kind.params(), rate, mode); 2],
            eq: [Conversion::new(rate, Standard::Nab, Standard::Nab, Speed::Ips15, Direction::Playback); 2],
            kind,
            mode,
            rate,
            // The meters fall 20 dB a second.
            fall: 1.0 - 2.3 / rate,
            machine: Standard::Nab,
            tape: Standard::Nab,
            speed: Speed::Ips15,
            tone: Tone::Off,
            tone_db: -18.0,
            tone_phase: 0.0,
            noise: Generator::new(),
            spectrum: false,
            source: Source::Input,
            trim_db: 0.0,
            trim_gain: 1.0,
            in_peak: [0.0; 2],
            out_peak: [0.0; 2],
            clipped: [0; 2],
        };
        unit.noise.set_rate(rate as u32);
        unit.noise.set_level_db(unit.tone_db);
        unit.rebuild_eq();
        unit
    }

    fn rebuild_eq(&mut self) {
        let direction = if self.mode == Mode::Encode { Direction::Record } else { Direction::Playback };
        // Bypass passes the signal as it is, equalisation included.
        let tape = if self.mode == Mode::Bypass { self.machine } else { self.tape };
        self.eq = [Conversion::new(self.rate, self.machine, tape, self.speed, direction); 2];
    }

    pub fn kind(&self) -> Kind {
        self.kind
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
        for channel in &mut self.channels {
            channel.set_mode(mode);
        }
        self.rebuild_eq();
    }

    /// Switching family restarts the detectors: a compander is only decodable by the family that encoded it.
    pub fn set_kind(&mut self, kind: Kind) {
        if kind != self.kind {
            self.kind = kind;
            self.channels = [Compander::new(kind.params(), self.rate, self.mode); 2];
        }
    }

    pub fn machine(&self) -> Standard {
        self.machine
    }

    pub fn tape(&self) -> Standard {
        self.tape
    }

    pub fn speed(&self) -> Speed {
        self.speed
    }

    pub fn set_machine(&mut self, standard: Standard) {
        self.machine = standard;
        self.rebuild_eq();
    }

    pub fn set_tape(&mut self, standard: Standard) {
        self.tape = standard;
        self.rebuild_eq();
    }

    pub fn set_speed(&mut self, speed: Speed) {
        self.speed = speed;
        self.rebuild_eq();
    }

    /// True while a tape standard other than the machine's is being corrected for.
    pub fn converting(&self) -> bool {
        self.mode != Mode::Bypass && self.machine != self.tape
    }

    pub fn tone(&self) -> Tone {
        self.tone
    }

    pub fn tone_db(&self) -> f32 {
        self.tone_db
    }

    pub fn set_tone(&mut self, tone: Tone) {
        self.tone = tone;
        self.noise.set_kind(if tone == Tone::Pink { noise::Kind::Pink } else { noise::Kind::Silence });
    }

    pub fn spectrum(&self) -> bool {
        self.spectrum
    }

    pub fn source(&self) -> Source {
        self.source
    }

    pub fn set_spectrum(&mut self, on: bool) {
        self.spectrum = on;
    }

    pub fn set_source(&mut self, source: Source) {
        self.source = source;
    }

    /// Level of the tone in dBFS (0 = a full-scale sine, the scale of the spectrum view; pink noise has the RMS of such a sine), within `TONE_MIN_DB..=TONE_MAX_DB`.
    pub fn set_tone_db(&mut self, db: f32) {
        self.tone_db = db.clamp(TONE_MIN_DB, TONE_MAX_DB);
        self.noise.set_level_db(self.tone_db);
    }

    pub fn trim_db(&self) -> f32 {
        self.trim_db
    }

    /// The output trim, dB within +-`TRIM_LIMIT_DB`.
    pub fn set_trim_db(&mut self, db: f32) {
        self.trim_db = db.clamp(-TRIM_LIMIT_DB, TRIM_LIMIT_DB);
        self.trim_gain = pow10(self.trim_db / 20.0);
    }

    pub fn process_block(&mut self, frames: &mut [[f32; 2]]) {
        let tone_amplitude = pow10(self.tone_db / 20.0);
        let tone_step = 2.0 * core::f32::consts::PI * self.tone.hz() / self.rate;
        for frame in frames {
            let tone = (self.tone != Tone::Off).then(|| {
                if self.tone == Tone::Pink {
                    let mut sample = [0.0f32];
                    self.noise.fill(&mut sample);
                    return sample[0];
                }
                self.tone_phase += tone_step;
                if self.tone_phase > core::f32::consts::PI {
                    self.tone_phase -= 2.0 * core::f32::consts::PI;
                }
                tone_amplitude * sin(self.tone_phase)
            });
            for c in 0..2 {
                let x = frame[c];
                self.in_peak[c] = x.abs().max(self.in_peak[c] * self.fall);
                let mut y = match (tone, self.mode) {
                    (Some(tone), _) => tone,
                    (None, Mode::Encode) => {
                        let encoded = self.channels[c].process(x);
                        self.eq[c].process(encoded) * self.trim_gain
                    }
                    (None, _) => {
                        let corrected = self.eq[c].process(x);
                        self.channels[c].process(corrected) * self.trim_gain
                    }
                };
                // A burst out of silence overshoots for a few samples while the detector catches up; the converter must not wrap.
                if y.abs() > 1.0 {
                    self.clipped[c] += 1;
                    y = y.clamp(-1.0, 1.0);
                }
                self.out_peak[c] = y.abs().max(self.out_peak[c] * self.fall);
                frame[c] = y;
            }
        }
    }

    pub fn input_db(&self, channel: usize) -> f32 {
        db(self.in_peak[channel])
    }

    pub fn output_db(&self, channel: usize) -> f32 {
        db(self.out_peak[channel])
    }

    /// The gain the compander applies now, dB (0 in bypass).
    pub fn gain_db(&self, channel: usize) -> f32 {
        if self.mode == Mode::Bypass || self.tone != Tone::Off { 0.0 } else { self.channels[channel].gain_db() }
    }

    /// The detector's RMS level, dBFS.
    pub fn level_db(&self, channel: usize) -> f32 {
        self.channels[channel].level_db()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    Mode(Mode),
    Kind(Kind),
    /// Trim steps (+1 / -1 of `TRIM_STEP_DB`).
    Trim(i8),
    /// Cycle the alignment tone.
    Tone,
    /// Tone level steps (1 dB).
    ToneLevel(i8),
    Speed,
    Machine,
    Tape,
    /// Show the spectrum instead of the channel strips.
    Spectrum,
    /// Analyse the input or the output.
    Source,
    Quit,
}

pub const BUTTON_COLUMNS: usize = 8;
pub const BUTTONS: [Hit; 16] = [
    Hit::Mode(Mode::Bypass),
    Hit::Mode(Mode::Encode),
    Hit::Mode(Mode::Decode),
    Hit::Kind(Kind::I),
    Hit::Kind(Kind::II),
    Hit::Trim(-1),
    Hit::Trim(1),
    Hit::Spectrum,
    Hit::Tone,
    Hit::ToneLevel(-1),
    Hit::ToneLevel(1),
    Hit::Speed,
    Hit::Machine,
    Hit::Tape,
    Hit::Source,
    Hit::Quit,
];

/// The text on a button (at most 11 characters, so it fits at the large scale on a 1280-wide screen).
pub fn label(unit: &Unit, hit: Hit) -> Line<16> {
    let mut line = Line::<16>::new();
    let _ = match hit {
        Hit::Mode(Mode::Bypass) => line.write_str("Bypass"),
        Hit::Mode(Mode::Encode) => line.write_str("Encode"),
        Hit::Mode(Mode::Decode) => line.write_str("Decode"),
        Hit::Kind(kind) => line.write_str(kind.name()),
        Hit::Trim(step) => write!(line, "Trim {}", if step < 0 { "-" } else { "+" }),
        Hit::Tone => write!(line, "Tone {}", unit.tone().name()),
        Hit::ToneLevel(step) => write!(line, "Tone {}", if step < 0 { "-" } else { "+" }),
        Hit::Speed => write!(line, "{} ips", unit.speed().name()),
        Hit::Machine => write!(line, "Mach {}", unit.machine().name()),
        Hit::Tape => write!(line, "Tape {}", unit.tape().name()),
        Hit::Spectrum => line.write_str("Spectrum"),
        Hit::Source => write!(line, "Src {}", unit.source().name()),
        Hit::Quit => line.write_str("Quit"),
    };
    line
}

/// Whether a button is drawn lit.
pub fn lit(unit: &Unit, hit: Hit) -> bool {
    match hit {
        Hit::Mode(mode) => unit.mode() == mode,
        Hit::Kind(kind) => unit.kind() == kind,
        Hit::Tone => unit.tone() != Tone::Off,
        Hit::Tape => unit.converting(),
        Hit::Spectrum => unit.spectrum(),
        _ => false,
    }
}

pub struct Layout {
    pub width: i32,
    pub height: i32,
    pub scale: i32,
    pub margin: i32,
    pub header: Rect,
    /// The two channel strips, left and right.
    pub strips: [Rect; 2],
    pub buttons: [Rect; 16],
}

impl Layout {
    pub fn new(width: i32, height: i32) -> Layout {
        let scale = if width >= 1200 { 2 } else { 1 };
        let margin = 6 * scale;
        let header_h = GLYPH_H * scale * 2 + margin * 2;
        let button_h = GLYPH_H * scale * 2 + margin * 2;
        let rows = (BUTTONS.len() / BUTTON_COLUMNS) as i32;
        let buttons_y = height - rows * (button_h + margin);
        let top = header_h + margin;
        let strip_w = (width - 3 * margin) / 2;
        let strip_h = buttons_y - margin - top;
        let strips = [0, 1].map(|i| Rect { x: margin + i * (strip_w + margin), y: top, w: strip_w, h: strip_h });
        let columns = BUTTON_COLUMNS as i32;
        let button_w = (width - (columns + 1) * margin) / columns;
        let buttons = core::array::from_fn(|i| {
            let (row, column) = ((i / BUTTON_COLUMNS) as i32, (i % BUTTON_COLUMNS) as i32);
            Rect { x: margin + column * (button_w + margin), y: buttons_y + row * (button_h + margin), w: button_w, h: button_h }
        });
        Layout { width, height, scale, margin, header: Rect { x: 0, y: 0, w: width, h: header_h }, strips, buttons }
    }

    pub fn hit(&self, x: i32, y: i32) -> Option<Hit> {
        self.buttons.iter().position(|b| b.contains(x, y)).map(|i| BUTTONS[i])
    }
}

/// Applies a hit to the unit; false for Quit, which the caller handles.
pub fn press(unit: &mut Unit, hit: Hit) -> bool {
    match hit {
        Hit::Mode(mode) => unit.set_mode(mode),
        Hit::Kind(kind) => unit.set_kind(kind),
        Hit::Trim(step) => unit.set_trim_db(unit.trim_db() + step as f32 * TRIM_STEP_DB),
        Hit::Tone => unit.set_tone(unit.tone().next()),
        Hit::ToneLevel(step) => unit.set_tone_db(unit.tone_db() + step as f32),
        Hit::Speed => unit.set_speed(unit.speed().next()),
        Hit::Machine => unit.set_machine(unit.machine().other()),
        Hit::Tape => unit.set_tape(unit.tape().other()),
        Hit::Spectrum => unit.set_spectrum(!unit.spectrum()),
        Hit::Source => unit.set_source(unit.source().other()),
        Hit::Quit => return false,
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(unit: &mut Unit, db: f32, frames: usize) -> f32 {
        let amplitude = 10f32.powf(db / 20.0) * 2f32.sqrt();
        let mut last = 0.0;
        for block in 0..frames / 480 {
            let mut buffer = [[0.0f32; 2]; 480];
            for (n, frame) in buffer.iter_mut().enumerate() {
                let t = (block * 480 + n) as f32 / 48_000.0;
                let v = amplitude * (2.0 * core::f32::consts::PI * 1000.0 * t).sin();
                *frame = [v, v];
            }
            unit.process_block(&mut buffer);
            last = buffer[479][0];
        }
        last
    }

    #[test]
    fn encode_lowers_a_loud_tone_and_the_meters_follow() {
        let mut unit = Unit::new(48_000, Kind::I, Mode::Encode);
        tone(&mut unit, -12.0, 48_000);
        // The pre-emphasis lifts 1 kHz by 7.7 dB before the detector, so the pivot (-12 dBFS) is passed early.
        assert!((unit.gain_db(0) + 3.8).abs() < 0.5, "{}", unit.gain_db(0));
        tone(&mut unit, -40.0, 96_000);
        assert!(unit.gain_db(0) > 10.0, "{}", unit.gain_db(0));
        assert!(unit.input_db(1) < -30.0 && unit.output_db(1) > unit.input_db(1));
    }

    #[test]
    fn switching_type_keeps_the_mode_and_bypass_is_flat() {
        let mut unit = Unit::new(48_000, Kind::I, Mode::Decode);
        assert!(press(&mut unit, Hit::Kind(Kind::II)));
        assert_eq!((unit.kind(), unit.mode()), (Kind::II, Mode::Decode));
        assert!(press(&mut unit, Hit::Mode(Mode::Bypass)));
        let mut frames = [[0.25f32, -0.5]; 8];
        unit.process_block(&mut frames);
        assert_eq!(frames[7], [0.25, -0.5]);
        assert_eq!(unit.gain_db(0), 0.0);
        assert!(!press(&mut unit, Hit::Quit));
    }

    #[test]
    fn nothing_leaves_full_scale() {
        let mut unit = Unit::new(48_000, Kind::II, Mode::Encode);
        let mut frames = [[0.0f32; 2]; 4800];
        for frame in frames.iter_mut().skip(2400) {
            *frame = [0.9, -0.9];
        }
        unit.process_block(&mut frames);
        assert!(frames.iter().all(|f| f[0].abs() <= 1.0 && f[1].abs() <= 1.0));
    }

    fn rms_db(frames: &[[f32; 2]]) -> f32 {
        let sum: f32 = frames.iter().map(|f| f[0] * f[0]).sum();
        10.0 * log10(sum / frames.len() as f32)
    }

    #[test]
    fn the_tone_goes_out_at_its_level_unprocessed() {
        let mut unit = Unit::new(48_000, Kind::II, Mode::Encode);
        press(&mut unit, Hit::Tone);
        press(&mut unit, Hit::Trim(1));
        unit.set_tone_db(-12.0);
        assert_eq!(unit.tone(), Tone::Hz1000);
        let mut frames = [[0.0f32; 2]; 4800];
        unit.process_block(&mut frames);
        // A sine at -12 dBFS has an RMS 3.01 dB lower still (rms_db is re 1.0).
        let level = rms_db(&frames);
        assert!((level + 15.01).abs() < 0.1, "{level}");
        assert!(frames.iter().all(|f| f[0] == f[1]));
        assert_eq!(unit.gain_db(0), 0.0);
        press(&mut unit, Hit::ToneLevel(1));
        assert_eq!(unit.tone_db(), -11.0);
        unit.set_tone_db(10.0);
        assert_eq!(unit.tone_db(), TONE_MAX_DB);
    }

    #[test]
    fn the_trim_scales_the_output_and_is_limited() {
        let mut unit = Unit::new(48_000, Kind::I, Mode::Bypass);
        for _ in 0..12 {
            press(&mut unit, Hit::Trim(1));
        }
        assert_eq!(unit.trim_db(), 6.0);
        let mut frames = [[0.1f32, -0.1]; 4];
        unit.process_block(&mut frames);
        assert!((frames[3][0] - 0.1 * 1.9953).abs() < 1e-3 && (frames[3][1] + 0.1 * 1.9953).abs() < 1e-3);
        unit.set_trim_db(100.0);
        assert_eq!(unit.trim_db(), TRIM_LIMIT_DB);
        unit.set_trim_db(-100.0);
        assert_eq!(unit.trim_db(), -TRIM_LIMIT_DB);
    }

    #[test]
    fn a_tape_recorded_for_the_other_standard_plays_back_flat() {
        for (machine, tape) in [(Standard::Iec, Standard::Nab), (Standard::Nab, Standard::Iec)] {
            for hz in [60.0f32, 1000.0, 8000.0] {
                let mut record = Unit::new(48_000, Kind::II, Mode::Encode);
                let mut play = Unit::new(48_000, Kind::II, Mode::Decode);
                for unit in [&mut record, &mut play] {
                    unit.set_machine(machine);
                    unit.set_tape(tape);
                    assert!(unit.converting());
                }
                let mut worst = 0.0f32;
                let mut input = [[0.0f32; 2]; 480];
                for block in 0..200 {
                    for (n, frame) in input.iter_mut().enumerate() {
                        let t = (block * 480 + n) as f32 / 48_000.0;
                        let v = 0.03 * (2.0 * core::f32::consts::PI * hz * t).sin();
                        *frame = [v, v];
                    }
                    let mut out = input;
                    record.process_block(&mut out);
                    play.process_block(&mut out);
                    if block > 100 {
                        worst = worst.max(rms_db(&out) - rms_db(&input)).max(rms_db(&input) - rms_db(&out));
                    }
                }
                assert!(worst < 0.4, "{machine:?}/{tape:?} {hz} Hz: {worst} dB");
            }
        }
    }

    #[test]
    fn pink_noise_reads_flat_in_third_octave_bands_at_its_level() {
        let mut unit = Unit::new(48_000, Kind::II, Mode::Encode);
        press(&mut unit, Hit::Spectrum);
        press(&mut unit, Hit::Source);
        assert!(unit.spectrum() && unit.source() == Source::Output);
        unit.set_tone(Tone::Pink);
        unit.set_tone_db(-18.0);
        let mut analyzer = std::boxed::Box::new(ltas::Analyzer::<8192>::new());
        let mut config = ltas::Config::new(48_000);
        config.hop = 4096;
        assert!(analyzer.configure(config));
        let mut all = std::vec::Vec::new();
        for _ in 0..400 {
            let mut frames = [[0.0f32; 2]; 480];
            unit.process_block(&mut frames);
            assert!(frames.iter().all(|f| f[0] == f[1]));
            all.extend(frames.iter().map(|f| f[0]));
        }
        let total = rms_db(&all.iter().map(|&x| [x, x]).collect::<std::vec::Vec<_>>());
        assert!((total + 21.01).abs() < 0.7, "{total}");
        analyzer.push(&all);
        let mut levels = [0.0f32; 64];
        analyzer.levels_db(ltas::Mode::Linear, &mut levels);
        let mut checked = 0;
        for i in 0..analyzer.outputs() {
            let hz = analyzer.centre_hz(i);
            if (100.0..10_000.0).contains(&hz) {
                checked += 1;
                let band = levels[i];
                assert!((band - levels[10]).abs() < 2.0, "{hz} Hz {band} dB vs {}", levels[10]);
            }
        }
        assert!(checked >= 18);
    }

    #[test]
    fn buttons_are_hit_where_they_are_drawn() {
        let layout = Layout::new(1024, 768);
        for (rect, hit) in layout.buttons.iter().zip(BUTTONS) {
            assert_eq!(layout.hit(rect.x + rect.w / 2, rect.y + rect.h / 2), Some(hit));
        }
        assert_eq!(layout.hit(10, 10), None);
        assert!(layout.strips[0].x + layout.strips[0].w < layout.strips[1].x);
    }
}
