//! A stub DNS resolver's wire format (RFC 1035): one recursive A query, and the reply read back. No I/O here;
//! `tape-ip` sends the datagrams to the servers DHCP named (option 6) and the telnet link of `fterm` does the same
//! a step at a time.
//!
//! Only what a client needs: IPv4 (A) answers, compressed names in the reply, a CNAME chain followed by taking the
//! first A record of the answer section (a recursive server puts the chain's end there), and a clear reason when the
//! server says no. No caching, no TCP fallback (a truncated reply is reported), no IPv6.

pub const PORT: u16 = 53;
/// Longest name (without the trailing dot) RFC 1035 allows.
pub const MAX_NAME: usize = 253;
/// Room for any query `build_query` makes: header, a maximal name with its length bytes, type and class.
pub const MAX_QUERY: usize = 12 + MAX_NAME + 2 + 4;

const TYPE_A: u16 = 1;
const CLASS_IN: u16 = 1;

/// Why a reply did not give an address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    /// Not an answer to our query (other id, not a reply, other question): keep waiting.
    NotOurs,
    /// Too short or inconsistent.
    Malformed,
    /// The name does not exist (NXDOMAIN).
    NoSuchName,
    /// The server could not answer (SERVFAIL, REFUSED, ...): the RCODE.
    Server(u8),
    /// The name exists but has no IPv4 address.
    NoAddress,
    /// The reply was cut short for UDP and the answer did not fit.
    Truncated,
}

