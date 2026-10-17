//! PNG writer for screenshots: 8-bit RGB, no allocation, streamed row by row through a callback.
//!
//! Each row uses PNG filter 4-less "Up" (type 2), so flat areas (most of a chart) become runs of zero bytes; the zlib stream is
//! one fixed-Huffman deflate block whose only match is "repeat the previous byte" (distance 1), which collapses those runs. A
//! 1280x1024 chart comes out at tens of kilobytes instead of 4 MB. The deflate bits are cut into IDAT chunks of at most
//! `CHUNK` bytes as they are produced (a deflate stream may be split anywhere at a byte boundary).

const CHUNK: usize = 4096;

pub fn crc32_update(mut crc: u32, data: &[u8]) -> u32 {
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 };
        }
    }
    crc
}

pub fn crc32(data: &[u8]) -> u32 {
    !crc32_update(!0, data)
}

pub fn adler32_update(adler: u32, data: &[u8]) -> u32 {
    let (mut a, mut b) = (adler & 0xffff, adler >> 16);
    for &byte in data {
        a = (a + byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

fn chunk<F: FnMut(&[u8])>(sink: &mut F, kind: &[u8; 4], data: &[u8]) {
    sink(&(data.len() as u32).to_be_bytes());
    sink(kind);
    sink(data);
    let crc = !crc32_update(crc32_update(!0, kind), data);
    sink(&crc.to_be_bytes());
}

struct Deflate<'a, F: FnMut(&[u8])> {
    sink: &'a mut F,
    buffer: [u8; CHUNK],
    length: usize,
    bits: u32,
    count: u32,
}

impl<'a, F: FnMut(&[u8])> Deflate<'a, F> {
    fn byte(&mut self, byte: u8) {
        if self.length == CHUNK {
            self.flush();
        }
        self.buffer[self.length] = byte;
        self.length += 1;
    }

    fn flush(&mut self) {
        if self.length > 0 {
            chunk(self.sink, b"IDAT", &self.buffer[..self.length]);
            self.length = 0;
        }
    }

    /// `count` bits of `value`, least significant first (how deflate packs everything but Huffman codes).
    fn put(&mut self, value: u32, count: u32) {
        self.bits |= value << self.count;
        self.count += count;
        while self.count >= 8 {
            let byte = self.bits as u8;
            self.bits >>= 8;
            self.count -= 8;
            self.byte(byte);
        }
    }

    /// A Huffman code is packed most significant bit first.
    fn code(&mut self, code: u32, length: u32) {
        let mut reversed = 0;
        for bit in 0..length {
            reversed |= ((code >> bit) & 1) << (length - 1 - bit);
        }
        self.put(reversed, length);
    }

    fn symbol(&mut self, symbol: u32) {
        match symbol {
            0..=143 => self.code(0x30 + symbol, 8),
            144..=255 => self.code(0x190 + symbol - 144, 9),
            256..=279 => self.code(symbol - 256, 7),
            _ => self.code(0xc0 + symbol - 280, 8),
        }
    }

    fn literal(&mut self, byte: u8) {
        self.symbol(byte as u32);
    }

    /// A match of `length` (3..=258) bytes at distance 1.
    fn run(&mut self, length: u32) {
        const BASE: [u32; 29] = [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
        const EXTRA: [u32; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
        let mut index = 28;
        while BASE[index] > length {
            index -= 1;
        }
        self.symbol(257 + index as u32);
        self.put(length - BASE[index], EXTRA[index]);
        self.code(0, 5); // distance code 0 = distance 1
    }
}

/// Writes an RGB PNG of `width` x `height` pixels (`pixels` is 0x00RRGGBB, `stride` pixels per row) through `sink`, which
/// receives the file in order, in pieces. Returns false when the arguments are inconsistent (nothing is written).
pub fn write_rgb<F: FnMut(&[u8])>(pixels: &[u32], width: u32, height: u32, stride: u32, mut sink: F) -> bool {
    if width == 0 || height == 0 || stride < width || pixels.len() < (stride as usize) * (height as usize - 1) + width as usize {
        return false;
    }
    sink(b"\x89PNG\r\n\x1a\n");
    let mut header = [0u8; 13];
    header[0..4].copy_from_slice(&width.to_be_bytes());
    header[4..8].copy_from_slice(&height.to_be_bytes());
    header[8] = 8; // bit depth
    header[9] = 2; // truecolour
    chunk(&mut sink, b"IHDR", &header);

    let mut deflate = Deflate { sink: &mut sink, buffer: [0; CHUNK], length: 0, bits: 0, count: 0 };
    deflate.byte(0x78); // zlib header: deflate, 32 KiB window
    deflate.byte(0x01);
    deflate.put(1, 1); // BFINAL
    deflate.put(1, 2); // BTYPE = fixed Huffman
    let mut adler = 1u32;
    let mut line = [0u8; 1];
    for y in 0..height as usize {
        // The filtered row: type byte 2, then each byte minus the byte above (zero above the first row).
        line[0] = 2;
        adler = adler32_update(adler, &line);
        deflate.literal(2);
        let mut run = 0u32;
        let mut previous = 2u8; // distance 1 from the first data byte is the filter byte, which is not a data byte
        let mut first = true;
        for x in 0..width as usize {
            let now = pixels[y * stride as usize + x];
            let above = if y > 0 { pixels[(y - 1) * stride as usize + x] } else { 0 };
            for shift in [16u32, 8, 0] {
                let value = ((now >> shift) as u8).wrapping_sub((above >> shift) as u8);
                adler = adler32_update(adler, &[value]);
                if !first && value == previous {
                    run += 1;
                    if run == 258 {
                        deflate.run(run);
                        run = 0;
                        first = true; // the next byte starts afresh, as a literal
                    }
                    continue;
                }
                if run >= 3 {
                    deflate.run(run);
                } else {
                    for _ in 0..run {
                        deflate.literal(previous);
                    }
                }
                run = 0;
                deflate.literal(value);
                previous = value;
                first = false;
            }
        }
        if run >= 3 {
            deflate.run(run);
        } else {
            for _ in 0..run {
                deflate.literal(previous);
            }
        }
    }
    deflate.symbol(256);
    if deflate.count > 0 {
        let rest = deflate.bits as u8;
        deflate.bits = 0;
        deflate.count = 0;
        deflate.byte(rest);
    }
    for byte in adler.to_be_bytes() {
        deflate.byte(byte);
    }
    deflate.flush();
    chunk(&mut sink, b"IEND", &[]);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksums_match_the_published_vectors() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(adler32_update(1, b"Wikipedia"), 0x11e6_0398);
    }

    /// With FOS_PNG_OUT=path the picture is also written out, for decoding with an independent decoder.
    #[test]
    fn busy_picture_for_an_external_decoder() {
        let (width, height) = (301u32, 120u32);
        let mut pixels = Vec::new();
        for y in 0..height {
            for x in 0..width {
                let noisy = (x * 7919 + y * 104729) % 13 == 0;
                pixels.push(if noisy { x * 0x10101 + y } else if y < 60 { 0x204060 } else { (y * 2) << 8 });
            }
        }
        let mut file = Vec::new();
        assert!(write_rgb(&pixels, width, height, width, |piece| file.extend_from_slice(piece)));
        if let Ok(path) = std::env::var("FOS_PNG_OUT") {
            std::fs::write(path, &file).unwrap();
        }
    }

    #[test]
    fn rejects_a_short_pixel_slice() {
        assert!(!write_rgb(&[0u32; 5], 4, 2, 4, |_| {}));
    }

    #[test]
    fn flat_picture_is_small_and_well_formed() {
        let pixels = [0x112233u32; 640 * 100];
        let mut file = Vec::new();
        assert!(write_rgb(&pixels, 640, 100, 640, |piece| file.extend_from_slice(piece)));
        assert_eq!(&file[..8], b"\x89PNG\r\n\x1a\n");
        assert!(file.len() < 8000, "{} bytes", file.len());
        assert_eq!(&file[file.len() - 12..file.len() - 8], &[0, 0, 0, 0]);
        assert_eq!(&file[file.len() - 8..file.len() - 4], b"IEND");
    }
}
