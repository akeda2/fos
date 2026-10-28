//! `miccal`: calibrate a measurement microphone. It listens to the audio input, draws the band levels the way `ltas` does and
//! turns them into a per-band correction (`crates/miccal` has the maths and the file formats), which `s` saves on the first
//! exFAT disk, else the FOSFS disk. Touch buttons and keys do the same things. A hard-float tape (see `ltas`).
#![no_std]
#![no_main]

mod view;

use core::fmt::Write;
use audio_screen::{Input, Playback, Screen, poll_keys};
use gfx::theme::theme;
use ltas::{Analyzer, Config, Mode};
use miccal::{Header, Inputs, MAX_POINTS, Method, Outcome};
use noise::{Generator, Kind as NoiseKind};
use tape_rt::{Key, KeyDecoder, Line, TapeApi, TouchEvent, alloc_zeroed, arg_value as value, parse_f32, println, save};

tape_rt::tape_help!(
    br#"miccal [device=N|loopN] [outdev=M|loopN] [rate=48000] [chan=1] [n=16384]
       [method=coupler|reference|calibrator] [source=off|pink|steps]
       [spl=94|perband=DB] [tone=94] [ref=FILE] [name=NAME] [save=N]
       [level=-20] [dwell=1.5] [out=MASK] [theme=dark|classic|light]
Calibrates a measurement microphone: 1/3-octave levels of the input are drawn
(dB re full scale) and become a correction (spl = level + correction), saved
as NAME.cal (and NAME-rew.txt, a REW-style deviation file) on the first exFAT
disk, else the first FAT32 disk, else the FOSFS disk. NAME is up to 8 of a-z
0-9 . _ - (default mic). Touch the buttons or use the keys. Methods:
 coupler    a closed pink-noise source (spl= its total level, or perband= the
            level in each 1/3-octave band) covers the mic: key 1 captures it
 reference  pink noise (or stepped sines) played out and measured with a
            reference mic (key 2), then the mic under test, same place (key 1);
            ref=FILE gives the reference mic's own correction (absolute)
 calibrator a tone of tone= dB SPL (94 at 1 kHz): key 3 captures the flat
            sensitivity; after a coupler or reference capture it fixes the level
Keys (Escape quits):  m method  g source off/pink/steps  1 mic  2 reference
  3 tone  r reset  space freeze  c clear  s save  + - generator level
  Up Down SPL (target or tone)  , . channel  k theme
"#
);

const MAXN: usize = 16384;
const MAXB: usize = MAX_POINTS;
const BLOCK: usize = 32 << 10;
const MAX_WIDTH: u32 = 1920;
const MAX_HEIGHT: u32 = 1080;
const NAN: f32 = f32::NAN;

static mut LEVELS: [f32; MAXN / 2 + 1] = [0.0; MAXN / 2 + 1];
static mut BLOCK_BYTES: [u8; BLOCK] = [0; BLOCK];
static mut SAMPLES: [f32; BLOCK / 2] = [0.0; BLOCK / 2];
static mut CAL_TEXT: [u8; 4096] = [0; 4096];
static mut REW_TEXT: [u8; 4096] = [0; 4096];
static mut REF_TEXT: [u8; 8192] = [0; 8192];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    Off,
    Pink,
    Steps,
}

impl Source {
    fn name(self) -> &'static str {
        match self {
            Source::Off => "off",
            Source::Pink => "pink",
            Source::Steps => "steps",
        }
    }

    fn from_name(name: &str) -> Option<Source> {
        [Source::Off, Source::Pink, Source::Steps].into_iter().find(|s| s.name() == name)
    }

    fn next(self) -> Source {
        match self {
            Source::Off => Source::Pink,
            Source::Pink => Source::Steps,
            Source::Steps => Source::Off,
        }
    }
}

/// The buttons, in screen order (row by row); keys do the same.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    Method,
    Source,
    Mic,
    Reference,
    Tone,
    Reset,
    Freeze,
    Clear,
    GenDown,
    GenUp,
    SplDown,
    SplUp,
    Channel,
    Save,
    Theme,
    Quit,
}

