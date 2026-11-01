//! `ebb`: a small full-screen text editor, the embedded cousin of `ebba`.
//!
//! This library is the whole editor minus the tape ABI: keys and mouse events go in, a grid of
//! characters and attributes comes out, and `main.rs` only moves bytes between that and the
//! kernel. That keeps it testable on the host. The text engine is `edit-core`.
#![cfg_attr(not(test), no_std)]

#[cfg(test)]
extern crate std;

use core::fmt::Write;
use edit_core::{Doc, layout};

// ---- keys ---------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    Backspace,
    Tab,
    BackTab,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Delete,
    Insert,
    /// Ctrl plus a letter, as the control byte (Ctrl-A is 1).
    Ctrl(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyEvent {
    pub key: Key,
    pub shift: bool,
    pub ctrl: bool,
}

/// Turns the bytes `read_key` delivers into keys. The local keyboard sends plain cursor
/// sequences and UTF-8 for the Nordic letters; a terminal over serial or `term net` also sends
/// modifiers (`ESC [ 1 ; 2 C` is Shift-Right, `ESC [ 1 ; 5 C` Ctrl-Right), which are kept.
pub struct Decoder {
    state: u8,
    number: u16,
    first: u16,
    utf8: u32,
    utf8_left: u8,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    pub const fn new() -> Self {
        Self { state: 0, number: 0, first: 0, utf8: 0, utf8_left: 0 }
    }

    fn plain(key: Key) -> Option<KeyEvent> {
        Some(KeyEvent { key, shift: false, ctrl: false })
    }

    pub fn feed(&mut self, byte: u8) -> Option<KeyEvent> {
        match self.state {
            0 => {
                if self.utf8_left > 0 {
                    if byte & 0xc0 == 0x80 {
                        self.utf8 = self.utf8 << 6 | u32::from(byte & 0x3f);
                        self.utf8_left -= 1;
                        if self.utf8_left == 0 {
                            return char::from_u32(self.utf8).and_then(|c| Self::plain(Key::Char(c)));
                        }
                        return None;
                    }
                    self.utf8_left = 0;
                }
                match byte {
                    0x1b => {
                        self.state = 1;
                        None
                    }
                    b'\r' | b'\n' => Self::plain(Key::Enter),
                    8 | 127 => Self::plain(Key::Backspace),
                    b'\t' => Self::plain(Key::Tab),
                    1..=26 => Self::plain(Key::Ctrl(byte)),
                    32..=126 => Self::plain(Key::Char(byte as char)),
                    0xc2..=0xdf => {
                        (self.utf8, self.utf8_left) = (u32::from(byte & 0x1f), 1);
                        None
                    }
                    0xe0..=0xef => {
                        (self.utf8, self.utf8_left) = (u32::from(byte & 0x0f), 2);
                        None
                    }
                    0xf0..=0xf4 => {
                        (self.utf8, self.utf8_left) = (u32::from(byte & 0x07), 3);
                        None
                    }
                    _ => None,
                }
            }
            1 => {
                self.state = 0;
                match byte {
                    b'[' => {
                        (self.state, self.number, self.first) = (2, 0, 0);
                        None
                    }
                    b'O' => {
                        self.state = 3;
                        None
                    }
                    _ => self.feed(byte),
                }
            }
            2 => match byte {
                b'0'..=b'9' => {
                    self.number = self.number.saturating_mul(10).saturating_add(u16::from(byte - b'0'));
                    None
                }
                b';' => {
                    (self.first, self.number) = (self.number, 0);
                    None
                }
                _ => {
                    self.state = 0;
                    let (code, modifier) = if self.first != 0 { (self.first, self.number) } else { (self.number, 1) };
                    let modifier = modifier.max(1) - 1;
                    let (shift, ctrl) = (modifier & 1 != 0, modifier & 4 != 0);
                    let key = match byte {
                        b'A' => Key::Up,
                        b'B' => Key::Down,
                        b'C' => Key::Right,
                        b'D' => Key::Left,
                        b'H' => Key::Home,
                        b'F' => Key::End,
                        b'Z' => Key::BackTab,
                        b'~' => match code {
                            1 | 7 => Key::Home,
                            2 => Key::Insert,
                            3 => Key::Delete,
                            4 | 8 => Key::End,
                            5 => Key::PageUp,
                            6 => Key::PageDown,
                            _ => return None,
                        },
                        _ => return None,
                    };
                    Some(KeyEvent { key, shift, ctrl })
                }
            },
            _ => {
                self.state = 0;
                let key = match byte {
                    b'A' => Key::Up,
                    b'B' => Key::Down,
                    b'C' => Key::Right,
                    b'D' => Key::Left,
                    b'H' => Key::Home,
                    b'F' => Key::End,
                    _ => return None,
                };
                Self::plain(key)
            }
        }
    }
}

// ---- screen -------------------------------------------------------------------------------

pub const NORMAL: u8 = 0;
/// Line numbers.
pub const DIM: u8 = 1;
/// Selection and the status bar.
pub const REVERSE: u8 = 2;
/// The cell under the mouse pointer.
pub const POINTER: u8 = 3;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cell {
    pub ch: char,
    pub attr: u8,
}

pub const BLANK: Cell = Cell { ch: ' ', attr: NORMAL };

pub struct Grid<'a> {
    pub cols: usize,
    pub rows: usize,
    pub cells: &'a mut [Cell],
}

