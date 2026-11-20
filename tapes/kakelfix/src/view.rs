//! Drawing: header, soft keys, the Mixer page's three views (the Analyzer's are in `analyze.rs`), the confirm dialog and the mouse cursor. Everything is drawn
//! each frame onto the plain background layer; positions come from `kakelfix::Layout`.

use crate::{METERS, Shadowed as Hardware};
use fosfor_abi::tape::{MixerControl, MixerMeter, MIXER_CONTROL_ACTION, MIXER_CONTROL_BOOL, MIXER_CONTROL_ENUM, MIXER_FLAG_HAZARD, MIXER_FLAG_READONLY, MIXER_METER_OVER};
use gfx::Canvas;
use gfx::theme::theme;
use kakelfix::layout::fader_position;
use kakelfix::analyzer::RESOLUTIONS;
use kakelfix::{Action, AnView, App, MeterScale, Device, Layout, Page, Rect, SOFT_SLOTS, SoftKind, Text, View, WaveMode, field, fit_name, format_db, format_pan};
use mixer_model::{Group, Strip};
use core::fmt::Write;

/// Falling meter bars (24 dB per second) with an over light held for 1.5 s.
pub struct Meters {
    level: [f32; METERS],
    over: [f32; METERS],
    count: usize,
}

impl Meters {
    pub fn new() -> Meters {
        Meters { level: [-144.0; METERS], over: [0.0; METERS], count: 0 }
    }

    pub fn update(&mut self, readings: &[MixerMeter], seconds: f32) {
        self.count = readings.len().min(METERS);
        for (i, reading) in readings.iter().take(METERS).enumerate() {
            let peak = reading.peak_cdb as f32 / 100.0;
            self.level[i] = peak.max(self.level[i] - 24.0 * seconds);
            self.over[i] = if reading.flags & MIXER_METER_OVER as u16 != 0 { 1.5 } else { (self.over[i] - seconds).max(0.0) };
        }
    }

    fn get(&self, index: usize) -> Option<(f32, bool)> {
        (index < self.count).then(|| (self.level[index], self.over[index] > 0.0))
    }
}

pub(crate) fn text(canvas: &mut Canvas, layout: &Layout, x: i32, y: i32, s: &str, rgb: u32) -> i32 {
    canvas.text_scaled(x, y, s, layout.scale, rgb)
}

/// At most `chars` characters of `s`.
fn fit(s: &str, chars: i32) -> &str {
    match s.char_indices().nth(chars.max(0) as usize) {
        Some((end, _)) => &s[..end],
        None => s,
    }
}

/// `s` centred in `area`, a name cut so that its number stays ("Mic 10" -> "Mic10", not "Mic 1").
pub(crate) fn centered(canvas: &mut Canvas, layout: &Layout, area: Rect, s: &str, rgb: u32) {
    let chars = (area.w / layout.col).max(0);
    let short;
    let s = if s.chars().count() as i32 > chars && s.len() <= 32 {
        short = fit_name(s, chars as usize);
        short.as_str()
    } else {
        fit(s, chars)
    };
    let width = s.chars().count() as i32 * layout.col;
    text(canvas, layout, area.x + (area.w - width) / 2, area.y + (area.h - layout.row) / 2, s, rgb);
}

fn box_(canvas: &mut Canvas, area: Rect, fill: u32, border: u32) {
    canvas.fill_rect(area.x, area.y, area.w.max(0) as u32, area.h.max(0) as u32, fill);
    canvas.rect(area.x, area.y, area.w.max(0) as u32, area.h.max(0) as u32, border);
}

fn level_colour(db: f32) -> u32 {
    if db > -3.0 {
        theme().red
    } else if db > -18.0 {
        theme().amber
    } else {
        theme().green
    }
}

