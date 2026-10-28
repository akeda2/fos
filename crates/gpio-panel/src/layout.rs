//! Where everything is on the screen, for any picture size from about 640x480 up: the connector on the left as two columns
//! of pads, one pad per row of the connector, and the buttons for the selected pin on the right. Hit testing is here too, so
//! it can be tested without a screen.

use crate::header::{self, Pad, ROWS};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.w && y < self.y + self.h
    }
}

/// What a touch (or a key) can land on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    /// A GPIO pad of the connector.
    Pin(u8),
    /// A function button: 0 input, 1 output, 2..=7 alt0..alt5.
    Function(u8),
    Level(bool),
    Toggle,
    Pulse,
    /// 0 none, 1 up, 2 down.
    Pull(u8),
    Arm,
}

#[derive(Clone, Copy, Debug)]
pub struct Button {
    pub rect: Rect,
    pub hit: Hit,
    pub label: &'static str,
}

pub const BUTTONS: usize = 16;

pub struct Layout {
    pub width: i32,
    pub height: i32,
    /// Text scale: 1 for glyphs 8x16, 2 on a big screen.
    pub scale: i32,
    pub margin: i32,
    pub title: Rect,
    /// The connector's backing plate.
    pub plate: Rect,
    /// The pad drawn for each of the 40 connector positions (pad 1 first).
    pub pads: [Rect; 40],
    /// The area a finger may land on for each pad: its half of the row, labels included.
    pub cells: [Rect; 40],
    /// The text columns beside the pads: left labels end at `left_text` (right aligned), right labels start at `right_text`.
    pub left_text: i32,
    pub right_text: i32,
    /// The selected pin's description.
    pub info: Rect,
    pub buttons: [Button; BUTTONS],
}

const LABELS: [(&str, Hit); BUTTONS] = [
    ("IN", Hit::Function(0)),
    ("OUT", Hit::Function(1)),
    ("ALT0", Hit::Function(2)),
    ("ALT1", Hit::Function(3)),
    ("ALT2", Hit::Function(4)),
    ("ALT3", Hit::Function(5)),
    ("ALT4", Hit::Function(6)),
    ("ALT5", Hit::Function(7)),
    ("LOW", Hit::Level(false)),
    ("HIGH", Hit::Level(true)),
    ("TOGGLE", Hit::Toggle),
    ("PULSE", Hit::Pulse),
    ("NO PULL", Hit::Pull(0)),
    ("PULL UP", Hit::Pull(1)),
    ("PULL DOWN", Hit::Pull(2)),
    ("ARM", Hit::Arm),
];

impl Layout {
    pub fn new(width: i32, height: i32) -> Layout {
        let margin = (height / 80).max(6);
        let scale = if height >= 1000 { 2 } else { 1 };
        let glyph = 8 * scale;
        let title_h = (height / 12).clamp(34, 72);
        let top = title_h + margin;
        let row_h = ((height - top - margin) / ROWS as i32).max(12);
        let pad = (row_h * 4 / 5).clamp(12, 44);
        let gap = pad / 3 + 6;
        let label_w = 16 * glyph;
        let block_w = 2 * label_w + 2 * pad + gap;
        let plate = Rect { x: margin, y: top, w: block_w, h: row_h * ROWS as i32 };
        let first_pad = margin + label_w;
        let second_pad = first_pad + pad + gap;
        let middle = first_pad + pad + gap / 2;
        let mut pads = [Rect::default(); 40];
        let mut cells = [Rect::default(); 40];
        for index in 0..40 {
            let (row, column) = (index as i32 / 2, index as i32 % 2);
            let y = top + row * row_h;
            let x = if column == 0 { first_pad } else { second_pad };
            pads[index] = Rect { x, y: y + (row_h - pad) / 2, w: pad, h: pad };
            cells[index] = if column == 0 { Rect { x: margin, y, w: middle - margin, h: row_h } } else { Rect { x: middle, y, w: margin + block_w - middle, h: row_h } };
        }

        let panel_x = margin + block_w + 2 * margin;
        let panel_w = (width - panel_x - margin).max(120);
        let line_h = 16 * scale + 2;
        let info = Rect { x: panel_x, y: top, w: panel_w, h: 6 * line_h };
        let button_h = (height / 14).clamp(30, 56);
        let spacing = (button_h / 5).max(6);
        let mut buttons = [Button { rect: Rect::default(), hit: Hit::Arm, label: "" }; BUTTONS];
        // Rows of buttons: function (2), alt (6), level (4), pull (3).
        let rows: [(usize, usize); 4] = [(0, 2), (2, 6), (8, 4), (12, 3)];
        let mut y = info.y + info.h + margin;
        for (first, count) in rows {
            let each = (panel_w - (count as i32 - 1) * spacing) / count as i32;
            for k in 0..count {
                let (label, hit) = LABELS[first + k];
                buttons[first + k] = Button { rect: Rect { x: panel_x + k as i32 * (each + spacing), y, w: each, h: button_h }, hit, label };
            }
            y += button_h + spacing;
        }
        let arm_w = 14 * glyph;
        let (label, hit) = LABELS[15];
        buttons[15] = Button { rect: Rect { x: width - margin - arm_w, y: margin / 2, w: arm_w, h: title_h - margin }, hit, label };
        Layout {
            width,
            height,
            scale,
            margin,
            title: Rect { x: 0, y: 0, w: width, h: title_h },
            plate,
            pads,
            cells,
            left_text: first_pad - 6,
            right_text: second_pad + pad + 6,
            info,
            buttons,
        }
    }

