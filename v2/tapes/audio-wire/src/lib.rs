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

pub const ETHERTYPE: u16 = 0x88ba;
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

/// Reads one sample from a device-format slot (little-endian, signed, left-justified in its bytes) as a
/// 32-bit value with the sample in the top bits.
fn device_sample(slot: &[u8]) -> i32 {
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

/// Everything an SDP description of one RTP audio stream needs.
pub struct Sdp {
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
/// length. Lines end in CRLF as SDP requires. The `ts-refclk` is the sender's MAC (`localmac`), since this
/// stack has no PTP: a receiver that insists on a PTP reference will not accept the stream, one that only
/// needs the packet format will (ffplay, GStreamer, VLC with the right flags).
pub fn write_sdp(sdp: &Sdp, out: &mut [u8]) -> usize {
    use core::fmt::Write;
    let mut cursor = Cursor { bytes: out, length: 0 };
    let a = sdp.origin;
    let d = sdp.destination;
    let milliseconds = sdp.packet_us / 1000;
    let thousandths = sdp.packet_us % 1000;
    let _ = write!(cursor, "v=0\r\no=- {0} {0} IN IP4 {1}.{2}.{3}.{4}\r\ns=fos audio-net\r\n", sdp.session, a[0], a[1], a[2], a[3]);
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
    let _ = write!(cursor, "a=ts-refclk:localmac={:02X}-{:02X}-{:02X}-{:02X}-{:02X}-{:02X}\r\na=mediaclk:direct=0\r\na=sendonly\r\n", m[0], m[1], m[2], m[3], m[4], m[5]);
    cursor.length
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
        let sdp = Sdp { origin: [192, 168, 1, 10], destination: [239, 69, 1, 1], port: 5004, payload_type: 96, channels: 2, rate: 48_000, bits: 24, packet_us: 1000, mac: [0x18, 0x66, 0xda, 0x25, 0x1f, 0x73], session: 7 };
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
    fn sdp_packet_times_and_unicast() {
        let mut sdp = Sdp { origin: [10, 0, 0, 1], destination: [10, 0, 0, 2], port: 6000, payload_type: 97, channels: 22, rate: 96_000, bits: 16, packet_us: 125, mac: [0; 6], session: 1 };
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

    #[test]
    fn fill_advice() {
        let fill = Fill::new(192);
        assert_eq!(fill.advise(192), Slip::None);
        assert_eq!(fill.advise(250), Slip::None);
        assert_eq!(fill.advise(300), Slip::Drop);
        assert_eq!(fill.advise(60), Slip::Repeat);
    }
}
