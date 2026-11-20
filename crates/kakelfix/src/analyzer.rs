//! The Analyzer page: what it shows (`Analysis`, the settings the keys change) and the two waveform displays that need
//! more than the latest samples: `Envelope`, a scrolling min/max per pixel column over a span of seconds, and `Scope`, a
//! single trace started on a rising zero crossing. The spectrum is `crates/ltas`, driven by the tape. Both buffers hold
//! only numbers, so the tape can take them zeroed from the heap (`tape_rt::alloc_zeroed`) and `configure` them.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AnView {
    Waveform,
    Spectrum,
    Both,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WaveMode {
    Envelope,
    Scope,
}

/// The envelope's spans (seconds across the plot) and the scope's (milliseconds); Up and Down step through them.
pub const SPANS_S: [f32; 3] = [1.0, 5.0, 30.0];
pub const TIMEBASES_MS: [f32; 4] = [5.0, 10.0, 20.0, 50.0];
/// Spectrum resolutions, as the tape maps them onto `ltas::Resolution`.
pub const RESOLUTIONS: [&str; 4] = ["1/3 oct", "1/6 oct", "1/12 oct", "FFT"];
/// The lowest level the dB waveform scale shows.
pub const FLOOR_DB: f32 = -60.0;

/// The Analyzer page's settings. `generation` changes whenever what was gathered so far no longer fits (another pair,
/// span or resolution, or Reset): the tape then starts its buffers afresh.
#[derive(Clone, Copy, Debug)]
pub struct Analysis {
    pub view: AnView,
    pub wave: WaveMode,
    pub span: usize,
    pub timebase: usize,
    /// The waveform's vertical scale: dB (true) or linear.
    pub db: bool,
    pub frozen: bool,
    pub resolution: usize,
    /// The pair shown (channels `2 * pair` and `2 * pair + 1`) out of `channels` captured; the tape sets `channels`.
    pub pair: usize,
    pub channels: usize,
    /// The capture rate while capturing, else 0; set by the tape.
    pub rate: u32,
    pub generation: u32,
}

impl Analysis {
    pub const fn new() -> Self {
        Self { view: AnView::Both, wave: WaveMode::Envelope, span: 1, timebase: 1, db: false, frozen: false, resolution: 0, pair: 0, channels: 2, rate: 0, generation: 0 }
    }

    /// The two channels shown, 0-based; a one-channel device shows its channel twice.
    pub fn channels_shown(&self) -> (usize, usize) {
        let first = (self.pair * 2).min(self.channels.max(1) - 1);
        (first, (first + 1).min(self.channels.max(1) - 1))
    }

    pub fn pairs(&self) -> usize {
        self.channels.div_ceil(2).max(1)
    }

    /// Shows pair `pair` (clamped); what was gathered starts over when it changes.
    pub fn set_pair(&mut self, pair: usize) {
        let pair = pair.min(self.pairs() - 1);
        if pair != self.pair {
            self.pair = pair;
            self.restart();
        }
    }

    pub fn step_pair(&mut self, forward: bool) {
        let pairs = self.pairs();
        let next = if forward { (self.pair + 1) % pairs } else { (self.pair + pairs - 1) % pairs };
        if next != self.pair {
            self.pair = next;
            self.restart();
        }
    }

    /// Up (longer) or Down (shorter): the envelope's span or the scope's time base, whichever is shown.
    pub fn step_span(&mut self, longer: bool) {
        let (value, count) = match self.wave {
            WaveMode::Envelope => (&mut self.span, SPANS_S.len()),
            WaveMode::Scope => (&mut self.timebase, TIMEBASES_MS.len()),
        };
        let next = if longer { (*value + 1).min(count - 1) } else { value.saturating_sub(1) };
        if next != *value {
            *value = next;
            self.restart();
        }
    }

    pub fn next_resolution(&mut self) {
        self.resolution = (self.resolution + 1) % RESOLUTIONS.len();
        self.restart();
    }

    pub fn next_view(&mut self) {
        self.view = match self.view {
            AnView::Waveform => AnView::Spectrum,
            AnView::Spectrum => AnView::Both,
            AnView::Both => AnView::Waveform,
        };
    }

