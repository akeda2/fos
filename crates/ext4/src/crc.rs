//! CRC32C (Castagnoli), as ext4's metadata_csum and jbd2 v2/v3 use it: reflected polynomial 0x82F63B78, no initial or
//! final inversion beyond what the caller passes as the seed (ext4 seeds with `!0` and keeps the raw register).

const fn table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut index = 0;
    while index < 256 {
        let mut crc = index as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0x82f6_3b78 } else { crc >> 1 };
            bit += 1;
        }
        table[index] = crc;
        index += 1;
    }
    table
}

static TABLE: [u32; 256] = table();

pub fn crc32c(seed: u32, bytes: &[u8]) -> u32 {
    let mut crc = seed;
    for &byte in bytes {
        crc = TABLE[((crc ^ u32::from(byte)) & 0xff) as usize] ^ (crc >> 8);
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        // The standard check value: CRC-32C("123456789") = 0xE3069283 with the usual init and final inversion.
        assert_eq!(!crc32c(!0, b"123456789"), 0xe306_9283);
        assert_eq!(!crc32c(!0, &[0u8; 32]), 0x8a91_36aa);
    }
}