const BUTTON_ACTIONS: [Action; view::BUTTONS] = [
    Action::Method, Action::Source, Action::Mic, Action::Reference, Action::Tone, Action::Reset, Action::Freeze, Action::Clear,
    Action::GenDown, Action::GenUp, Action::SplDown, Action::SplUp, Action::Channel, Action::Save, Action::Theme, Action::Quit,
];

fn any(values: &[f32]) -> bool {
    values.iter().any(|v| !v.is_nan())
}

struct App {
    analyzer: &'static mut Analyzer<MAXN>,
    generator: Generator,
    input: Input,
    playback_ok: bool,
    rate: u32,
    method: Method,
    source: Source,
    gen_level: f32,
    coupler_db: f32,
    per_band: bool,
    tone_spl: f32,
    settle_us: u64,
    dwell_us: u64,
    count: usize,
    centres: [f32; MAXB],
    resolved: [bool; MAXB],
    current: [f32; MAXB],
    mic: [f32; MAXB],
    reference: [f32; MAXB],
    tone: [f32; MAXB],
    corr: [f32; MAXB],
    ref_cal: Option<[f32; MAXB]>,
    result: Result<Outcome, &'static str>,
    frozen: bool,
    step_band: usize,
    step_start: u64,
    step_settled: bool,
    saves: u32,
    name: &'static str,
    wanted_disk: Option<usize>,
    message: Line<80>,
}

impl App {
    fn say(&mut self, text: &str) {
        self.message.clear();
        let _ = write!(self.message, "{text}");
    }

    fn first_resolved(&self) -> usize {
        (0..self.count).find(|&i| self.resolved[i]).unwrap_or(0)
    }

    fn next_resolved(&self, from: usize) -> usize {
        (from + 1..self.count).find(|&i| self.resolved[i]).unwrap_or_else(|| self.first_resolved())
    }

    fn restart_steps(&mut self) {
        self.current.fill(NAN);
        self.analyzer.reset();
        self.step_band = self.first_resolved();
        self.generator.set_frequency(self.centres[self.step_band]);
        self.step_start = tape_rt::micros();
        self.step_settled = false;
    }

    fn set_source(&mut self, wanted: Source) {
        let mut source = wanted;
        if source != Source::Off && !self.playback_ok {
            self.say("the device has no output; the source stays off");
            source = Source::Off;
        }
        self.source = source;
        match source {
            Source::Off => self.generator.set_kind(NoiseKind::Silence),
            Source::Pink => self.generator.set_kind(NoiseKind::Pink),
            Source::Steps => self.generator.set_kind(NoiseKind::Sine),
        }
        self.generator.set_level_db(self.gen_level);
        self.analyzer.reset();
        self.current.fill(NAN);
        if source == Source::Steps {
            self.restart_steps();
        }
    }

    /// The running average of the input into `current` (a source of off or pink).
    fn update_current(&mut self, levels: &mut [f32]) {
        if self.analyzer.frames() == 0 {
            self.current.fill(NAN);
            return;
        }
        self.analyzer.levels_db(Mode::Linear, levels);
        for i in 0..self.count {
            self.current[i] = if self.resolved[i] && levels[i] > -150.0 { levels[i] } else { NAN };
        }
    }

    /// One band of the stepped-sine sweep: let the room settle, average, record, go to the next band.
    fn step(&mut self, levels: &mut [f32]) {
        let elapsed = tape_rt::micros().wrapping_sub(self.step_start);
        if !self.step_settled {
            if elapsed >= self.settle_us {
                self.analyzer.reset();
                self.step_settled = true;
            }
        } else if elapsed >= self.settle_us + self.dwell_us {
            self.analyzer.levels_db(Mode::Linear, levels);
            self.current[self.step_band] = if self.analyzer.frames() > 0 && levels[self.step_band] > -150.0 { levels[self.step_band] } else { NAN };
            self.step_band = self.next_resolved(self.step_band);
            self.generator.set_frequency(self.centres[self.step_band]);
            self.step_start = tape_rt::micros();
            self.step_settled = false;
        }
    }

