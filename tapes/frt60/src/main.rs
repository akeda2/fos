//! `frt60`: measure a room's reverberation (RT60, EDT, clarity) per octave or 1/3-octave band from a balloon pop or an
//! exponential sweep, and draw it as a curve, a waterfall, a colour map and a single band's decay. `crates/rt60` has the
//! maths; this is the screen, the capture and the files. A hard-float tape (see `ltas`, `miccal`).
#![no_std]
#![no_main]

mod help;
mod view;

use core::fmt::Write;
use audio_screen::{Input, Playback, Screen, poll_keys};
use dsp::fft::Plan;
use dsp::math::{exp64, log10};
use gfx::theme::theme;
use rt60::files::{Header, wav_header, write_csv};
use rt60::{Band, Capture, MAX_BANDS, Report, Sweep, Waterfall, direct_sound, measure, waterfall_span};
use tape_rt::{Key, KeyDecoder, Line, TapeApi, TouchEvent, alloc_zeroed, arg_value as value, parse_f32, println, save};
use view::{Data, Layout, View};

tape_rt::tape_help!(
    br#"frt60 [device=N|loopN] [outdev=M|loopN] [rate=48000] [chan=1]
      [method=balloon|sweep] [bands=octave|third] [view=summary|waterfall|
      heat|decay] [thresh=-30] [len=4] [level=-12] [name=NAME] [save=N]
      [out=MASK] [theme=dark|classic|light] [text=1|2|auto] [simroom=SECONDS]
Measures a room's reverberation time per band. Pop a balloon (method=balloon,
key Space arms a trigger with a pre-roll; the recording is len= seconds) or
play an exponential sweep through a loudspeaker (method=sweep, needs an
output, takes about 11 s at 48 kHz). EDT, T20, T30, C50, C80 and D50 are
worked out per band by Schroeder backward integration (ISO 3382); a time with
too little dynamic range is left blank. Views: summary (T against frequency),
waterfall, heat map (time downward) and one band's decay with its T20 fit.
s saves NAME.csv, NAME.wav (the impulse response, 32-bit float) and NAME.png on
the first exFAT disk, else FAT32, else FOSFS (NAME up to 8 of a-z 0-9 . _ -;
without name= the exFAT/FAT32 files are stamped with the date and time).
text=1 sets the screen text and buttons small, 2 large (auto: large from
1280x700 up).
simroom=SEC: a self-test; Space plays a synthetic pop of that RT60 instead.
Keys (Escape quits):  Space arm/start/stop  m balloon/sweep  b octave/third
  v view  Left Right band (or tap the plot)  Up Down trigger/output level
  n length  , . channel  c clear  s save  k theme  h help (what it all means)
"#
);

/// The longest recording and the transform size of the sweep: 2^19 samples (10.9 s at 48 kHz).
const MAXN: usize = 1 << 19;
const BLOCK: usize = 32 << 10;
const MAX_WIDTH: u32 = 1920;
const MAX_HEIGHT: u32 = 1080;
const LENGTHS: [f32; 5] = [2.0, 3.0, 4.0, 6.0, 8.0];

static mut BLOCK_BYTES: [u8; BLOCK] = [0; BLOCK];
static mut SAMPLES: [f32; BLOCK / 2] = [0.0; BLOCK / 2];
static mut WATERFALL: Waterfall = Waterfall::new();

#[derive(Clone, Copy, PartialEq, Eq)]
enum Method {
    Balloon,
    Sweep,
}

impl Method {
    fn name(self) -> &'static str {
        match self {
            Method::Balloon => "balloon",
            Method::Sweep => "sweep",
        }
    }

    fn from_name(name: &str) -> Option<Method> {
        [Method::Balloon, Method::Sweep].into_iter().find(|m| m.name() == name)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    /// Balloon: listening, waiting for the pop.
    Armed,
    Recording,
    Sweeping,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    Start,
    Method,
    Bands,
    View,
    Prev,
    Next,
    LevelDown,
    LevelUp,
    Length,
    Channel,
    Clear,
    Save,
    Theme,
    Help,
    Quit,
}

const BUTTON_ACTIONS: [Action; view::BUTTONS] = [
    Action::Start, Action::Method, Action::Bands, Action::View, Action::Prev, Action::Next, Action::LevelDown,
    Action::LevelUp, Action::Length, Action::Channel, Action::Clear, Action::Save, Action::Theme, Action::Help,
    Action::Quit,
];

/// The memory only the sweep needs: the twiddle tables and the four transform buffers (about 12 MB).
struct SweepMemory {
    plan: &'static mut Plan<MAXN>,
    rec_re: &'static mut [f32],
    rec_im: &'static mut [f32],
    ref_re: &'static mut [f32],
    ref_im: &'static mut [f32],
}

