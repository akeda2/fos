//! The document: text, caret and selection, undo and the clipboard. All movement and editing the
//! editor offers is here so it can be tested on the host; the tape only maps keys to these calls
//! and draws the result.

use crate::history::{DELETE, History, INSERT};
use crate::layout::{self, Rows};
use crate::Buffer;

pub struct Doc<'a> {
    pub buf: Buffer<'a>,
    hist: History<'a>,
    clip: &'a mut [u8],
    clip_len: usize,
    /// The clipboard holds whole lines (cut or copied with no selection) and pastes above the caret's line.
    clip_lines: bool,
    pub caret: usize,
    pub anchor: usize,
    want_col: Option<usize>,
    /// The end of the last typed character, so the next one joins its undo step.
    typing_end: Option<usize>,
    pub dirty: bool,
    pub tab: usize,
}

impl<'a> Doc<'a> {
    pub fn new(buf: Buffer<'a>, log: &'a mut [u8], clip: &'a mut [u8]) -> Self {
        Self {
            buf,
            hist: History::new(log),
            clip,
            clip_len: 0,
            clip_lines: false,
            caret: 0,
            anchor: 0,
            want_col: None,
            typing_end: None,
            dirty: false,
            tab: 4,
        }
    }

    pub fn has_selection(&self) -> bool {
        self.caret != self.anchor
    }

    pub fn selection(&self) -> (usize, usize) {
        (self.caret.min(self.anchor), self.caret.max(self.anchor))
    }

    pub fn clipboard(&self) -> &[u8] {
        &self.clip[..self.clip_len]
    }

    pub fn can_undo(&self) -> bool {
        self.hist.can_undo()
    }

    pub fn can_redo(&self) -> bool {
        self.hist.can_redo()
    }

    fn place(&mut self, pos: usize, extend: bool) {
        self.caret = pos.min(self.buf.len());
        if !extend {
            self.anchor = self.caret;
        }
        self.typing_end = None;
    }

    // ---- movement ------------------------------------------------------------------------

    pub fn move_to(&mut self, pos: usize, extend: bool) {
        self.want_col = None;
        self.place(pos, extend);
    }

    pub fn left(&mut self, extend: bool) {
        // Without extending, a selection collapses to its start.
        let target = if self.has_selection() && !extend { self.selection().0 } else { self.buf.prev_char(self.caret) };
        self.move_to(target, extend);
    }

    pub fn right(&mut self, extend: bool) {
        let target = if self.has_selection() && !extend { self.selection().1 } else { self.buf.next_char(self.caret) };
        self.move_to(target, extend);
    }

    fn is_word(ch: char) -> bool {
        ch.is_alphanumeric() || ch == '_'
    }

    pub fn word_left(&mut self, extend: bool) {
        let mut p = self.caret;
        while p > 0 && !Self::is_word(self.buf.char_at(self.buf.prev_char(p)).0) {
            p = self.buf.prev_char(p);
        }
        while p > 0 && Self::is_word(self.buf.char_at(self.buf.prev_char(p)).0) {
            p = self.buf.prev_char(p);
        }
        self.move_to(p, extend);
    }

    pub fn word_right(&mut self, extend: bool) {
        let mut p = self.caret;
        while p < self.buf.len() && !Self::is_word(self.buf.char_at(p).0) {
            p = self.buf.next_char(p);
        }
        while p < self.buf.len() && Self::is_word(self.buf.char_at(p).0) {
            p = self.buf.next_char(p);
        }
        self.move_to(p, extend);
    }

    /// Start of the screen row holding the caret.
    pub fn home(&mut self, width: usize, extend: bool) {
        let row = layout::row_of(&self.buf, width, self.tab, self.caret);
        self.move_to(row.start, extend);
    }

    pub fn end(&mut self, width: usize, extend: bool) {
        let row = layout::row_of(&self.buf, width, self.tab, self.caret);
        self.move_to(row.end, extend);
    }

    pub fn doc_start(&mut self, extend: bool) {
        self.move_to(0, extend);
    }

