//! The echo tape's screen logic without a screen: where every control is (`Layout`), what a touch or a MIDI controller on it
//! means (`Hit`, `press`, `drag`, `midi`) and what it currently shows. `main.rs` draws and runs it; the geometry and the
//! mapping are testable on the host.
#![cfg_attr(not(test), no_std)]

use instrument_rt::Rect;
use tape_echo::{HEAD1, HEAD2, HEAD3, TapTempo, TapeEcho};

pub const GLYPH_H: i32 = gfx::GLYPH_HEIGHT as i32;
const MAX_WIDGETS: usize = 24;
pub const SLIDERS: [&str; 12] =
    ["Repeat", "Intensity", "Echo", "Dry", "Input", "Reverb", "Decay", "Bass", "Treble", "Wow", "Saturate", "Spread"];
pub const HEADS: [(&str, u8); 3] = [("Head 1", HEAD1), ("Head 2", HEAD2), ("Head 3", HEAD3)];
/// MIDI controller of the first slider; the twelve sliders follow on consecutive controllers (20-31, undefined by MIDI).
pub const CC_FIRST_SLIDER: u8 = 20;
/// Held (value 64 or more) freezes the tape, like a sustain pedal.
pub const CC_FREEZE: u8 = 64;
/// A value of 64 or more is one tap.
pub const CC_TAP: u8 = 65;
/// The head switches: a value of 64 or more flips the head.
pub const CC_HEADS: [u8; 3] = [102, 103, 104];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    /// Index into `SLIDERS`.
    Slider(u8),
    /// Index into `HEADS`.
    Head(u8),
    Freeze,
    Tap,
}

#[derive(Clone, Copy)]
pub struct Widget {
    pub rect: Rect,
    pub hit: Hit,
}

pub struct Layout {
    pub width: i32,
    pub height: i32,
    pub scale: i32,
    pub margin: i32,
    pub header: Rect,
    pub panel: Rect,
    widgets: [Widget; MAX_WIDGETS],
    widget_count: usize,
}

impl Layout {
    /// Two columns of six sliders on the left, the head switches, Freeze and Tap stacked on the right.
    pub fn new(width: i32, height: i32) -> Self {
        let scale = if width >= 1200 { 2 } else { 1 };
        let margin = 6 * scale;
        let header_h = GLYPH_H * scale * 2 + margin * 2;
        let top = header_h + margin;
        let panel = Rect { x: margin, y: top, w: width - 2 * margin, h: height - top - margin };
        let mut layout = Layout {
            width,
            height,
            scale,
            margin,
            header: Rect { x: 0, y: 0, w: width, h: header_h },
            panel,
            widgets: [Widget { rect: Rect::ZERO, hit: Hit::Freeze }; MAX_WIDGETS],
            widget_count: 0,
        };
        let sliders_w = (panel.w - margin) * 70 / 100;
        let rows = (SLIDERS.len() / 2) as i32;
        let row_h = (panel.h - margin) / rows;
        let column_w = sliders_w / 2;
        for (index, _) in SLIDERS.iter().enumerate() {
            let (column, row) = ((index / 6) as i32, (index % 6) as i32);
            let rect = Rect {
                x: panel.x + margin + column * column_w,
                y: panel.y + margin / 2 + row * row_h + margin / 2,
                w: column_w - margin,
                h: row_h - margin,
            };
            layout.add(rect, Hit::Slider(index as u8));
        }
        let buttons_x = panel.x + margin + sliders_w + margin;
        let buttons_w = panel.w - sliders_w - 3 * margin;
        let button_rows = 5;
        let button_h = (panel.h - margin) / button_rows;
        let hits = [Hit::Head(0), Hit::Head(1), Hit::Head(2), Hit::Freeze, Hit::Tap];
        for (row, hit) in hits.into_iter().enumerate() {
            let rect = Rect {
                x: buttons_x,
                y: panel.y + margin / 2 + row as i32 * button_h + margin / 2,
                w: buttons_w,
                h: button_h - margin,
            };
            layout.add(rect, hit);
        }
        layout
    }

