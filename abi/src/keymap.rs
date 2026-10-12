//! Keyboard decoding shared by every key source: PS/2 scancode set 1 (what an 8042
//! with translation delivers) and USB HID usage codes (boot-protocol reports). Both
//! produce plain ASCII for a US layout, which is all the shell needs. Pure functions
//! and a small state machine, so the host unit tests cover them.

/// Private codes for the keys that have no ASCII value, queued like any other key byte. They
/// are all >= 0x80, which the shell's line editor ignores; a running tape gets them expanded
/// to the ANSI sequences a terminal sends (`ESC [ A` for Up, ...), see `ansi_sequence`.
pub const KEY_UP: u8 = 0x81;
pub const KEY_DOWN: u8 = 0x82;
pub const KEY_RIGHT: u8 = 0x83;
pub const KEY_LEFT: u8 = 0x84;
pub const KEY_HOME: u8 = 0x85;
pub const KEY_END: u8 = 0x86;
pub const KEY_DELETE: u8 = 0x87;
pub const KEY_INSERT: u8 = 0x88;
pub const KEY_PAGE_UP: u8 = 0x89;
pub const KEY_PAGE_DOWN: u8 = 0x8a;

/// Private codes for the letters a Nordic layout adds. Not plain UTF-8 bytes: the second byte of å ä ö Å Ä Ö (0xa5 0xa4 0xb6
/// 0x85 0x84 0x96) would collide with the cursor codes above. A running tape gets them expanded to UTF-8 (`utf8_sequence`); the
/// shell, whose input is ASCII, ignores them like every code >= 0x80.
pub const KEY_ARING: u8 = 0x90;
pub const KEY_AUML: u8 = 0x91;
pub const KEY_OUML: u8 = 0x92;
pub const KEY_ARING_UPPER: u8 = 0x93;
pub const KEY_AUML_UPPER: u8 = 0x94;
pub const KEY_OUML_UPPER: u8 = 0x95;
pub const KEY_EURO: u8 = 0x96;

/// The UTF-8 bytes a tape receives for one of the letter codes above.
pub fn utf8_sequence(code: u8) -> Option<&'static [u8]> {
    Some(match code {
        KEY_ARING => "å".as_bytes(),
        KEY_AUML => "ä".as_bytes(),
        KEY_OUML => "ö".as_bytes(),
        KEY_ARING_UPPER => "Å".as_bytes(),
        KEY_AUML_UPPER => "Ä".as_bytes(),
        KEY_OUML_UPPER => "Ö".as_bytes(),
        KEY_EURO => "€".as_bytes(),
        _ => return None,
    })
}

/// Which physical-key-to-character table is used. The scancodes and usages name *keys*; the layout says what is printed on them.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Layout {
    #[default]
    Us,
    /// Swedish (Finnish is the same): å ä ö on the keys right of P and L, the symbols moved, AltGr for `@ $ { [ ] } \ ~ |` and the
    /// euro sign. The dead keys (´ ¨ ^ ~ and ` as accents) type their plain character instead, or nothing; § ½ ¤ £ are not
    /// produced (the console is ASCII plus the letters above).
    Swedish,
}

/// The bytes after `ESC [` that a terminal sends for one of the private key codes above.
pub fn ansi_sequence(code: u8) -> Option<&'static [u8]> {
    Some(match code {
        KEY_UP => b"A",
        KEY_DOWN => b"B",
        KEY_RIGHT => b"C",
        KEY_LEFT => b"D",
        KEY_HOME => b"H",
        KEY_END => b"F",
        KEY_DELETE => b"3~",
        KEY_INSERT => b"2~",
        KEY_PAGE_UP => b"5~",
        KEY_PAGE_DOWN => b"6~",
        _ => return None,
    })
}

const SET1_PLAIN: &[u8; 0x3a] = b"\0\x1b1234567890-=\x08\tqwertyuiop[]\n\0asdfghjkl;'`\0\\zxcvbnm,./\0*\0 ";
const SET1_SHIFT: &[u8; 0x3a] = b"\0\x1b!@#$%^&*()_+\x08\tQWERTYUIOP{}\n\0ASDFGHJKL:\"~\0|ZXCVBNM<>?\0*\0 ";

/// HID usages 0x04..=0x38 (letters, digits, Enter, Esc, Backspace, Tab, Space, punctuation).
const HID_PLAIN: &[u8; 0x35] = b"abcdefghijklmnopqrstuvwxyz1234567890\n\x1b\x08\t -=[]\\\0;'`,./";
const HID_SHIFT: &[u8; 0x35] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ!@#$%^&*()\n\x1b\x08\t _+{}|\0:\"~<>?";

fn is_letter(byte: u8) -> bool {
    byte.is_ascii_alphabetic()
}

