//! What the SSH client tapes (`sshmount`, `ssh`) share. Every message is prefixed with the tape's name (`tool`).
//!
//! - `load_seed`: the ed25519 login key, `settings/ssh_key` (64 hex digits; `keygen` makes it).
//! - `Trust`: the server's host key against `settings/known_hosts` (`HOST[:PORT] SHA256:fingerprint`), trust on first use.
//! - `dial`: DHCP on the active NIC, DNS for the host name, a TCP connection: a `Wire`, the `ssh::client::Link` over it.
#![no_std]

use ssh::authorized::{fingerprint_public, parse_seed, seed_text};
use ssh::client::{Error, Link};
use tape_ip::net;
use tape_ip::tcp::Socket;
use tape_net::{ArpCache, MacAddress};
use tape_rt::TapeApi;
use tape_rt::config::{read_named, write_named};

/// How long a send may wait for the server's acknowledgement.
const SEND_MICROS: u64 = 20_000_000;

pub type Tcp = Socket<16384>;

/// A TCP connection as the client's byte stream. `wait_micros` is how long `recv` waits; with `polling` set an empty wait
/// is `Error::Timeout` (an interactive session asks again), otherwise `Error::Link`.
pub struct Wire {
    pub socket: Tcp,
    pub api: &'static TapeApi,
    pub wait_micros: u64,
    pub polling: bool,
}

impl Link for Wire {
    fn send(&mut self, data: &[u8]) -> Result<(), Error> {
        self.socket.send(self.api, data, SEND_MICROS).map_err(|_| Error::Link)
    }

    fn recv(&mut self, out: &mut [u8]) -> Result<usize, Error> {
        match self.socket.recv(self.api, out, self.wait_micros) {
            Ok(count) => Ok(count),
            // Still open: the wait ran out. A closed connection reads as Ok(0) above.
            Err(_) if self.polling && self.socket.is_open() => Err(Error::Timeout),
            Err(_) => Err(Error::Link),
        }
    }

    fn random(&mut self, out: &mut [u8]) {
        if tape_rt::random(out) != Some(true) {
            for chunk in out.chunks_mut(8) {
                let value = tape_rt::micros().to_le_bytes();
                chunk.copy_from_slice(&value[..chunk.len()]);
            }
        }
    }
}

/// The key seed from settings (64 hex digits); `make` creates it when there is none (`keygen`).
pub fn load_seed(tool: &str, api: &TapeApi, file: &str, make: bool) -> Option<[u8; 32]> {
    let mut text = [0u8; 128];
    let length = read_named(api, file, &mut text);
    if length > 0 {
        let seed = core::str::from_utf8(&text[..length as usize]).ok().and_then(parse_seed);
        if seed.is_none() {
            tape_rt::println!("{tool}: settings/{file} is not 64 hex digits.");
        }
        return seed;
    }
    if !make {
        tape_rt::println!("{tool}: no settings/{file}: run `{tool} keygen` first and put its line on the server.");
        return None;
    }
    let mut seed = [0u8; 32];
    if tape_rt::random(&mut seed) != Some(true) {
        tape_rt::println!("{tool}: this kernel has no random numbers for a key (needs tape ABI 33).");
        return None;
    }
    let mut body = [0u8; 65];
    seed_text(&seed, &mut body);
    if write_named(api, file, &body) != 0 {
        tape_rt::println!("{tool}: could not write settings/{file}.");
        return None;
    }
    tape_rt::println!("{tool}: made settings/{file}.");
    Some(seed)
}

/// Checks the server's host key against `settings/known_hosts` for one connection.
pub struct Trust<'a> {
    pub tool: &'a str,
    pub api: &'a TapeApi,
    pub host: &'a str,
    pub port: u16,
    /// Set when the key was refused, so the caller does not also print a login error.
    pub refused: bool,
}

impl Trust<'_> {
    /// Unknown hosts are added, a different key is refused.
    pub fn check(&mut self, public: &[u8; 32]) -> bool {
        let trusted = check_host(self.tool, self.api, self.host, self.port, public);
        self.refused = !trusted;
        trusted
    }
}

