//! Bash-style patterns for one path component (docs/fosfs-v4.md "Wildcards in shell tools"): `*`, `?`, `[abc]`, `[a-z]`,
//! `[!a-z]`, and a backslash that takes the next character literally. A pattern never sees a `/`: the shell splits a path
//! into components and matches each against one folder's listing.

/// Whether `pattern` has an unescaped `*`, `?` or a complete `[...]` set; a pattern without them is a plain name.
pub fn has_wildcards(pattern: &str) -> bool {
    let bytes = pattern.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 1,
            b'*' | b'?' => return true,
            b'[' if class(bytes, index + 1, 0, false).is_some() => return true,
            _ => {}
        }
        index += 1;
    }
    false
}

/// `pattern` without its escaping backslashes, written to `out`; the length, or `None` if `out` is too small.
pub fn unescape(pattern: &str, out: &mut [u8]) -> Option<usize> {
    let bytes = pattern.as_bytes();
    let (mut index, mut length) = (0, 0);
    while index < bytes.len() {
        if bytes[index] == b'\\' && index + 1 < bytes.len() {
            index += 1;
        }
        *out.get_mut(length)? = bytes[index];
        length += 1;
        index += 1;
    }
    Some(length)
}

fn same(a: u8, b: u8, fold: bool) -> bool {
    a == b || (fold && a.eq_ignore_ascii_case(&b))
}

/// The set that starts after a `[` at `start`: whether `byte` is in it and the index after the closing `]`; `None` when the
/// set is not closed (the `[` is then an ordinary character).
fn class(pattern: &[u8], start: usize, byte: u8, fold: bool) -> Option<(bool, usize)> {
    let mut index = start;
    let negate = matches!(pattern.get(index), Some(b'!' | b'^'));
    if negate {
        index += 1;
    }
    let first = index;
    let mut hit = false;
    loop {
        let low = *pattern.get(index)?;
        if low == b']' && index > first {
            return Some((hit != negate, index + 1));
        }
        index += 1;
        if pattern.get(index) == Some(&b'-') && pattern.get(index + 1).is_some_and(|&high| high != b']') {
            let high = pattern[index + 1];
            let lower = byte.to_ascii_lowercase();
            hit |= (low..=high).contains(&byte) || (fold && (low.to_ascii_lowercase()..=high.to_ascii_lowercase()).contains(&lower));
            index += 2;
        } else {
            hit |= same(low, byte, fold);
        }
    }
}

/// Whether `name` matches `pattern`; `fold` ignores ASCII case (FAT and exFAT volumes).
pub fn matches(pattern: &str, name: &[u8], fold: bool) -> bool {
    let pattern = pattern.as_bytes();
    let (mut at, mut on) = (0, 0);
    // Where the last `*` stood: the pattern index after it and the name index it has swallowed up to.
    let mut star: Option<(usize, usize)> = None;
    while on < name.len() {
        let step = match pattern.get(at) {
            Some(b'*') => {
                star = Some((at + 1, on));
                at += 1;
                continue;
            }
            Some(b'?') => Some(at + 1),
            Some(b'[') => match class(pattern, at + 1, name[on], fold) {
                Some((true, next)) => Some(next),
                Some((false, _)) => None,
                None => same(b'[', name[on], fold).then_some(at + 1),
            },
            Some(b'\\') if at + 1 < pattern.len() => same(pattern[at + 1], name[on], fold).then_some(at + 2),
            Some(&literal) => same(literal, name[on], fold).then_some(at + 1),
            None => None,
        };
        match (step, star) {
            (Some(next), _) => {
                at = next;
                on += 1;
            }
            (None, Some((resume, swallowed))) => {
                at = resume;
                on = swallowed + 1;
                star = Some((resume, on));
            }
            (None, None) => return false,
        }
    }
    pattern[at..].iter().all(|&byte| byte == b'*')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stars_and_questions() {
        assert!(matches("*", b"anything", false));
        assert!(matches("*", b"", false));
        assert!(matches("*.flac", b"lp1.flac", false));
        assert!(!matches("*.flac", b"lp1.flac.bak", false));
        assert!(matches("a*b*c", b"axxbyyc", false));
        assert!(!matches("a*b*c", b"axxbyy", false));
        assert!(matches("lp?.txt", b"lp1.txt", false));
        assert!(!matches("lp?.txt", b"lp12.txt", false));
        assert!(matches("*a*a", b"banana", false));
        assert!(!matches("", b"x", false));
        assert!(matches("", b"", false));
    }

    #[test]
    fn sets() {
        assert!(matches("lp[12].txt", b"lp2.txt", false));
        assert!(!matches("lp[12].txt", b"lp3.txt", false));
        assert!(matches("[a-c]x", b"bx", false));
        assert!(!matches("[a-c]x", b"dx", false));
        assert!(matches("[!a-c]x", b"dx", false));
        assert!(!matches("[!a-c]x", b"ax", false));
        assert!(matches("[]]x", b"]x", false));
        assert!(matches("[a-]x", b"-x", false));
        // an unclosed set is an ordinary character
        assert!(matches("[ab", b"[ab", false));
    }

    #[test]
    fn escapes_and_case() {
        assert!(matches("a\\*b", b"a*b", false));
        assert!(!matches("a\\*b", b"axb", false));
        assert!(!matches("A*", b"abc", false));
        assert!(matches("A*", b"abc", true));
        assert!(matches("[A-C]x", b"bx", true));
    }

    #[test]
    fn detects_wildcards() {
        assert!(has_wildcards("*.txt"));
        assert!(has_wildcards("a?"));
        assert!(has_wildcards("[ab]"));
        assert!(!has_wildcards("plain.txt"));
        assert!(!has_wildcards("a\\*b"));
        assert!(!has_wildcards("[ab"));
    }

    #[test]
    fn unescapes() {
        let mut out = [0u8; 8];
        let length = unescape("a\\*b", &mut out).unwrap();
        assert_eq!(&out[..length], b"a*b");
        assert!(unescape("toolongname", &mut out).is_none());
    }
}
