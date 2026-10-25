//! The epiano tape's screen logic without a screen: where every control is (`Layout`), what a touch on it means (`Hit`,
//! `press`, `drag`) and what a control currently shows. `main.rs` draws and runs it; the geometry is testable on the host.
#![cfg_attr(not(test), no_std)]

use epiano::{EPiano, ModTarget, Model, Voicing};
use instrument_rt::{Keyboard, Rect};

pub const GLYPH_H: i32 = gfx::GLYPH_HEIGHT as i32;
const MAX_WIDGETS: usize = 40;
pub const SLIDERS: [&str; 22] = [
    "Bell", "Bark", "Decay", "Drive", "Chorus", "Phaser", "Trem rate", "Trem depth", "Echo", "Repeat", "Feedback", "Reverb", "Volume", "Mod", "Detune", "Touch", "Vowel", "Glide", "Q",
    "Range", "Soft", "Wah",
];
pub const TARGETS: [(&str, ModTarget); 5] =
    [("Trem", ModTarget::Tremolo), ("Echo fb", ModTarget::Echo), ("Bark", ModTarget::Bark), ("All", ModTarget::All), ("Wah", ModTarget::Wah)];
/// Sliders per column; the sliders fill the first column, then the second.
const ROWS: usize = SLIDERS.len().div_ceil(2);

/// Something a finger can be on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    /// An index into `SLIDERS`.
    Slider(u8),
    /// What the mod wheel steers.
    Target(u8),
    Sustain,
    /// How the tine is synthesised (index into `Model::ALL`).
    Model(u8),
    /// The vowel filter (lit) or the old sweep after each pickup.
    Voicing,
    /// A key, by MIDI note.
    Key(u8),
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
    pub keyboard: Keyboard,
    pub keys_shown: bool,
    widgets: [Widget; MAX_WIDGETS],
    widget_count: usize,
}

