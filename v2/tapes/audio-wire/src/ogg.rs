//! Ogg Opus (RFC 3533 pages, RFC 7845 packet mapping) for `audio-net serve`: one Opus packet per Ogg page, so a
//! player can start at any page and the stream needs no length (the HTTP response never ends).
//!
//! An Ogg page is `"OggS"`, version 0, a header-type byte (1 = continued packet, 2 = first page, 4 = last), the
//! 64-bit *granule position* (for Opus: 48 kHz samples decoded once this page is played, counting the pre-skip), a
//! stream serial, a page sequence number, a CRC-32 of the whole page, a segment count and one lacing byte per segment
//! (a packet is cut into 255-byte segments and ends with a shorter one, which may be empty). The CRC uses the
//! polynomial 0x04c11db7, starts at 0, is not reflected and has no final XOR, and is computed with the CRC field zero.

pub const MAX_PAGE: usize = 27 + 255 + 255 * 255;
/// Samples a decoder discards at the start (libopus's encoder look-ahead at 48 kHz, 6.5 ms).
pub const PRE_SKIP: u16 = 312;

const fn table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut index = 0;
    while index < 256 {
        let mut crc = (index as u32) << 24;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 0x8000_0000 != 0 { (crc << 1) ^ 0x04c1_1db7 } else { crc << 1 };
            bit += 1;
        }
        table[index] = crc;
        index += 1;
    }
    table
}
static TABLE: [u32; 256] = table();

pub fn crc32(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0u32, |crc, &byte| (crc << 8) ^ TABLE[((crc >> 24) as u8 ^ byte) as usize])
}

/// Writes one page holding the single `packet` (at most 255 * 255 - 1 bytes) into `out`; returns its length, or
/// `None` if it does not fit. `header_type` is 2 for the first page of the stream.
pub fn write_page(out: &mut [u8], header_type: u8, granule: u64, serial: u32, sequence: u32, packet: &[u8]) -> Option<usize> {
    let segments = packet.len() / 255 + 1; // the last segment is < 255, possibly 0
    if segments > 255 || out.len() < 27 + segments + packet.len() {
        return None;
    }
    out[..4].copy_from_slice(b"OggS");
    out[4] = 0;
    out[5] = header_type;
    out[6..14].copy_from_slice(&granule.to_le_bytes());
    out[14..18].copy_from_slice(&serial.to_le_bytes());
    out[18..22].copy_from_slice(&sequence.to_le_bytes());
    out[22..26].fill(0);
    out[26] = segments as u8;
    for lace in out[27..27 + segments - 1].iter_mut() {
        *lace = 255;
    }
    out[27 + segments - 1] = (packet.len() % 255) as u8;
    let total = 27 + segments + packet.len();
    out[27 + segments..total].copy_from_slice(packet);
    let crc = crc32(&out[..total]);
    out[22..26].copy_from_slice(&crc.to_le_bytes());
    Some(total)
}

/// The `OpusHead` identification packet (RFC 7845 section 5.1): channel mapping family 0 is mono or stereo.
pub fn opus_head(channels: u8, input_rate: u32) -> [u8; 19] {
    let mut head = [0u8; 19];
    head[..8].copy_from_slice(b"OpusHead");
    head[8] = 1;
    head[9] = channels;
    head[10..12].copy_from_slice(&PRE_SKIP.to_le_bytes());
    head[12..16].copy_from_slice(&input_rate.to_le_bytes());
    // bytes 16..18: output gain 0 dB; byte 18: mapping family 0
    head
}

/// The `OpusTags` comment packet (section 5.2) with a vendor string and no comments.
pub fn opus_tags(vendor: &str, out: &mut [u8]) -> usize {
    out[..8].copy_from_slice(b"OpusTags");
    out[8..12].copy_from_slice(&(vendor.len() as u32).to_le_bytes());
    out[12..12 + vendor.len()].copy_from_slice(vendor.as_bytes());
    out[12 + vendor.len()..16 + vendor.len()].fill(0);
    16 + vendor.len()
}

/// The path of an HTTP `GET` request line in `request` (up to the first space after it), once the request is
/// complete (a blank line has arrived). `Err(())` is a complete request that is not a plain GET; `Ok(None)` means
/// "wait for more".
pub fn http_get_path(request: &[u8]) -> Result<Option<&str>, ()> {
    let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
        return if request.len() >= 1024 { Err(()) } else { Ok(None) };
    };
    let line = request[..end].split(|&byte| byte == b'\r').next().unwrap_or(&[]);
    let line = core::str::from_utf8(line).map_err(|_| ())?;
    let mut words = line.split(' ');
    match (words.next(), words.next()) {
        (Some("GET"), Some(path)) => Ok(Some(path)),
        _ => Err(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_check_value() {
        assert_eq!(crc32(b"123456789"), 0x89a1_897f);
    }

    #[test]
    fn page_layout_and_crc() {
        let mut page = [0u8; 2048];
        let packet = [7u8; 600]; // 255 + 255 + 90
        let length = write_page(&mut page, 0, 48_000 + 312, 0x1234_5678, 3, &packet).unwrap();
        assert_eq!(length, 27 + 3 + 600);
        assert_eq!(&page[..4], b"OggS");
        assert_eq!(&page[26..30], &[3, 255, 255, 90]);
        assert_eq!(u64::from_le_bytes(page[6..14].try_into().unwrap()), 48_312);
        let stored = u32::from_le_bytes(page[22..26].try_into().unwrap());
        page[22..26].fill(0);
        assert_eq!(crc32(&page[..length]), stored);
    }

    #[test]
    fn exact_multiple_of_255_ends_with_an_empty_segment() {
        let mut page = [0u8; 600];
        let length = write_page(&mut page, 0, 0, 1, 0, &[1u8; 255]).unwrap();
        assert_eq!(&page[26..29], &[2, 255, 0]);
        assert_eq!(length, 27 + 2 + 255);
        assert!(write_page(&mut page[..100], 0, 0, 1, 0, &[1u8; 255]).is_none());
    }

    #[test]
    fn head_and_tags() {
        let head = opus_head(2, 48_000);
        assert_eq!(&head[..9], b"OpusHead\x01");
        assert_eq!(head[9], 2);
        assert_eq!(u16::from_le_bytes([head[10], head[11]]), 312);
        let mut tags = [0u8; 64];
        let length = opus_tags("fos", &mut tags);
        assert_eq!(&tags[..length], b"OpusTags\x03\0\0\0fos\0\0\0\0");
    }

    #[test]
    fn http_requests() {
        assert_eq!(http_get_path(b"GET /live.opus HTTP/1.1\r\nHost: x\r\n"), Ok(None));
        assert_eq!(http_get_path(b"GET /live.opus HTTP/1.1\r\nHost: x\r\n\r\n"), Ok(Some("/live.opus")));
        assert_eq!(http_get_path(b"POST / HTTP/1.1\r\n\r\n"), Err(()));
    }
}
