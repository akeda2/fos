//! GF(2^8) arithmetic for RAID 6, with the field Linux's `lib/raid6` uses: polynomial x^8 + x^4 + x^3 + x^2 + 1 (0x11d)
//! and generator g = 2. The Q syndrome of a stripe is the sum over its data disks of g^j times the data, j being the
//! disk's place in the syndrome order.

const fn tables() -> ([u8; 512], [u8; 256]) {
    let (mut exp, mut log) = ([0u8; 512], [0u8; 256]);
    let (mut x, mut i) = (1u16, 0usize);
    while i < 255 {
        exp[i] = x as u8;
        log[x as usize] = i as u8;
        x <<= 1;
        if x & 0x100 != 0 {
            x ^= 0x11d;
        }
        i += 1;
    }
    while i < 512 {
        exp[i] = exp[i - 255];
        i += 1;
    }
    (exp, log)
}

static TABLES: ([u8; 512], [u8; 256]) = tables();

pub fn mul(a: u8, b: u8) -> u8 {
    if a == 0 || b == 0 { 0 } else { TABLES.0[TABLES.1[a as usize] as usize + TABLES.1[b as usize] as usize] }
}

/// g^n.
pub fn pow(n: usize) -> u8 {
    TABLES.0[n % 255]
}

/// 1/a (a is not 0).
pub fn inv(a: u8) -> u8 {
    TABLES.0[255 - TABLES.1[a as usize] as usize]
}

/// `dst ^= c * src`, byte by byte.
pub fn mul_add(dst: &mut [u8], src: &[u8], c: u8) {
    for (d, s) in dst.iter_mut().zip(src) {
        *d ^= mul(*s, c);
    }
}

pub fn mul_in_place(buf: &mut [u8], c: u8) {
    for b in buf {
        *b = mul(*b, c);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_field_matches_linuxs_tables() {
        // first powers of 2 and the wrap at 0x11d, as in lib/raid6/mktables.c
        assert_eq!([pow(0), pow(1), pow(7), pow(8), pow(9), pow(254), pow(255)], [1, 2, 128, 0x1d, 0x3a, 0x8e, 1]);
        for a in 1..=255u8 {
            assert_eq!(mul(a, inv(a)), 1);
            assert_eq!(mul(a, 1), a);
            for b in [2u8, 3, 0x57, 0xff] {
                assert_eq!(mul(a, b), mul(b, a));
            }
        }
        // against shift-and-add multiplication, for every pair
        for a in 0..=255u16 {
            for b in 0..=255u16 {
                let (mut x, mut y, mut product) = (a, b, 0u16);
                while y != 0 {
                    if y & 1 != 0 {
                        product ^= x;
                    }
                    x <<= 1;
                    if x & 0x100 != 0 {
                        x ^= 0x11d;
                    }
                    y >>= 1;
                }
                assert_eq!(mul(a as u8, b as u8) as u16, product);
            }
        }
    }
}
