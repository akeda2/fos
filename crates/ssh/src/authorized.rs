//! The login table (`settings/authorized_keys`) and the host key file (`settings/ssh_host_key`).
//!
//! `authorized_keys` is one key per line, like OpenSSH's, with the options this server understands in front:
//!
//! ```text
//! user=anna,uid=1000,gid=1000 ssh-ed25519 AAAAC3Nza... anna@laptop
//! user=backup,uid=0,gid=0,ro ssh-ed25519 AAAAC3Nza... nightly job
//! ```
//!
//! `user=` is the login name (required); `uid=` and `gid=` default to 0 (an account that sees everything, so give your
//! users real ids); `ro` refuses every change. `#` starts a comment line. Only ssh-ed25519 keys are accepted.

use crate::session::Account;
use crate::wire::{Writer, base64_decode, base64_encode};
use sha2::{Digest, Sha256};

pub const KEYS: usize = 16;
const NAME_MAX: usize = 32;

#[derive(Clone, Copy)]
struct Entry {
    user: [u8; NAME_MAX],
    user_length: usize,
    key: [u8; 32],
    account: Account,
}

pub struct AuthorizedKeys {
    entries: [Entry; KEYS],
    count: usize,
}

impl Default for AuthorizedKeys {
    fn default() -> Self {
        Self::new()
    }
}

impl AuthorizedKeys {
    pub const fn new() -> AuthorizedKeys {
        let empty = Entry { user: [0; NAME_MAX], user_length: 0, key: [0; 32], account: Account { uid: 0, gid: 0, read_only: false, shell: false } };
        AuthorizedKeys { entries: [empty; KEYS], count: 0 }
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Adds the keys of `text`; the reason on the first line that is not one.
    pub fn parse(&mut self, text: &str) -> Result<(), &'static str> {
        for line in text.lines().map(str::trim).filter(|line| !line.is_empty() && !line.starts_with('#')) {
            let mut words = line.split_ascii_whitespace();
            let mut word = words.next().ok_or("an empty line")?;
            let mut entry = Entry { user: [0; NAME_MAX], user_length: 0, key: [0; 32], account: Account { uid: 0, gid: 0, read_only: false, shell: false } };
            if word != "ssh-ed25519" {
                if word.starts_with("ssh-") || word.starts_with("ecdsa-") {
                    return Err("authorized_keys: only ssh-ed25519 keys are accepted");
                }
                for option in word.split(',') {
                    match option.split_once('=') {
                        Some(("user", name)) if !name.is_empty() && name.len() <= NAME_MAX => {
                            entry.user[..name.len()].copy_from_slice(name.as_bytes());
                            entry.user_length = name.len();
                        }
                        Some(("uid", number)) => entry.account.uid = number.parse().map_err(|_| "authorized_keys: uid= takes a number")?,
                        Some(("gid", number)) => entry.account.gid = number.parse().map_err(|_| "authorized_keys: gid= takes a number")?,
                        None if option == "ro" => entry.account.read_only = true,
                        None if option == "shell" => entry.account.shell = true,
                        _ => return Err("authorized_keys: options are user=NAME, uid=N, gid=N, ro and shell"),
                    }
                }
                word = words.next().ok_or("authorized_keys: a line has options but no key")?;
                if word != "ssh-ed25519" {
                    return Err("authorized_keys: only ssh-ed25519 keys are accepted");
                }
            }
            if entry.user_length == 0 {
                return Err("authorized_keys: every key needs user=NAME in front");
            }
            let mut blob = [0u8; 80];
            let length = base64_decode(words.next().ok_or("authorized_keys: a key line has no key")?, &mut blob).ok_or("authorized_keys: the key is not base64")?;
            let mut reader = crate::wire::Reader::new(&blob[..length]);
            if reader.string() != Some(b"ssh-ed25519") {
                return Err("authorized_keys: the key is not an ssh-ed25519 key");
            }
            entry.key = reader.string().and_then(|key| key.try_into().ok()).ok_or("authorized_keys: the key is cut short")?;
            if self.count == KEYS {
                return Err("authorized_keys: at most 16 keys");
            }
            self.entries[self.count] = entry;
            self.count += 1;
        }
        Ok(())
    }

    pub fn lookup(&self, user: &str, key: &[u8; 32]) -> Option<Account> {
        // Every entry is compared, so the time does not say which login names exist.
        let mut found = None;
        for entry in &self.entries[..self.count] {
            if &entry.user[..entry.user_length] == user.as_bytes() && &entry.key == key {
                found = Some(entry.account);
            }
        }
        found
    }
}

