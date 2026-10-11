//! Wire formats and sample plumbing for the `audio-net` tape: pure, allocation-free and host-testable.
//!
//! **Network sample format.** Audio crosses the network as interleaved frames of big-endian signed
//! samples, 16 or 24 bits (RTP calls these `L16` and `L24`; RFC 3190 for L24). The kernel's USB audio
//! ring holds frames in the *device's* format: little-endian samples in 2-, 3- or 4-byte slots. The
//! functions here convert between the two and pick, reorder or place channels on the way.
//!
//! **Raw Ethernet transport** (`ETHERTYPE`): an 18-byte header after the Ethernet header, then the
//! samples. There is no addressing beyond MAC addresses; a stream is named by a 32-bit id the sender
//! picks, the receiver locks onto the first sender it hears (until it falls silent) and uses the
//! sequence number to count loss and the frame timestamp to place audio on the timeline.

#![cfg_attr(not(test), no_std)]

pub mod ogg;

pub const ETHERTYPE: u16 = 0x88ba;
/// Announcements of machines on a layer 2 network (raw Ethernet or AVTP, no IP): the same SAP packets as on the
/// IP network, carried straight in Ethernet frames to the broadcast address, so that a name can stand for a MAC.
pub const ANNOUNCE_ETHERTYPE: u16 = 0x88bb;
pub const HEADER_LEN: usize = 18;
pub const ETHERNET_HEADER: usize = 14;
/// Payload bytes a raw frame may carry: a 1500-byte Ethernet payload minus our header.
pub const MAX_RAW_PAYLOAD: usize = 1500 - HEADER_LEN;
/// A conservative payload for RTP over UDP over IPv4: 1500 - 20 (IP) - 8 (UDP) - 12 (RTP).
/// AES67 asks for no more than 1440 bytes of audio per packet.
pub const MAX_RTP_PAYLOAD: usize = 1440;
pub const MAX_CHANNELS: usize = 32;

/// Channels as a bit mask: `ChannelSet::parse("9-10,1")`. Channel numbers are 1-based, as on the devices.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ChannelSet(pub u32);

impl ChannelSet {
    pub fn parse(text: &str) -> Option<ChannelSet> {
        let mut mask = 0u32;
        for part in text.split(',') {
            let (low, high) = match part.split_once('-') {
                Some((low, high)) => (low.parse::<usize>().ok()?, high.parse::<usize>().ok()?),
                None => {
                    let single = part.parse::<usize>().ok()?;
                    (single, single)
                }
            };
            if low == 0 || high < low || high > MAX_CHANNELS {
                return None;
            }
            for channel in low..=high {
                mask |= 1 << (channel - 1);
            }
        }
        (mask != 0).then_some(ChannelSet(mask))
    }

    pub fn count(&self) -> usize {
        self.0.count_ones() as usize
    }

    /// The 0-based channel numbers, ascending.
    pub fn channels(&self) -> impl Iterator<Item = usize> {
        let mask = self.0;
        (0..MAX_CHANNELS).filter(move |&channel| mask & (1 << channel) != 0)
    }
}

/// The header of one raw-Ethernet audio frame (after the 14-byte Ethernet header):
///
/// ```text
///  0     version (1)
///  1     flags (bit 0: first packet of a stream)
///  2-3   sequence number (big endian)
///  4-7   timestamp: the stream's frame counter at the first frame of this packet
///  8-11  stream id
///  12    channels in this packet
///  13    bits per sample (16 or 24)
///  14-15 frames in this packet
///  16-17 sample rate / 25 (so 48000 is 1920 and 192000 is 7680)
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Header {
    pub first: bool,
    pub sequence: u16,
    pub timestamp: u32,
    pub stream: u32,
    pub channels: u8,
    pub bits: u8,
    pub frames: u16,
    pub rate: u32,
}

impl Header {
    pub fn encode(&self, out: &mut [u8]) {
        out[0] = 1;
        out[1] = u8::from(self.first);
        out[2..4].copy_from_slice(&self.sequence.to_be_bytes());
        out[4..8].copy_from_slice(&self.timestamp.to_be_bytes());
        out[8..12].copy_from_slice(&self.stream.to_be_bytes());
        out[12] = self.channels;
        out[13] = self.bits;
        out[14..16].copy_from_slice(&self.frames.to_be_bytes());
        out[16..18].copy_from_slice(&((self.rate / 25) as u16).to_be_bytes());
    }

    pub fn parse(bytes: &[u8]) -> Option<Header> {
        if bytes.len() < HEADER_LEN || bytes[0] != 1 {
            return None;
        }
        let header = Header {
            first: bytes[1] & 1 != 0,
            sequence: u16::from_be_bytes([bytes[2], bytes[3]]),
            timestamp: u32::from_be_bytes(bytes[4..8].try_into().ok()?),
            stream: u32::from_be_bytes(bytes[8..12].try_into().ok()?),
            channels: bytes[12],
            bits: bytes[13],
            frames: u16::from_be_bytes([bytes[14], bytes[15]]),
            rate: u32::from(u16::from_be_bytes([bytes[16], bytes[17]])) * 25,
        };
        let valid = header.channels >= 1 && usize::from(header.channels) <= MAX_CHANNELS && matches!(header.bits, 16 | 24) && header.frames >= 1;
        let length = usize::from(header.frames) * usize::from(header.channels) * usize::from(header.bits / 8);
        (valid && bytes.len() >= HEADER_LEN + length).then_some(header)
    }

    pub fn payload_len(&self) -> usize {
        usize::from(self.frames) * usize::from(self.channels) * usize::from(self.bits / 8)
    }
}

/// The largest frame count per packet from the usual packet times (1 ms = rate/1000 frames, then
/// 500, 250, 125 us, then smaller still) whose payload fits `max_payload`. At least one frame.
pub fn frames_per_packet(rate: u32, channels: usize, bits: usize, max_payload: usize) -> usize {
    let frame_bytes = (channels * bits / 8).max(1);
    for divisor in [1u32, 2, 4, 8, 16, 32, 64] {
        let frames = (rate / 1000 / divisor).max(1) as usize;
        if frames * frame_bytes <= max_payload {
            return frames;
        }
    }
    1
}

/// Like `frames_per_packet`, but no longer than `packet_us` microseconds (at least one frame): a shorter packet
/// time trades network and CPU load for latency.
pub fn frames_per_packet_within(rate: u32, channels: usize, bits: usize, max_payload: usize, packet_us: u32) -> usize {
    let limit = (u64::from(rate) * u64::from(packet_us) / 1_000_000).max(1) as usize;
    // Longer than 1 ms (Wi-Fi: fewer, bigger packets): whole milliseconds, as many as the payload holds.
    let frame_bytes = (channels * bits / 8).max(1);
    let per_ms = (rate / 1000).max(1) as usize;
    for milliseconds in (2..=packet_us / 1000).rev() {
        let frames = per_ms * milliseconds as usize;
        if frames * frame_bytes <= max_payload {
            return frames;
        }
    }
    frames_per_packet(rate, channels, bits, max_payload).min(limit)
}

/// Reads one sample from a device-format slot (little-endian, signed, left-justified in its bytes) as a
/// 32-bit value with the sample in the top bits.
pub fn device_sample(slot: &[u8]) -> i32 {
    let mut value = 0u32;
    for (index, &byte) in slot.iter().enumerate() {
        value |= u32::from(byte) << (8 * (4 - slot.len() + index));
    }
    value as i32
}

/// Packs `frames` frames from device-format `device` (`device_channels` channels in `slot`-byte slots)
/// into network order, keeping the channels in `selection` (in ascending order), `bits` per sample.
/// Returns the bytes written to `out`.
pub fn pack(device: &[u8], slot: usize, device_channels: usize, frames: usize, selection: &ChannelSet, bits: usize, out: &mut [u8]) -> usize {
    let sample_bytes = bits / 8;
    let mut at = 0;
    for frame in device.chunks(slot * device_channels).take(frames) {
        for channel in selection.channels() {
            let value = if channel < device_channels { device_sample(&frame[channel * slot..(channel + 1) * slot]) } else { 0 };
            let bytes = value.to_be_bytes(); // the top `sample_bytes` bytes carry the sample
            out[at..at + sample_bytes].copy_from_slice(&bytes[..sample_bytes]);
            at += sample_bytes;
        }
    }
    at
}