impl Grid<'_> {
    pub fn clear(&mut self) {
        self.cells[..self.cols * self.rows].fill(BLANK);
    }

    pub fn set(&mut self, col: usize, row: usize, ch: char, attr: u8) {
        if col < self.cols && row < self.rows {
            self.cells[row * self.cols + col] = Cell { ch, attr };
        }
    }

    pub fn text(&mut self, col: usize, row: usize, text: &str, attr: u8) -> usize {
        let mut c = col;
        for ch in text.chars() {
            self.set(c, row, ch, attr);
            c += 1;
        }
        c
    }
}

/// A small `fmt::Write` for status lines.
struct Line {
    bytes: [u8; 160],
    len: usize,
}

impl Write for Line {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for &b in s.as_bytes() {
            if self.len < self.bytes.len() {
                self.bytes[self.len] = b;
                self.len += 1;
            }
        }
        Ok(())
    }
}

impl Line {
    fn new() -> Self {
        Self { bytes: [0; 160], len: 0 }
    }
    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.bytes[..self.len]).unwrap_or("")
    }
}

// ---- the editor ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Save,
    Quit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Key(KeyEvent),
    /// The operator asked the tape to stop (a lone Escape).
    Abort,
    /// Pointer at a text cell; `down` while the left button (or a finger) is held.
    Mouse { col: usize, row: usize, down: bool },
}

pub struct Editor<'a> {
    pub doc: Doc<'a>,
    pub name: [u8; 128],
    pub name_len: usize,
    pub select_mode: bool,
    pub crlf: bool,
    cols: usize,
    rows: usize,
    top: usize,
    quit_armed: bool,
    help: bool,
    dragging: bool,
    hover: Option<(usize, usize)>,
    message: [u8; 96],
    message_len: usize,
}

const HELP: &[&str] = &[
    "ebb - help (any key returns)",
    "",
    "Ctrl-S save          Ctrl-Q quit (twice to discard changes)",
    "Ctrl-Z undo          Ctrl-Y redo",
    "Ctrl-C copy          Ctrl-X cut           Ctrl-V paste",
    "                     (with nothing selected: the whole line)",
    "Ctrl-A select all    Ctrl-T select mode: cursor keys select",
    "Ctrl-E word left     Ctrl-R word right",
    "Home / End           start / end of the screen row",
    "PageUp / PageDown    a screen up / down",
    "Tab                  tab; with lines selected, indent them",
    "Shift-Tab            unindent (terminals: Shift-arrows select)",
    "Mouse                click to place the cursor, drag to select",
    "Escape               quit too, like Ctrl-Q",
    "",
    "A file is read whole into memory and written whole on save.",
];

