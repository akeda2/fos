//! The parts of `fterm` that need no hardware: the command line and the Ctrl-A command keys. Host-tested.
//!
//! Every binding is a plain key or Ctrl+letter, so it works the same on a Nordic keyboard (no AltGr combinations).
#![no_std]

pub mod form;

use tape_rt::{SERIAL_PARITY_EVEN, SERIAL_PARITY_ODD, SERIAL_STOP2};

pub const DEFAULT_BAUD: u32 = 115_200;
pub const DEFAULT_FORMAT: u32 = 0x03;
/// Ctrl-A: the prefix of every fterm command, as in minicom and screen.
pub const PREFIX: u8 = 0x01;

/// What `fterm` talks to.
#[derive(Debug, PartialEq, Eq)]
pub enum Target<'a> {
    /// A serial port; `Config::port`, `baud` and `format` apply.
    Serial,
    Telnet(TelnetTarget),
    /// Another fos machine's shell over `term net`.
    Net(Peer<'a>),
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct Fixed {
    pub local: [u8; 4],
    pub prefix: u8,
    pub gateway: [u8; 4],
}

#[derive(Debug, PartialEq, Eq)]
pub struct TelnetTarget {
    pub server: [u8; 4],
    pub port: u16,
    /// `None`: ask DHCP.
    pub fixed: Option<Fixed>,
}

/// A `term net` peer: a MAC address, or the name it announces (resolved through the kernel's peer table).
#[derive(Debug, PartialEq, Eq)]
pub enum Peer<'a> {
    Mac([u8; 6]),
    Name(&'a str),
}

#[derive(Debug, PartialEq, Eq)]
pub struct Config<'a> {
    pub target: Target<'a>,
    /// 1-based, as `serial_info` numbers them.
    pub port: usize,
    pub baud: u32,
    pub format: u32,
    pub echo: bool,
    /// Send LF after every CR.
    pub crlf: bool,
    pub list: bool,
}

impl Default for Config<'_> {
    fn default() -> Self {
        Config { target: Target::Serial, port: 1, baud: DEFAULT_BAUD, format: DEFAULT_FORMAT, echo: false, crlf: false, list: false }
    }
}

pub fn parse_ip(text: &str) -> Option<[u8; 4]> {
    let mut out = [0u8; 4];
    let mut parts = text.split('.');
    for slot in &mut out {
        *slot = parts.next()?.parse().ok()?;
    }
    parts.next().is_none().then_some(out)
}

pub fn parse_mac(text: &str) -> Option<[u8; 6]> {
    let mut out = [0u8; 6];
    let mut parts = text.split(':');
    for slot in &mut out {
        let part = parts.next()?;
        if part.is_empty() || part.len() > 2 {
            return None;
        }
        *slot = u8::from_str_radix(part, 16).ok()?;
    }
    parts.next().is_none().then_some(out)
}

/// `8n1`, `7e1`, `8o2`: data bits 5-8, parity n/e/o, stop bits 1-2 (1.5 is not offered).
pub fn parse_format(text: &str) -> Option<u32> {
    let bytes = text.as_bytes();
    if bytes.len() != 3 {
        return None;
    }
    let data = match bytes[0] {
        b'5'..=b'8' => u32::from(bytes[0] - b'5'),
        _ => return None,
    };
    let parity = match bytes[1].to_ascii_lowercase() {
        b'n' => 0,
        b'o' => SERIAL_PARITY_ODD,
        b'e' => SERIAL_PARITY_EVEN,
        _ => return None,
    };
    let stop = match bytes[2] {
        b'1' => 0,
        b'2' => SERIAL_STOP2,
        _ => return None,
    };
    Some(data | parity | stop)
}

pub fn format_name(format: u32, out: &mut [u8; 3]) {
    out[0] = b'5' + (format & 3) as u8;
    out[1] = match format & 0x18 {
        0x08 => b'o',
        0x18 => b'e',
        _ => b'n',
    };
    out[2] = if format & 0x04 != 0 { b'2' } else { b'1' };
}