    pub fn doc_end(&mut self, extend: bool) {
        self.move_to(self.buf.len(), extend);
    }

    /// Moves `rows` screen rows down (negative: up), keeping the column the caret first had.
    pub fn vertical(&mut self, width: usize, rows: isize, extend: bool) {
        let row = layout::row_of(&self.buf, width, self.tab, self.caret);
        let col = match self.want_col {
            Some(c) => c,
            None => layout::col_of(&self.buf, self.tab, row.start, self.caret),
        };
        let total = layout::row_count(&self.buf, width, self.tab) as isize;
        let target = (row.index as isize + rows).clamp(0, total - 1) as usize;
        let dest = layout::row_at(&self.buf, width, self.tab, target);
        let pos = layout::pos_at_col(&self.buf, self.tab, &dest, col);
        self.place(pos, extend);
        self.want_col = Some(col);
    }

    pub fn select_all(&mut self) {
        self.anchor = 0;
        self.caret = self.buf.len();
        self.typing_end = None;
    }

    // ---- editing -------------------------------------------------------------------------

    /// Replaces `start..end` with `text`; false when it does not fit. Two records, joined, so one undo.
    fn replace(&mut self, start: usize, end: usize, text: &[u8], joined_to_prev: bool) -> bool {
        if self.buf.len() - (end - start) + text.len() > self.buf.capacity() {
            return false;
        }
        let mut joined = joined_to_prev;
        if end > start {
            let n = end - start;
            if let Some(slot) = self.hist.reserve(DELETE, joined, start, n) {
                self.buf.copy_to(start, slot);
            }
            joined = true;
            self.buf.remove(start, n);
        }
        if !text.is_empty() {
            if let Some(slot) = self.hist.reserve(INSERT, joined, start, text.len()) {
                slot.copy_from_slice(text);
            }
            self.buf.insert(start, text);
        }
        self.dirty = true;
        true
    }

    fn edit(&mut self, start: usize, end: usize, text: &[u8]) -> bool {
        self.typing_end = None;
        self.want_col = None;
        if !self.replace(start, end, text, false) {
            return false;
        }
        self.caret = start + text.len();
        self.anchor = self.caret;
        true
    }

    /// Types text at the caret, replacing the selection. A run of single plain characters is one undo step.
    pub fn insert(&mut self, text: &[u8]) -> bool {
        let (s, e) = self.selection();
        let plain = text.len() <= 4 && !text.contains(&b'\n') && e == s;
        let join = plain && self.typing_end == Some(s);
        self.want_col = None;
        if !self.replace(s, e, text, join) {
            return false;
        }
        self.caret = s + text.len();
        self.anchor = self.caret;
        self.typing_end = if plain { Some(self.caret) } else { None };
        true
    }

    pub fn insert_char(&mut self, ch: char) -> bool {
        let mut raw = [0u8; 4];
        self.insert(ch.encode_utf8(&mut raw).as_bytes())
    }

    /// A newline that keeps the indentation of the current line.
    pub fn newline(&mut self) -> bool {
        let (s, _) = self.selection();
        let ls = self.buf.line_start(s);
        let mut indent = [0u8; 64];
        indent[0] = b'\n';
        let mut n = 1;
        let mut p = ls;
        while p < s && n < indent.len() && matches!(self.buf.byte(p), b' ' | b'\t') {
            indent[n] = self.buf.byte(p);
            n += 1;
            p += 1;
        }
        self.insert(&indent[..n])
    }

    pub fn backspace(&mut self) -> bool {
        if self.has_selection() {
            let (s, e) = self.selection();
            return self.edit(s, e, b"");
        }
        if self.caret == 0 {
            return false;
        }
        let from = self.buf.prev_char(self.caret);
        self.edit(from, self.caret, b"")
    }

    pub fn delete(&mut self) -> bool {
        if self.has_selection() {
            let (s, e) = self.selection();
            return self.edit(s, e, b"");
        }
        if self.caret >= self.buf.len() {
            return false;
        }
        let to = self.buf.next_char(self.caret);
        self.edit(self.caret, to, b"")
    }

