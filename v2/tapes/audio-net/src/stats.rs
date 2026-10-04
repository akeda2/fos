//! What the running stream tells the panel: counters, the ring level, the clock offset and a programme peak meter
//! (Nordic-style quasi-peak detector, see `audio_wire::meter`) per channel. The stream loops write these (a few
//! operations per sample), the panel reads them a few times a second. Everything runs on one thread (tasks switch
//! only inside `idle()`), so plain statics are enough.

use audio_wire::meter::{CLIP, Coefficients, Ppm};

pub const MAX_CHANNELS: usize = 32;

pub struct Stats {
    pub sent: u64,
    pub received: u64,
    pub lost: u64,
    pub level_frames: u32,
    pub target_frames: u32,
    pub offset_ppm: i32,
    pub rate: u32,
    /// One detector per channel of the stream being sent and of the stream being received.
    pub send: [Ppm; MAX_CHANNELS],
    pub receive: [Ppm; MAX_CHANNELS],
    /// A sample at (nearly) full scale was seen since the panel last looked.
    pub send_clip: [bool; MAX_CHANNELS],
    pub receive_clip: [bool; MAX_CHANNELS],
    pub send_channels: usize,
    pub receive_channels: usize,
    /// Meters on: the detectors run on every sample. Off (the panel's M key) skips that work entirely.
    pub meters_on: bool,
    coefficients: Coefficients,
    coefficients_rate: u32,
}

const fn fresh() -> Stats {
    Stats {
        sent: 0, received: 0, lost: 0, level_frames: 0, target_frames: 0, offset_ppm: 0, rate: 48_000,
        send: [Ppm::new(); MAX_CHANNELS], receive: [Ppm::new(); MAX_CHANNELS], send_clip: [false; MAX_CHANNELS], receive_clip: [false; MAX_CHANNELS],
        send_channels: 0, receive_channels: 0, meters_on: true, coefficients: Coefficients::ZERO, coefficients_rate: 0,
    }
}

static mut STATS: Stats = fresh();

pub fn get() -> &'static mut Stats {
    unsafe { &mut *(&raw mut STATS) }
}

pub fn reset() {
    let on = get().meters_on;
    *get() = fresh();
    get().meters_on = on;
}

/// The detector coefficients for the rate the stream runs at (recomputed only when the rate changes).
fn coefficients(stats: &mut Stats) -> Coefficients {
    if stats.coefficients_rate != stats.rate {
        stats.coefficients = Coefficients::for_rate(stats.rate);
        stats.coefficients_rate = stats.rate;
    }
    stats.coefficients
}

/// Feeds big-endian 16/24-bit interleaved samples (the wire format) to the detectors; only the top 16 bits matter to a
/// meter.
pub fn peaks_from_wire(payload: &[u8], channels: usize, bits: usize, frames: usize, send: bool) {
    let stats = get();
    let bytes = bits / 8;
    if !stats.meters_on || channels == 0 || bytes < 2 {
        return;
    }
    let c = coefficients(stats);
    let (detectors, clips) = if send { (&mut stats.send, &mut stats.send_clip) } else { (&mut stats.receive, &mut stats.receive_clip) };
    if send { stats.send_channels = channels } else { stats.receive_channels = channels }
    for frame in 0..frames {
        for channel in 0..channels.min(MAX_CHANNELS) {
            let at = (frame * channels + channel) * bytes;
            let Some(sample) = payload.get(at..at + bytes) else { return };
            let magnitude = i32::from(i16::from_be_bytes([sample[0], sample[1]])).unsigned_abs() << 16;
            detectors[channel].feed(magnitude, &c);
            if magnitude >= CLIP {
                clips[channel] = true;
            }
        }
    }
}

/// The same for 16-bit interleaved samples (what the Opus codec works in); `first` is the first channel's number.
pub fn peaks_from_i16(pcm: &[i16], width: usize, frames: usize, first: usize, send: bool) {
    let stats = get();
    if !stats.meters_on {
        return;
    }
    let c = coefficients(stats);
    let count = if send { &mut stats.send_channels } else { &mut stats.receive_channels };
    *count = (*count).max(first + width);
    let (detectors, clips) = if send { (&mut stats.send, &mut stats.send_clip) } else { (&mut stats.receive, &mut stats.receive_clip) };
    for frame in 0..frames {
        for lane in 0..width {
            let channel = first + lane;
            if channel >= MAX_CHANNELS {
                continue;
            }
            if let Some(&sample) = pcm.get(frame * width + lane) {
                let magnitude = u32::from(sample.unsigned_abs()) << 16;
                detectors[channel].feed(magnitude, &c);
                if magnitude >= CLIP {
                    clips[channel] = true;
                }
            }
        }
    }
}