/// `[PORT] [BAUD] [FORMAT] [echo] [crlf]`, `list`, `telnet IP [PORT] [static LOCAL/PREFIX GATEWAY]` or `net NAME|MAC`. In the serial
/// form the first number is the port, the second the baud rate; `echo` and `crlf` may come anywhere.
pub fn parse_args(text: &str) -> Result<Config<'_>, &'static str> {
    let mut config = Config::default();
    let mut words = text.split_ascii_whitespace().peekable();
    match words.peek().copied() {
        Some("telnet") => {
            words.next();
            let server = parse_ip(words.next().ok_or("telnet needs a server address")?).ok_or("bad server address")?;
            let mut target = TelnetTarget { server, port: 23, fixed: None };
            if let Some(port) = words.peek().and_then(|word| word.parse::<u16>().ok()) {
                words.next();
                if port == 0 {
                    return Err("bad TCP port");
                }
                target.port = port;
            }
            config.target = Target::Telnet(target);
        }
        Some("net") => {
            words.next();
            let who = words.next().ok_or("net needs a name or a MAC address")?;
            config.target = Target::Net(match parse_mac(who) {
                Some(mac) => Peer::Mac(mac),
                None => Peer::Name(who),
            });
        }
        _ => {}
    }
    let mut numbers = 0;
    while let Some(word) = words.next() {
        match word {
            "list" if config.target == Target::Serial => config.list = true,
            "echo" => config.echo = true,
            "crlf" => config.crlf = true,
            "static" if matches!(config.target, Target::Telnet(_)) => {
                let local = words.next().ok_or("static needs LOCAL/PREFIX and GATEWAY")?;
                let (address, prefix) = local.split_once('/').ok_or("write the address as LOCAL/PREFIX")?;
                let prefix: u8 = prefix.parse().map_err(|_| "bad prefix")?;
                if prefix > 32 {
                    return Err("bad prefix");
                }
                let local = parse_ip(address).ok_or("bad local address")?;
                let gateway = parse_ip(words.next().ok_or("static needs a gateway (0.0.0.0 for none)")?).ok_or("bad gateway")?;
                if let Target::Telnet(target) = &mut config.target {
                    target.fixed = Some(Fixed { local, prefix, gateway });
                }
            }
            _ if config.target != Target::Serial => return Err("unknown argument"),
            _ => {
                if let Some(format) = parse_format(word) {
                    config.format = format;
                } else if let Ok(number) = word.parse::<u32>() {
                    match numbers {
                        0 => config.port = number as usize,
                        1 => config.baud = number,
                        _ => return Err("too many numbers"),
                    }
                    numbers += 1;
                } else {
                    return Err("unknown argument");
                }
            }
        }
    }
    if config.port == 0 {
        return Err("port numbers start at 1");
    }
    if config.baud == 0 || config.baud > 115_200 {
        return Err("baud must be 1-115200");
    }
    Ok(config)
}

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Nothing,
    /// A byte for the remote end.
    Send(u8),
    Exit,
    Help,
    Status,
    Break,
    ToggleDtr,
    ToggleRts,
    ToggleEcho,
    ToggleCrlf,
    Clear,
    Unknown,
}

/// Turns `read_key` values into actions. Enter is sent as CR (the local keyboard gives LF, a `term net` client CR; a
/// remote terminal expects CR); the Escape key, which the kernel reports as an abort request, is sent as ESC so that it
/// reaches vi and friends. The tape is left with `Ctrl-A x`.
#[derive(Default)]
pub struct Keys {
    prefix: bool,
}

impl Keys {
    pub const fn new() -> Self {
        Keys { prefix: false }
    }

