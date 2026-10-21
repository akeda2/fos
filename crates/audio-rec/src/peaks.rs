//! Feeding the meters from audio in the device's own format: interleaved frames of little-endian, left-justified
//! samples in `slot`-byte slots (2, 3 or 4 bytes), or 4-byte floats. The detectors work on the top 16 bits, which is all a
//! meter can show; a float above full scale reads as full scale (and latches the clip light).

use audio_wire::{Format, device_sample};
use audio_wire::meter::{CLIP, Coefficients, Ppm};

/// Feeds every frame of `data` to `detectors` (one per channel, at most `detectors.len()` of the `channels` are
/// used), latching `clips[channel]` for a sample at or near full scale. Returns the loudest magnitude seen in the
/// block over the channels in `mask` (bit n = channel n+1; for the silence detector, which should only listen to what is
/// being recorded), in the meter's scale (2^31 = full scale).
pub fn feed(data: &[u8], channels: usize, format: Format, coefficients: &Coefficients, detectors: &mut [Ppm], clips: &mut [bool], mask: u32) -> u32 {
    let mut loudest = 0u32;
    let slot = format.slot;
    if channels == 0 || !(2..=4).contains(&slot) || (format.float && slot != 4) {
        return 0;
    }
    let used = channels.min(detectors.len()).min(clips.len());
    for frame in data.chunks_exact(channels * slot) {
        for channel in 0..used {
            let sample = device_sample(&frame[channel * slot..(channel + 1) * slot], format);
            // The magnitude of the top 16 bits, moved back up: what the detectors expect.
            let magnitude = i32::from((sample >> 16) as i16).unsigned_abs() << 16;
            detectors[channel].feed(magnitude, coefficients);
            if magnitude >= CLIP {
                clips[channel] = true;
            }
            if mask & (1 << channel) != 0 {
                loudest = loudest.max(magnitude);
            }
        }
    }
    loudest
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_24_bit_and_16_bit_slots() {
        let c = Coefficients::for_rate(48_000);
        let mut detectors = [Ppm::new(); 2];
        let mut clips = [false; 2];
        // Two 24-bit frames: ch0 = +0x400000 (a half scale of 24 bits... 0x40_0000 of 0x80_0000), ch1 = -full scale.
        let data = [0x00, 0x00, 0x40, 0x00, 0x00, 0x80, 0x00, 0x00, 0x40, 0x00, 0x00, 0x80];
        let loudest = feed(&data, 2, Format::new(3, 3 * 8, false), &c, &mut detectors, &mut clips, !0);
        assert!(loudest >= CLIP);
        assert_eq!(clips, [false, true]);
        assert!(detectors[0].level > 0 && detectors[1].level > detectors[0].level);
        // 16-bit silence gives zero.
        let loudest = feed(&[0u8; 8], 2, Format::new(2, 2 * 8, false), &c, &mut detectors, &mut clips, !0);
        assert_eq!(loudest, 0);
        // A float device: 0.5 is half scale, 1.5 clips.
        let float = Format::new(4, 32, true);
        let data = [0.5f32.to_le_bytes(), 1.5f32.to_le_bytes()].concat();
        let mut clips = [false; 2];
        assert!(feed(&data, 2, float, &c, &mut detectors, &mut clips, !0) >= CLIP);
        assert_eq!(clips, [false, true]);
        // The loudest is taken over the masked channels only.
        let data = [0x00, 0x00, 0x40, 0x00, 0x00, 0x10]; // ch0 loud, ch1 quiet (24-bit)
        assert!(feed(&data, 2, Format::new(3, 3 * 8, false), &c, &mut detectors, &mut clips, 0b01) > feed(&data, 2, Format::new(3, 3 * 8, false), &c, &mut detectors, &mut clips, 0b10));
    }
}
