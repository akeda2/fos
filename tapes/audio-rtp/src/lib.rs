//! Host-testable, fixed-buffer pieces of the RTP/UDP real-time audio tape.
//!
//! The executable in `main.rs` owns the polling loop and the kernel
//! callback ABI; this crate keeps argument parsing, the payload-type
//! table, and frame construction testable on the host, mirroring
//! `ip-chat`/`telnet`'s own split.
//!
//! Startup forms (network configuration always first, mirroring
//! `telnet`'s convention -- unlike raw-Ethernet `audio-net`, which has no
//! network-config step at all):
//!
//! ```text
//! tape run static LOCAL_IP/PREFIX GATEWAY [PORT]
//! tape run static LOCAL_IP/PREFIX GATEWAY send NAME PEER_IP [PEER_PORT]
//! tape run static LOCAL_IP/PREFIX GATEWAY capture PEER_IP [PEER_PORT]
//! tape run dhcp [PORT]
//! tape run dhcp send NAME PEER_IP [PEER_PORT]
//! tape run dhcp capture PEER_IP [PEER_PORT]
//! ```
//!
//! `PORT`/`PEER_PORT` default to `5004`. `static` also accepts the
//! legacy dotted-netmask and separate-`/PREFIX` forms `telnet` already
//! does.
#![no_std]

use tape_net::{
    EthernetFrame, Ipv4Address, Ipv4Header, Ipv4Packet, MacAddress, Result, RtpPacket, UdpDatagram,
};

pub const MAX_FRAME_LEN: usize = 1514;

/// PCM payload bytes per RTP packet. This kernel's network stack has no
/// IP fragmentation anywhere (`kernel/src/nic.rs`'s `MAX_FRAME_LEN` is
/// the hard ceiling on one Ethernet frame, and nothing reassembles a
/// fragmented IPv4 datagram), so the real ceiling is `1514 - 14 (eth) -
/// 20 (ip) - 8 (udp) - 12 (rtp) = 1460` bytes, ever, per packet. `1440`
/// is a clean divisor (360 stereo/16-bit frames, or 180 stereo/24-bit-
/// in-4-byte-container frames) comfortably under that ceiling -- not the
/// 20 ms/3-ish-KB real-world convention, which would silently need
/// fragmentation this kernel cannot do.
pub const RTP_PAYLOAD_BYTES: usize = 1440;

/// Not an IANA-registered default, just a common convention several
/// real-world RTP tools (`ffmpeg` among them) already use unprompted.
pub const DEFAULT_PORT: u16 = 5004;

/// `RtpPacket::payload_type` values this tape actually uses -- one real
/// RFC 3551 static match, and two documented, unnegotiated dynamic-range
/// conventions (no SDP exists anywhere in this project). See
/// `payload_type_for`.
pub const PAYLOAD_TYPE_L16_44100_STEREO: u8 = 10;
pub const PAYLOAD_TYPE_L16_48000_STEREO: u8 = 96;
pub const PAYLOAD_TYPE_L24_48000_STEREO: u8 = 97;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub channels: u8,
    pub bits_per_sample: u8,
}