    pub fn feed(&mut self, key: u32) -> Action {
        let byte = match key {
            tape_rt::KEY_NONE => return Action::Nothing,
            tape_rt::KEY_ABORT => 0x1b,
            _ => (key - tape_rt::KEY_BASE) as u8,
        };
        if self.prefix {
            self.prefix = false;
            return match byte.to_ascii_lowercase() {
                b'x' | 0x18 => Action::Exit, // x or Ctrl-X
                b'h' => Action::Help,
                b's' => Action::Status,
                b'b' => Action::Break,
                b'd' => Action::ToggleDtr,
                b'r' => Action::ToggleRts,
                b'e' => Action::ToggleEcho,
                b'l' => Action::ToggleCrlf,
                b'c' => Action::Clear,
                b'a' | PREFIX => Action::Send(PREFIX),
                0x1b => Action::Send(0x1b),
                _ => Action::Unknown,
            };
        }
        match byte {
            PREFIX => {
                self.prefix = true;
                Action::Nothing
            }
            b'\n' => Action::Send(b'\r'),
            _ => Action::Send(byte),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(byte: u8) -> u32 {
        tape_rt::KEY_BASE + u32::from(byte)
    }

    #[test]
    fn defaults() {
        assert_eq!(parse_args("").unwrap(), Config::default());
    }

    #[test]
    fn full_command_line() {
        let config = parse_args("2 9600 7e2 echo crlf").unwrap();
        assert_eq!((config.port, config.baud, config.echo, config.crlf), (2, 9600, true, true));
        assert_eq!(config.format, 0x02 | SERIAL_PARITY_EVEN | SERIAL_STOP2);
    }

    #[test]
    fn rejects_nonsense() {
        assert!(parse_args("0").is_err());
        assert!(parse_args("1 0").is_err());
        assert!(parse_args("1 230400").is_err());
        assert!(parse_args("1 9600 5").is_err());
        assert!(parse_args("banana").is_err());
        assert!(parse_args("9n1x").is_err());
    }

    #[test]
    fn telnet_and_net_targets() {
        let config = parse_args("telnet 10.0.0.5").unwrap();
        assert_eq!(config.target, Target::Telnet(TelnetTarget { server: [10, 0, 0, 5], port: 23, fixed: None }));
        let config = parse_args("telnet 10.0.0.5 2323 static 10.0.0.9/24 10.0.0.1 echo").unwrap();
        let fixed = Fixed { local: [10, 0, 0, 9], prefix: 24, gateway: [10, 0, 0, 1] };
        assert_eq!(config.target, Target::Telnet(TelnetTarget { server: [10, 0, 0, 5], port: 2323, fixed: Some(fixed) }));
        assert!(config.echo);
        assert_eq!(parse_args("net aa:bb:cc:0:1:2").unwrap().target, Target::Net(Peer::Mac([0xaa, 0xbb, 0xcc, 0, 1, 2])));
        assert_eq!(parse_args("net skylake3 crlf").unwrap().target, Target::Net(Peer::Name("skylake3")));
        for bad in ["telnet", "telnet 1.2.3", "telnet 1.2.3.4 0", "telnet 1.2.3.4 static 1.2.3.5 1.2.3.1", "net", "net x 5"] {
            assert!(parse_args(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn format_round_trip() {
        for text in ["8n1", "7e1", "8o2", "5n2"] {
            let mut out = [0; 3];
            format_name(parse_format(text).unwrap(), &mut out);
            assert_eq!(&out, text.as_bytes());
        }
    }

    #[test]
    fn plain_keys_pass_through() {
        let mut keys = Keys::new();
        assert_eq!(keys.feed(key(b'a')), Action::Send(b'a'));
        assert_eq!(keys.feed(key(3)), Action::Send(3)); // Ctrl-C reaches the remote end
        assert_eq!(keys.feed(key(b'\n')), Action::Send(b'\r'));
        assert_eq!(keys.feed(tape_rt::KEY_ABORT), Action::Send(0x1b));
        assert_eq!(keys.feed(tape_rt::KEY_NONE), Action::Nothing);
    }

    #[test]
    fn prefix_commands() {
        let mut keys = Keys::new();
        assert_eq!(keys.feed(key(PREFIX)), Action::Nothing);
        assert_eq!(keys.feed(key(b'x')), Action::Exit);
        keys.feed(key(PREFIX));
        assert_eq!(keys.feed(key(b'B')), Action::Break);
        keys.feed(key(PREFIX));
        assert_eq!(keys.feed(key(PREFIX)), Action::Send(PREFIX));
        keys.feed(key(PREFIX));
        assert_eq!(keys.feed(key(b'q')), Action::Unknown);
        assert_eq!(keys.feed(key(b'x')), Action::Send(b'x')); // the prefix is spent
    }
}
