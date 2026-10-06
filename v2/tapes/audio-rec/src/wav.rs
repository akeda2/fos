//! WAV file headers for a take whose length is not known in advance.
//!
//! The header sits at the start of the file with a fixed length for a given format (`header_len`), so it can be
//! rewritten in place as the take grows (the recorder keeps its first sector and rewrites it at checkpoints).
//! Layout: `RIFF`/`WAVE`, a `JUNK` chunk that is turned into `ds64` when the data passes 4 GiB (the file's id is then
//! `RF64`, EBU TECH 3306), `fmt `, `data`.
//!
//! `fmt ` is plain PCM (16 bytes) when the file is one or two channels of whole 16-bit samples, which every reader
//! knows; otherwise WAVE_FORMAT_EXTENSIBLE (40 bytes), which is what multichannel and 24-bit files need to be read
//! correctly. Samples are stored as the device delivers them: little-endian, in `slot` bytes, left-justified, `bits`
//! of them valid (a 24-bit sample in a 4-byte slot is a valid case).

const JUNK_LEN: usize = 28; // the size of a ds64 chunk body with no table entries
const PCM_FMT: usize = 16;
const EXT_FMT: usize = 40;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Format {
    pub channels: u16,
    pub rate: u32,
    /// Bytes per sample in the file (the device's slot size).
    pub slot: u16,
    /// Significant bits per sample.
    pub bits: u16,
}

impl Format {
    pub fn frame_bytes(&self) -> u64 {
        u64::from(self.channels) * u64::from(self.slot)
    }

    fn plain(&self) -> bool {
        self.channels <= 2 && self.bits == self.slot * 8 && self.slot == 2
    }
}

/// Bytes of the header for `format`: the same whatever the data length.
pub fn header_len(format: &Format) -> usize {
    12 + 8 + JUNK_LEN + 8 + if format.plain() { PCM_FMT } else { EXT_FMT } + 8
}

/// The speaker-position mask for `channels` channels: the first N of the standard positions (stereo = front left and
/// right), which is the best a recorder can say; 0 ("unspecified") beyond 18.
fn channel_mask(channels: u16) -> u32 {
    if channels == 0 || channels > 18 { 0 } else { (1u32 << channels) - 1 }
}

/// Writes the header for `data_bytes` of audio into `out` (at least `header_len` bytes); returns its length.
pub fn write_header(out: &mut [u8], format: &Format, data_bytes: u64) -> usize {
    let length = header_len(format);
    let big = data_bytes > 0xffff_ff00 - length as u64;
    let mut at = 0;
    let mut put = |bytes: &[u8]| {
        out[at..at + bytes.len()].copy_from_slice(bytes);
        at += bytes.len();
    };
    let riff_size = length as u64 - 8 + data_bytes;
    put(if big { b"RF64" } else { b"RIFF" });
    put(&(if big { 0xffff_ffff } else { riff_size as u32 }).to_le_bytes());
    put(b"WAVE");
    if big {
        put(b"ds64");
        put(&(JUNK_LEN as u32).to_le_bytes());
        put(&riff_size.to_le_bytes());
        put(&data_bytes.to_le_bytes());
        put(&(data_bytes / format.frame_bytes().max(1)).to_le_bytes()); // sample (frame) count
        put(&0u32.to_le_bytes()); // no table entries
    } else {
        put(b"JUNK");
        put(&(JUNK_LEN as u32).to_le_bytes());
        put(&[0u8; JUNK_LEN]);
    }
    put(b"fmt ");
    let plain = format.plain();
    put(&(if plain { PCM_FMT } else { EXT_FMT } as u32).to_le_bytes());
    put(&(if plain { 1u16 } else { 0xfffe }).to_le_bytes());
    put(&format.channels.to_le_bytes());
    put(&format.rate.to_le_bytes());
    put(&((u64::from(format.rate) * format.frame_bytes()) as u32).to_le_bytes());
    put(&(format.frame_bytes() as u16).to_le_bytes());
    put(&(format.slot * 8).to_le_bytes());
    if !plain {
        put(&22u16.to_le_bytes()); // cbSize
        put(&format.bits.to_le_bytes()); // valid bits per sample
        put(&channel_mask(format.channels).to_le_bytes());
        // KSDATAFORMAT_SUBTYPE_PCM
        put(&[0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71]);
    }
    put(b"data");
    put(&(if big { 0xffff_ffff } else { data_bytes as u32 }).to_le_bytes());
    debug_assert_eq!(at, length);
    length
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u32_at(bytes: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
    }

    #[test]
    fn plain_stereo_16() {
        let format = Format { channels: 2, rate: 48_000, slot: 2, bits: 16 };
        let mut out = [0u8; 128];
        let length = write_header(&mut out, &format, 1000);
        assert_eq!(length, 80);
        assert_eq!(&out[0..4], b"RIFF");
        assert_eq!(u32_at(&out, 4), 72 + 1000);
        assert_eq!(&out[12..16], b"JUNK");
        assert_eq!(&out[48..52], b"fmt ");
        assert_eq!(u32_at(&out, 52), 16);
        assert_eq!(u16::from_le_bytes([out[56], out[57]]), 1);
        assert_eq!(&out[72..76], b"data");
        assert_eq!(u32_at(&out, 76), 1000);
    }

    #[test]
    fn multichannel_24_is_extensible() {
        let format = Format { channels: 6, rate: 96_000, slot: 3, bits: 24 };
        let mut out = [0u8; 128];
        let length = write_header(&mut out, &format, 0);
        assert_eq!(length, 104);
        assert_eq!(u16::from_le_bytes([out[56], out[57]]), 0xfffe);
        assert_eq!(u16::from_le_bytes([out[58], out[59]]), 6);
        assert_eq!(u32_at(&out, 64), 96_000 * 18); // byte rate
        assert_eq!(u16::from_le_bytes([out[68], out[69]]), 18); // block align
        assert_eq!(u16::from_le_bytes([out[70], out[71]]), 24);
        assert_eq!(u32_at(&out, 76), 0b11_1111); // channel mask
        assert_eq!(&out[96..100], b"data");
    }

    #[test]
    fn over_4_gib_is_rf64() {
        let format = Format { channels: 2, rate: 48_000, slot: 4, bits: 24 };
        let mut out = [0u8; 128];
        let data = 5u64 << 30;
        let length = write_header(&mut out, &format, data);
        assert_eq!(&out[0..4], b"RF64");
        assert_eq!(u32_at(&out, 4), 0xffff_ffff);
        assert_eq!(&out[12..16], b"ds64");
        assert_eq!(u64::from_le_bytes(out[28..36].try_into().unwrap()), data);
        assert_eq!(u64::from_le_bytes(out[36..44].try_into().unwrap()), data / 8);
        assert_eq!(&out[length - 8..length - 4], b"data");
        assert_eq!(u32_at(&out, length - 4), 0xffff_ffff);
    }
}
