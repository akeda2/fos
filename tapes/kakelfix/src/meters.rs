//! The Meters page: every captured input metered at once (sample peak and Nordic PPM per input, EBU R128 loudness per
//! pair), a 60 s history of each, and the drawing: a bar per input (or pair) on the chosen scale with the input's gain
//! and switches under it, the selected one read out large along the bottom. Capture is `analyze::Capture`'s; this
//! takes the frames it reads. Everything is taken from the heap once, at startup.

use crate::Shadowed as Hardware;
use crate::analyze::channel_name;
use crate::view::{centered, text};
use audio_screen::Input;
use audio_screen::meters::{Channel, Reading};
use core::fmt::Write;
use dsp::loudness::Loudness;
use dsp::pcm::unpack_channel;
use fosfor_abi::tape::{MIXER_CONTROL_BOOL, MIXER_FLAG_HAZARD, TapeApi};
use gfx::Canvas;
use gfx::meter::{Scale, draw_bar, draw_history, draw_scale};
use gfx::theme::theme;
use kakelfix::meters::short_label;
use kakelfix::{App, Layout, MeterScale, Rect, Text, field};
use tape_rt::alloc_zeroed;

/// Inputs metered (more are captured but not shown) and their pairs.
pub const MAX_INPUTS: usize = mixer_model::MAX_CHANNELS;
const MAX_PAIRS: usize = MAX_INPUTS / 2;
/// History values, one per 100 ms: a minute.
const HISTORY: usize = 600;
const HISTORY_STEP_US: u64 = 100_000;
/// Readings are taken at most this often (the holds and the PPM's fall need a steady pace, not every audio read).
const READ_STEP_US: u64 = 20_000;
/// The EBU R128 programme target.
pub const TARGET_LUFS: f32 = -23.0;

/// Loudness in LUFS, 6 LU a tick either side of the -23 target: amber 1 LU above it, red 5 LU above.
pub const LUFS: Scale = Scale {
    name: "LUFS",
    min: -59.0,
    max: -5.0,
    dbfs_at_zero: 0.0,
    warn: TARGET_LUFS + 1.0,
    over: TARGET_LUFS + 5.0,
    ticks: &[(-5.0, "-5"), (-11.0, "-11"), (-17.0, "-17"), (-23.0, "-23"), (-29.0, "-29"), (-35.0, "-35"), (-41.0, "-41"), (-47.0, "-47"), (-53.0, "-53"), (-59.0, "-59")],
};

fn scale_of(kind: MeterScale) -> &'static Scale {
    match kind {
        MeterScale::Dbfs => &Scale::DBFS,
        MeterScale::Nordic => &Scale::NORDIC,
        MeterScale::Lufs => &LUFS,
    }
}

const SILENT: Reading = Reading { level: -144.0, hold: -144.0, clipped: false };

pub struct Bank {
    channels: &'static mut [Channel; MAX_INPUTS],
    loudness: &'static mut [Loudness; MAX_PAIRS],
    /// `[dBFS peak, Nordic]` per input, as last read.
    readings: [[Reading; 2]; MAX_INPUTS],
    /// The highest of each since the last history step.
    highest: [[f32; 2]; MAX_INPUTS],
    /// Per input `[dBFS, Nordic]`, per pair short-term loudness; rings of `HISTORY`, `head` the next to write.
    history: &'static mut [[[f32; HISTORY]; 2]; MAX_INPUTS],
    loudness_history: &'static mut [[f32; HISTORY]; MAX_PAIRS],
    head: usize,
    filled: usize,
    inputs: usize,
    last_read: u64,
    last_step: u64,
    seen: u32,
}

impl Bank {
    /// For `channels` captured at `rate`; None when the kernel has no memory.
    pub fn new(api: &TapeApi, rate: u32, channels: usize) -> Option<Bank> {
        let bank = Bank {
            channels: alloc_zeroed(api)?,
            loudness: alloc_zeroed(api)?,
            readings: [[SILENT; 2]; MAX_INPUTS],
            highest: [[-144.0; 2]; MAX_INPUTS],
            history: alloc_zeroed(api)?,
            loudness_history: alloc_zeroed(api)?,
            head: 0,
            filled: 0,
            inputs: channels.min(MAX_INPUTS),
            last_read: 0,
            last_step: 0,
            seen: 0,
        };
        // Both hold only numbers, so zeroed memory is a valid value to assign over.
        for channel in bank.channels.iter_mut().take(bank.inputs) {
            *channel = Channel::new(rate);
        }
        for pair in bank.loudness.iter_mut().take(bank.inputs.div_ceil(2)) {
            *pair = Loudness::new(rate);
        }
        Some(bank)
    }

