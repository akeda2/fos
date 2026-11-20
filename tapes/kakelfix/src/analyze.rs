//! The Analyzer page: capture from the active device while the page is shown, the envelope, scope and two spectra
//! (`crates/ltas`, left and right) fed from it, and their drawing. Capture starts when the page is entered and stops when
//! it is left (the Meters page captures too, through `meters::Bank`), so the Mixer page never holds the device's input. The buffers are taken from the heap once, at startup.

use crate::view::{centered, text};
use audio_screen::Input;
use core::fmt::Write;
use gfx::Canvas;
use gfx::plot::{Frame, Scale, axis_label, curve, spline};
use gfx::theme::theme;
use kakelfix::analyzer::{Envelope, FLOOR_DB, SPANS_S, Scope, TIMEBASES_MS};
use kakelfix::{App, Layout, Page, Rect, Text, WaveMode};
use ltas::{Analyzer, Config, Mode, Resolution};
use mixer_model::lin_to_db;
use tape_rt::{FP_AVX2, TapeApi, alloc_bytes, alloc_zeroed};

/// The FFT length: 8192 points is 5.9 Hz per bin at 48 kHz, enough for 1/12 octave down to about 100 Hz.
const FFT: usize = 8192;
const BINS: usize = FFT / 2 + 1;
/// Frames read per `audio_read`.
const CHUNK: usize = 2048;
const LOW_HZ: f32 = 20.0;
const HIGH_HZ: f32 = 20000.0;
/// The spectrum's dB scale: 0 dBFS at the top, 100 dB down.
const SCALE: Scale = Scale { db_top: 0.0, db_range: 100.0 };

struct Buffers {
    scratch: &'static mut [u8],
    left: &'static mut [f32; CHUNK],
    right: &'static mut [f32; CHUNK],
    envelope: &'static mut Envelope,
    scope: &'static mut Scope,
    spectra: [&'static mut Analyzer<FFT>; 2],
    levels: &'static mut [f32; BINS],
}

pub struct Capture {
    input: Result<Input, &'static str>,
    buffers: Option<Buffers>,
    rate: u32,
    running: bool,
    failed: bool,
    seen: u32,
    columns: usize,
    bank: Option<crate::meters::Bank>,
}

impl Capture {
    /// `rate` is the `rate=` argument (0: 48 kHz when the device offers it, else its first rate).
    pub fn new(api: &TapeApi, rate: u32) -> Capture {
        let input = Input::query(api, 1);
        let buffers = input.as_ref().ok().and_then(|input| {
            Some(Buffers {
                scratch: alloc_bytes(api, CHUNK * input.frame_bytes())?,
                left: alloc_zeroed(api)?,
                right: alloc_zeroed(api)?,
                envelope: alloc_zeroed(api)?,
                scope: alloc_zeroed(api)?,
                spectra: [alloc_zeroed(api)?, alloc_zeroed(api)?],
                levels: alloc_zeroed(api)?,
            })
        });
        let rate = match &input {
            Ok(input) if rate == 0 => {
                let rates = &input.rates[..input.rate_count];
                if rates.contains(&48_000) || rates.is_empty() { 48_000 } else { rates[0] }
            }
            _ => rate,
        };
        let bank = match &input {
            Ok(input) => crate::meters::Bank::new(api, rate, input.channels),
            Err(_) => None,
        };
        Capture { input, buffers, rate, running: false, failed: false, seen: 0, columns: 0, bank }
    }

    pub fn channels(&self) -> usize {
        self.input.as_ref().map_or(0, |input| input.channels)
    }

    /// Why the page cannot show anything, if it cannot.
    pub fn bank(&self) -> Option<&crate::meters::Bank> {
        self.bank.as_ref()
    }

    pub(crate) fn problem(&self) -> Option<&'static str> {
        match (&self.input, &self.buffers) {
            (Err(reason), _) => Some(reason),
            (Ok(_), None) => Some("out of memory for the analyzer"),
            _ if self.failed => Some("the capture did not start (see the message above)"),
            _ => None,
        }
    }

