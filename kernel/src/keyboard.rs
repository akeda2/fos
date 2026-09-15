use crate::arch::x86::{inb, outb};

const DATA_PORT: u16 = 0x60;
const STATUS_PORT: u16 = 0x64;
const STATUS_OUTPUT_FULL: u8 = 1;
const STATUS_INPUT_FULL: u8 = 2;

pub struct Keyboard {
    left_shift: bool,
    right_shift: bool,
}

impl Keyboard {
    pub fn initialize() -> Option<Self> {
        if !wait_for_input_empty() {
            return None;
        }
        unsafe { outb(DATA_PORT, 0xf4) };
        if !wait_for_output() {
            return None;
        }
        if unsafe { inb(DATA_PORT) } != 0xfa {
            return None;
        }
        Some(Self {
            left_shift: false,
            right_shift: false,
        })
    }

    pub fn read_char(&mut self) -> Option<u8> {
        if unsafe { inb(STATUS_PORT) } & STATUS_OUTPUT_FULL == 0 {
            return None;
        }
        let scancode = unsafe { inb(DATA_PORT) };
        self.decode_scancode(scancode)
    }

    fn decode_scancode(&mut self, scancode: u8) -> Option<u8> {
        let released = scancode & 0x80 != 0;
        let code = scancode & 0x7f;
        match code {
            0x2a => self.left_shift = !released,
            0x36 => self.right_shift = !released,
            _ if released => {}
            0x1c => return Some(b'\n'),
            0x0e => return Some(8),
            _ => return keycode_to_ascii(code, self.left_shift || self.right_shift),
        }
        None
    }
}

fn wait_for_input_empty() -> bool {
    for _ in 0..100_000 {
        if unsafe { inb(STATUS_PORT) } & STATUS_INPUT_FULL == 0 {
            return true;
        }
    }
    false
}

fn wait_for_output() -> bool {
    for _ in 0..100_000 {
        if unsafe { inb(STATUS_PORT) } & STATUS_OUTPUT_FULL != 0 {
            return true;
        }
    }
    false
}

fn keycode_to_ascii(code: u8, shifted: bool) -> Option<u8> {
    let normal = match code {
        0x02..=0x0b => b"1234567890"[(code - 0x02) as usize],
        0x10..=0x19 => b"qwertyuiop"[(code - 0x10) as usize],
        0x1e..=0x26 => b"asdfghjkl"[(code - 0x1e) as usize],
        0x2c..=0x32 => b"zxcvbnm"[(code - 0x2c) as usize],
        0x39 => b' ',
        0x0c => b'-',
        0x0d => b'=',
        0x1a => b'[',
        0x1b => b']',
        0x27 => b';',
        0x28 => b'\'',
        0x29 => b'`',
        0x2b => b'\\',
        0x33 => b',',
        0x34 => b'.',
        0x35 => b'/',
        _ => return None,
    };
    if !shifted {
        return Some(normal);
    }
    Some(match normal {
        b'a'..=b'z' => normal - b'a' + b'A',
        b'1' => b'!',
        b'2' => b'@',
        b'3' => b'#',
        b'4' => b'$',
        b'5' => b'%',
        b'6' => b'^',
        b'7' => b'&',
        b'8' => b'*',
        b'9' => b'(',
        b'0' => b')',
        b'-' => b'_',
        b'=' => b'+',
        b'[' => b'{',
        b']' => b'}',
        b';' => b':',
        b'\'' => b'"',
        b'`' => b'~',
        b'\\' => b'|',
        b',' => b'<',
        b'.' => b'>',
        b'/' => b'?',
        _ => normal,
    })
}

#[cfg(test)]
mod tests {
    use super::keycode_to_ascii;

    #[test]
    fn translates_us_keyboard_letters_and_shifted_symbols() {
        assert_eq!(keycode_to_ascii(0x23, false), Some(b'h'));
        assert_eq!(keycode_to_ascii(0x23, true), Some(b'H'));
        assert_eq!(keycode_to_ascii(0x02, true), Some(b'!'));
    }
}
