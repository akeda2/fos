//! The LTAS display: a log-frequency axis from 20 Hz to 20 kHz, a dB axis, and up to four curves. Drawn into a RAM canvas;
//! the static part (background, grid, labels) is drawn once into a second canvas and copied each frame.

use dsp::math::log10;
use gfx::{Canvas, format_fixed};
use ltas::{Analyzer, Mode, Resolution};

pub const BACKGROUND: u32 = 0x101418;
const GRID: u32 = 0x232a31;
const GRID_MAJOR: u32 = 0x3a444d;
pub const TEXT: u32 = 0xc8d0d8;
pub const DIM: u32 = 0x7d8791;
pub const LIVE: u32 = 0x4aa3ff;
pub const EXP: u32 = 0xffd34a;
pub const LTAS: u32 = 0x4ade80;
pub const PEAK: u32 = 0xff5c5c;
const LTAS_BAR: u32 = 0x1c4a32;
const UNRESOLVED: u32 = 0xd08000;
const LOW_HZ: f32 = 20.0;
const HIGH_HZ: f32 = 20000.0;

/// Curve colours in `Mode::ALL` order: live, exp, ltas, peak.
pub const COLOURS: [u32; 4] = [LIVE, EXP, LTAS, PEAK];

#[derive(Clone, Copy)]
pub struct Layout {
    pub width: u32,
    pub height: u32,
    pub left: i32,
    pub top: i32,
    pub plot_w: u32,
    pub plot_h: u32,
}

#[derive(Clone, Copy)]
pub struct Scale {
    pub db_top: f32,
    pub db_range: f32,
}

impl Layout {
    pub fn new(width: u32, height: u32) -> Self {
        let (left, top, right, bottom) = (56, 44, 16, 44);
        Self { width, height, left, top, plot_w: width - left as u32 - right, plot_h: height - top as u32 - bottom }
    }

    pub fn x(&self, hz: f32) -> i32 {
        let span = log10(HIGH_HZ) - log10(LOW_HZ);
        self.left + ((log10(hz) - log10(LOW_HZ)) / span * self.plot_w as f32) as i32
    }

    pub fn y(&self, scale: &Scale, db: f32) -> i32 {
        let fraction = (scale.db_top - db) / scale.db_range;
        let clamped = if fraction < 0.0 { 0.0 } else if fraction > 1.0 { 1.0 } else { fraction };
        self.top + (clamped * self.plot_h as f32) as i32
    }

    pub fn bottom(&self) -> i32 {
        self.top + self.plot_h as i32
    }
}

fn label(canvas: &mut Canvas, x: i32, y: i32, value: u32, colour: u32) {
    let mut text = [0u8; 12];
    let (number, suffix) = if value >= 1000 { (value / 1000, "k") } else { (value, "") };
    let shown = format_fixed(&mut text, number as f32, 0);
    let end = canvas.text(x - (gfx::text_width(shown) + gfx::text_width(suffix)) as i32 / 2, y, shown, colour);
    canvas.text(end, y, suffix, colour);
}

pub fn draw_grid(canvas: &mut Canvas, layout: &Layout, scale: &Scale) {
    canvas.clear(BACKGROUND);
    let step = 10.0;
    let mut db = (scale.db_top / step) as i32 as f32 * step;
    while db >= scale.db_top - scale.db_range - 0.01 {
        let y = layout.y(scale, db);
        canvas.fill_rect(layout.left, y, layout.plot_w, 1, if db == 0.0 { GRID_MAJOR } else { GRID });
        let mut text = [0u8; 12];
        let shown = format_fixed(&mut text, db, 0);
        canvas.text_right(layout.left - 6, y - 8, shown, DIM);
        db -= step;
    }
    for decade in [10u32, 100, 1000, 10000] {
        for multiple in 1..=9u32 {
            let hz = decade * multiple;
            if (hz as f32) < LOW_HZ || (hz as f32) > HIGH_HZ {
                continue;
            }
            let x = layout.x(hz as f32);
            canvas.fill_rect(x, layout.top, 1, layout.plot_h, if multiple == 1 { GRID_MAJOR } else { GRID });
            if matches!(multiple, 1 | 2 | 5) {
                label(canvas, x, layout.bottom() + 8, hz, DIM);
            }
        }
    }
    canvas.rect(layout.left, layout.top, layout.plot_w, layout.plot_h, GRID_MAJOR);
    canvas.text(6, layout.bottom() + 8, "Hz", DIM);
    canvas.text(6, layout.top - 8, "dB", DIM);
}

