//! A 61-key on-screen keyboard (C2-C7, MIDI 36-96) and the typed-key keyboard, shared by the instrument tapes.

use gfx::Canvas;

pub const FIRST_KEY: u8 = 36;
pub const KEYS: usize = 61;
pub const WHITE_KEYS: i32 = 36;

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

/// Position of a note among the white keys of the range (C2 = 0), and whether it is a black key.
fn white_index(note: u8) -> (i32, bool) {
    let offset = (note - FIRST_KEY) as i32;
    let (octave, pitch) = (offset / 12, offset % 12);
    const WHITES_BELOW: [i32; 12] = [0, 0, 1, 1, 2, 3, 3, 4, 4, 5, 5, 6];
    (octave * 7 + WHITES_BELOW[pitch as usize], matches!(pitch, 1 | 3 | 6 | 8 | 10))
}

#[derive(Clone, Copy)]
pub struct Keyboard {
    pub area: Rect,
    white_w: i32,
    black_w: i32,
    black_h: i32,
}

impl Keyboard {
    /// A keyboard `height` tall with its top at `top`, centred in a `width` wide screen with `margin` at the sides.
    pub fn new(width: i32, top: i32, height: i32, margin: i32) -> Self {
        let keys_w = (width - 2 * margin) / WHITE_KEYS * WHITE_KEYS;
        let white_w = keys_w / WHITE_KEYS;
        Keyboard { area: Rect { x: (width - keys_w) / 2, y: top, w: keys_w, h: height }, white_w, black_w: white_w * 6 / 10, black_h: height * 62 / 100 }
    }

    pub fn key_rect(&self, note: u8) -> Rect {
        let (index, black) = white_index(note);
        if black {
            // A black key straddles the line between the white key before it and the one after.
            Rect { x: self.area.x + (index + 1) * self.white_w - self.black_w / 2, y: self.area.y, w: self.black_w, h: self.black_h }
        } else {
            Rect { x: self.area.x + index * self.white_w, y: self.area.y, w: self.white_w, h: self.area.h }
        }
    }

    /// The key under (`x`, `y`): black keys before white ones, which they cover.
    pub fn hit(&self, x: i32, y: i32) -> Option<u8> {
        if !self.area.contains(x, y) {
            return None;
        }
        let notes = FIRST_KEY..FIRST_KEY + KEYS as u8;
        notes.clone().filter(|&n| white_index(n).1).chain(notes.filter(|&n| !white_index(n).1)).find(|&n| self.key_rect(n).contains(x, y))
    }

    /// Draws the keys, a held one in `active` (amber for white keys, `active_black` for black).
    pub fn draw(&self, canvas: &mut Canvas, held: impl Fn(u8) -> bool, active: u32, active_black: u32) {
        let notes = FIRST_KEY..FIRST_KEY + KEYS as u8;
        for note in notes.clone().filter(|&n| !white_index(n).1) {
            let rect = self.key_rect(note);
            canvas.fill_rect(rect.x, rect.y, rect.w as u32, rect.h as u32, if held(note) { active } else { 0xf2ecd8 });
            canvas.rect(rect.x, rect.y, rect.w as u32, rect.h as u32, 0x000000);
            if (note - FIRST_KEY) % 12 == 0 && rect.w >= gfx::GLYPH_WIDTH as i32 * 2 {
                let mut name = [b'C', b'0' + 2 + (note - FIRST_KEY) / 12];
                if let Ok(text) = core::str::from_utf8_mut(&mut name) {
                    canvas.text(rect.x + 2, rect.y + rect.h - gfx::GLYPH_HEIGHT as i32 - 2, text, 0x555555);
                }
            }
        }
        for note in notes.filter(|&n| white_index(n).1) {
            let rect = self.key_rect(note);
            canvas.fill_rect(rect.x, rect.y, rect.w as u32, rect.h as u32, if held(note) { active_black } else { 0x141414 });
            canvas.rect(rect.x, rect.y, rect.w as u32, rect.h as u32, 0x000000);
        }
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
