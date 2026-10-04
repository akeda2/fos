//! What the running stream tells the panel: counters, the ring level, the clock offset and a peak per channel. The
//! stream loops write these (a few stores per packet), the panel reads them a couple of times a second. Everything
//! runs on one thread (tasks switch only inside `idle()`), so plain statics are enough.

pub const MAX_CHANNELS: usize = 32;

pub struct Stats {
    pub sent: u64,
    pub received: u64,
    pub lost: u64,
    pub level_frames: u32,
    pub target_frames: u32,
    pub offset_ppm: i32,
    pub rate: u32,
    /// Largest sample magnitude (0..2^31) per channel since the panel last looked, for the send and receive side.
    pub send_peaks: [u32; MAX_CHANNELS],
    pub receive_peaks: [u32; MAX_CHANNELS],
    pub send_channels: usize,
    pub receive_channels: usize,
}

static mut STATS: Stats = Stats { sent: 0, received: 0, lost: 0, level_frames: 0, target_frames: 0, offset_ppm: 0, rate: 48_000, send_peaks: [0; MAX_CHANNELS], receive_peaks: [0; MAX_CHANNELS], send_channels: 0, receive_channels: 0 };

pub fn get() -> &'static mut Stats {
    unsafe { &mut *(&raw mut STATS) }
}

pub fn reset() {
    *get() = Stats { sent: 0, received: 0, lost: 0, level_frames: 0, target_frames: 0, offset_ppm: 0, rate: 48_000, send_peaks: [0; MAX_CHANNELS], receive_peaks: [0; MAX_CHANNELS], send_channels: 0, receive_channels: 0 };
}

fn note(peaks: &mut [u32; MAX_CHANNELS], channel: usize, magnitude: u32) {
    if channel < MAX_CHANNELS && magnitude > peaks[channel] {
        peaks[channel] = magnitude;
    }
}

/// Peaks of big-endian 16/24-bit interleaved samples (the wire format), as 32-bit magnitudes.
pub fn peaks_from_wire(payload: &[u8], channels: usize, bits: usize, frames: usize, send: bool) {
    let stats = get();
    let bytes = bits / 8;
    if channels == 0 || bytes < 2 {
        return;
    }
    let peaks = if send { &mut stats.send_peaks } else { &mut stats.receive_peaks };
    if send { stats.send_channels = channels } else { stats.receive_channels = channels }
    for frame in 0..frames {
        for channel in 0..channels {
            let at = (frame * channels + channel) * bytes;
            let Some(sample) = payload.get(at..at + bytes) else { return };
            let value = i32::from(i16::from_be_bytes([sample[0], sample[1]]));
            note(peaks, channel, value.unsigned_abs() << 16);
        }
    }
}

/// Peaks of 16-bit interleaved samples (what the Opus codec works in); `first` is the first channel's number.
pub fn peaks_from_i16(pcm: &[i16], width: usize, frames: usize, first: usize, send: bool) {
    let stats = get();
    let peaks = if send { &mut stats.send_peaks } else { &mut stats.receive_peaks };
    let count = if send { &mut stats.send_channels } else { &mut stats.receive_channels };
    *count = (*count).max(first + width);
    for frame in 0..frames {
        for lane in 0..width {
            if let Some(&sample) = pcm.get(frame * width + lane) {
                note(peaks, first + lane, u32::from(sample.unsigned_abs()) << 16);
            }
        }
    }
}

/// A level in half-steps of 3 dB below full scale: 0 is full scale, 20 is 60 dB down or silence.
pub fn steps_below_full(magnitude: u32) -> u32 {
    if magnitude == 0 {
        return 20;
    }
    // 6 dB per bit of magnitude; the bit below the leading one gives the half step (below 0.75 of the range).
    let leading = magnitude.leading_zeros();
    let below_three_quarters = leading < 31 && (magnitude >> (30 - leading)) & 1 == 0;
    (leading * 2 + u32::from(below_three_quarters)).min(20)
}