/// Steps through `points` (x, y) as a staircase (band mode) or a polyline (FFT mode).
fn curve(canvas: &mut Canvas, colour: u32, points: impl Iterator<Item = (i32, i32, i32)>, stairs: bool) {
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

/// Calls `each(x, db)` for every pixel column from the first band centre to the last, along a Catmull-Rom spline through the
/// band levels (equally spaced on the log axis). The outer neighbours are kept within 12 dB of the segment so that a band at
/// the -200 dB floor does not throw the curve around.
fn smooth<const N: usize>(analyzer: &Analyzer<N>, layout: &Layout, levels: &[f32], mut each: impl FnMut(i32, f32)) {
    let count = analyzer.outputs();
    if count == 0 {
        return;
    }
    let (first, last) = (layout.left, layout.left + layout.plot_w as i32);
    let at = |i: usize| layout.x(analyzer.centre_hz(i));
    for i in 0..count.saturating_sub(1) {
        let (x0, x1) = (at(i), at(i + 1));
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
    let x = at(count - 1);
    if x >= first && x <= last {
        each(x, levels[count - 1]);
    }
}

/// Draws the shown curves (indexed as `Mode::ALL`) of `analyzer` over the grid already in `canvas`. In a band mode `smooth`
/// draws a spline through the band levels instead of the bars (FFT mode is always a polyline).
pub fn draw_curves<const N: usize>(canvas: &mut Canvas, layout: &Layout, scale: &Scale, analyzer: &mut Analyzer<N>, shown: [bool; 4], levels: &mut [f32], smooth_curves: bool) {
    let outputs = analyzer.outputs();
    let stairs = analyzer.config().resolution != Resolution::Fft;
    let edge = |analyzer: &Analyzer<N>, i: usize| {
        let (lo, hi) = analyzer.edges_hz(i);
        let lo = if lo < LOW_HZ { LOW_HZ } else { lo };
        (layout.x(lo), layout.x(hi))
    };
    if stairs {
        for i in 0..outputs {
            if !analyzer.band_is_resolved(i) {
                let (x0, x1) = edge(analyzer, i);
                canvas.fill_rect(x0, layout.bottom() + 1, (x1 - x0).max(1) as u32, 2, UNRESOLVED);
            }
        }
    }
    if shown[2] && stairs && smooth_curves {
        analyzer.levels_db(Mode::Linear, levels);
        smooth(analyzer, layout, levels, |x, db| {
            let y = layout.y(scale, db);
            canvas.fill_rect(x, y, 1, (layout.bottom() - y).max(0) as u32, LTAS_BAR);
        });
    } else if shown[2] && stairs {
        analyzer.levels_db(Mode::Linear, levels);
        for i in 0..outputs {
            let (x0, x1) = edge(analyzer, i);
            let y = layout.y(scale, levels[i]);
            canvas.fill_rect(x0 + 1, y, (x1 - x0 - 1).max(1) as u32, (layout.bottom() - y).max(0) as u32, LTAS_BAR);
        }
    }
    for (index, mode) in Mode::ALL.into_iter().enumerate() {
        if !shown[index] {
            continue;
        }
        analyzer.levels_db(mode, levels);
        let analyzer_ref: &Analyzer<N> = analyzer;
        if stairs && smooth_curves {
            let mut previous: Option<(i32, i32)> = None;
            smooth(analyzer_ref, layout, levels, |x, db| {
                let y = layout.y(scale, db);
                if let Some((px, py)) = previous {
                    canvas.line(px, py, x, y, COLOURS[index]);
                    canvas.line(px, py + 1, x, y + 1, COLOURS[index]);
                }
                previous = Some((x, y));
            });
            continue;
        }
        let points = (0..outputs).map(|i| {
            let (x0, x1) = edge(analyzer_ref, i);
            (x0, x1, layout.y(scale, levels[i]))
        });
        curve(canvas, COLOURS[index], points, stairs);
    }
}
