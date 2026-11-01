//! Wrapped layout. A logical line is cut into screen rows of at most `width` columns; a tab fills
//! to the next multiple of `tab`. Every glyph is one column wide (the console has no wide glyphs).
//! Scans from the start of the text, which is fine for the small files the editor loads to RAM.

use crate::Buffer;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Row {
    /// Index of the row from the top of the text.
    pub index: usize,
    /// Logical (numbered) line, from 0.
    pub line: usize,
    /// First row of its logical line (the line number is shown here).
    pub first: bool,
    pub start: usize,
    /// End of the row's text, before the newline if any.
    pub end: usize,
    /// Where the next row starts, or None for the last row.
    pub next: Option<usize>,
    /// The row ended because the text did not fit, not at a newline or the end.
    pub wrapped: bool,
}

pub struct Rows<'b, 'a> {
    buf: &'b Buffer<'a>,
    width: usize,
    tab: usize,
    start: usize,
    index: usize,
    line: usize,
    first: bool,
    done: bool,
}

fn advance(col: usize, ch: char, tab: usize) -> usize {
    if ch == '\t' { tab - col % tab } else { 1 }
}

impl<'b, 'a> Rows<'b, 'a> {
    pub fn new(buf: &'b Buffer<'a>, width: usize, tab: usize) -> Self {
        Self { buf, width: width.max(tab.max(2)), tab: tab.max(1), start: 0, index: 0, line: 0, first: true, done: false }
    }
}

impl Iterator for Rows<'_, '_> {
    type Item = Row;

    fn next(&mut self) -> Option<Row> {
        if self.done {
            return None;
        }
        let buf = self.buf;
        let len = buf.len();
        let (mut pos, mut col) = (self.start, 0);
        let mut wrapped = false;
        while pos < len {
            let (ch, n) = buf.char_at(pos);
            if ch == '\n' {
                break;
            }
            let w = advance(col, ch, self.tab);
            if col + w > self.width {
                wrapped = true;
                break;
            }
            col += w;
            pos += n;
        }
        if pos >= len && col >= self.width {
            wrapped = true;
        }
        let newline = pos < len && !wrapped;
        let next = if wrapped {
            Some(pos)
        } else if newline {
            Some(pos + 1)
        } else {
            None
        };
        let row = Row { index: self.index, line: self.line, first: self.first, start: self.start, end: pos, next, wrapped };
        match next {
            Some(n) => {
                self.start = n;
                self.first = !wrapped;
                if !wrapped {
                    self.line += 1;
                }
                self.index += 1;
            }
            None => self.done = true,
        }
        Some(row)
    }
}

/// The column of `pos` within the row starting at `row_start`.
pub fn col_of(buf: &Buffer, tab: usize, row_start: usize, pos: usize) -> usize {
    let (mut p, mut col) = (row_start, 0);
    while p < pos && p < buf.len() {
        let (ch, n) = buf.char_at(p);
        col += advance(col, ch, tab.max(1));
        p += n;
    }
    col
}

/// The position in `row` under column `col` (the end of the row when `col` is past it).
pub fn pos_at_col(buf: &Buffer, tab: usize, row: &Row, col: usize) -> usize {
    let (mut p, mut c) = (row.start, 0);
    while p < row.end {
        let (ch, n) = buf.char_at(p);
        let w = advance(c, ch, tab.max(1));
        if c + w > col {
            return p;
        }
        c += w;
        p += n;
    }
    row.end
}

/// The row that holds `pos`. A caret at the end of a wrapped row sits at the start of the next.
pub fn row_of(buf: &Buffer, width: usize, tab: usize, pos: usize) -> Row {
    let mut last = None;
    for row in Rows::new(buf, width, tab) {
        let inside = pos < row.end || (pos == row.end && !row.wrapped);
        if inside {
            return row;
        }
        last = Some(row);
    }
    last.unwrap_or(Row { index: 0, line: 0, first: true, start: 0, end: 0, next: None, wrapped: false })
}

/// The row with the given index (the last row when it is past the end).
pub fn row_at(buf: &Buffer, width: usize, tab: usize, index: usize) -> Row {
    let mut last = None;
    for row in Rows::new(buf, width, tab) {
        if row.index == index {
            return row;
        }
        last = Some(row);
    }
    last.unwrap()
}

pub fn row_count(buf: &Buffer, width: usize, tab: usize) -> usize {
    Rows::new(buf, width, tab).count()
}