pub fn draw(canvas: &mut Canvas, layout: &Layout, app: &App, hardware: &Hardware, meters: &Meters, capture: &mut crate::analyze::Capture, pointer: Option<(i32, i32)>) {
    header(canvas, layout, app, hardware);
    soft_keys(canvas, layout, app);
    match app.page {
        Page::Mixer => match app.view {
            View::Outputs => outputs(canvas, layout, app, meters),
            View::Matrix => matrix(canvas, layout, app),
            View::Device => device(canvas, layout, app, hardware),
        },
        Page::Recorder => crate::record::draw(canvas, layout, app, capture.bank(), capture.problem(), capture.threaded()),
        Page::Analyzer => crate::analyze::draw(canvas, layout, app, capture),
        Page::Meters => crate::meters::draw(canvas, layout, app, hardware, capture.bank(), capture.problem()),
    }
    footer(canvas, layout, app);
    if let Some((index, value)) = app.confirm {
        dialog(canvas, layout, app, hardware, index, value);
    }
    if let Some((x, y)) = pointer {
        canvas.cursor(x, y, layout.scale + 1, theme().text, theme().background);
    }
}

fn header(canvas: &mut Canvas, layout: &Layout, app: &App, hardware: &Hardware) {
    let t = theme();
    let mut line = Text::<96>::new();
    let page = match (app.page, app.view) {
        (Page::Mixer, View::Outputs) => "Mixer / Outputs",
        (Page::Mixer, View::Matrix) => "Mixer / Matrix",
        (Page::Mixer, View::Device) => "Mixer / Device",
        (Page::Recorder, _) => "Recorder",
        (Page::Analyzer, _) => match app.analysis.view {
            AnView::Waveform => "Analyzer / Waveform",
            AnView::Spectrum => "Analyzer / Spectrum",
            AnView::Both => "Analyzer / Both",
        },
        (Page::Meters, _) => "Meters",
    };
    let _ = write!(line, "KakelFix  {}  {page}", hardware.device.mixer.title());
    text(canvas, layout, 8, 4, line.as_str(), t.text);
    // While a take runs, every page says so.
    if let Some(lamp) = crate::record::header_lamp(app) {
        let width = lamp.as_str().len() as i32 * layout.col;
        let x = layout.keys.x - width - 8;
        canvas.fill_rect(x - layout.row, 4 + layout.row / 4, (layout.row / 2) as u32, (layout.row / 2) as u32, t.red);
        text(canvas, layout, x, 4, lamp.as_str(), t.red);
    }
    line.clear();
    if app.page == Page::Mixer && app.view == View::Outputs {
        let _ = write!(line, "mix sent to output {} (pick the output above)   pan law {}", app.label(&app.output()).as_str(), app.mix.pan_law.name());
    } else if app.page == Page::Mixer && app.view == View::Matrix {
        let _ = write!(line, "rows: sources   columns: outputs   pan law {}", app.mix.pan_law.name());
    } else if app.page == Page::Analyzer {
        let a = &app.analysis;
        let (first, second) = a.channels_shown();
        let _ = write!(line, "inputs {}+{} of {}", first + 1, second + 1, a.channels);
        if a.rate != 0 {
            let _ = write!(line, "   {} Hz", a.rate);
        }
        let wave = if a.wave == WaveMode::Envelope { "envelope" } else { "scope" };
        let _ = write!(line, "   {wave}   {}{}", RESOLUTIONS[a.resolution], if a.frozen { "   FROZEN" } else { "" });
    } else if app.page == Page::Recorder {
        let r = &app.recording;
        let _ = write!(line, "inputs {}   {} Hz   peak meters: {}", r.channels, r.rate, if r.nordic { "Nordic PPM" } else { "dBFS" });
    } else if app.page == Page::Meters {
        let m = &app.metering;
        let _ = write!(line, "inputs {}", m.channels);
        if app.analysis.rate != 0 {
            let _ = write!(line, "   {} Hz", app.analysis.rate);
        }
        let _ = line.write_str(match m.scale {
            MeterScale::Dbfs => "   sample peak, dBFS",
            MeterScale::Nordic => "   Nordic PPM (IEC 60268-10 type I): TEST = -18 dBFS",
            MeterScale::Lufs => "   EBU R128 per pair: bar M, line S, target -23 LUFS",
        });
    }
    text(canvas, layout, 8, 4 + layout.row, line.as_str(), t.dim);
    if !app.message.is_empty() {
        let width = app.message.as_str().chars().count() as i32 * layout.col;
        text(canvas, layout, layout.keys.x - width - 8, 4 + layout.row, app.message.as_str(), t.amber);
    }
    canvas.line(0, layout.header.h - 2, layout.width, layout.header.h - 2, t.grid_major);
}

