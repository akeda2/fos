//! Tape synchronisation between machines, the parts that do not touch hardware.
//!
//! The protocol (kernel/src/term.rs `tapesync_*`): the machine that wants tapes sends `TAG_SYNC_REQUEST` with a random
//! 16-byte nonce; the other one answers (as a GET stream) with a *manifest*: one entry per stamped tape on its boot
//! volume (name, size, build time, ABI version, SHA-256), followed by HMAC-SHA-256 over `nonce || manifest` with the
//! shared secret in the `synckey` file. The requester only trusts a manifest whose HMAC checks out for *its* nonce, then
//! fetches files with plain NETFS and accepts each only if its SHA-256 equals the signed one. So the secret never
//! travels, a replayed manifest fails (new nonce), and a forged or damaged file cannot be installed.
//!
//! A *stamp* is 16 bytes in the ELF section `.fos_build`, added after linking by `tools/stamp_tape.py`: the build
//! time (seconds since 1970; the commit time for a clean tree, so two machines building one commit agree), the tape
//! ABI version and CPU it was built for, and three reserved bytes. Only files with a stamp are tapes as far as sync is concerned.

#![cfg_attr(not(test), no_std)]

pub const SECTION: &str = ".fos_build";
pub const STAMP_LEN: usize = 16;
pub const NONCE_LEN: usize = 16;
pub const MAC_LEN: usize = 32;
pub const NAME_LEN: usize = 16;
pub const MAX_TAPES: usize = 64;

const MAGIC: &[u8; 8] = b"FOSSYNC1";
const HEADER_LEN: usize = 16;
const ENTRY_LEN: usize = 64;
/// The biggest manifest: header, every entry, the HMAC.
pub const MANIFEST_MAX: usize = HEADER_LEN + MAX_TAPES * ENTRY_LEN + MAC_LEN;

// ---- SHA-256 (FIPS 180-4) ----

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be,
    0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa,
    0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85,
    0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3,
    0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f,
    0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// Incremental SHA-256.
pub struct Sha256 {
    state: [u32; 8],
    block: [u8; 64],
    filled: usize,
    total: u64,
}

impl Sha256 {
    pub fn new() -> Sha256 {
        Sha256 {
            state: [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19],
            block: [0; 64],
            filled: 0,
            total: 0,
        }
    }

    fn compress(state: &mut [u32; 8], block: &[u8]) {
        let mut w = [0u32; 64];
        for (i, word) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = h.wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (slot, value) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(value);
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.total += data.len() as u64;
        if self.filled > 0 {
            let take = (64 - self.filled).min(data.len());
            self.block[self.filled..self.filled + take].copy_from_slice(&data[..take]);
            self.filled += take;
            data = &data[take..];
            if self.filled < 64 {
                return;
            }
            let block = self.block;
            Self::compress(&mut self.state, &block);
            self.filled = 0;
        }
        let mut blocks = data.chunks_exact(64);
        for block in &mut blocks {
            Self::compress(&mut self.state, block);
        }
        let rest = blocks.remainder();
        self.block[..rest.len()].copy_from_slice(rest);
        self.filled = rest.len();
    }

    pub fn finish(mut self) -> [u8; 32] {
        let bits = self.total * 8;
        let mut pad = [0u8; 72];
        pad[0] = 0x80;
        let zeros = (119 - self.filled) % 64;
        pad[zeros + 1..zeros + 9].copy_from_slice(&bits.to_be_bytes());
        // `update` would count the padding as data, which only matters to `total`, no longer read.
        self.update(&pad[..zeros + 9]);
        let mut out = [0u8; 32];
        for (chunk, word) in out.chunks_exact_mut(4).zip(self.state) {
            chunk.copy_from_slice(&word.to_be_bytes());
        }
        out
    }
}

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(data);
    hash.finish()
}