fn check_host(tool: &str, api: &TapeApi, host: &str, port: u16, public: &[u8; 32]) -> bool {
    let mut print = [0u8; 50];
    fingerprint_public(public, &mut print);
    let print = core::str::from_utf8(&print).unwrap_or("");
    let mut label = [0u8; 64];
    let mut written = host.len().min(40);
    label[..written].copy_from_slice(&host.as_bytes()[..written]);
    if port != 22 {
        label[written] = b':';
        let mut digits = [0u8; 5];
        let mut at = 5;
        let mut rest = port;
        while rest > 0 {
            at -= 1;
            digits[at] = b'0' + (rest % 10) as u8;
            rest /= 10;
        }
        label[written + 1..written + 1 + 5 - at].copy_from_slice(&digits[at..]);
        written += 1 + 5 - at;
    }
    let label = core::str::from_utf8(&label[..written]).unwrap_or(host);
    let mut file = [0u8; 4096];
    let length = read_named(api, "known_hosts", &mut file[..4000]).max(0) as usize;
    let known = core::str::from_utf8(&file[..length]).unwrap_or("");
    for line in known.lines() {
        let mut words = line.split_ascii_whitespace();
        if words.next() == Some(label) {
            if words.next() == Some(print) {
                return true;
            }
            tape_rt::println!("{tool}: THE KEY OF {label} CHANGED. Refusing to log in.");
            tape_rt::println!("{tool}: it was meant to be {print}; if that is right, remove its line from settings/known_hosts.");
            return false;
        }
    }
    let mut at = length;
    if at > 0 && file[at - 1] != b'\n' {
        file[at] = b'\n';
        at += 1;
    }
    for part in [label.as_bytes(), b" ", print.as_bytes(), b"\n"] {
        file[at..at + part.len()].copy_from_slice(part);
        at += part.len();
    }
    if write_named(api, "known_hosts", &file[..at]) != 0 {
        tape_rt::println!("{tool}: could not write settings/known_hosts; not trusting {label}.");
        return false;
    }
    tape_rt::println!("{tool}: {label} is new: remembered its key {print}.");
    true
}

/// Splits `USER@HOST[:PORT]`; prints what is wrong and returns None.
pub fn parse_target<'a>(tool: &str, target: &'a str) -> Option<(&'a str, &'a str, u16)> {
    let Some((user, rest)) = target.split_once('@') else {
        tape_rt::println!("{tool}: write USER@HOST[:PORT].");
        return None;
    };
    let (host, port) = match rest.split_once(':') {
        Some((host, port)) => match port.parse::<u16>() {
            Ok(port) if port != 0 => (host, port),
            _ => {
                tape_rt::println!("{tool}: the port is a number from 1 to 65535.");
                return None;
            }
        },
        None => (rest, 22),
    };
    if user.is_empty() || host.is_empty() {
        tape_rt::println!("{tool}: write USER@HOST.");
        return None;
    }
    Some((user, host, port))
}

/// DHCP on the active NIC, DNS for `host`, then a TCP connection to `host:port`.
pub fn dial(tool: &str, api: &'static TapeApi, socket: Tcp, host: &str, port: u16, wait_micros: u64) -> Option<Wire> {
    let nic = tape_rt::active_nic();
    if nic >= (api.nic_count)() as usize {
        tape_rt::println!("{tool}: no network interface.");
        return None;
    }
    let mut mac = [0u8; 6];
    (api.nic_mac)(nic, mac.as_mut_ptr());
    let Some((config, _, dns_server)) = net::dhcp_with_servers(api, nic, MacAddress(mac)) else {
        tape_rt::println!("{tool}: no DHCP lease on NIC {}.", nic + 1);
        return None;
    };
    let address = match tape_ip::dns::lookup(api, nic, &config, &mut ArpCache::<8>::new(), dns_server.unwrap_or(config.gateway), tape_net::dns::PORT, host) {
        Ok(address) => address,
        Err(why) => {
            tape_rt::println!("{tool}: {host}: {why}.");
            return None;
        }
    };
    let mut wire = Wire { socket, api, wait_micros, polling: false };
    if let Err(why) = wire.socket.connect(api, nic, &config, address, port, 3_000_000) {
        tape_rt::println!("{tool}: {host}: {why}.");
        return None;
    }
    Some(wire)
}
