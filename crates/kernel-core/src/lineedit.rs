//! The shell's line editor: the keys a person expects at a prompt, for the local keyboard and for a `term net` client
//! alike. Insert and delete anywhere in the line, Left/Right/Home/End, Ctrl-A/E/K/U/W/L/C, and a history of the
//! last thirty-two commands on Up/Down (the half-typed line is kept while browsing, and comes back past the newest
//! entry). Tab completes the word at the cursor through a hook the port installs (`set_completer`): one match is
//! filled in, several are filled in as far as they agree, and a second Tab lists them.
//!
//! Input arrives a byte at a time. A `term net` client (and a serial terminal) sends arrow keys as ANSI sequences
//! (`ESC [ A` and so on), which a small state machine here assembles; the local keyboard decoders deliver the same
//! keys as single private codes (`fosfor_abi::keymap`). Editing is drawn with the sequences the framebuffer console
//! and every terminal understand: carriage return, cursor left/right, erase to the end of the line, clear screen. A
//! change in the middle of the line redraws the line from the prompt; typing at the end just echoes the character.
//!
//! å ä ö Å Ä Ö: from a local Nordic keyboard (private codes) or as UTF-8 from a terminal. The line holds one byte per
//! character (Latin-1, so every character is one cursor step) and `feed` hands the finished line back as UTF-8.

use fosfor_abi::keymap::{utf8_sequence, KEY_DELETE, KEY_DOWN, KEY_END, KEY_HOME, KEY_LEFT, KEY_RIGHT, KEY_UP};

/// The longest command line.
pub const LINE_MAX: usize = 240;
const HISTORY: usize = 32;
const PROMPT: &str = "fos> ";

/// Offers every possible completion of the last word of the text so far (UTF-8, up to the cursor) by calling `add` with the
/// whole word each time (a folder ends in `/`).
pub type Completer = fn(line: &str, add: &mut dyn FnMut(&str));

/// How many candidates one Tab considers, and how many bytes of names they may take together.
const CANDIDATES: usize = 96;
const CANDIDATE_BYTES: usize = 3072;

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
    /// The last key was a Tab that completed nothing more: a second one lists the candidates.
    tab_armed: bool,
    completer: Option<Completer>,
    escape: Escape,
    parameter: u16,
    /// A UTF-8 lead byte (0xc2/0xc3) waiting for its second byte.
    lead: u8,
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

/// How many editors exist: one per shell session (the server kernel's console and its terminal sessions; the embedded
/// kernels only use the first).
pub const EDITORS: usize = 5;

const EMPTY: Editor = Editor {
    line: [0; LINE_MAX],
    length: 0,
    cursor: 0,
    last_was_cr: false,
    tab_armed: false,
    completer: None,
    escape: Escape::None,
    parameter: 0,
    lead: 0,
    history: [[0; LINE_MAX]; HISTORY],
    history_lengths: [0; HISTORY],
    count: 0,
    browsing: None,
    draft: [0; LINE_MAX],
    draft_length: 0,
};

static mut EDITORS_STATE: [Editor; EDITORS] = [EMPTY; EDITORS];
static mut SELECTED: usize = 0;

fn editor() -> &'static mut Editor {
    unsafe { &mut (*(&raw mut EDITORS_STATE))[SELECTED] }
}

/// Makes editor `index` (line, cursor, history) the one `feed` and `reset` work on. Call it before each byte when several
/// sessions share the shell; it is a plain index, so the caller must be the only one typing at that moment.
pub fn select(index: usize) {
    unsafe { SELECTED = index.min(EDITORS - 1) };
}

/// Installs the Tab completion hook (for every editor).
pub fn set_completer(completer: Completer) {
    for editor in unsafe { &mut *(&raw mut EDITORS_STATE) }.iter_mut() {
        editor.completer = Some(completer);
    }
}

/// Whether the selected editor's line is empty (Ctrl-D on an empty line means end of input).
pub fn is_empty() -> bool {
    editor().length == 0
}

/// A new session starts with an empty line (the history stays).
pub fn reset() {
    let editor = editor();
    editor.length = 0;
    editor.cursor = 0;
    editor.last_was_cr = false;
    editor.escape = Escape::None;
    editor.lead = 0;
    editor.browsing = None;
}

/// The Latin-1 code of a Nordic letter's private key code.
fn latin1_of_key(key: u8) -> Option<u8> {
    let text = core::str::from_utf8(utf8_sequence(key)?).ok()?;
    u8::try_from(u32::from(text.chars().next()?)).ok()
}

