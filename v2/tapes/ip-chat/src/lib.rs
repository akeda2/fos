//! Pure, fixed-buffer pieces of the IPv4/UDP chat tape.
//!
//! The executable in `main.rs` owns the polling loop and the kernel callback
//! ABI.  Keeping argument parsing and packet construction here makes those
//! pieces testable on the host without pretending that the host has an
//! RTL8139 or a fos console.
#![no_std]

use tape_net::{
    EthernetFrame, Ipv4Address, Ipv4Header, Ipv4Packet, MacAddress, Result, UdpDatagram,
};

pub const MAX_FRAME_LEN: usize = 1514;
pub const CHAT_PORT: u16 = 4242;
pub const BROADCAST_IP: Ipv4Address = Ipv4Address([255, 255, 255, 255]);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NetworkConfig {
    pub local: Ipv4Address,
    pub netmask: Ipv4Address,
    pub gateway: Ipv4Address,
    pub peer: Option<Ipv4Address>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StartupMode {
    Static {
        local: Ipv4Address,
        netmask: Ipv4Address,
        gateway: Ipv4Address,
        peer: Option<Ipv4Address>,
    },
    Dhcp {
        peer: Option<Ipv4Address>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StartupError {
    MissingMode,
    UnknownMode,
    WrongArgumentCount,
    BadAddress,
    BadNetmask,
}

/// Parse a dotted-decimal IPv4 address without allocation or formatting
/// machinery.  Leading zeroes are accepted, but each component must contain
/// at least one decimal digit and fit in one octet.
pub fn parse_ipv4(text: &[u8]) -> Option<Ipv4Address> {
    let mut octets = [0u8; 4];
    let mut component = 0usize;
    let mut value = 0u16;
    let mut digits = 0usize;

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

/// Parses a `/PREFIX` (0-32) CIDR suffix into its equivalent dotted
/// netmask -- mirrors `tapes/audio-rtp`/`tapes/telnet`/`tapes/router`'s
/// own identical, independently-duplicated helper of the same name (no
/// shared `tape-net` parser exists for this; each tape keeps its own
/// small copy, consistent with this project's "small, explainable, no
/// premature shared abstraction" philosophy).
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
            number = number.checked_mul(10)?.checked_add(u32::from(byte - b'0'))?;
        }
        Some(number)
    })?;
    if prefix > 32 {
        return None;
    }
    let mask = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix) };
    Some(Ipv4Address::new(mask.to_be_bytes()))
}

/// Parses a combined `IP/PREFIX` token (both halves in one word) --
/// mirrors `tapes/audio-rtp`'s `parse_cidr` of the same name.
fn parse_cidr(text: &[u8]) -> core::result::Result<(Ipv4Address, Ipv4Address), StartupError> {
    let Some(slash) = text.iter().position(|&byte| byte == b'/') else {
        return Err(StartupError::BadAddress);
    };
    let address = parse_ipv4(&text[..slash]).ok_or(StartupError::BadAddress)?;
    let netmask = parse_prefix_mask(&text[slash..]).ok_or(StartupError::BadNetmask)?;
    Ok((address, netmask))
}

