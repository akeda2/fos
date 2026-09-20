//! Host-testable, fixed-buffer pieces of the Telnet tape.
//!
//! The tape accepts either an explicit static IPv4 configuration or DHCP:
//!
//! ```text
//! tape run static LOCAL_IP/PREFIX GATEWAY SERVER_IP SERVER_PORT
//! tape run static LOCAL_IP /PREFIX GATEWAY SERVER_IP SERVER_PORT
//! tape run static LOCAL_IP NETMASK GATEWAY SERVER_IP SERVER_PORT
//! tape run dhcp SERVER_IP SERVER_PORT
//! ```
//!
//! `SERVER_PORT` is decimal and must be 1..=65535.  Use `0.0.0.0` for
//! `GATEWAY` when the server is on the local subnet.  DHCP supplies the local
//! address, netmask, and gateway, while the server address and port are
//! always supplied by the caller.  The client advertises an 80x25 `xterm`
//! terminal and has no DNS or heap allocation.
//!
//! Telnet's byte-oriented protocol is kept separate from TCP.  `TelnetParser`
//! consumes server bytes one at a time, emits application bytes unchanged,
//! and emits only the small negotiation replies this client understands.
//! `encode_application` doubles IAC bytes on the way to the server.  Neither
//! function interprets UTF-8: a multi-byte UTF-8 sequence remains the same
//! sequence of bytes.
#![no_std]

use tape_net::{
    EthernetFrame, Ipv4Address, Ipv4Header, Ipv4Packet, MacAddress, Result, TcpSegment,
};

