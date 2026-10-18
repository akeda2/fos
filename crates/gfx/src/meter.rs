//! Level meters for the pixel screen: vertical segmented bars on a dB scale with tick labels (a `Scale` says what the numbers
//! mean: dBFS, or the Nordic PPM scale whose 0 is TEST = -18 dBFS), a peak-hold marker and a clip lamp, and a horizontal
//! bipolar bar (-1..+1) for correlation and balance. The readings come from elsewhere (`dsp::level`, `audio_wire::meter`).

use crate::plot::{DIM, GRID, GRID_MAJOR, TEXT};
use crate::{Canvas, format_fixed};

pub const GREEN: u32 = 0x3ccf6a;
pub const AMBER: u32 = 0xffc83d;
pub const RED: u32 = 0xff4d4d;
const OFF: u32 = 0x1a2026;
const HOLD: u32 = 0xffffff;

/// A vertical scale from `min` to `max` (the scale's own units, dB). `dbfs_at_zero` is the dBFS of the scale's 0 (0 for dBFS,
/// -18 for Nordic); `warn` and `over` are where the bar turns amber and red; `ticks` are the labelled values.
pub struct Scale {
    pub name: &'static str,
    pub min: f32,
    pub max: f32,
    pub dbfs_at_zero: f32,
    pub warn: f32,
    pub over: f32,
    pub ticks: &'static [(f32, &'static str)],
}

impl Scale {
    /// Full scale as a dBFS meter, -60..0.
    pub const DBFS: Scale = Scale {
        name: "dBFS",
        min: -60.0,
        max: 0.0,
        dbfs_at_zero: 0.0,
        warn: -18.0,
        over: -9.0,
        ticks: &[(0.0, "0"), (-3.0, "-3"), (-6.0, "-6"), (-9.0, "-9"), (-12.0, "-12"), (-18.0, "-18"), (-24.0, "-24"), (-30.0, "-30"), (-40.0, "-40"), (-50.0, "-50"), (-60.0, "-60")],
    };

    /// The Nordic PPM scale (IEC 60268-10 type I as used by the Nordic broadcasters): TEST at -18 dBFS, permitted maximum +9, the scale runs to +12 like the other Nordic meters.
    pub const NORDIC: Scale = Scale {
        name: "Nordic",
        min: -42.0,
        max: 12.0,
        dbfs_at_zero: -18.0,
        warn: 0.0,
        over: 9.0,
        ticks: &[(12.0, "+12"), (9.0, "+9"), (6.0, "+6"), (3.0, "+3"), (0.0, "TEST"), (-3.0, "-3"), (-6.0, "-6"), (-9.0, "-9"), (-12.0, "-12"), (-18.0, "-18"), (-24.0, "-24"), (-30.0, "-30"), (-36.0, "-36"), (-42.0, "-42")],
    };

    /// Loudness relative to the target (LU, 0 = the target) on the EBU "+9" scale, -18..+9 (EBU Tech 3341 / R128 s1).
    pub const LU9: Scale = Scale {
        name: "LU",
        min: -18.0,
        max: 9.0,
        dbfs_at_zero: 0.0,
        warn: 1.0,
        over: 9.0,
        ticks: &[(9.0, "+9"), (6.0, "+6"), (3.0, "+3"), (0.0, "0"), (-3.0, "-3"), (-6.0, "-6"), (-9.0, "-9"), (-12.0, "-12"), (-15.0, "-15"), (-18.0, "-18")],
    };

    /// The EBU "+18" scale, -36..+18 LU.
    pub const LU18: Scale = Scale {
        name: "LU",
        min: -36.0,
        max: 18.0,
        dbfs_at_zero: 0.0,
        warn: 1.0,
        over: 9.0,
        ticks: &[(18.0, "+18"), (12.0, "+12"), (6.0, "+6"), (0.0, "0"), (-6.0, "-6"), (-12.0, "-12"), (-18.0, "-18"), (-24.0, "-24"), (-30.0, "-30"), (-36.0, "-36")],
    };