    pub fn restart(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }

    /// What `kakelfix.cfg` keeps; the tape saves when it changes.
    pub fn persisted(&self) -> Persisted {
        Persisted { view: self.view, wave: self.wave, span: self.span, timebase: self.timebase, db: self.db, resolution: self.resolution, pair: self.pair }
    }

    /// Takes the saved settings from `value(key)` (the config file and command line); unknown values are ignored.
    pub fn load<'a>(&mut self, value: impl Fn(&str) -> Option<&'a str>) {
        let pick = |key: &str, names: &[&str]| value(key).and_then(|v| names.iter().position(|name| *name == v));
        if let Some(i) = pick("an-view", &VIEW_NAMES) {
            self.view = [AnView::Waveform, AnView::Spectrum, AnView::Both][i];
        }
        if let Some(i) = pick("an-wave", &WAVE_NAMES) {
            self.wave = [WaveMode::Envelope, WaveMode::Scope][i];
        }
        if let Some(i) = pick("an-span", &SPAN_NAMES) {
            self.span = i;
        }
        if let Some(i) = pick("an-timebase", &TIMEBASE_NAMES) {
            self.timebase = i;
        }
        if let Some(i) = pick("an-scale", &["linear", "db"]) {
            self.db = i == 1;
        }
        if let Some(i) = pick("an-resolution", &RESOLUTION_NAMES) {
            self.resolution = i;
        }
        if let Some(pair) = value("an-pair").and_then(|v| v.parse::<usize>().ok()) {
            self.pair = pair.saturating_sub(1);
        }
        self.restart();
    }

    /// Calls `put(key, value)` for every saved setting, in the words `load` reads.
    pub fn save(&self, mut put: impl FnMut(&str, core::fmt::Arguments)) {
        put("an-view", format_args!("{}", VIEW_NAMES[self.view as usize]));
        put("an-wave", format_args!("{}", WAVE_NAMES[self.wave as usize]));
        put("an-span", format_args!("{}", SPAN_NAMES[self.span]));
        put("an-timebase", format_args!("{}", TIMEBASE_NAMES[self.timebase]));
        put("an-scale", format_args!("{}", if self.db { "db" } else { "linear" }));
        put("an-resolution", format_args!("{}", RESOLUTION_NAMES[self.resolution]));
        put("an-pair", format_args!("{}", self.pair + 1));
    }
}

/// Config-file words for the settings, in the order of their enums and tables.
const VIEW_NAMES: [&str; 3] = ["waveform", "spectrum", "both"];
const WAVE_NAMES: [&str; 2] = ["envelope", "scope"];
const SPAN_NAMES: [&str; 3] = ["1", "5", "30"];
const TIMEBASE_NAMES: [&str; 4] = ["5", "10", "20", "50"];
const RESOLUTION_NAMES: [&str; 4] = ["third", "sixth", "twelfth", "fft"];

/// The saved part of `Analysis` (not Freeze, the capture or the generation).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Persisted {
    view: AnView,
    wave: WaveMode,
    span: usize,
    timebase: usize,
    db: bool,
    resolution: usize,
    pair: usize,
}

impl Default for Analysis {
    fn default() -> Self {
        Self::new()
    }
}

/// The widest envelope, in pixel columns.
pub const MAX_COLUMNS: usize = 4096;

/// A scrolling waveform overview: each pixel column holds the lowest and highest sample of both channels over
/// `span / columns` seconds; the newest column is at the right.
pub struct Envelope {
    columns: usize,
    per_column: usize,
    count: usize,
    current: [[f32; 2]; 2],
    ring: [[[f32; 2]; 2]; MAX_COLUMNS],
    head: usize,
    filled: usize,
}

impl Envelope {
    pub const fn new() -> Self {
        Self { columns: 0, per_column: 1, count: 0, current: [[0.0; 2]; 2], ring: [[[0.0; 2]; 2]; MAX_COLUMNS], head: 0, filled: 0 }
    }

