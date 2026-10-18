//! The loudness (LUFS) panel left of the scope: a bar for momentary or short-term loudness on the EBU "+9" or "+18" LU scale
//! (0 = the target), the numbers (M, S, I, LRA) and a history of the short-term loudness. The measurement is `dsp::loudness`.

use crate::layout::Rect;
use core::fmt::Write;
use dsp::loudness::{FLOOR_LUFS, Loudness};
use gfx::meter::{self, Scale};
use gfx::plot::{DIM, TEXT};
use gfx::{Canvas, format_fixed};
use tape_rt::Line;

/// Programme loudness targets: EBU R128, ATSC A/85, streaming.
pub const TARGETS: [f32; 3] = [-23.0, -24.0, -14.0];
/// History points (one per second): about 8 minutes, more than any panel is wide.
pub const HISTORY: usize = 512;
const HISTORY_H: u32 = 110;
const BAR_W: u32 = 26;
const LABELS: i32 = 34;

pub struct Panel {
    pub target: usize,
    pub wide: bool,
    pub short: bool,
    history: [f32; HISTORY],
    filled: usize,
    next: usize,
    steps: u32,
}

impl Panel {
    pub fn new() -> Self {
        Self { target: 0, wide: false, short: false, history: [FLOOR_LUFS; HISTORY], filled: 0, next: 0, steps: 0 }
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

    fn bar_x(area: Rect) -> i32 {
        area.x + LABELS
    }

    fn bar_height(area: Rect) -> u32 {
        area.h.saturating_sub(HISTORY_H + 44)
    }

    /// True when the area is wide enough for the panel.
    pub fn fits(area: Rect) -> bool {
        area.w >= 200 && Self::bar_height(area) >= 60
    }

    /// The static part: bar scale and titles.
    pub fn draw_grid(&self, canvas: &mut Canvas, area: Rect) {
        if !Self::fits(area) {
            return;
        }
        meter::draw_scale(canvas, self.scale(), Self::bar_x(area), BAR_W, area.y, Self::bar_height(area));
        canvas.text(Self::numbers_x(area), area.y - 26, "loudness", DIM);
        canvas.text(area.x, area.y + area.h as i32 - HISTORY_H as i32 - 20, "short-term history", DIM);
    }

    fn numbers_x(area: Rect) -> i32 {
        Self::bar_x(area) + BAR_W as i32 + 6 + LABELS + 6
    }

    pub fn draw(&self, canvas: &mut Canvas, area: Rect, loudness: &Loudness) {
        if !Self::fits(area) {
            return;
        }
        let target = self.target_lufs();
        let scale = self.scale();
        let shown = if self.short { loudness.short_term() } else { loudness.momentary() };
        let to_lu = |lufs: f32| if lufs <= FLOOR_LUFS + 1.0 { -200.0 } else { (lufs - target).max(scale.min + 0.01) };
        let level = to_lu(shown);
        let height = Self::bar_height(area);
        let (x, y) = (Self::bar_x(area), area.y);
        meter::draw_bar(canvas, scale, x, BAR_W, y, height, level, level, false);

        let tx = Self::numbers_x(area);
        let mut row = y;
        let mut line = |canvas: &mut Canvas, label: &str, value: Option<f32>, unit: &str, colour: u32| {
            let mut text = [0u8; 12];
            let number = match value {
                Some(v) if v > FLOOR_LUFS + 1.0 => format_fixed(&mut text, v, 1),
                _ => "--",
            };
            let mut out = Line::<24>::new();
            let _ = write!(out, "{label} {number:>6} {unit}");
            canvas.fill_rect(tx, row, 24 * 8, 16, gfx::plot::BACKGROUND);
            canvas.text(tx, row, out.as_str(), colour);
            row += 24;
        };
        let (m, s) = (loudness.momentary(), loudness.short_term());
        line(canvas, if self.short { "M " } else { "M>" }, Some(m), "LUFS", TEXT);
        line(canvas, if self.short { "S>" } else { "S " }, Some(s), "LUFS", TEXT);
        line(canvas, "I ", loudness.integrated(), "LUFS", TEXT);
        line(canvas, "LRA", loudness.range(), "LU", TEXT);
        let mut out = Line::<24>::new();
        let _ = write!(out, "target {target:.0} LUFS");
        canvas.fill_rect(tx, row + 8, 24 * 8, 16, gfx::plot::BACKGROUND);
        canvas.text(tx, row + 8, out.as_str(), meter::AMBER);
        let mut out = Line::<24>::new();
        let _ = write!(out, "scale +{:.0}/-{:.0} LU", scale.max, -scale.min);
        canvas.fill_rect(tx, row + 32, 24 * 8, 16, gfx::plot::BACKGROUND);
        canvas.text(tx, row + 32, out.as_str(), DIM);

        let mut values = [0.0f32; HISTORY];
        let oldest = (self.next + HISTORY - self.filled) % HISTORY;
        for (index, slot) in values.iter_mut().take(self.filled).enumerate() {
            *slot = to_lu(self.history[(oldest + index) % HISTORY]);
        }
        let box_w = area.w.min(HISTORY as u32);
        meter::draw_history(canvas, scale, area.x, area.y + (area.h - HISTORY_H) as i32, box_w, HISTORY_H, &values[..self.filled]);
    }
}
