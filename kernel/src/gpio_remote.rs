//! GPIO over the network: the Pi serves its pins; either kernel is a client (`gpio @HOST ...`), and so is `tools/gpio.py`.
//! Served where NETFS is (at the prompt and in `term net`, not while a tape runs).
//!
//! The protocol, with the client, is in `kernel_core::gpio_client` (shared with the server kernel); this file is the Pi's side:
//! the challenge, the check and the pins.

use crate::gpio;
use crate::tapesync;
use kernel_core::gpio_client::*;
use tape_sync::{NONCE_LEN, equal};

const NONCE_TTL_TICKS: u64 = 5 * crate::timer::TICK_HZ;

struct Pending {
    source: [u8; 6],
    nonce: [u8; NONCE_LEN],
    issued: u64,
}

static mut PENDING: Option<Pending> = None;

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

/// A request frame from `source` (NETFS' dispatcher calls this for tag 0x41).
pub fn serve(index: usize, source: [u8; 6], payload: &[u8]) {
    let Some(&op) = payload.first() else { return };
    let Some(volume) = crate::storage::boot_volume() else { return };
    let mut key = [0u8; tapesync::KEY_MAX];
    let Some(key_length) = tapesync::read_key(&volume, &mut key) else { return };
    let key = &key[..key_length];
    let reply = |frame: &[u8]| crate::netfs::netfs_reply(index, &source, TAG_GPIO_REPLY, frame);
    if op == 0 {
        if !gpio::available() {
            return reply(&[STATUS_NO_GPIO]);
        }
        let nonce = tapesync::nonce();
        unsafe { *core::ptr::addr_of_mut!(PENDING) = Some(Pending { source, nonce, issued: crate::timer::ticks() }) };
        let mut frame = [0u8; 1 + NONCE_LEN];
        frame[1..].copy_from_slice(&nonce);
        return reply(&frame);
    }
    let Some(args_length) = args_len(op) else { return };
    if payload.len() < 1 + NONCE_LEN + args_length + MAC_LEN {
        return;
    }
    let nonce: [u8; NONCE_LEN] = payload[1..1 + NONCE_LEN].try_into().unwrap();
    let valid = unsafe { (*core::ptr::addr_of!(PENDING)).as_ref() }
        .is_some_and(|pending| pending.source == source && pending.nonce == nonce && crate::timer::ticks().wrapping_sub(pending.issued) < NONCE_TTL_TICKS);
    if !valid {
        return;
    }
    unsafe { *core::ptr::addr_of_mut!(PENDING) = None };
    let args = &payload[1 + NONCE_LEN..1 + NONCE_LEN + args_length];
    let mac = &payload[1 + NONCE_LEN + args_length..1 + NONCE_LEN + args_length + MAC_LEN];
    if !equal(&mac_of(REQUEST_LABEL, op, &nonce, args, key), mac) {
        return;
    }
    let mut data = [0u8; 8 * 3 + 2 * PINS];
    let (status, length) = execute(op, args, &mut data);
    let mut frame = [0u8; 1 + NONCE_LEN + 8 * 3 + 2 * PINS + MAC_LEN];
    frame[0] = status;
    frame[1..1 + NONCE_LEN].copy_from_slice(&nonce);
    frame[1 + NONCE_LEN..1 + NONCE_LEN + length].copy_from_slice(&data[..length]);
    let end = 1 + NONCE_LEN + length;
    let mac = mac_of(REPLY_LABEL, status, &nonce, &data[..length], key);
    frame[end..end + MAC_LEN].copy_from_slice(&mac);
    reply(&frame[..end + MAC_LEN]);
}

fn status_of(error: gpio::Error) -> u8 {
    match error {
        gpio::Error::NoGpio => STATUS_NO_GPIO,
        gpio::Error::Refused => STATUS_REFUSED,
        gpio::Error::BadArgument => STATUS_BAD,
    }
}

fn execute(op: u8, args: &[u8], data: &mut [u8]) -> (u8, usize) {
    let Some(snapshot) = gpio::snapshot() else { return (STATUS_NO_GPIO, 0) };
    match (op, args.len()) {
        (OP_STATE, 0) => {
            data[0..8].copy_from_slice(&snapshot.levels.to_le_bytes());
            data[8..16].copy_from_slice(&snapshot.allowed.to_le_bytes());
            data[16..24].copy_from_slice(&snapshot.writable.to_le_bytes());
            data[24..24 + PINS].copy_from_slice(&snapshot.function);
            data[24 + PINS..24 + 2 * PINS].copy_from_slice(&snapshot.pull);
            (STATUS_OK, 24 + 2 * PINS)
        }
        (OP_SET | OP_FUNCTION | OP_PULL, _) if !gpio::remote_writes() => (STATUS_NO_WRITE, 0),
        (OP_SET, 16) => {
            let refused = gpio::write(u64_at(args, 0), u64_at(args, 8));
            data[0..8].copy_from_slice(&refused.to_le_bytes());
            (if refused == 0 { STATUS_OK } else { STATUS_REFUSED }, 8)
        }
        (OP_FUNCTION, 3) => (gpio::set_function(usize::from(args[0]), args[1], args[2] != 0).map_or_else(status_of, |()| STATUS_OK), 0),
        (OP_PULL, 2) => (gpio::set_pull(usize::from(args[0]), args[1]).map_or_else(status_of, |()| STATUS_OK), 0),
        _ => (STATUS_BAD, 0),
    }
}
