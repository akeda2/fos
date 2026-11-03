//! The folder name hash of ext3/ext4 htree folders (Linux `fs/ext4/hash.c`): legacy, half-MD4 and TEA, each with signed
//! (signed `char` handling, what x86 volumes use). The hash decides which leaf block of an indexed folder to read, so it
//! must be exact: these are checked against `debugfs dx_hash`. The unsigned variants (versions 3-5, volumes made on ARM
//! and the like) cannot be checked that way, so they are not implemented and those folders are scanned linearly.

const DEFAULT_SEED: [u32; 4] = [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];

/// The major hash of `name` for the root's `version` (0..=2) and the superblock's seed; `None` for a version we do not know.
pub fn hash(version: u8, seed: [u32; 4], name: &[u8]) -> Option<u32> {
    if version > 2 {
        return None;
    }
    let mut buf = if seed == [0; 4] { DEFAULT_SEED } else { seed };
    let major = match version {
        0 => legacy(name),
        1 => {
            let mut rest = name;
            loop {
                let mut input = [0u32; 8];
                words(rest, &mut input);
                half_md4(&mut buf, &input);
                if rest.len() <= 32 {
                    break;
                }
                rest = &rest[32..];
            }
            buf[1]
        }
        2 => {
            let mut rest = name;
            loop {
                let mut input = [0u32; 4];
                words(rest, &mut input);
                tea(&mut buf, &input);
                if rest.len() <= 16 {
                    break;
                }
                rest = &rest[16..];
            }
            buf[0]
        }
        _ => return None,
    };
    let major = major & !1;
    Some(if major == 0x7fff_ffff << 1 { 0x7fff_fffe << 1 } else { major })
}

fn legacy(name: &[u8]) -> u32 {
    let (mut hash0, mut hash1) = (0x12a3_fe2du32, 0x37ab_e8f9u32);
    for &byte in name {
        let c = byte as i8 as u32;
        let mut hash = hash1.wrapping_add(hash0 ^ c.wrapping_mul(7_152_373));
        if hash & 0x8000_0000 != 0 {
            hash = hash.wrapping_sub(0x7fff_ffff);
        }
        hash1 = hash0;
        hash0 = hash;
    }
    hash0 << 1
}

/// Packs up to `out.len() * 4` bytes of `name` into words, padding with the length (`str2hashbuf`).
fn words(name: &[u8], out: &mut [u32]) {
    let len = name.len() as u32;
    let pad = len | len << 8 | len << 16 | len << 24;
    let take = name.len().min(out.len() * 4);
    let (mut value, mut index) = (pad, 0);
    for (i, &byte) in name[..take].iter().enumerate() {
        if i % 4 == 0 {
            value = pad;
        }
        let c = byte as i8 as u32;
        value = c.wrapping_add(value << 8);
        if i % 4 == 3 {
            out[index] = value;
            index += 1;
            value = pad;
        }
    }
    if index < out.len() {
        out[index] = value;
        index += 1;
    }
    while index < out.len() {
        out[index] = pad;
        index += 1;
    }
}

fn tea(buf: &mut [u32; 4], input: &[u32; 4]) {
    let (mut sum, mut b0, mut b1) = (0u32, buf[0], buf[1]);
    for _ in 0..16 {
        sum = sum.wrapping_add(0x9e37_79b9);
        b0 = b0.wrapping_add((b1 << 4).wrapping_add(input[0]) ^ b1.wrapping_add(sum) ^ (b1 >> 5).wrapping_add(input[1]));
        b1 = b1.wrapping_add((b0 << 4).wrapping_add(input[2]) ^ b0.wrapping_add(sum) ^ (b0 >> 5).wrapping_add(input[3]));
    }
    buf[0] = buf[0].wrapping_add(b0);
    buf[1] = buf[1].wrapping_add(b1);
}

