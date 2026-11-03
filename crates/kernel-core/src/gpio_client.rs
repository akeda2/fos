//! GPIO over the network, the client side, shared by the embedded kernel and the server kernel (`kernel-mf`): `gpio @HOST ...`
//! asks another machine (a Pi 4) for its pins. The server side (`kernel/src/gpio_remote.rs`) needs the pins and stays in the
//! embedded kernel; the server kernel has no pins and answers a challenge request with `STATUS_NO_GPIO`.
//!
//! Same frames as NETFS (ethertype 0x88b6), tags `TAG_GPIO_REQUEST` 0x41 and `TAG_GPIO_REPLY` 0x42, served where NETFS is (at
//! the prompt and in `term net`, not while a tape runs). Every request is signed with the `synckey` file (the key `tapesync`
//! uses; no file, no answer) and answered only after a challenge, so a recorded frame cannot be replayed:
//!
//! ```text
//! client -> server   41  00                                    ask for a challenge
//! server -> client   42  00 NONCE16                            (42 02: no GPIO pins here)
//! client -> server   41  OP NONCE16 ARGS.. MAC16               MAC = HMAC-SHA256(key, "fosgpio1" OP NONCE ARGS)[..16]
//! server -> client   42  STATUS NONCE16 DATA.. MAC16           MAC = HMAC-SHA256(key, "fosgpio1r" STATUS NONCE DATA)[..16]
//! ```
//!
//! The nonce is single use, belongs to the machine that asked for it and expires after 5 s. A request with a bad MAC is
//! dropped without a word. Integers are little-endian. Ethernet pads short frames with zeros, so every length is fixed by the
//! op (`args_len`, `data_len`), never read from the frame.
//!
//! | OP | what | ARGS | DATA |
//! |----|------|------|------|
//! | 1 | state | - | levels u64, allowed u64, writable u64, 58 function bytes, 58 pull bytes |
//! | 2 | set outputs | mask u64, values u64 | refused u64 |
//! | 3 | function | pin u8, function u8 (0 in, 1 out, 2..7 alt0..5), level u8 | - |
//! | 4 | pull | pin u8, pull u8 (0 none, 1 up, 2 down) | - |
//!
//! STATUS: 0 done, 1 bad request, 2 no GPIO here, 3 writes are not enabled there (`gpio-remote` in `settings`), 4 pin refused.
//! Reading always works with the key; writing also needs `gpio-remote`.

use tape_sync::{NONCE_LEN, equal, hmac_sha256};

pub const TAG_GPIO_REQUEST: u8 = 0x41;
pub const TAG_GPIO_REPLY: u8 = 0x42;

pub const PINS: usize = 58;
pub const FUNC_IN: u8 = 0;
pub const FUNC_OUT: u8 = 1;

pub const MAC_LEN: usize = 16;
pub const OP_STATE: u8 = 1;
pub const OP_SET: u8 = 2;
pub const OP_FUNCTION: u8 = 3;
pub const OP_PULL: u8 = 4;

pub const STATUS_OK: u8 = 0;
pub const STATUS_BAD: u8 = 1;
pub const STATUS_NO_GPIO: u8 = 2;
pub const STATUS_NO_WRITE: u8 = 3;
pub const STATUS_REFUSED: u8 = 4;

pub const REQUEST_LABEL: &[u8] = b"fosgpio1";
pub const REPLY_LABEL: &[u8] = b"fosgpio1r";
pub const KEY_MAX: usize = crate::tapesync_client::KEY_MAX;

/// Everything a listing shows, from this machine or decoded from another's answer.
pub struct Snapshot {
    pub levels: u64,
    pub allowed: u64,
    /// Pins another machine may write (`gpio-remote`), `allowed` or 0.
    pub writable: u64,
    pub function: [u8; PINS],
    pub pull: [u8; PINS],
}

pub fn mac_of(label: &[u8], first: u8, nonce: &[u8], body: &[u8], key: &[u8]) -> [u8; MAC_LEN] {
    let full = hmac_sha256(key, &[label, &[first], nonce, body]);
    full[..MAC_LEN].try_into().unwrap()
}

pub fn args_len(op: u8) -> Option<usize> {
    match op {
        OP_STATE => Some(0),
        OP_SET => Some(16),
        OP_FUNCTION => Some(3),
        OP_PULL => Some(2),
        _ => None,
    }
}

