//! The frt60 screen: three header lines, one big plot (which of four it is depends on the view) and a bank of touch buttons.
//! The geometry scales up on big screens so a finger can hit the buttons.
//!
//! Views: `Summary` (reverberation time against frequency, T20/T30/EDT per band), `Waterfall` (the decay of every band as
//! slices in depth), `Heat` (the same data as a colour map, time downward) and `Decay` (one band's decay curve and the line fitted
//! to it).

use dsp::math::log10;
use gfx::theme::{mix, theme};
use gfx::{Canvas, format_fixed, text_width};
use rt60::decay::FLOOR_DB;
use rt60::{Report, WATERFALL_STEPS, Waterfall};

pub const BUTTONS: usize = 15;
pub const BUTTONS_PER_ROW: usize = 5;

/// The level range the waterfall and the colour map show, from 0 dB down.
pub const RANGE_DB: f32 = 60.0;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum View {
    Summary,
    Waterfall,
    Heat,
    Decay,
}

impl View {
    pub fn next(self) -> View {
        match self {
            View::Summary => View::Waterfall,
            View::Waterfall => View::Heat,
            View::Heat => View::Decay,
            View::Decay => View::Summary,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            View::Summary => "summary",
            View::Waterfall => "waterfall",
            View::Heat => "heat",
            View::Decay => "decay",
        }
    }

    pub fn from_name(name: &str) -> Option<View> {
        [View::Summary, View::Waterfall, View::Heat, View::Decay].into_iter().find(|v| v.name() == name)
    }
}

#[derive(Clone, Copy)]
pub struct Layout {
    pub width: u32,
    /// Text and button size: 1, or 2 on a big screen.
    pub text: u32,
    pub left: i32,
    pub top: i32,
    pub plot_w: u32,
    pub plot_h: u32,
    pub button_top: i32,
    pub button_h: i32,
}

impl Layout {
    /// `text` forces the font scale (1 or 2); None picks 2 on a screen of 1280x700 or more.
    pub fn new(width: u32, height: u32, text: Option<u32>) -> Self {
        let text = text.unwrap_or(if width >= 1280 && height >= 700 { 2 } else { 1 });
        let line = 16 * text as i32 + 4;
        let header = 3 * line + 6;
        let button_h = 19 * text as i32 + 3;
        let button_top = height as i32 - 3 * button_h - 9;
        let labels = 24;
        let left = 56;
        Self {
            width,
            text,
            left,
            top: header,
            plot_w: (width as i32 - left - 16) as u32,
            plot_h: (button_top - header - labels - 8).max(40) as u32,
            button_top,
            button_h,
        }
    }

    pub fn bottom(&self) -> i32 {
        self.top + self.plot_h as i32
    }

    /// The button rectangle `(x, y, w, h)`.
    pub fn button(&self, index: usize) -> (i32, i32, u32, u32) {
        let (row, column) = (index / BUTTONS_PER_ROW, index % BUTTONS_PER_ROW);
        let w = self.width as i32 / BUTTONS_PER_ROW as i32;
        (column as i32 * w + 2, self.button_top + row as i32 * (self.button_h + 3), (w - 4) as u32, self.button_h as u32)
    }

    pub fn button_at(&self, x: i32, y: i32) -> Option<usize> {
        (0..BUTTONS).find(|&i| {
            let (bx, by, bw, bh) = self.button(i);
            x >= bx && x < bx + bw as i32 && y >= by && y < by + bh as i32
        })
    }

    /// The y of header text row `row`.
    pub fn row(&self, row: i32) -> i32 {
        4 + row * (16 * self.text as i32 + 4)
    }

    /// The band (0..count) a tap at `x` in the plot is nearest to; None outside the plot.
    pub fn band_at(&self, x: i32, y: i32, count: usize) -> Option<usize> {
        if count == 0 || x < self.left || x >= self.left + self.plot_w as i32 || y < self.top || y > self.bottom() {
            return None;
        }
        Some(((x - self.left) as usize * count / self.plot_w as usize).min(count - 1))
    }
}