    fn capture(&mut self, action: Action) {
        if !any(&self.current[..self.count]) {
            return self.say("nothing measured yet");
        }
        match action {
            Action::Mic => self.mic = self.current,
            Action::Reference => self.reference = self.current,
            _ => {
                let band = miccal::reference_band(&self.centres[..self.count]).unwrap_or(0);
                if self.current[band].is_nan() {
                    return self.say("no level at 1 kHz yet");
                }
                self.tone.fill(NAN);
                self.tone[band] = self.current[band];
            }
        }
        self.say(match action {
            Action::Mic => "mic captured",
            Action::Reference => "reference captured",
            _ => "tone captured (1 kHz band)",
        });
        self.recompute();
    }

    fn recompute(&mut self) {
        let count = self.count;
        let inputs = Inputs {
            centres: &self.centres[..count],
            measured: &self.mic[..count],
            reference: any(&self.reference[..count]).then_some(&self.reference[..count]),
            reference_correction: self.ref_cal.as_ref().map(|c| &c[..count]),
            tone: any(&self.tone[..count]).then_some(&self.tone[..count]),
            tone_spl: self.tone_spl,
            target_band_db: if self.per_band { self.coupler_db } else { miccal::pink_band_level(self.coupler_db, count) },
        };
        self.corr.fill(NAN);
        self.result = miccal::correction(self.method, &inputs, &mut self.corr[..count]);
        if self.result.is_err() {
            self.corr.fill(NAN);
        }
    }

    fn apply(&mut self, api: &TapeApi, action: Action) {
        self.message.clear();
        match action {
            Action::Method => {
                self.method = self.method.next();
                let source = if self.method == Method::Reference { Source::Pink } else { Source::Off };
                self.set_source(source);
            }
            Action::Source => self.set_source(self.source.next()),
            Action::Mic | Action::Reference | Action::Tone => self.capture(action),
            Action::Reset => {
                self.analyzer.reset();
                self.current.fill(NAN);
                if self.source == Source::Steps {
                    self.restart_steps();
                }
                self.say("reset");
            }
            Action::Freeze => {
                self.frozen = !self.frozen;
                if !self.frozen && self.source == Source::Steps {
                    self.step_start = tape_rt::micros();
                    self.step_settled = false;
                }
            }
            Action::Clear => {
                for curve in [&mut self.mic, &mut self.reference, &mut self.tone] {
                    curve.fill(NAN);
                }
                self.say("captures cleared");
            }
            Action::GenDown | Action::GenUp => {
                self.gen_level = (self.gen_level + if action == Action::GenUp { 1.0 } else { -1.0 }).clamp(-80.0, 0.0);
                self.generator.set_level_db(self.gen_level);
            }
            Action::SplDown | Action::SplUp => {
                let step = if action == Action::SplUp { 0.5 } else { -0.5 };
                if self.method == Method::Coupler {
                    self.coupler_db += step;
                } else {
                    self.tone_spl += step;
                }
            }
            Action::Channel => {
                if self.input.step_channel(Key::Char(b'.')) {
                    self.analyzer.reset();
                    self.current.fill(NAN);
                    let text = self.input.channel + 1;
                    self.message.clear();
                    let _ = write!(self.message, "input channel {text}");
                }
            }
            Action::Save => self.save(api),
            Action::Theme => {
                gfx::theme::next();
            }
            Action::Quit => {}
        }
        self.recompute();
    }

    fn save(&mut self, api: &TapeApi) {
        let outcome = match self.result {
            Ok(outcome) => outcome,
            Err(reason) => return self.say(reason),
        };
        self.saves += 1;
        let (mut unused, mut stamp) = (Line::<40>::new(), Line::<40>::new());
        save::names("miccal", self.saves, &mut unused, &mut stamp);
        let header = Header { name: self.name, stamp: stamp.as_str(), method: self.method, rate: self.rate, outcome };
        let (cal_bytes, rew_bytes) = unsafe { (&mut *(&raw mut CAL_TEXT), &mut *(&raw mut REW_TEXT)) };
        let (mut cal, mut rew) = (save::Sink::new(cal_bytes), save::Sink::new(rew_bytes));
        let n = self.count;
        if miccal::write_fos(&mut cal, &header, &self.centres[..n], &self.corr[..n]).is_err() || miccal::write_rew(&mut rew, &header, &self.centres[..n], &self.corr[..n]).is_err() {
            return self.say("file too big");
        }
        let base = file_base(self.name);
        let disk = save::find_disk(api, self.wanted_disk);
        let mut rew_base = Line::<16>::new();
        let _ = write!(rew_base, "{}-rew", base.as_str());
        if put(api, disk, base.as_str(), ".cal", cal.written()) != 0 || put(api, disk, rew_base.as_str(), ".txt", rew.written()) != 0 {
            return self.say("cannot write (disk full or read-only?)");
        }
        self.message.clear();
        let _ = match disk {
            Some(disk) => write!(self.message, "saved disk{disk}/{}.cal", base.as_str()),
            None => write!(self.message, "saved {}.cal on the FOSFS disk", base.as_str()),
        };
    }
}

