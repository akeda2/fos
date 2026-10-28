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
    Telnet(TelnetTarget<'a>),
    /// Another fos machine's shell over `term net`.
    Net(Peer<'a>),
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct Fixed {
    pub local: [u8; 4],
    pub prefix: u8,
    pub gateway: [u8; 4],
}

/// The Telnet server: an address, or a host name the DNS client resolves first.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Server<'a> {
    Ip([u8; 4]),
    Name(&'a str),
}

/// A DNS server given on the command line (`dns A.B.C.D[:PORT]`; the port is for test servers, 53 otherwise).
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct DnsServer {
    pub address: [u8; 4],
    pub port: u16,
}

#[derive(Debug, PartialEq, Eq)]
pub struct TelnetTarget<'a> {
    pub server: Server<'a>,
    pub port: u16,
    /// `None`: ask DHCP.
    pub fixed: Option<Fixed>,
    /// `None`: the one DHCP names (option 6), else the gateway.
    pub dns: Option<DnsServer>,
}

/// A host name for `telnet`: valid DNS syntax and at least one letter (so `1.2.3` is a bad address, not a name).
fn parse_server(word: &str) -> Result<Server<'_>, &'static str> {
    match parse_ip(word) {
        Some(address) => Ok(Server::Ip(address)),
        None if tape_net::dns::valid_name(word) && word.bytes().any(|byte| byte.is_ascii_alphabetic()) => Ok(Server::Name(word)),
        None => Err("bad server address"),
    }
}

fn parse_dns(word: &str) -> Result<DnsServer, &'static str> {
    let (address, port) = match word.split_once(':') {
        Some((address, port)) => (address, port.parse::<u16>().ok().filter(|&port| port != 0).ok_or("bad DNS port")?),
        None => (word, tape_net::dns::PORT),
    };
    Ok(DnsServer { address: parse_ip(address).ok_or("bad DNS server address")?, port })
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
    /// The command prefix byte: Ctrl-A, or the Ctrl letter `prefix=LETTER` names (for a session inside a session).
    pub prefix: u8,
}

impl Default for Config<'_> {
    fn default() -> Self {
        Config { target: Target::Serial, port: 1, baud: DEFAULT_BAUD, format: DEFAULT_FORMAT, echo: false, crlf: false, list: false, prefix: PREFIX }
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

/// `prefix=b` (or `prefix=B`): Ctrl-B is the command prefix. Not c, h, i, j, m (Ctrl-C, backspace, tab, LF, CR), which a
/// terminal needs for itself.
pub fn parse_prefix(text: &str) -> Result<u8, &'static str> {
    match text.as_bytes() {
        [letter] if letter.is_ascii_alphabetic() && !b"chijmCHIJM".contains(letter) => Ok(letter.to_ascii_lowercase() - b'a' + 1),
        _ => Err("prefix= takes one letter (not c, h, i, j or m): Ctrl-LETTER starts the commands"),
    }
}

/// `[PORT] [BAUD] [FORMAT] [echo] [crlf] [prefix=LETTER]`, `list`, `telnet IP|NAME [PORT] [static LOCAL/PREFIX GATEWAY] [dns IP[:PORT]]` or `net NAME|MAC`. In the serial
/// form the first number is the port, the second the baud rate; `echo` and `crlf` may come anywhere.
pub fn parse_args(text: &str) -> Result<Config<'_>, &'static str> {
    let mut config = Config::default();
    let mut words = text.split_ascii_whitespace().peekable();
    match words.peek().copied() {
        Some("telnet") => {
            words.next();
            let server = parse_server(words.next().ok_or("telnet needs a server address")?)?;
            let mut target = TelnetTarget { server, port: 23, fixed: None, dns: None };
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
            _ if word.starts_with("prefix=") => config.prefix = parse_prefix(&word[7..])?,
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
            "dns" if matches!(config.target, Target::Telnet(_)) => {
                let server = parse_dns(words.next().ok_or("dns needs a server address")?)?;
                if let Target::Telnet(target) = &mut config.target {
                    target.dns = Some(server);
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
pub struct Keys {
    prefix: bool,
    /// The byte that starts a command (Ctrl-A unless `prefix=` says otherwise).
    lead: u8,
}

impl Default for Keys {
    fn default() -> Self {
        Keys::new()
    }
}

impl Keys {
    pub const fn new() -> Self {
        Keys::with_prefix(PREFIX)
    }

    pub const fn with_prefix(lead: u8) -> Self {
        Keys { prefix: false, lead }
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
                b'a' => Action::Send(self.lead),
                _ if byte == self.lead => Action::Send(self.lead),
                0x1b => Action::Send(0x1b),
                _ => Action::Unknown,
            };
        }
        match byte {
            _ if byte == self.lead => {
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
        assert_eq!(config.target, Target::Telnet(TelnetTarget { server: Server::Ip([10, 0, 0, 5]), port: 23, fixed: None, dns: None }));
        let config = parse_args("telnet 10.0.0.5 2323 static 10.0.0.9/24 10.0.0.1 echo").unwrap();
        let fixed = Fixed { local: [10, 0, 0, 9], prefix: 24, gateway: [10, 0, 0, 1] };
        assert_eq!(config.target, Target::Telnet(TelnetTarget { server: Server::Ip([10, 0, 0, 5]), port: 2323, fixed: Some(fixed), dns: None }));
        assert!(config.echo);
        let config = parse_args("telnet Lab.example 2323 dns 10.0.0.1:5353").unwrap();
        let dns = Some(DnsServer { address: [10, 0, 0, 1], port: 5353 });
        assert_eq!(config.target, Target::Telnet(TelnetTarget { server: Server::Name("Lab.example"), port: 2323, fixed: None, dns }));
        assert_eq!(parse_args("telnet lab dns 10.0.0.1").unwrap().target, Target::Telnet(TelnetTarget { server: Server::Name("lab"), port: 23, fixed: None, dns: Some(DnsServer { address: [10, 0, 0, 1], port: 53 }) }));
        for bad in ["telnet bad_name!", "telnet a..b", "telnet lab dns", "telnet lab dns x", "telnet lab dns 10.0.0.1:0", "net x dns 10.0.0.1"] {
            assert!(parse_args(bad).is_err(), "{bad}");
        }
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
    fn another_prefix_leaves_ctrl_a_to_the_remote_end() {
        assert_eq!(parse_prefix("b"), Ok(2));
        assert_eq!(parse_prefix("B"), Ok(2));
        for bad in ["", "c", "h", "i", "j", "m", "1", "bb"] {
            assert!(parse_prefix(bad).is_err(), "{bad}");
        }
        assert_eq!(parse_args("net x prefix=b").unwrap().prefix, 2);
        let mut keys = Keys::with_prefix(2);
        assert_eq!(keys.feed(key(PREFIX)), Action::Send(PREFIX)); // reaches an inner fterm
        assert_eq!(keys.feed(key(b'x')), Action::Send(b'x'));
        assert_eq!(keys.feed(key(2)), Action::Nothing);
        assert_eq!(keys.feed(key(b'x')), Action::Exit);
        keys.feed(key(2));
        assert_eq!(keys.feed(key(2)), Action::Send(2));
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
