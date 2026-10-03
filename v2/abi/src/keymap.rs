//! Keyboard decoding shared by every key source: PS/2 scancode set 1 (what an 8042
//! with translation delivers) and USB HID usage codes (boot-protocol reports). Both
//! produce plain ASCII for a US layout, which is all the shell needs. Pure functions
//! and a small state machine, so the host unit tests cover them.

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

/// A PS/2 set-1 decoder: feed it the raw bytes from port 0x60.
#[derive(Default)]
pub struct Set1Decoder {
    extended: bool,
    left_shift: bool,
    right_shift: bool,
    ctrl: bool,
    caps: bool,
}

impl Set1Decoder {
    pub const fn new() -> Self {
        Set1Decoder { extended: false, left_shift: false, right_shift: false, ctrl: false, caps: false }
    }

    /// Returns the ASCII byte a key press produces, if any. Releases, modifiers,
    /// extended keys (arrows, keypad Enter...) and controller replies produce none.
    pub fn feed(&mut self, byte: u8) -> Option<u8> {
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
            0x1d => self.ctrl = !released, // either Ctrl key (the extended one is right Ctrl)
            0x3a if !extended && !released => self.caps = !self.caps,
            _ => {}
        }
        if released || extended || code as usize >= SET1_PLAIN.len() || byte == 0xfa || byte == 0xfe || byte == 0xaa {
            return None;
        }
        finish(SET1_PLAIN[code as usize], SET1_SHIFT[code as usize], self.left_shift || self.right_shift, self.ctrl, self.caps)
    }
}

/// ASCII for a HID keyboard usage (usage page 0x07) with the given modifiers.
/// `modifiers` is the boot report's modifier byte (bit 0/4 Ctrl, bit 1/5 Shift).
pub fn hid_ascii(usage: u8, modifiers: u8, caps: bool) -> Option<u8> {
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
        assert_eq!(type_set1(&[0xe0, 0x48, 0xe0, 0xc8]), b""); // up arrow: ignored
        assert_eq!(type_set1(&[0xfa, 0xaa]), b""); // controller reply bytes
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
    }
}