/// Pages (Mixer, Recorder, Analyzer) in the page colour; the active page's views indented under it, joined to it by a
/// line, each page key with its number key (1-4); actions (Pair, Freeze, Quit, ...) with a plain frame.
fn soft_keys(canvas: &mut Canvas, layout: &Layout, app: &App) {
    let t = theme();
    let page_colour = t.curves[1];
    let keys = app.soft_keys();
    if let Some(page) = (0..SOFT_SLOTS).find(|&k| keys[k].kind == SoftKind::Page && keys[k].active) {
        let views = (page + 1..SOFT_SLOTS).take_while(|&k| keys[k].kind == SoftKind::View);
        let page_key = layout.soft(app, page);
        let x = page_key.x + layout.col / 2;
        for slot in views {
            let area = layout.soft(app, slot);
            let (top, middle) = (page_key.y + page_key.h, area.y + area.h / 2);
            canvas.fill_rect(x, top, 2, (middle - top + 2).max(0) as u32, page_colour);
            canvas.fill_rect(x, middle, (area.x - x).max(0) as u32, 2, page_colour);
        }
    }
    for (slot, key) in keys.iter().enumerate() {
        let area = layout.soft(app, slot);
        let (frame, label) = match key.kind {
            SoftKind::Empty => continue,
            SoftKind::Page => (page_colour, if key.active { t.text } else { page_colour }),
            SoftKind::View => (if key.active { t.text } else { t.frame }, t.text),
            SoftKind::Action => (if key.active { t.amber } else { t.grid_major }, if key.active { t.amber } else { t.text }),
        };
        box_(canvas, area, if key.active { t.grid_major } else { t.grid }, frame);
        if let (SoftKind::Page, Action::Page(page)) = (key.kind, key.action) {
            // The page's key (1-4) at the left; the name centred in what is left.
            let digit = [b'1' + page as u8];
            let digit = core::str::from_utf8(&digit).unwrap_or("");
            text(canvas, layout, area.x + layout.col / 2, area.y + (area.h - layout.row) / 2, digit, label);
            let rest = Rect::new(area.x + layout.col * 3 / 2, area.y, area.w - layout.col * 3 / 2, area.h);
            centered(canvas, layout, rest, key.label, label);
        } else {
            centered(canvas, layout, area, key.label, label);
        }
    }
}

fn footer(canvas: &mut Canvas, layout: &Layout, app: &App) {
    let hint = match (app.page, app.view) {
        (Page::Mixer, View::Outputs) => "Up/Dn 1 dB  PgUp/PgDn 6  +- 0.1  0 unity  x off  m mute  , . pan  o output  p pair  Tab view  1-4 page",
        (Page::Mixer, View::Matrix) => "arrows select  Enter or click again: open  +- 0.1 dB  0 x m  p P pair  Tab view  1-4 page",
        (Page::Mixer, View::Device) => "Up/Dn select  Left/Right/Enter or click again: change  Tab view  1-4 page",
        (Page::Analyzer, _) => "Tab view  Left/Right , . or tap pair  Up/Dn span  space freeze  w env/scope  d dB/lin  f resolution  r reset  1-4 page",
        (Page::Recorder, _) => "R or space record/stop  Left/Right input  Enter arm  a arm all  f format  d disk  Tab scale  r reset  1-4 page",
        (Page::Meters, _) => "Left/Right input  Up/Dn gain  +- gain  h +48V  i switch  Tab or s scale  r reset  a inputs  drag a gain  1-4 page",
    };
    text(canvas, layout, 8, layout.footer.y + 4, fit(hint, layout.footer.w / layout.col - 2), theme().dim);
}