pub fn data_len(op: u8) -> usize {
    match op {
        OP_STATE => 24 + 2 * PINS,
        OP_SET => 8,
        _ => 0,
    }
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

/// What the client needs from its kernel.
pub trait Env {
    fn print(&self, arguments: core::fmt::Arguments);
    /// The `synckey` file's text from the boot volume.
    fn key(&self, out: &mut [u8; KEY_MAX]) -> Option<usize>;
    /// HOST (a name, a peer number or a MAC) to the machine's MAC and the interface to reach it on.
    fn peer(&self, host: &str) -> Option<([u8; 6], usize)>;
    /// Sends one frame with tag `TAG_GPIO_REQUEST` and waits up to 2 s for one `TAG_GPIO_REPLY` from the same machine; its
    /// payload goes to `out`.
    fn exchange(&self, mac: [u8; 6], nic: usize, request: &[u8], out: &mut [u8]) -> Result<usize, &'static str>;
}

macro_rules! say {
    ($env:expr, $($arguments:tt)*) => {
        $env.print(format_args!($($arguments)*))
    };
}

/// Asks the machine at `mac` (interface `nic`) to do `op`; the answer's data is copied to `out`. `Err` says why not.
pub fn call(env: &impl Env, mac: [u8; 6], nic: usize, op: u8, args: &[u8], out: &mut [u8]) -> Result<(u8, usize), &'static str> {
    let mut key = [0u8; KEY_MAX];
    let key_length = env.key(&mut key).ok_or("no `synckey` file here (one line of text, the same on both machines)")?;
    let key = &key[..key_length];
    const NO_ANSWER: &str = "no answer (not a fos machine at its prompt, no synckey there, or another one)";
    let mut frame = [0u8; 1 + NONCE_LEN + MAC_LEN + 16 + 8];
    let mut answer = [0u8; 1 + NONCE_LEN + 8 * 3 + 2 * PINS + MAC_LEN];
    let length = env.exchange(mac, nic, &[0], &mut answer).map_err(|_| NO_ANSWER)?;
    if length >= 1 && answer[0] == STATUS_NO_GPIO {
        return Err("that machine has no GPIO pins");
    }
    if length < 1 + NONCE_LEN || answer[0] != STATUS_OK {
        return Err("the answer is not a GPIO challenge");
    }
    let nonce: [u8; NONCE_LEN] = answer[1..1 + NONCE_LEN].try_into().unwrap();
    if args_len(op) != Some(args.len()) {
        return Err("bad request");
    }
    frame[0] = op;
    frame[1..1 + NONCE_LEN].copy_from_slice(&nonce);
    frame[1 + NONCE_LEN..1 + NONCE_LEN + args.len()].copy_from_slice(args);
    let end = 1 + NONCE_LEN + args.len();
    let mac_bytes = mac_of(REQUEST_LABEL, op, &nonce, args, key);
    frame[end..end + MAC_LEN].copy_from_slice(&mac_bytes);
    let length = env.exchange(mac, nic, &frame[..end + MAC_LEN], &mut answer).map_err(|_| "no answer to the request (the key does not match, or the challenge expired)")?;
    // An error status carries no data; a good one carries exactly this op's data.
    let size = if answer[0] == STATUS_OK || (op == OP_SET && answer[0] == STATUS_REFUSED) { data_len(op) } else { 0 };
    if length < 1 + NONCE_LEN + size + MAC_LEN || answer[1..1 + NONCE_LEN] != nonce {
        return Err("the answer does not belong to this request");
    }
    let data = &answer[1 + NONCE_LEN..1 + NONCE_LEN + size];
    if !equal(&mac_of(REPLY_LABEL, answer[0], &nonce, data, key), &answer[1 + NONCE_LEN + size..1 + NONCE_LEN + size + MAC_LEN]) {
        return Err("the answer is not signed with our key");
    }
    if data.len() > out.len() {
        return Err("the answer is longer than expected");
    }
    out[..data.len()].copy_from_slice(data);
    Ok((answer[0], data.len()))
}

/// `Snapshot` from the data of an `OP_STATE` answer.
pub fn decode_state(data: &[u8]) -> Option<Snapshot> {
    if data.len() != 24 + 2 * PINS {
        return None;
    }
    let mut snapshot = Snapshot { levels: u64_at(data, 0), allowed: u64_at(data, 8), writable: u64_at(data, 16), function: [0; PINS], pull: [0; PINS] };
    snapshot.function.copy_from_slice(&data[24..24 + PINS]);
    snapshot.pull.copy_from_slice(&data[24 + PINS..24 + 2 * PINS]);
    Some(snapshot)
}

pub fn status_text(status: u8) -> &'static str {
    match status {
        STATUS_OK => "done",
        STATUS_NO_GPIO => "that machine has no GPIO pins",
        STATUS_NO_WRITE => "writes are not enabled there (`gpio-remote` in its settings file)",
        STATUS_REFUSED => "that pin (or one of them) is not allowed or not an output",
        _ => "the request was not understood",
    }
}

pub const USAGE: &str = "gpio [@HOST] [list [all]|read N|set N 0|1|mode N in|out 0|1|alt0..alt5|pull N none|up|down]";

pub const FUNCTIONS: [&str; 8] = ["in", "out", "alt0", "alt1", "alt2", "alt3", "alt4", "alt5"];
pub const PULLS: [&str; 3] = ["none", "up", "down"];

pub enum Op {
    List(bool),
    Read(usize),
    Set(usize, bool),
    Mode(usize, u8, bool),
    Pull(usize, u8),
}

