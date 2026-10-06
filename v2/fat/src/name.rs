//! File names: what this code will create, how FAT32 turns one into a short (8.3) name plus long-name entries, and
//! the exFAT name hash. Names are printable ASCII only.

use crate::Error;

/// The longest name created. An entry set (long name entries and the short one on FAT32; file, stream and name
/// entries on exFAT) has to sit inside one 512-byte sector, which 16 entries allow comfortably.
pub const MAX_NAME: usize = 60;

pub fn validate(name: &[u8]) -> Result<(), Error> {
    if name.is_empty() || name.len() > MAX_NAME || name[0] == b' ' || name[name.len() - 1] == b' ' || name[name.len() - 1] == b'.' {
        return Err(Error::BadName);
    }
    for &byte in name {
        if !(0x20..0x7f).contains(&byte) || b"\"*/:<>?\\|".contains(&byte) {
            return Err(Error::BadName);
        }
    }
    Ok(())
}

pub fn eq_ignore_case(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.eq_ignore_ascii_case(y))
}

/// A FAT32 short name: the 11 stored bytes, the NT case flags (0x08 = base lower case, 0x10 = extension lower case)
/// and whether long-name entries are needed besides it.
pub struct Short {
    pub bytes: [u8; 11],
    pub case_flags: u8,
    pub needs_long: bool,
}

fn short_char_ok(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'()-@^_`{}~".contains(&byte)
}

/// Case of a part: Some(true) all letters lower, Some(false) all upper, None mixed; digits and symbols are neutral.
fn case_of(part: &[u8]) -> Option<bool> {
    let lower = part.iter().any(u8::is_ascii_lowercase);
    let upper = part.iter().any(u8::is_ascii_uppercase);
    if lower && upper { None } else { Some(lower) }
}

/// Splits at the last dot (a leading dot belongs to the base).
pub fn split(name: &[u8]) -> (&[u8], &[u8]) {
    match name.iter().rposition(|&byte| byte == b'.') {
        Some(dot) if dot > 0 => (&name[..dot], &name[dot + 1..]),
        _ => (name, &[]),
    }
}

/// The short name for `name` when it fits 8.3 on its own.
pub fn exact_short(name: &[u8]) -> Option<Short> {
    let (base, extension) = split(name);
    if base.is_empty() || base.len() > 8 || extension.len() > 3 || !base.iter().chain(extension).all(|&byte| short_char_ok(byte)) {
        return None;
    }
    let (base_lower, extension_lower) = (case_of(base)?, case_of(extension)?);
    let mut bytes = [b' '; 11];
    for (slot, &byte) in bytes.iter_mut().zip(base) {
        *slot = byte.to_ascii_uppercase();
    }
    for (slot, &byte) in bytes[8..].iter_mut().zip(extension) {
        *slot = byte.to_ascii_uppercase();
    }
    // A name that has a dot but nothing after it is not representable (the dot would vanish).
    Some(Short { bytes, case_flags: (if base_lower { 0x08 } else { 0 }) | (if extension_lower { 0x10 } else { 0 }), needs_long: false })
}

/// An alias `BASE~N.EXT` for a long name; `tail` is the number N.
pub fn alias(name: &[u8], tail: u32) -> Short {
    let (base, extension) = split(name);
    let mut bytes = [b' '; 11];
    let mut digits = [0u8; 10];
    let mut count = 0;
    let mut rest = tail;
    while rest > 0 {
        digits[count] = b'0' + (rest % 10) as u8;
        rest /= 10;
        count += 1;
    }
    let room = 8 - 1 - count; // base characters that fit before "~N"
    let mut at = 0;
    for &byte in base {
        if at == room {
            break;
        }
        let byte = byte.to_ascii_uppercase();
        if short_char_ok(byte) {
            bytes[at] = byte;
            at += 1;
        }
    }
    bytes[at] = b'~';
    at += 1;
    for index in (0..count).rev() {
        bytes[at] = digits[index];
        at += 1;
    }
    let mut at = 8;
    for &byte in extension {
        if at == 11 {
            break;
        }
        let byte = byte.to_ascii_uppercase();
        if short_char_ok(byte) {
            bytes[at] = byte;
            at += 1;
        }
    }
    Short { bytes, case_flags: 0, needs_long: true }
}

/// The checksum of a short name that the long-name entries carry.
pub fn short_checksum(short: &[u8; 11]) -> u8 {
    let mut sum = 0u8;
    for &byte in short {
        sum = ((sum & 1) << 7).wrapping_add(sum >> 1).wrapping_add(byte);
    }
    sum
}

/// The stored short name as text ("NAME.EXT"), lower-cased by the NT flags, into `out`; returns its length.
pub fn short_text(short: &[u8], case_flags: u8, out: &mut [u8; 13]) -> usize {
    let mut length = 0;
    for (index, &byte) in short[..8].iter().enumerate() {
        if byte == b' ' {
            break;
        }
        out[length] = if case_flags & 0x08 != 0 { byte.to_ascii_lowercase() } else { byte };
        let _ = index;
        length += 1;
    }
    if short[8] != b' ' {
        out[length] = b'.';
        length += 1;
        for &byte in &short[8..11] {
            if byte == b' ' {
                break;
            }
            out[length] = if case_flags & 0x10 != 0 { byte.to_ascii_lowercase() } else { byte };
            length += 1;
        }
    }
    length
}

/// exFAT's name hash: over the UTF-16LE bytes of the up-cased name.
pub fn exfat_hash(name: &[u8]) -> u16 {
    let mut hash = 0u16;
    for &byte in name {
        for part in [byte.to_ascii_uppercase(), 0] {
            hash = (hash << 15 | hash >> 1).wrapping_add(u16::from(part));
        }
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_names() {
        let short = exact_short(b"take001.wav").unwrap();
        assert_eq!(&short.bytes, b"TAKE001 WAV");
        assert_eq!(short.case_flags, 0x18);
        assert!(exact_short(b"Take001.wav").is_none()); // mixed case needs a long name
        assert!(exact_short(b"longer_name.wav").is_none());
        let alias = alias(b"gig night 1.wav", 1);
        assert_eq!(&alias.bytes, b"GIGNIG~1WAV");
        let mut text = [0u8; 13];
        let length = short_text(b"TAKE001 WAV", 0x18, &mut text);
        assert_eq!(&text[..length], b"take001.wav");
    }

    #[test]
    fn names_are_validated() {
        assert!(validate(b"ok name.wav").is_ok());
        assert_eq!(validate(b"bad:name"), Err(Error::BadName));
        assert_eq!(validate(b""), Err(Error::BadName));
        assert_eq!(validate("åäö".as_bytes()), Err(Error::BadName));
    }
}