/// The reverse: network-order frames of `wire_channels` channels become device-format frames of
/// `device_channels` channels in `slot`-byte slots. `placement[i]` is the device channel (0-based) that
/// network channel `i` goes to, or `None` to drop it; device channels nothing is placed on stay silent.
pub fn unpack(wire: &[u8], wire_channels: usize, bits: usize, frames: usize, placement: &[Option<usize>], slot: usize, device_channels: usize, out: &mut [u8]) -> usize {
    let sample_bytes = bits / 8;
    let device_frame = slot * device_channels;
    for frame in 0..frames {
        let target = &mut out[frame * device_frame..(frame + 1) * device_frame];
        target.fill(0);
        for (channel, destination) in placement.iter().enumerate().take(wire_channels) {
            let Some(destination) = *destination else { continue };
            if destination >= device_channels {
                continue;
            }
            let at = (frame * wire_channels + channel) * sample_bytes;
            let mut value = 0u32;
            for (index, &byte) in wire[at..at + sample_bytes].iter().enumerate() {
                value |= u32::from(byte) << (8 * (3 - index));
            }
            // Keep the top `slot` bytes of the 32-bit value (little-endian in the slot).
            let bytes = value.to_le_bytes();
            target[destination * slot..(destination + 1) * slot].copy_from_slice(&bytes[4 - slot..]);
        }
    }
    frames * device_frame
}

/// Drift control for a receiver. The sender's and the playback device's clocks differ by a few hundred
/// parts per million, so the playback ring slowly fills or drains. `Fill` watches the ring level against a
/// target and says, per packet, whether to drop one frame (too full), repeat one (too empty) or leave it.
/// This is the simple slip method: a one-frame glitch every few milliseconds at worst; a resampler
/// (stage D of the plan) replaces it with a continuous speed adjustment.
#[derive(Clone, Copy)]
pub struct Fill {
    target_frames: usize,
    tolerance_frames: usize,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Slip {
    None,
    Drop,
    Repeat,
}

impl Fill {
    pub fn new(target_frames: usize) -> Fill {
        Fill { target_frames, tolerance_frames: (target_frames / 2).max(2) }
    }

    pub fn target(&self) -> usize {
        self.target_frames
    }

    /// A new target while running (the adaptive receiver moves it).
    pub fn set_target(&mut self, target_frames: usize) {
        *self = Fill::new(target_frames);
    }

    pub fn advise(&self, level_frames: usize) -> Slip {
        if level_frames > self.target_frames + self.tolerance_frames {
            Slip::Drop
        } else if level_frames + self.tolerance_frames < self.target_frames {
            Slip::Repeat
        } else {
            Slip::None
        }
    }
}

/// Network samples (big endian, 16 or 24 bits) to 32-bit signed values with the sample in the top bits, the
/// working format of the resampler. `frames` frames of `channels` channels go to `out[..frames * channels]`.
pub fn wire_to_frames(wire: &[u8], channels: usize, bits: usize, frames: usize, out: &mut [i32]) {
    let sample_bytes = bits / 8;
    for index in 0..frames * channels {
        let at = index * sample_bytes;
        let mut value = 0u32;
        for (byte_index, &byte) in wire[at..at + sample_bytes].iter().enumerate() {
            value |= u32::from(byte) << (8 * (3 - byte_index));
        }
        out[index] = value as i32;
    }
}

/// 32-bit working frames to device-format frames (`slot`-byte little-endian slots, `device_channels` channels),
/// placing channel `i` of the frames on device channel `placement[i]` (`None` drops it); the rest stay silent.
pub fn frames_to_device(frames: &[i32], channels: usize, count: usize, placement: &[Option<usize>], slot: usize, device_channels: usize, out: &mut [u8]) -> usize {
    let device_frame = slot * device_channels;
    for frame in 0..count {
        let target = &mut out[frame * device_frame..(frame + 1) * device_frame];
        target.fill(0);
        for (channel, destination) in placement.iter().enumerate().take(channels) {
            let Some(destination) = *destination else { continue };
            if destination >= device_channels {
                continue;
            }
            let bytes = (frames[frame * channels + channel] as u32).to_le_bytes();
            target[destination * slot..(destination + 1) * slot].copy_from_slice(&bytes[4 - slot..]);
        }
    }
    count * device_frame
}

/// A variable-rate resampler for multichannel audio: it plays a stream recorded on one clock out on another
/// whose rate differs by a few hundred parts per million, without dropping or repeating a sample.
///
/// Each channel keeps its last four input samples; an output sample is the Catmull-Rom cubic through them,
/// evaluated at a fractional position between the middle two. Feeding one input frame moves the position
/// forward by one; every output frame moves it back by `step` (input frames per output frame, in Q16, 1.0 =
/// 65536). With `step` exactly 1.0 the position stays 0 and the output is the input delayed by one frame,
/// bit for bit; otherwise the interpolation brings a small high-frequency loss (about 0.1 dB at 5 kHz, 1 dB
/// at 15 kHz in the worst case) that is the price of continuous, click-free rate adaptation. Integer
/// arithmetic only.
pub struct Resampler {
    channels: usize,
    history: [[i32; MAX_CHANNELS]; 4],
    /// Position of the next output sample between `history[1]` and `history[2]`, Q16.
    position: u32,
    pub step: u32,
    primed: u8,
}

pub const UNITY_STEP: u32 = 1 << 16;

impl Resampler {
    pub fn new(channels: usize) -> Resampler {
        Resampler { channels: channels.min(MAX_CHANNELS), history: [[0; MAX_CHANNELS]; 4], position: UNITY_STEP, step: UNITY_STEP, primed: 0 }
    }

    pub fn reset(&mut self) {
        *self = Resampler::new(self.channels);
    }

    /// Feeds one input frame (`channels` values); returns whether an output frame is due, and if so writes it
    /// to `out`. Call again with the same frame's successors via `next_output` until it says no.
    pub fn push(&mut self, frame: &[i32]) {
        self.history[0] = self.history[1];
        self.history[1] = self.history[2];
        self.history[2] = self.history[3];
        self.history[3][..self.channels].copy_from_slice(&frame[..self.channels]);
        self.position = self.position.saturating_sub(UNITY_STEP);
        self.primed = (self.primed + 1).min(4);
    }

    /// The next output frame for the frames pushed so far, if the position allows one.
    pub fn next_output(&mut self, out: &mut [i32]) -> bool {
        if self.position >= UNITY_STEP || self.primed < 4 {
            return false;
        }
        let t = i64::from(self.position);
        for channel in 0..self.channels {
            let (h0, h1, h2, h3) = (i64::from(self.history[0][channel]), i64::from(self.history[1][channel]), i64::from(self.history[2][channel]), i64::from(self.history[3][channel]));
            let a = -h0 + 3 * h1 - 3 * h2 + h3;
            let b = 2 * h0 - 5 * h1 + 4 * h2 - h3;
            let c = h2 - h0;
            let inner = ((a * t) >> 16) + b;
            let middle = ((inner * t) >> 16) + c;
            let value = h1 + (((middle * t) >> 16) >> 1);
            out[channel] = value.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
        }
        self.position += self.step;
        true
    }
}

/// The rate controller for a receiver. It watches how full the playback ring is and sets the resampler's step:
/// if the ring runs above its target the receiver is producing too much, so each output frame should use up
/// *more* input (step above 1); if it runs low, less. A proportional term reacts to the level, an integral term
/// learns the steady clock difference (a few hundred ppm), so the level settles on the target and the step on
/// the true ratio. The step is limited to +-2000 ppm.
#[derive(Clone, Copy)]
pub struct Servo {
    target_frames: i32,
    /// The learned steady offset, in parts per billion (to keep integer precision).
    integral_ppb: i64,
    pub step: u32,
}

const MAX_PPB: i64 = 2_000_000;

impl Servo {
    pub fn new(target_frames: usize) -> Servo {
        Servo { target_frames: target_frames as i32, integral_ppb: 0, step: UNITY_STEP }
    }

    pub fn target(&self) -> usize {
        self.target_frames as usize
    }

    /// A new target while running; the learned clock offset is kept.
    pub fn set_target(&mut self, target_frames: usize) {
        self.target_frames = target_frames as i32;
    }

