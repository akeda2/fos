//! Plotting pieces shared by the tapes that draw a level chart (LTAS, and what comes after): the dark palette, a plot
//! `Frame` with a dB (vertical) scale, the dB grid, a staircase and a polyline through levels, and a Catmull-Rom spline through
//! levels at given x positions. The horizontal axis (log Hz, time, ...) is the tape's own; the frame only says where the
//! plot lies.

use crate::theme::theme;
use crate::{Canvas, format_fixed};

/// Where the plot lies on a `width` x `height` screen: a margin for the axis labels around `plot_w` x `plot_h`.
#[derive(Clone, Copy)]
pub struct Frame {
    pub left: i32,
    pub top: i32,
    pub plot_w: u32,
    pub plot_h: u32,
}

/// The vertical axis: `db_top` at the top edge, `db_top - db_range` at the bottom.
#[derive(Clone, Copy)]
pub struct Scale {
    pub db_top: f32,
    pub db_range: f32,
}

impl Frame {
    pub fn new(width: u32, height: u32, left: i32, top: i32, right: i32, bottom: i32) -> Self {
        Self { left, top, plot_w: width - left as u32 - right as u32, plot_h: height - top as u32 - bottom as u32 }
    }

    /// The pixel row of `db`, clamped to the plot.
    pub fn y(&self, scale: &Scale, db: f32) -> i32 {
        let fraction = (scale.db_top - db) / scale.db_range;
        let clamped = if fraction < 0.0 { 0.0 } else if fraction > 1.0 { 1.0 } else { fraction };
        self.top + (clamped * self.plot_h as f32) as i32
    }

    pub fn bottom(&self) -> i32 {
        self.top + self.plot_h as i32
    }

    pub fn right(&self) -> i32 {
        self.left + self.plot_w as i32
    }
}

/// Clears `canvas` and draws the horizontal dB lines every 10 dB with their labels, the 0 dB line stronger.
pub fn draw_db_grid(canvas: &mut Canvas, frame: &Frame, scale: &Scale) {
    canvas.clear(theme().background);
    let step = 10.0;
    let mut db = (scale.db_top / step) as i32 as f32 * step;
    while db >= scale.db_top - scale.db_range - 0.01 {
        let y = frame.y(scale, db);
        canvas.fill_rect(frame.left, y, frame.plot_w, 1, if db == 0.0 { theme().grid_major } else { theme().grid });
        let mut text = [0u8; 12];
        let shown = format_fixed(&mut text, db, 0);
        canvas.text_right(frame.left - 6, y - 8, shown, theme().dim);
        db -= step;
    }
}

/// A number centred on `x`, with `k` for thousands (`2k`, `500`): axis labels.
pub fn axis_label(canvas: &mut Canvas, x: i32, y: i32, value: u32, colour: u32) {
    let mut text = [0u8; 12];
    let (number, suffix) = if value >= 1000 { (value / 1000, "k") } else { (value, "") };
    let shown = format_fixed(&mut text, number as f32, 0);
    let end = canvas.text(x - (crate::text_width(shown) + crate::text_width(suffix)) as i32 / 2, y, shown, colour);
    canvas.text(end, y, suffix, colour);
}

/// Steps through `(x0, x1, y)` as a staircase (`stairs`, one step per band) or a polyline through the band centres.
pub fn curve(canvas: &mut Canvas, colour: u32, points: impl Iterator<Item = (i32, i32, i32)>, stairs: bool) {
    let mut previous: Option<(i32, i32)> = None;
    for (x0, x1, y) in points {
        if stairs {
            canvas.fill_rect(x0, y, (x1 - x0).max(1) as u32, 2, colour);
            if let Some((_, py)) = previous {
                let (a, b) = if py < y { (py, y) } else { (y, py) };
                canvas.fill_rect(x0, a, 1, (b - a + 1) as u32, colour);
            }
            previous = Some((x1, y));
        } else {
            let x = (x0 + x1) / 2;
            if let Some((px, py)) = previous {
                canvas.line(px, py, x, y, colour);
            }
            previous = Some((x, y));
        }
    }
}

/// Calls `each(x, level)` for every pixel column from the first point to the last that lies in `first..=last`, along a
/// Catmull-Rom spline through `levels`, point `i` at column `x_of(i)` (increasing). The outer neighbours are kept within
/// 12 dB of the segment so that a level at the -200 dB floor does not throw the curve around.
pub fn spline(levels: &[f32], first: i32, last: i32, x_of: impl Fn(usize) -> i32, mut each: impl FnMut(i32, f32)) {
    let count = levels.len();
    if count == 0 {
        return;
    }
    for i in 0..count - 1 {
        let (x0, x1) = (x_of(i), x_of(i + 1));
        if x1 <= x0 {
            continue;
        }
        let (p1, p2) = (levels[i], levels[i + 1]);
        let (low, high) = ((if p1 < p2 { p1 } else { p2 }) - 12.0, (if p1 > p2 { p1 } else { p2 }) + 12.0);
        let keep = |v: f32| if v < low { low } else if v > high { high } else { v };
        let p0 = keep(if i > 0 { levels[i - 1] } else { p1 });
        let p3 = keep(if i + 2 < count { levels[i + 2] } else { p2 });
        for x in x0..x1 {
            let u = (x - x0) as f32 / (x1 - x0) as f32;
            let value = 0.5 * (2.0 * p1 + (p2 - p0) * u + (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3) * u * u + (3.0 * p1 - p0 - 3.0 * p2 + p3) * u * u * u);
            if x >= first && x <= last {
                each(x, value);
            }
        }
    }
    let x = x_of(count - 1);
    if x >= first && x <= last {
        each(x, levels[count - 1]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spline_passes_through_the_levels() {
        let levels = [0.0, -10.0, -4.0];
        let mut seen = [f32::NAN; 21];
        spline(&levels, 0, 20, |i| i as i32 * 10, |x, v| seen[x as usize] = v);
        assert!((seen[0] - 0.0).abs() < 1e-4);
        assert!((seen[10] + 10.0).abs() < 1e-4);
        assert!((seen[20] + 4.0).abs() < 1e-4);
    }

    #[test]
    fn rows_are_clamped() {
        let frame = Frame::new(100, 100, 10, 10, 10, 10);
        let scale = Scale { db_top: 0.0, db_range: 80.0 };
        assert_eq!(frame.y(&scale, 20.0), 10);
        assert_eq!(frame.y(&scale, -200.0), frame.bottom());
        assert_eq!(frame.y(&scale, -40.0), 50);
    }
}
