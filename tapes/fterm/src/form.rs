//! The settings form behind `fterm`'s panel: plain state and rules, no screen and no kernel calls, so it is host-tested.
//!
//! The form always reduces to an ordinary `fterm` command line (`Form::args`), which `parse_args` then checks; the saved
//! `fterm.cfg` is that same line, so `run fterm @fterm.cfg` repeats what the panel did. `Form::from_args` is the way back.
use crate::{DEFAULT_BAUD, DEFAULT_FORMAT, Peer, Server, Target, format_name, parse_args};
use core::fmt::Write;
use tape_panel::Text;

pub const BAUDS: [u32; 10] = [1200, 2400, 4800, 9600, 14_400, 19_200, 28_800, 38_400, 57_600, 115_200];
/// 8n1, 7e1, 7o1, 8e1, 8o1, 8n2.
pub const FORMATS: [u32; 6] = [0x03, 0x02 | 0x18, 0x02 | 0x08, 0x03 | 0x18, 0x03 | 0x08, 0x03 | 0x04];
pub const TRANSPORTS: [&str; 3] = ["Serial port", "Telnet", "term net (a fos machine)"];
pub const SERIAL: usize = 0;
pub const TELNET: usize = 1;
pub const NET: usize = 2;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Row {
    Transport,
    Port,
    Baud,
    Format,
    Server,
    TcpPort,
    Addressing,
    Local,
    Gateway,
    Dns,
    Peer,
    Echo,
    Crlf,
}

pub const ROWS: [Row; 13] = [
    Row::Transport,
    Row::Port,
    Row::Baud,
    Row::Format,
    Row::Server,
    Row::TcpPort,
    Row::Addressing,
    Row::Local,
    Row::Gateway,
    Row::Dns,
    Row::Peer,
    Row::Echo,
    Row::Crlf,
];

impl Row {
    /// Text rows are typed into; the others cycle.
    pub fn is_text(self) -> bool {
        matches!(self, Row::Server | Row::TcpPort | Row::Local | Row::Gateway | Row::Dns | Row::Peer)
    }

    /// Whether the row applies to the chosen transport (and, for the static addresses, to the addressing mode).
    pub fn applies(self, form: &Form) -> bool {
        match self {
            Row::Transport | Row::Echo | Row::Crlf => true,
            Row::Port | Row::Baud | Row::Format => form.transport == SERIAL,
            Row::Server | Row::TcpPort | Row::Addressing | Row::Dns => form.transport == TELNET,
            Row::Local | Row::Gateway => form.transport == TELNET && !form.dhcp,
            Row::Peer => form.transport == NET,
        }
    }