fn parse(words: &mut core::str::SplitWhitespace) -> Option<Op> {
    let pin = |word: Option<&str>| word?.parse::<usize>().ok().filter(|&pin| pin < PINS);
    let op = match words.next().unwrap_or("list") {
        "list" => Op::List(words.next() == Some("all")),
        "read" => Op::Read(pin(words.next())?),
        "set" => {
            let number = pin(words.next())?;
            Op::Set(number, level(words.next()?)?)
        }
        "mode" => {
            let number = pin(words.next())?;
            let name = words.next()?;
            let function = FUNCTIONS.iter().position(|known| *known == name)? as u8;
            // An output must say where it starts: nothing is driven by surprise.
            let start = if function == FUNC_OUT { level(words.next()?)? } else { false };
            Op::Mode(number, function, start)
        }
        "pull" => {
            let number = pin(words.next())?;
            let name = words.next()?;
            Op::Pull(number, PULLS.iter().position(|known| *known == name)? as u8)
        }
        _ => return None,
    };
    words.next().is_none().then_some(op)
}

fn level(word: &str) -> Option<bool> {
    match word {
        "0" | "low" | "off" => Some(false),
        "1" | "high" | "on" => Some(true),
        _ => None,
    }
}

/// `gpio [@HOST] ...`: with a host the request goes over the network, without one `local` runs it on this machine's pins.
pub fn command(env: &impl Env, rest: &str, local: impl FnOnce(Op)) {
    let mut words = rest.split_whitespace();
    let host = match words.clone().next() {
        Some(word) if word.starts_with('@') => {
            words.next();
            Some(&word[1..])
        }
        _ => None,
    };
    let Some(op) = parse(&mut words) else { return say!(env, "Usage: {USAGE}") };
    match host {
        Some(host) => run_remote(env, host, op),
        None => local(op),
    }
}

fn run_remote(env: &impl Env, host: &str, op: Op) {
    let Some((mac, nic)) = env.peer(host) else { return say!(env, "gpio: no machine called {host} answered (see `nic peers`).") };
    let mut data = [0u8; 8 * 3 + 2 * PINS];
    let (code, args, length): (u8, [u8; 16], usize) = match op {
        Op::List(_) | Op::Read(_) => (OP_STATE, [0; 16], 0),
        Op::Set(pin, high) => {
            let mut args = [0u8; 16];
            args[0..8].copy_from_slice(&(1u64 << pin).to_le_bytes());
            args[8..16].copy_from_slice(&(u64::from(high) << pin).to_le_bytes());
            (OP_SET, args, 16)
        }
        Op::Mode(pin, function, start) => (OP_FUNCTION, pad([pin as u8, function, u8::from(start)]), 3),
        Op::Pull(pin, pull) => (OP_PULL, pad([pin as u8, pull, 0]), 2),
    };
    let (status, received) = match call(env, mac, nic, code, &args[..length], &mut data) {
        Ok(answer) => answer,
        Err(reason) => return say!(env, "gpio: {reason}."),
    };
    if status != STATUS_OK {
        return say!(env, "gpio: {}.", status_text(status));
    }
    match op {
        Op::List(_) | Op::Read(_) => {
            let Some(snapshot) = decode_state(&data[..received]) else { return say!(env, "gpio: the answer is not a pin list.") };
            match op {
                Op::Read(pin) => line(env, &snapshot, pin),
                Op::List(all) => list(env, &snapshot, all),
                _ => {}
            }
        }
        _ => say!(env, "done"),
    }
}

fn pad<const N: usize>(bytes: [u8; N]) -> [u8; 16] {
    let mut out = [0u8; 16];
    out[..N].copy_from_slice(&bytes);
    out
}

pub fn line(env: &impl Env, snapshot: &Snapshot, pin: usize) {
    let readonly = if snapshot.allowed >> pin & 1 == 0 { '*' } else { ' ' };
    let function = FUNCTIONS[usize::from(snapshot.function[pin] & 7)];
    let level = if snapshot.levels >> pin & 1 != 0 { "high" } else { "low" };
    let pull = PULLS.get(usize::from(snapshot.pull[pin])).copied().unwrap_or("?");
    say!(env, "GPIO{pin:<2}{readonly} {function:<5} {level:<5} pull {pull}");
}

/// The pins that can be changed, and any other that is not a plain input; `all` shows every pin. `*` marks a read-only pin.
pub fn list(env: &impl Env, snapshot: &Snapshot, all: bool) {
    let mut shown = 0;
    for pin in 0..PINS {
        if all || snapshot.allowed >> pin & 1 != 0 || snapshot.function[pin] != FUNC_IN {
            line(env, snapshot, pin);
            shown += 1;
        }
    }
    say!(env, "{shown} pins shown; * = read only{}.", if snapshot.writable == 0 { "; writing from other machines is off" } else { "" });
}
