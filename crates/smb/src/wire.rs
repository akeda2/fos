//! What the client and the server share: little-endian field access, UTF-16 names, the NetBIOS-style frame header and the
//! 64-byte SMB2 header (MS-SMB2 2.2.1). All integers are little-endian; the frame header is a zero byte and a 24-bit length.

pub const HEADER: usize = 64;
/// A message starts with `\xFESMB`.
pub const PROTOCOL_ID: [u8; 4] = [0xFE, b'S', b'M', b'B'];

pub const CMD_NEGOTIATE: u16 = 0;
pub const CMD_SESSION_SETUP: u16 = 1;
pub const CMD_LOGOFF: u16 = 2;
pub const CMD_TREE_CONNECT: u16 = 3;
pub const CMD_TREE_DISCONNECT: u16 = 4;
pub const CMD_CREATE: u16 = 5;
pub const CMD_CLOSE: u16 = 6;
pub const CMD_FLUSH: u16 = 7;
pub const CMD_READ: u16 = 8;
pub const CMD_WRITE: u16 = 9;
pub const CMD_LOCK: u16 = 10;
pub const CMD_IOCTL: u16 = 11;
pub const CMD_CANCEL: u16 = 12;
pub const CMD_ECHO: u16 = 13;
pub const CMD_QUERY_DIRECTORY: u16 = 14;
pub const CMD_CHANGE_NOTIFY: u16 = 15;
pub const CMD_QUERY_INFO: u16 = 16;
pub const CMD_SET_INFO: u16 = 17;

/// The header flag of a response.
pub const FLAG_RESPONSE: u32 = 1;
/// The header flag of a request that continues a compound chain (it takes the previous request's file id).
pub const FLAG_RELATED: u32 = 4;

pub fn put16(buffer: &mut [u8], at: usize, value: u16) {
    buffer[at..at + 2].copy_from_slice(&value.to_le_bytes());
}
pub fn put32(buffer: &mut [u8], at: usize, value: u32) {
    buffer[at..at + 4].copy_from_slice(&value.to_le_bytes());
}
pub fn put64(buffer: &mut [u8], at: usize, value: u64) {
    buffer[at..at + 8].copy_from_slice(&value.to_le_bytes());
}
pub fn get16(buffer: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([buffer[at], buffer[at + 1]])
}
pub fn get32(buffer: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([buffer[at], buffer[at + 1], buffer[at + 2], buffer[at + 3]])
}
pub fn get64(buffer: &[u8], at: usize) -> u64 {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&buffer[at..at + 8]);
    u64::from_le_bytes(bytes)
}

/// Writes `text` as UTF-16LE at `at`, '/' becoming '\'; returns the byte count, or `None` when it does not fit.
pub fn put_name(buffer: &mut [u8], at: usize, text: &str) -> Option<usize> {
    let mut length = 0;
    for unit in text.encode_utf16() {
        if at + length + 2 > buffer.len() {
            return None;
        }
        let unit = if unit == b'/' as u16 { b'\\' as u16 } else { unit };
        put16(buffer, at + length, unit);
        length += 2;
    }
    Some(length)
}

/// Writes UTF-16LE `name` as UTF-8 to `out` (cut off where it no longer fits), '\' becoming '/'; returns the length.
pub fn name_to_utf8(name: &[u8], out: &mut [u8]) -> usize {
    let units = name.chunks_exact(2).map(|pair| u16::from_le_bytes([pair[0], pair[1]]));
    let mut length = 0;
    for character in char::decode_utf16(units) {
        let character = character.unwrap_or('\u{fffd}');
        let character = if character == '\\' { '/' } else { character };
        if length + character.len_utf8() > out.len() {
            break;
        }
        length += character.encode_utf8(&mut out[length..]).len();
    }
    length
}

/// Writes UTF-8 `text` as UTF-16LE at `at` without turning '/' into '\' (names inside a folder listing); `None` when it
/// does not fit.
pub fn put_utf16(buffer: &mut [u8], at: usize, text: &[u8]) -> Option<usize> {
    let text = core::str::from_utf8(text).ok()?;
    let mut length = 0;
    for unit in text.encode_utf16() {
        if at + length + 2 > buffer.len() {
            return None;
        }
        put16(buffer, at + length, unit);
        length += 2;
    }
    Some(length)
}

/// The 4-byte frame header for a message of `length` bytes.
pub fn frame_header(length: usize) -> [u8; 4] {
    [0, (length >> 16) as u8, (length >> 8) as u8, length as u8]
}

/// The length a frame header announces, or `None` when it is not a session message.
pub fn frame_length(header: &[u8; 4]) -> Option<usize> {
    (header[0] == 0).then(|| (header[1] as usize) << 16 | (header[2] as usize) << 8 | header[3] as usize)
}