    /// One update: the ring `level` in frames, after `frames` input frames at `rate` Hz. Returns the new step.
    pub fn update(&mut self, level: usize, frames: usize, rate: u32) -> u32 {
        let error = i64::from(level as i32 - self.target_frames);
        // The plant is an integrator (the level moves by rate x step offset per second), so the loop is second
        // order: natural frequency w = sqrt(Ki * rate), damping z = Kp * rate / (2 w). At 48 kHz these gains give
        // w = 0.14 rad/s (a ~45 s period) and z = 0.87: it settles without ringing.
        // Integral: 4e-7 per frame-second -> 400 ppb * error * (frames / rate).
        // Proportional: 5 ppm per frame of error.
        let proportional = 5000 * error;
        // Anti-windup: while the output is already at its limit and the error pushes further that way, the integral
        // stands still (otherwise it saturates as well and the loop takes long to come back once the error changes sign).
        let saturated = (self.integral_ppb + proportional).abs() >= MAX_PPB && (self.integral_ppb + proportional).signum() == error.signum();
        if !saturated {
            self.integral_ppb = (self.integral_ppb + 400 * error * frames as i64 / i64::from(rate.max(1))).clamp(-MAX_PPB, MAX_PPB);
        }
        let ppb = (self.integral_ppb + proportional).clamp(-MAX_PPB, MAX_PPB);
        self.step = (i64::from(UNITY_STEP) + ppb * i64::from(UNITY_STEP) / 1_000_000_000) as u32;
        self.step
    }

    /// The learned steady offset in parts per million (for the statistics line).
    pub fn offset_ppm(&self) -> i32 {
        (self.integral_ppb / 1000) as i32
    }
}

/// Everything an SDP description of one RTP audio stream needs.
pub struct Sdp<'a> {
    /// The session name (`s=`): what a peer list shows.
    pub name: &'a str,
    /// True for a receiver announcing where it listens (`a=recvonly`) rather than a sender (`a=sendonly`).
    pub receive_only: bool,
    /// A machine that is only present (its panel is open) and not streaming: `a=x-fos-idle`.
    pub idle: bool,
    pub origin: [u8; 4],
    pub destination: [u8; 4],
    pub port: u16,
    pub payload_type: u8,
    pub channels: usize,
    pub rate: u32,
    pub bits: usize,
    /// Microseconds of audio per packet (AES67: 125, 250, 333, 1000 or 4000).
    pub packet_us: u32,
    /// The sender's MAC: stands in for a PTP clock in `ts-refclk` (RFC 7273's `localmac`).
    pub mac: [u8; 6],
    /// The grandmaster's clock identity when the stream's RTP timestamps are on a PTP timeline (`ts-refclk:ptp=`,
    /// AES67); `None` when they count from an arbitrary start.
    pub ptp: Option<[u8; 8]>,
    pub session: u32,
}

/// A tiny `core::fmt::Write` into a byte slice.
pub struct Cursor<'a> {
    pub bytes: &'a mut [u8],
    pub length: usize,
}

impl core::fmt::Write for Cursor<'_> {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        let end = self.length + text.len();
        if end > self.bytes.len() {
            return Err(core::fmt::Error);
        }
        self.bytes[self.length..end].copy_from_slice(text.as_bytes());
        self.length = end;
        Ok(())
    }
}

fn is_multicast(address: [u8; 4]) -> bool {
    (224..=239).contains(&address[0])
}

/// Writes an AES67-style SDP (RFC 4566, RFC 3190 `L16`/`L24`, RFC 7273 clocking) into `out` and returns its
/// length. Lines end in CRLF as SDP requires. The `ts-refclk` is the PTP grandmaster when the sender's
/// timestamps are on its timeline (`Sdp::ptp`), else the sender's MAC (`localmac`): a receiver that insists on a
/// PTP reference will not accept the latter, one that only needs the packet format will (ffplay, GStreamer, VLC
/// with the right flags).
pub fn write_sdp(sdp: &Sdp<'_>, out: &mut [u8]) -> usize {
    use core::fmt::Write;
    let mut cursor = Cursor { bytes: out, length: 0 };
    let a = sdp.origin;
    let d = sdp.destination;
    let milliseconds = sdp.packet_us / 1000;
    let thousandths = sdp.packet_us % 1000;
    let _ = write!(cursor, "v=0\r\no=- {0} {0} IN IP4 {1}.{2}.{3}.{4}\r\ns={5}\r\n", sdp.session, a[0], a[1], a[2], a[3], sdp.name);
    let _ = write!(cursor, "c=IN IP4 {}.{}.{}.{}", d[0], d[1], d[2], d[3]);
    if is_multicast(d) {
        let _ = write!(cursor, "/32");
    }
    let _ = write!(cursor, "\r\nt=0 0\r\nm=audio {} RTP/AVP {}\r\n", sdp.port, sdp.payload_type);
    let _ = write!(cursor, "a=rtpmap:{} L{}/{}/{}\r\n", sdp.payload_type, sdp.bits, sdp.rate, sdp.channels);
    if thousandths == 0 {
        let _ = write!(cursor, "a=ptime:{milliseconds}\r\n");
    } else {
        // 0.125, 0.25, 0.333 ...: the fraction without trailing zeros
        let mut fraction = [b'0'; 3];
        fraction[0] = b'0' + (thousandths / 100) as u8;
        fraction[1] = b'0' + (thousandths / 10 % 10) as u8;
        fraction[2] = b'0' + (thousandths % 10) as u8;
        let mut end = 3;
        while end > 1 && fraction[end - 1] == b'0' {
            end -= 1;
        }
        let _ = write!(cursor, "a=ptime:{milliseconds}.{}\r\n", core::str::from_utf8(&fraction[..end]).unwrap_or("0"));
    }
    let m = sdp.mac;
    if let Some(g) = sdp.ptp {
        let _ = write!(cursor, "a=ts-refclk:ptp=IEEE1588-2008:{:02X}-{:02X}-{:02X}-{:02X}-{:02X}-{:02X}-{:02X}-{:02X}:0\r\n", g[0], g[1], g[2], g[3], g[4], g[5], g[6], g[7]);
    } else {
        let _ = write!(cursor, "a=ts-refclk:localmac={:02X}-{:02X}-{:02X}-{:02X}-{:02X}-{:02X}\r\n", m[0], m[1], m[2], m[3], m[4], m[5]);
    }
    let _ = write!(cursor, "a=mediaclk:direct=0\r\na={}\r\n", if sdp.receive_only { "recvonly" } else { "sendonly" });
    if sdp.idle {
        let _ = write!(cursor, "a=x-fos-idle\r\n");
    }
    cursor.length
}

/// An SDP for an Opus session: one RTP stream per stereo pair (and one mono stream for an odd channel), on
/// consecutive UDP ports `port`, `port + 2`, ... (RFC 7587 `opus/48000/2`; `stereo=1` and `sprop-stereo=1` say the
/// stream is stereo, and a mono last stream says `0`).
pub struct SdpOpus<'a> {
    pub name: &'a str,
    pub receive_only: bool,
    /// A machine that is only present (its panel is open) and not streaming: `a=x-fos-idle`.
    pub idle: bool,
    pub origin: [u8; 4],
    pub destination: [u8; 4],
    pub port: u16,
    pub payload_type: u8,
    pub channels: usize,
    /// Milliseconds of audio per packet (the Opus frame size), times ten (25 for 2.5 ms).
    pub packet_ms_x10: u32,
    pub session: u32,
}

pub fn write_sdp_opus(sdp: &SdpOpus<'_>, out: &mut [u8]) -> usize {
    use core::fmt::Write;
    let mut cursor = Cursor { bytes: out, length: 0 };
    let (a, d) = (sdp.origin, sdp.destination);
    let _ = write!(cursor, "v=0\r\no=- {0} {0} IN IP4 {1}.{2}.{3}.{4}\r\ns={5}\r\n", sdp.session, a[0], a[1], a[2], a[3], sdp.name);
    let _ = write!(cursor, "c=IN IP4 {}.{}.{}.{}", d[0], d[1], d[2], d[3]);
    if is_multicast(d) {
        let _ = write!(cursor, "/32");
    }
    let _ = write!(cursor, "\r\nt=0 0\r\na={}\r\n", if sdp.receive_only { "recvonly" } else { "sendonly" });
    if sdp.idle {
        let _ = write!(cursor, "a=x-fos-idle\r\n");
    }
    let streams = sdp.channels.div_ceil(2);
    for stream in 0..streams {
        let stereo = !(stream == streams - 1 && sdp.channels % 2 == 1);
        let _ = write!(cursor, "m=audio {} RTP/AVP {}\r\n", sdp.port as usize + 2 * stream, sdp.payload_type);
        let _ = write!(cursor, "a=rtpmap:{0} opus/48000/2\r\na=fmtp:{0} stereo={1};sprop-stereo={1};maxplaybackrate=48000\r\n", sdp.payload_type, u8::from(stereo));
        if sdp.packet_ms_x10 % 10 == 0 {
            let _ = write!(cursor, "a=ptime:{}\r\n", sdp.packet_ms_x10 / 10);
        } else {
            let _ = write!(cursor, "a=ptime:{}.{}\r\n", sdp.packet_ms_x10 / 10, sdp.packet_ms_x10 % 10);
        }
    }
    cursor.length
}

