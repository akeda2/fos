//! GPIO over the network, for both kernels: the Pi serves its pins, either kernel is a client (`gpio @HOST ...`), and so is
//! `tools/gpio.py`. Same frames as NETFS (ethertype 0x88b6), tags `TAG_GPIO_REQUEST` 0x41 and `TAG_GPIO_REPLY` 0x42, served
//! where NETFS is (at the prompt and in `term net`, not while a tape runs).
//!
//! Every request is signed with the `synckey` file (the key `tapesync` uses; no file, no answer) and answered only after a
//! challenge, so a recorded frame cannot be replayed:
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

use crate::gpio::{self, PINS, Snapshot};
use crate::netfs::{TAG_GPIO_REPLY, TAG_GPIO_REQUEST};
use crate::tapesync;
use tape_sync::{NONCE_LEN, hmac_sha256};

const MAC_LEN: usize = 16;
const NONCE_TTL_TICKS: u64 = 5 * crate::timer::TICK_HZ;

pub const OP_STATE: u8 = 1;
pub const OP_SET: u8 = 2;
pub const OP_FUNCTION: u8 = 3;
pub const OP_PULL: u8 = 4;

pub const STATUS_OK: u8 = 0;
pub const STATUS_BAD: u8 = 1;
pub const STATUS_NO_GPIO: u8 = 2;
pub const STATUS_NO_WRITE: u8 = 3;
pub const STATUS_REFUSED: u8 = 4;

fn mac_of(label: &[u8], first: u8, nonce: &[u8], body: &[u8], key: &[u8]) -> [u8; MAC_LEN] {
    let full = hmac_sha256(key, &[label, &[first], nonce, body]);
    full[..MAC_LEN].try_into().unwrap()
}

const REQUEST_LABEL: &[u8] = b"fosgpio1";
const REPLY_LABEL: &[u8] = b"fosgpio1r";

struct Pending {
    source: [u8; 6],
    nonce: [u8; NONCE_LEN],
    issued: u64,
}

static mut PENDING: Option<Pending> = None;

fn args_len(op: u8) -> Option<usize> {
    match op {
        OP_STATE => Some(0),
        OP_SET => Some(16),
        OP_FUNCTION => Some(3),
        OP_PULL => Some(2),
        _ => None,
    }
}

fn data_len(op: u8) -> usize {
    match op {
        OP_STATE => 24 + 2 * PINS,
        OP_SET => 8,
        _ => 0,
    }
}

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
    if !tape_sync::equal(&mac_of(REQUEST_LABEL, op, &nonce, args, key), mac) {
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

/// Asks the machine at `mac` (interface `nic`) to do `op`; the answer's data is copied to `out`. `Err` says why not.
pub fn call(mac: [u8; 6], nic: usize, op: u8, args: &[u8], out: &mut [u8]) -> Result<(u8, usize), &'static str> {
    let volume = crate::storage::boot_volume().ok_or("no boot volume")?;
    let mut key = [0u8; tapesync::KEY_MAX];
    let key_length = tapesync::read_key(&volume, &mut key).ok_or("no `synckey` file here (one line of text, the same on both machines)")?;
    let key = &key[..key_length];
    const NO_ANSWER: &str = "no answer (not a fos machine at its prompt, no synckey there, or another one)";
    let mut frame = [0u8; 1 + NONCE_LEN + MAC_LEN + 16 + 8];
    let mut answer = [0u8; 1 + NONCE_LEN + 8 * 3 + 2 * PINS + MAC_LEN];
    let length = crate::netfs::exchange(mac, nic, TAG_GPIO_REQUEST, &[0], TAG_GPIO_REPLY, &mut answer).map_err(|_| NO_ANSWER)?;
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
    let length = crate::netfs::exchange(mac, nic, TAG_GPIO_REQUEST, &frame[..end + MAC_LEN], TAG_GPIO_REPLY, &mut answer).map_err(|_| "no answer to the request (the key does not match, or the challenge expired)")?;
    // An error status carries no data; a good one carries exactly this op's data.
    let size = if answer[0] == STATUS_OK || (op == OP_SET && answer[0] == STATUS_REFUSED) { data_len(op) } else { 0 };
    if length < 1 + NONCE_LEN + size + MAC_LEN || answer[1..1 + NONCE_LEN] != nonce {
        return Err("the answer does not belong to this request");
    }
    let data = &answer[1 + NONCE_LEN..1 + NONCE_LEN + size];
    if !tape_sync::equal(&mac_of(REPLY_LABEL, answer[0], &nonce, data, key), &answer[1 + NONCE_LEN + size..1 + NONCE_LEN + size + MAC_LEN]) {
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