    fn reset(&mut self) {
        for channel in self.channels.iter_mut().take(self.inputs) {
            channel.reset();
        }
        for pair in self.loudness.iter_mut().take(self.inputs.div_ceil(2)) {
            pair.reset();
        }
        self.filled = 0;
        self.head = 0;
    }

    /// Feeds every input from what capture has, takes readings and history steps when they are due; true when there was
    /// audio.
    pub fn update(&mut self, api: &TapeApi, input: &Input, scratch: &mut [u8], left: &mut [f32], right: &mut [f32], generation: u32) -> bool {
        if self.seen != generation {
            self.seen = generation;
            self.reset();
        }
        let frame = input.frame_bytes();
        if frame == 0 {
            return false;
        }
        let mut fresh = false;
        loop {
            let got = (api.audio_read)(scratch.as_mut_ptr(), scratch.len() / frame * frame);
            if got < frame {
                break;
            }
            fresh = true;
            let data = &scratch[..got];
            let unpack = |out: &mut [f32], channel| unpack_channel(data, out, input.channels, input.slot, input.bits, input.float, channel);
            for pair in 0..self.inputs.div_ceil(2) {
                let (first, second) = (pair * 2, pair * 2 + 1);
                let frames = unpack(left, first);
                self.channels[first].feed(&left[..frames]);
                if second < self.inputs {
                    unpack(right, second);
                    self.channels[second].feed(&right[..frames]);
                } else {
                    // A last odd input is measured alone: silence beside it, not itself twice (+3 LU).
                    right[..frames].fill(0.0);
                }
                self.loudness[pair].feed(&left[..frames], &right[..frames]);
            }
        }
        let now = tape_rt::micros();
        let since = now.wrapping_sub(self.last_read);
        if since >= READ_STEP_US {
            self.last_read = now;
            let dt = (since as f32 / 1e6).min(0.5);
            for input in 0..self.inputs {
                self.readings[input] = self.channels[input].read_both(dt);
                for kind in 0..2 {
                    self.highest[input][kind] = self.highest[input][kind].max(self.readings[input][kind].level);
                }
            }
        }
        if now.wrapping_sub(self.last_step) >= HISTORY_STEP_US {
            self.last_step = now;
            for input in 0..self.inputs {
                for kind in 0..2 {
                    self.history[input][kind][self.head] = self.highest[input][kind];
                    self.highest[input][kind] = -144.0;
                }
            }
            for pair in 0..self.inputs.div_ceil(2) {
                self.loudness_history[pair][self.head] = self.loudness[pair].short_term();
            }
            self.head = (self.head + 1) % HISTORY;
            self.filled = (self.filled + 1).min(HISTORY);
        }
        fresh
    }

    /// The newest `out.len()` (at most) history values of the selected input or pair on `kind`'s scale, oldest first.
    fn history(&self, kind: MeterScale, input: usize, out: &mut [f32]) -> usize {
        let ring = match kind {
            MeterScale::Dbfs => &self.history[input][0],
            MeterScale::Nordic => &self.history[input][1],
            MeterScale::Lufs => &self.loudness_history[input / 2],
        };
        let count = self.filled.min(out.len());
        for (i, value) in out[..count].iter_mut().enumerate() {
            *value = ring[(self.head + HISTORY - count + i) % HISTORY];
        }
        count
    }
}

/// A pair's name: "Analog 1/2" for "Analog 1" and "Analog 2", else "A+B"; a last odd input alone.
pub(crate) fn pair_name(app: &App, pair: usize, inputs: usize) -> Text<24> {
    let first = channel_name(app, pair * 2);
    let mut out = Text::new();
    if pair * 2 + 1 >= inputs {
        let _ = out.write_str(first.as_str());
        return out;
    }
    let second = channel_name(app, pair * 2 + 1);
    let _ = match (first.as_str().rsplit_once(' '), second.as_str().rsplit_once(' ')) {
        (Some((a, x)), Some((b, y))) if a == b => write!(out, "{a} {x}/{y}"),
        _ => write!(out, "{}+{}", first.as_str(), second.as_str()),
    };
    out
}