/// The 16-bit value of a device-format slot (`slot` little-endian bytes, left-justified): its top 16 bits.
pub fn device_sample_16(slot_bytes: &[u8]) -> i16 {
    (device_sample(slot_bytes) >> 16) as i16
}

/// A programme peak meter (PPM) in the Nordic style (IEC 60268-10 type I): a quasi-peak detector that charges fast
/// (a 5 ms burst reads about 2 dB below its height, 10 ms about 1 dB) and falls back slowly, 20 dB in about
/// 1.7 s, whatever the level. Integer arithmetic only, one multiply per sample. Levels are magnitudes scaled to 2^31 =
/// digital full scale; `db_tenths` turns them into tenths of a dB below/above full scale.
///
/// The Nordic scale has TEST (0) at -18 dBFS (the EBU line-up level) and the permitted maximum level at +9, which is
/// -9 dBFS; digital full scale is +18 on that scale.
pub mod meter {
    /// Full scale in the detector's units (the 16 bits of a sample moved to the top of an `i32`).
    pub const FULL_SCALE: u32 = 0x7fff_0000;
    /// The Nordic scale's 0 (TEST) in dBFS.
    pub const NORDIC_TEST_DBFS: i32 = -18;
    /// A sample this close to full scale (about -0.04 dB) counts as clipping.
    pub const CLIP: u32 = 0x7ff0_0000;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Coefficients {
        /// Fraction (of 2^16) of the gap to a higher input closed per sample.
        attack: u32,
        /// Fraction (of 2^26) of the level lost per sample.
        release: u32,
    }

    impl Coefficients {
        /// An inert value to start from (`for_rate` makes a useful one).
        pub const ZERO: Coefficients = Coefficients { attack: 0, release: 0 };

        /// For a sample rate: a 5 ms burst reads about 2 dB low and a 10 ms one about 1 dB low (type I), and the level falls back 20 dB in
        /// 1.7 s.
        pub fn for_rate(rate: u32) -> Coefficients {
            let rate = rate.max(8_000);
            Coefficients { attack: (1000 * 48_000 / rate).max(1), release: (1893 * 48_000 / rate).max(1) }
        }
    }

    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct Ppm {
        /// The detector's reading now.
        pub level: u32,
        /// The highest reading since `take_max` last looked (the display samples a few times a second; a peak
        /// that came and went in between must still show).
        pub highest: u32,
    }

    impl Ppm {
        pub const fn new() -> Ppm {
            Ppm { level: 0, highest: 0 }
        }

        /// Feeds the magnitude of one sample.
        #[inline]
        pub fn feed(&mut self, magnitude: u32, c: &Coefficients) {
            if magnitude > self.level {
                self.level += (u64::from(magnitude - self.level) * u64::from(c.attack) >> 16) as u32;
            } else {
                self.level -= ((u64::from(self.level) * u64::from(c.release)) >> 26) as u32;
            }
            if self.level > self.highest {
                self.highest = self.level;
            }
        }

        /// The highest reading since the last call (and restarts from the current one).
        pub fn take_max(&mut self) -> u32 {
            let highest = self.highest;
            self.highest = self.level;
            highest
        }
    }

    /// log2 of `value` (> 0) in Q16, by the squaring method: exact to well under 0.001 dB.
    pub fn log2_q16(value: u32) -> i32 {
        let whole = 31 - value.leading_zeros() as i32;
        // Normalise to [1, 2) in Q31.
        let mut mantissa = u64::from(value) << (31 - whole);
        let mut fraction = 0u32;
        for _ in 0..16 {
            mantissa = (mantissa * mantissa) >> 31;
            fraction <<= 1;
            if mantissa >= 1 << 32 {
                mantissa >>= 1;
                fraction |= 1;
            }
        }
        (whole << 16) + fraction as i32
    }

    /// A level as tenths of a dB relative to digital full scale (0 at full scale, negative below). Silence gives
    /// -1200 (-120 dB).
    pub fn db_tenths(level: u32) -> i32 {
        if level < 64 {
            return -1200;
        }
        let octaves = i64::from(log2_q16(level) - log2_q16(FULL_SCALE));
        // 6.0206 dB per octave = 60.206 tenths.
        ((octaves * 60_206 / 1000 + (1 << 15)) >> 16) as i32
    }
}

/// IEEE 1722 (AVTP) audio: the AAF PCM format, the layer-2 transport of AVB. An AVTPDU is a 24-byte header and
/// the samples, big endian and interleaved, directly in an Ethernet frame of ethertype 0x22f0.
pub mod avtp {
    pub const ETHERTYPE: u16 = 0x22f0;
    pub const HEADER_LEN: usize = 24;
    /// Subtype of the AVTP Audio Format.
    const SUBTYPE_AAF: u8 = 0x02;
    /// The AAF `format` values for integer PCM.
    pub const FORMAT_INT24: u8 = 3;
    pub const FORMAT_INT16: u8 = 4;
    /// The AAF `nsr` (nominal sample rate) codes: 8, 16, 32, 44.1, 48, 88.2, 96, 176.4, 192 kHz.
    const RATES: [u32; 9] = [8_000, 16_000, 32_000, 44_100, 48_000, 88_200, 96_000, 176_400, 192_000];

    pub fn rate_code(rate: u32) -> Option<u8> {
        RATES.iter().position(|&known| known == rate).map(|index| index as u8 + 1)
    }

    pub fn code_rate(code: u8) -> Option<u32> {
        RATES.get(usize::from(code).checked_sub(1)?).copied()
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Header {
        /// Only 8 bits on the wire.
        pub sequence: u8,
        /// Whether `timestamp` (the presentation time, nanoseconds of the gPTP clock) is valid.
        pub timestamp_valid: bool,
        pub stream_id: [u8; 8],
        pub timestamp: u32,
        pub channels: u16,
        pub bits: u8,
        pub rate: u32,
        /// Bytes of samples after the header.
        pub data_length: u16,
    }

    impl Header {
        /// Writes the 24 bytes; false if the rate or bit depth has no AAF code.
        pub fn encode(&self, out: &mut [u8]) -> bool {
            let (Some(code), format) = (rate_code(self.rate), if self.bits == 24 { FORMAT_INT24 } else { FORMAT_INT16 }) else { return false };
            out[0] = SUBTYPE_AAF;
            out[1] = 0x80 | if self.timestamp_valid { 0x01 } else { 0 }; // sv = 1, version 0, mr/gv 0, tv
            out[2] = self.sequence;
            out[3] = 0; // tu: timestamp uncertain
            out[4..12].copy_from_slice(&self.stream_id);
            out[12..16].copy_from_slice(&self.timestamp.to_be_bytes());
            out[16] = format;
            out[17] = (code << 4) | ((self.channels >> 8) & 3) as u8; // nsr (4 bits), 2 reserved, channels (10 bits)
            out[18] = self.channels as u8;
            out[19] = self.bits;
            out[20..22].copy_from_slice(&self.data_length.to_be_bytes());
            out[22] = 0; // sp, evt
            out[23] = 0;
            true
        }

        pub fn parse(bytes: &[u8]) -> Option<Header> {
            if bytes.len() < HEADER_LEN || bytes[0] != SUBTYPE_AAF || bytes[1] & 0x80 == 0 || (bytes[1] >> 4) & 7 != 0 {
                return None;
            }
            let format = bytes[16];
            let bits = bytes[19];
            if !(format == FORMAT_INT24 && bits == 24 || format == FORMAT_INT16 && bits == 16) {
                return None;
            }
            let channels = (u16::from(bytes[17] & 3) << 8) | u16::from(bytes[18]);
            let data_length = u16::from_be_bytes([bytes[20], bytes[21]]);
            if channels == 0 || usize::from(data_length) > bytes.len() - HEADER_LEN || usize::from(data_length) % (usize::from(channels) * usize::from(bits) / 8) != 0 {
                return None;
            }
            Some(Header {
                sequence: bytes[2],
                timestamp_valid: bytes[1] & 1 != 0,
                stream_id: bytes[4..12].try_into().ok()?,
                timestamp: u32::from_be_bytes(bytes[12..16].try_into().ok()?),
                channels,
                bits,
                rate: code_rate(bytes[17] >> 4)?,
                data_length,
            })
        }
    }
}

/// Session announcements: SAP (RFC 2974) carrying SDP, and a reader for the SDP this stack writes (and the simple
/// SDP other AES67 gear announces). Senders and receivers announce themselves on 239.255.255.255:9875 so a peer list
/// can show them and a name can stand in for an address.
pub mod sap {
    pub const GROUP: [u8; 4] = [239, 255, 255, 255];
    pub const PORT: u16 = 9875;
    pub const MAX_NAME: usize = 24;