/// `NAME` reduced to what a FOSFS name allows, at most 8 bytes, so `NAME-rew.txt` fits its 16.
fn file_base(name: &str) -> Line<8> {
    let mut base = Line::<8>::new();
    for byte in name.bytes().map(|b| b.to_ascii_lowercase()) {
        if base.length < 8 && (byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')) {
            base.bytes[base.length] = byte;
            base.length += 1;
        }
    }
    if base.is_empty() {
        let _ = write!(base, "mic");
    }
    base
}

/// Writes `diskN/BASE+EXT`, or `BASE+EXT` on the boot FOSFS volume when there is no exFAT/FAT32 disk; 0 on success.
fn put(api: &TapeApi, disk: Option<usize>, base: &str, extension: &str, data: &[u8]) -> isize {
    let mut name = Line::<48>::new();
    let _ = match disk {
        Some(disk) => write!(name, "disk{disk}/{base}{extension}"),
        None => write!(name, "{base}{extension}"),
    };
    (api.fs_write)(name.as_str().as_ptr(), name.length, data.as_ptr(), data.len())
}

/// The reference mic's own correction from `ref=FILE`, on `centres`; it has to be an absolute miccal file.
fn load_reference(api: &TapeApi, file: &str, centres: &[f32]) -> Result<[f32; MAXB], &'static str> {
    let buffer = unsafe { &mut *(&raw mut REF_TEXT) };
    let length = (api.fs_read)(file.as_ptr(), file.len(), buffer.as_mut_ptr(), buffer.len());
    if length < 0 {
        return Err("ref=: cannot read the file");
    }
    let text = core::str::from_utf8(&buffer[..length as usize]).map_err(|_| "ref=: not a text file")?;
    let (mut hz, mut db) = ([0.0; MAXB], [0.0; MAXB]);
    let parsed = miccal::parse(text, &mut hz, &mut db);
    if parsed.count == 0 {
        return Err("ref=: no frequency/correction lines in the file");
    }
    if !parsed.absolute {
        return Err("ref=: needs an absolute miccal file (a relative or REW file has no level)");
    }
    let mut out = [NAN; MAXB];
    miccal::interpolate(&hz[..parsed.count], &db[..parsed.count], centres, &mut out[..centres.len()]);
    Ok(out)
}

/// A finger lands once per touch; its movement and lift are not buttons.
struct Fingers {
    held: [Option<i32>; 8],
}

impl Fingers {
    fn pressed(&mut self, event: &TouchEvent) -> bool {
        if !event.down {
            for slot in &mut self.held {
                if *slot == Some(event.id) {
                    *slot = None;
                }
            }
            return false;
        }
        if self.held.contains(&Some(event.id)) {
            return false;
        }
        match self.held.iter_mut().find(|slot| slot.is_none()) {
            Some(slot) => {
                *slot = Some(event.id);
                true
            }
            None => false,
        }
    }
}

