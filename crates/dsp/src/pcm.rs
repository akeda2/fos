//! Device sample frames to f32. USB audio frames are interleaved channels, each in a `slot`-byte little-endian signed word
//! holding `bits` significant bits, left-justified in the slot (24 bits in 3 bytes, 24 in 4, 16 in 2 ...), or, for a float
//! device, a 4-byte IEEE single that is passed on as it is (`audio_sample::Format`).

/// Takes `channel` (0-based) out of `data`'s whole frames into `out` as -1..1; returns the samples written. 0 for a format
/// that cannot be read (channel out of range, slot over 4 bytes, `bits` over the slot).
pub fn unpack_channel(data: &[u8], out: &mut [f32], channels: usize, slot: usize, bits: usize, float: bool, channel: usize) -> usize {
    let format = audio_sample::Format::new(slot, bits, float);
    if channels == 0 || channel >= channels || !format.valid() {
        return 0;
    }
    let frame = channels * slot;
    let mut count = 0;
    for (chunk, sample) in data.chunks_exact(frame).zip(out.iter_mut()) {
        *sample = format.read(&chunk[channel * slot..channel * slot + slot]);
        count += 1;
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_16_and_24_bit_and_picks_the_channel() {
        let data = [0x00, 0x40, 0x00, 0xc0, 0xff, 0x7f, 0x00, 0x80]; // frames (0.5,-0.5) (~1,-1), 2 channels, 16 bit
        let mut out = [0.0; 4];
        assert_eq!(unpack_channel(&data, &mut out, 2, 2, 16, false, 0), 2);
        assert_eq!((out[0], out[1]), (0.5, 32767.0 / 32768.0));
        assert_eq!(unpack_channel(&data, &mut out, 2, 2, 16, false, 1), 2);
        assert_eq!((out[0], out[1]), (-0.5, -1.0));
        let data24 = [0x00, 0x00, 0x40, 0x00, 0x00, 0xc0];
        assert_eq!(unpack_channel(&data24, &mut out, 1, 3, 24, false, 0), 2);
        assert_eq!((out[0], out[1]), (0.5, -0.5));
        assert_eq!(unpack_channel(&data24, &mut out, 1, 3, 24, false, 1), 0);
        assert_eq!(unpack_channel(&data24, &mut out, 1, 3, 32, false, 0), 0);
        assert_eq!(unpack_channel(&data24[..5], &mut out, 1, 3, 24, false, 0), 1, "a partial frame is left alone");
    }
}
