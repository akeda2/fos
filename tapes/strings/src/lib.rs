//! The strings tape's screen logic without a screen: where every control is (`Layout`), what a touch on it means (`Hit`,
//! `press`, `drag`) and what a control currently shows. `main.rs` draws and runs it; the geometry is testable on the host.
#![cfg_attr(not(test), no_std)]

use instrument_rt::{Keyboard, Rect};
use strings::{ModTarget, SECTIONS, Strings};

pub const GLYPH_H: i32 = gfx::GLYPH_HEIGHT as i32;
const MAX_WIDGETS: usize = 32;
pub const SLIDERS: [&str; 15] =
    ["Attack", "Release", "Ensemble", "Phaser", "Reverb", "Volume", "Mod", "Echo", "Repeat", "Feedback", "Vowel", "Q", "Range", "Env", "Wah"];
pub const TARGETS: [(&str, ModTarget); 4] =
    [("Phaser", ModTarget::Phaser), ("Ens", ModTarget::Ensemble), ("Both", ModTarget::Both), ("Wah", ModTarget::Wah)];

/// Something a finger can be on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    /// A section fader (index into `SECTION_NAMES`).
    Fader(u8),
    /// 0 attack, 1 release, 2 ensemble, 3 phaser, 4 reverb, 5 volume, 6 mod wheel, 7 echo level, 8 echo repeat rate, 9 echo feedback,
    /// 10 wah vowel, 11 wah Q, 12 wah range, 13 wah envelope, 14 wah pedal.
    Slider(u8),
    /// What the mod wheel steers.
    Target(u8),
    Sustain,
    /// The wah on or off.
    Wah,
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
    pub fader_panel: Rect,
    pub control_panel: Rect,
    pub keyboard: Keyboard,
    pub keys_shown: bool,
    /// The fader travel: the y of the top (1.0) and bottom (0.0) of every fader's rail.
    pub track_top: i32,
    pub track_bottom: i32,
    widgets: [Widget; MAX_WIDGETS],
    widget_count: usize,
}