impl Layout {
    /// Where everything goes on a `width` x `height` screen; without the on-screen keyboard the panel takes its space.
    pub fn new(width: i32, height: i32, keys_shown: bool) -> Self {
        let scale = if width >= 1200 { 2 } else { 1 };
        let margin = 6 * scale;
        let header_h = GLYPH_H * scale * 2 + margin * 2;
        let keys_h = if keys_shown { height * 28 / 100 } else { 0 };
        let body_top = header_h + margin;
        let body_h = height - body_top - keys_h - if keys_shown { margin * 2 } else { margin };
        let panel = Rect { x: margin, y: body_top, w: width - 2 * margin, h: body_h };
        let keyboard = Keyboard::new(width, height - keys_h - margin, keys_h, margin);
        let mut layout = Layout {
            width,
            height,
            scale,
            margin,
            header: Rect { x: 0, y: 0, w: width, h: header_h },
            panel,
            keyboard,
            keys_shown,
            widgets: [Widget { rect: Rect::ZERO, hit: Hit::Sustain }; MAX_WIDGETS],
            widget_count: 0,
        };

        // Two columns of sliders, a row with the five mod targets and the pedal, a row with the tine models and the voicing.
        let row_h = (panel.h - margin) / (ROWS as i32 + 2);
        let column_w = (panel.w - margin) / 2;
        for index in 0..SLIDERS.len() {
            let (column, row) = ((index / ROWS) as i32, (index % ROWS) as i32);
            let rect = Rect { x: panel.x + margin + column * column_w, y: panel.y + margin / 2 + row * row_h + margin / 2, w: column_w - margin, h: row_h - margin };
            layout.add(rect, Hit::Slider(index as u8));
        }
        let y = panel.y + margin / 2 + ROWS as i32 * row_h + margin / 2;
        let button_w = (panel.w - margin) / (TARGETS.len() as i32 + 1);
        for index in 0..TARGETS.len() + 1 {
            let hit = if index < TARGETS.len() { Hit::Target(index as u8) } else { Hit::Sustain };
            layout.add(Rect { x: panel.x + margin + index as i32 * button_w, y, w: button_w - margin, h: row_h - margin }, hit);
        }
        let y = y + row_h;
        let button_w = (panel.w - margin) / (Model::ALL.len() as i32 + 1);
        for index in 0..Model::ALL.len() {
            layout.add(Rect { x: panel.x + margin + index as i32 * button_w, y, w: button_w - margin, h: row_h - margin }, Hit::Model(index as u8));
        }
        let index = Model::ALL.len() as i32;
        layout.add(Rect { x: panel.x + margin + index * button_w, y, w: button_w - margin, h: row_h - margin }, Hit::Voicing);
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

    /// The control under (`x`, `y`); the keys only count while they are shown.
    pub fn hit(&self, x: i32, y: i32) -> Option<Hit> {
        if self.keys_shown {
            if let Some(note) = self.keyboard.hit(x, y) {
                return Some(Hit::Key(note));
            }
        }
        self.widgets().iter().find(|widget| widget.rect.contains(x, y)).map(|widget| widget.hit)
    }

    /// A slider's value, 0..=1, for a finger at `x`.
    pub fn slider_value_at(&self, hit: Hit, x: i32) -> f32 {
        let Some(rect) = self.widget_rect(hit) else { return 0.0 };
        ((x - rect.x) as f32 / rect.w.max(1) as f32).clamp(0.0, 1.0)
    }

    /// How hard a finger at height `y` strikes `note`: the lower on the key, the harder (a touch screen has no velocity).
    pub fn velocity_at(&self, note: u8, y: i32) -> u8 {
        let key = self.keyboard.key_rect(note);
        let depth = ((y - key.y) as f32 / key.h.max(1) as f32).clamp(0.0, 1.0);
        (35.0 + 92.0 * depth) as u8
    }
}

/// The value a slider shows.
pub fn slider_value(piano: &EPiano, index: u8) -> f32 {
    match index {
        0 => piano.bell(),
        1 => piano.bark(),
        2 => piano.decay(),
        3 => piano.drive(),
        4 => piano.chorus(),
        5 => piano.phaser(),
        6 => piano.tremolo_rate(),
        7 => piano.tremolo_depth(),
        8 => piano.echo_level(),
        9 => piano.echo_repeat(),
        10 => piano.echo_feedback(),
        11 => piano.reverb(),
        12 => piano.volume(),
        13 => piano.mod_wheel(),
        14 => piano.detune(),
        15 => piano.touch(),
        16 => piano.vowel(),
        17 => piano.glide(),
        18 => piano.q(),
        19 => piano.range(),
        20 => piano.soft(),
        _ => piano.wah_pedal(),
    }
}

fn set_slider(piano: &mut EPiano, index: u8, value: f32) {
    match index {
        0 => piano.set_bell(value),
        1 => piano.set_bark(value),
        2 => piano.set_decay(value),
        3 => piano.set_drive(value),
        4 => piano.set_chorus(value),
        5 => piano.set_phaser(value),
        6 => piano.set_tremolo_rate(value),
        7 => piano.set_tremolo_depth(value),
        8 => piano.set_echo_level(value),
        9 => piano.set_echo_repeat(value),
        10 => piano.set_echo_feedback(value),
        11 => piano.set_reverb(value),
        12 => piano.set_volume(value),
        13 => piano.set_mod_wheel(value),
        14 => piano.set_detune(value),
        15 => piano.set_touch(value),
        16 => piano.set_vowel(value),
        17 => piano.set_glide(value),
        18 => piano.set_q(value),
        19 => piano.set_range(value),
        20 => piano.set_soft(value),
        _ => piano.set_wah_pedal(value),
    }
}

/// Whether a switch is on.
pub fn lit(piano: &EPiano, hit: Hit) -> bool {
    match hit {
        Hit::Target(index) => piano.mod_target() == TARGETS[index as usize].1,
        Hit::Sustain => piano.sustain(),
        Hit::Model(index) => piano.model() == Model::ALL[index as usize],
        Hit::Voicing => piano.voicing() == Voicing::Vowel,
        _ => false,
    }
}

pub fn label(hit: Hit) -> &'static str {
    match hit {
        Hit::Target(index) => TARGETS[index as usize].0,
        Hit::Sustain => "Pedal",
        Hit::Model(index) => Model::ALL[index as usize].name(),
        Hit::Voicing => "Vowel",
        _ => "",
    }
}