/// The background (once, and after a theme change).
pub fn draw_grid(canvas: &mut Canvas) {
    canvas.clear(theme().background);
}

/// What the plot draws. `reports` has one entry per band and the overall figure after them.
pub struct Data<'a> {
    pub centres: &'a [f32],
    pub reports: &'a [Report],
    pub waterfall: &'a Waterfall,
    /// The selected band's decay curve in dB (from the direct sound) and the sample rate; None without a measurement.
    pub curve: Option<(&'a [f32], u32)>,
    /// 0..bands, or `bands` for the overall figure.
    pub selected: usize,
}

impl Data<'_> {
    fn measured(&self) -> bool {
        self.reports.iter().any(|r| r.dynamic_db > 0.0)
    }
}

fn label_y(layout: &Layout) -> i32 {
    layout.bottom() + 6
}

/// A level 0..=1 of the waterfall range (0 for the floor).
fn unit(db: f32) -> f32 {
    ((db + RANGE_DB) / RANGE_DB).clamp(0.0, 1.0)
}

/// A colour map black - blue - cyan - yellow - red for `t` in 0..=1.
fn heat(t: f32) -> u32 {
    const STOPS: [u32; 5] = [0x000010, 0x1030c0, 0x10c0d0, 0xf0e020, 0xf03010];
    let scaled = t.clamp(0.0, 1.0) * 4.0;
    let i = (scaled as usize).min(3);
    mix(STOPS[i], STOPS[i + 1], ((scaled - i as f32) * 256.0) as u32, 256)
}

/// A "nice" top for a seconds axis: the next of 0.5, 1, 1.5, 2, 3, 4, 5, 6, 8, 10 ... above `value`.
pub fn nice_top(value: f32) -> f32 {
    [0.5, 1.0, 1.5, 2.0, 3.0, 4.0, 5.0, 6.0, 8.0, 10.0, 15.0, 20.0].into_iter().find(|&top| top >= value).unwrap_or(30.0)
}

fn seconds_text(buffer: &mut [u8], value: f32) -> &str {
    format_fixed(buffer, value, if value < 10.0 { 1 } else { 0 })
}

/// The preferred (nominal) name of a band centre: 251.2 Hz is "250", 1995 Hz "2k", 3162 Hz "3.15k".
fn nominal_name(hz: f32) -> &'static str {
    const NAMES: [(f32, &str); 31] = [
        (20.0, "20"), (25.0, "25"), (31.5, "31.5"), (40.0, "40"), (50.0, "50"), (63.0, "63"), (80.0, "80"), (100.0, "100"),
        (125.0, "125"), (160.0, "160"), (200.0, "200"), (250.0, "250"), (315.0, "315"), (400.0, "400"), (500.0, "500"),
        (630.0, "630"), (800.0, "800"), (1000.0, "1k"), (1250.0, "1.25k"), (1600.0, "1.6k"), (2000.0, "2k"),
        (2500.0, "2.5k"), (3150.0, "3.15k"), (4000.0, "4k"), (5000.0, "5k"), (6300.0, "6.3k"), (8000.0, "8k"),
        (10000.0, "10k"), (12500.0, "12.5k"), (16000.0, "16k"), (20000.0, "20k"),
    ];
    let log = |value: f32| log10(value.max(1.0));
    let mut best = NAMES[0];
    for entry in NAMES {
        if (log(entry.0) - log(hz)).abs() < (log(best.0) - log(hz)).abs() {
            best = entry;
        }
    }
    best.1
}

/// Names the bands under the plot: skips a label that would touch the previous one (the selected band always shows).
fn band_labels(canvas: &mut Canvas, layout: &Layout, centres: &[f32], selected: usize, x_of: impl Fn(usize) -> i32) {
    let mut last = -1000; // far enough left that the first label always shows (i32::MIN would overflow the difference)
    for (i, &hz) in centres.iter().enumerate() {
        let x = x_of(i);
        if x - last < 44 && i != selected {
            continue;
        }
        let name = nominal_name(hz);
        canvas.text(x - text_width(name) as i32 / 2, label_y(layout), name, if i == selected { theme().text } else { theme().dim });
        last = x;
    }
}