pub const MAX_FRAME_LEN: usize = 1514;
pub const TCP_SEGMENT_CAPACITY: usize = 1024;
pub const TELNET_IAC: u8 = 255;
pub const TELNET_DONT: u8 = 254;
pub const TELNET_DO: u8 = 253;
pub const TELNET_WONT: u8 = 252;
pub const TELNET_WILL: u8 = 251;
pub const TELNET_SB: u8 = 250;
pub const TELNET_SE: u8 = 240;
pub const TELNET_BINARY: u8 = 0;
pub const TELNET_ECHO: u8 = 1;
pub const TELNET_SUPPRESS_GO_AHEAD: u8 = 3;
pub const TELNET_TERMINAL_TYPE: u8 = 24;
pub const TELNET_NAWS: u8 = 31;
pub const TELNET_TTYPE_IS: u8 = 0;
pub const TELNET_TTYPE_SEND: u8 = 1;
pub const TELNET_WINDOW_WIDTH: u16 = 80;
pub const TELNET_WINDOW_HEIGHT: u16 = 25;
pub const TELNET_TERMINAL_NAME: &[u8] = b"xterm";
const MAX_SUBNEGOTIATION: usize = 32;
pub const MARKER_CONNECTING: &[u8] = b"TELNET CONNECTING\n";
pub const MARKER_CONNECTED: &[u8] = b"TELNET CONNECTED\n";
pub const MARKER_DISCONNECTED: &[u8] = b"TELNET DISCONNECTED\n";
pub const MARKER_ABORTED: &[u8] = b"TELNET ABORTED\n";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TelnetConfig {
    pub local_ip: Ipv4Address,
    pub netmask: Ipv4Address,
    pub gateway: Ipv4Address,
    pub server_ip: Ipv4Address,
    pub server_port: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StartupMode {
    Static(TelnetConfig),
    Dhcp {
        server_ip: Ipv4Address,
        server_port: u16,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StartupError {
    MissingArguments,
    UnknownMode,
    WrongArgumentCount,
    BadAddress,
    BadNetmask,
    BadPort,
}

fn split_words<'a>(input: &'a [u8], words: &mut [Option<&'a [u8]>; 6]) -> usize {
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

/// Parse the documented static or DHCP startup syntax.
pub fn parse_startup(input: &[u8]) -> core::result::Result<StartupMode, StartupError> {
    let mut words = [None; 6];
    let count = split_words(input, &mut words);
    if count == 0 {
        return Err(StartupError::MissingArguments);
    }
    match words[0] {
        Some(b"static") => {
            if !(5..=6).contains(&count) {
                return Err(StartupError::WrongArgumentCount);
            }
            let (local_ip, netmask, next) = if words[1].unwrap().contains(&b'/') {
                let (address, mask) = parse_cidr(words[1].unwrap())?;
                (address, mask, 2)
            } else if count == 6 && words[2].unwrap().starts_with(b"/") {
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
            if count != next + 3 {
                return Err(StartupError::WrongArgumentCount);
            }
            Ok(StartupMode::Static(TelnetConfig {
                local_ip,
                netmask,
                gateway: parse_ipv4(words[next].unwrap()).ok_or(StartupError::BadAddress)?,
                server_ip: parse_ipv4(words[next + 1].unwrap()).ok_or(StartupError::BadAddress)?,
                server_port: parse_port(words[next + 2].unwrap()).ok_or(StartupError::BadPort)?,
            }))
        }
        Some(b"dhcp") => {
            if count != 3 {
                return Err(StartupError::WrongArgumentCount);
            }
            Ok(StartupMode::Dhcp {
                server_ip: parse_ipv4(words[1].unwrap()).ok_or(StartupError::BadAddress)?,
                server_port: parse_port(words[2].unwrap()).ok_or(StartupError::BadPort)?,
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

/// Wrap an already-built TCP segment in IPv4 and Ethernet headers.
pub fn build_tcp_frame(
    output: &mut [u8],
    source_mac: MacAddress,
    destination_mac: MacAddress,
    source_ip: Ipv4Address,
    destination_ip: Ipv4Address,
    segment: &TcpSegment<'_>,
) -> Result<usize> {
    let mut tcp_bytes = [0u8; MAX_FRAME_LEN];
    let tcp_len = segment.encode(&mut tcp_bytes, source_ip, destination_ip)?;
    let packet = Ipv4Packet {
        header: Ipv4Header::new(source_ip, destination_ip, 6, tcp_len),
        payload: &tcp_bytes[..tcp_len],
    };
    let mut ip_bytes = [0u8; MAX_FRAME_LEN];
    let ip_len = packet.encode(&mut ip_bytes)?;
    EthernetFrame::new(destination_mac, source_mac, 0x0800, &ip_bytes[..ip_len]).encode(output)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TelnetEvent {
    None,
    Data(u8),
    Reply {
        verb: u8,
        option: u8,
    },
    Subnegotiation {
        option: u8,
        data: [u8; MAX_SUBNEGOTIATION],
        length: u8,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ParserState {
    Data,
    Iac,
    Option(u8),
    SubnegotiationOption,
    Subnegotiation(u8),
    SubnegotiationIac(u8),
}

/// Incremental Telnet parser.
///
/// This policy accepts the byte-preserving options needed for a useful
/// terminal stream plus TERMINAL-TYPE and NAWS. All other WILL/DO requests
/// are refused. Subnegotiation is retained in a bounded buffer until IAC SE,
/// including when TCP splits the sequence across segments.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TelnetParser {
    state: ParserState,
    subnegotiation: [u8; MAX_SUBNEGOTIATION],
    subnegotiation_length: usize,
}

impl TelnetParser {
    pub const fn new() -> Self {
        Self {
            state: ParserState::Data,
            subnegotiation: [0; MAX_SUBNEGOTIATION],
            subnegotiation_length: 0,
        }
    }

    pub fn feed(&mut self, byte: u8) -> TelnetEvent {
        match self.state {
            ParserState::Data => {
                if byte == TELNET_IAC {
                    self.state = ParserState::Iac;
                    TelnetEvent::None
                } else {
                    TelnetEvent::Data(byte)
                }
            }
            ParserState::Iac => match byte {
                TELNET_IAC => {
                    self.state = ParserState::Data;
                    TelnetEvent::Data(TELNET_IAC)
                }
                TELNET_WILL | TELNET_WONT | TELNET_DO | TELNET_DONT => {
                    self.state = ParserState::Option(byte);
                    TelnetEvent::None
                }
                TELNET_SB => {
                    self.subnegotiation_length = 0;
                    self.state = ParserState::SubnegotiationOption;
                    TelnetEvent::None
                }
                _ => {
                    self.state = ParserState::Data;
                    TelnetEvent::None
                }
            },
            ParserState::Option(verb) => {
                self.state = ParserState::Data;
                match verb {
                    TELNET_WILL => TelnetEvent::Reply {
                        verb: if supports_server_option(byte) {
                            TELNET_DO
                        } else {
                            TELNET_DONT
                        },
                        option: byte,
                    },
                    TELNET_WONT => TelnetEvent::Reply {
                        verb: TELNET_DONT,
                        option: byte,
                    },
                    TELNET_DO => TelnetEvent::Reply {
                        verb: if supports_client_option(byte) {
                            TELNET_WILL
                        } else {
                            TELNET_WONT
                        },
                        option: byte,
                    },
                    TELNET_DONT => TelnetEvent::Reply {
                        verb: TELNET_WONT,
                        option: byte,
                    },
                    _ => TelnetEvent::None,
                }
            }
            ParserState::SubnegotiationOption => {
                self.state = ParserState::Subnegotiation(byte);
                TelnetEvent::None
            }
            ParserState::Subnegotiation(option) => {
                if byte == TELNET_IAC {
                    self.state = ParserState::SubnegotiationIac(option);
                } else {
                    if self.subnegotiation_length < MAX_SUBNEGOTIATION {
                        self.subnegotiation[self.subnegotiation_length] = byte;
                        self.subnegotiation_length += 1;
                    }
                }
                TelnetEvent::None
            }
            ParserState::SubnegotiationIac(option) => match byte {
                TELNET_SE => {
                    self.state = ParserState::Data;
                    let mut data = [0; MAX_SUBNEGOTIATION];
                    data[..self.subnegotiation_length]
                        .copy_from_slice(&self.subnegotiation[..self.subnegotiation_length]);
                    TelnetEvent::Subnegotiation {
                        option,
                        data,
                        length: self.subnegotiation_length as u8,
                    }
                }
                TELNET_IAC => {
                    if self.subnegotiation_length < MAX_SUBNEGOTIATION {
                        self.subnegotiation[self.subnegotiation_length] = TELNET_IAC;
                        self.subnegotiation_length += 1;
                    }
                    self.state = ParserState::Subnegotiation(option);
                    TelnetEvent::None
                }
                _ => {
                    self.state = ParserState::Subnegotiation(option);
                    TelnetEvent::None
                }
            },
        }
    }
}

impl Default for TelnetParser {
    fn default() -> Self {
        Self::new()
    }
}

fn supports_server_option(option: u8) -> bool {
    matches!(
        option,
        TELNET_BINARY | TELNET_ECHO | TELNET_SUPPRESS_GO_AHEAD
    )
}

fn supports_client_option(option: u8) -> bool {
    matches!(
        option,
        TELNET_BINARY | TELNET_SUPPRESS_GO_AHEAD | TELNET_TERMINAL_TYPE | TELNET_NAWS
    )
}

/// Encode application bytes for a Telnet stream.  IAC is doubled so a literal
/// 0xff in a UTF-8-adjacent byte stream cannot be mistaken for a command.
pub fn encode_application(input: &[u8], output: &mut [u8]) -> Option<usize> {
    let mut written = 0;
    for &byte in input {
        let needed = if byte == TELNET_IAC { 2 } else { 1 };
        if output.len().saturating_sub(written) < needed {
            return None;
        }
        output[written] = byte;
        written += 1;
        if byte == TELNET_IAC {
            output[written] = TELNET_IAC;
            written += 1;
        }
    }
    Some(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tape_net::{EthernetFrame, Ipv4Packet};

    #[test]
    fn parses_documented_static_startup_syntax() {
        assert_eq!(
            parse_startup(b"static 192.0.2.10 255.255.255.0 0.0.0.0 192.0.2.20 23"),
            Ok(StartupMode::Static(TelnetConfig {
                local_ip: Ipv4Address::new([192, 0, 2, 10]),
                netmask: Ipv4Address::new([255, 255, 255, 0]),
                gateway: Ipv4Address::UNSPECIFIED,
                server_ip: Ipv4Address::new([192, 0, 2, 20]),
                server_port: 23,
            }))
        );
        let StartupMode::Static(config) =
            parse_startup(b"static 192.0.2.10 255.255.255.0 192.0.2.1 192.0.2.20 65535").unwrap()
        else {
            panic!("expected static startup mode");
        };
        assert_eq!(config.server_port, 65535);
        for syntax in [
            b"static 192.0.2.10/24 192.0.2.1 192.0.2.20 23".as_slice(),
            b"static 192.0.2.10 /24 192.0.2.1 192.0.2.20 23".as_slice(),
        ] {
            let StartupMode::Static(config) = parse_startup(syntax).unwrap() else {
                panic!("expected static startup mode");
            };
            assert_eq!(config.local_ip, Ipv4Address::new([192, 0, 2, 10]));
            assert_eq!(config.netmask, Ipv4Address::new([255, 255, 255, 0]));
        }
        assert_eq!(
            parse_startup(b"dhcp 192.0.2.20 23"),
            Ok(StartupMode::Dhcp {
                server_ip: Ipv4Address::new([192, 0, 2, 20]),
                server_port: 23,
            })
        );
    }

    #[test]
    fn rejects_bad_startup_values() {
        assert_eq!(parse_startup(b""), Err(StartupError::MissingArguments));
        assert_eq!(
            parse_startup(b"dhcp 192.0.2.10"),
            Err(StartupError::WrongArgumentCount)
        );
        assert_eq!(
            parse_startup(b"static 192.0.2.10 255.255.255.0 0.0.0.0 192.0.2.20"),
            Err(StartupError::WrongArgumentCount)
        );
        assert_eq!(
            parse_startup(b"static 192.0.2.999 255.255.255.0 0.0.0.0 192.0.2.20 23"),
            Err(StartupError::BadAddress)
        );
        assert_eq!(
            parse_startup(b"static 192.0.2.10 255.255.255.0 0.0.0.0 192.0.2.20 0"),
            Err(StartupError::BadPort)
        );
        assert_eq!(
            parse_startup(b"static 192.0.2.10/33 0.0.0.0 192.0.2.20 23"),
            Err(StartupError::BadNetmask)
        );
    }

    #[test]
    fn telnet_parser_preserves_utf8_bytes_and_escaped_iac() {
        let mut parser = TelnetParser::new();
        let mut bytes = [0u8; 4];
        let mut count = 0;
        for byte in [0xc3, 0xa9, TELNET_IAC, TELNET_IAC] {
            if let TelnetEvent::Data(value) = parser.feed(byte) {
                bytes[count] = value;
                count += 1;
            }
        }
        assert_eq!(&bytes[..count], &[0xc3, 0xa9, TELNET_IAC]);
    }

    #[test]
    fn telnet_negotiates_supported_options_and_parses_subnegotiation() {
        let mut parser = TelnetParser::new();
        assert_eq!(parser.feed(TELNET_IAC), TelnetEvent::None);
        assert_eq!(parser.feed(TELNET_WILL), TelnetEvent::None);
        assert_eq!(
            parser.feed(TELNET_BINARY),
            TelnetEvent::Reply {
                verb: TELNET_DO,
                option: TELNET_BINARY
            }
        );
        for byte in [TELNET_IAC, TELNET_SB, 24, b'x', TELNET_IAC] {
            assert_eq!(parser.feed(byte), TelnetEvent::None);
        }
        assert_eq!(
            parser.feed(TELNET_SE),
            TelnetEvent::Subnegotiation {
                option: TELNET_TERMINAL_TYPE,
                data: {
                    let mut data = [0; MAX_SUBNEGOTIATION];
                    data[0] = b'x';
                    data
                },
                length: 1,
            }
        );
        assert_eq!(parser.feed(b'Z'), TelnetEvent::Data(b'Z'));
    }

    #[test]
    fn telnet_refuses_unknown_options_and_doubles_iac() {
        let mut parser = TelnetParser::new();
        for byte in [TELNET_IAC, TELNET_DO] {
            assert_eq!(parser.feed(byte), TelnetEvent::None);
        }
        assert_eq!(
            parser.feed(42),
            TelnetEvent::Reply {
                verb: TELNET_WONT,
                option: 42
            }
        );

        let mut output = [0u8; 4];
        assert_eq!(
            encode_application(&[b'a', TELNET_IAC, b'b'], &mut output),
            Some(4)
        );
        assert_eq!(&output, b"a\xff\xffb");
        assert_eq!(encode_application(&[TELNET_IAC], &mut [0u8; 1]), None);
    }

    #[test]
    fn telnet_preserves_tab_application_bytes() {
        let mut output = [0u8; 1];
        assert_eq!(encode_application(b"\t", &mut output), Some(1));
        assert_eq!(output, [b'\t']);
    }

    #[test]
    fn builds_a_checksummed_tcp_frame() {
        let segment = TcpSegment::new(40000, 23, 10, 20, tape_net::TCP_ACK, 512, b"hi");
        let mut frame = [0u8; MAX_FRAME_LEN];
        let length = build_tcp_frame(
            &mut frame,
            MacAddress::new([0, 1, 2, 3, 4, 5]),
            MacAddress::new([6, 7, 8, 9, 10, 11]),
            Ipv4Address::new([192, 0, 2, 10]),
            Ipv4Address::new([192, 0, 2, 20]),
            &segment,
        )
        .unwrap();
        let ethernet = EthernetFrame::parse(&frame[..length]).unwrap();
        let packet = Ipv4Packet::parse(ethernet.payload, true).unwrap();
        let parsed = TcpSegment::parse(
            packet.payload,
            packet.header.source,
            packet.header.destination,
            true,
        )
        .unwrap();
        assert_eq!(parsed.data, b"hi");
        assert_eq!(parsed.destination_port, 23);
    }
}