/// A level for the readouts: one decimal, "-inf" below the meter's floor.
fn level(out: &mut Text<64>, value: f32) {
    if value <= -100.0 {
        let _ = out.write_str("-inf");
    } else {
        let tenths = (value * 10.0 + if value < 0.0 { -0.5 } else { 0.5 }) as i32;
        let _ = write!(out, "{}{}.{}", if tenths < 0 { "-" } else if tenths > 0 { "+" } else { "" }, (tenths / 10).abs(), (tenths % 10).abs());
    }
}

pub fn draw(canvas: &mut Canvas, layout: &Layout, app: &App, hardware: &Hardware, bank: Option<&Bank>, problem: Option<&str>) {
    let t = theme();
    if let Some(reason) = problem {
        return centered(canvas, layout, layout.body, reason, t.dim);
    }
    if app.picking.is_some() {
        return picker(canvas, layout, app);
    }
    let Some(bank) = bank else { return };
    let m = &app.metering;
    let scale = scale_of(m.scale);
    let area = layout.meter_area();
    let first_bar = layout.meter_bar(app.first_meter, app).unwrap_or(area);
    let span = layout.meter_span(app);
    draw_scale(canvas, scale, area.x, span as u32, first_bar.y, first_bar.h as u32);
    if m.scale == MeterScale::Lufs {
        canvas.fill_rect(area.x, scale.row(TARGET_LUFS, first_bar.y, first_bar.h as u32), span as u32, 1, t.amber);
    }
    let per = m.per_bar();
    for bar in 0..m.bars() {
        let (Some(column), Some(area), Some(first)) = (layout.meter_column(bar, app), layout.meter_bar(bar, app), m.bar_input(bar)) else { continue };
        let selected = bar == m.bar();
        if selected {
            canvas.fill_rect(column.x - 2, column.y - 14, (column.w + 4) as u32, (column.h + 14) as u32, t.grid);
        }
        let bar_w = (area.w - 8).min(layout.col * 3 * per as i32);
        let x = area.x + (area.w - bar_w) / 2;
        let (value, hold, clipped, name) = if m.scale == MeterScale::Lufs {
            let loudness = &bank.loudness[(first / 2).min(MAX_PAIRS - 1)];
            let clipped = (first..(first + 2).min(bank.inputs)).any(|i| bank.readings[i][0].clipped);
            (loudness.momentary(), loudness.short_term(), clipped, pair_name(app, first / 2, bank.inputs))
        } else {
            let reading = bank.readings[first.min(MAX_INPUTS - 1)][(m.scale == MeterScale::Nordic) as usize];
            let mut name = Text::new();
            let _ = name.write_str(channel_name(app, first).as_str());
            (reading.level, reading.hold, reading.clipped, name)
        };
        draw_bar(canvas, scale, x, bar_w as u32, area.y, area.h as u32, value, hold, clipped);
        if let Some(label) = layout.meter_name(bar, app) {
            centered(canvas, layout, label, name.as_str(), if selected { t.text } else { t.dim });
        }
        for input in first..(first + per).min(m.channels) {
            for (row, &index) in app.input_controls(input).iter().enumerate() {
                let Some(cell) = layout.meter_cell(input, row, app) else { continue };
                let control = app.controls[index];
                let on = control.kind == MIXER_CONTROL_BOOL && control.value != 0;
                let hazard_on = on && control.flags & MIXER_FLAG_HAZARD != 0;
                let fill = if hazard_on { t.red } else if on { t.amber } else { t.off };
                let frame = if input == m.input { t.text } else { t.frame };
                canvas.fill_rect(cell.x, cell.y, cell.w as u32, cell.h as u32, fill);
                canvas.rect(cell.x, cell.y, cell.w as u32, cell.h as u32, frame);
                let shown = app.control_short(hardware, index);
                let ink = if on { t.background } else if control.kind == MIXER_CONTROL_BOOL { t.dim } else { t.text };
                centered(canvas, layout, cell, shown.as_str(), ink);
            }
        }
    }
    readout(canvas, layout, app, hardware, bank);
}