    /// Writes a SAP packet (version 1, IPv4 origin, no authentication or compression, payload type `application/sdp`)
    /// into `out`; `delete` says the session is going away. Returns the length.
    pub fn encode(origin: [u8; 4], delete: bool, id: u16, sdp: &[u8], out: &mut [u8]) -> usize {
        const TYPE: &[u8] = b"application/sdp\0";
        out[0] = 0x20 | if delete { 0x04 } else { 0 }; // V=1, A=0 (IPv4), R=0, T, E=0, C=0
        out[1] = 0; // no authentication data
        out[2..4].copy_from_slice(&id.to_be_bytes());
        out[4..8].copy_from_slice(&origin);
        out[8..8 + TYPE.len()].copy_from_slice(TYPE);
        let start = 8 + TYPE.len();
        out[start..start + sdp.len()].copy_from_slice(sdp);
        start + sdp.len()
    }

    #[derive(Debug, PartialEq, Eq)]
    pub struct Packet<'a> {
        pub origin: [u8; 4],
        pub delete: bool,
        pub id: u16,
        pub sdp: &'a [u8],
    }

    /// Reads a SAP packet; `None` for IPv6, encrypted, compressed or malformed ones.
    pub fn parse(bytes: &[u8]) -> Option<Packet<'_>> {
        if bytes.len() < 8 || bytes[0] >> 5 != 1 || bytes[0] & 0x10 != 0 || bytes[0] & 0x03 != 0 {
            return None;
        }
        let authentication = usize::from(bytes[1]) * 4;
        let mut start = 8 + authentication;
        let body = bytes.get(start..)?;
        // An optional NUL-terminated payload type; absent when the payload already starts with "v=0".
        if !body.starts_with(b"v=0") {
            start += body.iter().position(|&byte| byte == 0)? + 1;
        }
        Some(Packet { origin: bytes[4..8].try_into().ok()?, delete: bytes[0] & 0x04 != 0, id: u16::from_be_bytes([bytes[2], bytes[3]]), sdp: bytes.get(start..)? })
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Codec {
        L16,
        L24,
        Opus,
    }

    /// What a peer list needs from an SDP.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Session {
        pub name: [u8; MAX_NAME],
        pub name_length: usize,
        /// The session id from `o=`: a peer that restarts with new parameters changes it.
        pub session: u32,
        pub origin: [u8; 4],
        pub destination: [u8; 4],
        pub port: u16,
        pub payload_type: u8,
        pub codec: Codec,
        pub rate: u32,
        pub channels: usize,
        /// Microseconds (PCM) or the Opus frame time.
        pub packet_us: u32,
        pub receive_only: bool,
        /// The announcer is only present, not streaming.
        pub idle: bool,
        /// A layer 2 announcement (`a=x-fos-l2:raw` or `avtp`): 0 for an IP one, 1 raw Ethernet, 2 AVTP.
        pub l2: u8,
    }

    impl Session {
        pub fn name(&self) -> &str {
            core::str::from_utf8(&self.name[..self.name_length]).unwrap_or("?")
        }
    }

    fn parse_ip(text: &str) -> Option<[u8; 4]> {
        let mut octets = [0u8; 4];
        let mut parts = text.split('.');
        for octet in octets.iter_mut() {
            *octet = parts.next()?.parse().ok()?;
        }
        parts.next().is_none().then_some(octets)
    }

    /// Reads a session description: one or more `m=audio` lines (an Opus session has one per stereo pair), the
    /// connection address, the name, and the first `rtpmap` for the codec.
    pub fn parse_sdp(sdp: &[u8]) -> Option<Session> {
        let text = core::str::from_utf8(sdp).ok()?;
        let mut session = Session { name: [0; MAX_NAME], name_length: 0, session: 0, origin: [0; 4], destination: [0; 4], port: 0, payload_type: 0, codec: Codec::L24, rate: 48_000, channels: 0, packet_us: 1000, receive_only: false, idle: false, l2: 0 };
        let (mut have_media, mut have_codec, mut media_lines, mut last_mono) = (false, false, 0usize, false);
        for line in text.lines() {
            let line = line.trim_end();
            let Some((kind, value)) = line.split_once('=') else { continue };
            match kind {
                "s" => {
                    let bytes = value.as_bytes();
                    session.name_length = bytes.len().min(MAX_NAME);
                    session.name[..session.name_length].copy_from_slice(&bytes[..session.name_length]);
                }
                "o" => {
                    let mut fields = value.split_ascii_whitespace();
                    fields.next();
                    session.session = fields.next().and_then(|id| id.parse().ok()).unwrap_or(0);
                    session.origin = fields.nth(3).and_then(parse_ip).unwrap_or([0; 4]);
                }
                "c" => {
                    let address = value.split_ascii_whitespace().nth(2)?.split('/').next()?;
                    session.destination = parse_ip(address)?;
                }
                "m" => {
                    let mut fields = value.split_ascii_whitespace();
                    if fields.next() != Some("audio") {
                        continue;
                    }
                    let port: u16 = fields.next()?.parse().ok()?;
                    let payload_type: u8 = fields.nth(1)?.parse().ok()?;
                    if !have_media {
                        (session.port, session.payload_type, have_media) = (port, payload_type, true);
                    }
                    media_lines += 1;
                    last_mono = false;
                }
                "a" => {
                    if let Some(map) = value.strip_prefix("rtpmap:") {
                        let (payload, encoding) = map.split_once(' ')?;
                        if payload.parse::<u8>().ok() != Some(session.payload_type) || have_codec {
                            continue;
                        }
                        let mut parts = encoding.split('/');
                        let encoding_name = parts.next()?;
                        let codec = if encoding_name.eq_ignore_ascii_case("l16") {
                            Codec::L16
                        } else if encoding_name.eq_ignore_ascii_case("l24") {
                            Codec::L24
                        } else if encoding_name.eq_ignore_ascii_case("opus") {
                            Codec::Opus
                        } else {
                            return None;
                        };
                        session.codec = codec;
                        session.rate = parts.next()?.parse().ok()?;
                        if codec != Codec::Opus {
                            session.channels = parts.next().and_then(|count| count.parse().ok()).unwrap_or(1);
                        }
                        have_codec = true;
                    } else if value.starts_with("fmtp:") && value.contains("stereo=0") {
                        last_mono = true;
                    } else if let Some(time) = value.strip_prefix("ptime:") {
                        let (whole, fraction) = time.split_once('.').unwrap_or((time, "0"));
                        let mut micros = whole.parse::<u32>().ok()? * 1000;
                        for (index, digit) in fraction.bytes().take(3).enumerate() {
                            micros += u32::from(digit - b'0') * [100, 10, 1][index];
                        }
                        session.packet_us = micros;
                    } else if value == "x-fos-idle" {
                        session.idle = true;
                    } else if let Some(kind) = value.strip_prefix("x-fos-l2:") {
                        session.l2 = if kind == "avtp" { 2 } else { 1 };
                    } else if value == "recvonly" {
                        session.receive_only = true;
                    }
                }
                _ => {}
            }
        }
        if session.codec == Codec::Opus {
            session.channels = media_lines * 2 - usize::from(last_mono);
            session.packet_us = if session.packet_us == 1000 { 20_000 } else { session.packet_us };
        }
        (have_media && have_codec && session.channels > 0).then_some(session)
    }
}

/// The Ethernet destination for an IPv4 multicast address: 01:00:5e plus the low 23 bits (RFC 1112).
pub fn multicast_mac(address: [u8; 4]) -> [u8; 6] {
    [0x01, 0x00, 0x5e, address[1] & 0x7f, address[2], address[3]]
}

pub fn is_multicast_address(address: [u8; 4]) -> bool {
    is_multicast(address)
}