impl<'a> Editor<'a> {
    pub fn new(doc: Doc<'a>, cols: usize, rows: usize) -> Self {
        Self {
            doc,
            name: [0; 128],
            name_len: 0,
            select_mode: false,
            crlf: false,
            cols,
            rows,
            top: 0,
            quit_armed: false,
            help: false,
            dragging: false,
            hover: None,
            message: [0; 96],
            message_len: 0,
        }
    }

    pub fn set_name(&mut self, name: &[u8]) {
        let n = name.len().min(self.name.len());
        self.name[..n].copy_from_slice(&name[..n]);
        self.name_len = n;
    }

    pub fn name(&self) -> &[u8] {
        &self.name[..self.name_len]
    }

    pub fn say(&mut self, text: &str) {
        let n = text.len().min(self.message.len());
        self.message[..n].copy_from_slice(&text.as_bytes()[..n]);
        self.message_len = n;
    }

    pub fn message(&self) -> &str {
        core::str::from_utf8(&self.message[..self.message_len]).unwrap_or("")
    }

    /// Called by the tape after it tried a save.
    pub fn saved(&mut self, result: Result<usize, &str>) {
        match result {
            Ok(bytes) => {
                self.doc.dirty = false;
                let mut line = Line::new();
                let _ = write!(line, "saved {bytes} bytes");
                self.say(line.as_str());
            }
            Err(why) => {
                let mut line = Line::new();
                let _ = write!(line, "NOT saved: {why}");
                self.say(line.as_str());
            }
        }
    }

    fn gutter(&self) -> usize {
        let mut lines = 1;
        for i in 0..self.doc.buf.len() {
            if self.doc.buf.byte(i) == b'\n' {
                lines += 1;
            }
        }
        let mut digits = 1;
        while lines >= 10 {
            lines /= 10;
            digits += 1;
        }
        (digits + 1).max(4)
    }

    fn text_width(&self) -> usize {
        self.cols.saturating_sub(self.gutter()).max(8)
    }

    fn visible_rows(&self) -> usize {
        self.rows.saturating_sub(1).max(1)
    }

    fn request_quit(&mut self) -> Action {
        if !self.doc.dirty || self.quit_armed {
            return Action::Quit;
        }
        self.quit_armed = true;
        self.say("unsaved changes: Ctrl-Q again discards them, Ctrl-S saves");
        Action::None
    }

    fn after_edit(&mut self) {
        self.select_mode = false;
    }

    pub fn handle(&mut self, event: Event) -> Action {
        if self.help {
            if matches!(event, Event::Key(_)) {
                self.help = false;
            }
            return Action::None;
        }
        match event {
            Event::Abort => self.request_quit(),
            Event::Mouse { col, row, down } => {
                self.mouse(col, row, down);
                Action::None
            }
            Event::Key(key) => {
                if !matches!(key.key, Key::Ctrl(17)) {
                    self.quit_armed = false;
                }
                self.key(key)
            }
        }
    }

    fn mouse(&mut self, col: usize, row: usize, down: bool) {
        self.hover = Some((col, row));
        if !down {
            self.dragging = false;
            return;
        }
        let gutter = self.gutter();
        if row >= self.visible_rows() {
            return;
        }
        let width = self.text_width();
        let tab = self.doc.tab;
        let index = self.top + row;
        let target = layout::row_at(&self.doc.buf, width, tab, index);
        let pos = layout::pos_at_col(&self.doc.buf, tab, &target, col.saturating_sub(gutter));
        self.select_mode = false;
        let extend = self.dragging;
        self.doc.move_to(pos, extend);
        self.dragging = true;
    }