/// The inputs picker over the page: presets, then every input lit when shown, then Done; the cursor framed in amber.
fn picker(canvas: &mut Canvas, layout: &Layout, app: &App) {
    let t = theme();
    let b = layout.body;
    text(canvas, layout, b.x + 8, b.y + 4, "Show on the Meters page:  a preset, or tap inputs on and off", t.text);
    let groups = app.meter_groups();
    let m = &app.metering;
    if let Some(first) = layout.pick(groups.len(), app) {
        text(canvas, layout, b.x + 8, first.y - layout.row - 6, "Inputs", t.dim);
    }
    for item in 0..app.pick_items() {
        let Some(area) = layout.pick(item, app) else { continue };
        let (label, lit) = if let Some((name, mask)) = groups.get(item) {
            (Text::<24>::from(name.as_str()), m.shown & m.all() == *mask)
        } else if item < groups.len() + m.channels.max(1) {
            let input = item - groups.len();
            (Text::from(channel_name(app, input).as_str()), m.is_shown(input))
        } else {
            (Text::from("Done"), false)
        };
        let cursor = app.picking == Some(item);
        canvas.fill_rect(area.x, area.y, area.w as u32, area.h as u32, if lit { t.curves[0] } else { t.grid });
        canvas.rect(area.x, area.y, area.w as u32, area.h as u32, if cursor { t.amber } else { t.frame });
        if cursor {
            canvas.rect(area.x + 1, area.y + 1, (area.w - 2).max(0) as u32, (area.h - 2).max(0) as u32, t.amber);
        }
        centered(canvas, layout, area, label.as_str(), if lit { t.background } else { t.text });
    }
}

/// The selected input (or pair) along the bottom: its name and controls, its levels in large digits, its history.
fn readout(canvas: &mut Canvas, layout: &Layout, app: &App, hardware: &Hardware, bank: &Bank) {
    let t = theme();
    let m = &app.metering;
    let strip = layout.meter_strip();
    canvas.fill_rect(strip.x, strip.y - 4, strip.w as u32, 1, t.grid_major);
    let history_w = (strip.w * 2 / 5).min(HISTORY as i32);
    let chart = Rect::new(strip.x + strip.w - history_w, strip.y + 4, history_w, strip.h - 8);
    let mut line = Text::<64>::new();
    let name = if m.scale == MeterScale::Lufs { pair_name(app, m.input / 2, bank.inputs) } else { Text::from(channel_name(app, m.input).as_str()) };
    let mut x = text(canvas, layout, strip.x, strip.y + 4, name.as_str(), t.text) + layout.col * 2;
    for &index in app.input_controls(m.input).iter() {
        let control = &app.controls[index];
        line.clear();
        let _ = write!(line, "{} {}", short_label(field(&control.name)), app.control_value(hardware, index).as_str());
        x = text(canvas, layout, x, strip.y + 4, line.as_str(), t.dim) + layout.col * 2;
    }
    line.clear();
    let input = m.input.min(MAX_INPUTS - 1);
    match m.scale {
        MeterScale::Lufs => {
            let loudness = &bank.loudness[(m.input / 2).min(MAX_PAIRS - 1)];
            let _ = line.write_str("M ");
            level(&mut line, loudness.momentary());
            let _ = line.write_str("  S ");
            level(&mut line, loudness.short_term());
            let _ = line.write_str("  I ");
            match loudness.integrated() {
                Some(value) => level(&mut line, value),
                None => {
                    let _ = line.write_str("--");
                }
            }
            let _ = line.write_str("  LRA ");
            match loudness.range() {
                Some(value) => {
                    let _ = write!(line, "{}.{}", value as i32, ((value * 10.0) as i32 % 10).abs());
                }
                None => {
                    let _ = line.write_str("--");
                }
            }
        }
        kind => {
            let reading = bank.readings[input][(kind == MeterScale::Nordic) as usize];
            level(&mut line, reading.level);
            let _ = line.write_str(if kind == MeterScale::Nordic { " PPM  hold " } else { " dBFS  hold " });
            level(&mut line, reading.hold);
            if reading.clipped {
                let _ = line.write_str("  CLIP");
            }
        }
    }
    canvas.text_scaled(strip.x, strip.y + layout.row + 8, line.as_str(), layout.scale * 2, t.text);
    let mut values = [0f32; HISTORY];
    let count = bank.history(m.scale, if m.scale == MeterScale::Lufs { input & !1 } else { input }, &mut values[..chart.w as usize]);
    let scale = scale_of(m.scale);
    draw_history(canvas, scale, chart.x, chart.y, chart.w as u32, chart.h as u32, &values[..count]);
    line.clear();
    let what = if m.scale == MeterScale::Lufs { "short-term" } else { "peak" };
    let _ = write!(line, "{what}, last {} s", chart.w as usize / 10);
    text(canvas, layout, chart.x - (line.as_str().len() as i32 + 1) * layout.col, chart.y + chart.h - layout.row, line.as_str(), t.dim);
}