    fn add(&mut self, rect: Rect, hit: Hit) {
        self.widgets[self.widget_count] = Widget { rect, hit };
        self.widget_count += 1;
    }

    pub fn widgets(&self) -> &[Widget] {
        &self.widgets[..self.widget_count]
    }

    pub fn widget_rect(&self, hit: Hit) -> Option<Rect> {
        self.widgets().iter().find(|widget| widget.hit == hit).map(|widget| widget.rect)
    }

    pub fn hit(&self, x: i32, y: i32) -> Option<Hit> {
        self.widgets().iter().find(|widget| widget.rect.contains(x, y)).map(|widget| widget.hit)
    }

    /// A slider's value, 0..=1, for a finger at `x`.
    pub fn slider_value_at(&self, hit: Hit, x: i32) -> f32 {
        let Some(rect) = self.widget_rect(hit) else { return 0.0 };
        ((x - rect.x) as f32 / rect.w.max(1) as f32).clamp(0.0, 1.0)
    }
}

/// The value a slider shows.
pub fn slider_value(echo: &TapeEcho, index: u8) -> f32 {
    match index {
        0 => echo.repeat_rate(),
        1 => echo.intensity(),
        2 => echo.echo_level(),
        3 => echo.dry(),
        4 => echo.input(),
        5 => echo.reverb(),
        6 => echo.reverb_decay(),
        7 => echo.bass(),
        8 => echo.treble(),
        9 => echo.wow(),
        10 => echo.saturation(),
        _ => echo.spread(),
    }
}

pub fn set_slider(echo: &mut TapeEcho, index: u8, value: f32) {
    match index {
        0 => echo.set_repeat_rate(value),
        1 => echo.set_intensity(value),
        2 => echo.set_echo_level(value),
        3 => echo.set_dry(value),
        4 => echo.set_input(value),
        5 => echo.set_reverb(value),
        6 => echo.set_reverb_decay(value),
        7 => echo.set_bass(value),
        8 => echo.set_treble(value),
        9 => echo.set_wow(value),
        10 => echo.set_saturation(value),
        _ => echo.set_spread(value),
    }
}

pub fn lit(echo: &TapeEcho, hit: Hit) -> bool {
    match hit {
        Hit::Head(index) => echo.heads() & HEADS[index as usize].1 != 0,
        Hit::Freeze => echo.frozen(),
        _ => false,
    }
}

pub fn label(hit: Hit) -> &'static str {
    match hit {
        Hit::Head(index) => HEADS[index as usize].0,
        Hit::Freeze => "Freeze",
        Hit::Tap => "Tap",
        Hit::Slider(index) => SLIDERS[index as usize],
    }
}

/// One tap: the interval of the last taps becomes the head-1 delay (folded into the tape's range).
pub fn tap(echo: &mut TapeEcho, taps: &mut TapTempo, now_us: u64) {
    if let Some(seconds) = taps.tap(now_us) {
        let fitted = echo.fit_delay(seconds);
        echo.set_delay(fitted);
    }
}

/// A finger lands on `hit` at (`x`, `y`).
pub fn press(layout: &Layout, echo: &mut TapeEcho, taps: &mut TapTempo, hit: Hit, x: i32, y: i32, now_us: u64) {
    match hit {
        Hit::Head(index) => echo.set_heads(echo.heads() ^ HEADS[index as usize].1),
        Hit::Freeze => echo.set_freeze(!echo.frozen()),
        Hit::Tap => tap(echo, taps, now_us),
        Hit::Slider(_) => drag(layout, echo, hit, x, y),
    }
}

/// A finger that landed on a slider moves to (`x`, `y`).
pub fn drag(layout: &Layout, echo: &mut TapeEcho, hit: Hit, x: i32, _y: i32) {
    if let Hit::Slider(index) = hit {
        set_slider(echo, index, layout.slider_value_at(hit, x));
    }
}