    /// Empties it for `columns` pixel columns showing `span_s` seconds at `rate` Hz.
    pub fn configure(&mut self, columns: usize, rate: u32, span_s: f32) {
        self.columns = columns.clamp(1, MAX_COLUMNS);
        self.per_column = ((rate as f32 * span_s) as usize / self.columns).max(1);
        self.count = 0;
        self.head = 0;
        self.filled = 0;
    }

    pub fn push(&mut self, left: &[f32], right: &[f32]) {
        for (&l, &r) in left.iter().zip(right) {
            if self.count == 0 {
                self.current = [[l, l], [r, r]];
            } else {
                let c = &mut self.current;
                c[0] = [c[0][0].min(l), c[0][1].max(l)];
                c[1] = [c[1][0].min(r), c[1][1].max(r)];
            }
            self.count += 1;
            if self.count == self.per_column {
                self.ring[self.head] = self.current;
                self.head = (self.head + 1) % self.columns;
                self.filled = (self.filled + 1).min(self.columns);
                self.count = 0;
            }
        }
    }

    pub fn columns(&self) -> usize {
        self.columns
    }

    /// Complete columns so far (at most `columns`).
    pub fn filled(&self) -> usize {
        self.filled
    }

    /// Column `age` back from the newest (0 is the newest): `[channel][min, max]`.
    pub fn back(&self, age: usize) -> [[f32; 2]; 2] {
        self.ring[(self.head + self.columns - 1 - age % self.columns) % self.columns]
    }
}

/// The longest scope trace in samples (50 ms at 192 kHz).
pub const SCOPE_MAX: usize = 9600;
/// A rising crossing counts once the signal has been below `-HYSTERESIS` (noise around zero does not trigger).
const HYSTERESIS: f32 = 0.002;

/// A triggered trace of `len` samples of both channels: it waits for the left channel to rise through zero and records
/// from there; with no crossing within four traces it records anyway (free run), so silence still shows.
pub struct Scope {
    len: usize,
    work: [[f32; SCOPE_MAX]; 2],
    shown: [[f32; SCOPE_MAX]; 2],
    pos: usize,
    recording: bool,
    below: bool,
    waited: usize,
    has_trace: bool,
    triggered: bool,
    shown_triggered: bool,
}

impl Scope {
    pub const fn new() -> Self {
        Self { len: 1, work: [[0.0; SCOPE_MAX]; 2], shown: [[0.0; SCOPE_MAX]; 2], pos: 0, recording: false, below: false, waited: 0, has_trace: false, triggered: false, shown_triggered: false }
    }

    /// Empties it for traces of `span_ms` at `rate` Hz.
    pub fn configure(&mut self, rate: u32, span_ms: f32) {
        self.len = ((rate as f32 * span_ms / 1000.0) as usize).clamp(2, SCOPE_MAX);
        self.pos = 0;
        self.recording = false;
        self.below = false;
        self.waited = 0;
        self.has_trace = false;
        self.triggered = false;
        self.shown_triggered = false;
    }

    pub fn push(&mut self, left: &[f32], right: &[f32]) {
        for (&l, &r) in left.iter().zip(right) {
            if !self.recording {
                let crossed = self.below && l >= 0.0;
                if l < -HYSTERESIS {
                    self.below = true;
                }
                self.waited += 1;
                if crossed || self.waited >= self.len * 4 {
                    self.recording = true;
                    self.triggered = crossed;
                    self.pos = 0;
                } else {
                    continue;
                }
            }
            self.work[0][self.pos] = l;
            self.work[1][self.pos] = r;
            self.pos += 1;
            if self.pos == self.len {
                self.shown[0][..self.len].copy_from_slice(&self.work[0][..self.len]);
                self.shown[1][..self.len].copy_from_slice(&self.work[1][..self.len]);
                self.has_trace = true;
                self.shown_triggered = self.triggered;
                self.recording = false;
                self.below = false;
                self.waited = 0;
            }
        }
    }

    /// The latest complete trace of `channel` (empty before the first), and whether a crossing started it.
    pub fn trace(&self, channel: usize) -> (&[f32], bool) {
        if self.has_trace { (&self.shown[channel][..self.len], self.shown_triggered) } else { (&[], false) }
    }
}
