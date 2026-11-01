//! A gap buffer over a caller-provided slice: the free space ("gap") sits at the last edit, so typing
//! moves no bytes and a jump moves the gap once.

pub struct Buffer<'a> {
    data: &'a mut [u8],
    gap_start: usize,
    gap_end: usize,
}

impl<'a> Buffer<'a> {
    /// An empty buffer using all of `data`.
    pub fn new(data: &'a mut [u8]) -> Self {
        let end = data.len();
        Self { data, gap_start: 0, gap_end: end }
    }

    /// A buffer holding `text`, or None when `data` is too small for it.
    pub fn with_text(data: &'a mut [u8], text: &[u8]) -> Option<Self> {
        if text.len() > data.len() {
            return None;
        }
        data[..text.len()].copy_from_slice(text);
        let end = data.len();
        Some(Self { data, gap_start: text.len(), gap_end: end })
    }

    /// A buffer whose text is already in the first `len` bytes of `data` (a file read straight into it).
    pub fn adopt(data: &'a mut [u8], len: usize) -> Self {
        let len = len.min(data.len());
        let end = data.len();
        Self { data, gap_start: len, gap_end: end }
    }

    pub fn len(&self) -> usize {
        self.data.len() - (self.gap_end - self.gap_start)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn capacity(&self) -> usize {
        self.data.len()
    }

    /// The byte at `pos` (which must be below `len`).
    pub fn byte(&self, pos: usize) -> u8 {
        if pos < self.gap_start { self.data[pos] } else { self.data[pos + (self.gap_end - self.gap_start)] }
    }

    /// The text as the two slices on either side of the gap, in order.
    pub fn as_slices(&self) -> (&[u8], &[u8]) {
        (&self.data[..self.gap_start], &self.data[self.gap_end..])
    }

    /// The whole text as one slice (moves the gap to the end), for a single file write.
    pub fn contiguous(&mut self) -> &[u8] {
        self.move_gap(self.len());
        &self.data[..self.gap_start]
    }

    fn move_gap(&mut self, pos: usize) {
        let gap = self.gap_end - self.gap_start;
        if pos < self.gap_start {
            let count = self.gap_start - pos;
            self.data.copy_within(pos..self.gap_start, self.gap_end - count);
            self.gap_start = pos;
            self.gap_end = pos + gap;
        } else if pos > self.gap_start {
            let count = pos - self.gap_start;
            self.data.copy_within(self.gap_end..self.gap_end + count, self.gap_start);
            self.gap_start += count;
            self.gap_end += count;
        }
    }

    /// Inserts `text` at `pos`; false (nothing changed) when it does not fit.
    pub fn insert(&mut self, pos: usize, text: &[u8]) -> bool {
        if pos > self.len() || text.len() > self.gap_end - self.gap_start {
            return false;
        }
        self.move_gap(pos);
        self.data[pos..pos + text.len()].copy_from_slice(text);
        self.gap_start += text.len();
        true
    }

    /// Removes `count` bytes at `pos` (clamped to the end).
    pub fn remove(&mut self, pos: usize, count: usize) {
        let count = count.min(self.len().saturating_sub(pos));
        self.move_gap(pos);
        self.gap_end += count;
    }

    /// Copies the bytes from `start` into `out` (as many as fit); returns how many.
    pub fn copy_to(&self, start: usize, out: &mut [u8]) -> usize {
        let count = out.len().min(self.len().saturating_sub(start));
        for (i, slot) in out[..count].iter_mut().enumerate() {
            *slot = self.byte(start + i);
        }
        count
    }

    /// The character at `pos` and its length in bytes. An invalid sequence is one Latin-1 character.
    pub fn char_at(&self, pos: usize) -> (char, usize) {
        let first = self.byte(pos);
        if first < 0x80 {
            return (first as char, 1);
        }
        let need = match first {
            0xC2..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF4 => 4,
            _ => return (first as char, 1),
        };
        if pos + need > self.len() {
            return (first as char, 1);
        }
        let mut raw = [0u8; 4];
        self.copy_to(pos, &mut raw[..need]);
        match core::str::from_utf8(&raw[..need]) {
            Ok(text) => (text.chars().next().unwrap_or(first as char), need),
            Err(_) => (first as char, 1),
        }
    }

    /// The start of the character that ends at `pos`.
    pub fn prev_char(&self, pos: usize) -> usize {
        if pos == 0 {
            return 0;
        }
        let mut start = pos - 1;
        let mut back = 0;
        while start > 0 && back < 3 && self.byte(start) & 0xC0 == 0x80 {
            start -= 1;
            back += 1;
        }
        if start + self.char_at(start).1 == pos { start } else { pos - 1 }
    }

    pub fn next_char(&self, pos: usize) -> usize {
        if pos >= self.len() { self.len() } else { pos + self.char_at(pos).1 }
    }

    /// Start of the logical line (after the previous newline) containing `pos`.
    pub fn line_start(&self, pos: usize) -> usize {
        let mut p = pos.min(self.len());
        while p > 0 && self.byte(p - 1) != b'\n' {
            p -= 1;
        }
        p
    }

    /// End of the logical line containing `pos` (the newline's position, or the end of the text).
    pub fn line_end(&self, pos: usize) -> usize {
        let mut p = pos.min(self.len());
        while p < self.len() && self.byte(p) != b'\n' {
            p += 1;
        }
        p
    }
}
