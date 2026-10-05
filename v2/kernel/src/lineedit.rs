//! The shell's line editor: the keys a person expects at a prompt, for the local keyboard and for a `term net` client
//! alike. Insert and delete anywhere in the line, Left/Right/Home/End, Ctrl-A/E/K/U/W/L/C, and a history of the
//! last sixteen commands on Up/Down (the half-typed line is kept while browsing, and comes back past the newest
//! entry). Tab completion is not done.
//!
//! Input arrives a byte at a time. A `term net` client (and a serial terminal) sends arrow keys as ANSI sequences
//! (`ESC [ A` and so on), which a small state machine here assembles; the local keyboard decoders deliver the same
//! keys as single private codes (`fos_v2_abi::keymap`). Editing is drawn with the sequences the framebuffer console
//! and every terminal understand: carriage return, cursor left/right, erase to the end of the line, clear screen. A
//! change in the middle of the line redraws the line from the prompt; typing at the end just echoes the character.

use fos_v2_abi::keymap::{KEY_DELETE, KEY_DOWN, KEY_END, KEY_HOME, KEY_LEFT, KEY_RIGHT, KEY_UP};

/// The longest command line.
pub const LINE_MAX: usize = 120;
const HISTORY: usize = 16;
const PROMPT: &str = "fos> ";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Escape {
    None,
    /// Saw ESC.
    Esc,
    /// Inside `ESC [` or `ESC O`, collecting a number.
    Csi,
}

struct Editor {
    line: [u8; LINE_MAX],
    length: usize,
    cursor: usize,
    last_was_cr: bool,
    escape: Escape,
    parameter: u16,
    history: [[u8; LINE_MAX]; HISTORY],
    history_lengths: [usize; HISTORY],
    /// How many entries are stored (the newest is at `count - 1`).
    count: usize,
    /// The entry being shown (an index), or None while editing a new line.
    browsing: Option<usize>,
    /// The line that was being typed when browsing began.
    draft: [u8; LINE_MAX],
    draft_length: usize,
}

static mut EDITOR: Editor = Editor {
    line: [0; LINE_MAX],
    length: 0,
    cursor: 0,
    last_was_cr: false,
    escape: Escape::None,
    parameter: 0,
    history: [[0; LINE_MAX]; HISTORY],
    history_lengths: [0; HISTORY],
    count: 0,
    browsing: None,
    draft: [0; LINE_MAX],
    draft_length: 0,
};

fn editor() -> &'static mut Editor {
    unsafe { &mut *(&raw mut EDITOR) }
}

/// A new session starts with an empty line (the history stays).
pub fn reset() {
    let editor = editor();
    editor.length = 0;
    editor.cursor = 0;
    editor.last_was_cr = false;
    editor.escape = Escape::None;
    editor.browsing = None;
}

impl Editor {
    /// Redraws the whole line from the prompt and puts the cursor where it belongs.
    fn redraw(&self) {
        crate::print(format_args!("\r{PROMPT}"));
        if let Ok(text) = core::str::from_utf8(&self.line[..self.length]) {
            crate::print(format_args!("{text}"));
        }
        crate::print(format_args!("\x1b[K"));
        if self.cursor < self.length {
            crate::print(format_args!("\x1b[{}D", self.length - self.cursor));
        }
    }

    fn insert(&mut self, byte: u8) {
        if self.length >= LINE_MAX {
            return;
        }
        let at_end = self.cursor == self.length;
        self.line.copy_within(self.cursor..self.length, self.cursor + 1);
        self.line[self.cursor] = byte;
        self.length += 1;
        self.cursor += 1;
        if at_end {
            crate::print(format_args!("{}", byte as char));
        } else {
            self.redraw();
        }
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let at_end = self.cursor == self.length;
        self.line.copy_within(self.cursor..self.length, self.cursor - 1);
        self.length -= 1;
        self.cursor -= 1;
        if at_end {
            crate::print(format_args!("\x08 \x08"));
        } else {
            self.redraw();
        }
    }

    fn delete(&mut self) {
        if self.cursor < self.length {
            self.line.copy_within(self.cursor + 1..self.length, self.cursor);
            self.length -= 1;
            self.redraw();
        }
    }