    /// dBFS of a value on this scale.
    pub fn dbfs(&self, value: f32) -> f32 {
        value + self.dbfs_at_zero
    }

    /// The row of `value` in a bar whose top is at `y` and height `h` (clamped).
    pub fn row(&self, value: f32, y: i32, h: u32) -> i32 {
        let f = (self.max - value) / (self.max - self.min);
        y + (if f < 0.0 { 0.0 } else if f > 1.0 { 1.0 } else { f } * h as f32) as i32
    }

    fn colour(&self, value: f32) -> u32 {
        if value > self.over - 0.01 { RED } else if value > self.warn - 0.01 { AMBER } else { GREEN }
    }
}

/// Pixels from the right-hand scale labels to the dBFS column (a "TEST" label and a gap).
pub const DBFS_COLUMN: i32 = 60;

/// The static part, for the grid layer: faint tick lines across `bars_x..bars_x + bars_w`, the scale's labels on both sides of
/// the bars (right-aligned at `bars_x - 6`, left-aligned after them) and, when the scale is not dBFS, the dBFS equivalents in
/// a dimmer column to the right of those (`DBFS_COLUMN` pixels further out).
pub fn draw_scale(canvas: &mut Canvas, scale: &Scale, bars_x: i32, bars_w: u32, y: i32, h: u32) {
    for &(value, label) in scale.ticks {
        let row = scale.row(value, y, h);
        canvas.fill_rect(bars_x, row, bars_w, 1, if value == 0.0 { GRID_MAJOR } else { GRID });
        let colour = if value == 0.0 { TEXT } else { DIM };
        canvas.text_right(bars_x - 6, row - 8, label, colour);
        canvas.text(bars_x + bars_w as i32 + 6, row - 8, label, colour);
        if scale.dbfs_at_zero != 0.0 {
            let mut text = [0u8; 12];
            let shown = format_fixed(&mut text, scale.dbfs(value), 0);
            canvas.text(bars_x + bars_w as i32 + 6 + DBFS_COLUMN, row - 8, shown, DIM);
        }
    }
    canvas.text_right(bars_x - 6, y - 26, scale.name, TEXT);
    canvas.text(bars_x + bars_w as i32 + 6, y - 26, scale.name, TEXT);
    if scale.dbfs_at_zero != 0.0 {
        canvas.text(bars_x + bars_w as i32 + 6 + DBFS_COLUMN, y - 26, "dBFS", DIM);
    }
}

/// One bar: lit in 3-pixel segments up to `value` (the scale's units), a hold marker at `hold` and a red lamp above the bar
/// when `clipped`.
pub fn draw_bar(canvas: &mut Canvas, scale: &Scale, x: i32, w: u32, y: i32, h: u32, value: f32, hold: f32, clipped: bool) {
    canvas.fill_rect(x, y, w, h, OFF);
    let top = scale.row(value, y, h);
    let mut row = y + h as i32 - 3;
    while row >= top {
        let at = scale.max - (row - y) as f32 * (scale.max - scale.min) / h as f32;
        canvas.fill_rect(x, row, w, 2, scale.colour(at));
        row -= 3;
    }
    if hold > scale.min {
        let marker = scale.row(hold, y, h);
        canvas.fill_rect(x, marker.min(y + h as i32 - 2), w, 2, HOLD);
    }
    canvas.fill_rect(x, y - 12, w, 8, if clipped { RED } else { OFF });
}

