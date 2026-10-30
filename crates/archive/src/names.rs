//! Output file names, all made from one base name the user types (or gives at startup as `name=`).
//!
//! `BASE` may start with a volume (`disk2/`, `@LABEL/`). For the base `lp` the raw Lt/Rt take is `lp.lr.flac` and the
//! decoded file `lp.flac`; when either exists the next free number is put after the base (`lp2.lr.flac`, `lp2.flac`), so a
//! take is never overwritten. On FOSFS a file name is at most 16 characters of lowercase letters, digits, `.`, `_` and `-`,
//! so the part after the volume is at most 6 characters there (room for two digits and `.lr.flac`); on a FAT or exFAT disk
//! (`diskN/`) 40.

const MAX_NAME: usize = 80;
/// The longest base on FOSFS: 16 less `.lr.flac` and two digits.
pub const FOSFS_LEAF: usize = 6;
pub const FAT_LEAF: usize = 40;
/// Numbers tried before giving up.
pub const MAX_NUMBER: u32 = 99;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NameError {
    Empty,
    /// A character outside lowercase letters, digits, `.`, `_`, `-` (and one `/` after the volume).
    BadChar,
    /// More characters than a file name on that volume can take.
    TooLong(usize),
}

/// A file name (volume prefix included), by value.
#[derive(Clone, Copy)]
pub struct Name {
    bytes: [u8; MAX_NAME],
    len: usize,
}

impl Name {
    pub const fn new() -> Name {
        Name { bytes: [0; MAX_NAME], len: 0 }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }

    pub fn as_str(&self) -> &str {
        core::str::from_utf8(self.as_bytes()).unwrap_or("")
    }

    fn push(&mut self, bytes: &[u8]) {
        self.bytes[self.len..self.len + bytes.len()].copy_from_slice(bytes);
        self.len += bytes.len();
    }
}

impl core::fmt::Debug for Name {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl Default for Name {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Names {
    base: Name,
}

impl Names {
    pub fn parse(text: &str) -> Result<Names, NameError> {
        let bytes = text.as_bytes();
        let leaf = bytes.iter().rposition(|&b| b == b'/').map_or(0, |at| at + 1);
        let (volume, name) = bytes.split_at(leaf);
        if name.is_empty() {
            return Err(NameError::Empty);
        }
        if !volume.is_empty() && !valid_volume(&volume[..volume.len() - 1]) {
            return Err(NameError::BadChar);
        }
        if name.iter().any(|&b| !(b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-'))) || name[0] == b'.' {
            return Err(NameError::BadChar);
        }
        let fat = volume.starts_with(b"disk");
        let most = if fat { FAT_LEAF } else { FOSFS_LEAF };
        if name.len() > most {
            return Err(NameError::TooLong(most));
        }
        let mut base = Name::new();
        base.push(bytes);
        Ok(Names { base })
    }

    pub fn base(&self) -> &str {
        self.base.as_str()
    }

    /// The raw Lt/Rt take for take number `n` (1 adds no number).
    pub fn raw(&self, n: u32) -> Name {
        self.make(n, b".lr.flac")
    }

    /// The decoded file for take number `n`.
    pub fn decoded(&self, n: u32) -> Name {
        self.make(n, b".flac")
    }

    fn make(&self, n: u32, extension: &[u8]) -> Name {
        let mut name = self.base;
        if n > 1 {
            let mut digits = [0u8; 3];
            let mut at = digits.len();
            let mut rest = n;
            while rest > 0 {
                at -= 1;
                digits[at] = b'0' + (rest % 10) as u8;
                rest /= 10;
            }
            name.push(&digits[at..]);
        }
        name.push(extension);
        name
    }

    /// The first take number whose raw and decoded names do not exist yet, according to `exists`.
    pub fn first_free(&self, mut exists: impl FnMut(&[u8]) -> bool) -> Option<u32> {
        (1..=MAX_NUMBER).find(|&n| !exists(self.raw(n).as_bytes()) && !exists(self.decoded(n).as_bytes()))
    }

    /// The decoded name that goes with a raw one (`lp2.lr.flac` gives `lp2.flac`), if it is one.
    pub fn decoded_for(raw: &[u8]) -> Option<Name> {
        let stem = raw.strip_suffix(b".lr.flac")?;
        let mut name = Name::new();
        if stem.len() + 5 > MAX_NAME {
            return None;
        }
        name.push(stem);
        name.push(b".flac");
        Some(name)
    }
}

fn valid_volume(volume: &[u8]) -> bool {
    match volume {
        [b'@', label @ ..] => !label.is_empty() && label.iter().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.')),
        [b'd', b'i', b's', b'k', digits @ ..] => !digits.is_empty() && digits.iter().all(u8::is_ascii_digit),
        _ => false,
    }
}

/// What a name field accepts as it is typed: lowercase letters (capitals fold down), digits, `.`, `_`, `-`, and `/` or `@`
/// for a volume. Anything else is ignored.
pub fn typed(byte: u8) -> Option<u8> {
    match byte {
        b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-' | b'/' | b'@' => Some(byte),
        b'A'..=b'Z' => Some(byte.to_ascii_lowercase()),
        _ => None,
    }
}