/// HMAC-SHA-256 (RFC 2104) over the concatenation of `parts`.
pub fn hmac_sha256(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut block = [0u8; 64];
    if key.len() > 64 {
        block[..32].copy_from_slice(&sha256(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(&block.map(|byte| byte ^ 0x36));
    for part in parts {
        inner.update(part);
    }
    let inner = inner.finish();
    let mut outer = Sha256::new();
    outer.update(&block.map(|byte| byte ^ 0x5c));
    outer.update(&inner);
    outer.finish()
}

/// Comparison that does not stop at the first difference.
pub fn equal(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

// ---- The build stamp ----

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Stamp {
    pub build: u64,
    pub abi: u32,
    /// The CPU the tape is built for (`ARCH_*`). Stamps made before this field existed have 0, which is x86-64.
    pub arch: u8,
}

pub const ARCH_X86_64: u8 = 0;
pub const ARCH_AARCH64: u8 = 1;

/// The CPU this build runs on: `decide` only offers tapes for it.
#[cfg(target_arch = "aarch64")]
pub const HOST_ARCH: u8 = ARCH_AARCH64;
#[cfg(not(target_arch = "aarch64"))]
pub const HOST_ARCH: u8 = ARCH_X86_64;

impl Stamp {
    pub fn encode(&self) -> [u8; STAMP_LEN] {
        let mut out = [0u8; STAMP_LEN];
        out[0..8].copy_from_slice(&self.build.to_le_bytes());
        out[8..12].copy_from_slice(&self.abi.to_le_bytes());
        out[12] = self.arch;
        out
    }

    pub fn decode(bytes: &[u8]) -> Option<Stamp> {
        if bytes.len() != STAMP_LEN {
            return None;
        }
        Some(Stamp { build: u64::from_le_bytes(bytes[0..8].try_into().ok()?), abi: u32::from_le_bytes(bytes[8..12].try_into().ok()?), arch: bytes[12] })
    }
}

// ---- The manifest ----

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Entry {
    pub name: [u8; NAME_LEN],
    pub length: u32,
    pub stamp: Stamp,
    pub sha256: [u8; 32],
}

impl Entry {
    pub const EMPTY: Entry = Entry { name: [0; NAME_LEN], length: 0, stamp: Stamp { build: 0, abi: 0, arch: 0 }, sha256: [0; 32] };

    pub fn name_bytes(&self) -> &[u8] {
        let end = self.name.iter().position(|&byte| byte == 0).unwrap_or(NAME_LEN);
        &self.name[..end]
    }
}

/// Writes the manifest for `entries` (at most `MAX_TAPES`) into `out`, signed for `nonce`; returns its length.
pub fn encode(entries: &[Entry], key: &[u8], nonce: &[u8; NONCE_LEN], out: &mut [u8; MANIFEST_MAX]) -> usize {
    let count = entries.len().min(MAX_TAPES);
    out[..HEADER_LEN].fill(0);
    out[..8].copy_from_slice(MAGIC);
    out[8..10].copy_from_slice(&(count as u16).to_le_bytes());
    for (index, entry) in entries[..count].iter().enumerate() {
        let at = HEADER_LEN + index * ENTRY_LEN;
        out[at..at + NAME_LEN].copy_from_slice(&entry.name);
        out[at + 16..at + 20].copy_from_slice(&entry.length.to_le_bytes());
        out[at + 20..at + 28].copy_from_slice(&entry.stamp.build.to_le_bytes());
        // The manifest entry has no spare bytes: the CPU rides in bits 16-23 of the ABI word. A machine that predates
        // it reads such a tape as built for an impossibly new ABI and skips it, which is the safe outcome.
        let word = entry.stamp.abi | u32::from(entry.stamp.arch) << 16;
        out[at + 28..at + 32].copy_from_slice(&word.to_le_bytes());
        out[at + 32..at + 64].copy_from_slice(&entry.sha256);
    }
    let body = HEADER_LEN + count * ENTRY_LEN;
    let mac = hmac_sha256(key, &[nonce, &out[..body]]);
    out[body..body + MAC_LEN].copy_from_slice(&mac);
    body + MAC_LEN
}

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    Malformed,
    BadSignature,
}

/// Checks `bytes` against `key` and the `nonce` that was sent; fills `entries` and returns how many.
pub fn decode(bytes: &[u8], key: &[u8], nonce: &[u8; NONCE_LEN], entries: &mut [Entry; MAX_TAPES]) -> Result<usize, Error> {
    if bytes.len() < HEADER_LEN + MAC_LEN || &bytes[..8] != MAGIC {
        return Err(Error::Malformed);
    }
    let count = usize::from(u16::from_le_bytes([bytes[8], bytes[9]]));
    let body = HEADER_LEN + count * ENTRY_LEN;
    if count > MAX_TAPES || bytes.len() != body + MAC_LEN {
        return Err(Error::Malformed);
    }
    if !equal(&hmac_sha256(key, &[nonce, &bytes[..body]]), &bytes[body..]) {
        return Err(Error::BadSignature);
    }
    for (index, entry) in entries.iter_mut().take(count).enumerate() {
        let at = HEADER_LEN + index * ENTRY_LEN;
        entry.name.copy_from_slice(&bytes[at..at + NAME_LEN]);
        entry.length = u32::from_le_bytes(bytes[at + 16..at + 20].try_into().unwrap());
        let word = u32::from_le_bytes(bytes[at + 28..at + 32].try_into().unwrap());
        entry.stamp = Stamp { build: u64::from_le_bytes(bytes[at + 20..at + 28].try_into().unwrap()), abi: word & 0xffff, arch: (word >> 16) as u8 };
        entry.sha256.copy_from_slice(&bytes[at + 32..at + 64]);
    }
    Ok(count)
}

// ---- The decision ----

/// What to do about one tape the other machine has.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    /// Not here: fetch it.
    New,
    /// Here, older than theirs: replace it.
    Newer,
    /// Same bytes.
    Same,
    /// Here and newer (or as new) with other bytes: leave ours.
    Keep,
    /// Same build time but different bytes (another toolchain or a dirty tree): leave ours, say so.
    Differs,
    /// Built for a newer tape ABI than this kernel offers: it could not run here.
    TooNew,
    /// A file of that name exists here and is not a stamped tape: never overwritten.
    NotATape,
    /// The name is not a valid FOSFS name.
    BadName,
    /// Built for another CPU (an x86-64 tape offered to an ARM machine, or the reverse): it could not run here.
    OtherCpu,
}

/// `local` is this machine's stamped entry of the same name, `local_exists` whether *any* file has that name.
pub fn decide(remote: &Entry, local: Option<&Entry>, local_exists: bool, kernel_abi: u32, kernel_arch: u8) -> Action {
    let name = remote.name_bytes();
    if name.is_empty() || name.len() > NAME_LEN || !name.iter().all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-')) {
        return Action::BadName;
    }
    if remote.stamp.arch != kernel_arch {
        return Action::OtherCpu;
    }
    if remote.stamp.abi > kernel_abi {
        return Action::TooNew;
    }
    match local {
        None if local_exists => Action::NotATape,
        None => Action::New,
        Some(mine) if mine.sha256 == remote.sha256 => Action::Same,
        Some(mine) if remote.stamp.build > mine.stamp.build => Action::Newer,
        Some(mine) if remote.stamp.build == mine.stamp.build => Action::Differs,
        Some(_) => Action::Keep,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> std::string::String {
        bytes.iter().map(|b| std::format!("{b:02x}")).collect()
    }

    #[test]
    fn sha256_known_answers() {
        assert_eq!(hex(&sha256(b"")), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(hex(&sha256(b"abc")), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(
            hex(&sha256(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq")),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn sha256_incremental_matches_one_shot() {
        let data: std::vec::Vec<u8> = (0..1000u32).map(|i| (i * 7) as u8).collect();
        for split in [0, 1, 55, 56, 63, 64, 65, 500, 999, 1000] {
            let mut hash = Sha256::new();
            hash.update(&data[..split]);
            hash.update(&data[split..]);
            assert_eq!(hash.finish(), sha256(&data), "split {split}");
        }
        // The padding boundary cases: lengths 55, 56 and 64.
        assert_eq!(hex(&sha256(&[b'a'; 55])), "9f4390f8d30c2dd92ec9f095b65e2b9ae9b0a925a5258e241c9f1e910f734318");
        assert_eq!(hex(&sha256(&[b'a'; 56])), "b35439a4ac6f0948b6d6f9e3c6af0f5f590ce20f1bde7090ef7970686ec6738a");
        assert_eq!(hex(&sha256(&[b'a'; 64])), "ffe054fe7ae0cb6dc65c3af9b61d5209f439851db43d0ba5997337df154668eb");
    }

    #[test]
    fn hmac_rfc4231() {
        assert_eq!(hex(&hmac_sha256(&[0x0b; 20], &[b"Hi There"])), "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7");
        assert_eq!(
            hex(&hmac_sha256(b"Jefe", &[b"what do ya want ", b"for nothing?"])),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // A key longer than the block is hashed first (RFC 4231 case 6).
        assert_eq!(
            hex(&hmac_sha256(&[0xaa; 131], &[b"Test Using Larger Than Block-Size Key - Hash Key First"])),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    fn entry(name: &str, build: u64, abi: u32, fill: u8) -> Entry {
        let mut e = Entry::EMPTY;
        e.name[..name.len()].copy_from_slice(name.as_bytes());
        e.length = 1234;
        e.stamp = Stamp { build, abi, arch: ARCH_X86_64 };
        e.sha256 = [fill; 32];
        e
    }

    #[test]
    fn stamp_round_trip() {
        let stamp = Stamp { build: 1_760_000_000, abi: 8, arch: ARCH_AARCH64 };
        assert_eq!(Stamp::decode(&stamp.encode()), Some(stamp));
        assert_eq!(Stamp::decode(&[0; 15]), None);
    }

    #[test]
    fn manifest_round_trip_and_rejections() {
        let entries = [entry("play", 100, 8, 1), entry("record", 200, 7, 2)];
        let nonce = [9u8; NONCE_LEN];
        let mut wire = [0u8; MANIFEST_MAX];
        let length = encode(&entries, b"secret", &nonce, &mut wire);
        assert_eq!(length, HEADER_LEN + 2 * ENTRY_LEN + MAC_LEN);
        let mut out = [Entry::EMPTY; MAX_TAPES];
        assert_eq!(decode(&wire[..length], b"secret", &nonce, &mut out), Ok(2));
        assert_eq!(out[..2], entries);
        assert_eq!(decode(&wire[..length], b"other", &nonce, &mut out), Err(Error::BadSignature));
        assert_eq!(decode(&wire[..length], b"secret", &[8u8; NONCE_LEN], &mut out), Err(Error::BadSignature));
        let mut tampered = wire;
        tampered[HEADER_LEN + 40] ^= 1;
        assert_eq!(decode(&tampered[..length], b"secret", &nonce, &mut out), Err(Error::BadSignature));
        assert_eq!(decode(&wire[..length - 1], b"secret", &nonce, &mut out), Err(Error::Malformed));
        assert_eq!(decode(&wire[..10], b"secret", &nonce, &mut out), Err(Error::Malformed));
    }

    #[test]
    fn manifest_holds_the_maximum() {
        let entries = [entry("t", 1, 8, 3); MAX_TAPES];
        let nonce = [0u8; NONCE_LEN];
        let mut wire = [0u8; MANIFEST_MAX];
        let length = encode(&entries, b"k", &nonce, &mut wire);
        assert_eq!(length, MANIFEST_MAX);
        let mut out = [Entry::EMPTY; MAX_TAPES];
        assert_eq!(decode(&wire[..length], b"k", &nonce, &mut out), Ok(MAX_TAPES));
    }

    #[test]
    fn decisions() {
        let theirs = entry("play", 200, 8, 1);
        assert_eq!(decide(&theirs, None, false, 8, ARCH_X86_64), Action::New);
        assert_eq!(decide(&theirs, None, true, 8, ARCH_X86_64), Action::NotATape);
        assert_eq!(decide(&theirs, Some(&entry("play", 100, 8, 2)), true, 8, ARCH_X86_64), Action::Newer);
        assert_eq!(decide(&theirs, Some(&entry("play", 300, 8, 2)), true, 8, ARCH_X86_64), Action::Keep);
        assert_eq!(decide(&theirs, Some(&entry("play", 200, 8, 2)), true, 8, ARCH_X86_64), Action::Differs);
        assert_eq!(decide(&theirs, Some(&entry("play", 100, 8, 1)), true, 8, ARCH_X86_64), Action::Same);
        assert_eq!(decide(&entry("play", 200, 9, 1), None, false, 8, ARCH_X86_64), Action::TooNew);
        assert_eq!(decide(&entry("Bad Name", 200, 8, 1), None, false, 8, ARCH_X86_64), Action::BadName);
        assert_eq!(decide(&Entry::EMPTY, None, false, 8, ARCH_X86_64), Action::BadName);
        // An ARM tape is never offered to an x86 machine, nor the reverse, and the CPU survives the manifest.
        let mut arm = entry("play", 200, 8, 1);
        arm.stamp.arch = ARCH_AARCH64;
        assert_eq!(decide(&arm, None, false, 8, ARCH_X86_64), Action::OtherCpu);
        assert_eq!(decide(&arm, None, false, 8, ARCH_AARCH64), Action::New);
        let mut out = [0u8; MANIFEST_MAX];
        let length = encode(&[arm], b"key", &[0; NONCE_LEN], &mut out);
        let mut back = [Entry::EMPTY; MAX_TAPES];
        assert_eq!(decode(&out[..length], b"key", &[0; NONCE_LEN], &mut back), Ok(1));
        assert_eq!(back[0].stamp, arm.stamp);
    }
}