fn half_md4(buf: &mut [u32; 4], input: &[u32; 8]) {
    let f = |x: u32, y: u32, z: u32| z ^ (x & (y ^ z));
    let g = |x: u32, y: u32, z: u32| (x & y).wrapping_add((x ^ y) & z);
    let h = |x: u32, y: u32, z: u32| x ^ y ^ z;
    let [mut a, mut b, mut c, mut d] = *buf;
    macro_rules! round {
        ($f:ident, $a:ident, $b:ident, $c:ident, $d:ident, $x:expr, $s:expr) => {
            $a = $a.wrapping_add($f($b, $c, $d)).wrapping_add($x).rotate_left($s)
        };
    }
    const K2: u32 = 0x5a82_7999;
    const K3: u32 = 0x6ed9_eba1;
    round!(f, a, b, c, d, input[0], 3);
    round!(f, d, a, b, c, input[1], 7);
    round!(f, c, d, a, b, input[2], 11);
    round!(f, b, c, d, a, input[3], 19);
    round!(f, a, b, c, d, input[4], 3);
    round!(f, d, a, b, c, input[5], 7);
    round!(f, c, d, a, b, input[6], 11);
    round!(f, b, c, d, a, input[7], 19);
    round!(g, a, b, c, d, input[1].wrapping_add(K2), 3);
    round!(g, d, a, b, c, input[3].wrapping_add(K2), 5);
    round!(g, c, d, a, b, input[5].wrapping_add(K2), 9);
    round!(g, b, c, d, a, input[7].wrapping_add(K2), 13);
    round!(g, a, b, c, d, input[0].wrapping_add(K2), 3);
    round!(g, d, a, b, c, input[2].wrapping_add(K2), 5);
    round!(g, c, d, a, b, input[4].wrapping_add(K2), 9);
    round!(g, b, c, d, a, input[6].wrapping_add(K2), 13);
    round!(h, a, b, c, d, input[3].wrapping_add(K3), 3);
    round!(h, d, a, b, c, input[7].wrapping_add(K3), 9);
    round!(h, c, d, a, b, input[2].wrapping_add(K3), 11);
    round!(h, b, c, d, a, input[6].wrapping_add(K3), 15);
    round!(h, a, b, c, d, input[1].wrapping_add(K3), 3);
    round!(h, d, a, b, c, input[5].wrapping_add(K3), 9);
    round!(h, c, d, a, b, input[0].wrapping_add(K3), 11);
    round!(h, b, c, d, a, input[4].wrapping_add(K3), 15);
    buf[0] = buf[0].wrapping_add(a);
    buf[1] = buf[1].wrapping_add(b);
    buf[2] = buf[2].wrapping_add(c);
    buf[3] = buf[3].wrapping_add(d);
}

#[cfg(test)]
mod tests {
    use super::hash;
    use std::process::Command;

    /// `debugfs dx_hash` is e2fsprogs' own implementation of the same function.
    #[test]
    fn matches_e2fsprogs() {
        let probe = Command::new("/usr/sbin/debugfs").args(["-R", "dx_hash hello", "/dev/null"]).output();
        let Ok(probe) = probe else { return };
        if !String::from_utf8_lossy(&probe.stdout).contains("Hash of hello") {
            return;
        }
        let uuid = "12345678-1234-5678-9abc-def012345678";
        let bytes = [0x12, 0x34, 0x56, 0x78, 0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0, 0x12, 0x34, 0x56, 0x78];
        let seed = std::array::from_fn(|i| u32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap()));
        let long = "a-name-longer-than-thirty-two-bytes-so-it-takes-two-rounds-of-half-md4";
        for (version, algorithm) in [(0, "legacy"), (1, "half_md4"), (2, "tea")] {
            for name in ["a", "hello", "file-00042-with-a-longish-name.txt", long, "räksmörgås.txt", "\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}"] {
                for with_seed in [false, true] {
                    let mut command = Command::new("/usr/sbin/debugfs");
                    let request = if with_seed { format!("dx_hash -h {algorithm} -s {uuid} {name}") } else { format!("dx_hash -h {algorithm} {name}") };
                    let out = command.args(["-R", &request, "/dev/null"]).output().unwrap();
                    let text = String::from_utf8_lossy(&out.stdout);
                    let want = text.split("is 0x").nth(1).and_then(|rest| u32::from_str_radix(&rest[..8], 16).ok()).expect(&text);
                    let ours = hash(version, if with_seed { seed } else { [0; 4] }, name.as_bytes()).unwrap();
                    assert_eq!(ours, want & !1, "{algorithm} {name} seed={with_seed}");
                }
            }
        }
    }
}