impl Layout {
    /// Where everything goes on a `width` x `height` screen; without the on-screen keyboard the panels take its space.
    pub fn new(width: i32, height: i32, keys_shown: bool) -> Self {
        let scale = if width >= 1200 { 2 } else { 1 };
        let margin = 6 * scale;
        let header_h = GLYPH_H * scale * 2 + margin * 2;
        let keys_h = if keys_shown { height * 28 / 100 } else { 0 };
        let body_top = header_h + margin;
        let body_h = height - body_top - keys_h - if keys_shown { margin * 2 } else { margin };
        let faders_w = (width - 3 * margin) * 55 / 100;
        let fader_panel = Rect { x: margin, y: body_top, w: faders_w, h: body_h };
        let control_panel = Rect { x: margin * 2 + faders_w, y: body_top, w: width - 3 * margin - faders_w, h: body_h };
        let keyboard = Keyboard::new(width, height - keys_h - margin, keys_h, margin);
        let mut layout = Layout {
            width,
            height,
            scale,
            margin,
            header: Rect { x: 0, y: 0, w: width, h: header_h },
            fader_panel,
            control_panel,
            keyboard,
            keys_shown,
            track_top: body_top + GLYPH_H * scale + margin * 2,
            track_bottom: body_top + body_h - GLYPH_H * scale - margin * 2,
            widgets: [Widget { rect: Rect::ZERO, hit: Hit::Sustain }; MAX_WIDGETS],
            widget_count: 0,
        };

        let column = (fader_panel.w - margin) / SECTIONS as i32;
        for index in 0..SECTIONS {
            let rect = Rect { x: fader_panel.x + margin / 2 + index as i32 * column, y: layout.track_top - margin, w: column, h: layout.track_bottom - layout.track_top + margin * 2 };
            layout.add(rect, Hit::Fader(index as u8));
        }
        // The sliders, then a row of the mod wheel's four targets and a row with the pedal and the wah switch, sharing the
        // control panel's height.
        let rows = SLIDERS.len() as i32 + 2;
        let row_h = (control_panel.h - margin) / rows;
        for index in 0..SLIDERS.len() {
            let y = control_panel.y + margin / 2 + index as i32 * row_h;
            layout.add(Rect { x: control_panel.x + margin, y: y + margin / 2, w: control_panel.w - 2 * margin, h: row_h - margin }, Hit::Slider(index as u8));
        }
        let y = control_panel.y + margin / 2 + SLIDERS.len() as i32 * row_h + margin / 2;
        let button_w = (control_panel.w - 2 * margin) / 4;
        for index in 0..TARGETS.len() {
            layout.add(Rect { x: control_panel.x + margin + index as i32 * button_w, y, w: button_w - margin / 2, h: row_h - margin }, Hit::Target(index as u8));
        }
        let (y, button_w) = (y + row_h, (control_panel.w - 2 * margin) / 2);
        for (index, hit) in [Hit::Sustain, Hit::Wah].into_iter().enumerate() {
            layout.add(Rect { x: control_panel.x + margin + index as i32 * button_w, y, w: button_w - margin / 2, h: row_h - margin }, hit);
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

    /// The control under (`x`, `y`); the keys only count while they are shown.
    pub fn hit(&self, x: i32, y: i32) -> Option<Hit> {
        if self.keys_shown {
            if let Some(note) = self.keyboard.hit(x, y) {
                return Some(Hit::Key(note));
            }
        }
        self.widgets().iter().find(|widget| widget.rect.contains(x, y)).map(|widget| widget.hit)
    }

    /// A fader's value, 0..=1, for a finger at height `y`.
    pub fn fader_value(&self, y: i32) -> f32 {
        let travel = (self.track_bottom - self.track_top).max(1) as f32;
        ((self.track_bottom - y) as f32 / travel).clamp(0.0, 1.0)
    }

    /// The y of a fader's knob at `value`.
    pub fn knob_y(&self, value: f32) -> i32 {
        self.track_bottom - ((self.track_bottom - self.track_top) as f32 * value.clamp(0.0, 1.0)) as i32
    }

    /// A slider's value, 0..=1, for a finger at `x`.
    pub fn slider_value_at(&self, hit: Hit, x: i32) -> f32 {
        let Some(rect) = self.widget_rect(hit) else { return 0.0 };
        ((x - rect.x) as f32 / rect.w.max(1) as f32).clamp(0.0, 1.0)
    }
}

/// The value a slider shows.
pub fn slider_value(machine: &Strings, index: u8) -> f32 {
    match index {
        0 => machine.attack(),
        1 => machine.release(),
        2 => machine.ensemble(),
        3 => machine.phaser(),
        4 => machine.reverb(),
        5 => machine.volume(),
        6 => machine.mod_wheel(),
        7 => machine.echo().echo_level(),
        8 => machine.echo().repeat_rate(),
        9 => machine.echo().intensity(),
        10 => machine.vowel(),
        11 => machine.q(),
        12 => machine.range(),
        13 => machine.env(),
        _ => machine.wah_pedal(),
    }
}

fn set_slider(machine: &mut Strings, index: u8, value: f32) {
    match index {
        0 => machine.set_attack(value),
        1 => machine.set_release(value),
        2 => machine.set_ensemble(value),
        3 => machine.set_phaser(value),
        4 => machine.set_reverb(value),
        5 => machine.set_volume(value),
        6 => machine.set_mod_wheel(value),
        7 => machine.echo_mut().set_echo_level(value),
        8 => machine.echo_mut().set_repeat_rate(value),
        9 => machine.echo_mut().set_intensity(value),
        10 => machine.set_vowel(value),
        11 => machine.set_q(value),
        12 => machine.set_range(value),
        13 => machine.set_env(value),
        _ => machine.set_wah_pedal(value),
    }
}

/// Whether a switch is on.
pub fn lit(machine: &Strings, hit: Hit) -> bool {
    match hit {
        Hit::Target(index) => machine.mod_target() == TARGETS[index as usize].1,
        Hit::Sustain => machine.sustain(),
        Hit::Wah => machine.wah_on(),
        _ => false,
    }
}

pub fn label(hit: Hit) -> &'static str {
    match hit {
        Hit::Target(index) => TARGETS[index as usize].0,
        Hit::Sustain => "Pedal",
        Hit::Wah => "Wah",
        _ => "",
    }
}

/// A finger lands on `hit` at (`x`, `y`).
pub fn press(layout: &Layout, machine: &mut Strings, hit: Hit, x: i32, y: i32) {
    match hit {
        Hit::Key(note) => machine.note_on(note),
        Hit::Target(index) => machine.set_mod_target(TARGETS[index as usize].1),
        Hit::Sustain => machine.set_sustain(!machine.sustain()),
        Hit::Wah => machine.set_wah(!machine.wah_on()),
        _ => drag(layout, machine, hit, x, y),
    }
}

/// A finger that landed on a fader or slider moves to (`x`, `y`).
pub fn drag(layout: &Layout, machine: &mut Strings, hit: Hit, x: i32, y: i32) {
    match hit {
        Hit::Fader(index) => machine.set_level(index as usize, layout.fader_value(y)),
        Hit::Slider(index) => set_slider(machine, index, layout.slider_value_at(hit, x)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZES: [(i32, i32); 3] = [(640, 480), (1280, 1024), (3312, 1872)];

    #[test]
    fn every_widget_is_on_screen_and_none_overlap() {
        for (w, h) in SIZES {
            for keys in [true, false] {
                let layout = Layout::new(w, h, keys);
                let screen = Rect { x: 0, y: 0, w, h };
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
    fn hiding_the_keys_gives_the_panels_the_room() {
        let (shown, hidden) = (Layout::new(1280, 1024, true), Layout::new(1280, 1024, false));
        assert!(hidden.control_panel.h > shown.control_panel.h);
        let key = shown.keyboard.key_rect(60);
        assert_eq!(shown.hit(key.x + 1, key.y + key.h - 2), Some(Hit::Key(60)));
        assert!(!matches!(hidden.hit(key.x + 1, key.y + key.h - 2), Some(Hit::Key(_))));
    }

    #[test]
    fn touches_move_faders_sliders_and_switches() {
        let layout = Layout::new(1280, 1024, true);
        let mut machine = Strings::new(48000);
        let fader = layout.widget_rect(Hit::Fader(3)).unwrap();
        let x = fader.x + fader.w / 2;
        assert_eq!(layout.hit(x, layout.track_top), Some(Hit::Fader(3)));
        press(&layout, &mut machine, Hit::Fader(3), x, layout.track_top);
        assert!(machine.level(3) > 0.99);
        drag(&layout, &mut machine, Hit::Fader(3), x, layout.track_bottom + 50);
        assert!(machine.level(3) < 0.01);

        let slider = layout.widget_rect(Hit::Slider(5)).unwrap();
        press(&layout, &mut machine, Hit::Slider(5), slider.x + slider.w / 2, slider.y);
        assert!((machine.volume() - 0.5).abs() < 0.02);

        let target = layout.widget_rect(Hit::Target(1)).unwrap();
        assert_eq!(layout.hit(target.x + 2, target.y + 2), Some(Hit::Target(1)));
        press(&layout, &mut machine, Hit::Target(1), 0, 0);
        assert!(lit(&machine, Hit::Target(1)) && !lit(&machine, Hit::Target(0)));
        press(&layout, &mut machine, Hit::Sustain, 0, 0);
        assert!(lit(&machine, Hit::Sustain));
        press(&layout, &mut machine, Hit::Sustain, 0, 0);
        assert!(!lit(&machine, Hit::Sustain));
        let wah = layout.widget_rect(Hit::Wah).unwrap();
        assert_eq!(layout.hit(wah.x + 2, wah.y + 2), Some(Hit::Wah));
        press(&layout, &mut machine, Hit::Wah, 0, 0);
        assert!(lit(&machine, Hit::Wah));
    }
}