/// Inputs blue, playback green, the output's master amber: the band and the strip labels, so the groups tell apart at a glance.
fn group_colour(group: Group) -> u32 {
    match group {
        Group::Input => theme().curves[0],
        Group::Playback => theme().curves[2],
        Group::Output => theme().amber,
    }
}

/// Meter numbers (inputs, playback, outputs) of a strip's channels.
fn meter_channels(app: &App, strip: &Strip) -> core::ops::Range<usize> {
    let base = match strip.group {
        Group::Input => 0,
        Group::Playback => app.mix.n[0],
        Group::Output => app.mix.n[0] + app.mix.n[1],
    };
    base + strip.first..base + strip.first + if strip.stereo { 2 } else { 1 }
}

fn outputs(canvas: &mut Canvas, layout: &Layout, app: &App, meters: &Meters) {
    let t = theme();
    let outputs = app.outputs();
    for (index, output) in outputs.iter().enumerate() {
        let Some(area) = layout.tab(index, app) else { continue };
        let selected = index == app.out_sel;
        box_(canvas, area, if selected { t.grid_major } else { t.grid }, if selected { t.text } else { t.frame });
        centered(canvas, layout, area, app.label(output).as_str(), t.text);
    }
    let sources = app.sources();
    let output = app.output();
    let group_of = |index: usize| sources.get(index).map_or(Group::Output, |strip| strip.group);
    for index in 0..app.strip_count() {
        let Some(band) = layout.band(index, app) else { continue };
        let group = group_of(index);
        let colour = group_colour(group);
        let first = index == app.first_strip || group_of(index - 1) != group;
        let last = index + 1 == app.strip_count() || group_of(index + 1) != group;
        let width = if last { band.w - 6 } else { band.w };
        canvas.fill_rect(band.x, band.y + band.h - 3, width as u32, 2, colour);
        if first {
            let name = match group {
                Group::Input => "Inputs (hardware)",
                Group::Playback => "Playback (from tapes)",
                Group::Output => "Output",
            };
            text(canvas, layout, band.x, band.y - 2, name, colour);
        }
    }
    for index in 0..app.strip_count() {
        let Some(column) = layout.strip(index, app) else { continue };
        let selected = index == app.src_sel;
        box_(canvas, column, if selected { t.grid } else { t.background }, if selected { t.text } else { t.frame });
        let (db, pan, mute, unknown, is_send) = app.strip_level(index);
        let strip = sources.get(index).copied().unwrap_or(output);
        let label = if is_send { app.label(&strip) } else { Text::from("Master") };
        centered(canvas, layout, layout.label(index, app).unwrap_or(column), label.as_str(), group_colour(group_of(index)));
        if let Some(area) = layout.pan(index, app) {
            if is_send {
                box_(canvas, area, t.off, t.frame);
                let mid = area.x + area.w / 2;
                canvas.line(mid, area.y, mid, area.y + area.h - 1, t.grid_major);
                let at = area.x + ((pan + 1.0) / 2.0 * (area.w - 4) as f64) as i32;
                canvas.fill_rect(at, area.y + 1, 4, (area.h - 2).max(1) as u32, t.text);
                centered(canvas, layout, area, format_pan(pan).as_str(), t.dim);
            }
        }
        // A flat bar: the whole column is the fader, filled from the bottom to the level, the dB written on it.
        let Some(fader) = layout.fader(index, app) else { continue };
        box_(canvas, fader, t.off, t.frame);
        let top = fader.y + fader.h - (fader_position(db) * fader.h as f64) as i32;
        let fill = if unknown { t.dim } else if mute { t.amber } else { group_colour(group_of(index)) };
        if db.is_finite() || unknown {
            canvas.fill_rect(fader.x + 1, top, (fader.w - 2).max(0) as u32, (fader.y + fader.h - 1 - top).max(0) as u32, fill);
            canvas.fill_rect(fader.x + 1, top, (fader.w - 2).max(0) as u32, 2, t.text);
        }
        for mark in [6.0, 0.0, -12.0, -30.0, -60.0] {
            let y = fader.y + fader.h - (fader_position(mark) * fader.h as f64) as i32;
            let reach = if mark == 0.0 { fader.w } else { fader.w / 4 };
            canvas.line(fader.x, y, fader.x + reach - 1, y, t.grid_major);
        }
        let value = Rect::new(fader.x, fader.y + 4, fader.w, layout.row);
        centered(canvas, layout, value, format_db(db, mute, unknown).as_str(), if mute { t.amber } else if unknown { t.dim } else { t.text });
        if let Some(area) = layout.meter(index, app) {
            let channels = meter_channels(app, &strip);
            let bars = channels.len() as i32;
            let bar_w = ((area.w - (bars - 1) * 2) / bars).max(1);
            for (n, channel) in channels.enumerate() {
                let x = area.x + n as i32 * (bar_w + 2);
                canvas.fill_rect(x, area.y, bar_w as u32, area.h as u32, t.off);
                let Some((level, over)) = meters.get(channel) else { continue };
                let height = (fader_position(level as f64) * area.h as f64) as i32;
                canvas.fill_rect(x, area.y + area.h - height, bar_w as u32, height.max(0) as u32, level_colour(level));
                if over {
                    canvas.fill_rect(x, area.y, bar_w as u32, 4, t.red);
                }
            }
        }
    }
    // The swipe band: moving the strips from here never touches a fader.
    let band = layout.swipe_band();
    if band.h > 4 && app.strip_count() > 0 {
        box_(canvas, band, t.grid, t.grid_major);
        centered(canvas, layout, band, "<  swipe here to scroll the strips  >", t.dim);
    }
}

