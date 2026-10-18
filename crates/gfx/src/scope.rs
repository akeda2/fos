//! A goniometer (phase scope) for stereo: a persistent "phosphor" trace and its graticule. A sample pair (l, r) is plotted at
//! x = (r - l) / 2, y = (l + r) / 2 (full scale = the box edge): a mono signal is a vertical line, left only a line up to the
//! left, right only up to the right, opposite polarity a horizontal line, uncorrelated signals a ball.
//! `Phosphor` keeps one intensity byte per pixel of a square area, caller-owned; `trace` adds hits, `decay` fades them once a
//! display frame, `draw` paints them over the canvas.

use crate::Canvas;
use dsp::math::{log10, sqrt};

/// How the distance from the centre maps to the picture. The angle of a point is never changed.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Law {
    /// Distance proportional to amplitude: a -18 dBFS signal reaches 1/8 of the way to the edge.
    Linear,
    /// Distance proportional to dB over `LOG_RANGE_DB` (the edge is full scale): quiet signals stay visible.
    Log,
}

impl Law {
    pub fn next(self) -> Law {
        match self {
            Law::Linear => Law::Log,
            Law::Log => Law::Linear,
        }
    }
}

/// The default dB span from the edge to the centre in `Law::Log` (`Phosphor::range_db`).
pub const LOG_RANGE_DB: f32 = 40.0;

pub struct Phosphor<'a> {
    cells: &'a mut [u8],
    size: usize,
    pub law: Law,
    /// dB from the edge to the centre in `Law::Log`.
    pub range_db: f32,
}

impl<'a> Phosphor<'a> {
    /// `cells` must hold `size * size` bytes.
    pub fn new(cells: &'a mut [u8], size: usize) -> Option<Self> {
        (cells.len() >= size * size && size >= 2).then_some(Self { cells, size, law: Law::Linear, range_db: LOG_RANGE_DB })
    }

    pub fn size(&self) -> usize {
        self.size
    }

    pub fn clear(&mut self) {
        self.cells[..self.size * self.size].fill(0);
    }

    fn position(&self, l: f32, r: f32, gain: f32) -> Option<(usize, usize)> {
        let half = (self.size - 1) as f32 / 2.0;
        let (mut u, mut v) = ((r - l) * 0.5 * gain, (l + r) * 0.5 * gain);
        if self.law == Law::Log {
            // distance d (1.0 = edge) becomes 1 + dB(d) / range, clamped at the centre; the direction stays
            let d2 = u * u + v * v;
            if d2 > 1e-12 {
                let shown = (1.0 + 10.0 * log10(d2) / self.range_db).max(0.0);
                let scale = shown / sqrt(d2);
                (u, v) = (u * scale, v * scale);
            }
        }
        let x = half + u * half;
        let y = half - v * half;
        (x >= 0.0 && y >= 0.0 && x < self.size as f32 && y < self.size as f32).then_some((x as usize, y as usize))
    }

    fn hit(&mut self, x: usize, y: usize, amount: u8) {
        let cell = &mut self.cells[y * self.size + x];
        *cell = cell.saturating_add(amount);
    }

    /// Plots the pairs. `gain` scales the picture (1.0 = full scale touches the edge); `lines` joins consecutive points.
    pub fn trace(&mut self, left: &[f32], right: &[f32], gain: f32, lines: bool, amount: u8) {
        let mut previous: Option<(usize, usize)> = None;
        for (&l, &r) in left.iter().zip(right) {
            let point = self.position(l, r, gain);
            if let Some((x, y)) = point {
                match (lines, previous) {
                    (true, Some((px, py))) => self.line(px, py, x, y, amount),
                    _ => self.hit(x, y, amount),
                }
            }
            previous = point;
        }
    }

    fn line(&mut self, x0: usize, y0: usize, x1: usize, y1: usize, amount: u8) {
        let (mut x, mut y) = (x0 as i32, y0 as i32);
        let (dx, dy) = ((x1 as i32 - x).abs(), -(y1 as i32 - y).abs());
        let (sx, sy) = (if x < x1 as i32 { 1 } else { -1 }, if y < y1 as i32 { 1 } else { -1 });
        let mut error = dx + dy;
        loop {
            self.hit(x as usize, y as usize, amount);
            if x == x1 as i32 && y == y1 as i32 {
                break;
            }
            let doubled = 2 * error;
            if doubled >= dy {
                error += dy;
                x += sx;
            }
            if doubled <= dx {
                error += dx;
                y += sy;
            }
        }
    }

    /// Fades every cell to `keep`/256 of itself (at least one step), once per display frame.
    pub fn decay(&mut self, keep: u32) {
        for cell in &mut self.cells[..self.size * self.size] {
            let value = *cell as u32;
            if value > 0 {
                let next = value * keep >> 8;
                *cell = if next >= value { (value - 1) as u8 } else { next as u8 };
            }
        }
    }

    /// Paints the lit cells over `canvas` with the top-left at (x, y): green that brightens towards white with intensity.
    pub fn draw(&self, canvas: &mut Canvas, x: i32, y: i32) {
        for row in 0..self.size {
            for column in 0..self.size {
                let v = self.cells[row * self.size + column] as u32;
                if v > 0 {
                    canvas.set(x + column as i32, y + row as i32, ramp(v));
                }
            }
        }
    }
}