#[cfg(test)]
mod tests {
    #[test]
    fn packet_time_caps_the_frame_count() {
        use super::*;
        assert_eq!(frames_per_packet_within(48_000, 2, 24, 1400, 1000), 48);
        assert_eq!(frames_per_packet_within(48_000, 2, 24, 1400, 250), 12);
        assert_eq!(frames_per_packet_within(48_000, 2, 24, 1400, 125), 6);
        assert_eq!(frames_per_packet_within(48_000, 32, 24, 1400, 1000), 12); // the payload limit still applies
    }

    #[test]
    fn servo_and_fill_take_a_new_target() {
        use super::*;
        let mut servo = Servo::new(576);
        servo.update(600, 48, 48_000);
        let learned = servo.offset_ppm();
        servo.set_target(144);
        assert_eq!(servo.target(), 144);
        assert_eq!(servo.offset_ppm(), learned);
        let mut fill = Fill::new(576);
        fill.set_target(144);
        assert_eq!(fill.target(), 144);
    }

    use super::*;

    #[test]
    fn channel_sets() {
        let set = ChannelSet::parse("9-10,1").unwrap();
        assert_eq!(set.count(), 3);
        assert_eq!(set.channels().collect::<Vec<_>>(), vec![0, 8, 9]);
        assert!(ChannelSet::parse("0").is_none());
        assert!(ChannelSet::parse("3-2").is_none());
        assert!(ChannelSet::parse("33").is_none());
        assert!(ChannelSet::parse("a").is_none());
    }

    #[test]
    fn header_round_trip() {
        let header = Header { first: true, sequence: 513, timestamp: 0xdead_beef, stream: 7, channels: 2, bits: 24, frames: 48, rate: 48_000 };
        let mut bytes = [0u8; HEADER_LEN + 288];
        header.encode(&mut bytes);
        assert_eq!(Header::parse(&bytes), Some(header));
        assert!(Header::parse(&bytes[..HEADER_LEN + 100]).is_none(), "a truncated payload is refused");
        bytes[0] = 2;
        assert!(Header::parse(&bytes).is_none(), "an unknown version is refused");
    }

    #[test]
    fn packet_sizes_fit() {
        assert_eq!(frames_per_packet(48_000, 2, 24, MAX_RTP_PAYLOAD), 48); // 1 ms stereo: 288 bytes
        assert_eq!(frames_per_packet_within(48_000, 2, 24, MAX_RTP_PAYLOAD, 2000), 96); // 2 ms stereo: 576 bytes
        assert_eq!(frames_per_packet_within(48_000, 8, 24, MAX_RTP_PAYLOAD, 2000), 48); // 2 ms of 8 channels is 2304 bytes
        assert_eq!(frames_per_packet_within(48_000, 2, 24, MAX_RTP_PAYLOAD, 1000), 48);
        assert_eq!(frames_per_packet(48_000, 22, 24, MAX_RTP_PAYLOAD), 12); // 22 channels need 250 us: 792 bytes (1 ms is 3168)
        assert_eq!(frames_per_packet(96_000, 22, 24, MAX_RTP_PAYLOAD), 12); // 96 kHz: 1 ms = 96 frames, 250 us = 24 (1584 B) too big, 125 us = 12
        assert_eq!(frames_per_packet(48_000, 64, 24, 1000), 3);
    }

    #[test]
    fn device_to_wire_24_bit() {
        // two frames of a 3-channel device with 3-byte slots; select channels 1 and 3
        let device = [
            0x01, 0x02, 0x03, 0xaa, 0xbb, 0xcc, 0x10, 0x20, 0x30, // frame 0
            0xfe, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80, // frame 1: -2, 0, -8388608
        ];
        let mut out = [0u8; 12];
        let written = pack(&device, 3, 3, 2, &ChannelSet(0b101), 24, &mut out);
        assert_eq!(written, 12);
        // little-endian 0x030201 becomes big-endian 03 02 01
        assert_eq!(out, [0x03, 0x02, 0x01, 0x30, 0x20, 0x10, 0xff, 0xff, 0xfe, 0x80, 0x00, 0x00]);
    }

    #[test]
    fn widths_convert_both_ways() {
        // 24-bit device -> 16-bit wire keeps the top 16 bits
        let device = [0x34, 0x12, 0x7f]; // 0x7f1234
        let mut out = [0u8; 2];
        pack(&device, 3, 1, 1, &ChannelSet(1), 16, &mut out);
        assert_eq!(out, [0x7f, 0x12]);
        // 16-bit wire -> 24-bit device: shifted up, low byte zero
        let wire = [0x7f, 0x12];
        let mut back = [0u8; 3];
        unpack(&wire, 1, 16, 1, &[Some(0)], 3, 1, &mut back);
        assert_eq!(back, [0x00, 0x12, 0x7f]);
        // 24-bit wire -> 16-bit device: truncated
        let wire = [0x7f, 0x12, 0x34];
        let mut back = [0u8; 2];
        unpack(&wire, 1, 24, 1, &[Some(0)], 2, 1, &mut back);
        assert_eq!(back, [0x12, 0x7f]);
    }

    #[test]
    fn placement_and_silence() {
        // a 2-channel network stream placed on channels 3 and 1 of a 4-channel device with 2-byte slots
        let wire = [0x11, 0x22, 0x33, 0x44]; // L=0x1122, R=0x3344 (16-bit BE)
        let mut out = [0xffu8; 8];
        unpack(&wire, 2, 16, 1, &[Some(2), Some(0)], 2, 4, &mut out);
        assert_eq!(out, [0x44, 0x33, 0, 0, 0x22, 0x11, 0, 0]);
    }

    #[test]
    fn round_trip_is_lossless_at_equal_width() {
        let mut device = [0u8; 6 * 3 * 4];
        for (index, byte) in device.iter_mut().enumerate() {
            *byte = (index * 37 + 11) as u8;
        }
        let mut wire = [0u8; 4 * 6 * 3];
        pack(&device, 3, 6, 4, &ChannelSet(0b111111), 24, &mut wire);
        let mut back = [0u8; 6 * 3 * 4];
        let placement: Vec<Option<usize>> = (0..6).map(Some).collect();
        unpack(&wire, 6, 24, 4, &placement, 3, 6, &mut back);
        assert_eq!(back, device);
    }

    #[test]
    fn sdp_for_a_stereo_stream() {
        let sdp = Sdp { name: "studio", receive_only: false, idle: false, origin: [192, 168, 1, 10], destination: [239, 69, 1, 1], port: 5004, payload_type: 96, channels: 2, rate: 48_000, bits: 24, packet_us: 1000, mac: [0x18, 0x66, 0xda, 0x25, 0x1f, 0x73], ptp: None, session: 7 };
        let mut out = [0u8; 512];
        let length = write_sdp(&sdp, &mut out);
        let text = core::str::from_utf8(&out[..length]).unwrap();
        assert!(text.starts_with("v=0\r\no=- 7 7 IN IP4 192.168.1.10\r\n"));
        assert!(text.contains("c=IN IP4 239.69.1.1/32\r\n"), "multicast carries a TTL");
        assert!(text.contains("m=audio 5004 RTP/AVP 96\r\n"));
        assert!(text.contains("a=rtpmap:96 L24/48000/2\r\n"));
        assert!(text.contains("a=ptime:1\r\n"));
        assert!(text.contains("a=ts-refclk:localmac=18-66-DA-25-1F-73\r\n"));
        assert!(text.contains("a=mediaclk:direct=0\r\n"));
    }

    #[test]
    fn sdp_names_the_ptp_grandmaster_when_the_timestamps_follow_it() {
        let sdp = Sdp { name: "x", receive_only: false, idle: false, origin: [10, 0, 0, 1], destination: [239, 1, 1, 1], port: 5004, payload_type: 96, channels: 2, rate: 48_000, bits: 24, packet_us: 1000, mac: [1; 6], ptp: Some([0x8c, 0xec, 0x4b, 0xff, 0xfe, 0xf3, 0x4a, 0x20]), session: 1 };
        let mut buffer = [0u8; 600];
        let length = write_sdp(&sdp, &mut buffer);
        let text = core::str::from_utf8(&buffer[..length]).unwrap();
        assert!(text.contains("a=ts-refclk:ptp=IEEE1588-2008:8C-EC-4B-FF-FE-F3-4A-20:0\r\n"));
        assert!(!text.contains("localmac"));
        assert!(text.contains("a=mediaclk:direct=0\r\n"));
    }