    /// Tab: a tab character, or with a multi-line selection indent every selected line.
    pub fn tab_key(&mut self, unindent: bool) -> bool {
        let (s, e) = self.selection();
        let multi = self.has_selection() && (s..e).any(|p| self.buf.byte(p) == b'\n');
        if !multi {
            if unindent {
                return self.unindent_line(self.caret);
            }
            return self.insert(b"\t");
        }
        self.indent_lines(s, e, unindent)
    }

    fn unindent_line(&mut self, pos: usize) -> bool {
        let ls = self.buf.line_start(pos);
        let mut n = 0;
        if ls < self.buf.len() && self.buf.byte(ls) == b'\t' {
            n = 1;
        } else {
            while n < self.tab && ls + n < self.buf.len() && self.buf.byte(ls + n) == b' ' {
                n += 1;
            }
        }
        if n == 0 {
            return false;
        }
        let caret = self.caret;
        self.edit(ls, ls + n, b"");
        self.move_to(caret.saturating_sub(n).max(ls), false);
        true
    }

    fn indent_lines(&mut self, s: usize, e: usize, unindent: bool) -> bool {
        // A selection ending at a line start does not include that line.
        let e = if e > s && self.buf.byte(e - 1) == b'\n' { e - 1 } else { e };
        let first = self.buf.line_start(s);
        // Work from the last line up so earlier positions stay valid.
        let mut line = self.buf.line_start(e);
        let mut joined = false;
        let mut total: isize = 0;
        loop {
            if unindent {
                let mut n = 0;
                if line < self.buf.len() && self.buf.byte(line) == b'\t' {
                    n = 1;
                } else {
                    while n < self.tab && line + n < self.buf.len() && self.buf.byte(line + n) == b' ' {
                        n += 1;
                    }
                }
                if n > 0 {
                    self.replace(line, line + n, b"", joined);
                    joined = true;
                    total -= n as isize;
                }
            } else if self.replace(line, line, b"\t", joined) {
                joined = true;
                total += 1;
            }
            if line <= first {
                break;
            }
            line = self.buf.line_start(line - 1);
        }
        self.typing_end = None;
        let end = (e as isize + total).max(first as isize) as usize;
        self.anchor = first;
        self.caret = end;
        total != 0
    }

    // ---- clipboard -----------------------------------------------------------------------

    fn store(&mut self, start: usize, end: usize, lines: bool) -> bool {
        let n = end - start;
        if n > self.clip.len() {
            return false;
        }
        self.buf.copy_to(start, &mut self.clip[..n]);
        self.clip_len = n;
        self.clip_lines = lines;
        true
    }

    /// The range copy and cut work on: the selection, else the whole caret line with its newline.
    fn clip_range(&self) -> (usize, usize, bool) {
        if self.has_selection() {
            let (s, e) = self.selection();
            return (s, e, false);
        }
        let s = self.buf.line_start(self.caret);
        let e = (self.buf.line_end(self.caret) + 1).min(self.buf.len());
        (s, e, true)
    }

    pub fn copy(&mut self) -> bool {
        let (s, e, lines) = self.clip_range();
        self.store(s, e, lines)
    }

    pub fn cut(&mut self) -> bool {
        let (s, e, lines) = self.clip_range();
        if s == e || !self.store(s, e, lines) {
            return false;
        }
        self.edit(s, e, b"")
    }

    pub fn paste(&mut self) -> bool {
        if self.clip_len == 0 {
            return false;
        }
        let (n, lines) = (self.clip_len, self.clip_lines);
        if lines && !self.has_selection() {
            let ls = self.buf.line_start(self.caret);
            let caret_offset = self.caret - ls;
            // A last line without a newline needs one so the pasted line stays a line.
            let ok = self.insert_clip(ls, ls, n);
            if ok {
                self.caret = ls + n + caret_offset;
                if self.caret > self.buf.len() {
                    self.caret = self.buf.len();
                }
                self.anchor = self.caret;
            }
            return ok;
        }
        let (s, e) = self.selection();
        self.insert_clip(s, e, n)
    }