/// A finger lands on `hit` at (`x`, `y`).
pub fn press(layout: &Layout, piano: &mut EPiano, hit: Hit, x: i32, y: i32) {
    match hit {
        Hit::Key(note) => piano.note_on(note, layout.velocity_at(note, y)),
        Hit::Target(index) => piano.set_mod_target(TARGETS[index as usize].1),
        Hit::Sustain => piano.set_sustain(!piano.sustain()),
        Hit::Model(index) => piano.set_model(Model::ALL[index as usize]),
        Hit::Voicing => piano.set_voicing(if piano.voicing() == Voicing::Vowel { Voicing::Sweep } else { Voicing::Vowel }),
        Hit::Slider(_) => drag(layout, piano, hit, x, y),
    }
}

/// A finger that landed on a slider moves to (`x`, `y`).
pub fn drag(layout: &Layout, piano: &mut EPiano, hit: Hit, x: i32, _y: i32) {
    if let Hit::Slider(index) = hit {
        set_slider(piano, index, layout.slider_value_at(hit, x));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZES: [(i32, i32); 4] = [(640, 480), (1366, 768), (1280, 1024), (3312, 1872)];

    #[test]
    fn every_widget_is_on_screen_and_none_overlap() {
        for (w, h) in SIZES {
            for keys in [true, false] {
                let layout = Layout::new(w, h, keys);
                let screen = Rect { x: 0, y: 0, w, h };
                assert_eq!(layout.widgets().len(), SLIDERS.len() + TARGETS.len() + 1 + Model::ALL.len() + 1);
                for (i, a) in layout.widgets().iter().enumerate() {
                    let r = a.rect;
                    assert!(r.w > 4 && r.h > 4, "{w}x{h} {:?}", a.hit);
                    assert!(screen.contains(r.x, r.y) && screen.contains(r.x + r.w - 1, r.y + r.h - 1), "{w}x{h} {:?}", a.hit);
                    for b in &layout.widgets()[i + 1..] {
                        let s = b.rect;
                        assert!(r.x + r.w <= s.x || s.x + s.w <= r.x || r.y + r.h <= s.y || s.y + s.h <= r.y, "{w}x{h} {:?} {:?}", a.hit, b.hit);
                    }
                }
                if keys {
                    assert!(layout.widgets().iter().all(|a| a.rect.y + a.rect.h <= layout.keyboard.area.y), "{w}x{h}");
                }
            }
        }
    }

    #[test]
    fn touches_move_sliders_and_switches_and_strike_keys_by_height() {
        let layout = Layout::new(1280, 1024, true);
        let mut piano = EPiano::new(48000);
        let slider = layout.widget_rect(Hit::Slider(12)).unwrap();
        assert_eq!(layout.hit(slider.x + 2, slider.y + 2), Some(Hit::Slider(12)));
        press(&layout, &mut piano, Hit::Slider(12), slider.x + slider.w / 2, slider.y);
        assert!((piano.volume() - 0.5).abs() < 0.02);
        drag(&layout, &mut piano, Hit::Slider(12), slider.x + slider.w + 50, slider.y);
        assert!(piano.volume() > 0.99);

        press(&layout, &mut piano, Hit::Target(2), 0, 0);
        assert!(lit(&piano, Hit::Target(2)) && !lit(&piano, Hit::Target(3)));
        press(&layout, &mut piano, Hit::Sustain, 0, 0);
        assert!(lit(&piano, Hit::Sustain));
        press(&layout, &mut piano, Hit::Model(2), 0, 0);
        assert!(lit(&piano, Hit::Model(2)) && !lit(&piano, Hit::Model(0)));
        assert!(lit(&piano, Hit::Voicing));
        press(&layout, &mut piano, Hit::Voicing, 0, 0);
        assert!(!lit(&piano, Hit::Voicing) && piano.voicing() == Voicing::Sweep);

        let key = layout.keyboard.key_rect(60);
        assert_eq!(layout.hit(key.x + 1, key.y + key.h - 2), Some(Hit::Key(60)));
        assert!(layout.velocity_at(60, key.y + key.h - 1) > layout.velocity_at(60, key.y + 1) + 60);
        press(&layout, &mut piano, Hit::Key(60), key.x + 1, key.y + key.h / 2);
        assert!(piano.is_held(60));
    }
}
