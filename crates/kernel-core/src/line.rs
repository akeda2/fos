//! A short line of text on the stack, for `write!` where nothing can allocate.

pub struct Line {
    bytes: [u8; 110],
    length: usize,
}

impl Line {
    pub fn new() -> Line {
        Line { bytes: [0; 110], length: 0 }
    }
    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.bytes[..self.length]).unwrap_or("")
    }
}

impl core::fmt::Write for Line {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        let take = text.len().min(self.bytes.len() - self.length);
        self.bytes[self.length..self.length + take].copy_from_slice(&text.as_bytes()[..take]);
        self.length += take;
        Ok(())
    }
}