    fn insert_clip(&mut self, s: usize, e: usize, n: usize) -> bool {
        // The clipboard slice is not borrowed across `edit`, so stage through the history-free path.
        if self.buf.len() - (e - s) + n > self.buf.capacity() {
            return false;
        }
        self.typing_end = None;
        self.want_col = None;
        let mut joined = false;
        if e > s {
            if let Some(slot) = self.hist.reserve(DELETE, false, s, e - s) {
                self.buf.copy_to(s, slot);
            }
            self.buf.remove(s, e - s);
            joined = true;
        }
        if let Some(slot) = self.hist.reserve(INSERT, joined, s, n) {
            slot.copy_from_slice(&self.clip[..n]);
        }
        self.buf.insert(s, &self.clip[..n]);
        self.dirty = true;
        self.caret = s + n;
        self.anchor = self.caret;
        true
    }

    // ---- undo ----------------------------------------------------------------------------

    pub fn undo(&mut self) -> bool {
        let mut any = false;
        while let Some(rec) = self.hist.pop() {
            any = true;
            let (pos, len) = (rec.pos, rec.len);
            if rec.kind == INSERT {
                self.buf.remove(pos, len);
                self.caret = pos;
            } else {
                let data = self.hist.data(&rec);
                self.buf.insert(pos, data);
                self.caret = pos + len;
            }
            if !rec.joined {
                break;
            }
        }
        if any {
            self.anchor = self.caret;
            self.typing_end = None;
            self.want_col = None;
            self.dirty = true;
        }
        any
    }

    pub fn redo(&mut self) -> bool {
        let mut any = false;
        loop {
            let Some(rec) = self.hist.advance() else { break };
            any = true;
            let (pos, len) = (rec.pos, rec.len);
            if rec.kind == INSERT {
                let data = self.hist.data(&rec);
                self.buf.insert(pos, data);
                self.caret = pos + len;
            } else {
                self.buf.remove(pos, len);
                self.caret = pos;
            }
            if !self.hist.next_joined() {
                break;
            }
        }
        if any {
            self.anchor = self.caret;
            self.typing_end = None;
            self.want_col = None;
            self.dirty = true;
        }
        any
    }