    /// Starts or stops capture with the page and feeds what arrived; true when there is something new to draw.
    pub fn update(&mut self, api: &TapeApi, app: &mut App, layout: &Layout) -> bool {
        if app.page != Page::Analyzer && app.page != Page::Meters {
            self.stop(api, app);
            self.failed = false;
            return false;
        }
        let (Ok(input), Some(b)) = (&self.input, &mut self.buffers) else { return false };
        if !self.running && !self.failed {
            match input.start_capture(api, self.rate) {
                Ok(()) => {
                    self.running = true;
                    app.analysis.rate = self.rate;
                    self.seen = app.analysis.generation.wrapping_sub(1);
                }
                Err(code) => {
                    self.failed = true;
                    app.message.clear();
                    let _ = write!(app.message, "capture at {} Hz: code {code}", self.rate);
                    return true;
                }
            }
        }
        if !self.running {
            return false;
        }
        if app.page == Page::Meters {
            let Some(bank) = &mut self.bank else { return false };
            return bank.update(api, input, b.scratch, &mut b.left[..], &mut b.right[..], app.metering.generation);
        }
        let a = app.analysis;
        let columns = layout.analyzer_plots(a.view).0.map_or(self.columns.max(1), |plot| plot.w.max(1) as usize);
        if self.seen != a.generation || columns != self.columns {
            self.seen = a.generation;
            self.columns = columns;
            b.envelope.configure(columns, self.rate, SPANS_S[a.span]);
            b.scope.configure(self.rate, TIMEBASES_MS[a.timebase]);
            let mut config = Config::new(self.rate);
            config.hop = FFT / 4;
            config.tau_s = 0.5;
            config.avx2 = tape_rt::fp_level() >= FP_AVX2;
            config.resolution = [Resolution::Third, Resolution::Sixth, Resolution::Twelfth, Resolution::Fft][a.resolution];
            for spectrum in b.spectra.iter_mut() {
                spectrum.configure(config);
            }
        }
        let pair = a.channels_shown();
        let mut fresh = false;
        loop {
            let got = input.read_pair(api, b.scratch, &mut b.left[..], &mut b.right[..], pair);
            if got == 0 {
                break;
            }
            fresh = true;
            if !a.frozen {
                b.envelope.push(&b.left[..got], &b.right[..got]);
                b.scope.push(&b.left[..got], &b.right[..got]);
                b.spectra[0].push(&b.left[..got]);
                b.spectra[1].push(&b.right[..got]);
            }
        }
        fresh && !a.frozen
    }

    pub fn stop(&mut self, api: &TapeApi, app: &mut App) {
        if self.running {
            (api.audio_stop)();
            self.running = false;
            app.analysis.rate = 0;
        }
    }
}

pub fn draw(canvas: &mut Canvas, layout: &Layout, app: &App, capture: &mut Capture) {
    if let Some(reason) = capture.problem() {
        return centered(canvas, layout, layout.body, reason, theme().dim);
    }
    pair_buttons(canvas, layout, app);
    let Some(b) = &mut capture.buffers else { return };
    let (wave, spectrum) = layout.analyzer_plots(app.analysis.view);
    if let Some(plot) = wave {
        waveform(canvas, layout, app, b, plot);
    }
    if let Some(plot) = spectrum {
        spectra(canvas, layout, app, b, plot);
    }
}

/// The row of pair buttons: tap one to show that pair, swipe the row to reach the rest; the shown pair lit.
fn pair_buttons(canvas: &mut Canvas, layout: &Layout, app: &App) {
    let t = theme();
    let a = &app.analysis;
    for pair in 0..a.pairs() {
        let Some(area) = layout.an_pair(pair, app) else { continue };
        let shown = pair == a.pair;
        canvas.fill_rect(area.x, area.y, area.w as u32, area.h as u32, if shown { t.grid_major } else { t.grid });
        canvas.rect(area.x, area.y, area.w as u32, area.h as u32, if shown { t.text } else { t.frame });
        let name = crate::meters::pair_name(app, pair, a.channels);
        centered(canvas, layout, area, name.as_str(), if shown { t.text } else { t.dim });
    }
    let row = layout.an_pairs();
    if app.first_pair > 0 {
        text(canvas, layout, row.x, row.y + row.h, "<", t.dim);
    }
    if app.first_pair + layout.visible_pairs() < a.pairs() {
        text(canvas, layout, row.x + row.w - layout.col * 2, row.y + row.h, ">", t.dim);
    }
}

