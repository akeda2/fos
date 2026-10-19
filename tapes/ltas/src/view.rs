//! The LTAS display: a log-frequency axis from 20 Hz to 20 kHz, a dB axis, and up to four curves. Drawn into a RAM canvas;
//! the static part (background, grid, labels) is drawn once into a second canvas and copied each frame.

use dsp::math::log10;
use gfx::Canvas;
use ltas::{Analyzer, Mode, Resolution};

pub use gfx::plot::Scale;
use gfx::plot::{Frame, axis_label, curve, spline};
use gfx::theme::theme;
const LOW_HZ: f32 = 20.0;
const HIGH_HZ: f32 = 20000.0;

/// The plot frame on the screen plus the log-frequency mapping; the frame's fields and `y` are reached through `Deref`.
#[derive(Clone, Copy)]
pub struct Layout {
    pub height: u32,
    frame: Frame,
}

impl core::ops::Deref for Layout {
    type Target = Frame;
    fn deref(&self) -> &Frame {
        &self.frame
    }
}

impl Layout {
    pub fn new(width: u32, height: u32) -> Self {
        Self { height, frame: Frame::new(width, height, 56, 44, 16, 44) }
    }

    pub fn x(&self, hz: f32) -> i32 {
        let span = log10(HIGH_HZ) - log10(LOW_HZ);
        self.left + ((log10(hz) - log10(LOW_HZ)) / span * self.plot_w as f32) as i32
    }
}

pub fn draw_grid(canvas: &mut Canvas, layout: &Layout, scale: &Scale) {
    gfx::plot::draw_db_grid(canvas, layout, scale);
    for decade in [10u32, 100, 1000, 10000] {
        for multiple in 1..=9u32 {
            let hz = decade * multiple;
            if (hz as f32) < LOW_HZ || (hz as f32) > HIGH_HZ {
                continue;
            }
            let x = layout.x(hz as f32);
            canvas.fill_rect(x, layout.top, 1, layout.plot_h, if multiple == 1 { theme().grid_major } else { theme().grid });
            if matches!(multiple, 1 | 2 | 5) {
                axis_label(canvas, x, layout.bottom() + 8, hz, theme().dim);
            }
        }
    }
    canvas.rect(layout.left, layout.top, layout.plot_w, layout.plot_h, theme().grid_major);
    canvas.text(6, layout.bottom() + 8, "Hz", theme().dim);
    canvas.text(6, layout.top - 8, "dB", theme().dim);
}

/// The spline through the band levels, bands equally spaced on the log axis; `each(x, db)` for every pixel column.
fn smooth_bands<const N: usize>(analyzer: &Analyzer<N>, layout: &Layout, levels: &[f32], each: impl FnMut(i32, f32)) {
    let count = analyzer.outputs();
    spline(&levels[..count], layout.left, layout.right(), |i| layout.x(analyzer.centre_hz(i)), each);
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
                canvas.fill_rect(x0, layout.bottom() + 1, (x1 - x0).max(1) as u32, 2, theme().unresolved);
            }
        }
    }
    if shown[2] && stairs && smooth_curves {
        analyzer.levels_db(Mode::Linear, levels);
        smooth_bands(analyzer, layout, levels, |x, db| {
            let y = layout.y(scale, db);
            canvas.fill_rect(x, y, 1, (layout.bottom() - y).max(0) as u32, theme().bar_fill);
        });
    } else if shown[2] && stairs {
        analyzer.levels_db(Mode::Linear, levels);
        for i in 0..outputs {
            let (x0, x1) = edge(analyzer, i);
            let y = layout.y(scale, levels[i]);
            canvas.fill_rect(x0 + 1, y, (x1 - x0 - 1).max(1) as u32, (layout.bottom() - y).max(0) as u32, theme().bar_fill);
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
            smooth_bands(analyzer_ref, layout, levels, |x, db| {
                let y = layout.y(scale, db);
                if let Some((px, py)) = previous {
                    canvas.line(px, py, x, y, theme().curves[index]);
                    canvas.line(px, py + 1, x, y + 1, theme().curves[index]);
                }
                previous = Some((x, y));
            });
            continue;
        }
        let points = (0..outputs).map(|i| {
            let (x0, x1) = edge(analyzer_ref, i);
            (x0, x1, layout.y(scale, levels[i]))
        });
        curve(canvas, theme().curves[index], points, stairs);
    }
}
