use crate::arch::x86::{inb, outb};
use crate::interrupts;

const DATA_PORT: u16 = 0x60;
const STATUS_PORT: u16 = 0x64;
const STATUS_OUTPUT_FULL: u8 = 1;
const STATUS_INPUT_FULL: u8 = 2;

/// Scan Code Set 1 make codes for the function keys recognized as "leave
/// this mode" keys throughout `shell.rs` (`term`, `term net`, `nic sniff`,
/// `chat`, `tape load net`, ...). Function keys sit at the same physical
/// position and produce the same scancode on every keyboard layout this
/// project targets (US, UK, Nordic, ...): unlike letter/symbol keys, they
/// are never reached through Shift or AltGr. F12 was the original choice
/// for exactly that reason, but some terminal emulators and KVMs
/// intercept F12 themselves (for their own menus/binds) before it ever
/// reaches a guest, so several other function keys are recognized too --
/// whichever one actually reaches the guest works, with no need to
/// remember which; F1/F2/F3/F5/F9/F11 are left free since some terminals
/// use those for other emulator-level bindings, and F12 stays first in
/// this list since it remains the documented, most-mentioned choice.
const ESCAPE_SCANCODES: [u8; 6] = [
    0x58, // F12
    0x3e, // F4
    0x40, // F6
    0x41, // F7
    0x42, // F8
    0x44, // F10
];

/// One decoded keyboard event as seen by `term`'s raw pass-through loop.
pub enum KeyEvent {
    /// An ordinary key was pressed; forward this byte to the remote end.
    Char(u8),
    /// One of `ESCAPE_SCANCODES` was pressed: stop forwarding and return
    /// to the fos shell.
    Escape,
}

pub struct Keyboard {
    left_shift: bool,
    right_shift: bool,
}

/// Shift-key decoding state for scancodes drained from the IRQ1 queue.
/// Single-threaded and only ever touched by `pop_char`, which callers use
/// from the main shell loop (not from interrupt context).
static mut DECODER: Keyboard = Keyboard {
    left_shift: false,
    right_shift: false,
};

impl Keyboard {
    /// Enables scanning on the PS/2 keyboard. Must run before
    /// `interrupts::initialize` unmasks IRQ1, since this handshake polls the
    /// controller directly rather than going through the interrupt queue.
    pub fn initialize() -> Option<()> {
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
        Some(())
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

/// Reads one raw scancode directly from the controller. Called only from the
/// IRQ1 handler, which already knows a byte is waiting.
pub fn read_pending_scancode() -> Option<u8> {
    if unsafe { inb(STATUS_PORT) } & STATUS_OUTPUT_FULL == 0 {
        return None;
    }
    Some(unsafe { inb(DATA_PORT) })
}

/// Drains one interrupt-queued scancode and decodes it to ASCII, tracking
/// shift state across calls. Safe to call repeatedly from the shell's main
/// loop; briefly disables interrupts internally (via `interrupts::pop_scancode`)
/// to avoid racing the IRQ1 handler.
pub fn pop_char() -> Option<u8> {
    let scancode = interrupts::pop_scancode()?;
    #[allow(static_mut_refs)]
    unsafe {
        DECODER.decode_scancode(scancode)
    }
}

/// Drains one interrupt-queued scancode for `term`'s raw pass-through loop.
/// Recognizes any of `ESCAPE_SCANCODES` ("leave raw mode" make codes)
/// before decoding, so they are never forwarded as characters; every
/// other key goes through the same shift-aware decoder as `pop_char`, so
/// typed text still reaches the remote end as ordinary ASCII.
pub fn poll_term_event() -> Option<KeyEvent> {
    let scancode = interrupts::pop_scancode()?;
    let released = scancode & 0x80 != 0;
    if ESCAPE_SCANCODES.contains(&(scancode & 0x7f)) {
        return if released { None } else { Some(KeyEvent::Escape) };
    }
    #[allow(static_mut_refs)]
    let decoded = unsafe { DECODER.decode_scancode(scancode) };
    decoded.map(KeyEvent::Char)
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