/// A capture channel's name: the mixer's name for that input when it has one, else its number.
pub(crate) fn channel_name(app: &App, channel: usize) -> Text<16> {
    app.input_name(channel)
}

/// 0 at the dB floor, 1 at full scale.
fn db_fraction(magnitude: f32) -> f32 {
    ((lin_to_db(magnitude as f64) as f32 - FLOOR_DB) / -FLOOR_DB).clamp(0.0, 1.0)
}

/// A sample's distance above (positive) or below the lane's centre line, as a fraction of half the lane.
fn wave_fraction(db: bool, value: f32) -> f32 {
    if db {
        if value < 0.0 { -db_fraction(-value) } else { db_fraction(value) }
    } else {
        value.clamp(-1.0, 1.0)
    }
}

/// Two lanes, left channel over right, each with its centre line; the envelope grows from the right edge, the scope
/// trace spans the lane.
fn waveform(canvas: &mut Canvas, layout: &Layout, app: &App, b: &Buffers, plot: Rect) {
    let t = theme();
    let a = &app.analysis;
    let (first, second) = a.channels_shown();
    let lane_h = plot.h / 2;
    for (lane, channel) in [first, second].into_iter().enumerate() {
        let area = Rect::new(plot.x, plot.y + lane as i32 * lane_h, plot.w, lane_h);
        let (centre, half) = (area.y + area.h / 2, (area.h / 2 - 1).max(1) as f32);
        let colour = t.curves[lane];
        canvas.rect(area.x, area.y, area.w as u32, area.h as u32, t.grid_major);
        for guide in [0.5f32, -0.5] {
            // Linear: half scale (-6 dB). dB: half way to the floor.
            canvas.fill_rect(area.x, centre - (guide * half) as i32, area.w as u32, 1, t.grid);
        }
        canvas.fill_rect(area.x, centre, area.w as u32, 1, t.grid_major);
        let name = channel_name(app, channel);
        text(canvas, layout, layout.body.x, centre - layout.row / 2, name.as_str(), colour);
        let y = |value: f32| centre - (wave_fraction(a.db, value) * half) as i32;
        match a.wave {
            WaveMode::Envelope => {
                for age in 0..b.envelope.filled().min(area.w as usize) {
                    let [low, high] = b.envelope.back(age)[lane];
                    let (top, bottom) = if a.db {
                        (centre - (db_fraction(high.max(0.0)) * half) as i32, centre + (db_fraction((-low).max(0.0)) * half) as i32)
                    } else {
                        (y(high), y(low))
                    };
                    canvas.fill_rect(area.x + area.w - 1 - age as i32, top, 1, (bottom - top + 1).max(1) as u32, colour);
                }
            }
            WaveMode::Scope => {
                let (trace, _) = b.scope.trace(lane);
                if trace.len() >= 2 {
                    let mut previous = (area.x, y(trace[0]));
                    for x in 1..area.w {
                        let point = (area.x + x, y(trace[x as usize * trace.len() / area.w as usize]));
                        canvas.line(previous.0, previous.1, point.0, point.1, colour);
                        previous = point;
                    }
                }
            }
        }
    }
    let mut note = Text::<48>::new();
    let _ = match a.wave {
        WaveMode::Envelope => write!(note, "{} s  {}", SPANS_S[a.span] as u32, if a.db { "dB" } else { "linear" }),
        WaveMode::Scope => {
            let triggered = b.scope.trace(0).1;
            write!(note, "{} ms  {}  {}", TIMEBASES_MS[a.timebase] as u32, if a.db { "dB" } else { "linear" }, if triggered { "trig" } else { "free" })
        }
    };
    let width = note.as_str().len() as i32 * layout.col;
    text(canvas, layout, plot.x + plot.w - width, plot.y - layout.row, note.as_str(), t.dim);
}

