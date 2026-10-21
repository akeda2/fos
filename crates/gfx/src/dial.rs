//! Pieces of a clock face: a filled disc, the position of one of 60 dots on a ring (0 at the top, clockwise: the seconds of
//! a Wharton-style broadcast clock), and seven-segment digits drawn as rectangles at any size, with the unlit segments
//! faintly visible like a real display.

use crate::Canvas;
use crate::theme::mix;
use dsp::math::{cos, sin};

/// A filled circle (integer midpoint rows).
pub fn fill_disc(canvas: &mut Canvas, cx: i32, cy: i32, radius: i32, rgb: u32) {
    for dy in -radius..=radius {
        let mut dx = radius;
        while dx * dx + dy * dy > radius * radius + radius {
            dx -= 1;
        }
        canvas.fill_rect(cx - dx, cy + dy, (2 * dx + 1) as u32, 1, rgb);
    }
}

/// Where dot `index` (0..60) of a ring of `radius` around (`cx`, `cy`) goes: dot 0 at 12 o'clock, 15 at 3.
pub fn ring_position(index: u32, cx: i32, cy: i32, radius: i32) -> (i32, i32) {
    let angle = (index % 60) as f32 * (core::f32::consts::TAU / 60.0);
    let (s, c) = (sin(angle), cos(angle));
    (cx + (radius as f32 * s + if s >= 0.0 { 0.5 } else { -0.5 }) as i32, cy - (radius as f32 * c + if c >= 0.0 { 0.5 } else { -0.5 }) as i32)
}

/// Segment bits: 1 top, 2 upper right, 4 lower right, 8 bottom, 16 lower left, 32 upper left, 64 middle.
pub const SEGMENTS: [u8; 10] = [0x3f, 0x06, 0x5b, 0x4f, 0x66, 0x6d, 0x7d, 0x07, 0x7f, 0x6f];

/// Colour of an unlit segment next to lit ones of colour `on`. Faint on purpose: an LCD seen from the side lifts its blacks.
pub fn ghost(background: u32, on: u32) -> u32 {
    mix(background, on, 1, 32)
}

/// One digit in a box `w` x `h` at (`x`, `y`) with segments `t` thick; `None` is blank (unlit segments only).
pub fn digit(canvas: &mut Canvas, x: i32, y: i32, w: i32, h: i32, t: i32, value: Option<u8>, on: u32, off: u32) {
    let lit = value.map_or(0, |d| SEGMENTS[d as usize % 10]);
    let gap = (t / 6).max(1);
    let (inner_x, inner_w) = (x + t / 2 + gap, w - t - 2 * gap);
    let middle = y + h / 2;
    let (upper_top, upper_bottom) = (y + t / 2 + gap, middle - gap);
    let (lower_top, lower_bottom) = (middle + gap, y + h - t / 2 - gap);
    let horizontal = [(1u8, y), (64, middle - t / 2), (8, y + h - t)];
    let vertical = [(2u8, x + w - t, upper_top, upper_bottom), (4, x + w - t, lower_top, lower_bottom), (16, x, lower_top, lower_bottom), (32, x, upper_top, upper_bottom)];
    for (bit, top) in horizontal {
        canvas.fill_rect(inner_x, top, inner_w as u32, t as u32, if lit & bit != 0 { on } else { off });
    }
    for (bit, left, top, bottom) in vertical {
        canvas.fill_rect(left, top, t as u32, (bottom - top).max(1) as u32, if lit & bit != 0 { on } else { off });
    }
}

/// The two dots of a colon in a column `w` wide at `x`, spanning a digit box of height `h` from `y`.
pub fn colon(canvas: &mut Canvas, x: i32, y: i32, w: i32, h: i32, rgb: u32) {
    let side = w.max(2);
    canvas.fill_rect(x + (w - side) / 2, y + h / 3 - side / 2, side as u32, side as u32, rgb);
    canvas.fill_rect(x + (w - side) / 2, y + 2 * h / 3 - side / 2, side as u32, side as u32, rgb);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_has_twelve_at_the_top_and_three_on_the_right() {
        assert_eq!(ring_position(0, 100, 100, 50), (100, 50));
        assert_eq!(ring_position(15, 100, 100, 50), (150, 100));
        assert_eq!(ring_position(30, 100, 100, 50), (100, 150));
        assert_eq!(ring_position(45, 100, 100, 50), (50, 100));
    }

    #[test]
    fn digits_light_the_expected_segments() {
        let counts: [u32; 10] = core::array::from_fn(|d| SEGMENTS[d].count_ones());
        assert_eq!(counts, [6, 2, 5, 5, 4, 5, 6, 3, 7, 6]);
    }

    fn lit(value: Option<u8>) -> usize {
        let mut buffer = [0u32; 40 * 60];
        let mut canvas = Canvas::new(&mut buffer, 40, 60).unwrap();
        digit(&mut canvas, 2, 2, 30, 50, 6, value, 1, 0);
        buffer.iter().filter(|&&p| p == 1).count()
    }

    #[test]
    fn a_digit_draws_only_its_segments_and_a_blank_nothing() {
        assert_eq!(lit(None), 0);
        assert!(lit(Some(8)) > lit(Some(1)) * 2);
        assert!(lit(Some(7)) < lit(Some(0)));
    }

    #[test]
    fn a_disc_is_round_and_inside_its_box() {
        let mut buffer = [0u32; 21 * 21];
        let mut canvas = Canvas::new(&mut buffer, 21, 21).unwrap();
        fill_disc(&mut canvas, 10, 10, 8, 1);
        assert_eq!((canvas.get(10, 10), canvas.get(10, 2), canvas.get(2, 10), canvas.get(2, 2)), (1, 1, 1, 0));
        let area = buffer.iter().filter(|&&p| p == 1).count();
        assert!((215..=235).contains(&area), "pi (r + 0.5)^2 = 227, got {area}");
    }
}
