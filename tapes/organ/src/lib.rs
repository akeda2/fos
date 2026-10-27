//! The organ tape's screen logic without a screen: where every control is (`Layout`), what a touch on it means (`Hit`,
//! `press`, `drag`), which control is lit (`lit`) and how a typed key plays a note (`note_for_char`). The tape in `main.rs`
//! draws and runs it; keeping this here makes the geometry testable on the host.
#![cfg_attr(not(test), no_std)]

use organ::{DRAWBARS, DriveKind, FIRST_KEY, KEYS, Leslie, Organ, PRESETS, Percussion, Scanner, WheelTarget};

pub const GLYPH_W: i32 = gfx::GLYPH_WIDTH as i32;
pub const GLYPH_H: i32 = gfx::GLYPH_HEIGHT as i32;
const MAX_WIDGETS: usize = 48;
pub const WHITE_KEYS: i32 = 36;
pub const SLIDERS: [&str; 4] = ["Drive", "Volume", "Click", "Reverb"];
const PERCUSSION: [(&str, Percussion); 3] = [("Off", Percussion::Off), ("2nd", Percussion::Second), ("3rd", Percussion::Third)];
const SCANNERS: [Scanner; 7] = [Scanner::Off, Scanner::V1, Scanner::V2, Scanner::V3, Scanner::C1, Scanner::C2, Scanner::C3];
const LESLIES: [(&str, Leslie); 4] = [("Off", Leslie::Off), ("Stop", Leslie::Stop), ("Slow", Leslie::Slow), ("Fast", Leslie::Fast)];

const WHEELS: [(&str, WheelTarget); 2] = [("Leslie", WheelTarget::Leslie), ("Drive", WheelTarget::Drive)];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub const ZERO: Rect = Rect { x: 0, y: 0, w: 0, h: 0 };

    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }
}

/// Something a finger can be on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    Drawbar(u8),
    Preset(u8),
    Percussion(u8),
    PercussionFast,
    PercussionSoft,
    Scanner(u8),
    Leslie(u8),
    /// The kind of amplifier the overdrive models (index into `DriveKind::ALL`).
    Amp(u8),
    /// What the MIDI mod wheel steers (index into `WHEELS`); with Drive the pitch wheel switches the Leslie.
    Wheel(u8),
    /// 0 drive, 1 volume, 2 key click.
    Slider(u8),
    /// A key, by MIDI note.
    Key(u8),
}

#[derive(Clone, Copy)]
pub struct Widget {
    pub rect: Rect,
    pub hit: Hit,
}