/// Looks up this tape's payload type for a local device's actual queried
/// format (`audio_sample_rate_at`/`audio_channels_and_bits_at`). Returns
/// `None` for any format this tape does not have a documented payload
/// type for -- the same "don't build untestable generality" rejection
/// every other format check in this codebase uses, rather than
/// generalizing to an arbitrary rate/bit-depth/channel-count.
pub fn payload_type_for(format: AudioFormat) -> Option<u8> {
    match (format.sample_rate, format.bits_per_sample, format.channels) {
        (44_100, 16, 2) => Some(PAYLOAD_TYPE_L16_44100_STEREO),
        (48_000, 16, 2) => Some(PAYLOAD_TYPE_L16_48000_STEREO),
        (48_000, 24, 2) => Some(PAYLOAD_TYPE_L24_48000_STEREO),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetworkMode {
    Static {
        local: Ipv4Address,
        netmask: Ipv4Address,
        gateway: Ipv4Address,
    },
    Dhcp,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StreamMode<'a> {
    Listen {
        port: u16,
    },
    Send {
        name: &'a [u8],
        peer_ip: Ipv4Address,
        peer_port: u16,
    },
    Capture {
        peer_ip: Ipv4Address,
        peer_port: u16,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StartupConfig<'a> {
    pub network: NetworkMode,
    pub stream: StreamMode<'a>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StartupError {
    MissingMode,
    UnknownMode,
    WrongArgumentCount,
    BadAddress,
    BadNetmask,
    BadPort,
    MissingName,
}

fn split_words<'a>(input: &'a [u8], words: &mut [Option<&'a [u8]>; 8]) -> usize {
    let mut count = 0;
    let mut offset = 0;
    while offset < input.len() {
        while offset < input.len() && input[offset].is_ascii_whitespace() {
            offset += 1;
        }
        if offset == input.len() {
            break;
        }
        let start = offset;
        while offset < input.len() && !input[offset].is_ascii_whitespace() {
            offset += 1;
        }
        if count == words.len() {
            return count + 1;
        }
        words[count] = Some(&input[start..offset]);
        count += 1;
    }
    count
}

/// Parse dotted decimal without allocation or `core::fmt`.
pub fn parse_ipv4(text: &[u8]) -> Option<Ipv4Address> {
    let mut octets = [0u8; 4];
    let mut component = 0;
    let mut value = 0u16;
    let mut digits = 0;

    for &byte in text.iter().chain(core::iter::once(&b'.')) {
        match byte {
            b'0'..=b'9' => {
                value = value.checked_mul(10)?.checked_add(u16::from(byte - b'0'))?;
                if value > 255 {
                    return None;
                }
                digits += 1;
            }
            b'.' if digits != 0 && component < 4 => {
                octets[component] = value as u8;
                component += 1;
                value = 0;
                digits = 0;
            }
            _ => return None,
        }
    }

    (component == 4).then_some(Ipv4Address::new(octets))
}

fn parse_port(text: &[u8]) -> Option<u16> {
    if text.is_empty() {
        return None;
    }
    let mut value = 0u32;
    for &byte in text {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add(u32::from(byte - b'0'))?;
        if value > u32::from(u16::MAX) {
            return None;
        }
    }
    (value != 0).then_some(value as u16)
}

fn parse_prefix_mask(text: &[u8]) -> Option<Ipv4Address> {
    let prefix = text.strip_prefix(b"/").and_then(|value| {
        if value.is_empty() {
            return None;
        }
        let mut number = 0u32;
        for &byte in value {
            if !byte.is_ascii_digit() {
                return None;
            }
            number = number
                .checked_mul(10)?
                .checked_add(u32::from(byte - b'0'))?;
        }
        Some(number)
    })?;
    if prefix > 32 {
        return None;
    }
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    Some(Ipv4Address::new(mask.to_be_bytes()))
}

fn parse_cidr(text: &[u8]) -> core::result::Result<(Ipv4Address, Ipv4Address), StartupError> {
    let Some(slash) = text.iter().position(|&byte| byte == b'/') else {
        return Err(StartupError::BadAddress);
    };
    let address = parse_ipv4(&text[..slash]).ok_or(StartupError::BadAddress)?;
    let netmask = parse_prefix_mask(&text[slash..]).ok_or(StartupError::BadNetmask)?;
    Ok((address, netmask))
}

/// Parses the trailing stream-mode words (everything after the network
/// config): nothing or a bare port number means listen mode; `send
/// NAME PEER_IP [PEER_PORT]` or `capture PEER_IP [PEER_PORT]` mean the
/// other two.
fn parse_stream_mode<'a>(
    words: &[Option<&'a [u8]>; 8],
    start: usize,
    count: usize,
) -> core::result::Result<StreamMode<'a>, StartupError> {
    let remaining = count - start;
    match words.get(start).copied().flatten() {
        None => Ok(StreamMode::Listen {
            port: DEFAULT_PORT,
        }),
        Some(b"send") => {
            let name = words.get(start + 1).copied().flatten().ok_or(StartupError::MissingName)?;
            let peer_ip = words
                .get(start + 2)
                .copied()
                .flatten()
                .and_then(parse_ipv4)
                .ok_or(StartupError::BadAddress)?;
            let peer_port = match remaining {
                3 => DEFAULT_PORT,
                4 => words
                    .get(start + 3)
                    .copied()
                    .flatten()
                    .and_then(parse_port)
                    .ok_or(StartupError::BadPort)?,
                _ => return Err(StartupError::WrongArgumentCount),
            };
            Ok(StreamMode::Send {
                name,
                peer_ip,
                peer_port,
            })
        }
        Some(b"capture") => {
            let peer_ip = words
                .get(start + 1)
                .copied()
                .flatten()
                .and_then(parse_ipv4)
                .ok_or(StartupError::BadAddress)?;
            let peer_port = match remaining {
                2 => DEFAULT_PORT,
                3 => words
                    .get(start + 2)
                    .copied()
                    .flatten()
                    .and_then(parse_port)
                    .ok_or(StartupError::BadPort)?,
                _ => return Err(StartupError::WrongArgumentCount),
            };
            Ok(StreamMode::Capture { peer_ip, peer_port })
        }
        Some(word) => {
            if remaining != 1 {
                return Err(StartupError::WrongArgumentCount);
            }
            let port = parse_port(word).ok_or(StartupError::BadPort)?;
            Ok(StreamMode::Listen { port })
        }
    }
}

/// Parse the documented static/dhcp startup syntax.
pub fn parse_startup(input: &[u8]) -> core::result::Result<StartupConfig<'_>, StartupError> {
    let mut words = [None; 8];
    let count = split_words(input, &mut words);
    if count == 0 {
        return Err(StartupError::MissingMode);
    }

    match words[0] {
        Some(b"static") => {
            if count < 3 {
                return Err(StartupError::WrongArgumentCount);
            }
            let (local, netmask, next) = if words[1].unwrap().contains(&b'/') {
                let (address, mask) = parse_cidr(words[1].unwrap())?;
                (address, mask, 2)
            } else if words[2].map(|w| w.starts_with(b"/")).unwrap_or(false) {
                (
                    parse_ipv4(words[1].unwrap()).ok_or(StartupError::BadAddress)?,
                    parse_prefix_mask(words[2].unwrap()).ok_or(StartupError::BadNetmask)?,
                    3,
                )
            } else {
                (
                    parse_ipv4(words[1].unwrap()).ok_or(StartupError::BadAddress)?,
                    parse_ipv4(words[2].unwrap()).ok_or(StartupError::BadNetmask)?,
                    3,
                )
            };
            if count <= next {
                return Err(StartupError::WrongArgumentCount);
            }
            let gateway = parse_ipv4(words[next].unwrap()).ok_or(StartupError::BadAddress)?;
            let stream = parse_stream_mode(&words, next + 1, count)?;
            Ok(StartupConfig {
                network: NetworkMode::Static {
                    local,
                    netmask,
                    gateway,
                },
                stream,
            })
        }
        Some(b"dhcp") => {
            let stream = parse_stream_mode(&words, 1, count)?;
            Ok(StartupConfig {
                network: NetworkMode::Dhcp,
                stream,
            })
        }
        _ => Err(StartupError::UnknownMode),
    }
}

pub fn as_u32(address: Ipv4Address) -> u32 {
    u32::from_be_bytes(address.0)
}

pub fn same_subnet(local: Ipv4Address, netmask: Ipv4Address, other: Ipv4Address) -> bool {
    as_u32(local) & as_u32(netmask) == as_u32(other) & as_u32(netmask)
}

/// Wrap an already-built RTP packet in UDP/IPv4/Ethernet headers. The
/// caller owns the output buffer; the RTP payload remains borrowed only
/// until this function returns.
#[allow(clippy::too_many_arguments)]
pub fn build_rtp_udp_frame(
    output: &mut [u8],
    source_mac: MacAddress,
    destination_mac: MacAddress,
    source_ip: Ipv4Address,
    destination_ip: Ipv4Address,
    source_port: u16,
    destination_port: u16,
    rtp: &RtpPacket<'_>,
) -> Result<usize> {
    let mut rtp_bytes = [0u8; MAX_FRAME_LEN];
    let rtp_len = rtp.encode(&mut rtp_bytes)?;

    let mut udp_bytes = [0u8; MAX_FRAME_LEN];
    let udp = UdpDatagram::new(source_port, destination_port, &rtp_bytes[..rtp_len]);
    let udp_len = udp.encode(&mut udp_bytes, source_ip, destination_ip)?;

    let mut ip_bytes = [0u8; MAX_FRAME_LEN];
    let packet = Ipv4Packet {
        header: Ipv4Header::new(source_ip, destination_ip, 17, udp_len),
        payload: &udp_bytes[..udp_len],
    };
    let ip_len = packet.encode(&mut ip_bytes)?;

    EthernetFrame::new(destination_mac, source_mac, 0x0800, &ip_bytes[..ip_len]).encode(output)
}

/// Derives a pseudo-random-enough 32-bit seed from this NIC's own MAC
/// address and the current tick count -- this kernel has no RNG, and
/// none is needed: there is no RTCP/collision detection to defeat, just
/// a wish that a fresh session's SSRC/initial sequence/initial timestamp
/// not always start at the same value. `ssrc`/`initial_sequence`/
/// `initial_timestamp` each derive a different value from this one seed
/// rather than needing three independent sources.
pub fn derive_stream_seed(own_mac: MacAddress, ticks_now: u64) -> u32 {
    let mac_bits = u32::from_be_bytes([
        own_mac.0[2],
        own_mac.0[3],
        own_mac.0[4],
        own_mac.0[5],
    ]);
    mac_bits ^ (ticks_now as u32) ^ ((ticks_now >> 32) as u32)
}

pub fn ssrc_from_seed(seed: u32) -> u32 {
    seed
}

pub fn initial_sequence_from_seed(seed: u32) -> u16 {
    (seed >> 8) as u16
}

pub fn initial_timestamp_from_seed(seed: u32) -> u32 {
    seed.wrapping_mul(0x9e37_79b1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_type_table_covers_documented_formats_only() {
        assert_eq!(
            payload_type_for(AudioFormat {
                sample_rate: 44_100,
                channels: 2,
                bits_per_sample: 16
            }),
            Some(PAYLOAD_TYPE_L16_44100_STEREO)
        );
        assert_eq!(
            payload_type_for(AudioFormat {
                sample_rate: 48_000,
                channels: 2,
                bits_per_sample: 16
            }),
            Some(PAYLOAD_TYPE_L16_48000_STEREO)
        );
        assert_eq!(
            payload_type_for(AudioFormat {
                sample_rate: 48_000,
                channels: 2,
                bits_per_sample: 24
            }),
            Some(PAYLOAD_TYPE_L24_48000_STEREO)
        );
        assert_eq!(
            payload_type_for(AudioFormat {
                sample_rate: 44_100,
                channels: 1,
                bits_per_sample: 16
            }),
            None
        );
        assert_eq!(
            payload_type_for(AudioFormat {
                sample_rate: 96_000,
                channels: 2,
                bits_per_sample: 16
            }),
            None
        );
    }

    #[test]
    fn parses_listen_startup_syntax() {
        let config = parse_startup(b"static 192.0.2.10/24 192.0.2.1").unwrap();
        assert_eq!(
            config.network,
            NetworkMode::Static {
                local: Ipv4Address::new([192, 0, 2, 10]),
                netmask: Ipv4Address::new([255, 255, 255, 0]),
                gateway: Ipv4Address::new([192, 0, 2, 1]),
            }
        );
        assert_eq!(
            config.stream,
            StreamMode::Listen {
                port: DEFAULT_PORT
            }
        );

        let config = parse_startup(b"static 192.0.2.10 255.255.255.0 192.0.2.1 6000").unwrap();
        assert_eq!(config.stream, StreamMode::Listen { port: 6000 });

        let config = parse_startup(b"dhcp").unwrap();
        assert_eq!(config.network, NetworkMode::Dhcp);
        assert_eq!(
            config.stream,
            StreamMode::Listen {
                port: DEFAULT_PORT
            }
        );

        let config = parse_startup(b"dhcp 7000").unwrap();
        assert_eq!(config.stream, StreamMode::Listen { port: 7000 });
    }

    #[test]
    fn parses_send_and_capture_startup_syntax() {
        let config = parse_startup(b"static 192.0.2.10/24 192.0.2.1 send tone.pcm 192.0.2.20").unwrap();
        assert_eq!(
            config.stream,
            StreamMode::Send {
                name: b"tone.pcm",
                peer_ip: Ipv4Address::new([192, 0, 2, 20]),
                peer_port: DEFAULT_PORT,
            }
        );

        let config =
            parse_startup(b"static 192.0.2.10/24 192.0.2.1 send tone.pcm 192.0.2.20 6001").unwrap();
        assert_eq!(
            config.stream,
            StreamMode::Send {
                name: b"tone.pcm",
                peer_ip: Ipv4Address::new([192, 0, 2, 20]),
                peer_port: 6001,
            }
        );

        let config = parse_startup(b"dhcp capture 192.0.2.20").unwrap();
        assert_eq!(
            config.stream,
            StreamMode::Capture {
                peer_ip: Ipv4Address::new([192, 0, 2, 20]),
                peer_port: DEFAULT_PORT,
            }
        );

        let config = parse_startup(b"dhcp capture 192.0.2.20 6002").unwrap();
        assert_eq!(
            config.stream,
            StreamMode::Capture {
                peer_ip: Ipv4Address::new([192, 0, 2, 20]),
                peer_port: 6002,
            }
        );
    }

    #[test]
    fn rejects_bad_startup_values() {
        assert_eq!(parse_startup(b""), Err(StartupError::MissingMode));
        assert_eq!(
            parse_startup(b"foo 192.0.2.10/24 192.0.2.1"),
            Err(StartupError::UnknownMode)
        );
        assert_eq!(
            parse_startup(b"static 192.0.2.10/24"),
            Err(StartupError::WrongArgumentCount)
        );
        assert_eq!(
            parse_startup(b"static 192.0.2.999/24 192.0.2.1"),
            Err(StartupError::BadAddress)
        );
        assert_eq!(
            parse_startup(b"static 192.0.2.10/24 192.0.2.1 send"),
            Err(StartupError::MissingName)
        );
        assert_eq!(
            parse_startup(b"static 192.0.2.10/24 192.0.2.1 send tone.pcm"),
            Err(StartupError::BadAddress)
        );
        assert_eq!(
            parse_startup(b"static 192.0.2.10/24 192.0.2.1 capture 192.0.2.20 0"),
            Err(StartupError::BadPort)
        );
        assert_eq!(
            parse_startup(b"static 192.0.2.10/24 192.0.2.1 capture 192.0.2.20 6000 extra"),
            Err(StartupError::WrongArgumentCount)
        );
    }

    #[test]
    fn builds_a_udp_wrapped_rtp_frame() {
        let rtp = RtpPacket::new(96, false, 1, 100, 0x1234_5678, b"pcm-bytes");
        let mut frame = [0u8; MAX_FRAME_LEN];
        let length = build_rtp_udp_frame(
            &mut frame,
            MacAddress::new([0, 1, 2, 3, 4, 5]),
            MacAddress::new([6, 7, 8, 9, 10, 11]),
            Ipv4Address::new([192, 0, 2, 10]),
            Ipv4Address::new([192, 0, 2, 20]),
            5004,
            5004,
            &rtp,
        )
        .unwrap();
        let ethernet = EthernetFrame::parse(&frame[..length]).unwrap();
        let packet = Ipv4Packet::parse(ethernet.payload, true).unwrap();
        let datagram = UdpDatagram::parse(
            packet.payload,
            true,
            packet.header.source,
            packet.header.destination,
        )
        .unwrap();
        let parsed = RtpPacket::parse(datagram.payload).unwrap();
        assert_eq!(parsed.payload, b"pcm-bytes");
        assert_eq!(parsed.ssrc, 0x1234_5678);
    }
}
