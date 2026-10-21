//! The `gpio` shell command, both kernels: this machine's pins, or another's with `@HOST` (`gpio_remote.rs`).

use crate::gpio::{self, PINS, Snapshot};
use crate::gpio_remote as remote;

const USAGE: &str = "gpio [@HOST] [list [all]|read N|set N 0|1|mode N in|out 0|1|alt0..alt5|pull N none|up|down]";

const FUNCTIONS: [&str; 8] = ["in", "out", "alt0", "alt1", "alt2", "alt3", "alt4", "alt5"];
const PULLS: [&str; 3] = ["none", "up", "down"];

enum Op {
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
            let start = if function == gpio::FUNC_OUT { level(words.next()?)? } else { false };
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

pub fn command(rest: &str) {
    let mut words = rest.split_whitespace();
    let mut peeked = words.clone();
    let host = match peeked.next() {
        Some(word) if word.starts_with('@') => {
            words.next();
            Some(&word[1..])
        }
        _ => None,
    };
    let Some(op) = parse(&mut words) else { return println!("Usage: {USAGE}") };
    match host {
        Some(host) => run_remote(host, op),
        None => run_local(op),
    }
}

fn run_local(op: Op) {
    if !gpio::available() {
        return println!("gpio: this machine has no GPIO pins; `gpio @HOST ...` works on one that has.");
    }
    let outcome = match op {
        Op::List(all) => return list(&gpio::snapshot().unwrap(), all),
        Op::Read(pin) => return one(&gpio::snapshot().unwrap(), pin),
        Op::Set(pin, high) => {
            let refused = gpio::write(1 << pin, u64::from(high) << pin);
            if refused == 0 { Ok(()) } else { Err("that pin is not allowed or not an output (`gpio mode N out 0|1` first)") }
        }
        Op::Mode(pin, function, start) => gpio::set_function(pin, function, start).map_err(gpio::Error::text),
        Op::Pull(pin, pull) => gpio::set_pull(pin, pull).map_err(gpio::Error::text),
    };
    match outcome {
        Ok(()) => println!("done"),
        Err(reason) => println!("gpio: {reason}."),
    }
}

fn run_remote(host: &str, op: Op) {
    let Some((mac, nic, _)) = crate::netfs::netfs_peer(host) else { return println!("gpio: no machine called {host} answered (see `nic peers`).") };
    let mut data = [0u8; 8 * 3 + 2 * PINS];
    let (code, args, length): (u8, [u8; 16], usize) = match op {
        Op::List(_) | Op::Read(_) => (remote::OP_STATE, [0; 16], 0),
        Op::Set(pin, high) => {
            let mut args = [0u8; 16];
            args[0..8].copy_from_slice(&(1u64 << pin).to_le_bytes());
            args[8..16].copy_from_slice(&(u64::from(high) << pin).to_le_bytes());
            (remote::OP_SET, args, 16)
        }
        Op::Mode(pin, function, start) => (remote::OP_FUNCTION, pad([pin as u8, function, u8::from(start)]), 3),
        Op::Pull(pin, pull) => (remote::OP_PULL, pad([pin as u8, pull, 0]), 2),
    };
    let (status, received) = match remote::call(mac, nic, code, &args[..length], &mut data) {
        Ok(answer) => answer,
        Err(reason) => return println!("gpio: {reason}."),
    };
    if status != remote::STATUS_OK {
        return println!("gpio: {}.", remote::status_text(status));
    }
    match op {
        Op::List(_) | Op::Read(_) => {
            let Some(snapshot) = remote::decode_state(&data[..received]) else { return println!("gpio: the answer is not a pin list.") };
            match op {
                Op::Read(pin) => one(&snapshot, pin),
                Op::List(all) => list(&snapshot, all),
                _ => {}
            }
        }
        _ => println!("done"),
    }
}

fn pad<const N: usize>(bytes: [u8; N]) -> [u8; 16] {
    let mut out = [0u8; 16];
    out[..N].copy_from_slice(&bytes);
    out
}

fn line(snapshot: &Snapshot, pin: usize) {
    let readonly = if snapshot.allowed >> pin & 1 == 0 { '*' } else { ' ' };
    let function = FUNCTIONS[usize::from(snapshot.function[pin] & 7)];
    let level = if snapshot.levels >> pin & 1 != 0 { "high" } else { "low" };
    let pull = PULLS.get(usize::from(snapshot.pull[pin])).copied().unwrap_or("?");
    println!("GPIO{pin:<2}{readonly} {function:<5} {level:<5} pull {pull}");
}

fn one(snapshot: &Snapshot, pin: usize) {
    line(snapshot, pin);
}

/// The pins that can be changed, and any other that is not a plain input; `all` shows every pin. `*` marks a read-only pin.
fn list(snapshot: &Snapshot, all: bool) {
    let mut shown = 0;
    for pin in 0..PINS {
        if all || snapshot.allowed >> pin & 1 != 0 || snapshot.function[pin] != gpio::FUNC_IN {
            line(snapshot, pin);
            shown += 1;
        }
    }
    println!("{shown} pins shown; * = read only{}.", if snapshot.writable == 0 { "; writing from other machines is off" } else { "" });
}