/// Where everything goes on a `width` x `height` screen.
pub struct Layout {
    pub width: i32,
    pub height: i32,
    /// Text scale for labels.
    pub scale: i32,
    pub margin: i32,
    pub header: Rect,
    pub drawbar_panel: Rect,
    pub switch_panel: Rect,
    pub keyboard: Rect,
    /// False when a MIDI keyboard is connected: the on-screen keys are not drawn or touchable and the panels use the room.
    pub keys_shown: bool,
    /// The drawbar travel: position 0 (pushed in) at `track_top`, position 8 at `track_bottom`.
    pub track_top: i32,
    pub track_bottom: i32,
    pub white_w: i32,
    pub black_w: i32,
    pub black_h: i32,
    /// Row labels of the switch panel: text and its top left corner.
    pub labels: [(&'static str, i32, i32); 10],
    pub widgets: [Widget; MAX_WIDGETS],
    pub widget_count: usize,
}

/// Position of a note among the white keys of the 61-key range (C2 = 0), and whether it is a black key.
fn white_index(note: u8) -> (i32, bool) {
    let offset = (note - FIRST_KEY) as i32;
    let (octave, pitch) = (offset / 12, offset % 12);
    const WHITES_BELOW: [i32; 12] = [0, 0, 1, 1, 2, 3, 3, 4, 4, 5, 5, 6];
    (octave * 7 + WHITES_BELOW[pitch as usize], matches!(pitch, 1 | 3 | 6 | 8 | 10))
}

impl Layout {
    pub fn new(width: i32, height: i32, keys_shown: bool) -> Self {
        let scale = (height / 540).max(1);
        let margin = (height / 60).max(3);
        let header_h = (height / 10).max(GLYPH_H * scale + 2 * margin);
        let keyboard_h = if keys_shown { height * 30 / 100 } else { 0 };
        let keys_w = (width - 2 * margin) / WHITE_KEYS * WHITE_KEYS;
        let white_w = keys_w / WHITE_KEYS;
        let keyboard = Rect { x: (width - keys_w) / 2, y: if keys_shown { height - margin - keyboard_h } else { height }, w: keys_w, h: keyboard_h };
        let header = Rect { x: margin, y: margin, w: width - 2 * margin, h: header_h };
        let panel_y = header.y + header.h + margin;
        let panel_h = keyboard.y - margin - panel_y;
        let drawbar_w = (width - 3 * margin) * 55 / 100;
        let drawbar_panel = Rect { x: margin, y: panel_y, w: drawbar_w, h: panel_h };
        let switch_panel = Rect { x: margin * 2 + drawbar_w, y: panel_y, w: width - 3 * margin - drawbar_w, h: panel_h };

        let label_h = GLYPH_H * scale + margin;
        let mut layout = Layout {
            width,
            height,
            scale,
            margin,
            header,
            drawbar_panel,
            switch_panel,
            keyboard,
            keys_shown,
            track_top: panel_y + label_h + margin,
            track_bottom: panel_y + panel_h - 2 * label_h - margin,
            white_w,
            black_w: white_w * 6 / 10,
            black_h: keyboard_h * 62 / 100,
            labels: [("", 0, 0); 10],
            widgets: [Widget { rect: Rect::ZERO, hit: Hit::Preset(0) }; MAX_WIDGETS],
            widget_count: 0,
        };

        let column = drawbar_w / DRAWBARS as i32;
        for index in 0..DRAWBARS {
            let rect = Rect { x: drawbar_panel.x + index as i32 * column, y: panel_y, w: column, h: panel_h };
            layout.add(rect, Hit::Drawbar(index as u8));
        }

        let rows = 10;
        let row_h = switch_panel.h / rows;
        let label_w = GLYPH_W * scale * 8 + margin;
        let gap = (margin / 2).max(2);
        let control_x = switch_panel.x + label_w;
        let control_w = switch_panel.w - label_w - margin;
        let mut row = 0;
        let mut buttons = |layout: &mut Layout, label: &'static str, hits: &[Hit]| {
            let y = switch_panel.y + row * row_h;
            layout.labels[row as usize] = (label, switch_panel.x + margin, y + (row_h - GLYPH_H * scale) / 2);
            let n = hits.len() as i32;
            let button_w = (control_w - gap * (n - 1)) / n;
            for (index, &hit) in hits.iter().enumerate() {
                layout.add(Rect { x: control_x + index as i32 * (button_w + gap), y: y + gap, w: button_w, h: row_h - 2 * gap }, hit);
            }
            row += 1;
        };
        let presets: [Hit; PRESETS.len()] = core::array::from_fn(|index| Hit::Preset(index as u8));
        buttons(&mut layout, "Preset", &presets);
        buttons(&mut layout, "Percuss.", &[Hit::Percussion(0), Hit::Percussion(1), Hit::Percussion(2), Hit::PercussionFast, Hit::PercussionSoft]);
        let scanners: [Hit; 7] = core::array::from_fn(|index| Hit::Scanner(index as u8));
        buttons(&mut layout, "Scanner", &scanners);
        buttons(&mut layout, "Leslie", &[Hit::Leslie(0), Hit::Leslie(1), Hit::Leslie(2), Hit::Leslie(3)]);
        let amps: [Hit; DriveKind::ALL.len()] = core::array::from_fn(|index| Hit::Amp(index as u8));
        buttons(&mut layout, "Amp", &amps);
        buttons(&mut layout, "Mod whl", &[Hit::Wheel(0), Hit::Wheel(1)]);
        for slider in 0..SLIDERS.len() as u8 {
            buttons(&mut layout, SLIDERS[slider as usize], &[Hit::Slider(slider)]);
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

    /// The rectangle of key `note` (36..=96) on the drawn keyboard.
    pub fn key_rect(&self, note: u8) -> Rect {
        let (index, black) = white_index(note);
        if black {
            // A black key straddles the line between the white key before it and the one after.
            Rect { x: self.keyboard.x + (index + 1) * self.white_w - self.black_w / 2, y: self.keyboard.y, w: self.black_w, h: self.black_h }
        } else {
            Rect { x: self.keyboard.x + index * self.white_w, y: self.keyboard.y, w: self.white_w, h: self.keyboard.h }
        }
    }

    /// The control under (`x`, `y`): black keys before white ones, which they cover.
    pub fn hit(&self, x: i32, y: i32) -> Option<Hit> {
        if self.keyboard.contains(x, y) {
            let notes = FIRST_KEY..FIRST_KEY + KEYS as u8;
            return notes.clone().filter(|&n| white_index(n).1).chain(notes.filter(|&n| !white_index(n).1)).find(|&n| self.key_rect(n).contains(x, y)).map(Hit::Key);
        }
        self.widgets().iter().find(|widget| widget.rect.contains(x, y)).map(|widget| widget.hit)
    }

    pub fn widget_rect(&self, hit: Hit) -> Option<Rect> {
        self.widgets().iter().find(|widget| widget.hit == hit).map(|widget| widget.rect)
    }

    /// Drawbar position 0..=8 for a finger at height `y`.
    pub fn drawbar_position(&self, y: i32) -> u8 {
        let travel = (self.track_bottom - self.track_top).max(1);
        (((y - self.track_top) * 8 + travel / 2) / travel).clamp(0, 8) as u8
    }

    /// The y of a drawbar's knob.
    pub fn knob_y(&self, position: u8) -> i32 {
        self.track_top + (self.track_bottom - self.track_top) * position.min(8) as i32 / 8
    }
}

/// A finger lands on a control (or a typed key's equivalent): switches act, keys sound, drawbars and sliders take the
/// position under the finger.
pub fn press(layout: &Layout, organ: &mut Organ, hit: Hit, x: i32, y: i32) {
    match hit {
        Hit::Preset(index) => organ.set_drawbars(PRESETS[index as usize % PRESETS.len()].1),
        Hit::Percussion(index) => organ.set_percussion(PERCUSSION[index as usize % 3].1),
        Hit::PercussionFast => organ.set_percussion_fast(!organ.percussion_fast()),
        Hit::PercussionSoft => organ.set_percussion_soft(!organ.percussion_soft()),
        Hit::Scanner(index) => organ.set_scanner(SCANNERS[index as usize % 7]),
        Hit::Leslie(index) => organ.set_leslie(LESLIES[index as usize % 4].1),
        Hit::Amp(index) => organ.set_drive_kind(DriveKind::ALL[index as usize % DriveKind::ALL.len()]),
        Hit::Wheel(index) => organ.set_wheel_target(WHEELS[index as usize % 2].1),
        Hit::Key(note) => organ.note_on(note),
        Hit::Drawbar(_) | Hit::Slider(_) => drag(layout, organ, hit, x, y),
    }
}

/// A finger that stays down and moves over a drawbar or slider it grabbed.
pub fn drag(layout: &Layout, organ: &mut Organ, hit: Hit, x: i32, y: i32) {
    match hit {
        Hit::Drawbar(index) => organ.set_drawbar(index as usize, layout.drawbar_position(y)),
        Hit::Slider(index) => {
            let Some(rect) = layout.widget_rect(hit) else { return };
            let value = ((x - rect.x) as f32 / rect.w.max(1) as f32).clamp(0.0, 1.0);
            match index {
                0 => organ.set_drive(value),
                1 => organ.set_volume(value),
                2 => organ.set_click(value),
                _ => organ.set_reverb(value),
            }
        }
        _ => {}
    }
}

/// The slider values 0..=1, in `SLIDERS` order.
pub fn slider_value(organ: &Organ, index: u8) -> f32 {
    match index {
        0 => organ.drive(),
        1 => organ.volume(),
        2 => organ.click(),
        _ => organ.reverb(),
    }
}

/// Whether a switch is on (drawn lit).
pub fn lit(organ: &Organ, hit: Hit) -> bool {
    match hit {
        Hit::Preset(index) => organ.drawbars() == PRESETS[index as usize % PRESETS.len()].1,
        Hit::Percussion(index) => organ.percussion() == PERCUSSION[index as usize % 3].1,
        Hit::PercussionFast | Hit::PercussionSoft => false,
        Hit::Scanner(index) => organ.scanner() == SCANNERS[index as usize % 7],
        Hit::Leslie(index) => organ.leslie() == LESLIES[index as usize % 4].1,
        Hit::Amp(index) => organ.drive_kind() == DriveKind::ALL[index as usize % DriveKind::ALL.len()],
        Hit::Wheel(index) => organ.wheel_target() == WHEELS[index as usize % 2].1,
        Hit::Key(note) => organ.is_held(note),
        Hit::Drawbar(_) | Hit::Slider(_) => false,
    }
}

/// The text on a switch.
pub fn label(organ: &Organ, hit: Hit) -> &'static str {
    match hit {
        Hit::Preset(index) => PRESETS[index as usize % PRESETS.len()].0,
        Hit::Percussion(index) => PERCUSSION[index as usize % 3].0,
        Hit::PercussionFast => if organ.percussion_fast() { "Fast" } else { "Slow" },
        Hit::PercussionSoft => if organ.percussion_soft() { "Soft" } else { "Norm" },
        Hit::Scanner(index) => SCANNERS[index as usize % 7].name(),
        Hit::Leslie(index) => LESLIES[index as usize % 4].0,
        Hit::Amp(index) => DriveKind::ALL[index as usize % DriveKind::ALL.len()].short(),
        Hit::Wheel(index) => WHEELS[index as usize % 2].0,
        _ => "",
    }
}

/// The typed-key keyboard (Nordic-safe plain letters and digits): two piano rows, `z s x d c v g b h n j m ,` from `base`
/// and `q 2 w 3 e r 5 t 6 y 7 u i` an octave above. `base` is the MIDI note of `z`.
pub fn note_for_char(c: u8, base: u8) -> Option<u8> {
    const LOWER: &[u8] = b"zsxdcvgbhnjm,";
    const UPPER: &[u8] = b"q2w3er5t6y7ui";
    let c = c.to_ascii_lowercase();
    let semitone = LOWER.iter().position(|&k| k == c).or_else(|| UPPER.iter().position(|&k| k == c).map(|p| p + 12))?;
    let note = base as usize + semitone;
    (FIRST_KEY as usize..FIRST_KEY as usize + KEYS).contains(&note).then_some(note as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZES: [(i32, i32); 6] = [(640, 480), (1344, 768), (1366, 768), (800, 600), (1280, 1024), (1920, 1080)];

    #[test]
    fn every_key_is_found_at_its_own_centre() {
        for (w, h) in SIZES {
            let layout = Layout::new(w, h, true);
            for note in FIRST_KEY..FIRST_KEY + KEYS as u8 {
                let rect = layout.key_rect(note);
                let (_, black) = white_index(note);
                let y = if black { rect.y + rect.h / 2 } else { rect.y + rect.h - 3 };
                assert_eq!(layout.hit(rect.x + rect.w / 2, y), Some(Hit::Key(note)), "{w}x{h} note {note}");
            }
        }
    }

    #[test]
    fn hidden_keyboard_gives_its_room_to_the_panels() {
        let (shown, hidden) = (Layout::new(1280, 1024, true), Layout::new(1280, 1024, false));
        assert!(hidden.drawbar_panel.h > shown.drawbar_panel.h && hidden.switch_panel.h > shown.switch_panel.h);
        assert!(hidden.drawbar_panel.y + hidden.drawbar_panel.h <= 1024);
        let key = shown.key_rect(60);
        assert_eq!(shown.hit(key.x + key.w / 2, key.y + key.h - 2), Some(Hit::Key(60)));
        assert!((0..1024).step_by(8).all(|y| (0..1280).step_by(8).all(|x| !matches!(hidden.hit(x, y), Some(Hit::Key(_))))));
    }

    #[test]
    fn keyboard_has_36_white_keys_and_stays_on_screen() {
        for (w, h) in SIZES {
            let layout = Layout::new(w, h, true);
            assert_eq!((FIRST_KEY..FIRST_KEY + KEYS as u8).filter(|&n| !white_index(n).1).count() as i32, WHITE_KEYS);
            let last = layout.key_rect(96);
            assert!(layout.keyboard.x >= 0 && last.x + last.w <= w && layout.keyboard.y + layout.keyboard.h <= h);
        }
    }

    #[test]
    fn controls_do_not_overlap_and_fit() {
        for (w, h) in SIZES {
            let layout = Layout::new(w, h, true);
            let widgets = layout.widgets();
            for (i, a) in widgets.iter().enumerate() {
                assert!(a.rect.x >= 0 && a.rect.y >= 0 && a.rect.x + a.rect.w <= w && a.rect.y + a.rect.h <= layout.keyboard.y, "{w}x{h} {:?}", a.hit);
                assert!(a.rect.w > GLYPH_W && a.rect.h > GLYPH_H, "{w}x{h} {:?} too small for its label", a.hit);
                for b in &widgets[i + 1..] {
                    let apart = a.rect.x + a.rect.w <= b.rect.x || b.rect.x + b.rect.w <= a.rect.x || a.rect.y + a.rect.h <= b.rect.y || b.rect.y + b.rect.h <= a.rect.y;
                    assert!(apart, "{w}x{h}: {:?} overlaps {:?}", a.hit, b.hit);
                }
            }
        }
    }

    #[test]
    fn dragging_a_drawbar_sets_its_position() {
        let layout = Layout::new(1280, 1024, true);
        let mut organ = Organ::new(48000);
        let column = layout.widget_rect(Hit::Drawbar(2)).unwrap();
        let hit = layout.hit(column.x + 5, layout.knob_y(5)).unwrap();
        assert_eq!(hit, Hit::Drawbar(2));
        press(&layout, &mut organ, hit, column.x + 5, layout.knob_y(5));
        assert_eq!(organ.drawbars()[2], 5);
        drag(&layout, &mut organ, hit, 0, layout.track_top - 50);
        assert_eq!(organ.drawbars()[2], 0);
        drag(&layout, &mut organ, hit, 0, layout.height);
        assert_eq!(organ.drawbars()[2], 8);
        assert_eq!(layout.drawbar_position(layout.knob_y(3)), 3);
    }

    #[test]
    fn switches_and_sliders_act() {
        let layout = Layout::new(1280, 1024, true);
        let mut organ = Organ::new(48000);
        press(&layout, &mut organ, Hit::Preset(1), 0, 0);
        assert!(lit(&organ, Hit::Preset(1)) && !lit(&organ, Hit::Preset(0)));
        press(&layout, &mut organ, Hit::Leslie(3), 0, 0);
        assert_eq!(organ.leslie(), Leslie::Fast);
        assert_eq!(organ.drive_kind(), DriveKind::PushPull);
        press(&layout, &mut organ, Hit::Amp(6), 0, 0);
        assert!(lit(&organ, Hit::Amp(6)) && !lit(&organ, Hit::Amp(3)) && organ.drive_kind() == DriveKind::Vox);
        press(&layout, &mut organ, Hit::Wheel(1), 0, 0);
        assert!(lit(&organ, Hit::Wheel(1)) && !lit(&organ, Hit::Wheel(0)) && organ.wheel_target() == WheelTarget::Drive);
        press(&layout, &mut organ, Hit::Scanner(5), 0, 0);
        assert!(lit(&organ, Hit::Scanner(5)));
        press(&layout, &mut organ, Hit::PercussionFast, 0, 0);
        assert_eq!(label(&organ, Hit::PercussionFast), "Slow");
        let slider = layout.widget_rect(Hit::Slider(1)).unwrap();
        press(&layout, &mut organ, Hit::Slider(1), slider.x + slider.w / 2, slider.y);
        assert!((organ.volume() - 0.5).abs() < 0.01);
        press(&layout, &mut organ, Hit::Key(60), 0, 0);
        assert!(organ.is_held(60));
    }

    #[test]
    fn typed_keys_make_notes() {
        assert_eq!(note_for_char(b'z', 48), Some(48));
        assert_eq!(note_for_char(b'Q', 48), Some(60));
        assert_eq!(note_for_char(b'i', 48), Some(72));
        assert_eq!(note_for_char(b'p', 48), None);
        assert_eq!(note_for_char(b'z', 30), None);
        assert_eq!(note_for_char(b'i', 90), None);
    }
}
