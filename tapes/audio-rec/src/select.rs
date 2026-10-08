//! Choosing channels: a multichannel interface has many inputs and a take usually wants a few of them (a UFX's microphones
//! are 9-12 of 22).

use core::fmt::Write;

/// Copies the channels in `mask` (bit n = channel n+1) out of device-format frames (`channels` channels in `slot`-byte
/// slots) into `out`, keeping the device's sample format, in ascending channel order. Returns the bytes written.
pub fn extract(data: &[u8], channels: usize, slot: usize, mask: u32, out: &mut [u8]) -> usize {
    let mut written = 0;
    for frame in data.chunks_exact(channels * slot) {
        for channel in 0..channels.min(32) {
            if mask & (1 << channel) != 0 {
                if written + slot > out.len() {
                    return written;
                }
                out[written..written + slot].copy_from_slice(&frame[channel * slot..(channel + 1) * slot]);
                written += slot;
            }
        }
    }
    written
}

/// The channels of `mask` as the text `parse` takes: `9-12,15`. All of `channels` is `all`.
pub fn describe(mask: u32, channels: usize, out: &mut impl Write) {
    let full = if channels >= 32 { !0 } else { (1u32 << channels) - 1 };
    if mask & full == full {
        let _ = write!(out, "all");
        return;
    }
    let mut channel = 0;
    let mut first = true;
    while channel < 32 {
        if mask & (1 << channel) == 0 {
            channel += 1;
            continue;
        }
        let start = channel;
        while channel + 1 < 32 && mask & (1 << (channel + 1)) != 0 {
            channel += 1;
        }
        if !first {
            let _ = write!(out, ",");
        }
        first = false;
        if channel == start {
            let _ = write!(out, "{}", start + 1);
        } else {
            let _ = write!(out, "{}-{}", start + 1, channel + 1);
        }
        channel += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_and_describes() {
        // Three frames of four 2-byte channels, each sample = frame*16 + channel.
        let mut data = Vec::new();
        for frame in 0..3u16 {
            for channel in 0..4u16 {
                data.extend_from_slice(&(frame * 16 + channel).to_le_bytes());
            }
        }
        let mut out = [0u8; 64];
        let n = extract(&data, 4, 2, 0b0110, &mut out);
        assert_eq!(n, 3 * 2 * 2);
        let samples: Vec<u16> = out[..n].chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        assert_eq!(samples, [1, 2, 17, 18, 33, 34]);
        let mut text = String::new();
        describe(0b1111_0000_0000, 22, &mut text);
        assert_eq!(text, "9-12");
        let mut text = String::new();
        describe(0b1000_0111_0101, 22, &mut text);
        assert_eq!(text, "1,3,5-7,12");
        let mut text = String::new();
        describe((1 << 22) - 1, 22, &mut text);
        assert_eq!(text, "all");
    }
}