impl Editor {
    /// Redraws the whole line from the prompt and puts the cursor where it belongs.
    fn redraw(&self) {
        crate::print(format_args!("\r{PROMPT}"));
        for &byte in &self.line[..self.length] {
            crate::print(format_args!("{}", byte as char));
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

    /// Tab: completes the word before the cursor.
    fn complete(&mut self, armed: bool) {
        let Some(completer) = self.completer else { return };
        // The line is Latin-1, one byte per character; the hook wants UTF-8.
        let mut text = [0u8; LINE_MAX * 2];
        let mut used = 0;
        for &byte in &self.line[..self.cursor] {
            used += char::from(byte).encode_utf8(&mut text[used..]).len();
        }
        let Ok(text) = core::str::from_utf8(&text[..used]) else { return };
        let word = &text[text.rfind(' ').map_or(0, |at| at + 1)..];
        let mut store = [0u8; CANDIDATE_BYTES];
        let mut spans = [(0usize, 0usize); CANDIDATES];
        let (mut bytes, mut count) = (0, 0);
        completer(text, &mut |name| {
            let fits = count < CANDIDATES && bytes + name.len() <= CANDIDATE_BYTES;
            if !fits || !name.starts_with(word) || spans[..count].iter().any(|&(at, length)| &store[at..at + length] == name.as_bytes()) {
                return;
            }
            store[bytes..bytes + name.len()].copy_from_slice(name.as_bytes());
            spans[count] = (bytes, name.len());
            bytes += name.len();
            count += 1;
        });
        if count == 0 {
            return;
        }
        let names = &mut spans[..count];
        names.sort_unstable_by(|a, b| store[a.0..a.0 + a.1].cmp(&store[b.0..b.0 + b.1]));
        // As much of the first candidate as every other one shares.
        let first = &store[names[0].0..names[0].0 + names[0].1];
        let mut common = first.len();
        for &(at, length) in names.iter() {
            common = common.min(first.iter().zip(&store[at..at + length]).take_while(|(a, b)| a == b).count());
        }
        let Ok(common) = core::str::from_utf8(&first[..common]).or_else(|error| core::str::from_utf8(&first[..error.valid_up_to()])) else { return };
        let extra = &common[word.len()..];
        if !extra.is_empty() || count == 1 {
            for character in extra.chars() {
                match u8::try_from(u32::from(character)) {
                    Ok(byte) => self.insert(byte),
                    Err(_) => return,
                }
            }
            if count == 1 && !common.ends_with('/') {
                self.insert(b' ');
            }
        } else if !armed {
            self.tab_armed = true;
        } else {
            crate::println!();
            let mut column = 0;
            for &(at, length) in names.iter() {
                let name = core::str::from_utf8(&store[at..at + length]).unwrap_or("?");
                // Only the last path component is new information (a folder keeps its slash).
                let shown = match name.trim_end_matches('/').rfind('/') {
                    Some(slash) => &name[slash + 1..],
                    None => name,
                };
                if column > 0 && column + shown.chars().count() + 2 > 78 {
                    crate::println!();
                    column = 0;
                }
                crate::print(format_args!("{shown}  "));
                column += shown.chars().count() + 2;
            }
            crate::println!();
            self.redraw();
        }
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
    // The second byte of a UTF-8 å ä ö: 0xc3 0xa5 is U+00E5, one Latin-1 character.
    let lead = core::mem::take(&mut editor.lead);
    if lead != 0 && (0x80..=0xbf).contains(&byte) {
        editor.insert((lead << 6) | (byte & 0x3f));
        return None;
    }
    // A CR LF pair from a terminal is one Enter.
    if byte == b'\n' && editor.last_was_cr {
        editor.last_was_cr = false;
        return None;
    }
    editor.last_was_cr = false;
    let armed = core::mem::take(&mut editor.tab_armed);
    match byte {
        0x09 => editor.complete(armed),
        0x1b => editor.escape = Escape::Esc,
        b'\r' | b'\n' => {
            editor.last_was_cr = byte == b'\r';
            crate::println!();
            editor.remember();
            let mut length = 0;
            for &byte in &editor.line[..editor.length] {
                if byte < 0x80 {
                    out[length] = byte;
                    length += 1;
                } else if length + 2 <= LINE_MAX {
                    out[length] = 0xc0 | (byte >> 6);
                    out[length + 1] = 0x80 | (byte & 0x3f);
                    length += 2;
                }
            }
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
        0xc2 | 0xc3 => editor.lead = byte & 3,
        _ => {
            if let Some(letter) = latin1_of_key(byte) {
                editor.insert(letter);
            }
        }
    }
    None
}
