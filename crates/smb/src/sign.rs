//! SMB 2/3 message signing (MS-SMB2 3.1.4.1): HMAC-SHA-256 for SMB 2.0.2 and 2.1, AES-128-CMAC with a derived key for
//! SMB 3.0. A client that logged in with a password signs its requests and insists on signed answers, so a server that
//! checks passwords has to sign. No allocation; the AES S-box is computed at compile time.
use tape_sync::hmac_sha256;

const fn xtime(byte: u8) -> u8 {
    (byte << 1) ^ if byte & 0x80 != 0 { 0x1b } else { 0 }
}

/// The AES S-box: the multiplicative inverse in GF(2^8), then the affine transform (FIPS-197 5.1.1).
const SBOX: [u8; 256] = {
    let mut table = [0u8; 256];
    let mut i = 1;
    while i < 256 {
        // The inverse is the one value whose product with i is 1.
        let mut candidate = 1;
        while candidate < 256 {
            let (mut a, mut b, mut product) = (i as u8, candidate as u8, 0u8);
            while b != 0 {
                if b & 1 != 0 {
                    product ^= a;
                }
                a = xtime(a);
                b >>= 1;
            }
            if product == 1 {
                break;
            }
            candidate += 1;
        }
        let inverse = candidate as u8;
        table[i] = inverse ^ inverse.rotate_left(1) ^ inverse.rotate_left(2) ^ inverse.rotate_left(3) ^ inverse.rotate_left(4) ^ 0x63;
        i += 1;
    }
    table[0] = 0x63;
    table
};

/// AES-128 with the round keys expanded once.
pub struct Aes128 {
    round_keys: [[u8; 16]; 11],
}

impl Aes128 {
    pub fn new(key: &[u8; 16]) -> Self {
        let mut words = [[0u8; 4]; 44];
        for i in 0..4 {
            words[i].copy_from_slice(&key[i * 4..i * 4 + 4]);
        }
        let mut rcon = 1u8;
        for i in 4..44 {
            let mut word = words[i - 1];
            if i % 4 == 0 {
                word = [SBOX[word[1] as usize] ^ rcon, SBOX[word[2] as usize], SBOX[word[3] as usize], SBOX[word[0] as usize]];
                rcon = xtime(rcon);
            }
            for j in 0..4 {
                words[i][j] = words[i - 4][j] ^ word[j];
            }
        }
        let mut round_keys = [[0u8; 16]; 11];
        for (round, round_key) in round_keys.iter_mut().enumerate() {
            for column in 0..4 {
                round_key[column * 4..column * 4 + 4].copy_from_slice(&words[round * 4 + column]);
            }
        }
        Aes128 { round_keys }
    }

    pub fn encrypt(&self, block: &mut [u8; 16]) {
        add(block, &self.round_keys[0]);
        for round in 1..=10 {
            for byte in block.iter_mut() {
                *byte = SBOX[*byte as usize];
            }
            // ShiftRows: state byte (row r, column c) is block[c * 4 + r], row r rotates left by r columns.
            let old = *block;
            for column in 0..4 {
                for row in 0..4 {
                    block[column * 4 + row] = old[((column + row) % 4) * 4 + row];
                }
            }
            if round != 10 {
                for column in block.chunks_exact_mut(4) {
                    let [a, b, c, d] = [column[0], column[1], column[2], column[3]];
                    column[0] = xtime(a) ^ (xtime(b) ^ b) ^ c ^ d;
                    column[1] = a ^ xtime(b) ^ (xtime(c) ^ c) ^ d;
                    column[2] = a ^ b ^ xtime(c) ^ (xtime(d) ^ d);
                    column[3] = (xtime(a) ^ a) ^ b ^ c ^ xtime(d);
                }
            }
            add(block, &self.round_keys[round]);
        }
    }
}

fn add(block: &mut [u8; 16], key: &[u8; 16]) {
    for (byte, k) in block.iter_mut().zip(key) {
        *byte ^= k;
    }
}