fn alloc_f32(api: &TapeApi, count: usize) -> Option<&'static mut [f32]> {
    let bytes = count * 4;
    let address = (api.alloc)(bytes);
    if address == 0 {
        return None;
    }
    unsafe {
        core::ptr::write_bytes(address as *mut u8, 0, bytes);
        Some(core::slice::from_raw_parts_mut(address as *mut f32, count))
    }
}

impl SweepMemory {
    fn new(api: &TapeApi) -> Option<Self> {
        let plan = alloc_zeroed::<Plan<MAXN>>(api)?;
        if !plan.init(MAXN) {
            return None;
        }
        Some(Self { plan, rec_re: alloc_f32(api, MAXN)?, rec_im: alloc_f32(api, MAXN)?, ref_re: alloc_f32(api, MAXN)?, ref_im: alloc_f32(api, MAXN)? })
    }
}

/// `simroom=`: a synthetic pop, a click and then white noise falling 60 dB in `t60` seconds, played through the output so the
/// loopback device carries it to the trigger (the end-to-end test of the whole chain with a known answer).
struct Pop {
    position: usize,
    amplitude: f32,
    decay: f32,
    state: u64,
}

impl Pop {
    fn new(t60: f32, rate: u32) -> Self {
        Self { position: 0, amplitude: 0.15, decay: exp64(-6.907755 / (t60 as f64 * rate as f64)) as f32, state: 12345 }
    }

    fn noise(&mut self) -> f32 {
        let mut sum = 0.0;
        for _ in 0..4 {
            self.state = self.state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            sum += (self.state >> 40) as f32 / (1u64 << 24) as f32 - 0.5;
        }
        sum * 1.732
    }

    /// Fills `out`; false once the pop has died away (80 dB down).
    fn fill(&mut self, out: &mut [f32], rate: u32) -> bool {
        let delay = rate as usize / 10;
        for v in out.iter_mut() {
            *v = if self.position < delay {
                0.0
            } else if self.position == delay {
                0.8
            } else {
                self.amplitude *= self.decay;
                self.noise() * self.amplitude
            };
            self.position += 1;
        }
        self.position <= delay || self.amplitude > 1.5e-5
    }
}

fn db_to_linear(db: f32) -> f32 {
    exp64(db as f64 * core::f64::consts::LN_10 / 20.0) as f32
}

fn finite(value: f32) -> bool {
    value.is_finite()
}

struct App {
    rate: u32,
    band: Band,
    method: Method,
    view: View,
    /// The help page shown instead of the plot, 1-based; 0 when it is closed.
    help: usize,
    input: Input,
    playback_ok: bool,
    buffer: *mut f32,
    capture: Option<Capture<'static>>,
    work: &'static mut [f32],
    sweep_memory: Option<SweepMemory>,
    waterfall: &'static mut Waterfall,
    phase: Phase,
    thresh_db: f32,
    level_db: f32,
    length_s: f32,
    sweep: Sweep,
    sweep_position: usize,
    pop: Option<Pop>,
    pop_t60: Option<f32>,
    reports: [Report; MAX_BANDS + 1],
    centres: [f32; MAX_BANDS],
    count: usize,
    selected: usize,
    has_result: bool,
    ir_start: usize,
    ir_end: usize,
    ir_in_sweep: bool,
    curve_for: Option<usize>,
    live_peak: f32,
    clipped: bool,
    latency_ms: f32,
    saves: u32,
    name: Option<&'static str>,
    wanted_disk: Option<usize>,
    message: Line<96>,
}

impl App {
    fn say(&mut self, text: &str) {
        self.message.clear();
        let _ = write!(self.message, "{text}");
    }

    fn drain(&self, api: &TapeApi, block: &mut [u8], samples: &mut [f32]) {
        for _ in 0..64 {
            if self.input.read(api, block, samples, self.input.channel) == 0 {
                break;
            }
        }
    }

    fn ir(&self) -> &[f32] {
        let all: &[f32] = match (&self.sweep_memory, self.ir_in_sweep) {
            (Some(memory), true) => &memory.rec_re[..],
            _ => self.capture.as_ref().map(|c| c.recorded()).unwrap_or(&[]),
        };
        &all[self.ir_start.min(all.len())..self.ir_end.min(all.len())]
    }

    fn set_method(&mut self, api: &TapeApi, method: Method) {
        if method == Method::Sweep {
            if !self.playback_ok {
                return self.say("the sweep needs an audio output; the device has none");
            }
            if self.sweep_memory.is_none() {
                self.sweep_memory = SweepMemory::new(api);
                if self.sweep_memory.is_none() {
                    return self.say("out of memory for the sweep");
                }
            }
        }
        self.method = method;
        self.phase = Phase::Idle;
        self.pop = None;
    }