    fn key(&mut self, ev: KeyEvent) -> Action {
        let width = self.text_width();
        let extend = ev.shift || self.select_mode;
        match ev.key {
            Key::Char(c) => {
                if !self.doc.insert_char(c) {
                    self.say("buffer full");
                }
                self.after_edit();
            }
            Key::Enter => {
                self.doc.newline();
                self.after_edit();
            }
            Key::Tab => {
                self.doc.tab_key(false);
                self.after_edit();
            }
            Key::BackTab => {
                self.doc.tab_key(true);
            }
            Key::Backspace => {
                self.doc.backspace();
                self.after_edit();
            }
            Key::Delete => {
                self.doc.delete();
                self.after_edit();
            }
            Key::Insert => {}
            Key::Left if ev.ctrl => self.doc.word_left(extend),
            Key::Right if ev.ctrl => self.doc.word_right(extend),
            Key::Left => self.doc.left(extend),
            Key::Right => self.doc.right(extend),
            Key::Up => self.doc.vertical(width, -1, extend),
            Key::Down => self.doc.vertical(width, 1, extend),
            Key::Home if ev.ctrl => self.doc.doc_start(extend),
            Key::End if ev.ctrl => self.doc.doc_end(extend),
            Key::Home => self.doc.home(width, extend),
            Key::End => self.doc.end(width, extend),
            Key::PageUp => {
                let n = self.visible_rows() as isize - 1;
                self.doc.vertical(width, -n.max(1), extend);
            }
            Key::PageDown => {
                let n = self.visible_rows() as isize - 1;
                self.doc.vertical(width, n.max(1), extend);
            }
            Key::Ctrl(1) => {
                self.doc.select_all();
            }
            Key::Ctrl(3) => {
                if self.doc.copy() {
                    self.say("copied");
                } else {
                    self.say("too much to copy");
                }
                self.select_mode = false;
            }
            Key::Ctrl(24) => {
                if self.doc.cut() {
                    self.say("cut");
                } else {
                    self.say("nothing cut");
                }
                self.after_edit();
            }
            Key::Ctrl(22) => {
                if !self.doc.paste() {
                    self.say("nothing to paste");
                }
                self.after_edit();
            }
            Key::Ctrl(26) => {
                if !self.doc.undo() {
                    self.say("nothing to undo");
                }
            }
            Key::Ctrl(25) => {
                if !self.doc.redo() {
                    self.say("nothing to redo");
                }
            }
            Key::Ctrl(20) => {
                self.select_mode = !self.select_mode;
                self.doc.anchor = self.doc.caret;
            }
            Key::Ctrl(5) => self.doc.word_left(extend),
            Key::Ctrl(18) => self.doc.word_right(extend),
            Key::Ctrl(7) => self.help = true,
            Key::Ctrl(19) => return Action::Save,
            Key::Ctrl(17) => return self.request_quit(),
            Key::Ctrl(_) => {}
        }
        Action::None
    }

