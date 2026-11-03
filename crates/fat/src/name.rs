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

/// The most UTF-16 units an exFAT name may have here (the entry set must fit one sector).
pub const MAX_UNITS: usize = 60;

/// The longest UTF-8 form of such a name (three bytes a unit).
pub const MAX_BYTES: usize = MAX_NAME * 3;

/// The simple upper-case mapping of a character: what exFAT's up-case table is for. Characters that would expand
/// (ß) stay as they are, like in the table.
pub fn upcase(c: char) -> char {
    let mut upper = c.to_uppercase();
    match (upper.next(), upper.next()) {
        (Some(single), None) => single,
        _ => c,
    }
}

/// Names are UTF-8 (exFAT stores UTF-16; FAT32 long names show anything outside ASCII as `?`). ASCII names compare
/// the old way; others by simple case folding, and text that is not UTF-8 byte for byte.
pub fn eq_ignore_case(a: &[u8], b: &[u8]) -> bool {
    if a.is_ascii() && b.is_ascii() {
        return a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.eq_ignore_ascii_case(y));
    }
    match (core::str::from_utf8(a), core::str::from_utf8(b)) {
        (Ok(a), Ok(b)) => a.chars().map(upcase).eq(b.chars().map(upcase)),
        _ => a == b,
    }
}

/// How many UTF-16 units `name` (UTF-8) takes; `None` when it is not UTF-8.
pub fn units(name: &[u8]) -> Option<usize> {
    Some(core::str::from_utf8(name).ok()?.chars().map(char::len_utf16).sum())
}

/// The entry-set size for an exFAT name: the file and stream entries plus 15 units per name entry.
pub fn exfat_entries(name: &[u8]) -> usize {
    2 + units(name).unwrap_or(0).div_ceil(15)
}

/// What exFAT will be asked to create: UTF-8 of at most 60 UTF-16 units, no controls or `" * / : < > ? \ |`, no
/// leading or trailing space and no trailing dot.
pub fn validate_exfat(name: &[u8]) -> Result<(), Error> {
    let Ok(text) = core::str::from_utf8(name) else { return Err(Error::BadName) };
    let count = units(name).unwrap_or(0);
    if count == 0 || count > MAX_UNITS || text.starts_with(' ') || text.ends_with(' ') || text.ends_with('.') {
        return Err(Error::BadName);
    }
    for c in text.chars() {
        let code = c as u32;
        if code < 0x20 || (0x7f..=0x9f).contains(&code) || matches!(c, '"' | '*' | '/' | ':' | '<' | '>' | '?' | '\\' | '|') {
            return Err(Error::BadName);
        }
    }
    Ok(())
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

/// exFAT's name hash: over the UTF-16LE bytes of the up-cased name (`name` is UTF-8).
pub fn exfat_hash(name: &[u8]) -> u16 {
    let mut hash = 0u16;
    for c in core::str::from_utf8(name).unwrap_or("").chars() {
        let mut pair = [0u16; 2];
        for unit in upcase(c).encode_utf16(&mut pair) {
            for part in unit.to_le_bytes() {
                hash = (hash << 15 | hash >> 1).wrapping_add(u16::from(part));
            }
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

    #[test]
    fn exfat_names_are_unicode() {
        assert!(validate_exfat("Räksmörgås 日本語 😀.txt".as_bytes()).is_ok());
        assert_eq!(validate_exfat(b"bad\xe5"), Err(Error::BadName));
        assert_eq!(validate_exfat("a:ö".as_bytes()), Err(Error::BadName));
        assert_eq!(validate_exfat("ö".repeat(61).as_bytes()), Err(Error::BadName));
        assert!(validate_exfat("ö".repeat(60).as_bytes()).is_ok());
        assert_eq!(units("a😀".as_bytes()), Some(3));
        assert!(eq_ignore_case("ÅÄÖ.TXT".as_bytes(), "åäö.txt".as_bytes()));
        assert!(!eq_ignore_case("a\u{30a}".as_bytes(), "å".as_bytes()));
        assert_eq!(exfat_hash("åäö".as_bytes()), exfat_hash("ÅÄÖ".as_bytes()));
        assert_eq!(exfat_hash(b"abc"), exfat_hash(b"ABC"));
    }
}