    #[test]
    fn sdp_packet_times_and_unicast() {
        let mut sdp = Sdp { name: "x", receive_only: false, idle: false, origin: [10, 0, 0, 1], destination: [10, 0, 0, 2], port: 6000, payload_type: 97, channels: 22, rate: 96_000, bits: 16, packet_us: 125, mac: [0; 6], ptp: None, session: 1 };
        let mut out = [0u8; 512];
        let length = write_sdp(&sdp, &mut out);
        let text = core::str::from_utf8(&out[..length]).unwrap();
        assert!(text.contains("c=IN IP4 10.0.0.2\r\n"), "unicast has no TTL");
        assert!(text.contains("a=ptime:0.125\r\n"));
        assert!(text.contains("a=rtpmap:97 L16/96000/22\r\n"));
        sdp.packet_us = 250;
        let length = write_sdp(&sdp, &mut out);
        assert!(core::str::from_utf8(&out[..length]).unwrap().contains("a=ptime:0.25\r\n"));
        assert!(write_sdp(&sdp, &mut [0u8; 20]) <= 20, "a short buffer is not overrun");
    }

    #[test]
    fn multicast_mapping() {
        assert_eq!(multicast_mac([239, 255, 1, 2]), [0x01, 0x00, 0x5e, 0x7f, 1, 2]);
        assert_eq!(multicast_mac([224, 128, 0, 1]), [0x01, 0x00, 0x5e, 0x00, 0, 1], "the top bit of the second octet is dropped");
        assert!(is_multicast_address([239, 1, 1, 1]) && !is_multicast_address([192, 168, 1, 1]));
    }

    fn sine_frames(count: usize, frequency: f64, rate: f64) -> Vec<i32> {
        (0..count).map(|n| ((n as f64 * frequency * 2.0 * core::f64::consts::PI / rate).sin() * 0.5 * i32::MAX as f64) as i32).collect()
    }

    #[test]
    fn unity_step_is_a_one_frame_delay_and_exact() {
        let mut resampler = Resampler::new(2);
        let input: Vec<i32> = (0..400).flat_map(|n| [n * 1000, -n * 777]).collect();
        let mut output = Vec::new();
        let mut frame = [0i32; 2];
        for pair in input.chunks(2) {
            resampler.push(pair);
            while resampler.next_output(&mut frame) {
                output.extend_from_slice(&frame);
            }
        }
        // After the four-frame warm-up every input frame yields one output, equal to the input two frames back.
        let produced = output.len() / 2;
        assert!(produced >= 395 - 4, "{produced}");
        for index in 0..produced {
            let source = index + 1; // the first output is history[1] when the fourth frame arrived: input frame 1
            assert_eq!(&output[index * 2..index * 2 + 2], &input[source * 2..source * 2 + 2], "frame {index}");
        }
    }

    #[test]
    fn a_small_ratio_changes_the_length_and_keeps_the_tone() {
        let rate = 48_000.0;
        let input = sine_frames(48_000, 1000.0, rate);
        for &(ppm, label) in &[(300i64, "input faster"), (-300, "input slower")] {
            let mut resampler = Resampler::new(1);
            resampler.step = (65_536 + ppm * 65_536 / 1_000_000) as u32;
            let mut output = Vec::new();
            let mut frame = [0i32; 1];
            for &sample in &input {
                resampler.push(&[sample]);
                while resampler.next_output(&mut frame) {
                    output.push(frame[0]);
                }
            }
            // Output count = input count / step (within a frame or two).
            let expected = (input.len() as f64 / (1.0 + ppm as f64 * 1e-6)) as i64;
            assert!((output.len() as i64 - expected).abs() <= 4, "{label}: {} vs {expected}", output.len());
            // The output is the 1 kHz sine sampled at a slightly different rate: compare against the ideal.
            let mut worst = 0.0f64;
            for (index, &value) in output.iter().enumerate().skip(8).take(40_000) {
                let position = index as f64 * (f64::from(resampler.step) / 65_536.0) + 1.0; // input time of output sample `index` (the step as actually quantised)
                let ideal = (position * 1000.0 * 2.0 * core::f64::consts::PI / rate).sin() * 0.5 * i32::MAX as f64;
                worst = worst.max((f64::from(value) - ideal).abs());
            }
            // Catmull-Rom on a 1 kHz tone at 48 kHz is accurate to ~ -80 dB of full scale.
            assert!(worst < 0.5 * i32::MAX as f64 * 1e-3, "{label}: worst error {worst}");
        }
    }

    #[test]
    fn the_servo_does_not_wind_up_while_the_output_is_at_its_limit() {
        let mut servo = Servo::new(576);
        // A ring far above the target for a long time: the output is at the limit from the first update on.
        for _ in 0..100_000 {
            servo.update(2000, 48, 48_000);
        }
        assert!(servo.offset_ppm() < 2000, "the integral must not run on while saturated: {} ppm", servo.offset_ppm());
        // When the level comes back below the target the step must fall below unity within a few updates.
        let mut updates = 0;
        while servo.update(300, 48, 48_000) >= UNITY_STEP && updates < 1000 {
            updates += 1;
        }
        assert!(updates < 1000, "the servo took too long to reverse");
    }

    #[test]
    fn the_servo_learns_a_clock_difference() {
        // The sender's clock runs 400 ppm fast: 48,019.2 frames per device second. Each loop iteration is one
        // 1 ms packet of 48 input frames; the device drains the ring at exactly 48,000 per second.
        let mut servo = Servo::new(288);
        let mut resampler = Resampler::new(1);
        let (mut level, mut producer_credit) = (288i64, 0f64);
        let mut frame = [0i32; 1];
        let mut worst_late = 0i64;
        for millisecond in 0..60_000 {
            // 48 frames arrive per ms of *sender* time; the device's ms is 1/(1+400ppm) of that: it drains 47.9808 frames.
            for _ in 0..48 {
                resampler.step = servo.step;
                resampler.push(&[0]);
                while resampler.next_output(&mut frame) {
                    level += 1;
                }
            }
            producer_credit += 47.980_8; // frames drained by the device per sender millisecond
            let drained = producer_credit as i64;
            producer_credit -= drained as f64;
            level -= drained;
            servo.update(level.max(0) as usize, 48, 48_000);
            if millisecond > 50_000 {
                worst_late = worst_late.max((level - 288).abs());
            }
        }
        assert!(worst_late < 20, "the level should settle near the target: off by {worst_late}");
        assert!((servo.offset_ppm() - 400).abs() < 25, "learned {} ppm, wanted about 400", servo.offset_ppm());
    }

    #[test]
    fn working_format_round_trip() {
        let wire = [0x12, 0x34, 0x56, 0xff, 0xff, 0xfe];
        let mut frames = [0i32; 2];
        wire_to_frames(&wire, 2, 24, 1, &mut frames);
        assert_eq!(frames, [0x1234_5600, (0xfffffe00u32) as i32]);
        let mut device = [0u8; 6];
        frames_to_device(&frames, 2, 1, &[Some(0), Some(1)], 3, 2, &mut device);
        assert_eq!(device, [0x56, 0x34, 0x12, 0xfe, 0xff, 0xff]);
    }

    #[test]
    fn opus_sdp_has_a_media_line_per_pair() {
        let sdp = SdpOpus { name: "opus-test", receive_only: false, idle: false, origin: [10, 0, 0, 1], destination: [10, 0, 0, 2], port: 5004, payload_type: 96, channels: 5, packet_ms_x10: 100, session: 3 };
        let mut out = [0u8; 1024];
        let length = write_sdp_opus(&sdp, &mut out);
        let text = core::str::from_utf8(&out[..length]).unwrap();
        assert_eq!(text.matches("m=audio").count(), 3, "5 channels: two stereo streams and a mono one");
        assert!(text.contains("m=audio 5004 RTP/AVP 96") && text.contains("m=audio 5006 RTP/AVP 96") && text.contains("m=audio 5008 RTP/AVP 96"));
        assert!(text.contains("a=rtpmap:96 opus/48000/2"));
        assert!(text.contains("stereo=0;sprop-stereo=0"), "the last, odd channel is a mono stream");
        assert!(text.contains("a=ptime:10\r\n"));
    }

    #[test]
    fn device_sample_to_16_bits() {
        assert_eq!(device_sample_16(&[0x00, 0x34, 0x12]), 0x1234); // 24-bit: the top two bytes
        assert_eq!(device_sample_16(&[0xff, 0x7f]), 0x7fff); // 16-bit slot unchanged
        assert_eq!(device_sample_16(&[0x00, 0x00, 0x80]), -0x8000);
    }