fn frame_box(canvas: &mut Canvas, layout: &Layout) {
    canvas.rect(layout.left, layout.top, layout.plot_w, layout.plot_h, theme().grid_major);
}

pub fn draw_plot(canvas: &mut Canvas, layout: &Layout, view: View, data: &Data) {
    if !data.measured() || data.centres.len() < 2 {
        frame_box(canvas, layout);
        return canvas.text_centered(layout.left + layout.plot_w as i32 / 2, layout.top + layout.plot_h as i32 / 2 - 8, "no measurement yet", theme().dim);
    }
    match view {
        View::Summary => summary(canvas, layout, data),
        View::Waterfall => waterfall(canvas, layout, data),
        View::Heat => heat_map(canvas, layout, data),
        View::Decay => decay(canvas, layout, data),
    }
}

fn summary(canvas: &mut Canvas, layout: &Layout, data: &Data) {
    let count = data.centres.len();
    let mut highest = 0.0f32;
    for r in data.reports {
        for t in [r.edt, r.t20, r.t30] {
            if t.is_finite() && t > highest {
                highest = t;
            }
        }
    }
    let top = nice_top(highest * 1.05);
    let y_of = |seconds: f32| layout.bottom() - (seconds / top * layout.plot_h as f32) as i32;
    for tick in 0..=5 {
        let seconds = top * tick as f32 / 5.0;
        let y = y_of(seconds);
        canvas.fill_rect(layout.left, y, layout.plot_w, 1, theme().grid);
        let mut text = [0u8; 16];
        canvas.text_right(layout.left - 6, y - 8, seconds_text(&mut text, seconds), theme().dim);
    }
    let (first, last) = (log10(data.centres[0]), log10(data.centres[count - 1]));
    let pad = 0.5 / count as f32 * (last - first);
    let span = (last - first) + 2.0 * pad;
    let x_of = |i: usize| layout.left + ((log10(data.centres[i]) - first + pad) / span * layout.plot_w as f32) as i32;
    for i in 0..count {
        canvas.fill_rect(x_of(i), layout.top, 1, layout.plot_h, if i == data.selected { theme().grid_major } else { theme().grid });
    }
    // The overall T20 as a level line across the plot
    let all = data.reports[count];
    if all.t20.is_finite() {
        canvas.fill_rect(layout.left, y_of(all.t20), layout.plot_w, 1, theme().dim);
    }
    let edt_colour = theme().curves[1];
    let series: [(u32, fn(&Report) -> f32, i32); 3] = [(edt_colour, |r| r.edt, 3), (theme().amber, |r| r.t30, 4), (theme().green, |r| r.t20, 5)];
    for (colour, pick, size) in series {
        let mut previous: Option<(i32, i32)> = None;
        for i in 0..count {
            let t = pick(&data.reports[i]);
            if !t.is_finite() {
                previous = None;
                continue;
            }
            let (x, y) = (x_of(i), y_of(t));
            if let Some((px, py)) = previous {
                canvas.line(px, py, x, y, colour);
                canvas.line(px, py + 1, x, y + 1, colour);
            }
            canvas.fill_rect(x - size / 2, y - size / 2, size as u32, size as u32, colour);
            previous = Some((x, y));
        }
    }
    frame_box(canvas, layout);
    band_labels(canvas, layout, data.centres, data.selected, x_of);
    legend(canvas, layout, &[("T20", theme().green), ("T30", theme().amber), ("EDT", edt_colour)]);
}

fn legend(canvas: &mut Canvas, layout: &Layout, entries: &[(&str, u32)]) {
    let mut x = layout.left + layout.plot_w as i32 - 8;
    for (name, colour) in entries {
        x -= text_width(name) as i32 + 16;
        canvas.fill_rect(x - 10, layout.top + 12, 8, 8, *colour);
        canvas.text(x, layout.top + 8, name, theme().text);
    }
}

