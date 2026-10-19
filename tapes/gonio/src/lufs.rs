//! The loudness (LUFS) panel left of the scope: the numbers (M, S, I, LRA) on the outer left, a bar each for momentary and
//! short-term loudness on the EBU "+9" or "+18" LU scale (0 = the target) against the scope, and a history of the short-term
//! loudness along the bottom, level with the correlation and balance bars. Narrow areas drop the units, then the bars. The measurement is `dsp::loudness`.

use crate::layout::Rect;
use core::fmt::Write;
use dsp::loudness::{FLOOR_LUFS, Loudness};
use gfx::meter::{self, Scale};
use gfx::theme::theme;
use gfx::{Canvas, format_fixed};
use tape_rt::Line;

/// Programme loudness targets: EBU R128, ATSC A/85, streaming.
pub const TARGETS: [f32; 3] = [-23.0, -24.0, -14.0];
/// History points (one per second): about 8 minutes, more than any panel is wide.
pub const HISTORY: usize = 512;
/// The history sits in the rows beside the correlation and balance bars (`layout::UNDER` less its label row).
const HISTORY_H: u32 = 78;
/// Room above it for its title.
const HISTORY_GAP: u32 = 24;
const BAR_W: u32 = 26;
const BARS_W: u32 = 2 * BAR_W + 10;
const LABELS: i32 = 34;

struct Geometry {
    bars: bool,
    units: bool,
    history: bool,
    bars_x: i32,
    bars_h: u32,
    /// Left edge of the numbers: centred between the area's edge and the bars' scale labels.
    numbers_x: i32,
}

pub struct Panel {
    pub target: usize,
    pub wide: bool,
    history: [f32; HISTORY],
    filled: usize,
    next: usize,
    steps: u32,
}

impl Panel {
    pub fn new() -> Self {
        Self { target: 0, wide: false, history: [FLOOR_LUFS; HISTORY], filled: 0, next: 0, steps: 0 }
    }

    pub fn target_lufs(&self) -> f32 {
        TARGETS[self.target]
    }