    #[test]
    fn fill_advice() {
        let fill = Fill::new(192);
        assert_eq!(fill.advise(192), Slip::None);
        assert_eq!(fill.advise(250), Slip::None);
        assert_eq!(fill.advise(300), Slip::Drop);
        assert_eq!(fill.advise(60), Slip::Repeat);
    }

    #[test]
    fn avtp_header_round_trip() {
        let header = avtp::Header { sequence: 200, timestamp_valid: true, stream_id: [1, 2, 3, 4, 5, 6, 0, 1], timestamp: 0x1234_5678, channels: 8, bits: 24, rate: 96_000, data_length: 8 * 3 * 4 };
        let mut bytes = [0u8; 24 + 96];
        assert!(header.encode(&mut bytes));
        assert_eq!(&bytes[..4], &[0x02, 0x81, 200, 0]);
        assert_eq!(bytes[16], 3);
        assert_eq!(bytes[17], 0x70); // nsr 7 (96 kHz), channels high bits 0
        assert_eq!(bytes[18], 8);
        assert_eq!(avtp::Header::parse(&bytes), Some(header));
        assert_eq!(avtp::Header::parse(&bytes[..100]), None); // data longer than the frame
    }

    #[test]
    fn avtp_rejects_other_formats() {
        let header = avtp::Header { sequence: 0, timestamp_valid: false, stream_id: [0; 8], timestamp: 0, channels: 2, bits: 16, rate: 48_000, data_length: 4 };
        let mut bytes = [0u8; 28];
        assert!(header.encode(&mut bytes));
        assert!(avtp::Header::parse(&bytes).is_some());
        bytes[0] = 0; // not AAF
        assert_eq!(avtp::Header::parse(&bytes), None);
        assert!(!avtp::Header { rate: 12_345, ..header }.encode(&mut [0u8; 28]));
    }

    #[test]
    fn sap_round_trip_and_session_parse() {
        let sdp = Sdp { name: "studio-a", receive_only: false, idle: false, origin: [192, 168, 1, 20], destination: [239, 69, 1, 1], port: 5004, payload_type: 96, channels: 4, rate: 48_000, bits: 24, packet_us: 250, mac: [0; 6], ptp: None, session: 77 };
        let mut text = [0u8; 600];
        let length = write_sdp(&sdp, &mut text);
        let mut packet = [0u8; 700];
        let total = sap::encode([192, 168, 1, 20], false, 0x1234, &text[..length], &mut packet);
        let parsed = sap::parse(&packet[..total]).unwrap();
        assert_eq!((parsed.origin, parsed.delete, parsed.id), ([192, 168, 1, 20], false, 0x1234));
        let session = sap::parse_sdp(parsed.sdp).unwrap();
        assert_eq!((session.name(), session.session, session.destination, session.port), ("studio-a", 77, [239, 69, 1, 1], 5004));
        assert_eq!((session.codec, session.rate, session.channels, session.packet_us, session.receive_only), (sap::Codec::L24, 48_000, 4, 250, false));
        let total = sap::encode([1, 2, 3, 4], true, 9, &text[..length], &mut packet);
        assert!(sap::parse(&packet[..total]).unwrap().delete);
    }

    #[test]
    fn opus_session_counts_streams_and_receivers_say_so() {
        let sdp = SdpOpus { name: "hall", receive_only: true, idle: false, origin: [10, 0, 0, 5], destination: [10, 0, 0, 5], port: 5004, payload_type: 96, channels: 5, packet_ms_x10: 100, session: 3 };
        let mut text = [0u8; 900];
        let length = write_sdp_opus(&sdp, &mut text);
        let session = sap::parse_sdp(&text[..length]).unwrap();
        assert_eq!((session.codec, session.channels, session.packet_us, session.receive_only), (sap::Codec::Opus, 5, 10_000, true));
        assert_eq!(session.name(), "hall");
    }

    #[test]
    fn sap_rejects_other_versions_and_garbage() {
        assert!(sap::parse(&[0x40, 0, 0, 0, 1, 2, 3, 4, b'v', b'=', b'0']).is_none());
        assert!(sap::parse(&[0x20]).is_none());
        assert!(sap::parse_sdp(b"v=0\r\ns=x\r\n").is_none());
    }

    fn run_sine(amplitude_dbfs: f64, millis: u32, ppm: &mut meter::Ppm, c: &meter::Coefficients) {
        let amplitude = meter::FULL_SCALE as f64 * 10f64.powf(amplitude_dbfs / 20.0);
        for index in 0..48 * millis {
            let sample = (amplitude * (2.0 * core::f64::consts::PI * 1000.0 * f64::from(index) / 48_000.0).sin()).abs();
            ppm.feed(sample as u32, c);
        }
    }

    #[test]
    fn db_of_known_levels() {
        assert_eq!(meter::db_tenths(meter::FULL_SCALE), 0);
        assert_eq!(meter::db_tenths(meter::FULL_SCALE / 2), -60); // half amplitude: -6.0 dB
        assert!((meter::db_tenths(meter::FULL_SCALE / 8) + 181).abs() <= 2); // -18.06 dB
        assert_eq!(meter::db_tenths(0), -1200);
        for tenths in [-100, -450, -900, -1100] {
            let level = (meter::FULL_SCALE as f64 * 10f64.powf(f64::from(tenths) / 200.0)) as u32;
            assert!((meter::db_tenths(level) - tenths).abs() <= 1, "{tenths}");
        }
    }

    #[test]
    fn a_line_up_tone_reads_test_level() {
        // 1 kHz at -18 dBFS is the Nordic TEST level: the meter should read about -18 dBFS (0 on its scale).
        let c = meter::Coefficients::for_rate(48_000);
        let mut ppm = meter::Ppm::new();
        run_sine(-18.0, 300, &mut ppm, &c);
        let reading = meter::db_tenths(ppm.take_max());
        assert!((-195..=-170).contains(&reading), "{reading}");
    }

    #[test]
    fn a_short_burst_reads_almost_its_height_and_falls_back_slowly() {
        let c = meter::Coefficients::for_rate(48_000);
        let mut ppm = meter::Ppm::new();
        run_sine(-6.0, 5, &mut ppm, &c); // a 5 ms burst: within about 3 dB of the peak
        let burst = meter::db_tenths(ppm.take_max());
        assert!((-90..=-55).contains(&burst), "{burst}");
        // Then silence: 20 dB down after about 1.7 s.
        run_sine(-200.0, 1700, &mut ppm, &c);
        let after = meter::db_tenths(ppm.level);
        assert!((burst - after - 200).abs() <= 60, "{burst} -> {after}");
    }

    #[test]
    fn the_rate_changes_the_coefficients_not_the_timing() {
        let c = meter::Coefficients::for_rate(96_000);
        let mut ppm = meter::Ppm::new();
        for _ in 0..96 * 300 {
            ppm.feed(meter::FULL_SCALE / 4, &c);
        }
        assert!((meter::db_tenths(ppm.level) + 120).abs() <= 6); // steady input reads its level (-12 dB)
        for _ in 0..96 * 1700 {
            ppm.feed(0, &c);
        }
        let fallen = -120 - meter::db_tenths(ppm.level);
        assert!((140..=260).contains(&fallen), "{fallen}"); // about 20 dB in 1.7 s at 96 kHz too
    }

    #[test]
    fn layer_2_announcement_parses() {
        let mut text = [0u8; 600];
        let length = write_sdp(&Sdp { name: "rx", receive_only: true, idle: true, origin: [0; 4], destination: [0; 4], port: 0, payload_type: 96, channels: 2, rate: 48_000, bits: 24, packet_us: 1000, mac: [2, 0, 0, 0, 0, 1], ptp: None, session: 7 }, &mut text);
        let mut sdp = text[..length].to_vec();
        sdp.extend_from_slice(b"a=x-fos-l2:avtp\r\n");
        let session = sap::parse_sdp(&sdp).unwrap();
        assert_eq!((session.name(), session.receive_only, session.idle, session.l2, session.channels), ("rx", true, true, 2, 2));
        // An IP announcement has no marker.
        assert_eq!(sap::parse_sdp(&text[..length]).unwrap().l2, 0);
        // The announcement travels as a SAP packet in an Ethernet payload.
        let mut packet = [0u8; 700];
        let size = sap::encode([0; 4], false, 5, &sdp, &mut packet);
        assert_eq!(sap::parse(&packet[..size]).unwrap().sdp, &sdp[..]);
    }
}