/// Applies shift/caps-lock/ctrl to a table lookup. Caps lock flips only letters; ctrl
/// turns a letter into its control code (Ctrl-C = 3).
fn finish(plain: u8, shifted: u8, shift: bool, ctrl: bool, caps: bool) -> Option<u8> {
    let byte = if is_letter(plain) {
        if shift != caps { shifted } else { plain }
    } else if shift {
        shifted
    } else {
        plain
    };
    if byte == 0 {
        return None;
    }
    if ctrl && is_letter(byte) {
        return Some(byte.to_ascii_lowercase() - b'a' + 1);
    }
    Some(byte)
}

/// The Swedish layout's answer for a HID usage: `None` when the US table applies, `Some(None)` when the key types nothing, else the
/// byte or private code. Only the keys that differ are here.
fn swedish(usage: u8, shift: bool, altgr: bool, caps: bool) -> Option<Option<u8>> {
    let letter = |lower: u8, upper: u8| Some(Some(if shift != caps { upper } else { lower }));
    let pick = |plain: u8, shifted: u8, third: u8| {
        Some(match (altgr, shift) {
            (true, _) if third != 0 => Some(third),
            (true, _) => None,
            (false, true) => (shifted != 0).then_some(shifted),
            (false, false) => (plain != 0).then_some(plain),
        })
    };
    match usage {
        0x1e => pick(b'1', b'!', 0),
        0x1f => pick(b'2', b'"', b'@'),
        0x20 => pick(b'3', b'#', 0),
        0x21 => pick(b'4', 0, b'$'),
        0x22 => pick(b'5', b'%', KEY_EURO),
        0x23 => pick(b'6', b'&', 0),
        0x24 => pick(b'7', b'/', b'{'),
        0x25 => pick(b'8', b'(', b'['),
        0x26 => pick(b'9', b')', b']'),
        0x27 => pick(b'0', b'=', b'}'),
        0x2d => pick(b'+', b'?', b'\\'),
        0x2e => pick(0, b'`', 0),
        0x2f => letter(KEY_ARING, KEY_ARING_UPPER),
        0x30 => pick(0, b'^', b'~'),
        0x31 | 0x32 => pick(b'\'', b'*', 0),
        0x33 => letter(KEY_OUML, KEY_OUML_UPPER),
        0x34 => letter(KEY_AUML, KEY_AUML_UPPER),
        0x35 => Some(None),
        0x36 => pick(b',', b';', 0),
        0x37 => pick(b'.', b':', 0),
        0x38 => pick(b'-', b'_', 0),
        0x64 => pick(b'<', b'>', b'|'),
        _ => None,
    }
}

/// The HID usage of the key a set-1 scancode names, for the keys the Swedish layout changes.
fn set1_usage(code: u8) -> u8 {
    match code {
        0x02..=0x0b => 0x1e + (code - 0x02),
        0x0c => 0x2d,
        0x0d => 0x2e,
        0x1a => 0x2f,
        0x1b => 0x30,
        0x2b => 0x31,
        0x27 => 0x33,
        0x28 => 0x34,
        0x29 => 0x35,
        0x33 => 0x36,
        0x34 => 0x37,
        0x35 => 0x38,
        0x56 => 0x64,
        _ => 0,
    }
}

/// A PS/2 set-1 decoder: feed it the raw bytes from port 0x60.
#[derive(Default)]
pub struct Set1Decoder {
    extended: bool,
    left_shift: bool,
    right_shift: bool,
    ctrl: bool,
    altgr: bool,
    caps: bool,
}

impl Set1Decoder {
    pub const fn new() -> Self {
        Set1Decoder { extended: false, left_shift: false, right_shift: false, ctrl: false, altgr: false, caps: false }
    }

    /// Returns the ASCII byte a key press produces, if any. Releases, modifiers,
    /// extended keys (arrows, keypad Enter...) and controller replies produce none.
    pub fn feed(&mut self, byte: u8) -> Option<u8> {
        self.feed_layout(byte, Layout::Us)
    }