fn waterfall(canvas: &mut Canvas, layout: &Layout, data: &Data) {
    let wf = data.waterfall;
    let count = wf.bands.min(data.centres.len());
    if count < 2 {
        return;
    }
    const SLICES: i32 = 32;
    let skew_x = layout.plot_w as i32 / 5;
    let skew_y = layout.plot_h as i32 * 2 / 5;
    let front_w = layout.plot_w as i32 - skew_x - 24;
    let height = layout.plot_h as i32 - skew_y - 8;
    let shift = |slice: i32| (slice * skew_x / (SLICES - 1), slice * skew_y / (SLICES - 1));
    let x_of = |band: usize, dx: i32| layout.left + 8 + band as i32 * front_w / (count as i32 - 1) + dx;
    let background = theme().background;
    for slice in (0..SLICES).rev() {
        // the impulse (0 dB everywhere) is at the back: a tall wall in front would hide the quieter, later slices
        let step = (SLICES - 1 - slice) as usize * (WATERFALL_STEPS - 1) / (SLICES as usize - 1);
        let (dx, dy) = shift(slice);
        let base = layout.bottom() - 4 - dy;
        let colour = mix(theme().green, theme().dim, slice as u32, SLICES as u32 - 1);
        let y_of = |band: usize| base - (unit(wf.cells[band][step]) * height as f32) as i32;
        for band in 0..count - 1 {
            let (x0, x1) = (x_of(band, dx), x_of(band + 1, dx));
            let (y0, y1) = (y_of(band), y_of(band + 1));
            for x in x0..=x1 {
                let y = y0 + (y1 - y0) * (x - x0) / (x1 - x0).max(1);
                canvas.fill_rect(x, y, 1, (base - y + 1).max(1) as u32, background);
                canvas.set(x, y, colour);
                canvas.set(x, y - 1, colour);
            }
        }
    }
    frame_box(canvas, layout);
    // time along the depth edge, frequencies along the front
    for tick in 0..=2 {
        let (dx, dy) = shift(tick * (SLICES - 1) / 2);
        let x = (x_of(count - 1, dx) + 6).min(layout.left + layout.plot_w as i32 - 40);
        let mut text = [0u8; 16];
        let end = canvas.text(x, layout.bottom() - 4 - dy - 8, seconds_text(&mut text, wf.span_s * (2 - tick) as f32 / 2.0), theme().dim);
        canvas.text(end, layout.bottom() - 4 - dy - 8, "s", theme().dim);
    }
    band_labels(canvas, layout, data.centres, data.selected, |i| x_of(i, 0));
    canvas.text(layout.left + 8, layout.top + 8, "0 to -60 dB; time runs back to the impulse", theme().dim);
}

fn heat_map(canvas: &mut Canvas, layout: &Layout, data: &Data) {
    let wf = data.waterfall;
    let count = wf.bands.min(data.centres.len());
    if count < 2 {
        return;
    }
    let row_h = layout.plot_h as f32 / WATERFALL_STEPS as f32;
    for x in 0..layout.plot_w as i32 {
        // pixel column -> a position between band centres, the level interpolated across it
        let at = (x as f32 + 0.5) / layout.plot_w as f32 * count as f32 - 0.5;
        let low = (at.max(0.0) as usize).min(count - 1);
        let high = (low + 1).min(count - 1);
        let share = (at - low as f32).clamp(0.0, 1.0);
        for step in 0..WATERFALL_STEPS {
            let a = wf.cells[low][step].max(-RANGE_DB);
            let b = wf.cells[high][step].max(-RANGE_DB);
            let y = layout.top + (step as f32 * row_h) as i32;
            let y_next = layout.top + ((step + 1) as f32 * row_h) as i32;
            canvas.fill_rect(layout.left + x, y, 1, (y_next - y).max(1) as u32, heat(unit(a + (b - a) * share)));
        }
    }
    frame_box(canvas, layout);
    for tick in 0..=4 {
        let y = layout.top + tick * layout.plot_h as i32 / 4;
        let mut text = [0u8; 16];
        canvas.text_right(layout.left - 6, (y - 8).max(layout.top), seconds_text(&mut text, wf.span_s * tick as f32 / 4.0), theme().dim);
    }
    let x_of = |i: usize| layout.left + (i as i32 * 2 + 1) * layout.plot_w as i32 / (2 * count as i32);
    if data.selected < count {
        canvas.fill_rect(x_of(data.selected) - 1, layout.top, 2, layout.plot_h, theme().text);
    }
    band_labels(canvas, layout, data.centres, data.selected, x_of);
    let right = layout.left + layout.plot_w as i32;
    for i in 0..60 {
        canvas.fill_rect(right - 70 + i, layout.top + 6, 1, 8, heat(i as f32 / 59.0));
    }
    canvas.text_right(right - 74, layout.top + 4, "-60", theme().text);
    canvas.text(right - 8 - 24, layout.top + 16, "0 dB", theme().text);
}