/// A USB-MIDI message; says whether it meant something.
pub fn midi(echo: &mut TapeEcho, taps: &mut TapTempo, message: &[u8], now_us: u64) -> bool {
    let [status, controller, value] = match *message {
        [status, controller, value] => [status, controller, value],
        _ => return false,
    };
    if status & 0xf0 != 0xb0 {
        return false;
    }
    let on = value >= 64;
    match controller {
        CC_FREEZE => echo.set_freeze(on),
        CC_TAP => {
            if on {
                tap(echo, taps, now_us);
            }
        }
        c if CC_HEADS.contains(&c) => {
            if on {
                let index = CC_HEADS.iter().position(|&h| h == c).unwrap_or(0);
                echo.set_heads(echo.heads() ^ HEADS[index].1);
            }
        }
        c if (CC_FIRST_SLIDER..CC_FIRST_SLIDER + SLIDERS.len() as u8).contains(&c) => {
            set_slider(echo, c - CC_FIRST_SLIDER, value as f32 / 127.0);
        }
        _ => return false,
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZES: [(i32, i32); 3] = [(640, 480), (1280, 1024), (3312, 1872)];

    #[test]
    fn every_widget_is_on_screen_and_none_overlap() {
        for (w, h) in SIZES {
            let layout = Layout::new(w, h);
            let screen = Rect { x: 0, y: 0, w, h };
            assert_eq!(layout.widgets().len(), SLIDERS.len() + 5);
            for (i, a) in layout.widgets().iter().enumerate() {
                let r = a.rect;
                assert!(r.w > 4 && r.h > 4, "{w}x{h} {:?}", a.hit);
                assert!(screen.contains(r.x, r.y) && screen.contains(r.x + r.w - 1, r.y + r.h - 1), "{w}x{h} {:?}", a.hit);
                for b in &layout.widgets()[i + 1..] {
                    let s = b.rect;
                    assert!(r.x + r.w <= s.x || s.x + s.w <= r.x || r.y + r.h <= s.y || s.y + s.h <= r.y, "{w}x{h} {:?} {:?}", a.hit, b.hit);
                }
            }
        }
    }

    #[test]
    fn touches_move_sliders_and_flip_switches() {
        let layout = Layout::new(1280, 1024);
        let mut echo = TapeEcho::new(48000);
        let mut taps = TapTempo::new();
        let slider = layout.widget_rect(Hit::Slider(1)).unwrap();
        assert_eq!(layout.hit(slider.x + 2, slider.y + 2), Some(Hit::Slider(1)));
        press(&layout, &mut echo, &mut taps, Hit::Slider(1), slider.x + slider.w / 2, slider.y, 0);
        assert!((echo.intensity() - 0.5).abs() < 0.02);
        let heads = echo.heads();
        press(&layout, &mut echo, &mut taps, Hit::Head(2), 0, 0, 0);
        assert_eq!(echo.heads(), heads ^ HEAD3);
        press(&layout, &mut echo, &mut taps, Hit::Freeze, 0, 0, 0);
        assert!(lit(&echo, Hit::Freeze));
    }

    #[test]
    fn midi_controllers_reach_the_knobs() {
        let mut echo = TapeEcho::new(48000);
        let mut taps = TapTempo::new();
        assert!(midi(&mut echo, &mut taps, &[0xb0, CC_FIRST_SLIDER + 5, 127], 0));
        assert!(echo.reverb() > 0.99);
        assert!(midi(&mut echo, &mut taps, &[0xb0, CC_FREEZE, 127], 0));
        assert!(echo.frozen());
        assert!(midi(&mut echo, &mut taps, &[0xb0, CC_FREEZE, 0], 0));
        assert!(!echo.frozen());
        assert!(!midi(&mut echo, &mut taps, &[0x90, 60, 100], 0));
        assert!(!midi(&mut echo, &mut taps, &[0xb0, 7, 100], 0));
    }

    #[test]
    fn taps_set_the_delay() {
        let mut echo = TapeEcho::new(48000);
        let mut taps = TapTempo::new();
        for i in 0..4u64 {
            tap(&mut echo, &mut taps, i * 200_000);
        }
        assert!((echo.delay() - 0.2).abs() < 0.01, "{}", echo.delay());
    }
}