    /// The number of screen rows (for scrolling).
    pub fn rows(&self, width: usize) -> usize {
        Rows::new(&self.buf, width, self.tab).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(doc: &Doc) -> std::string::String {
        let (a, b) = doc.buf.as_slices();
        let mut v = std::vec::Vec::from(a);
        v.extend_from_slice(b);
        std::string::String::from_utf8_lossy(&v).into_owned()
    }

    macro_rules! doc {
        ($d:ident, $init:expr) => {
            let mut data = [0u8; 256];
            let mut log = [0u8; 512];
            let mut clip = [0u8; 64];
            #[allow(unused_mut)]
            let mut $d = Doc::new(Buffer::with_text(&mut data, $init).unwrap(), &mut log, &mut clip);
        };
    }

    #[test]
    fn typing_and_undo_groups() {
        doc!(d, b"");
        for c in "hello".chars() {
            d.insert_char(c);
        }
        d.newline();
        d.insert_char('x');
        assert_eq!(text(&d), "hello\nx");
        assert!(d.undo());
        assert_eq!(text(&d), "hello\n");
        assert!(d.undo());
        assert_eq!(text(&d), "hello");
        assert!(d.undo());
        assert_eq!(text(&d), "");
        assert!(d.redo());
        assert_eq!(text(&d), "hello");
        assert!(d.redo() && d.redo());
        assert_eq!(text(&d), "hello\nx");
    }

    #[test]
    fn replace_selection_is_one_undo() {
        doc!(d, b"one two three");
        d.move_to(4, false);
        d.move_to(7, true);
        d.insert(b"2");
        assert_eq!(text(&d), "one 2 three");
        d.undo();
        assert_eq!(text(&d), "one two three");
        assert_eq!(d.caret, 7);
        d.redo();
        assert_eq!(text(&d), "one 2 three");
    }

    #[test]
    fn utf8_moves_and_deletes() {
        doc!(d, "åäö".as_bytes());
        d.doc_end(false);
        d.backspace();
        assert_eq!(text(&d), "åä");
        d.left(false);
        d.delete();
        assert_eq!(text(&d), "å");
    }

    #[test]
    fn latin1_bytes_survive() {
        doc!(d, &[b'a', 0xE5, b'b']);
        assert_eq!(d.buf.char_at(1), ('å', 1));
        d.doc_end(false);
        d.insert_char('c');
        let (a, b) = d.buf.as_slices();
        assert_eq!([a, b].concat(), [b'a', 0xE5, b'b', b'c']);
    }

    #[test]
    fn line_clipboard() {
        doc!(d, b"aa\nbb\ncc");
        d.move_to(4, false);
        assert!(d.cut());
        assert_eq!(text(&d), "aa\ncc");
        d.doc_start(false);
        assert!(d.paste());
        assert_eq!(text(&d), "bb\naa\ncc");
        d.undo();
        d.undo();
        assert_eq!(text(&d), "aa\nbb\ncc");
    }

    #[test]
    fn selection_clipboard() {
        doc!(d, b"hello world");
        d.move_to(0, false);
        d.move_to(5, true);
        d.copy();
        d.doc_end(false);
        d.paste();
        assert_eq!(text(&d), "hello worldhello");
    }

    #[test]
    fn vertical_keeps_column_and_wraps() {
        doc!(d, b"abcdefghij\nxy\nabcdefghij");
        d.move_to(8, false);
        d.vertical(80, 1, false);
        assert_eq!(d.caret, 13);
        d.vertical(80, 1, false);
        assert_eq!(d.caret, 14 + 8);
        d.vertical(80, -1, false);
        d.vertical(80, -1, false);
        assert_eq!(d.caret, 8);
        // Width 5 wraps the first line into two rows.
        d.move_to(2, false);
        d.vertical(5, 1, false);
        assert_eq!(d.caret, 7);
    }

    #[test]
    fn indent_and_unindent_lines() {
        doc!(d, b"a\nb\nc");
        d.select_all();
        d.tab_key(false);
        assert_eq!(text(&d), "\ta\n\tb\n\tc");
        d.tab_key(true);
        assert_eq!(text(&d), "a\nb\nc");
        d.undo();
        assert_eq!(text(&d), "\ta\n\tb\n\tc");
        d.undo();
        assert_eq!(text(&d), "a\nb\nc");
    }

    #[test]
    fn auto_indent() {
        doc!(d, b"  foo");
        d.doc_end(false);
        d.newline();
        assert_eq!(text(&d), "  foo\n  ");
    }

    #[test]
    fn full_buffer_refuses() {
        let mut data = [0u8; 4];
        let mut log = [0u8; 64];
        let mut clip = [0u8; 4];
        let mut d = Doc::new(Buffer::new(&mut data), &mut log, &mut clip);
        for c in "abcd".chars() {
            assert!(d.insert_char(c));
        }
        assert!(!d.insert_char('e'));
        assert_eq!(text(&d), "abcd");
    }

    #[test]
    fn full_history_is_forgotten_not_fatal() {
        let mut data = [0u8; 200];
        let mut log = [0u8; 40];
        let mut clip = [0u8; 4];
        let mut d = Doc::new(Buffer::new(&mut data), &mut log, &mut clip);
        for c in "abcdefghijkl\n".chars().cycle().take(30) {
            d.insert_char(c);
        }
        assert_eq!(d.buf.len(), 30);
    }

    #[test]
    fn layout_rows() {
        doc!(d, b"abcdef\n\nxy");
        let rows: std::vec::Vec<_> = Rows::new(&d.buf, 4, 4).collect();
        assert_eq!(rows.len(), 4);
        assert!(rows[0].wrapped && !rows[1].first && rows[1].line == 0);
        assert_eq!((rows[2].start, rows[2].end, rows[2].line), (7, 7, 1));
        assert_eq!(rows[3].line, 2);
        let r = layout::row_of(&d.buf, 4, 4, 4);
        assert_eq!(r.index, 1);
    }
}
