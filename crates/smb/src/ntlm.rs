//! The cryptography NTLMv2 needs, without any dependency: MD4 (the NT hash), MD5 and HMAC-MD5 (the v2 response), and the
//! NTLMv2 computation itself (MS-NLMP 3.3.2). Small and unhurried rather than fast: it runs once per login.

fn md4_block(state: &mut [u32; 4], block: &[u8]) {
    let mut x = [0u32; 16];
    for (i, word) in x.iter_mut().enumerate() {
        *word = u32::from_le_bytes([block[i * 4], block[i * 4 + 1], block[i * 4 + 2], block[i * 4 + 3]]);
    }
    let [mut a, mut b, mut c, mut d] = *state;
    let f = |x: u32, y: u32, z: u32| (x & y) | (!x & z);
    let g = |x: u32, y: u32, z: u32| (x & y) | (x & z) | (y & z);
    let h = |x: u32, y: u32, z: u32| x ^ y ^ z;
    for i in 0..4 {
        let k = i * 4;
        a = a.wrapping_add(f(b, c, d)).wrapping_add(x[k]).rotate_left(3);
        d = d.wrapping_add(f(a, b, c)).wrapping_add(x[k + 1]).rotate_left(7);
        c = c.wrapping_add(f(d, a, b)).wrapping_add(x[k + 2]).rotate_left(11);
        b = b.wrapping_add(f(c, d, a)).wrapping_add(x[k + 3]).rotate_left(19);
    }
    for i in 0..4 {
        a = a.wrapping_add(g(b, c, d)).wrapping_add(x[i]).wrapping_add(0x5a82_7999).rotate_left(3);
        d = d.wrapping_add(g(a, b, c)).wrapping_add(x[i + 4]).wrapping_add(0x5a82_7999).rotate_left(5);
        c = c.wrapping_add(g(d, a, b)).wrapping_add(x[i + 8]).wrapping_add(0x5a82_7999).rotate_left(9);
        b = b.wrapping_add(g(c, d, a)).wrapping_add(x[i + 12]).wrapping_add(0x5a82_7999).rotate_left(13);
    }
    for &i in &[0usize, 2, 1, 3] {
        a = a.wrapping_add(h(b, c, d)).wrapping_add(x[i]).wrapping_add(0x6ed9_eba1).rotate_left(3);
        d = d.wrapping_add(h(a, b, c)).wrapping_add(x[i + 8]).wrapping_add(0x6ed9_eba1).rotate_left(9);
        c = c.wrapping_add(h(d, a, b)).wrapping_add(x[i + 4]).wrapping_add(0x6ed9_eba1).rotate_left(11);
        b = b.wrapping_add(h(c, d, a)).wrapping_add(x[i + 12]).wrapping_add(0x6ed9_eba1).rotate_left(15);
    }
    state[0] = state[0].wrapping_add(a);
    state[1] = state[1].wrapping_add(b);
    state[2] = state[2].wrapping_add(c);
    state[3] = state[3].wrapping_add(d);
}

const MD5_K: [u32; 64] = [
    0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501,
    0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821,
    0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8,
    0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed, 0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a,
    0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70,
    0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665,
    0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
    0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb, 0xeb86d391,
];
const MD5_S: [u32; 16] = [7, 12, 17, 22, 5, 9, 14, 20, 4, 11, 16, 23, 6, 10, 15, 21];

fn md5_block(state: &mut [u32; 4], block: &[u8]) {
    let mut m = [0u32; 16];
    for (i, word) in m.iter_mut().enumerate() {
        *word = u32::from_le_bytes([block[i * 4], block[i * 4 + 1], block[i * 4 + 2], block[i * 4 + 3]]);
    }
    let [mut a, mut b, mut c, mut d] = *state;
    for i in 0..64 {
        let (f, g) = match i / 16 {
            0 => ((b & c) | (!b & d), i),
            1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
            2 => (b ^ c ^ d, (3 * i + 5) % 16),
            _ => (c ^ (b | !d), (7 * i) % 16),
        };
        let sum = a.wrapping_add(f).wrapping_add(MD5_K[i]).wrapping_add(m[g]);
        a = d;
        d = c;
        c = b;
        b = b.wrapping_add(sum.rotate_left(MD5_S[(i / 16) * 4 + i % 4]));
    }
    state[0] = state[0].wrapping_add(a);
    state[1] = state[1].wrapping_add(b);
    state[2] = state[2].wrapping_add(c);
    state[3] = state[3].wrapping_add(d);
}

/// MD4 or MD5 over the pieces of `parts` taken as one message (both pad alike: 0x80, zeros, the bit length LE).
fn merkle(parts: &[&[u8]], init: [u32; 4], block: fn(&mut [u32; 4], &[u8])) -> [u8; 16] {
    let mut state = init;
    let mut pending = [0u8; 128];
    let mut held = 0;
    let mut total: u64 = 0;
    for part in parts {
        for &byte in *part {
            pending[held] = byte;
            held += 1;
            total += 1;
            if held == 64 {
                block(&mut state, &pending[..64]);
                held = 0;
            }
        }
    }
    pending[held] = 0x80;
    let end = if held < 56 { 64 } else { 128 };
    for byte in &mut pending[held + 1..end] {
        *byte = 0;
    }
    pending[end - 8..end].copy_from_slice(&(total * 8).to_le_bytes());
    for chunk in pending[..end].chunks(64) {
        block(&mut state, chunk);
    }
    let mut out = [0u8; 16];
    for (i, word) in state.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
    out
}