fn decay(canvas: &mut Canvas, layout: &Layout, data: &Data) {
    let Some((curve, rate)) = data.curve else { return };
    let report = data.reports[data.selected.min(data.reports.len() - 1)];
    let seconds = (report.used_s * 1.05).clamp(0.2, curve.len() as f32 / rate as f32);
    let depth = ((((report.dynamic_db + 10.0).min(100.0) / 10.0) as i32) * 10).max(40) as f32;
    let (w, h) = (layout.plot_w as i32, layout.plot_h as i32);
    let y_of = |db: f32| layout.top + (-db.clamp(-depth, 0.0) / depth * h as f32) as i32;
    let mut db = 0.0;
    while db >= -depth {
        let y = y_of(db);
        canvas.fill_rect(layout.left, y, layout.plot_w, 1, theme().grid);
        let mut text = [0u8; 16];
        canvas.text_right(layout.left - 6, (y - 8).max(layout.top), format_fixed(&mut text, db, 0), theme().dim);
        db -= 10.0;
    }
    for tick in 0..=5 {
        let x = layout.left + tick * w / 5;
        canvas.fill_rect(x, layout.top, 1, layout.plot_h, theme().grid);
        let mut text = [0u8; 16];
        let end = canvas.text(x - 8, label_y(layout), seconds_text(&mut text, seconds * tick as f32 / 5.0), theme().dim);
        canvas.text(end, label_y(layout), "s", theme().dim);
    }
    // the evaluation range of T20: -5 to -25 dB
    for level in [-5.0, -25.0] {
        canvas.fill_rect(layout.left, y_of(level), layout.plot_w, 1, theme().grid_major);
    }
    let mut previous: Option<(i32, i32)> = None;
    for x in 0..w {
        let index = ((x as f32 / w as f32 * seconds) * rate as f32) as usize;
        let level = curve.get(index).copied().unwrap_or(FLOOR_DB);
        if level <= FLOOR_DB + 1.0 {
            break;
        }
        let point = (layout.left + x, y_of(level));
        if let Some((px, py)) = previous {
            canvas.line(px, py, point.0, point.1, theme().curves[1]);
            canvas.line(px, py + 1, point.0, point.1 + 1, theme().curves[1]);
        }
        previous = Some(point);
    }
    if report.slope < 0.0 {
        canvas.line(layout.left, y_of(report.line_db), layout.left + w, y_of(report.line_db + report.slope * seconds), theme().amber);
    }
    frame_box(canvas, layout);
    legend(canvas, layout, &[("T20 fit", theme().amber), ("decay", theme().curves[1])]);
}

pub fn draw_buttons(canvas: &mut Canvas, layout: &Layout, labels: &[(&str, bool)]) {
    for (i, (label, active)) in labels.iter().enumerate().take(BUTTONS) {
        let (x, y, w, h) = layout.button(i);
        canvas.fill_rect(x, y, w, h, if *active { theme().grid_major } else { theme().grid });
        canvas.rect(x, y, w, h, theme().dim);
        let width = text_width(label) as i32 * layout.text as i32;
        canvas.text_scaled(x + (w as i32 - width) / 2, y + (h as i32 - 16 * layout.text as i32) / 2, label, layout.text, theme().text);
    }
}
