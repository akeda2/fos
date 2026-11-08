//! SIP digest authentication (RFC 2617 / RFC 3261 section 22): challenges and responses with MD5, `qop=auth` or the
//! old form without qop.
//!
//! The server side keeps no state. A nonce is `<time in seconds, 8 hex><MD5 over secret, time and the client's address,
//! 32 hex>`; it is valid for [`NONCE_SECS`] seconds and only for the address it was given to. The nonce count is not
//! tracked, so a captured response can be replayed from the same address within that window: fine on the trusted
//! segment this is for, not a defence against a hostile network.

use crate::md5::{hex, md5};

pub const NONCE_SECS: u32 = 120;
pub const NONCE_LEN: usize = 40;

/// The parameters of an `Authorization: Digest ...` header.
#[derive(Default, Debug)]
pub struct Credentials<'a> {
    pub username: &'a str,
    pub realm: &'a str,
    pub nonce: &'a str,
    pub uri: &'a str,
    pub response: &'a str,
    pub qop: &'a str,
    pub nc: &'a str,
    pub cnonce: &'a str,
}

impl<'a> Credentials<'a> {
    pub fn parse(header: &'a str) -> Option<Credentials<'a>> {
        let rest = header.trim().strip_prefix("Digest").or_else(|| header.trim().strip_prefix("digest"))?;
        let mut credentials = Credentials::default();
        let mut text = rest.trim_start();
        while !text.is_empty() {
            let (key, after) = text.split_once('=')?;
            let after = after.trim_start();
            let (value, tail) = if let Some(quoted) = after.strip_prefix('"') {
                let end = quoted.find('"')?;
                (&quoted[..end], &quoted[end + 1..])
            } else {
                let end = after.find(',').unwrap_or(after.len());
                (after[..end].trim(), &after[end..])
            };
            let key = key.trim();
            let slot = [("username", &mut credentials.username), ("realm", &mut credentials.realm), ("nonce", &mut credentials.nonce), ("uri", &mut credentials.uri), ("response", &mut credentials.response), ("qop", &mut credentials.qop), ("nc", &mut credentials.nc), ("cnonce", &mut credentials.cnonce)]
                .into_iter()
                .find(|(name, _)| key.eq_ignore_ascii_case(name));
            if let Some((_, field)) = slot {
                *field = value;
            }
            text = tail.trim_start().trim_start_matches(',').trim_start();
        }
        Some(credentials)
    }
}

/// The digest response for a request, as 32 hex characters.
#[allow(clippy::too_many_arguments)]
pub fn response(user: &str, realm: &str, password: &str, method: &str, uri: &str, nonce: &str, qop: &str, nc: &str, cnonce: &str) -> [u8; 32] {
    let ha1 = hex(&md5(&[user.as_bytes(), b":", realm.as_bytes(), b":", password.as_bytes()]));
    let ha2 = hex(&md5(&[method.as_bytes(), b":", uri.as_bytes()]));
    let digest = if qop.is_empty() {
        md5(&[&ha1, b":", nonce.as_bytes(), b":", &ha2])
    } else {
        md5(&[&ha1, b":", nonce.as_bytes(), b":", nc.as_bytes(), b":", cnonce.as_bytes(), b":", qop.as_bytes(), b":", &ha2])
    };
    hex(&digest)
}

/// A nonce for `client` at `now_secs`.
pub fn make_nonce(secret: u32, now_secs: u32, client: [u8; 4]) -> [u8; NONCE_LEN] {
    let mut out = [0u8; NONCE_LEN];
    for (i, digit) in out[..8].iter_mut().enumerate() {
        *digit = b"0123456789abcdef"[((now_secs >> (28 - 4 * i)) & 15) as usize];
    }
    let tag = hex(&md5(&[&secret.to_le_bytes(), &now_secs.to_le_bytes(), &client]));
    out[8..].copy_from_slice(&tag[..32]);
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NonceCheck {
    Good,
    /// Ours, but older than [`NONCE_SECS`]: challenge again with `stale=true`.
    Stale,
    Bad,
}

pub fn check_nonce(secret: u32, now_secs: u32, client: [u8; 4], nonce: &str) -> NonceCheck {
    if nonce.len() != NONCE_LEN {
        return NonceCheck::Bad;
    }
    let Ok(issued) = u32::from_str_radix(&nonce[..8], 16) else { return NonceCheck::Bad };
    if make_nonce(secret, issued, client) != nonce.as_bytes() {
        return NonceCheck::Bad;
    }
    if now_secs.wrapping_sub(issued) > NONCE_SECS { NonceCheck::Stale } else { NonceCheck::Good }
}

/// The password of `user` in a users file: lines of `user:password`, `#` starts a comment.
pub fn password_of<'a>(users: &'a str, user: &str) -> Option<&'a str> {
    users.lines().map(str::trim).filter(|line| !line.starts_with('#')).find_map(|line| {
        let (name, password) = line.split_once(':')?;
        (name == user).then_some(password)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc_2617_example() {
        // The worked example of RFC 2617 section 3.5 (Mufasa, qop=auth).
        let r = response("Mufasa", "testrealm@host.com", "Circle Of Life", "GET", "/dir/index.html", "dcd98b7102dd2f0e8b11d0f600bfb0c093", "auth", "00000001", "0a4f113b");
        assert_eq!(&r, b"6629fae49393a05397450978507c4ef1");
    }

    #[test]
    fn parses_quoted_and_bare_parameters() {
        let c = Credentials::parse("Digest username=\"op\", realm=\"fos\", nonce=\"abc\", uri=\"sip:fos\", response=\"00\", qop=auth, nc=00000001, cnonce=\"x\"").unwrap();
        assert_eq!((c.username, c.realm, c.qop, c.nc, c.cnonce, c.uri), ("op", "fos", "auth", "00000001", "x", "sip:fos"));
    }

    #[test]
    fn nonce_is_bound_to_address_and_time() {
        let nonce = make_nonce(7, 1000, [10, 0, 0, 1]);
        let text = core::str::from_utf8(&nonce).unwrap();
        assert_eq!(check_nonce(7, 1010, [10, 0, 0, 1], text), NonceCheck::Good);
        assert_eq!(check_nonce(7, 1000 + NONCE_SECS + 1, [10, 0, 0, 1], text), NonceCheck::Stale);
        assert_eq!(check_nonce(7, 1010, [10, 0, 0, 2], text), NonceCheck::Bad);
        assert_eq!(check_nonce(8, 1010, [10, 0, 0, 1], text), NonceCheck::Bad);
    }

    #[test]
    fn users_file() {
        let users = "# comment\nop:secret\nlab:pw:with:colons\n";
        assert_eq!(password_of(users, "op"), Some("secret"));
        assert_eq!(password_of(users, "lab"), Some("pw:with:colons"));
        assert_eq!(password_of(users, "nobody"), None);
    }
}