fn double(block: &[u8; 16]) -> [u8; 16] {
    let mut out = [0u8; 16];
    let mut carry = 0;
    for i in (0..16).rev() {
        out[i] = (block[i] << 1) | carry;
        carry = block[i] >> 7;
    }
    if carry != 0 {
        out[15] ^= 0x87;
    }
    out
}

/// AES-128-CMAC (RFC 4493) of the concatenation of `parts`: the last block is held back until the end is known.
pub fn cmac(key: &[u8; 16], parts: &[&[u8]]) -> [u8; 16] {
    let aes = Aes128::new(key);
    let mut zero = [0u8; 16];
    aes.encrypt(&mut zero);
    let first = double(&zero);
    let second = double(&first);
    let (mut state, mut pending, mut held) = ([0u8; 16], [0u8; 16], 0usize);
    for &byte in parts.iter().flat_map(|part| part.iter()) {
        if held == 16 {
            add(&mut state, &pending);
            aes.encrypt(&mut state);
            held = 0;
        }
        pending[held] = byte;
        held += 1;
    }
    if held == 16 {
        add(&mut pending, &first);
    } else {
        pending[held] = 0x80;
        pending[held + 1..].fill(0);
        add(&mut pending, &second);
    }
    add(&mut state, &pending);
    aes.encrypt(&mut state);
    state
}

/// The SMB 3.0 signing key from the session key (SP 800-108 counter mode over HMAC-SHA-256, MS-SMB2 3.1.4.2).
pub fn signing_key_v3(session_key: &[u8; 16]) -> [u8; 16] {
    let mut key = [0u8; 16];
    let digest = hmac_sha256(session_key, &[&1u32.to_be_bytes(), b"SMB2AESCMAC\0", &[0], b"SmbSign\0", &128u32.to_be_bytes()]);
    key.copy_from_slice(&digest[..16]);
    key
}

/// The 16 signature bytes of a whole SMB2 message, given as the parts around its signature field (header bytes 48..64,
/// which count as zero), for `dialect`.
pub fn signature(dialect: u16, key: &[u8; 16], message: &[u8]) -> [u8; 16] {
    let parts: [&[u8]; 3] = [&message[..48], &[0u8; 16], &message[64..]];
    if dialect >= 0x0300 {
        cmac(key, &parts)
    } else {
        let mut out = [0u8; 16];
        out.copy_from_slice(&hmac_sha256(key, &parts)[..16]);
        out
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;

    fn unhex(text: &str) -> std::vec::Vec<u8> {
        (0..text.len() / 2).map(|i| u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).unwrap()).collect()
    }

    #[test]
    fn aes_fips_197() {
        let key: [u8; 16] = core::array::from_fn(|i| i as u8);
        let mut block: [u8; 16] = core::array::from_fn(|i| (i * 0x11) as u8);
        Aes128::new(&key).encrypt(&mut block);
        assert_eq!(block.to_vec(), unhex("69c4e0d86a7b0430d8cdb78070b4c55a"));
    }

    #[test]
    fn cmac_rfc_4493() {
        let key: [u8; 16] = unhex("2b7e151628aed2a6abf7158809cf4f3c").try_into().unwrap();
        let message = unhex("6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e5130c81c46a35ce411");
        assert_eq!(cmac(&key, &[&[]]).to_vec(), unhex("bb1d6929e95937287fa37d129b756746"));
        assert_eq!(cmac(&key, &[&message[..16]]).to_vec(), unhex("070a16b46b4d4144f79bdd9dd04a287c"));
        assert_eq!(cmac(&key, &[&message[..20]]).to_vec(), unhex("7d85449ea6ea19c823a7bf78837dfade"));
        assert_eq!(cmac(&key, &[&message[..40]]).to_vec(), unhex("dfa66747de9ae63030ca32611497c827"));
    }
}