    /// Starts listening (balloon) or playing (sweep), or cancels what is going on.
    fn start(&mut self, api: &TapeApi, block: &mut [u8], samples: &mut [f32]) {
        if self.phase != Phase::Idle {
            self.phase = Phase::Idle;
            self.pop = None;
            return self.say("stopped");
        }
        self.drain(api, block, samples);
        self.clipped = false;
        let rate = self.rate as usize;
        let (pre, total) = match self.method {
            Method::Balloon => (rate / 4, (rate / 4 + (self.length_s * rate as f32) as usize).min(MAXN)),
            Method::Sweep => (0, MAXN),
        };
        let slice = unsafe { core::slice::from_raw_parts_mut(self.buffer, total) };
        let mut capture = Capture::new(slice, pre);
        match self.method {
            Method::Balloon => {
                capture.arm(db_to_linear(self.thresh_db));
                self.phase = Phase::Armed;
                if let Some(t60) = self.pop_t60 {
                    self.pop = Some(Pop::new(t60, self.rate));
                }
                self.say("armed: pop the balloon");
            }
            Method::Sweep => {
                capture.arm(0.0);
                self.sweep_position = 0;
                self.phase = Phase::Sweeping;
                self.say("sweeping");
            }
        }
        self.capture = Some(capture);
    }

    /// The recording is complete: find the impulse response (deconvolving a sweep) and analyse it.
    fn finish(&mut self, api: &TapeApi) {
        self.phase = Phase::Idle;
        let Some(capture) = self.capture.as_ref() else { return };
        self.clipped = capture.clipped();
        let recorded = capture.recorded();
        match self.method {
            Method::Balloon => {
                self.ir_start = direct_sound(recorded);
                self.ir_end = recorded.len();
                self.ir_in_sweep = false;
                self.latency_ms = 0.0;
            }
            Method::Sweep => {
                let Some(memory) = self.sweep_memory.as_mut() else { return };
                memory.rec_re[..recorded.len()].copy_from_slice(recorded);
                memory.rec_re[recorded.len()..].fill(0.0);
                memory.rec_im.fill(0.0);
                memory.ref_im.fill(0.0);
                self.sweep.fill(&mut memory.ref_re[..self.sweep.samples], db_to_linear(self.level_db));
                memory.ref_re[self.sweep.samples..].fill(0.0);
                let avx2 = tape_rt::fp_level() >= tape_rt::FP_AVX2;
                rt60::sweep::deconvolve(memory.plan, avx2, memory.rec_re, memory.rec_im, memory.ref_re, memory.ref_im);
                // The loudspeaker's harmonics land before the response (negative time = the end of the circular result): keep
                // the response clear of them. The 10th harmonic sits ln(10)/ln(f2/f1) of the sweep length before it.
                let margin = (self.sweep.samples as f32 * 0.4) as usize;
                let end = MAXN - margin;
                self.ir_start = direct_sound(&memory.rec_re[..end]);
                self.ir_end = end;
                self.ir_in_sweep = true;
                self.latency_ms = self.ir_start as f32 * 1000.0 / self.rate as f32;
            }
        }
        let _ = api;
        if self.ir_end <= self.ir_start + self.rate as usize / 10 {
            self.has_result = false;
            return self.say("the recording is too short after the direct sound");
        }
        self.has_result = true;
        self.analyse();
        if !self.reports[self.count].dynamic_db.is_finite() || self.reports[self.count].dynamic_db <= 0.0 {
            self.has_result = false;
            self.say("no decay found: too quiet, or nothing popped");
        } else {
            self.say("done");
        }
    }

    /// All bands plus the overall figure from the stored impulse response, and the waterfall.
    fn analyse(&mut self) {
        let all: &[f32] = match (&self.sweep_memory, self.ir_in_sweep) {
            (Some(memory), true) => &memory.rec_re[..],
            _ => self.capture.as_ref().map(|c| c.recorded()).unwrap_or(&[]),
        };
        let ir = &all[self.ir_start.min(all.len())..self.ir_end.min(all.len())];
        let count = self.band.count();
        self.count = count;
        let total = measure(ir, self.rate, None, self.work);
        let mut span = waterfall_span(&[total]);
        if !finite(total.t20) && !finite(total.edt) {
            span = total.used_s.clamp(0.3, 6.0);
        }
        self.waterfall.clear(count, span);
        self.reports[count] = total;
        for i in 0..count {
            let edges = self.band.edges(i);
            self.centres[i] = edges.1;
            if edges.0 >= self.rate as f32 * 0.45 {
                self.reports[i] = Report { centre_hz: edges.1, ..Report::NONE };
                continue;
            }
            self.reports[i] = measure(ir, self.rate, Some(edges), self.work);
            self.waterfall.set(i, self.work, self.rate);
        }
        self.selected = self.selected.min(count);
        self.curve_for = None;
    }