    /// Whether a typed character belongs in the row's text.
    pub fn accepts(self, byte: u8) -> bool {
        match self {
            Row::TcpPort => byte.is_ascii_digit(),
            Row::Gateway => byte.is_ascii_digit() || byte == b'.',
            Row::Server => byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'),
            Row::Dns => byte.is_ascii_digit() || byte == b'.' || byte == b':',
            Row::Local => byte.is_ascii_digit() || byte == b'.' || byte == b'/',
            Row::Peer => byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'),
            _ => false,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Form {
    pub transport: usize,
    pub port: usize,
    pub baud: u32,
    pub format: u32,
    pub server: Text<40>,
    pub tcp_port: Text<5>,
    pub dhcp: bool,
    pub local: Text<18>,
    pub gateway: Text<15>,
    pub dns: Text<21>,
    pub peer: Text<20>,
    pub echo: bool,
    pub crlf: bool,
}

impl Default for Form {
    fn default() -> Self {
        Form {
            transport: SERIAL,
            port: 1,
            baud: DEFAULT_BAUD,
            format: DEFAULT_FORMAT,
            server: Text::new(),
            tcp_port: Text::from("23"),
            dhcp: true,
            local: Text::new(),
            gateway: Text::new(),
            dns: Text::new(),
            peer: Text::new(),
            echo: false,
            crlf: false,
        }
    }
}

impl Form {
    /// The form for a saved or typed command line (None if it does not parse).
    pub fn from_args(text: &str) -> Option<Form> {
        let config = parse_args(text).ok()?;
        let mut form = Form { port: config.port, baud: config.baud, format: config.format, echo: config.echo, crlf: config.crlf, ..Form::default() };
        match config.target {
            Target::Serial => {}
            Target::Telnet(target) => {
                form.transport = TELNET;
                match target.server {
                    Server::Ip(a) => {
                        let _ = write!(form.server, "{}.{}.{}.{}", a[0], a[1], a[2], a[3]);
                    }
                    Server::Name(name) => form.server = Text::from(name),
                }
                if let Some(dns) = target.dns {
                    let a = dns.address;
                    let _ = write!(form.dns, "{}.{}.{}.{}", a[0], a[1], a[2], a[3]);
                    if dns.port != 53 {
                        let _ = write!(form.dns, ":{}", dns.port);
                    }
                }
                form.tcp_port.clear();
                let _ = write!(form.tcp_port, "{}", target.port);
                if let Some(fixed) = target.fixed {
                    form.dhcp = false;
                    let a = fixed.local;
                    let _ = write!(form.local, "{}.{}.{}.{}/{}", a[0], a[1], a[2], a[3], fixed.prefix);
                    let g = fixed.gateway;
                    let _ = write!(form.gateway, "{}.{}.{}.{}", g[0], g[1], g[2], g[3]);
                }
            }
            Target::Net(peer) => {
                form.transport = NET;
                match peer {
                    Peer::Name(name) => form.peer = Text::from(name),
                    Peer::Mac(mac) => {
                        let _ = write!(form.peer, "{:x}:{:x}:{:x}:{:x}:{:x}:{:x}", mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]);
                    }
                }
            }
        }
        Some(form)
    }

    /// The command line this form stands for (it may still fail `parse_args`: an address left empty, say).
    pub fn args(&self) -> Text<160> {
        let mut out = Text::new();
        match self.transport {
            SERIAL => {
                let mut name = [0u8; 3];
                format_name(self.format, &mut name);
                let _ = write!(out, "{} {} {}", self.port, self.baud, core::str::from_utf8(&name).unwrap_or("8n1"));
            }
            TELNET => {
                let _ = write!(out, "telnet {} {}", self.server.as_str(), self.tcp_port.as_str());
                if !self.dhcp {
                    let gateway = if self.gateway.is_empty() { "0.0.0.0" } else { self.gateway.as_str() };
                    let _ = write!(out, " static {} {}", self.local.as_str(), gateway);
                }
                if !self.dns.is_empty() {
                    let _ = write!(out, " dns {}", self.dns.as_str());
                }
            }
            _ => {
                let _ = write!(out, "net {}", self.peer.as_str());
            }
        }
        if self.echo {
            let _ = write!(out, " echo");
        }
        if self.crlf {
            let _ = write!(out, " crlf");
        }
        out
    }

    /// Why the form cannot connect yet, if it cannot.
    pub fn problem(&self) -> Option<&'static str> {
        parse_args(self.args().as_str()).err()
    }

    /// The next (`step` 1) or previous (-1) row that applies, wrapping.
    pub fn move_row(&self, from: usize, step: isize) -> usize {
        let mut row = from;
        for _ in 0..ROWS.len() {
            row = (row as isize + step).rem_euclid(ROWS.len() as isize) as usize;
            if ROWS[row].applies(self) {
                return row;
            }
        }
        from
    }

    /// Changes a cycling row by `step` (+1 or -1); `ports` is how many serial ports the kernel lists.
    pub fn cycle(&mut self, row: Row, step: isize, ports: usize) {
        match row {
            Row::Transport => self.transport = (self.transport as isize + step).rem_euclid(TRANSPORTS.len() as isize) as usize,
            Row::Port => self.port = 1 + (self.port as isize - 1 + step).rem_euclid(ports.max(1) as isize) as usize,
            Row::Baud => {
                let at = BAUDS.iter().position(|&baud| baud == self.baud).unwrap_or(0);
                self.baud = BAUDS[(at as isize + step).rem_euclid(BAUDS.len() as isize) as usize];
            }
            Row::Format => {
                let at = FORMATS.iter().position(|&format| format == self.format).unwrap_or(0);
                self.format = FORMATS[(at as isize + step).rem_euclid(FORMATS.len() as isize) as usize];
            }
            Row::Addressing => self.dhcp = !self.dhcp,
            Row::Echo => self.echo = !self.echo,
            Row::Crlf => self.crlf = !self.crlf,
            _ => {}
        }
    }

    pub fn text_mut(&mut self, row: Row) -> Option<TextRef<'_>> {
        match row {
            Row::Server => Some(TextRef::Name(&mut self.server)),
            Row::TcpPort => Some(TextRef::Port(&mut self.tcp_port)),
            Row::Local => Some(TextRef::Local(&mut self.local)),
            Row::Gateway => Some(TextRef::Short(&mut self.gateway)),
            Row::Dns => Some(TextRef::Dns(&mut self.dns)),
            Row::Peer => Some(TextRef::Peer(&mut self.peer)),
            _ => None,
        }
    }
}

