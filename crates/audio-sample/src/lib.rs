//! One place that knows how a device sample is laid out, for every audio tape. A device slot is `slot` little-endian bytes
//! (1 to 4): either a signed integer with `bits` significant bits, left-justified in the slot (24 bits in 3 bytes, 24 in 4,
//! 16 in 2 ...), or, with `float` set, a 4-byte IEEE 754 single (`tape_rt::audio_float` says which a stream is). Full scale
//! is 1.0 as a float and the slot's top bit pattern as an integer. Tapes that keep samples as f32 read and write them here,
//! so a float device reaches the DSP unchanged; only the integer-only places (wire formats, Opus) convert, at their edge.
#![cfg_attr(not(test), no_std)]

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Format {
    /// Bytes per sample in a frame.
    pub slot: usize,
    /// Significant bits of an integer sample (ignored for float).
    pub bits: usize,
    pub float: bool,
}

impl Format {
    pub fn new(slot: usize, bits: usize, float: bool) -> Self {
        Format { slot, bits, float }
    }

    /// Whether samples of this layout can be read here: an integer in 1 to 4 bytes holding `bits`, or a 4-byte float.
    pub fn valid(self) -> bool {
        if self.float { self.slot == 4 } else { (1..=4).contains(&self.slot) && self.bits >= 1 && self.bits <= self.slot * 8 }
    }

    /// The sample in `bytes` (at least `slot` long) as -1..1 (a float device may exceed it; NaN reads as 0).
    #[inline]
    pub fn read(self, bytes: &[u8]) -> f32 {
        if self.float {
            let value = f32::from_bits(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]));
            return if value == value { value } else { 0.0 };
        }
        let mut raw = 0u32;
        for (index, &byte) in bytes[..self.slot].iter().enumerate() {
            raw |= u32::from(byte) << (8 * index);
        }
        let shift = 32 - self.slot * 8;
        (((raw << shift) as i32) >> shift) as f32 * (1.0 / (1u64 << (self.slot * 8 - 1)) as f32)
    }

    /// Writes `value` (-1..1) into `out[..slot]`. Integers are clipped to full scale and cut to `bits` (the lower bits of a
    /// wider slot stay zero); a float is stored as it is.
    #[inline]
    pub fn write(self, value: f32, out: &mut [u8]) {
        if self.float {
            out[..4].copy_from_slice(&value.to_bits().to_le_bytes());
            return;
        }
        let peak = ((1u64 << (self.bits - 1)) - 1) as f32;
        let clipped = if value > 1.0 { 1.0 } else if value < -1.0 { -1.0 } else { value };
        let integer = ((clipped * peak) as i32 as i64) << (self.slot * 8 - self.bits);
        out[..self.slot].copy_from_slice(&integer.to_le_bytes()[..self.slot]);
    }

    /// The sample as a 32-bit integer with the signal in the top bits (what the wire formats and the level meters take); a
    /// float is clipped at full scale.
    #[inline]
    pub fn top32(self, bytes: &[u8]) -> i32 {
        if self.float {
            let value = self.read(bytes);
            return if value >= 1.0 { i32::MAX } else if value <= -1.0 { i32::MIN } else { (value * 2147483648.0) as i32 };
        }
        let mut raw = 0u32;
        for (index, &byte) in bytes[..self.slot].iter().enumerate() {
            raw |= u32::from(byte) << (8 * (4 - self.slot + index));
        }
        raw as i32
    }

    /// The reverse of `top32`: a left-justified 32-bit integer into the slot (a float slot gets value / 2^31).
    #[inline]
    pub fn put_top32(self, value: i32, out: &mut [u8]) {
        if self.float {
            out[..4].copy_from_slice(&((value as f32) * (1.0 / 2147483648.0)).to_bits().to_le_bytes());
            return;
        }
        out[..self.slot].copy_from_slice(&value.to_le_bytes()[4 - self.slot..]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integers_round_trip_and_clip() {
        for (slot, bits) in [(2, 16), (3, 24), (4, 24), (4, 32)] {
            let format = Format::new(slot, bits, false);
            let mut bytes = [0u8; 4];
            for value in [0.0, 0.5, -0.5, 0.25] {
                format.write(value, &mut bytes);
                assert!((format.read(&bytes) - value).abs() < 1e-4, "{slot}/{bits} {value}");
            }
            format.write(3.0, &mut bytes);
            assert!(format.read(&bytes) > 0.999);
            format.write(-3.0, &mut bytes);
            assert!(format.read(&bytes) <= -0.999);
        }
    }

    #[test]
    fn float_passes_through_unchanged() {
        let format = Format::new(4, 32, true);
        let mut bytes = [0u8; 4];
        for value in [0.1234567, -2.5, 1.0e-30] {
            format.write(value, &mut bytes);
            assert_eq!(format.read(&bytes), value, "no conversion, not even a clip");
        }
        bytes = f32::NAN.to_bits().to_le_bytes();
        assert_eq!(format.read(&bytes), 0.0);
    }

    #[test]
    fn top32_matches_the_old_device_sample() {
        assert_eq!(Format::new(3, 24, false).top32(&[0x00, 0x34, 0x12]), 0x1234_0000);
        assert_eq!(Format::new(2, 16, false).top32(&[0xff, 0x7f]), 0x7fff_0000);
        assert_eq!(Format::new(3, 24, false).top32(&[0, 0, 0x80]), i32::MIN);
        let float = Format::new(4, 32, true);
        let mut bytes = [0u8; 4];
        float.write(0.5, &mut bytes);
        assert_eq!(float.top32(&bytes), 0x4000_0000);
        float.write(2.0, &mut bytes);
        assert_eq!(float.top32(&bytes), i32::MAX);
        float.put_top32(0x4000_0000, &mut bytes);
        assert_eq!(float.read(&bytes), 0.5);
        let mut int = [0u8; 3];
        Format::new(3, 24, false).put_top32(0x1234_5600, &mut int);
        assert_eq!(int, [0x56, 0x34, 0x12]);
    }
}