    /// The decay curve of the selected band, for the decay view.
    fn ensure_curve(&mut self) {
        if !self.has_result || self.curve_for == Some(self.selected) {
            return;
        }
        let all: &[f32] = match (&self.sweep_memory, self.ir_in_sweep) {
            (Some(memory), true) => &memory.rec_re[..],
            _ => self.capture.as_ref().map(|c| c.recorded()).unwrap_or(&[]),
        };
        let ir = &all[self.ir_start.min(all.len())..self.ir_end.min(all.len())];
        let edges = (self.selected < self.count).then(|| self.band.edges(self.selected));
        measure(ir, self.rate, edges, self.work);
        self.curve_for = Some(self.selected);
    }

    fn apply(&mut self, api: &TapeApi, action: Action, block: &mut [u8], samples: &mut [f32]) {
        self.message.clear();
        match action {
            Action::Start => self.start(api, block, samples),
            Action::Method => {
                let next = if self.method == Method::Balloon { Method::Sweep } else { Method::Balloon };
                self.set_method(api, next);
            }
            Action::Bands => {
                self.band = self.band.next();
                self.selected = self.band.count();
                if self.has_result {
                    self.analyse();
                }
            }
            Action::View => self.view = self.view.next(),
            Action::Prev => self.selected = if self.selected == 0 { self.count } else { self.selected - 1 },
            Action::Next => self.selected = if self.selected >= self.count { 0 } else { self.selected + 1 },
            Action::LevelDown | Action::LevelUp => {
                let step = if action == Action::LevelUp { 3.0 } else { -3.0 };
                match self.method {
                    Method::Balloon => self.thresh_db = (self.thresh_db + step).clamp(-80.0, -3.0),
                    Method::Sweep => self.level_db = (self.level_db + step).clamp(-60.0, -1.0),
                }
                let _ = match self.method {
                    Method::Balloon => write!(self.message, "trigger {:+.0} dB", self.thresh_db),
                    Method::Sweep => write!(self.message, "output {:+.0} dB", self.level_db),
                };
            }
            Action::Length => {
                let limit = (MAXN - self.rate as usize / 4) as f32 / self.rate as f32;
                let next = LENGTHS.iter().copied().find(|&l| l > self.length_s && l <= limit).unwrap_or(LENGTHS[0]);
                self.length_s = next;
            }
            Action::Channel => {
                if self.input.step_channel(Key::Char(b'.')) {
                    let text = self.input.channel + 1;
                    self.say("input channel ");
                    let _ = write!(self.message, "{text}");
                }
            }
            Action::Clear => {
                self.has_result = false;
                self.phase = Phase::Idle;
                self.pop = None;
                self.say("cleared");
            }
            Action::Save | Action::Help | Action::Quit => {}
            Action::Theme => {
                gfx::theme::next();
            }
        }
    }

    fn save(&mut self, api: &TapeApi, frame: &[u32], width: u32, height: u32) {
        if !self.has_result {
            return self.say("nothing to save yet");
        }
        self.saves += 1;
        let disk = save::find_disk(api, self.wanted_disk);
        let mut base = Line::<40>::new();
        let mut stamp = Line::<40>::new();
        save::names("frt60", self.saves, &mut Line::<40>::new(), &mut stamp);
        match (self.name, disk) {
            (Some(name), _) => {
                let _ = write!(base, "{name}");
            }
            (None, Some(_)) => save::names("frt60", self.saves, &mut base, &mut Line::<40>::new()),
            (None, None) => {
                let _ = write!(base, "rt{:03}", self.saves);
            }
        }
        let header = Header { name: self.name.unwrap_or("room"), method: self.method.name(), rate: self.rate, stamp: stamp.as_str() };
        let (band, count) = (self.band, self.count);
        let reports = &self.reports[..=count];
        let result = save::with_buffers(api, |csv_bytes, png_bytes| {
            let mut csv = save::Sink::new(csv_bytes);
            let mut png = save::Sink::new(png_bytes);
            write_csv(&mut csv, &header, &band, reports).map_err(|_| "CSV too big")?;
            let mut fits = true;
            if !gfx::png::write_rgb(&frame[..(width * height) as usize], width, height, width, |chunk| fits &= png.push(chunk)) || !fits {
                return Err("PNG too big");
            }
            if put(api, disk, base.as_str(), "csv", csv.written()) != 0 || put(api, disk, base.as_str(), "png", png.written()) != 0 {
                return Err("cannot write (disk full or read-only?)");
            }
            Ok(())
        })
        .unwrap_or(Err("out of memory"));
        if let Err(reason) = result {
            return self.say(reason);
        }
        // The impulse response, as many samples as one buffer holds.
        let ir = self.ir();
        let frames = ir.len().min((save::BUFFER_BYTES - 44) / 4);
        let rate = self.rate;
        let wrote = save::with_buffers(api, |wav_bytes, _| {
            let mut wav = save::Sink::new(wav_bytes);
            wav.push(&wav_header(rate, frames));
            for &x in &ir[..frames] {
                wav.push(&x.to_le_bytes());
            }
            put(api, disk, base.as_str(), "wav", wav.written())
        });
        if wrote != Some(0) {
            return self.say("cannot write the WAV");
        }
        self.message.clear();
        let _ = match disk {
            Some(disk) => write!(self.message, "saved disk{disk}/{}.csv .wav .png", base.as_str()),
            None => write!(self.message, "saved {}.csv .wav .png (FOSFS)", base.as_str()),
        };
    }
}