pub fn md4(data: &[u8]) -> [u8; 16] {
    merkle(&[data], [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476], md4_block)
}

pub fn md5(parts: &[&[u8]]) -> [u8; 16] {
    merkle(parts, [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476], md5_block)
}

pub fn hmac_md5(key: &[u8], parts: &[&[u8]]) -> [u8; 16] {
    let mut block = [0u8; 64];
    if key.len() > 64 {
        block[..16].copy_from_slice(&md5(&[key]));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner = [0x36u8; 64];
    let mut outer = [0x5cu8; 64];
    for i in 0..64 {
        inner[i] ^= block[i];
        outer[i] ^= block[i];
    }
    let mut chain = [&inner[..]; 9];
    let count = parts.len().min(8);
    chain[1..=count].copy_from_slice(&parts[..count]);
    let inner_hash = md5(&chain[..=count]);
    md5(&[&outer, &inner_hash])
}

/// The NT hash: MD4 of the password as UTF-16LE (MS-NLMP 3.3.1). Passwords are cut at 64 characters.
pub fn nt_hash(password: &str) -> [u8; 16] {
    let mut utf16 = [0u8; 128];
    let mut length = 0;
    for unit in password.encode_utf16().take(64) {
        utf16[length..length + 2].copy_from_slice(&unit.to_le_bytes());
        length += 2;
    }
    md4(&utf16[..length])
}

/// The NTLMv2 key: HMAC-MD5 of the upper-cased user name and the domain (both UTF-16LE) keyed by the NT hash.
pub fn ntlmv2_key(nt_hash: &[u8; 16], user: &[u8], domain: &[u8]) -> [u8; 16] {
    let mut upper = [0u8; 128];
    let user = &user[..user.len().min(128)];
    for (i, pair) in user.chunks_exact(2).enumerate() {
        let unit = u16::from_le_bytes([pair[0], pair[1]]);
        let unit = if (b'a' as u16..=b'z' as u16).contains(&unit) { unit - 32 } else { unit };
        upper[i * 2..i * 2 + 2].copy_from_slice(&unit.to_le_bytes());
    }
    hmac_md5(nt_hash, &[&upper[..user.len() & !1], domain])
}

/// The NTProofStr a client sends and a server recomputes: HMAC-MD5 over the server challenge and the blob.
pub fn ntlmv2_proof(key: &[u8; 16], challenge: &[u8; 8], blob: &[u8]) -> [u8; 16] {
    hmac_md5(key, &[challenge, blob])
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::{format, string::String, vec::Vec};
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn md4_vectors() {
        assert_eq!(hex(&md4(b"")), "31d6cfe0d16ae931b73c59d7e0c089c0");
        assert_eq!(hex(&md4(b"abc")), "a448017aaf21d8525fc10ae87aa6729d");
        assert_eq!(
            hex(&md4(b"12345678901234567890123456789012345678901234567890123456789012345678901234567890")),
            "e33b4ddc9c38f2199c3e7b164fcc0536"
        );
    }

    #[test]
    fn md5_vectors() {
        assert_eq!(hex(&md5(&[b""])), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(hex(&md5(&[b"ab", b"c"])), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            hex(&md5(&[b"12345678901234567890123456789012345678901234567890123456789012345678901234567890"])),
            "57edf4a22be3c955ac49da2e2107b67a"
        );
    }

    #[test]
    fn hmac_md5_rfc2202() {
        assert_eq!(hex(&hmac_md5(&[0x0b; 16], &[b"Hi There"])), "9294727a3638bb1c13f48ef8158bfc9d");
        assert_eq!(hex(&hmac_md5(b"Jefe", &[b"what do ya want ", b"for nothing?"])), "750c783e6ab0b503eaa86e310a5db738");
        // A key longer than the block is hashed first.
        assert_eq!(
            hex(&hmac_md5(&[0xaa; 80], &[b"Test Using Larger Than Block-Size Key - Hash Key First"])),
            "6b1ab7fe4bd7bf8f0b62e6ce61b9d0cd"
        );
    }

    // MS-NLMP 4.2.4: User "User", domain "Domain", password "Password".
    #[test]
    fn ntlmv2_spec_vector() {
        let hash = nt_hash("Password");
        assert_eq!(hex(&hash), "a4f49c406510bdcab6824ee7c30fd852");
        let utf16 = |s: &str| -> Vec<u8> { s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect() };
        let key = ntlmv2_key(&hash, &utf16("User"), &utf16("Domain"));
        assert_eq!(hex(&key), "0c868a403bfd7a93a3001ef22ef02e3f");
    }
}
