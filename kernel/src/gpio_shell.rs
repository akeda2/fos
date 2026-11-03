//! The `gpio` shell command, both kernels: this machine's pins, or another's with `@HOST`. Parsing, the network client and the
//! listing are shared (`kernel_core::gpio_client`); this file runs a command on the local pins.

use crate::gpio;
use kernel_core::gpio_client::{self as client, Op};

struct Embedded;

impl client::Env for Embedded {
    fn print(&self, line: core::fmt::Arguments) {
        println!("{}", line);
    }
    fn key(&self, out: &mut [u8; client::KEY_MAX]) -> Option<usize> {
        crate::tapesync::read_key(&crate::storage::boot_volume()?, out)
    }
    fn peer(&self, host: &str) -> Option<([u8; 6], usize)> {
        crate::netfs::netfs_peer(host).map(|(mac, nic, _)| (mac, nic))
    }
    fn exchange(&self, mac: [u8; 6], nic: usize, request: &[u8], out: &mut [u8]) -> Result<usize, &'static str> {
        crate::netfs::exchange(mac, nic, client::TAG_GPIO_REQUEST, request, client::TAG_GPIO_REPLY, out)
    }
}

pub fn command(rest: &str) {
    client::command(&Embedded, rest, run_local);
}

fn run_local(op: Op) {
    if !gpio::available() {
        return println!("gpio: this machine has no GPIO pins; `gpio @HOST ...` works on one that has.");
    }
    let outcome = match op {
        Op::List(all) => return client::list(&Embedded, &gpio::snapshot().unwrap(), all),
        Op::Read(pin) => return client::line(&Embedded, &gpio::snapshot().unwrap(), pin),
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