/// Writes `diskN/BASE.EXT`, or `BASE.EXT` on the boot FOSFS volume when there is no exFAT/FAT32 disk; 0 on success.
fn put(api: &TapeApi, disk: Option<usize>, base: &str, extension: &str, data: &[u8]) -> isize {
    match disk {
        Some(disk) => save::write_file(api, disk, base, extension, data),
        None => {
            let mut name = Line::<48>::new();
            let _ = write!(name, "{base}.{extension}");
            (api.fs_write)(name.as_str().as_ptr(), name.length, data.as_ptr(), data.len())
        }
    }
}

/// `NAME` reduced to what a FOSFS name allows, at most 8 bytes, so `NAME.csv` fits its 16.
fn file_base(name: &str) -> Line<8> {
    let mut base = Line::<8>::new();
    for byte in name.bytes().map(|b| b.to_ascii_lowercase()) {
        if base.length < 8 && (byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')) {
            base.bytes[base.length] = byte;
            base.length += 1;
        }
    }
    base
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
            Action::Start => {
                active = app.phase != Phase::Idle;
                match (app.phase, app.method) {
                    (Phase::Idle, Method::Balloon) => write!(label, "Sp Arm"),
                    (Phase::Idle, Method::Sweep) => write!(label, "Sp Sweep"),
                    _ => write!(label, "Sp Stop"),
                }
            }
            Action::Method => write!(label, "m {}", app.method.name()),
            Action::Bands => write!(label, "b {}", app.band.name()),
            Action::View => write!(label, "v {}", app.view.name()),
            Action::Prev => write!(label, "Lf Band"),
            Action::Next => write!(label, "Rt Band"),
            Action::LevelDown => write!(label, "Dn {}", if app.method == Method::Balloon { "Trig" } else { "Out" }),
            Action::LevelUp => write!(label, "Up {}", if app.method == Method::Balloon { "Trig" } else { "Out" }),
            Action::Length => write!(label, "n Len {:.0}s", app.length_s),
            Action::Channel => write!(label, ", Ch {}/{}", app.input.channel + 1, app.input.channels),
            Action::Clear => write!(label, "c Clear"),
            Action::Save => write!(label, "s Save"),
            Action::Theme => write!(label, "k Theme"),
            Action::Help => {
                active = app.help != 0;
                write!(label, "h Help")
            }
            Action::Quit => write!(label, "Esc Quit"),
        };
        (label, active)
    })
}

fn number(line: &mut impl Write, label: &str, value: f32, decimals: usize, unit: &str) {
    if value.is_finite() {
        let _ = write!(line, "{label} {value:.decimals$}{unit}  ");
    } else {
        let _ = write!(line, "{label} --  ");
    }
}