fn matrix(canvas: &mut Canvas, layout: &Layout, app: &App) {
    let t = theme();
    let outputs = app.outputs();
    let sources = app.sources();
    for (col, output) in outputs.iter().enumerate() {
        if let Some(area) = layout.cell(usize::MAX, col, app) {
            centered(canvas, layout, area, app.label(output).as_str(), if col == app.col { t.text } else { t.dim });
        }
    }
    for (row, source) in sources.iter().enumerate() {
        if let Some(area) = layout.cell(row, usize::MAX, app) {
            let label = app.label(source);
            let short = fit_name(label.as_str(), (area.w / layout.col).max(0) as usize);
            text(canvas, layout, area.x, area.y + (area.h - layout.row) / 2, short.as_str(), if row == app.row { t.text } else { t.dim });
        }
        for (col, output) in outputs.iter().enumerate() {
            let Some(area) = layout.cell(row, col, app) else { continue };
            let selected = (row, col) == (app.row, app.col);
            let send = app.mix.send(output, source);
            box_(canvas, area, if selected { t.grid_major } else { t.grid }, if selected { t.text } else { t.frame });
            let off = !send.db.is_finite() && !send.unknown && !send.mute;
            let shown = if off { Text::from("-") } else { format_db(send.db, send.mute, send.unknown) };
            let colour = if send.mute { t.amber } else if off || send.unknown { t.dim } else { t.text };
            centered(canvas, layout, area, shown.as_str(), colour);
        }
    }
}