fn button_labels(app: &App) -> [(Line<16>, bool); view::BUTTONS] {
    core::array::from_fn(|i| {
        let mut label = Line::<16>::new();
        let mut active = false;
        let _ = match BUTTON_ACTIONS[i] {
            Action::Method => write!(label, "m {}", match app.method { Method::Coupler => "coupler", Method::Reference => "ref mic", Method::Calibrator => "calib" }),
            Action::Source => {
                active = app.source != Source::Off;
                write!(label, "g {}", app.source.name())
            }
            Action::Mic => {
                active = any(&app.mic[..app.count]);
                write!(label, "1 Mic")
            }
            Action::Reference => {
                active = any(&app.reference[..app.count]);
                write!(label, "2 Ref")
            }
            Action::Tone => {
                active = any(&app.tone[..app.count]);
                write!(label, "3 Tone")
            }
            Action::Reset => write!(label, "r Reset"),
            Action::Freeze => {
                active = app.frozen;
                write!(label, "Sp Freeze")
            }
            Action::Clear => write!(label, "c Clear"),
            Action::GenDown => write!(label, "- Gen"),
            Action::GenUp => write!(label, "+ Gen"),
            Action::SplDown => write!(label, "Dn SPL"),
            Action::SplUp => write!(label, "Up SPL"),
            Action::Channel => write!(label, ", Ch {}/{}", app.input.channel + 1, app.input.channels),
            Action::Save => write!(label, "s Save"),
            Action::Theme => write!(label, "k Theme"),
            Action::Quit => write!(label, "Esc Quit"),
        };
        (label, active)
    })
}

