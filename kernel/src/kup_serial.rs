//! KUP over a serial line: the kernel update of `term.rs` (KUP over Ethernet) for a machine whose network does not work
//! yet, or one that has only a serial cable. The shell command `kup serial` announces `KUP serial: ready` and then speaks this
//! protocol on the console UART until the update is in the inactive A/B slot; `tools/send_kernel_serial.py` is the other end.
//!
//! A frame, in both directions: `F0 5B`, kind (1), sequence (u16 LE), length (u16 LE), payload, CRC-32 (LE) of everything
//! from the kind to the end of the payload. Stop and wait: every frame from the host is answered before the next is sent.
//!   host -> kernel   'S' start (seq 0): image length u32, image CRC-32 u32 (LE);  'D' data (seq 1, 2, ...): up to 2048 bytes;
//!                    'E' end;  'Q' abandon.
//!   kernel -> host   'A' ack (seq of the frame); 'N' nak (seq = the one it wants: bad CRC, a gap); 'F' failure (text);
//!                    'K' the image is verified, written to the inactive slot and the machine restarts.
//! A repeated data frame is acked again, not stored twice, so a lost ack costs one resend. Nothing is written to the disk until
//! the whole image has arrived and its CRC matches; a failed transfer leaves the slots and the running kernel untouched.
//! The console must stay silent while the transfer runs (the kernel prints nothing in between); the framing tolerates noise.

#[cfg(target_arch = "aarch64")]
use crate::slots as metadata;
#[cfg(not(target_arch = "aarch64"))]
use crate::debug as metadata;

use fosfor_abi::{SLOT_SECTORS, crc32, slot_start};

pub const MAGIC: [u8; 2] = [0xf0, 0x5b];
pub const CHUNK: usize = 2048;
/// Time for the sender to start after `ready`, and the longest silence once it has.
const START_TIMEOUT_US: u64 = 30_000_000;
const IDLE_TIMEOUT_US: u64 = 5_000_000;
const BYTE_TIMEOUT_US: u64 = 500_000;

/// The serial port and clock the protocol runs over.
pub trait Link {
    fn read_byte(&mut self) -> Option<u8>;
    fn write_byte(&mut self, byte: u8);
    fn micros(&self) -> u64;
}

enum Read {
    Timeout,
    Bad,
    Good { kind: u8, sequence: u16, length: usize },
}

fn next_byte(link: &mut impl Link, deadline: u64) -> Option<u8> {
    loop {
        if let Some(byte) = link.read_byte() {
            return Some(byte);
        }
        if link.micros() >= deadline {
            return None;
        }
    }
}

/// Fills `target` from the line, each byte within `BYTE_TIMEOUT_US` of the last.
fn fill(link: &mut impl Link, target: &mut [u8]) -> bool {
    for slot in target {
        let deadline = link.micros() + BYTE_TIMEOUT_US;
        let Some(byte) = next_byte(link, deadline) else { return false };
        *slot = byte;
    }
    true
}

/// Reads one frame into `buffer` (header, payload, CRC); the payload is `buffer[5..5 + length]`.
fn read_frame(link: &mut impl Link, buffer: &mut [u8; 5 + CHUNK + 4], wait_us: u64) -> Read {
    let deadline = link.micros() + wait_us;
    let mut previous = 0;
    loop {
        let Some(byte) = next_byte(link, deadline) else { return Read::Timeout };
        if previous == MAGIC[0] && byte == MAGIC[1] {
            break;
        }
        previous = byte;
    }
    if !fill(link, &mut buffer[..5]) {
        return Read::Bad;
    }
    let length = usize::from(u16::from_le_bytes([buffer[3], buffer[4]]));
    if length > CHUNK || !fill(link, &mut buffer[5..5 + length + 4]) {
        return Read::Bad;
    }
    let sent = u32::from_le_bytes([buffer[5 + length], buffer[6 + length], buffer[7 + length], buffer[8 + length]]);
    if crc32(&buffer[..5 + length]) != sent {
        return Read::Bad;
    }
    Read::Good { kind: buffer[0], sequence: u16::from_le_bytes([buffer[1], buffer[2]]), length }
}

fn send(link: &mut impl Link, kind: u8, sequence: u16, payload: &[u8]) {
    let mut frame = [0u8; 5 + 96];
    let length = payload.len().min(96);
    frame[0] = kind;
    frame[1..3].copy_from_slice(&sequence.to_le_bytes());
    frame[3..5].copy_from_slice(&(length as u16).to_le_bytes());
    frame[5..5 + length].copy_from_slice(&payload[..length]);
    let crc = crc32(&frame[..5 + length]).to_le_bytes();
    for byte in MAGIC.into_iter().chain(frame[..5 + length].iter().copied()).chain(crc) {
        link.write_byte(byte);
    }
}

