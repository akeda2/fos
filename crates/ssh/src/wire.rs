//! The SSH data types of RFC 4251 section 5 over plain slices: `Reader` takes them off a message, `Writer` puts them on one.
//! Neither allocates; a `Writer` that runs out of room remembers it (`ok()`), so a message is built without a check per field.

pub struct Reader<'a> {
    data: &'a [u8],
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Reader<'a> {
        Reader { data }
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub fn rest(&self) -> &'a [u8] {
        self.data
    }

    pub fn u8(&mut self) -> Option<u8> {
        let (&byte, rest) = self.data.split_first()?;
        self.data = rest;
        Some(byte)
    }

    pub fn bool(&mut self) -> Option<bool> {
        self.u8().map(|byte| byte != 0)
    }

    pub fn u32(&mut self) -> Option<u32> {
        let bytes = self.data.get(..4)?;
        self.data = &self.data[4..];
        Some(u32::from_be_bytes(bytes.try_into().ok()?))
    }

    pub fn u64(&mut self) -> Option<u64> {
        let bytes = self.data.get(..8)?;
        self.data = &self.data[8..];
        Some(u64::from_be_bytes(bytes.try_into().ok()?))
    }

    /// `n` raw bytes.
    pub fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        let bytes = self.data.get(..n)?;
        self.data = &self.data[n..];
        Some(bytes)
    }

    /// A `string`: a length, then that many bytes.
    pub fn string(&mut self) -> Option<&'a [u8]> {
        let length = self.u32()? as usize;
        self.bytes(length)
    }

    pub fn text(&mut self) -> Option<&'a str> {
        core::str::from_utf8(self.string()?).ok()
    }
}

pub struct Writer<'a> {
    buffer: &'a mut [u8],
    length: usize,
    overflow: bool,
}

impl<'a> Writer<'a> {
    pub fn new(buffer: &'a mut [u8]) -> Writer<'a> {
        Writer { buffer, length: 0, overflow: false }
    }

    pub fn len(&self) -> usize {
        self.length
    }

    pub fn is_empty(&self) -> bool {
        self.length == 0
    }

    /// False when something did not fit; the message is then not to be sent.
    pub fn ok(&self) -> bool {
        !self.overflow
    }

    pub fn written(&self) -> &[u8] {
        &self.buffer[..self.length]
    }

    pub fn raw(&mut self, data: &[u8]) -> &mut Self {
        match self.buffer.get_mut(self.length..self.length + data.len()) {
            Some(room) => {
                room.copy_from_slice(data);
                self.length += data.len();
            }
            None => self.overflow = true,
        }
        self
    }

    pub fn u8(&mut self, value: u8) -> &mut Self {
        self.raw(&[value])
    }

    pub fn bool(&mut self, value: bool) -> &mut Self {
        self.u8(value as u8)
    }

    pub fn u32(&mut self, value: u32) -> &mut Self {
        self.raw(&value.to_be_bytes())
    }

    pub fn u64(&mut self, value: u64) -> &mut Self {
        self.raw(&value.to_be_bytes())
    }

    pub fn string(&mut self, data: &[u8]) -> &mut Self {
        self.u32(data.len() as u32).raw(data)
    }

    /// A `mpint` from big-endian bytes: leading zeros dropped, one zero added in front when the top bit is set.
    pub fn mpint(&mut self, big_endian: &[u8]) -> &mut Self {
        let start = big_endian.iter().position(|&byte| byte != 0).unwrap_or(big_endian.len());
        let digits = &big_endian[start..];
        let pad = digits.first().is_some_and(|&byte| byte & 0x80 != 0);
        self.u32(digits.len() as u32 + pad as u32);
        if pad {
            self.u8(0);
        }
        self.raw(digits)
    }

    /// Overwrites four bytes already written (a length known only at the end).
    pub fn patch_u32(&mut self, at: usize, value: u32) {
        match self.buffer.get_mut(at..at + 4) {
            Some(room) if at + 4 <= self.length => room.copy_from_slice(&value.to_be_bytes()),
            _ => self.overflow = true,
        }
    }

    pub fn truncate(&mut self, length: usize) {
        self.length = self.length.min(length);
    }

    /// The room left, to read into directly; `advance` then counts what was put there.
    pub fn spare(&mut self) -> &mut [u8] {
        &mut self.buffer[self.length..]
    }

    pub fn advance(&mut self, n: usize) {
        if self.length + n > self.buffer.len() {
            self.overflow = true;
        } else {
            self.length += n;
        }
    }

    /// Reserves `n` bytes to fill in place (a length written afterwards, a signature); `None` when it does not fit.
    pub fn reserve(&mut self, n: usize) -> Option<&mut [u8]> {
        match self.buffer.get_mut(self.length..self.length + n) {
            Some(room) => {
                self.length += n;
                Some(room)
            }
            None => {
                self.overflow = true;
                None
            }
        }
    }
}

/// Standard base64 (RFC 4648, `=` padding) into `out`; the length decoded, or `None` for a bad character or no room.
pub fn base64_decode(text: &str, out: &mut [u8]) -> Option<usize> {
    let (mut accumulator, mut bits, mut length) = (0u32, 0u32, 0usize);
    for byte in text.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            _ => return None,
        };
        accumulator = accumulator << 6 | value as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            *out.get_mut(length)? = (accumulator >> bits) as u8;
            length += 1;
        }
    }
    Some(length)
}

pub fn base64_encode(data: &[u8], out: &mut [u8]) -> Option<usize> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut length = 0;
    for chunk in data.chunks(3) {
        let word = (chunk[0] as u32) << 16 | (*chunk.get(1).unwrap_or(&0) as u32) << 8 | *chunk.get(2).unwrap_or(&0) as u32;
        for index in 0..4 {
            *out.get_mut(length)? = if index <= chunk.len() { ALPHABET[(word >> (18 - 6 * index) & 63) as usize] } else { b'=' };
            length += 1;
        }
    }
    Some(length)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mpint_follows_rfc_4251() {
        let mut buffer = [0u8; 32];
        let mut writer = Writer::new(&mut buffer);
        writer.mpint(&[0, 0, 0x80, 1]).mpint(&[0x7f]).mpint(&[0, 0]);
        assert_eq!(writer.written(), [0, 0, 0, 3, 0, 0x80, 1, 0, 0, 0, 1, 0x7f, 0, 0, 0, 0]);
    }

    #[test]
    fn strings_round_trip_and_overflow_is_noticed() {
        let mut buffer = [0u8; 12];
        let mut writer = Writer::new(&mut buffer);
        writer.string(b"abc").u32(9);
        assert!(writer.ok());
        let mut reader = Reader::new(writer.written());
        assert_eq!(reader.string(), Some(&b"abc"[..]));
        assert_eq!(reader.u32(), Some(9));
        assert!(reader.is_empty());
        assert_eq!(reader.u8(), None);
        let mut small = [0u8; 5];
        let mut writer = Writer::new(&mut small);
        writer.string(b"abc");
        assert!(!writer.ok());
    }

    #[test]
    fn base64_round_trip() {
        let mut text = [0u8; 16];
        let mut data = [0u8; 8];
        for sample in [&b""[..], b"f", b"fo", b"foo", b"foob", b"fooba", b"foobar"] {
            let length = base64_encode(sample, &mut text).unwrap();
            assert_eq!(base64_decode(core::str::from_utf8(&text[..length]).unwrap(), &mut data), Some(sample.len()));
            assert_eq!(&data[..sample.len()], sample);
        }
        assert_eq!(base64_decode("Zm9v!", &mut data), None);
    }
}