fn draw(screen: &mut Screen, layout: &view::Layout, app: &App) {
    let mut canvas = screen.frame();
    let n = app.count;
    view::draw_curves(
        &mut canvas,
        layout,
        &view::Curves { centres: &app.centres[..n], current: &app.current[..n], mic: &app.mic[..n], reference: &app.reference[..n], tone: &app.tone[..n], correction: &app.corr[..n] },
    );
    let mut line = Line::<120>::new();
    let _ = write!(line, "MICCAL  {} Hz  ch {}/{}  {}  source {}", app.rate, app.input.channel + 1, app.input.channels, app.method.name(), app.source.name());
    if app.source != Source::Off {
        let _ = write!(line, " {:+.0} dB", app.gen_level);
    }
    let _ = write!(line, "  frames {}{}", app.analyzer.frames(), if app.frozen { "  FROZEN" } else { "" });
    canvas.text_scaled(8, layout.row(0), line.as_str(), layout.text, theme().text);
    if !app.message.is_empty() {
        let width = gfx::text_width(app.message.as_str()) as i32 * layout.text as i32;
        canvas.text_scaled(layout.width as i32 - width - 8, layout.row(0), app.message.as_str(), layout.text, theme().curves[1]);
    }

    let mut x = 8;
    for (name, colour, set) in [
        ("live", theme().curves[1], true),
        ("mic", theme().curves[2], any(&app.mic[..n])),
        ("ref", theme().curves[3], any(&app.reference[..n])),
        ("tone", theme().amber, any(&app.tone[..n])),
    ] {
        x = canvas.text_scaled(x, layout.row(1), name, layout.text, if set { colour } else { theme().dim }) + 16 * layout.text as i32;
    }
    let mut info = Line::<120>::new();
    if app.method == Method::Calibrator || any(&app.tone[..n]) {
        let _ = write!(info, "tone {:.1} dB SPL  ", app.tone_spl);
    }
    if app.method == Method::Coupler {
        if app.per_band {
            let _ = write!(info, "target {:.1} dB/band  ", app.coupler_db);
        } else {
            let _ = write!(info, "target {:.1} dB SPL pink  ", app.coupler_db);
        }
    }
    let colour = match app.result {
        Ok(outcome) => {
            let band = miccal::reference_band(&app.centres[..n]).unwrap_or(0);
            let _ = write!(info, "correction {} {:+.1} dB at 1 kHz", if outcome.absolute { "absolute" } else { "relative" }, app.corr[band]);
            theme().green
        }
        Err(reason) => {
            let _ = write!(info, "{reason}");
            theme().amber
        }
    };
    canvas.text_scaled(x, layout.row(1), info.as_str(), layout.text, colour);
    let labels = button_labels(app);
    let shown: [(&str, bool); view::BUTTONS] = core::array::from_fn(|i| (labels[i].0.as_str(), labels[i].1));
    view::draw_buttons(&mut canvas, layout, &shown);
    screen.present();
}

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(api: &'static TapeApi) {
    tape_rt::init(api);
    let started = match audio_screen::startup(api, MAX_WIDTH, MAX_HEIGHT) {
        Ok(started) => started,
        Err(reason) => return println!("miccal: {reason}."),
    };
    let (arguments, width, height) = (started.arguments, started.width, started.height);
    if let Some(name) = value(arguments, "theme") {
        if !gfx::theme::set_by_name(name) {
            return println!("miccal: theme= takes dark, classic or light.");
        }
    }
    let method = match value(arguments, "method") {
        None => Method::Coupler,
        Some(name) => match Method::from_name(name) {
            Some(method) => method,
            None => return println!("miccal: method= takes coupler, reference or calibrator."),
        },
    };
    let source = match value(arguments, "source") {
        None => if method == Method::Reference { Source::Pink } else { Source::Off },
        Some(name) => match Source::from_name(name) {
            Some(source) => source,
            None => return println!("miccal: source= takes off, pink or steps."),
        },
    };
    let rate: u32 = value(arguments, "rate").and_then(|v| v.parse().ok()).unwrap_or(48_000);
    let n: usize = value(arguments, "n").and_then(|v| v.parse().ok()).filter(|n| [2048, 4096, 8192, 16384].contains(n)).unwrap_or(16384);
    let chan = value(arguments, "chan").and_then(|v| v.parse::<usize>().ok()).unwrap_or(1);
    let input = match Input::query(api, chan) {
        Ok(input) => input,
        Err(reason) => return println!("miccal: {reason}."),
    };
    let playback_ok = input.playback_ok();

    let (Some(analyzer), Some(screen)) = (alloc_zeroed::<Analyzer<MAXN>>(api), Screen::new(api, width, height)) else {
        return println!("miccal: out of memory.");
    };
    let mut screen = screen.on(started.screen);
    let mut config = Config::new(rate);
    config.n = n;
    config.hop = n / 2;
    config.avx2 = tape_rt::fp_level() >= tape_rt::FP_AVX2;
    if !analyzer.configure(config) || analyzer.outputs() > MAXB {
        return println!("miccal: cannot configure the analyser at {rate} Hz.");
    }

    let count = analyzer.outputs();
    let mut centres = [NAN; MAXB];
    let mut resolved = [false; MAXB];
    for i in 0..count {
        centres[i] = analyzer.centre_hz(i);
        resolved[i] = analyzer.band_is_resolved(i);
    }
    let ref_cal = match value(arguments, "ref") {
        None => None,
        Some(file) => match load_reference(api, file, &centres[..count]) {
            Ok(curve) => Some(curve),
            Err(reason) => return println!("miccal: {reason}."),
        },
    };
    let dwell = value(arguments, "dwell").and_then(parse_f32).unwrap_or(1.5).clamp(0.2, 30.0);
    let settle = (n as f32 / rate as f32 + 0.25).max(0.5);
    let per_band = value(arguments, "perband").and_then(parse_f32);
    let mut generator = Generator::new();
    generator.set_rate(rate);
    let mut app = App {
        analyzer,
        generator,
        input,
        playback_ok,
        rate,
        method,
        source: Source::Off,
        gen_level: value(arguments, "level").and_then(parse_f32).unwrap_or(-20.0),
        coupler_db: per_band.or(value(arguments, "spl").and_then(parse_f32)).unwrap_or(94.0),
        per_band: per_band.is_some(),
        tone_spl: value(arguments, "tone").and_then(parse_f32).unwrap_or(94.0),
        settle_us: (settle * 1e6) as u64,
        dwell_us: (dwell * 1e6) as u64,
        count,
        centres,
        resolved,
        current: [NAN; MAXB],
        mic: [NAN; MAXB],
        reference: [NAN; MAXB],
        tone: [NAN; MAXB],
        corr: [NAN; MAXB],
        ref_cal,
        result: Err("capture the mic first (key 1)"),
        frozen: false,
        step_band: 0,
        step_start: 0,
        step_settled: false,
        saves: 0,
        name: value(arguments, "name").filter(|n| !n.is_empty()).unwrap_or("mic"),
        wanted_disk: value(arguments, "save").and_then(|v| v.parse().ok()),
        message: Line::new(),
    };
    let mut playback = Playback::new(api, &app.input, value(arguments, "out").and_then(|v| v.parse().ok()).unwrap_or(0b11));
    if let Err(status) = app.input.start(api, rate) {
        return println!("miccal: cannot start audio at {rate} Hz (code {status}); the device offers {:?}.", &app.input.rates[..app.input.rate_count]);
    }
    if !screen.open() {
        (api.audio_stop)();
        return println!("miccal: cannot take the screen.");
    }
    app.set_source(source);
    app.recompute();
    if app.message.is_empty() {
        app.say("ready");
    }

    let layout = view::Layout::new(width, height);
    view::draw_grid(&mut screen.grid(), &layout);
    let (levels, block, samples) = unsafe { (&mut *(&raw mut LEVELS), &mut *(&raw mut BLOCK_BYTES), &mut *(&raw mut SAMPLES)) };
    let mut decoder = KeyDecoder::new();
    let mut fingers = Fingers { held: [None; 8] };
    let mut quit = false;
    while !quit {
        let mut actions = [None::<Action>; 8];
        let mut queued = 0;
        let mut push = |action: Action, actions: &mut [Option<Action>; 8]| {
            if queued < actions.len() {
                actions[queued] = Some(action);
                queued += 1;
            }
        };
        let keep_going = poll_keys(api, &mut decoder, |key| {
            let action = match key {
                Key::Char(b'm') => Action::Method,
                Key::Char(b'g') => Action::Source,
                Key::Char(b'1') => Action::Mic,
                Key::Char(b'2') => Action::Reference,
                Key::Char(b'3') => Action::Tone,
                Key::Char(b'r') => Action::Reset,
                Key::Char(b' ') => Action::Freeze,
                Key::Char(b'c') => Action::Clear,
                Key::Char(b'-') => Action::GenDown,
                Key::Char(b'+') => Action::GenUp,
                Key::Down => Action::SplDown,
                Key::Up => Action::SplUp,
                Key::Char(b'.') | Key::Char(b',') => Action::Channel,
                Key::Char(b's') => Action::Save,
                Key::Char(b'k') => Action::Theme,
                _ => return,
            };
            push(action, &mut actions);
        });
        if !keep_going {
            break;
        }
        while let Some(event) = tape_rt::touch_event() {
            let Some(extent) = tape_rt::touch_extent(event.panel) else { continue };
            if event.screen >= 0 && event.screen != started.screen as i32 {
                continue;
            }
            if fingers.pressed(&event) {
                let (x, y) = (event.x * width as i32 / extent.0.max(1), event.y * height as i32 / extent.1.max(1));
                if let Some(button) = layout.button_at(x, y) {
                    push(BUTTON_ACTIONS[button], &mut actions);
                }
            }
        }
        for action in actions.into_iter().flatten() {
            if action == Action::Quit {
                quit = true;
            } else {
                app.apply(api, action);
                if action == Action::Theme {
                    view::draw_grid(&mut screen.grid(), &layout);
                }
            }
            screen.mark_dirty();
        }

        let mut worked = false;
        let count = app.input.read(api, &mut block[..], &mut samples[..], app.input.channel);
        if count > 0 {
            if !app.frozen && app.analyzer.push(&samples[..count]) > 0 {
                screen.mark_dirty();
            }
            worked = true;
        }
        if !app.frozen {
            match app.source {
                Source::Steps => app.step(levels),
                _ => app.update_current(levels),
            }
            app.recompute();
        }
        if let Some(playback) = playback.as_mut() {
            let generator = &mut app.generator;
            worked |= playback.pump(api, &app.input, |mono| generator.fill(mono));
        }
        if screen.due() {
            draw(&mut screen, &layout, &app);
        }
        if !worked {
            tape_rt::idle();
        }
    }
    (api.audio_stop)();
    screen.close();
    let mut stats = tape_rt::AudioStats::default();
    (api.audio_stats)(&mut stats);
    println!("miccal: {} frame(s) analysed, {} file set(s) saved; capture overruns {}, playback underruns {}.", app.analyzer.frames(), app.saves, stats.capture_overruns, stats.playback_underruns);
}