fn split_words<'a>(input: &'a [u8], words: &mut [Option<&'a [u8]>; 5]) -> usize {
    let mut count = 0usize;
    let mut offset = 0usize;
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

/// Parse the documented `tape run static ...` and `tape run dhcp ...` modes.
pub fn parse_startup(input: &[u8]) -> core::result::Result<StartupMode, StartupError> {
    let mut words = [None; 5];
    let count = split_words(input, &mut words);
    if count == 0 {
        return Err(StartupError::MissingMode);
    }

    match words[0].unwrap() {
        // Accepts three forms, matching telnet/audio-rtp/router's own
        // identical convention: a combined `LOCAL_IP/PREFIX GATEWAY
        // [PEER_IP]` (one word for address+prefix), a separate
        // `LOCAL_IP /PREFIX GATEWAY [PEER_IP]` (prefix as its own word),
        // or the legacy dotted `LOCAL_IP NETMASK GATEWAY [PEER_IP]`.
        b"static" => {
            if count < 3 {
                return Err(StartupError::WrongArgumentCount);
            }
            let (local, netmask, next) = if words[1].unwrap().contains(&b'/') {
                let (address, mask) = parse_cidr(words[1].unwrap())?;
                (address, mask, 2)
            } else if words[2].unwrap().starts_with(b"/") {
                (
                    parse_ipv4(words[1].unwrap()).ok_or(StartupError::BadAddress)?,
                    parse_prefix_mask(words[2].unwrap()).ok_or(StartupError::BadNetmask)?,
                    3,
                )
            } else {
                if count < 4 {
                    return Err(StartupError::WrongArgumentCount);
                }
                (
                    parse_ipv4(words[1].unwrap()).ok_or(StartupError::BadAddress)?,
                    parse_ipv4(words[2].unwrap()).ok_or(StartupError::BadNetmask)?,
                    3,
                )
            };
            if count != next + 1 && count != next + 2 {
                return Err(StartupError::WrongArgumentCount);
            }
            let gateway = parse_ipv4(words[next].unwrap()).ok_or(StartupError::BadAddress)?;
            let peer = if count == next + 2 {
                Some(parse_ipv4(words[next + 1].unwrap()).ok_or(StartupError::BadAddress)?)
            } else {
                None
            };
            Ok(StartupMode::Static {
                local,
                netmask,
                gateway,
                peer,
            })
        }
        b"dhcp" if count == 1 || count == 2 => Ok(StartupMode::Dhcp {
            peer: if count == 2 {
                Some(parse_ipv4(words[1].unwrap()).ok_or(StartupError::BadAddress)?)
            } else {
                None
            },
        }),
        b"dhcp" => Err(StartupError::WrongArgumentCount),
        _ => Err(StartupError::UnknownMode),
    }
}

pub fn as_u32(ip: Ipv4Address) -> u32 {
    u32::from_be_bytes(ip.0)
}

pub fn same_subnet(local: Ipv4Address, mask: Ipv4Address, other: Ipv4Address) -> bool {
    as_u32(local) & as_u32(mask) == as_u32(other) & as_u32(mask)
}

pub fn is_subnet_broadcast(local: Ipv4Address, mask: Ipv4Address, address: Ipv4Address) -> bool {
    address == BROADCAST_IP || as_u32(address) == (as_u32(local) & as_u32(mask)) | !as_u32(mask)
}

/// Build one Ethernet/IPv4/UDP frame.  The caller owns the output buffer and
/// the payload remains borrowed only until this function returns.
#[allow(clippy::too_many_arguments)]
pub fn build_udp_frame(
    output: &mut [u8],
    source_mac: MacAddress,
    destination_mac: MacAddress,
    source_ip: Ipv4Address,
    destination_ip: Ipv4Address,
    source_port: u16,
    destination_port: u16,
    payload: &[u8],
    checksum_enabled: bool,
) -> Result<usize> {
    let mut udp_bytes = [0u8; MAX_FRAME_LEN];
    let udp = UdpDatagram::new(source_port, destination_port, payload);
    let udp_len =
        udp.encode_with_checksum(&mut udp_bytes, source_ip, destination_ip, checksum_enabled)?;

    let mut ip_bytes = [0u8; MAX_FRAME_LEN];
    let packet = Ipv4Packet {
        header: Ipv4Header::new(source_ip, destination_ip, 17, udp_len),
        payload: &udp_bytes[..udp_len],
    };
    let ip_len = packet.encode(&mut ip_bytes)?;

    let ethernet = EthernetFrame::new(destination_mac, source_mac, 0x0800, &ip_bytes[..ip_len]);
    ethernet.encode(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_startup_modes_and_addresses() {
        assert_eq!(
            parse_ipv4(b"192.0.2.7"),
            Some(Ipv4Address::new([192, 0, 2, 7]))
        );
        assert_eq!(
            parse_startup(b"static 192.0.2.7 255.255.255.0 192.0.2.1 192.0.2.9"),
            Ok(StartupMode::Static {
                local: Ipv4Address::new([192, 0, 2, 7]),
                netmask: Ipv4Address::new([255, 255, 255, 0]),
                gateway: Ipv4Address::new([192, 0, 2, 1]),
                peer: Some(Ipv4Address::new([192, 0, 2, 9])),
            })
        );
        assert_eq!(parse_startup(b"dhcp"), Ok(StartupMode::Dhcp { peer: None }));
        assert!(parse_ipv4(b"192.0.2.999").is_none());
        assert_eq!(
            parse_startup(b"static 192.0.2.7 255.255.255.0"),
            Err(StartupError::WrongArgumentCount)
        );
    }

    #[test]
    fn parses_cidr_startup_syntax() {
        let expected_no_peer = StartupMode::Static {
            local: Ipv4Address::new([192, 0, 2, 7]),
            netmask: Ipv4Address::new([255, 255, 255, 0]),
            gateway: Ipv4Address::new([192, 0, 2, 1]),
            peer: None,
        };
        // Combined `IP/PREFIX` word.
        assert_eq!(
            parse_startup(b"static 192.0.2.7/24 192.0.2.1"),
            Ok(expected_no_peer)
        );
        // Separate `/PREFIX` word.
        assert_eq!(
            parse_startup(b"static 192.0.2.7 /24 192.0.2.1"),
            Ok(expected_no_peer)
        );
        // Either CIDR form still accepts a trailing peer.
        assert_eq!(
            parse_startup(b"static 192.0.2.7/24 192.0.2.1 192.0.2.9"),
            Ok(StartupMode::Static {
                local: Ipv4Address::new([192, 0, 2, 7]),
                netmask: Ipv4Address::new([255, 255, 255, 0]),
                gateway: Ipv4Address::new([192, 0, 2, 1]),
                peer: Some(Ipv4Address::new([192, 0, 2, 9])),
            })
        );
        assert_eq!(
            parse_startup(b"static 192.0.2.7/33 192.0.2.1"),
            Err(StartupError::BadNetmask)
        );
        assert_eq!(
            parse_startup(b"static 192.0.2.7 /33 192.0.2.1"),
            Err(StartupError::BadNetmask)
        );
    }

    #[test]
    fn builds_checksummed_udp_frame() {
        let mut frame = [0u8; MAX_FRAME_LEN];
        let source_mac = MacAddress::new([0, 1, 2, 3, 4, 5]);
        let destination_mac = MacAddress::new([6, 7, 8, 9, 10, 11]);
        let source_ip = Ipv4Address::new([192, 0, 2, 7]);
        let destination_ip = Ipv4Address::new([192, 0, 2, 9]);
        let length = build_udp_frame(
            &mut frame,
            source_mac,
            destination_mac,
            source_ip,
            destination_ip,
            40000,
            CHAT_PORT,
            b"hello",
            true,
        )
        .unwrap();
        let ethernet = EthernetFrame::parse(&frame[..length]).unwrap();
        assert_eq!(ethernet.destination, destination_mac);
        let packet = Ipv4Packet::parse(ethernet.payload, true).unwrap();
        let datagram = UdpDatagram::parse(packet.payload, true, source_ip, destination_ip).unwrap();
        assert_eq!(datagram.destination_port, CHAT_PORT);
        assert_eq!(datagram.payload, b"hello");
    }

    #[test]
    fn rejects_too_small_output() {
        let mut frame = [0u8; 20];
        assert_eq!(
            build_udp_frame(
                &mut frame,
                MacAddress::ZERO,
                MacAddress::BROADCAST,
                Ipv4Address::UNSPECIFIED,
                BROADCAST_IP,
                68,
                67,
                b"dhcp",
                false,
            ),
            Err(tape_net::Error::BufferTooSmall)
        );
    }

    #[test]
    fn builds_dhcp_bootstrap_with_zero_udp_checksum() {
        let mut frame = [0u8; MAX_FRAME_LEN];
        let length = build_udp_frame(
            &mut frame,
            MacAddress::new([0, 1, 2, 3, 4, 5]),
            MacAddress::BROADCAST,
            Ipv4Address::UNSPECIFIED,
            BROADCAST_IP,
            68,
            67,
            b"bootstrap",
            false,
        )
        .unwrap();
        let ethernet = EthernetFrame::parse(&frame[..length]).unwrap();
        let packet = Ipv4Packet::parse(ethernet.payload, true).unwrap();
        let datagram = UdpDatagram::parse(
            packet.payload,
            false,
            packet.header.source,
            packet.header.destination,
        )
        .unwrap();
        assert_eq!(datagram.checksum, 0);
        assert_eq!(datagram.payload, b"bootstrap");
    }
}