/// A text row's buffer, whatever its capacity.
pub enum TextRef<'a> {
    Short(&'a mut Text<15>),
    Name(&'a mut Text<40>),
    Dns(&'a mut Text<21>),
    Port(&'a mut Text<5>),
    Local(&'a mut Text<18>),
    Peer(&'a mut Text<20>),
}

impl TextRef<'_> {
    pub fn push(&mut self, byte: u8) {
        match self {
            TextRef::Short(text) => text.push(byte),
            TextRef::Name(text) => text.push(byte),
            TextRef::Dns(text) => text.push(byte),
            TextRef::Port(text) => text.push(byte),
            TextRef::Local(text) => text.push(byte),
            TextRef::Peer(text) => text.push(byte),
        }
    }

    pub fn clear(&mut self) {
        match self {
            TextRef::Short(text) => text.clear(),
            TextRef::Name(text) => text.clear(),
            TextRef::Dns(text) => text.clear(),
            TextRef::Port(text) => text.clear(),
            TextRef::Local(text) => text.clear(),
            TextRef::Peer(text) => text.clear(),
        }
    }

    pub fn pop(&mut self) {
        match self {
            TextRef::Short(text) => text.pop(),
            TextRef::Name(text) => text.pop(),
            TextRef::Dns(text) => text.pop(),
            TextRef::Port(text) => text.pop(),
            TextRef::Local(text) => text.pop(),
            TextRef::Peer(text) => text.pop(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_form_is_the_default_serial_line() {
        assert_eq!(Form::default().args().as_str(), "1 115200 8n1");
        assert_eq!(Form::default().problem(), None);
    }

    #[test]
    fn lines_survive_a_round_trip() {
        for line in [
            "2 9600 7e1 echo",
            "telnet 10.0.0.5 2323 crlf",
            "telnet 10.0.0.5 23 static 10.0.0.9/24 10.0.0.1",
            "telnet lab.example 23 dns 10.0.0.1",
            "telnet lab.example 2323 static 10.0.0.9/24 10.0.0.1 dns 10.0.2.2:5353 echo",
            "net skylake3",
            "net aa:bb:cc:0:1:2 echo crlf",
        ] {
            let form = Form::from_args(line).unwrap();
            assert_eq!(form.args().as_str(), line);
            assert_eq!(form.problem(), None, "{line}");
        }
    }

    #[test]
    fn unfinished_forms_say_why() {
        let mut form = Form::default();
        form.transport = TELNET;
        assert_eq!(form.problem(), Some("bad server address"));
        form.server = Text::from("10.0.0.5");
        assert_eq!(form.problem(), None);
        form.dhcp = false;
        assert!(form.problem().is_some());
        form.local = Text::from("10.0.0.9/24");
        assert_eq!(form.problem(), None);
        assert_eq!(form.args().as_str(), "telnet 10.0.0.5 23 static 10.0.0.9/24 0.0.0.0");
        form.transport = NET;
        assert!(form.problem().is_some());
    }

    #[test]
    fn rows_follow_the_transport() {
        let mut form = Form::default();
        let shown = |form: &Form| ROWS.iter().filter(|row| row.applies(form)).count();
        assert_eq!(shown(&form), 6);
        form.transport = TELNET;
        assert_eq!(shown(&form), 7);
        form.dhcp = false;
        assert_eq!(shown(&form), 9);
        form.transport = NET;
        assert_eq!(shown(&form), 4);
        // Moving skips what is hidden and wraps.
        let mut at = 0;
        let mut visited = [0usize; 4];
        for slot in &mut visited {
            *slot = at;
            at = form.move_row(at, 1);
        }
        assert_eq!(visited, [0, 10, 11, 12]);
        assert_eq!(at, 0);
        assert_eq!(form.move_row(0, -1), 12);
    }

    #[test]
    fn cycling_wraps() {
        let mut form = Form::default();
        form.cycle(Row::Transport, -1, 1);
        assert_eq!(form.transport, NET);
        form.cycle(Row::Port, 1, 3);
        form.cycle(Row::Port, 1, 3);
        form.cycle(Row::Port, 1, 3);
        assert_eq!(form.port, 1);
        form.cycle(Row::Port, -1, 3);
        assert_eq!(form.port, 3);
        form.cycle(Row::Baud, 1, 1);
        assert_eq!(form.baud, BAUDS[0]);
        form.cycle(Row::Format, 1, 1);
        assert_eq!(form.format, FORMATS[1]);
        form.cycle(Row::Format, -1, 1);
        form.cycle(Row::Format, -1, 1);
        assert_eq!(form.format, FORMATS[5]);
    }

    #[test]
    fn typing_is_filtered_per_row() {
        assert!(Row::Server.accepts(b'7') && Row::Server.accepts(b'.') && Row::Server.accepts(b'x') && !Row::Server.accepts(b'/'));
        assert!(Row::Dns.accepts(b':') && !Row::Dns.accepts(b'x'));
        assert!(Row::Local.accepts(b'/') && !Row::TcpPort.accepts(b'.'));
        assert!(Row::Peer.accepts(b'x') && Row::Peer.accepts(b':') && !Row::Peer.accepts(b' '));
        let mut form = Form::default();
        let mut text = form.text_mut(Row::Peer).unwrap();
        text.push(b'a');
        text.push(b'b');
        text.pop();
        assert_eq!(form.peer.as_str(), "a");
    }
}