/// Receives an image into `staging` and returns its length once the end frame has arrived and the whole-image CRC matches.
/// The caller installs it and finishes with `send(b'K')` or `send(b'F')` (see `serve`).
fn receive(link: &mut impl Link, staging: &mut [u8]) -> Result<usize, &'static str> {
    let mut buffer = [0u8; 5 + CHUNK + 4];
    let mut started: Option<(usize, u32)> = None;
    let mut received = 0usize;
    let mut expected: u16 = 1;
    loop {
        let wait = if started.is_some() { IDLE_TIMEOUT_US } else { START_TIMEOUT_US };
        match read_frame(link, &mut buffer, wait) {
            Read::Timeout => return Err("timed out waiting for the sender"),
            Read::Bad => send(link, b'N', expected, &[]),
            Read::Good { kind, sequence, length } => {
                let payload = &buffer[5..5 + length];
                match (kind, started) {
                    (b'S', None) if length == 8 => {
                        let total = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]) as usize;
                        let crc = u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]);
                        if total == 0 || total > staging.len() || total.div_ceil(512) as u64 > SLOT_SECTORS {
                            send(link, b'F', 0, b"image too large for a slot");
                            return Err("the image does not fit a kernel slot");
                        }
                        started = Some((total, crc));
                        send(link, b'A', 0, &[]);
                    }
                    (b'S', Some(_)) => send(link, b'A', 0, &[]),
                    (b'D', Some((total, _))) => {
                        if sequence == expected && received + length <= total {
                            staging[received..received + length].copy_from_slice(payload);
                            received += length;
                            expected = expected.wrapping_add(1);
                            send(link, b'A', sequence, &[]);
                        } else if sequence == expected.wrapping_sub(1) {
                            send(link, b'A', sequence, &[]);
                        } else {
                            send(link, b'N', expected, &[]);
                        }
                    }
                    (b'E', Some((total, crc))) => {
                        if received == total && crc32(&staging[..total]) == crc {
                            return Ok(total);
                        }
                        send(link, b'F', 0, b"length or checksum mismatch");
                        return Err("the image is incomplete or its checksum does not match");
                    }
                    (b'Q', _) => return Err("abandoned by the sender"),
                    _ => send(link, b'N', expected, &[]),
                }
            }
        }
    }
}

/// Writes the verified image into the inactive slot and makes it the pending one: the loader gives it exactly one try and
/// falls back to the running kernel unless it reaches its milestone. Returns the slot.
pub fn install(staging: &mut [u8], length: usize) -> Result<u8, &'static str> {
    let (Some(current), Some(disk), Some(payload)) = (metadata::read_metadata(), crate::storage::boot_disk(), crate::storage::payload_start()) else {
        return Err("the slot metadata or the boot disk is not available");
    };
    let slot = 1 - (current.active_slot & 1);
    let sectors = length.div_ceil(512);
    staging[length..sectors * 512].fill(0);
    crate::block::write(disk, payload + slot_start(u64::from(slot)), &staging[..sectors * 512]).map_err(|_| "writing the slot failed")?;
    metadata::update_metadata(|m| {
        m.pending_slot = Some(slot);
        m.attempted = false;
        m.confirmed = false;
        m.slot_sectors[usize::from(slot)] = sectors as u16;
    })
    .ok_or("updating the slot metadata failed")?;
    Ok(slot)
}

/// The whole update: receive, verify, install, tell the sender. Returns the slot now pending; the caller restarts the
/// machine. `staging` needs room for the image rounded up to a sector.
pub fn serve(link: &mut impl Link, staging: &mut [u8]) -> Result<u8, &'static str> {
    let length = receive(link, staging)?;
    match install(staging, length) {
        Ok(slot) => {
            send(link, b'K', u16::from(slot), &[]);
            Ok(slot)
        }
        Err(message) => {
            send(link, b'F', 0, message.as_bytes());
            Err(message)
        }
    }
}

/// The same transfer for a file (`recv NAME`, `tools/send_file_serial.py`): receive and verify, hand the
/// bytes to `store`, then answer `K` (sequence 0) or `F` with the reason. Returns the length received.
pub fn serve_file(link: &mut impl Link, staging: &mut [u8], store: impl FnOnce(&[u8]) -> Result<(), &'static str>) -> Result<usize, &'static str> {
    let length = receive(link, staging)?;
    match store(&staging[..length]) {
        Ok(()) => {
            send(link, b'K', 0, &[]);
            Ok(length)
        }
        Err(message) => {
            send(link, b'F', 0, message.as_bytes());
            Err(message)
        }
    }
}