    /// Draws the screen into `grid` and returns where the caret is (column, row).
    pub fn render(&mut self, grid: &mut Grid) -> (usize, usize) {
        self.cols = grid.cols;
        self.rows = grid.rows;
        grid.clear();
        if self.help {
            for (i, line) in HELP.iter().enumerate() {
                grid.text(2, i + 1, line, if i == 0 { REVERSE } else { NORMAL });
            }
            return (0, 0);
        }
        let gutter = self.gutter();
        let width = self.text_width();
        let tab = self.doc.tab;
        let visible = self.visible_rows();
        let caret_row = layout::row_of(&self.doc.buf, width, tab, self.doc.caret);
        if caret_row.index < self.top {
            self.top = caret_row.index;
        } else if caret_row.index >= self.top + visible {
            self.top = caret_row.index + 1 - visible;
        }
        let (sel_start, sel_end) = self.doc.selection();
        let len = self.doc.buf.len();
        for row in layout::Rows::new(&self.doc.buf, width, tab).skip(self.top).take(visible) {
            let y = row.index - self.top;
            if row.first {
                let mut n = Line::new();
                let _ = write!(n, "{}", row.line + 1);
                let s = n.as_str();
                grid.text(gutter - 1 - s.len(), y, s, DIM);
            }
            let (mut pos, mut col) = (row.start, 0);
            while pos < row.end {
                let (ch, n) = self.doc.buf.char_at(pos);
                let attr = if pos >= sel_start && pos < sel_end { REVERSE } else { NORMAL };
                if ch == '\t' {
                    let w = tab - col % tab;
                    for i in 0..w {
                        grid.set(gutter + col + i, y, ' ', attr);
                    }
                    col += w;
                } else {
                    let shown = if (ch as u32) < 32 || ch as u32 == 127 { '.' } else { ch };
                    grid.set(gutter + col, y, shown, attr);
                    col += 1;
                }
                pos += n;
            }
            // A selection that runs over the line break shows the break as a reversed blank.
            if !row.wrapped && row.end >= sel_start && row.end < sel_end && row.end < len {
                grid.set(gutter + col, y, ' ', REVERSE);
            }
        }
        if let Some((c, r)) = self.hover {
            if r < visible && c < grid.cols {
                let i = r * grid.cols + c;
                let cell = grid.cells[i];
                grid.cells[i] = Cell { ch: cell.ch, attr: POINTER };
            }
        }
        // Status bar.
        let bar = self.rows - 1;
        for c in 0..grid.cols {
            grid.set(c, bar, ' ', REVERSE);
        }
        let line_start = self.doc.buf.line_start(self.doc.caret);
        let column = layout::col_of(&self.doc.buf, tab, line_start, self.doc.caret) + 1;
        let mut status = Line::new();
        let name = core::str::from_utf8(self.name()).unwrap_or("?");
        let name = if name.is_empty() { "[no name]" } else { name };
        let _ = write!(status, " {}{}  Ln {}, Col {}", name, if self.doc.dirty { "*" } else { "" }, caret_row.line + 1, column);
        if self.select_mode {
            let _ = write!(status, "  SELECT");
        }
        let _ = write!(status, "  ");
        let text = if self.message_len > 0 { self.message() } else { "Ctrl-G help  Ctrl-S save  Ctrl-Q quit" };
        let _ = write!(status, "{text}");
        grid.text(0, bar, status.as_str(), REVERSE);
        let col = layout::col_of(&self.doc.buf, tab, caret_row.start, self.doc.caret);
        (gutter + col, caret_row.index - self.top)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use edit_core::Buffer;

    fn key(k: Key) -> Event {
        Event::Key(KeyEvent { key: k, shift: false, ctrl: false })
    }

    fn row_text(grid: &Grid, row: usize) -> std::string::String {
        (0..grid.cols).map(|c| grid.cells[row * grid.cols + c].ch).collect()
    }

    macro_rules! editor {
        ($e:ident, $init:expr) => {
            let mut data = [0u8; 1024];
            let mut log = [0u8; 1024];
            let mut clip = [0u8; 128];
            let mut $e = Editor::new(Doc::new(Buffer::with_text(&mut data, $init).unwrap(), &mut log, &mut clip), 40, 6);
        };
    }

    #[test]
    fn decoder_modifiers_and_utf8() {
        let mut d = Decoder::new();
        let mut feed = |bytes: &[u8]| bytes.iter().filter_map(|&b| d.feed(b)).last();
        assert_eq!(feed(b"\x1b[1;2C"), Some(KeyEvent { key: Key::Right, shift: true, ctrl: false }));
        assert_eq!(feed(b"\x1b[1;5D"), Some(KeyEvent { key: Key::Left, shift: false, ctrl: true }));
        assert_eq!(feed(b"\x1b[3~").unwrap().key, Key::Delete);
        assert_eq!(feed("å".as_bytes()).unwrap().key, Key::Char('å'));
        assert_eq!(feed(&[17]).unwrap().key, Key::Ctrl(17));
    }

    #[test]
    fn typing_renders_with_line_numbers() {
        editor!(e, b"");
        for c in "hej åäö".chars() {
            e.handle(key(Key::Char(c)));
        }
        let mut cells = [BLANK; 40 * 6];
        let mut grid = Grid { cols: 40, rows: 6, cells: &mut cells };
        let caret = e.render(&mut grid);
        assert!(row_text(&grid, 0).starts_with("  1 hej åäö"));
        assert_eq!(caret, (4 + 7, 0));
        assert!(row_text(&grid, 5).contains("*"));
    }

    #[test]
    fn quit_needs_confirmation_when_dirty() {
        editor!(e, b"");
        assert_eq!(e.handle(key(Key::Ctrl(17))), Action::Quit);
        e.handle(key(Key::Char('x')));
        assert_eq!(e.handle(key(Key::Ctrl(17))), Action::None);
        assert!(e.message().contains("unsaved"));
        assert_eq!(e.handle(Event::Abort), Action::Quit);
        e.handle(key(Key::Ctrl(17)));
        e.handle(key(Key::Char('y')));
        assert_eq!(e.handle(key(Key::Ctrl(17))), Action::None);
        assert_eq!(e.handle(key(Key::Ctrl(19))), Action::Save);
        e.saved(Ok(2));
        assert_eq!(e.handle(key(Key::Ctrl(17))), Action::Quit);
    }

    #[test]
    fn select_mode_and_mouse() {
        editor!(e, b"hello world\nsecond");
        e.handle(key(Key::Ctrl(20)));
        e.handle(key(Key::Right));
        e.handle(key(Key::Right));
        assert_eq!(e.doc.selection(), (0, 2));
        e.handle(key(Key::Ctrl(20)));
        // Click then drag: gutter is 4 wide.
        e.handle(Event::Mouse { col: 4 + 6, row: 0, down: true });
        assert_eq!(e.doc.caret, 6);
        e.handle(Event::Mouse { col: 4 + 11, row: 0, down: true });
        assert_eq!(e.doc.selection(), (6, 11));
        e.handle(Event::Mouse { col: 4, row: 1, down: false });
        e.handle(Event::Mouse { col: 4 + 2, row: 1, down: true });
        assert_eq!(e.doc.caret, 14);
    }

    #[test]
    fn scrolls_to_keep_caret_visible() {
        editor!(e, b"1\n2\n3\n4\n5\n6\n7\n8\n9");
        e.handle(key(Key::Ctrl(1)));
        e.handle(key(Key::Right));
        e.doc.doc_end(false);
        let mut cells = [BLANK; 40 * 6];
        let mut grid = Grid { cols: 40, rows: 6, cells: &mut cells };
        let (_, y) = e.render(&mut grid);
        assert_eq!(y, 4);
        assert!(row_text(&grid, 4).starts_with("  9 9"));
        assert!(row_text(&grid, 0).starts_with("  5 5"));
    }

    #[test]
    fn help_screen_and_selection_attr() {
        editor!(e, b"abc");
        e.handle(key(Key::Ctrl(1)));
        let mut cells = [BLANK; 40 * 6];
        let mut grid = Grid { cols: 40, rows: 6, cells: &mut cells };
        e.render(&mut grid);
        assert_eq!(grid.cells[4].attr, REVERSE);
        e.handle(key(Key::Ctrl(7)));
        e.render(&mut grid);
        assert!(row_text(&grid, 1).contains("help"));
        e.handle(key(Key::Char('q')));
        e.render(&mut grid);
        assert!(row_text(&grid, 0).contains("abc"));
    }
}
