//! Client side of GPIO over the network: the request and state types and the frame building. The protocol is documented in
//! `kernel/src/gpio_remote.rs` (the server, and the shell's client); `tools/gpio.py` is the host-side twin. Ethernet frames,
//! ethertype 0x88b6, one tag byte (0x41 request, 0x42 reply) then the payload. Lengths come from the op, never from the
//! frame: Ethernet pads short frames with zeros.

use tape_sync::hmac_sha256;

pub const PINS: usize = 58;
pub const NONCE_LEN: usize = 16;
pub const MAC_LEN: usize = 16;
/// The key is the first line of `synckey`, at most this long (as `tapesync` reads it).
pub const KEY_MAX: usize = 128;

pub const TAG_REQUEST: u8 = 0x41;
pub const TAG_REPLY: u8 = 0x42;
pub const ETHERTYPE: [u8; 2] = [0x88, 0xb6];

pub const OP_STATE: u8 = 1;
pub const OP_SET: u8 = 2;
pub const OP_FUNCTION: u8 = 3;
pub const OP_PULL: u8 = 4;

pub const STATUS_OK: u8 = 0;
pub const STATUS_NO_GPIO: u8 = 2;
pub const STATUS_NO_WRITE: u8 = 3;
pub const STATUS_REFUSED: u8 = 4;

/// What `State::pull` holds for a pin whose pull nobody has told us (a local pin: the tape ABI cannot read it back).
pub const PULL_UNKNOWN: u8 = 255;

/// The longest frame we send: header (14), tag, op, nonce, 16 bytes of arguments, MAC.
pub const FRAME_MAX: usize = 64;
/// The shortest Ethernet frame the NICs put on the wire (without the FCS).
const MIN_FRAME: usize = 60;

/// One thing to ask of the pins.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Request {
    State,
    /// Set the outputs in `mask` to the matching bits of `values`.
    Set { mask: u64, values: u64 },
    /// 0 input, 1 output (starting at `level`), 2..=7 alt0..alt5.
    Function { pin: u8, function: u8, level: bool },
    /// 0 none, 1 up, 2 down.
    Pull { pin: u8, pull: u8 },
}

impl Request {
    /// The op and its arguments, in `args`; returns (op, argument length).
    pub fn encode(&self, args: &mut [u8; 16]) -> (u8, usize) {
        match *self {
            Request::State => (OP_STATE, 0),
            Request::Set { mask, values } => {
                args[..8].copy_from_slice(&mask.to_le_bytes());
                args[8..].copy_from_slice(&values.to_le_bytes());
                (OP_SET, 16)
            }
            Request::Function { pin, function, level } => {
                args[..3].copy_from_slice(&[pin, function, u8::from(level)]);
                (OP_FUNCTION, 3)
            }
            Request::Pull { pin, pull } => {
                args[..2].copy_from_slice(&[pin, pull]);
                (OP_PULL, 2)
            }
        }
    }
}

pub const STATE_DATA_LEN: usize = 24 + 2 * PINS;

/// How many data bytes a good answer to `op` carries.
pub fn data_len(op: u8) -> usize {
    match op {
        OP_STATE => STATE_DATA_LEN,
        OP_SET => 8,
        _ => 0,
    }
}

/// Everything about the pins: what a listing shows.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct State {
    /// The level of every pin, GPIO0 in bit 0.
    pub levels: u64,
    /// Pins that may be changed at all.
    pub allowed: u64,
    /// Pins that may be changed by us now (`allowed`, or 0 when the machine does not take remote writes).
    pub writable: u64,
    pub function: [u8; PINS],
    pub pull: [u8; PINS],
}

impl State {
    pub fn decode(data: &[u8]) -> Option<State> {
        if data.len() != data_len(OP_STATE) {
            return None;
        }
        let word = |at: usize| u64::from_le_bytes(data[at..at + 8].try_into().unwrap());
        let mut state = State { levels: word(0), allowed: word(8), writable: word(16), function: [0; PINS], pull: [0; PINS] };
        state.function.copy_from_slice(&data[24..24 + PINS]);
        state.pull.copy_from_slice(&data[24 + PINS..24 + 2 * PINS]);
        Some(state)
    }

    pub fn level(&self, pin: u8) -> bool {
        self.levels >> pin & 1 != 0
    }

    pub fn is_allowed(&self, pin: u8) -> bool {
        self.allowed >> pin & 1 != 0
    }

    pub fn is_writable(&self, pin: u8) -> bool {
        self.writable >> pin & 1 != 0
    }

    pub fn function_of(&self, pin: u8) -> u8 {
        self.function[usize::from(pin).min(PINS - 1)]
    }

    pub fn pull_of(&self, pin: u8) -> u8 {
        self.pull[usize::from(pin).min(PINS - 1)]
    }
}

/// The MAC (first 16 bytes of HMAC-SHA-256) over `label`, the op or status, the nonce and `body`.
pub fn mac(label: &[u8], first: u8, nonce: &[u8], body: &[u8], key: &[u8]) -> [u8; MAC_LEN] {
    let full = hmac_sha256(key, &[label, &[first], nonce, body]);
    full[..MAC_LEN].try_into().unwrap()
}

pub const REQUEST_LABEL: &[u8] = b"fosgpio1";
pub const REPLY_LABEL: &[u8] = b"fosgpio1r";

/// A whole Ethernet frame to `dst` from `src` with `tag` and `payload`, zero padded to the minimum; returns its length.
pub fn build(out: &mut [u8; FRAME_MAX], dst: [u8; 6], src: [u8; 6], tag: u8, payload: &[u8]) -> usize {
    out.fill(0);
    out[..6].copy_from_slice(&dst);
    out[6..12].copy_from_slice(&src);
    out[12..14].copy_from_slice(&ETHERTYPE);
    out[14] = tag;
    out[15..15 + payload.len()].copy_from_slice(payload);
    (15 + payload.len()).max(MIN_FRAME)
}

/// The payload of `frame` if it is a GPIO reply from `peer`.
pub fn reply_from<'a>(frame: &'a [u8], peer: [u8; 6]) -> Option<&'a [u8]> {
    (frame.len() > 15 && frame[12..14] == ETHERTYPE && frame[6..12] == peer && frame[14] == TAG_REPLY).then(|| &frame[15..])
}

pub fn status_text(status: u8) -> &'static str {
    match status {
        STATUS_OK => "done",
        STATUS_NO_GPIO => "that machine has no GPIO pins",
        STATUS_NO_WRITE => "writes are not enabled there (gpio-remote)",
        STATUS_REFUSED => "that pin is not allowed or not an output",
        _ => "the request was not understood",
    }
}