/// A history chart: `values` (the scale's units, oldest first, one per pixel column; older ones than `w` are dropped) as a line
/// in the box at (x, y, w, h), with the scale's ticks as faint lines and 0 stronger. A value at or below `scale.min` is a gap.
pub fn draw_history(canvas: &mut Canvas, scale: &Scale, x: i32, y: i32, w: u32, h: u32, values: &[f32]) {
    canvas.fill_rect(x, y, w, h, OFF);
    for &(value, _) in scale.ticks {
        canvas.fill_rect(x, scale.row(value, y, h), w, 1, if value == 0.0 { GRID_MAJOR } else { GRID });
    }
    canvas.rect(x, y, w, h, GRID_MAJOR);
    let shown = &values[values.len().saturating_sub(w as usize)..];
    let first = x + (w as usize - shown.len()) as i32;
    let mut previous: Option<(i32, i32)> = None;
    for (index, &value) in shown.iter().enumerate() {
        if value <= scale.min {
            previous = None;
            continue;
        }
        let point = (first + index as i32, scale.row(value, y, h).min(y + h as i32 - 1));
        if let Some((px, py)) = previous {
            canvas.line(px, py, point.0, point.1, GREEN);
        } else {
            canvas.set(point.0, point.1, GREEN);
        }
        previous = Some(point);
    }
}

/// A horizontal bar from the middle: `value` in -1..=1 fills left (negative) or right (positive); ticks at -1, 0, +1.
/// `zones`: `negative_red` colours the negative side red (correlation), otherwise both sides are green (balance).
pub fn draw_bipolar(canvas: &mut Canvas, x: i32, y: i32, w: u32, h: u32, value: f32, negative_red: bool, left_label: &str, right_label: &str) {
    canvas.fill_rect(x, y, w, h, OFF);
    let middle = x + w as i32 / 2;
    let v = if value < -1.0 { -1.0 } else if value > 1.0 { 1.0 } else { value };
    let reach = (v * (w as f32 / 2.0)) as i32;
    if reach >= 0 {
        canvas.fill_rect(middle, y + 2, reach as u32, h - 4, GREEN);
    } else {
        canvas.fill_rect(middle + reach, y + 2, (-reach) as u32, h - 4, if negative_red { RED } else { GREEN });
    }
    for step in 0..=4 {
        let tick = x + (w as i32 - 1) * step / 4;
        canvas.fill_rect(tick, y + h as i32, 1, 6, if step == 2 { TEXT } else { DIM });
    }
    canvas.fill_rect(middle, y - 4, 1, h + 4, TEXT);
    canvas.text_centered(x, y + h as i32 + 8, left_label, DIM);
    canvas.text_centered(middle, y + h as i32 + 8, "0", DIM);
    canvas.text_centered(x + w as i32 - 1, y + h as i32 + 8, right_label, DIM);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_and_scales() {
        assert_eq!(Scale::DBFS.row(0.0, 100, 600), 100);
        assert_eq!(Scale::DBFS.row(-60.0, 100, 600), 700);
        assert_eq!(Scale::DBFS.row(-200.0, 100, 600), 700);
        assert_eq!(Scale::NORDIC.dbfs(0.0), -18.0);
        assert_eq!(Scale::NORDIC.dbfs(9.0), -9.0);
    }

    #[test]
    fn history_draws_and_drops_gaps() {
        let mut pixels = vec![0u32; 120 * 80];
        let mut canvas = Canvas::new(&mut pixels, 120, 80).unwrap();
        draw_history(&mut canvas, &Scale::LU18, 10, 10, 100, 60, &[-200.0, -3.0, 0.0, 5.0, -200.0, 2.0]);
        let green = pixels.iter().filter(|&&p| p == GREEN).count();
        assert!(green >= 4, "{green}");
    }

    #[test]
    fn bars_draw_without_panicking_at_the_extremes() {
        let mut pixels = vec![0u32; 200 * 400];
        let mut canvas = Canvas::new(&mut pixels, 200, 400).unwrap();
        draw_scale(&mut canvas, &Scale::NORDIC, 60, 40, 30, 300);
        draw_bar(&mut canvas, &Scale::NORDIC, 60, 18, 30, 300, 50.0, 50.0, true);
        draw_bar(&mut canvas, &Scale::NORDIC, 82, 18, 30, 300, -90.0, -200.0, false);
        draw_bipolar(&mut canvas, 20, 360, 160, 14, -3.0, true, "-1", "+1");
    }
}