/// A control's `value` as text, for a change not yet made.
fn describe(hardware: &Hardware, index: usize, control: &MixerControl, value: i64) -> Text<32> {
    let mut out = Text::new();
    match control.kind {
        MIXER_CONTROL_BOOL => {
            let _ = out.write_str(if value != 0 { "On" } else { "Off" });
        }
        MIXER_CONTROL_ACTION => {
            let _ = out.write_str("run");
        }
        MIXER_CONTROL_ENUM => {
            let mut buffer = [0u8; 32];
            let len = hardware.control_item(index, value as usize, &mut buffer).min(32);
            let _ = out.write_str(core::str::from_utf8(&buffer[..len]).unwrap_or(""));
        }
        _ => {
            let _ = write!(out, "{value} {}", field(&control.unit));
        }
    }
    out
}

fn device(canvas: &mut Canvas, layout: &Layout, app: &App, hardware: &Hardware) {
    let t = theme();
    for (index, control) in app.controls.iter().enumerate() {
        let Some(area) = layout.control(index, app) else { continue };
        let selected = index == app.ctl_sel;
        box_(canvas, area, if selected { t.grid_major } else { t.grid }, if selected { t.text } else { t.frame });
        let y = area.y + (area.h - layout.row) / 2;
        let x = text(canvas, layout, area.x + layout.col, y, field(&control.name), t.text);
        if app.asks(control) {
            text(canvas, layout, x + layout.col, y, "(asks first)", t.amber);
        } else if control.flags & MIXER_FLAG_HAZARD != 0 {
            text(canvas, layout, x + layout.col, y, "(hazard)", t.amber);
        } else if control.flags & MIXER_FLAG_READONLY != 0 {
            text(canvas, layout, x + layout.col, y, "(read-only)", t.dim);
        }
        let value = app.control_value(hardware, index);
        let width = value.as_str().chars().count() as i32 * layout.col;
        text(canvas, layout, area.x + area.w - width - layout.col, y, value.as_str(), t.text);
    }
    // The app's own settings, after the device's controls.
    let mut rate = Text::<16>::new();
    let _ = write!(rate, "{} Hz", app.rate);
    let own = [
        (app.ask_row(), "Ask before +48V", if app.ask_hazard { "Yes" } else { "No" }),
        (app.rate_row(), "Sample rate", rate.as_str()),
    ];
    for (index, name, value) in own {
        let Some(area) = layout.control(index, app) else { continue };
        let selected = index == app.ctl_sel;
        box_(canvas, area, if selected { t.grid_major } else { t.grid }, if selected { t.text } else { t.frame });
        let y = area.y + (area.h - layout.row) / 2;
        let x = text(canvas, layout, area.x + layout.col, y, name, t.text);
        text(canvas, layout, x + layout.col, y, "(KakelFix, saved in kakelfix.cfg)", t.dim);
        let width = value.len() as i32 * layout.col;
        text(canvas, layout, area.x + area.w - width - layout.col, y, value, t.text);
    }
}

fn dialog(canvas: &mut Canvas, layout: &Layout, app: &App, hardware: &Hardware, index: usize, value: i64) {
    let t = theme();
    let area = layout.dialog();
    box_(canvas, area, t.grid, t.amber);
    let Some(control) = app.controls.get(index) else { return };
    let mut line = Text::<80>::new();
    let _ = write!(line, "Set {} to {}?", field(&control.name), describe(hardware, index, control, value).as_str());
    centered(canvas, layout, Rect::new(area.x, area.y + layout.row, area.w, layout.row), line.as_str(), t.text);
    centered(canvas, layout, Rect::new(area.x, area.y + layout.row * 2 + 4, area.w, layout.row), "This setting can harm connected equipment.", t.amber);
    box_(canvas, layout.yes(), t.grid_major, t.text);
    centered(canvas, layout, layout.yes(), "Yes (y)", t.text);
    box_(canvas, layout.no(), t.grid_major, t.text);
    centered(canvas, layout, layout.no(), "No (n)", t.text);
}