/// Whether `name` is a host name this resolver will send: dot-separated labels of 1 to 63 letters, digits, `-` or `_`
/// (one trailing dot allowed), at most `MAX_NAME` bytes. A dotted-quad is also valid here; callers try an IP first.
pub fn valid_name(name: &str) -> bool {
    let name = name.strip_suffix('.').unwrap_or(name);
    if name.is_empty() || name.len() > MAX_NAME {
        return false;
    }
    name.split('.').all(|label| (1..=63).contains(&label.len()) && label.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'))
}

/// Writes a recursive-desired A query for `name` with transaction `id`; returns its length, or None for an invalid name
/// or a short buffer (`MAX_QUERY` always suffices).
pub fn build_query(out: &mut [u8], id: u16, name: &str) -> Option<usize> {
    if !valid_name(name) {
        return None;
    }
    let name = name.strip_suffix('.').unwrap_or(name);
    let needed = 12 + name.len() + 2 + 4;
    if out.len() < needed {
        return None;
    }
    out[0..2].copy_from_slice(&id.to_be_bytes());
    out[2..4].copy_from_slice(&0x0100u16.to_be_bytes()); // a query, recursion desired
    out[4..6].copy_from_slice(&1u16.to_be_bytes());
    out[6..12].fill(0);
    let mut at = 12;
    for label in name.split('.') {
        out[at] = label.len() as u8;
        out[at + 1..at + 1 + label.len()].copy_from_slice(label.as_bytes());
        at += 1 + label.len();
    }
    out[at] = 0;
    out[at + 1..at + 3].copy_from_slice(&TYPE_A.to_be_bytes());
    out[at + 3..at + 5].copy_from_slice(&CLASS_IN.to_be_bytes());
    Some(at + 5)
}

/// The offset after the (possibly compressed) name that starts at `at`. A pointer ends a name, so it is never followed:
/// only the question's name is compared, and that one is written out in full by every server.
fn skip_name(bytes: &[u8], mut at: usize) -> Option<usize> {
    loop {
        let length = *bytes.get(at)?;
        match length {
            0 => return Some(at + 1),
            _ if length & 0xC0 == 0xC0 => {
                bytes.get(at + 1)?;
                return Some(at + 2);
            }
            _ if length & 0xC0 != 0 => return None,
            _ => at += 1 + usize::from(length),
        }
    }
}

/// Whether the question name starting at `at` spells `name` (letters compare without case, as DNS does).
fn name_matches(bytes: &[u8], at: usize, name: &str) -> bool {
    let name = name.strip_suffix('.').unwrap_or(name);
    let mut at = at;
    for label in name.split('.') {
        let Some(&length) = bytes.get(at) else { return false };
        let Some(text) = bytes.get(at + 1..at + 1 + usize::from(length)) else { return false };
        if usize::from(length) != label.len() || !text.eq_ignore_ascii_case(label.as_bytes()) {
            return false;
        }
        at += 1 + usize::from(length);
    }
    bytes.get(at) == Some(&0)
}

/// Reads a reply to the query `build_query(_, id, name)` made: the first IPv4 address of the answer section.
pub fn parse_reply(bytes: &[u8], id: u16, name: &str) -> Result<[u8; 4], Failure> {
    if bytes.len() < 12 || bytes[0..2] != id.to_be_bytes() || bytes[2] & 0x80 == 0 {
        return Err(Failure::NotOurs);
    }
    let flags = u16::from_be_bytes([bytes[2], bytes[3]]);
    let (questions, answers) = (u16::from_be_bytes([bytes[4], bytes[5]]), u16::from_be_bytes([bytes[6], bytes[7]]));
    if questions != 1 || !name_matches(bytes, 12, name) {
        return Err(Failure::NotOurs);
    }
    match flags & 0xF {
        0 => {}
        3 => return Err(Failure::NoSuchName),
        other => return Err(Failure::Server(other as u8)),
    }
    let mut at = skip_name(bytes, 12).ok_or(Failure::Malformed)? + 4;
    for _ in 0..answers {
        at = skip_name(bytes, at).ok_or(Failure::Malformed)?;
        let fixed = bytes.get(at..at + 10).ok_or(Failure::Malformed)?;
        let (kind, class, length) = (u16::from_be_bytes([fixed[0], fixed[1]]), u16::from_be_bytes([fixed[2], fixed[3]]), usize::from(u16::from_be_bytes([fixed[8], fixed[9]])));
        let data = bytes.get(at + 10..at + 10 + length).ok_or(Failure::Malformed)?;
        if kind == TYPE_A && class == CLASS_IN && length == 4 {
            return Ok([data[0], data[1], data[2], data[3]]);
        }
        at += 10 + length;
    }
    if flags & 0x0200 != 0 { Err(Failure::Truncated) } else { Err(Failure::NoAddress) }
}

/// A sentence for `Failure`, for a tape's message.
pub fn describe(failure: Failure) -> &'static str {
    match failure {
        Failure::NotOurs | Failure::Malformed => "the DNS reply was not understood",
        Failure::NoSuchName => "no such name",
        Failure::Server(_) => "the DNS server refused or failed",
        Failure::NoAddress => "the name has no IPv4 address",
        Failure::Truncated => "the DNS reply was truncated",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reply to `build_query(_, id, name)` with the given answer records appended (each `(type, rdata)`, owner as a
    /// pointer to the question) and RCODE/flags.
    fn reply(id: u16, name: &str, flags: u16, records: &[(u16, &[u8])]) -> ([u8; 512], usize) {
        let mut out = [0u8; 512];
        let mut length = build_query(&mut out, id, name).unwrap();
        out[2..4].copy_from_slice(&(0x8180 | flags).to_be_bytes());
        out[6..8].copy_from_slice(&(records.len() as u16).to_be_bytes());
        for (kind, data) in records {
            out[length..length + 2].copy_from_slice(&[0xC0, 12]);
            out[length + 2..length + 4].copy_from_slice(&kind.to_be_bytes());
            out[length + 4..length + 6].copy_from_slice(&1u16.to_be_bytes());
            out[length + 6..length + 10].copy_from_slice(&60u32.to_be_bytes());
            out[length + 10..length + 12].copy_from_slice(&(data.len() as u16).to_be_bytes());
            out[length + 12..length + 12 + data.len()].copy_from_slice(data);
            length += 12 + data.len();
        }
        (out, length)
    }

    #[test]
    fn query_layout() {
        let mut out = [0u8; MAX_QUERY];
        let length = build_query(&mut out, 0x1234, "Example.org.").unwrap();
        assert_eq!(&out[..12], &[0x12, 0x34, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0]);
        assert_eq!(&out[12..length], b"\x07Example\x03org\x00\x00\x01\x00\x01");
    }

    #[test]
    fn names() {
        assert!(valid_name("a.b-c_d.example"));
        assert!(valid_name("localhost"));
        assert!(!valid_name(""));
        assert!(!valid_name("."));
        assert!(!valid_name("a..b"));
        assert!(!valid_name("bad name"));
        assert!(!valid_name(&"a".repeat(64)));
        assert!(valid_name(&"a".repeat(63)));
        assert!(!valid_name(&["abcdefghi."; 26].concat()));
        let mut out = [0u8; MAX_QUERY];
        assert_eq!(build_query(&mut out, 1, "a b"), None);
        assert_eq!(build_query(&mut [0u8; 8], 1, "a.b"), None);
    }

    #[test]
    fn maximal_name_fits() {
        let name = ["a".repeat(63), "b".repeat(63), "c".repeat(63), "d".repeat(61)].join(".");
        assert_eq!(name.len(), MAX_NAME);
        let mut out = [0u8; MAX_QUERY];
        assert_eq!(build_query(&mut out, 1, &name), Some(MAX_QUERY));
    }

    #[test]
    fn answer_found() {
        let (bytes, length) = reply(7, "host.example", 0, &[(1, &[10, 1, 2, 3])]);
        assert_eq!(parse_reply(&bytes[..length], 7, "HOST.example"), Ok([10, 1, 2, 3]));
    }

    #[test]
    fn cname_then_address() {
        let (bytes, length) = reply(7, "www.example", 0, &[(5, b"\x03web\x07example\x00"), (1, &[192, 0, 2, 9])]);
        assert_eq!(parse_reply(&bytes[..length], 7, "www.example"), Ok([192, 0, 2, 9]));
    }

    #[test]
    fn failures() {
        let (bytes, length) = reply(7, "x.example", 3, &[]);
        assert_eq!(parse_reply(&bytes[..length], 7, "x.example"), Err(Failure::NoSuchName));
        let (bytes, length) = reply(7, "x.example", 5, &[]);
        assert_eq!(parse_reply(&bytes[..length], 7, "x.example"), Err(Failure::Server(5)));
        let (bytes, length) = reply(7, "x.example", 0, &[]);
        assert_eq!(parse_reply(&bytes[..length], 7, "x.example"), Err(Failure::NoAddress));
        let (bytes, length) = reply(7, "x.example", 0, &[(5, b"\x00")]);
        assert_eq!(parse_reply(&bytes[..length], 7, "x.example"), Err(Failure::NoAddress));
        let (bytes, length) = reply(7, "x.example", 0x0200, &[]);
        assert_eq!(parse_reply(&bytes[..length], 7, "x.example"), Err(Failure::Truncated));
    }

    #[test]
    fn not_ours_is_ignored() {
        let (bytes, length) = reply(7, "x.example", 0, &[(1, &[1, 2, 3, 4])]);
        assert_eq!(parse_reply(&bytes[..length], 8, "x.example"), Err(Failure::NotOurs));
        assert_eq!(parse_reply(&bytes[..length], 7, "y.example"), Err(Failure::NotOurs));
        let mut query = [0u8; MAX_QUERY];
        let query_length = build_query(&mut query, 7, "x.example").unwrap();
        assert_eq!(parse_reply(&query[..query_length], 7, "x.example"), Err(Failure::NotOurs));
        assert_eq!(parse_reply(&bytes[..5], 7, "x.example"), Err(Failure::NotOurs));
    }

    #[test]
    fn short_records_are_malformed() {
        let (bytes, length) = reply(7, "x.example", 0, &[(1, &[1, 2, 3, 4])]);
        assert_eq!(parse_reply(&bytes[..length - 2], 7, "x.example"), Err(Failure::Malformed));
    }
}