fn draw(screen: &mut Screen, layout: &Layout, app: &App, cursor: Option<(i32, i32)>, clean: bool) {
    let mut canvas = screen.frame();
    let n = app.count;
    let data = Data {
        centres: &app.centres[..n],
        reports: &app.reports[..if app.has_result { n + 1 } else { 0 }],
        waterfall: &*app.waterfall,
        curve: (app.has_result && app.curve_for == Some(app.selected)).then(|| (&app.work[..app.ir_end - app.ir_start], app.rate)),
        selected: app.selected,
    };
    if app.help != 0 {
        help::draw(&mut canvas, layout, app.help - 1);
    } else {
        view::draw_plot(&mut canvas, layout, app.view, &data);
    }

    let mut line = Line::<120>::new();
    let _ = write!(line, "FRT60  {} Hz  ch {}/{}  {}  {}  {}", app.rate, app.input.channel + 1, app.input.channels, app.method.name(), app.band.name(), app.view.name());
    canvas.text_scaled(8, layout.row(0), line.as_str(), layout.text, theme().text);
    if !clean && !app.message.is_empty() {
        let width = gfx::text_width(app.message.as_str()) as i32 * layout.text as i32;
        canvas.text_scaled(layout.width as i32 - width - 8, layout.row(0), app.message.as_str(), layout.text, theme().curves[1]);
    }

    let mut status = Line::<140>::new();
    let colour = match app.phase {
        Phase::Idle => {
            if app.has_result {
                let _ = write!(status, "{}: impulse response {:.2} s", app.method.name(), (app.ir_end - app.ir_start) as f32 / app.rate as f32);
                if app.method == Method::Sweep {
                    let _ = write!(status, ", latency {:.1} ms", app.latency_ms);
                }
                theme().green
            } else {
                let _ = match app.method {
                    Method::Balloon => write!(status, "ready: trigger {:+.0} dB. Space arms it, then pop the balloon", app.thresh_db),
                    Method::Sweep => write!(status, "ready: output {:+.0} dB. Space plays the sweep and records", app.level_db),
                };
                theme().dim
            }
        }
        Phase::Armed => {
            let _ = write!(status, "armed: trigger {:+.0} dB, input {:+.0} dB. Pop it.", app.thresh_db, 20.0 * log10(app.live_peak.max(1e-6)));
            theme().amber
        }
        Phase::Recording | Phase::Sweeping => {
            let progress = app.capture.as_ref().map(|c| c.progress()).unwrap_or(0.0);
            let _ = write!(status, "{} {:.0}%", if app.phase == Phase::Recording { "recording" } else { "sweeping" }, progress * 100.0);
            theme().amber
        }
    };
    canvas.text_scaled(8, layout.row(1), status.as_str(), layout.text, colour);

    if app.has_result && app.clipped {
        canvas.text_scaled(8, layout.row(2), "input clipped: lower the level; the decay is unreliable", layout.text, theme().red);
    } else if app.has_result {
        let r = app.reports[app.selected.min(app.count)];
        let mut values = Line::<160>::new();
        if app.selected < app.count {
            let hz = r.centre_hz;
            let _ = if hz >= 1000.0 { write!(values, "{:.1} kHz  ", hz / 1000.0) } else { write!(values, "{hz:.0} Hz  ") };
        } else {
            let _ = write!(values, "all  ");
        }
        number(&mut values, "EDT", r.edt, 2, "");
        number(&mut values, "T20", r.t20, 2, "");
        number(&mut values, "T30", r.t30, 2, " s");
        number(&mut values, "C50", r.c50, 1, "");
        number(&mut values, "C80", r.c80, 1, " dB");
        number(&mut values, "D50", r.d50, 2, "");
        number(&mut values, "DR", r.dynamic_db, 0, " dB");
        canvas.text_scaled(8, layout.row(2), values.as_str(), layout.text, theme().text);
    }

    let labels = button_labels(app);
    let shown: [(&str, bool); view::BUTTONS] = core::array::from_fn(|i| (labels[i].0.as_str(), labels[i].1));
    view::draw_buttons(&mut canvas, layout, &shown);
    if let (Some((x, y)), false) = (cursor, clean) {
        canvas.cursor(x, y, layout.text, theme().text, theme().background);
    }
    screen.present();
}

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(api: &'static TapeApi) {
    tape_rt::init(api);
    let started = match audio_screen::startup(api, MAX_WIDTH, MAX_HEIGHT) {
        Ok(started) => started,
        Err(reason) => return println!("frt60: {reason}."),
    };
    let (arguments, width, height) = (started.arguments, started.width, started.height);
    if let Some(name) = value(arguments, "theme") {
        if !gfx::theme::set_by_name(name) {
            return println!("frt60: theme= takes dark, classic or light.");
        }
    }
    let text_scale = match value(arguments, "text") {
        None | Some("auto") => None,
        Some("1") => Some(1),
        Some("2") => Some(2),
        Some(_) => return println!("frt60: text= takes 1, 2 or auto."),
    };
    let method = match value(arguments, "method") {
        None => Method::Balloon,
        Some(name) => match Method::from_name(name) {
            Some(method) => method,
            None => return println!("frt60: method= takes balloon or sweep."),
        },
    };
    let band = match value(arguments, "bands") {
        None => Band::Octave,
        Some(name) => match Band::from_name(name) {
            Some(band) => band,
            None => return println!("frt60: bands= takes octave or third."),
        },
    };
    let first_view = match value(arguments, "view") {
        None => View::Summary,
        Some(name) => match View::from_name(name) {
            Some(view) => view,
            None => return println!("frt60: view= takes summary, waterfall, heat or decay."),
        },
    };
    let rate: u32 = value(arguments, "rate").and_then(|v| v.parse().ok()).unwrap_or(48_000);
    if !(8_000..=192_000).contains(&rate) {
        return println!("frt60: rate= must be 8000 to 192000.");
    }
    let chan = value(arguments, "chan").and_then(|v| v.parse::<usize>().ok()).unwrap_or(1);
    let input = match Input::query(api, chan) {
        Ok(input) => input,
        Err(reason) => return println!("frt60: {reason}."),
    };
    let playback_ok = input.playback_ok();
    let pop_t60 = value(arguments, "simroom").and_then(parse_f32).filter(|t| (0.05..=5.0).contains(t));
    if value(arguments, "simroom").is_some() && pop_t60.is_none() {
        return println!("frt60: simroom= takes seconds from 0.05 to 5.");
    }
    if pop_t60.is_some() && !playback_ok {
        return println!("frt60: simroom= needs an audio output.");
    }

    let (Some(screen), Some(work), Some(buffer)) = (Screen::new(api, width, height), alloc_f32(api, MAXN), alloc_f32(api, MAXN)) else {
        return println!("frt60: out of memory.");
    };
    let mut screen = screen.on(started.screen);
    let thresh_db = value(arguments, "thresh").and_then(parse_f32).unwrap_or(-30.0).clamp(-80.0, -3.0);
    let level_db = value(arguments, "level").and_then(parse_f32).unwrap_or(-12.0).clamp(-60.0, -1.0);
    let length_s = value(arguments, "len").and_then(parse_f32).unwrap_or(4.0).clamp(1.0, (MAXN as f32 - rate as f32 / 4.0) / rate as f32);
    let top = (0.45 * rate as f32).min(20_000.0);
    let mut app = App {
        rate,
        band,
        method: Method::Balloon,
        view: first_view,
        help: 0,
        input,
        playback_ok,
        buffer: buffer.as_mut_ptr(),
        capture: None,
        work,
        sweep_memory: None,
        waterfall: unsafe { &mut *(&raw mut WATERFALL) },
        phase: Phase::Idle,
        thresh_db,
        level_db,
        length_s,
        sweep: Sweep { rate, f1: 20.0, f2: top, samples: MAXN / 2 },
        sweep_position: 0,
        pop: None,
        pop_t60,
        reports: [Report::NONE; MAX_BANDS + 1],
        centres: [0.0; MAX_BANDS],
        count: band.count(),
        selected: band.count(),
        has_result: false,
        ir_start: 0,
        ir_end: 0,
        ir_in_sweep: false,
        curve_for: None,
        live_peak: 0.0,
        clipped: false,
        latency_ms: 0.0,
        saves: 0,
        name: value(arguments, "name").map(file_base).filter(|b| !b.is_empty()).map(|b| alloc_name(api, b.as_str())),
        wanted_disk: value(arguments, "save").and_then(|v| v.parse().ok()),
        message: Line::new(),
    };
    for i in 0..app.count {
        app.centres[i] = band.edges(i).1;
    }
    if method == Method::Sweep {
        println!("frt60: preparing the sweep tables (a few seconds).");
        app.set_method(api, Method::Sweep);
        if app.method != Method::Sweep {
            return println!("frt60: {}.", app.message.as_str());
        }
    }
    let mut playback = Playback::new(api, &app.input, value(arguments, "out").and_then(|v| v.parse().ok()).unwrap_or(0b11));
    if let Err(status) = app.input.start(api, rate) {
        return println!("frt60: cannot start audio at {rate} Hz (code {status}); the device offers {:?}.", &app.input.rates[..app.input.rate_count]);
    }
    if !screen.open() {
        (api.audio_stop)();
        return println!("frt60: cannot take the screen.");
    }
    app.say("ready");

    let layout = Layout::new(width, height, text_scale);
    view::draw_grid(&mut screen.grid());
    let (block, samples) = unsafe { (&mut *(&raw mut BLOCK_BYTES), &mut *(&raw mut SAMPLES)) };
    let mut decoder = KeyDecoder::new();
    let mut fingers = Fingers { held: [None; 8] };
    let mut cursor = None::<(i32, i32)>;
    let mut quit = false;
    while !quit {
        let mut actions = [None::<Action>; 8];
        let mut queued = 0;
        let mut band_tap = None::<usize>;
        let mut push = |action: Action, actions: &mut [Option<Action>; 8]| {
            if queued < actions.len() {
                actions[queued] = Some(action);
                queued += 1;
            }
        };
        let keep_going = poll_keys(api, &mut decoder, |key| {
            let action = match key {
                Key::Char(b' ') => Action::Start,
                Key::Char(b'm') => Action::Method,
                Key::Char(b'b') => Action::Bands,
                Key::Char(b'v') => Action::View,
                Key::Left => Action::Prev,
                Key::Right => Action::Next,
                Key::Down => Action::LevelDown,
                Key::Up => Action::LevelUp,
                Key::Char(b'n') => Action::Length,
                Key::Char(b'.') | Key::Char(b',') => Action::Channel,
                Key::Char(b'c') => Action::Clear,
                Key::Char(b's') => Action::Save,
                Key::Char(b'k') => Action::Theme,
                Key::Char(b'h') => Action::Help,
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
            let (x, y) = (event.x * width as i32 / extent.0.max(1), event.y * height as i32 / extent.1.max(1));
            if event.is_mouse() {
                cursor = Some((x, y));
                screen.mark_dirty();
            }
            if fingers.pressed(&event) {
                if let Some(button) = layout.button_at(x, y) {
                    push(BUTTON_ACTIONS[button], &mut actions);
                } else {
                    band_tap = layout.band_at(x, y, app.count);
                }
            }
        }
        if let Some(band) = band_tap {
            app.selected = band;
            screen.mark_dirty();
        }
        for action in actions.into_iter().flatten() {
            match action {
                Action::Quit => quit = true,
                Action::Help => app.help = if app.help >= help::pages(&layout) { 0 } else { app.help + 1 },
                Action::Save => {
                    app.help = 0;
                    app.ensure_curve();
                    draw(&mut screen, &layout, &app, None, true);
                    let frame = screen.pixels();
                    app.save(api, frame, width, height);
                }
                _ => {
                    app.help = 0;
                    app.apply(api, action, &mut block[..], &mut samples[..]);
                    if action == Action::Theme {
                        view::draw_grid(&mut screen.grid());
                    }
                }
            }
            screen.mark_dirty();
        }

        let mut worked = false;
        let count = app.input.read(api, &mut block[..], &mut samples[..], app.input.channel);
        if count > 0 {
            let peak = samples[..count].iter().fold(0.0f32, |m, x| m.max(x.abs()));
            app.live_peak = if peak > app.live_peak { peak } else { app.live_peak * 0.9 + peak * 0.1 };
            if app.phase != Phase::Idle {
                if let Some(capture) = app.capture.as_mut() {
                    let done = capture.feed(&samples[..count]);
                    if app.phase == Phase::Armed && capture.triggered() {
                        app.phase = Phase::Recording;
                    }
                    if done {
                        app.finish(api);
                    }
                }
            }
            screen.mark_dirty();
            worked = true;
        }
        if let Some(playback) = playback.as_mut() {
            let (rate, level) = (app.rate, db_to_linear(app.level_db));
            let (sweep, mut position, sweeping) = (app.sweep, app.sweep_position, app.phase == Phase::Sweeping);
            let pop = &mut app.pop;
            let wrote = playback.pump(api, &app.input, |mono| {
                if sweeping {
                    for v in mono.iter_mut() {
                        *v = sweep.sample(position, level);
                        position += 1;
                    }
                } else if let Some(p) = pop.as_mut() {
                    if !p.fill(mono, rate) {
                        *pop = None;
                    }
                } else {
                    mono.fill(0.0);
                }
            });
            app.sweep_position = position;
            worked |= wrote;
        }
        if app.view == View::Decay {
            app.ensure_curve();
        }
        if screen.due() {
            draw(&mut screen, &layout, &app, cursor, false);
        }
        if !worked {
            tape_rt::idle();
        }
    }
    (api.audio_stop)();
    screen.close();
    let mut stats = tape_rt::AudioStats::default();
    (api.audio_stats)(&mut stats);
    println!("frt60: {} file set(s) saved; capture overruns {}, playback underruns {}.", app.saves, stats.capture_overruns, stats.playback_underruns);
}

/// A copy of `name` that lives as long as the tape.
fn alloc_name(api: &TapeApi, name: &str) -> &'static str {
    let address = (api.alloc)(name.len().max(1));
    if address == 0 {
        return "room";
    }
    unsafe {
        core::ptr::copy_nonoverlapping(name.as_ptr(), address as *mut u8, name.len());
        core::str::from_utf8_unchecked(core::slice::from_raw_parts(address as *const u8, name.len()))
    }
}