    pub fn scale(&self) -> &'static Scale {
        if self.wide { &Scale::LU18 } else { &Scale::LU9 }
    }

    pub fn clear_history(&mut self) {
        self.filled = 0;
        self.next = 0;
        self.steps = 0;
    }

    /// Call after `Loudness::feed`: adds the short-term loudness to the history once per second (ten 100 ms steps).
    pub fn sample(&mut self, loudness: &mut Loudness) {
        while loudness.take_step() {
            self.steps += 1;
            if self.steps >= 10 {
                self.steps = 0;
                self.history[self.next] = loudness.short_term();
                self.next = (self.next + 1) % HISTORY;
                self.filled = (self.filled + 1).min(HISTORY);
            }
        }
    }

    /// What fits in `area`, widest first: numbers with units + bars, numbers without units + bars, numbers alone.
    fn geometry(area: Rect) -> Option<Geometry> {
        let bars_block = (2 * LABELS) as u32 + BARS_W;
        let history = area.h >= HISTORY_H + HISTORY_GAP + 60 + 24;
        let rest = if history { HISTORY_H + HISTORY_GAP } else { 0 };
        let bars_h = area.h.saturating_sub(rest);
        let (units, bars) = if area.w >= bars_block + 8 + 120 {
            (true, true)
        } else if area.w >= bars_block + 8 + 72 {
            (false, true)
        } else if area.w >= 72 {
            (false, false)
        } else {
            return None;
        };
        if bars && bars_h < 60 {
            return None;
        }
        // The bars sit against the scope; their scale labels stay inside the area.
        let bars_x = area.x + area.w as i32 - LABELS - BARS_W as i32;
        let numbers_w = if units { 15 * 8 } else { 10 * 8 };
        let free = if bars { bars_x - LABELS - area.x } else { area.w as i32 };
        let numbers_x = area.x + ((free - numbers_w) / 2).max(0);
        Some(Geometry { bars, units, history, bars_x, bars_h, numbers_x })
    }

    /// True when the area is big enough for anything.
    pub fn fits(area: Rect) -> bool {
        Self::geometry(area).is_some()
    }

    /// The static part: bar scale and titles.
    pub fn draw_grid(&self, canvas: &mut Canvas, area: Rect) {
        let Some(g) = Self::geometry(area) else { return };
        if g.bars {
            meter::draw_scale(canvas, self.scale(), g.bars_x, BARS_W, area.y, g.bars_h);
            canvas.text(g.bars_x, area.y - 26, "M", theme().dim);
            canvas.text(g.bars_x + BAR_W as i32 + 10, area.y - 26, "S", theme().dim);
        }
        canvas.text(g.numbers_x, area.y - 26, "loudness", theme().dim);
        if g.history {
            canvas.text(area.x, area.y + (area.h - HISTORY_H) as i32 - 20, "short-term history", theme().dim);
        }
    }

    pub fn draw(&self, canvas: &mut Canvas, area: Rect, loudness: &Loudness) {
        let Some(g) = Self::geometry(area) else { return };
        let target = self.target_lufs();
        let scale = self.scale();
        let to_lu = |lufs: f32| if lufs <= FLOOR_LUFS + 1.0 { -200.0 } else { (lufs - target).max(scale.min + 0.01) };
        let y = area.y;
        if g.bars {
            for (index, lufs) in [loudness.momentary(), loudness.short_term()].into_iter().enumerate() {
                let level = to_lu(lufs);
                let x = g.bars_x + index as i32 * (BAR_W as i32 + 10);
                meter::draw_bar(canvas, scale, x, BAR_W, y, g.bars_h, level, level, false);
            }
        }

        let tx = g.numbers_x;
        let units = g.units;
        let mut row = y;
        let mut line = |canvas: &mut Canvas, label: &str, value: Option<f32>, unit: &str, colour: u32| {
            let mut text = [0u8; 12];
            let number = match value {
                Some(v) if v > FLOOR_LUFS + 1.0 => format_fixed(&mut text, v, 1),
                _ => "--",
            };
            let mut out = Line::<24>::new();
            let _ = write!(out, "{label:<3} {number:>6}");
            if units {
                let _ = write!(out, " {unit}");
            }
            canvas.fill_rect(tx, row, 15 * 8, 16, theme().background);
            canvas.text(tx, row, out.as_str(), colour);
            row += 24;
        };
        let (m, s) = (loudness.momentary(), loudness.short_term());
        line(canvas, "M ", Some(m), "LUFS", theme().text);
        line(canvas, "S ", Some(s), "LUFS", theme().text);
        line(canvas, "I ", loudness.integrated(), "LUFS", theme().text);
        line(canvas, "LRA", loudness.range(), "LU", theme().text);
        if g.bars_h >= 160 {
            let mut out = Line::<24>::new();
            let _ = write!(out, "target {target:.0} LUFS");
            canvas.fill_rect(tx, row + 8, 15 * 8, 16, theme().background);
            canvas.text(tx, row + 8, out.as_str(), theme().amber);
            let mut out = Line::<24>::new();
            let _ = write!(out, "scale +{:.0}/-{:.0} LU", scale.max, -scale.min);
            canvas.fill_rect(tx, row + 32, 15 * 8, 16, theme().background);
            canvas.text(tx, row + 32, out.as_str(), theme().dim);
        }

        if !g.history {
            return;
        }
        let mut values = [0.0f32; HISTORY];
        let oldest = (self.next + HISTORY - self.filled) % HISTORY;
        for (index, slot) in values.iter_mut().take(self.filled).enumerate() {
            *slot = to_lu(self.history[(oldest + index) % HISTORY]);
        }
        let box_w = area.w.min(HISTORY as u32);
        let top = area.y + (area.h - HISTORY_H) as i32;
        meter::draw_history(canvas, scale, area.x, top, box_w, HISTORY_H, &values[..self.filled]);
    }
}