    /// `feed` for a keyboard layout.
    pub fn feed_layout(&mut self, byte: u8, layout: Layout) -> Option<u8> {
        if byte == 0xe0 || byte == 0xe1 {
            self.extended = true;
            return None;
        }
        let extended = core::mem::take(&mut self.extended);
        let released = byte & 0x80 != 0;
        let code = byte & 0x7f;
        match code {
            0x2a if !extended => self.left_shift = !released,
            0x36 if !extended => self.right_shift = !released,
            0x38 if extended => self.altgr = !released, // the right Alt key
            0x1d => self.ctrl = !released, // either Ctrl key (the extended one is right Ctrl)
            0x3a if !extended && !released => self.caps = !self.caps,
            _ => {}
        }
        // The "give up" keys (F12 first; F4/F6/F7/F8/F10 for KVMs that eat F12), the same set
        // v1 uses: each acts as Escape, which is what stops a running tape.
        if !released && !extended && matches!(code, 0x58 | 0x3e | 0x40 | 0x41 | 0x42 | 0x44) {
            return Some(0x1b);
        }
        if extended && !released {
            // The cursor block (and the Insert/Delete/Home/End/Page keys above it) are extended keys.
            let key = match code {
                0x48 => KEY_UP,
                0x50 => KEY_DOWN,
                0x4d => KEY_RIGHT,
                0x4b => KEY_LEFT,
                0x47 => KEY_HOME,
                0x4f => KEY_END,
                0x53 => KEY_DELETE,
                0x52 => KEY_INSERT,
                0x49 => KEY_PAGE_UP,
                0x51 => KEY_PAGE_DOWN,
                _ => 0,
            };
            if key != 0 {
                return Some(key);
            }
        }
        if released || extended || code as usize >= SET1_PLAIN.len() || byte == 0xfa || byte == 0xfe || byte == 0xaa {
            return None;
        }
        if layout == Layout::Swedish && !self.ctrl {
            if let Some(key) = swedish(set1_usage(code), self.left_shift || self.right_shift, self.altgr, self.caps) {
                return key;
            }
        }
        finish(SET1_PLAIN[code as usize], SET1_SHIFT[code as usize], self.left_shift || self.right_shift, self.ctrl, self.caps)
    }
}

/// ASCII for a HID keyboard usage (usage page 0x07) with the given modifiers.
/// `modifiers` is the boot report's modifier byte (bit 0/4 Ctrl, bit 1/5 Shift).
pub fn hid_ascii(usage: u8, modifiers: u8, caps: bool) -> Option<u8> {
    hid_key(usage, modifiers, caps, Layout::Us)
}