    fn left(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            crate::print(format_args!("\x1b[D"));
        }
    }

    fn right(&mut self) {
        if self.cursor < self.length {
            self.cursor += 1;
            crate::print(format_args!("\x1b[C"));
        }
    }

    fn home(&mut self) {
        if self.cursor > 0 {
            crate::print(format_args!("\x1b[{}D", self.cursor));
            self.cursor = 0;
        }
    }

    fn end(&mut self) {
        if self.cursor < self.length {
            crate::print(format_args!("\x1b[{}C", self.length - self.cursor));
            self.cursor = self.length;
        }
    }

    /// Ctrl-K: delete from the cursor to the end.
    fn kill_to_end(&mut self) {
        self.length = self.cursor;
        crate::print(format_args!("\x1b[K"));
    }

    /// Ctrl-U: delete from the start to the cursor.
    fn kill_to_start(&mut self) {
        self.line.copy_within(self.cursor..self.length, 0);
        self.length -= self.cursor;
        self.cursor = 0;
        self.redraw();
    }

    /// Ctrl-W: delete the word before the cursor (and the blanks between it and the cursor).
    fn kill_word(&mut self) {
        let mut start = self.cursor;
        while start > 0 && self.line[start - 1] == b' ' {
            start -= 1;
        }
        while start > 0 && self.line[start - 1] != b' ' {
            start -= 1;
        }
        self.line.copy_within(self.cursor..self.length, start);
        self.length -= self.cursor - start;
        self.cursor = start;
        self.redraw();
    }

    fn set_line(&mut self, text: &[u8]) {
        self.line[..text.len()].copy_from_slice(text);
        self.length = text.len();
        self.cursor = text.len();
        self.redraw();
    }

    /// Up: an older command (the first press keeps what was being typed).
    fn older(&mut self) {
        if self.count == 0 {
            return;
        }
        let index = match self.browsing {
            None => {
                self.draft[..self.length].copy_from_slice(&self.line[..self.length]);
                self.draft_length = self.length;
                self.count - 1
            }
            Some(0) => return,
            Some(index) => index - 1,
        };
        self.browsing = Some(index);
        let (entry, length) = (self.history[index], self.history_lengths[index]);
        self.set_line(&entry[..length]);
    }

    /// Down: a newer command, and after the newest the line that was being typed.
    fn newer(&mut self) {
        let Some(index) = self.browsing else { return };
        if index + 1 < self.count {
            self.browsing = Some(index + 1);
            let (entry, length) = (self.history[index + 1], self.history_lengths[index + 1]);
            self.set_line(&entry[..length]);
        } else {
            self.browsing = None;
            let (draft, length) = (self.draft, self.draft_length);
            self.set_line(&draft[..length]);
        }
    }

    /// Remembers a command that is about to run (not an empty one, nor a repeat of the previous).
    fn remember(&mut self) {
        self.browsing = None;
        if self.length == 0 || (self.count > 0 && self.history_lengths[self.count - 1] == self.length && self.history[self.count - 1][..self.length] == self.line[..self.length]) {
            return;
        }
        if self.count == HISTORY {
            self.history.copy_within(1..HISTORY, 0);
            self.history_lengths.copy_within(1..HISTORY, 0);
            self.count -= 1;
        }
        self.history[self.count][..self.length].copy_from_slice(&self.line[..self.length]);
        self.history_lengths[self.count] = self.length;
        self.count += 1;
    }

    fn csi(&mut self, last: u8) {
        match last {
            b'A' => self.older(),
            b'B' => self.newer(),
            b'C' => self.right(),
            b'D' => self.left(),
            b'H' => self.home(),
            b'F' => self.end(),
            b'~' => match self.parameter {
                1 | 7 => self.home(),
                3 => self.delete(),
                4 | 8 => self.end(),
                _ => {}
            },
            _ => {}
        }
    }
}

/// One typed byte. Returns the finished line (copied into `out`, its length) when Enter was pressed; the caller runs
/// it and prints the next prompt.
pub fn feed(byte: u8, out: &mut [u8; LINE_MAX]) -> Option<usize> {
    let editor = editor();
    // Inside an escape sequence.
    match editor.escape {
        Escape::Esc => {
            editor.escape = Escape::None;
            if byte == b'[' || byte == b'O' {
                editor.escape = Escape::Csi;
                editor.parameter = 0;
                return None;
            }
            // Not a sequence after all: the Escape is dropped and this byte is an ordinary key.
        }
        Escape::Csi => {
            match byte {
                b'0'..=b'9' => editor.parameter = editor.parameter.saturating_mul(10).saturating_add(u16::from(byte - b'0')),
                b';' => editor.parameter = 0,
                last => {
                    editor.escape = Escape::None;
                    editor.csi(last);
                }
            }
            return None;
        }
        Escape::None => {}
    }
    // A CR LF pair from a terminal is one Enter.
    if byte == b'\n' && editor.last_was_cr {
        editor.last_was_cr = false;
        return None;
    }
    editor.last_was_cr = false;
    match byte {
        0x1b => editor.escape = Escape::Esc,
        b'\r' | b'\n' => {
            editor.last_was_cr = byte == b'\r';
            crate::println!();
            editor.remember();
            let length = editor.length;
            out[..length].copy_from_slice(&editor.line[..length]);
            editor.length = 0;
            editor.cursor = 0;
            return Some(length);
        }
        0x08 | 0x7f => editor.backspace(),
        0x01 => editor.home(),
        0x05 => editor.end(),
        0x0b => editor.kill_to_end(),
        0x15 => editor.kill_to_start(),
        0x17 => editor.kill_word(),
        0x0c => {
            crate::print(format_args!("\x1b[2J"));
            editor.redraw();
        }
        0x03 => {
            crate::println!("^C");
            editor.length = 0;
            editor.cursor = 0;
            editor.browsing = None;
            return Some(0);
        }
        KEY_UP => editor.older(),
        KEY_DOWN => editor.newer(),
        KEY_LEFT => editor.left(),
        KEY_RIGHT => editor.right(),
        KEY_HOME => editor.home(),
        KEY_END => editor.end(),
        KEY_DELETE => editor.delete(),
        0x20..=0x7e => editor.insert(byte),
        _ => {}
    }
    None
}