/// What the spectrum's lines are, in the row above it: a swatch and name per channel (the 0.5 s average) and the dim peak
/// hold (the resolution is in the header).
fn legend(canvas: &mut Canvas, layout: &Layout, app: &App, plot: Rect) {
    let t = theme();
    let (first, second) = app.analysis.channels_shown();
    let y = plot.y - layout.row;
    let mut x = plot.x;
    let names = [channel_name(app, first), channel_name(app, second)];
    let entries = [(names[0].as_str(), t.curves[0]), (names[1].as_str(), t.curves[1]), ("peak hold", t.dim)];
    for (name, colour) in entries {
        canvas.fill_rect(x, y + layout.row / 2 - 1, (layout.col * 2) as u32, 2, colour);
        x += layout.col * 3;
        x = text(canvas, layout, x, y, name, colour) + layout.col * 2;
    }
}

/// `hz` on the log axis from 20 Hz to 20 kHz across `plot`.
fn x_of(plot: &Rect, hz: f32) -> i32 {
    let decades = lin_to_db((hz.max(LOW_HZ) / LOW_HZ) as f64) as f32 / 20.0;
    plot.x + (decades / 3.0 * plot.w as f32) as i32
}

/// The left and right spectra (exponential average, 0.5 s) with their peaks since the last reset behind them, dim. Band
/// resolutions are drawn as a spline through the band centres (as ltas does), FFT as a polyline; never as stairs.
fn spectra(canvas: &mut Canvas, layout: &Layout, app: &App, b: &mut Buffers, plot: Rect) {
    let t = theme();
    legend(canvas, layout, app, plot);
    let frame = Frame { left: plot.x, top: plot.y, plot_w: plot.w as u32, plot_h: plot.h as u32 };
    let mut db = 0;
    while db >= -(SCALE.db_range as i32) {
        let y = frame.y(&SCALE, db as f32);
        canvas.fill_rect(plot.x, y, plot.w as u32, 1, if db == 0 { t.grid_major } else { t.grid });
        let mut label = Text::<8>::new();
        let _ = write!(label, "{db}");
        text(canvas, layout, plot.x - (label.as_str().len() as i32 + 1) * layout.col, y - layout.row / 2, label.as_str(), t.dim);
        db -= 20;
    }
    for decade in [10u32, 100, 1000, 10000] {
        for multiple in 1..=9u32 {
            let hz = decade * multiple;
            if (hz as f32) < LOW_HZ || (hz as f32) > HIGH_HZ {
                continue;
            }
            let x = x_of(&plot, hz as f32);
            canvas.fill_rect(x, plot.y, 1, plot.h as u32, if multiple == 1 { t.grid_major } else { t.grid });
            if matches!(multiple, 1 | 2 | 5) {
                axis_label(canvas, x, plot.y + plot.h + 6, hz, t.dim);
            }
        }
    }
    canvas.rect(plot.x, plot.y, plot.w as u32, plot.h as u32, t.grid_major);
    for (mode, colours) in [(Mode::Peak, [t.dim, t.dim]), (Mode::Exp, [t.curves[0], t.curves[1]])] {
        for (channel, colour) in colours.into_iter().enumerate() {
            let spectrum = &mut *b.spectra[channel];
            let outputs = spectrum.outputs().min(BINS);
            spectrum.levels_db(mode, &mut b.levels[..]);
            let bands = spectrum.config().resolution != Resolution::Fft;
            let spectrum = &*spectrum;
            let levels = &b.levels;
            if bands {
                let mut previous: Option<(i32, i32)> = None;
                spline(&levels[..outputs], plot.x, plot.x + plot.w, |i| x_of(&plot, spectrum.centre_hz(i)), |x, db| {
                    let y = frame.y(&SCALE, db);
                    if let Some((px, py)) = previous {
                        canvas.line(px, py, x, y, colour);
                        canvas.line(px, py + 1, x, y + 1, colour);
                    }
                    previous = Some((x, y));
                });
                continue;
            }
            let points = (0..outputs)
                .filter(|&i| {
                    let (low, high) = spectrum.edges_hz(i);
                    high >= LOW_HZ && low <= HIGH_HZ
                })
                .map(|i| {
                    let (low, high) = spectrum.edges_hz(i);
                    (x_of(&plot, low).max(plot.x), x_of(&plot, high.min(HIGH_HZ)).min(plot.x + plot.w), frame.y(&SCALE, levels[i]))
                });
            curve(canvas, colour, points, false);
        }
    }
}
