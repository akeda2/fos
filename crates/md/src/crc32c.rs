//! CRC-32C (Castagnoli), the checksum of the partial parity log: the header and every entry carry the standard
//! CRC-32C (init and final XOR with 0xffffffff, reflected polynomial 0x82f63b78) of their bytes.

const TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0x82f6_3b78 } else { crc >> 1 };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
};

/// Continues a running checksum: start from `!0`, finish with `!`. [`crc32c`] does both.
pub fn update(mut crc: u32, data: &[u8]) -> u32 {
    for &byte in data {
        crc = (crc >> 8) ^ TABLE[((crc ^ byte as u32) & 0xff) as usize];
    }
    crc
}

pub fn crc32c(data: &[u8]) -> u32 {
    !update(!0, data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_published_check_value() {
        assert_eq!(crc32c(b"123456789"), 0xe306_9283);
        assert_eq!(!update(update(!0, b"1234"), b"56789"), 0xe306_9283);
    }
}