    /// Where the status text goes: below the buttons.
    pub fn footer_y(&self) -> i32 {
        self.buttons[14].rect.y + self.buttons[14].rect.h + self.margin
    }

    pub fn hit(&self, x: i32, y: i32) -> Option<Hit> {
        if let Some(button) = self.buttons.iter().find(|button| button.rect.contains(x, y)) {
            return Some(button.hit);
        }
        (0..40).find_map(|index| match header::HEADER[index] {
            Pad::Gpio(gpio) if self.cells[index].contains(x, y) => Some(Hit::Pin(gpio)),
            _ => None,
        })
    }

    /// The rectangle of the pad for `gpio`.
    pub fn pad_of(&self, gpio: u8) -> Option<Rect> {
        header::number_of(gpio).map(|number| self.pads[number - 1])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn centre(rect: Rect) -> (i32, i32) {
        (rect.x + rect.w / 2, rect.y + rect.h / 2)
    }

    #[test]
    fn it_fits_the_screens_it_is_meant_for() {
        for (w, h) in [(640, 480), (800, 600), (1024, 768), (1344, 768), (1920, 1080)] {
            let layout = Layout::new(w, h);
            let all = layout.buttons.iter().map(|button| button.rect).chain(layout.pads).chain([layout.info, layout.plate]);
            for rect in all {
                assert!(rect.x >= 0 && rect.y >= 0 && rect.x + rect.w <= w && rect.y + rect.h <= h, "{w}x{h}: {rect:?}");
            }
            assert!(layout.footer_y() + 4 * 18 < h, "{w}x{h}: footer at {}", layout.footer_y());
            for button in layout.buttons {
                assert!(button.rect.w >= 40 && button.rect.h >= 30, "{w}x{h}: {button:?} is too small for a finger");
            }
        }
    }

    #[test]
    fn a_finger_lands_on_the_right_thing() {
        let layout = Layout::new(1344, 768);
        let (x, y) = centre(layout.pad_of(17).unwrap());
        assert_eq!(layout.hit(x, y), Some(Hit::Pin(17)));
        // Pad 11 is the left one in its row, 12 (GPIO18) the right one.
        let (x, y) = centre(layout.pad_of(18).unwrap());
        assert_eq!(layout.hit(x, y), Some(Hit::Pin(18)));
        assert!(layout.pad_of(17).unwrap().x < layout.pad_of(18).unwrap().x);
        // The label beside a pad counts, power and ground do not.
        assert_eq!(layout.hit(layout.plate.x + 4, layout.pads[10].y + 4), Some(Hit::Pin(17)));
        assert_eq!(layout.hit(centre(layout.pads[0]).0, centre(layout.pads[0]).1), None);
        for button in layout.buttons {
            let (x, y) = centre(button.rect);
            assert_eq!(layout.hit(x, y), Some(button.hit));
        }
    }

    #[test]
    fn no_two_things_overlap() {
        let layout = Layout::new(1024, 768);
        for (i, a) in layout.buttons.iter().enumerate() {
            for b in &layout.buttons[i + 1..] {
                let apart = a.rect.x + a.rect.w <= b.rect.x || b.rect.x + b.rect.w <= a.rect.x || a.rect.y + a.rect.h <= b.rect.y || b.rect.y + b.rect.h <= a.rect.y;
                assert!(apart, "{a:?} / {b:?}");
            }
        }
    }
}