/// Dark green -> green -> white-green for intensity 1..=255.
fn ramp(v: u32) -> u32 {
    let green = 70 + v * 185 / 255;
    let other = if v > 140 { (v - 140) * 255 / 115 } else { 0 };
    (other << 16) | (green << 8) | (other + v / 8).min(255)
}

pub const AXIS: u32 = 0x2c363f;
pub const FRAME: u32 = 0x4b5964;
pub const LABEL: u32 = 0x9aa5af;

/// The graticule of a scope of `size` pixels with its top-left at (x, y), matching `Phosphor::draw`: the box, the 45-degree
/// L and R axes and the M (vertical) and S (horizontal) axes, labelled. `canvas` should be the grid layer.
pub fn draw_graticule(canvas: &mut Canvas, x: i32, y: i32, size: u32) {
    let (s, c) = (size as i32 - 1, (size as i32 - 1) / 2);
    canvas.line(x, y + c, x + s, y + c, AXIS);
    canvas.line(x + c, y, x + c, y + s, AXIS);
    canvas.line(x, y, x + s, y + s, AXIS);
    canvas.line(x + s, y, x, y + s, AXIS);
    // a diamond through the edge midpoints: the full-scale mono and opposite-polarity extremes
    for (a, b) in [((c, 0), (s, c)), ((s, c), (c, s)), ((c, s), (0, c)), ((0, c), (c, 0))] {
        canvas.line(x + a.0, y + a.1, x + b.0, y + b.1, AXIS);
    }
    canvas.rect(x, y, size, size, FRAME);
    canvas.text_centered(x + c, y - 18, "+M", LABEL);
    canvas.text_centered(x + c, y + s + 4, "-M", LABEL);
    canvas.text(x - 18, y + c - 8, "-S", LABEL);
    canvas.text(x + s + 4, y + c - 8, "+S", LABEL);
    canvas.text(x + 4, y + 4, "L", LABEL);
    canvas.text(x + s - 12, y + 4, "R", LABEL);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lit(p: &Phosphor) -> Vec<(usize, usize)> {
        (0..p.size * p.size).filter(|&i| p.cells[i] > 0).map(|i| (i % p.size, i / p.size)).collect()
    }

    #[test]
    fn log_law_lifts_quiet_signals_and_keeps_full_scale_and_direction() {
        let mut cells = vec![0u8; 201 * 201];
        let mut p = Phosphor::new(&mut cells, 201).unwrap();
        p.law = Law::Log;
        p.trace(&[0.1259], &[0.1259], 1.0, false, 100); // -18 dBFS mono
        let (x, y) = lit(&p)[0];
        assert_eq!(x, 100);
        // linear: 12.6 % of the radius; log: 1 - 18/40 = 55 % of it
        assert!((100 - y as i32 - 55).abs() <= 2, "{y}");
        p.clear();
        p.trace(&[0.999], &[0.999], 1.0, false, 100);
        assert!(lit(&p)[0].1 <= 1);
    }

    #[test]
    fn log_range_is_adjustable() {
        let mut cells = vec![0u8; 201 * 201];
        let mut p = Phosphor::new(&mut cells, 201).unwrap();
        p.law = Law::Log;
        p.range_db = 60.0;
        p.trace(&[0.1259], &[0.1259], 1.0, false, 100); // -18 dBFS: 1 - 18/60 = 70 %
        assert!((100 - lit(&p)[0].1 as i32 - 70).abs() <= 2);
    }

    #[test]
    fn mono_is_vertical_and_inverted_horizontal() {
        let mut cells = vec![0u8; 101 * 101];
        let mut p = Phosphor::new(&mut cells, 101).unwrap();
        p.trace(&[0.5, -0.5], &[0.5, -0.5], 1.0, false, 100);
        assert!(lit(&p).iter().all(|&(x, _)| x == 50));
        p.clear();
        p.trace(&[0.5], &[-0.5], 1.0, false, 100);
        assert_eq!(lit(&p), vec![(25, 50)]);
        p.clear();
        p.trace(&[1.0], &[0.0], 1.0, false, 100);
        let (x, y) = lit(&p)[0];
        assert!(x < 50 && y < 50);
    }

    #[test]
    fn decay_reaches_zero_and_lines_join() {
        let mut cells = vec![0u8; 11 * 11];
        let mut p = Phosphor::new(&mut cells, 11).unwrap();
        p.trace(&[0.0, 1.0], &[0.0, 1.0], 1.0, true, 255);
        assert!(lit(&p).len() >= 5);
        for _ in 0..300 {
            p.decay(200);
        }
        assert!(lit(&p).is_empty());
    }

    #[test]
    fn out_of_range_is_dropped() {
        let mut cells = vec![0u8; 11 * 11];
        let mut p = Phosphor::new(&mut cells, 11).unwrap();
        p.trace(&[1.0], &[1.0], 4.0, false, 255);
        assert!(lit(&p).is_empty());
    }
}
