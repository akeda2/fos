//! Title, artist and album of a track, as UTF-8 text a decoder fills while it opens the file: Vorbis comments in FLAC,
//! ID3v2 (and the old ID3v1 at the end) in MP3. Fixed-size and truncated at a character boundary, so no allocation.

const CAPACITY: usize = 96;

/// One short UTF-8 string.
#[derive(Clone, Copy)]
pub struct Text {
    bytes: [u8; CAPACITY],
    len: usize,
}

impl Text {
    pub const fn new() -> Self {
        Text { bytes: [0; CAPACITY], len: 0 }
    }

    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.bytes[..self.len]).unwrap_or("")
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn clear(&mut self) {
        self.len = 0;
    }

    fn push(&mut self, c: char) {
        // Control characters would move the console cursor.
        if c.is_control() || self.len + c.len_utf8() > CAPACITY {
            return;
        }
        self.len += c.encode_utf8(&mut self.bytes[self.len..]).len();
    }

    /// Replaces the text with UTF-8 bytes; invalid sequences become `?`.
    pub fn set_utf8(&mut self, data: &[u8]) {
        self.len = 0;
        for piece in data.utf8_chunks() {
            for c in piece.valid().chars() {
                self.push(c);
            }
            if !piece.invalid().is_empty() {
                self.push('?');
            }
        }
    }

    /// ISO-8859-1: every byte is the code point of the same value.
    pub fn set_latin1(&mut self, data: &[u8]) {
        self.len = 0;
        for &byte in data {
            if byte == 0 {
                break;
            }
            self.push(byte as char);
        }
    }

    /// UTF-16 with `big_endian` unless a byte-order mark says otherwise; ends at a NUL character.
    pub fn set_utf16(&mut self, data: &[u8], mut big_endian: bool) {
        self.len = 0;
        let mut data = data;
        if data.len() >= 2 && (data[..2] == [0xFF, 0xFE] || data[..2] == [0xFE, 0xFF]) {
            big_endian = data[0] == 0xFE;
            data = &data[2..];
        }
        let units = data.chunks_exact(2).map(|pair| if big_endian { u16::from_be_bytes([pair[0], pair[1]]) } else { u16::from_le_bytes([pair[0], pair[1]]) });
        for decoded in char::decode_utf16(units.take_while(|&unit| unit != 0)) {
            self.push(decoded.unwrap_or('?'));
        }
    }
}

#[derive(Clone, Copy)]
pub struct Tags {
    pub title: Text,
    pub artist: Text,
    pub album: Text,
}

impl Tags {
    pub const fn new() -> Self {
        Tags { title: Text::new(), artist: Text::new(), album: Text::new() }
    }

    pub fn clear(&mut self) {
        self.title.clear();
        self.artist.clear();
        self.album.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.title.is_empty() && self.artist.is_empty() && self.album.is_empty()
    }

    /// Stores a Vorbis comment `NAME=value` (the name in any case).
    pub fn set_vorbis(&mut self, comment: &[u8]) {
        let Some(equals) = comment.iter().position(|&b| b == b'=') else { return };
        let (name, value) = (&comment[..equals], &comment[equals + 1..]);
        let field = if name.eq_ignore_ascii_case(b"title") {
            &mut self.title
        } else if name.eq_ignore_ascii_case(b"artist") {
            &mut self.artist
        } else if name.eq_ignore_ascii_case(b"album") {
            &mut self.album
        } else {
            return;
        };
        field.set_utf8(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodings() {
        let mut t = Text::new();
        t.set_latin1(b"Bj\xf6rk");
        assert_eq!(t.as_str(), "Björk");
        t.set_utf16(&[0xFF, 0xFE, b'A', 0, 0xE5, 0, 0, 0, b'x', 0], false);
        assert_eq!(t.as_str(), "Aå");
        t.set_utf16(&[0, b'B', 0x00, 0xE4], true);
        assert_eq!(t.as_str(), "Bä");
        t.set_utf8(b"ok\x01\xffend");
        assert_eq!(t.as_str(), "ok?end");
        t.set_utf8(&[b'a'; 200]);
        assert_eq!(t.as_str().len(), CAPACITY);
    }

    #[test]
    fn vorbis_names_ignore_case() {
        let mut tags = Tags::new();
        tags.set_vorbis(b"Title=Allemande");
        tags.set_vorbis(b"ARTIST=Lute");
        tags.set_vorbis(b"genre=x");
        assert_eq!((tags.title.as_str(), tags.artist.as_str(), tags.album.is_empty()), ("Allemande", "Lute", true));
    }
}