/// `hid_ascii` for a keyboard layout (modifier bit 6 is the right Alt, AltGr).
pub fn hid_key(usage: u8, modifiers: u8, caps: bool, layout: Layout) -> Option<u8> {
    // HID usages of F4, F6, F7, F8, F10, F12: the "give up" keys, acting as Escape.
    if matches!(usage, 0x3d | 0x3f | 0x40 | 0x41 | 0x43 | 0x45) {
        return Some(0x1b);
    }
    match usage {
        0x49 => return Some(KEY_INSERT),
        0x4a => return Some(KEY_HOME),
        0x4b => return Some(KEY_PAGE_UP),
        0x4c => return Some(KEY_DELETE),
        0x4d => return Some(KEY_END),
        0x4e => return Some(KEY_PAGE_DOWN),
        0x4f => return Some(KEY_RIGHT),
        0x50 => return Some(KEY_LEFT),
        0x51 => return Some(KEY_DOWN),
        0x52 => return Some(KEY_UP),
        _ => {}
    }
    if layout == Layout::Swedish && modifiers & 0x11 == 0 {
        if let Some(key) = swedish(usage, modifiers & 0x22 != 0, modifiers & 0x40 != 0, caps) {
            return key;
        }
    }
    let index = usize::from(usage.checked_sub(4)?);
    if index >= HID_PLAIN.len() {
        return None;
    }
    finish(HID_PLAIN[index], HID_SHIFT[index], modifiers & 0x22 != 0, modifiers & 0x11 != 0, caps)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn type_set1(bytes: &[u8]) -> Vec<u8> {
        let mut decoder = Set1Decoder::new();
        bytes.iter().filter_map(|&byte| decoder.feed(byte)).collect()
    }

    #[test]
    fn set1_letters_digits_and_enter() {
        // h, i, 1, Enter (make codes; breaks are ignored)
        assert_eq!(type_set1(&[0x23, 0xa3, 0x17, 0x97, 0x02, 0x82, 0x1c, 0x9c]), b"hi1\n");
    }

    #[test]
    fn set1_shift_and_caps() {
        assert_eq!(type_set1(&[0x2a, 0x1e, 0x9e, 0xaa, 0x1e]), b"Aa"); // Shift-a, a
        assert_eq!(type_set1(&[0x3a, 0xba, 0x1e, 0x9e, 0x02]), b"A1"); // Caps: letters upper, digits not
        assert_eq!(type_set1(&[0x36, 0x02, 0x82]), b"!");
    }

    #[test]
    fn set1_ctrl_and_extended() {
        assert_eq!(type_set1(&[0x1d, 0x2e, 0xae, 0x9d]), &[3]); // Ctrl-C
        assert_eq!(type_set1(&[0xe0, 0x48, 0xe0, 0xc8]), &[KEY_UP]); // up arrow: a private code on press, nothing on release
        assert_eq!(type_set1(&[0xe0, 0x4b, 0xe0, 0xcb, 0xe0, 0x53, 0xe0, 0xd3]), &[KEY_LEFT, KEY_DELETE]);
        assert_eq!(ansi_sequence(KEY_LEFT), Some(&b"D"[..]));
        assert_eq!(ansi_sequence(KEY_DELETE), Some(&b"3~"[..]));
        assert_eq!(ansi_sequence(b'a'), None);
        assert_eq!(type_set1(&[0xfa, 0xaa]), b""); // controller reply bytes
    }

    #[test]
    fn function_keys_that_give_up_act_as_escape() {
        assert_eq!(type_set1(&[0x58, 0xd8]), &[0x1b]); // F12
        assert_eq!(type_set1(&[0x44, 0xc4]), &[0x1b]); // F10
        assert_eq!(type_set1(&[0x3b, 0xbb]), b""); // F1 does nothing
        assert_eq!(hid_ascii(0x45, 0, false), Some(0x1b)); // F12
        assert_eq!(hid_ascii(0x3a, 0, false), None); // F1
    }

    #[test]
    fn hid_usages() {
        assert_eq!(hid_ascii(0x04, 0, false), Some(b'a'));
        assert_eq!(hid_ascii(0x04, 0x02, false), Some(b'A'));
        assert_eq!(hid_ascii(0x04, 0, true), Some(b'A'));
        assert_eq!(hid_ascii(0x1e, 0, false), Some(b'1'));
        assert_eq!(hid_ascii(0x1e, 0x20, false), Some(b'!'));
        assert_eq!(hid_ascii(0x28, 0, false), Some(b'\n'));
        assert_eq!(hid_ascii(0x2a, 0, false), Some(8));
        assert_eq!(hid_ascii(0x2c, 0, false), Some(b' '));
        assert_eq!(hid_ascii(0x06, 0x01, false), Some(3)); // Ctrl-C
        assert_eq!(hid_ascii(0x39, 0, false), None); // caps lock itself
        assert_eq!(hid_ascii(0x00, 0, false), None);
        assert_eq!(hid_ascii(0x50, 0, false), Some(KEY_LEFT));
        assert_eq!(hid_ascii(0x4c, 0, false), Some(KEY_DELETE));
    }

    #[test]
    fn swedish_hid() {
        let sv = |usage, modifiers, caps| hid_key(usage, modifiers, caps, Layout::Swedish);
        assert_eq!(sv(0x2f, 0, false), Some(KEY_ARING));
        assert_eq!(sv(0x2f, 0x02, false), Some(KEY_ARING_UPPER));
        assert_eq!(sv(0x34, 0, true), Some(KEY_AUML_UPPER)); // caps lock
        assert_eq!(sv(0x33, 0, false), Some(KEY_OUML));
        assert_eq!(sv(0x1f, 0x02, false), Some(b'"'));
        assert_eq!(sv(0x1f, 0x40, false), Some(b'@'));
        assert_eq!(sv(0x24, 0x40, false), Some(b'{'));
        assert_eq!(sv(0x24, 0x02, false), Some(b'/'));
        assert_eq!(sv(0x2d, 0x40, false), Some(b'\\'));
        assert_eq!(sv(0x38, 0, false), Some(b'-'));
        assert_eq!(sv(0x64, 0x40, false), Some(b'|'));
        assert_eq!(sv(0x2e, 0, false), None); // dead acute
        assert_eq!(sv(0x04, 0, false), Some(b'a')); // letters as before
        assert_eq!(hid_ascii(0x2f, 0, false), Some(b'[')); // the US layout is untouched
        assert_eq!(utf8_sequence(KEY_AUML), Some("ä".as_bytes()));
    }

    #[test]
    fn swedish_set1() {
        let mut decoder = Set1Decoder::new();
        let mut type_sv = |bytes: &[u8]| bytes.iter().filter_map(|&byte| decoder.feed_layout(byte, Layout::Swedish)).collect::<Vec<u8>>();
        assert_eq!(type_sv(&[0x1a, 0x9a, 0x28, 0xa8, 0x27, 0xa7]), &[KEY_ARING, KEY_AUML, KEY_OUML]);
        assert_eq!(type_sv(&[0x2a, 0x28, 0xa8, 0xaa]), &[KEY_AUML_UPPER]);
        // AltGr (extended right Alt) + 7 is `{`; released, 7 is a digit again.
        assert_eq!(type_sv(&[0xe0, 0x38, 0x08, 0x88, 0xe0, 0xb8, 0x08]), &[b'{', b'7']);
        assert_eq!(type_sv(&[0x35, 0xb5]), b"-");
    }
}
