#![no_std]

/// Errors returned by the editor's fixed-capacity text buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BufferError {
    Full,
    InvalidLine,
}

/// A newline-delimited text buffer backed by caller-provided storage.
///
/// Lines are numbered from one. Input is normalized to LF line endings when
/// loaded; newly inserted lines always use one trailing LF byte.
pub struct TextBuffer<'a> {
    data: &'a mut [u8],
    len: usize,
}

impl<'a> TextBuffer<'a> {
    pub fn new(data: &'a mut [u8]) -> Self {
        Self { data, len: 0 }
    }

    pub fn load(&mut self, input: &[u8]) -> Result<(), BufferError> {
        let mut destination = 0;
        let mut index = 0;
        while index < input.len() {
            let byte = input[index];
            if byte == b'\r' {
                if index + 1 < input.len() && input[index + 1] == b'\n' {
                    index += 1;
                }
                if destination == self.data.len() {
                    return Err(BufferError::Full);
                }
                self.data[destination] = b'\n';
                destination += 1;
            } else {
                if destination == self.data.len() {
                    return Err(BufferError::Full);
                }
                self.data[destination] = byte;
                destination += 1;
            }
            index += 1;
        }
        self.len = destination;
        Ok(())
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.data[..self.len]
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn line_count(&self) -> usize {
        if self.len == 0 {
            return 0;
        }
        let mut count = 0;
        let mut start = 0;
        while start < self.len {
            let (_, next) = self.bounds_from(start);
            count += 1;
            start = next;
        }
        count
    }

    /// Returns the byte range for one line, including its LF if present.
    pub fn line_bounds(&self, line: usize) -> Option<(usize, usize)> {
        if line == 0 {
            return None;
        }
        let mut current = 1;
        let mut start = 0;
        while start < self.len {
            let (_, next) = self.bounds_from(start);
            if current == line {
                return Some((start, next));
            }
            current += 1;
            start = next;
        }
        None
    }

    /// Returns the content range for one line, excluding its LF.
    pub fn line_content(&self, line: usize) -> Option<&[u8]> {
        let (start, end) = self.line_bounds(line)?;
        let content_end = if end > start && self.data[end - 1] == b'\n' {
            end - 1
        } else {
            end
        };
        Some(&self.data[start..content_end])
    }

    pub fn insert_line(&mut self, line: usize, content: &[u8]) -> Result<(), BufferError> {
        let count = self.line_count();
        if line == 0 || line > count + 1 || content.contains(&b'\n') || content.contains(&b'\r') {
            return Err(BufferError::InvalidLine);
        }
        let offset = if line <= count {
            self.line_bounds(line).unwrap().0
        } else {
            self.len
        };
        let needs_separator = offset > 0 && self.data[offset - 1] != b'\n';
        let extra = usize::from(needs_separator) + content.len() + 1;
        self.replace_range(offset, offset, extra)?;
        let mut destination = offset;
        if needs_separator {
            self.data[destination] = b'\n';
            destination += 1;
        }
        self.data[destination..destination + content.len()].copy_from_slice(content);
        self.data[destination + content.len()] = b'\n';
        Ok(())
    }

    pub fn delete_line(&mut self, line: usize) -> Result<(), BufferError> {
        let (start, end) = self.line_bounds(line).ok_or(BufferError::InvalidLine)?;
        self.replace_range(start, end, 0)
    }

    pub fn replace_line(&mut self, line: usize, content: &[u8]) -> Result<(), BufferError> {
        if content.contains(&b'\n') || content.contains(&b'\r') {
            return Err(BufferError::InvalidLine);
        }
        let (start, end) = self.line_bounds(line).ok_or(BufferError::InvalidLine)?;
        let content_end = if end > start && self.data[end - 1] == b'\n' {
            end - 1
        } else {
            end
        };
        let newline = usize::from(content_end != end);
        self.replace_range(start, end, content.len() + newline)?;
        self.data[start..start + content.len()].copy_from_slice(content);
        if newline != 0 {
            self.data[start + content.len()] = b'\n';
        }
        Ok(())
    }

    fn bounds_from(&self, start: usize) -> (usize, usize) {
        let mut end = start;
        while end < self.len && self.data[end] != b'\n' {
            end += 1;
        }
        if end < self.len {
            end += 1;
        }
        (start, end)
    }

    fn replace_range(
        &mut self,
        start: usize,
        end: usize,
        replacement_length: usize,
    ) -> Result<(), BufferError> {
        let removed = end - start;
        let new_len = self
            .len
            .checked_sub(removed)
            .and_then(|length| length.checked_add(replacement_length))
            .ok_or(BufferError::Full)?;
        if new_len > self.data.len() {
            return Err(BufferError::Full);
        }
        self.data.copy_within(end..self.len, start + replacement_length);
        self.len = new_len;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{BufferError, TextBuffer};

    fn buffer<'a>(storage: &'a mut [u8]) -> TextBuffer<'a> {
        TextBuffer::new(storage)
    }

    #[test]
    fn normalizes_line_endings() {
        let mut storage = [0; 32];
        let mut text = buffer(&mut storage);
        text.load(b"one\r\ntwo\rthree").unwrap();
        assert_eq!(text.as_bytes(), b"one\ntwo\nthree");
    }

    #[test]
    fn inserts_deletes_and_replaces_lines() {
        let mut storage = [0; 64];
        let mut text = buffer(&mut storage);
        text.load(b"one\ntwo\n").unwrap();
        text.insert_line(2, b"between").unwrap();
        text.replace_line(1, b"ONE").unwrap();
        text.delete_line(3).unwrap();
        assert_eq!(text.as_bytes(), b"ONE\nbetween\n");
    }

    #[test]
    fn appending_after_an_unterminated_line_adds_a_separator() {
        let mut storage = [0; 32];
        let mut text = buffer(&mut storage);
        text.load(b"one").unwrap();
        text.insert_line(2, b"two").unwrap();
        assert_eq!(text.as_bytes(), b"one\ntwo\n");
    }

    #[test]
    fn rejects_multiline_replacements() {
        let mut storage = [0; 16];
        let mut text = buffer(&mut storage);
        text.load(b"one\n").unwrap();
        assert_eq!(
            text.replace_line(1, b"bad\nline"),
            Err(BufferError::InvalidLine)
        );
    }

    #[test]
    fn reports_full_storage() {
        let mut storage = [0; 4];
        let mut text = buffer(&mut storage);
        assert_eq!(text.load(b"12345"), Err(BufferError::Full));
    }
}
