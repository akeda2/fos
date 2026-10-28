//! The miccal screen: the spectrum plot (log frequency, dB re full scale) with the live, mic, reference and tone curves, the
//! correction plot under it, and a bank of touch buttons at the bottom. The geometry scales up on big screens so a finger
//! can hit the buttons.

use dsp::math::log10;
use gfx::plot::{Frame, Scale, axis_label, draw_db_grid};
use gfx::theme::theme;
use gfx::{Canvas, format_fixed, text_width};

const LOW_HZ: f32 = 20.0;
const HIGH_HZ: f32 = 20000.0;
pub const BUTTONS: usize = 16;
pub const BUTTONS_PER_ROW: usize = 8;

/// The spectrum plot's vertical scale: 0 dB at the top, 120 dB range.
pub const SPECTRUM: Scale = Scale { db_top: 0.0, db_range: 120.0 };

#[derive(Clone, Copy)]
pub struct Layout {
    pub width: u32,
    /// Text and button size: 1, or 2 on a big screen.
    pub text: u32,
    pub spectrum: Frame,
    pub correction: Frame,
    pub button_top: i32,
    pub button_h: i32,
}

impl Layout {
    pub fn new(width: u32, height: u32) -> Self {
        let text = if width >= 1280 && height >= 700 { 2 } else { 1 };
        let line = 16 * text as i32 + 4;
        let header = 2 * line + 6;
        let button_h = 28 * text as i32 + 8;
        let button_top = height as i32 - 2 * button_h - 6;
        let labels = 22;
        let plots = button_top - header - 2 * labels - 6;
        let spectrum_h = plots * 3 / 5;
        let correction_h = plots - spectrum_h;
        let spectrum_end = header + spectrum_h + labels;
        let correction_top = spectrum_end + 6;
        let correction_end = correction_top + correction_h + labels;
        Self {
            width,
            text,
            spectrum: Frame::new(width, spectrum_end as u32, 56, header, 16, labels),
            correction: Frame::new(width, correction_end as u32, 56, correction_top, 16, labels),
            button_top,
            button_h,
        }
    }

    pub fn x(&self, hz: f32) -> i32 {
        let span = log10(HIGH_HZ) - log10(LOW_HZ);
        self.spectrum.left + ((log10(hz) - log10(LOW_HZ)) / span * self.spectrum.plot_w as f32) as i32
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
}

/// The part that stays: background, the spectrum grid and the frequency lines of both plots.
pub fn draw_grid(canvas: &mut Canvas, layout: &Layout) {
    draw_db_grid(canvas, &layout.spectrum, &SPECTRUM);
    for frame in [&layout.spectrum, &layout.correction] {
        for decade in [10u32, 100, 1000, 10000] {
            for multiple in 1..=9u32 {
                let hz = decade * multiple;
                if (hz as f32) < LOW_HZ || (hz as f32) > HIGH_HZ {
                    continue;
                }
                let x = layout.x(hz as f32);
                canvas.fill_rect(x, frame.top, 1, frame.plot_h, if multiple == 1 { theme().grid_major } else { theme().grid });
                if matches!(multiple, 1 | 2 | 5) {
                    axis_label(canvas, x, frame.bottom() + 6, hz, theme().dim);
                }
            }
        }
        canvas.rect(frame.left, frame.top, frame.plot_w, frame.plot_h, theme().grid_major);
    }
}

/// Everything the moving part draws; NaN is a missing value.
pub struct Curves<'a> {
    pub centres: &'a [f32],
    pub current: &'a [f32],
    pub mic: &'a [f32],
    pub reference: &'a [f32],
    pub tone: &'a [f32],
    pub correction: &'a [f32],
}

fn trace(canvas: &mut Canvas, layout: &Layout, frame: &Frame, scale: &Scale, centres: &[f32], values: &[f32], colour: u32) {
    let mut previous: Option<(i32, i32)> = None;
    for (hz, &db) in centres.iter().zip(values) {
        if db.is_nan() {
            previous = None;
            continue;
        }
        let (x, y) = (layout.x(*hz), frame.y(scale, db));
        if let Some((px, py)) = previous {
            canvas.line(px, py, x, y, colour);
            canvas.line(px, py + 1, x, y + 1, colour);
        }
        canvas.fill_rect(x - 2, y - 2, 5, 5, colour);
        previous = Some((x, y));
    }
}

fn floor(x: f32) -> f32 {
    let whole = x as i32 as f32;
    if whole > x { whole - 1.0 } else { whole }
}

fn ceil(x: f32) -> f32 {
    -floor(-x)
}

/// The correction plot's scale: wide enough for the curve, in whole tens of dB.
pub fn correction_scale(correction: &[f32]) -> Scale {
    let valid = || correction.iter().copied().filter(|v| !v.is_nan());
    let (Some(high), Some(low)) = (valid().max_by(f32::total_cmp), valid().min_by(f32::total_cmp)) else {
        return Scale { db_top: 10.0, db_range: 20.0 };
    };
    let top = ceil((high + 4.0) / 10.0) * 10.0;
    let range = (ceil((top - low + 4.0) / 10.0) * 10.0).max(20.0);
    Scale { db_top: top, db_range: range }
}

pub fn draw_curves(canvas: &mut Canvas, layout: &Layout, data: &Curves) {
    let frame = &layout.spectrum;
    let [_, live, mic, reference] = theme().curves;
    trace(canvas, layout, frame, &SPECTRUM, data.centres, data.reference, reference);
    trace(canvas, layout, frame, &SPECTRUM, data.centres, data.mic, mic);
    trace(canvas, layout, frame, &SPECTRUM, data.centres, data.current, live);
    for (hz, &db) in data.centres.iter().zip(data.tone) {
        if !db.is_nan() {
            let (x, y) = (layout.x(*hz), frame.y(&SPECTRUM, db));
            canvas.rect(x - 5, y - 5, 11, 11, theme().amber);
        }
    }

    let frame = &layout.correction;
    let scale = correction_scale(data.correction);
    let step = if scale.db_range > 80.0 { 20.0 } else { 10.0 };
    let mut db = floor(scale.db_top / step) * step;
    while db >= scale.db_top - scale.db_range {
        let y = frame.y(&scale, db);
        canvas.fill_rect(frame.left, y, frame.plot_w, 1, theme().grid);
        let mut text = [0u8; 16];
        canvas.text_right(frame.left - 6, y - 8, format_fixed(&mut text, db, 0), theme().dim);
        db -= step;
    }
    trace(canvas, layout, frame, &scale, data.centres, data.correction, theme().green);
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