/// A host key seed from its file: 64 hex digits (whitespace around is fine).
pub fn parse_seed(text: &str) -> Option<[u8; 32]> {
    let text = text.trim();
    if text.len() != 64 {
        return None;
    }
    let mut seed = [0u8; 32];
    for (index, byte) in seed.iter_mut().enumerate() {
        *byte = u8::from_str_radix(text.get(index * 2..index * 2 + 2)?, 16).ok()?;
    }
    Some(seed)
}

/// `seed` as the 64 hex digits of the key file.
pub fn seed_text(seed: &[u8; 32], out: &mut [u8; 65]) {
    for (index, byte) in seed.iter().enumerate() {
        out[index * 2] = b"0123456789abcdef"[(byte >> 4) as usize];
        out[index * 2 + 1] = b"0123456789abcdef"[(byte & 15) as usize];
    }
    out[64] = b'\n';
}

/// OpenSSH's fingerprint of a host's or user's public key (`SHA256:` and 43 base64 characters).
pub fn fingerprint_public(public: &[u8; 32], out: &mut [u8; 50]) {
    let mut blob = [0u8; 64];
    let mut writer = Writer::new(&mut blob);
    writer.string(b"ssh-ed25519").string(public);
    let digest: [u8; 32] = Sha256::digest(writer.written()).into();
    out[..7].copy_from_slice(b"SHA256:");
    let mut text = [0u8; 44];
    base64_encode(&digest, &mut text);
    out[7..].copy_from_slice(&text[..43]);
}

/// The fingerprint of the key a seed makes.
pub fn fingerprint(seed: &[u8; 32], out: &mut [u8; 50]) {
    fingerprint_public(ed25519_dalek::SigningKey::from_bytes(seed).verifying_key().as_bytes(), out);
}

/// The `authorized_keys` form of the key a seed makes, `ssh-ed25519 BASE64` (80 bytes).
pub fn public_line(seed: &[u8; 32], out: &mut [u8; 80]) -> usize {
    let public = ed25519_dalek::SigningKey::from_bytes(seed).verifying_key();
    let mut blob = [0u8; 51];
    let mut writer = Writer::new(&mut blob);
    writer.string(b"ssh-ed25519").string(public.as_bytes());
    let mut text = [0u8; 68];
    base64_encode(writer.written(), &mut text);
    out[..12].copy_from_slice(b"ssh-ed25519 ");
    out[12..].copy_from_slice(&text);
    80
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl";

    #[test]
    fn parses_options_and_finds_the_key() {
        let mut keys = AuthorizedKeys::new();
        let text = format!("# comment\nuser=anna,uid=1000,gid=100 ssh-ed25519 {KEY} anna@laptop\nuser=bob,ro ssh-ed25519 {KEY}\n");
        keys.parse(&text).unwrap();
        assert_eq!(keys.len(), 2);
        let mut blob = [0u8; 80];
        let length = base64_decode(KEY, &mut blob).unwrap();
        let key: [u8; 32] = blob[length - 32..length].try_into().unwrap();
        assert_eq!(keys.lookup("anna", &key), Some(Account { uid: 1000, gid: 100, read_only: false, shell: false }));
        assert_eq!(keys.lookup("bob", &key), Some(Account { uid: 0, gid: 0, read_only: true, shell: false }));
        assert_eq!(keys.lookup("eve", &key), None);
        assert_eq!(keys.lookup("anna", &[0; 32]), None);
    }

    #[test]
    fn refuses_what_it_cannot_use() {
        for text in [
            format!("ssh-ed25519 {KEY}"),
            format!("user=a ssh-rsa {KEY}"),
            format!("user=a,uid=x ssh-ed25519 {KEY}"),
            format!("user=a,flag ssh-ed25519 {KEY}"),
            "user=a ssh-ed25519 !!!".to_string(),
        ] {
            assert!(AuthorizedKeys::new().parse(&text).is_err(), "{text}");
        }
    }

    #[test]
    fn seed_round_trips() {
        let seed = [0xab; 32];
        let mut text = [0u8; 65];
        seed_text(&seed, &mut text);
        assert_eq!(parse_seed(core::str::from_utf8(&text).unwrap()), Some(seed));
        assert_eq!(parse_seed("zz"), None);
    }
}
