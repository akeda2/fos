//! The network terminal's session, shared by the x86 and aarch64 kernels ("term net", wire protocol v1: ethertype 0x88b6, a tag
//! byte, then the payload; `tools/term_net.py` and `devloop.py` speak it).
//!
//! A client sends HELLO to open a session; either side then sends DATA frames carrying raw bytes (keystrokes one way, console
//! output the other), and BYE ends it. This module owns only what is the same on both ports: who the peer is, the stale-session
//! takeover rule, the `deny-term-net` gate, the relay ring that console output fills and the DATA frames that carry it out.
//! What a HELLO, a keystroke or a BYE *does* (prompt, line editor, tape input) differs per shell, so `handle` returns an
//! [`Frame`] and the caller reacts. Frames leave through `netfs::send`, which each port implements for its NIC.

use crate::{netfs, timer};

pub const TAG_HELLO: u8 = 0x20;
pub const TAG_DATA: u8 = 0x21;
pub const TAG_BYE: u8 = 0x22;
/// Bytes per DATA frame sent to the client.
const DATA_CAPACITY: usize = 1024;
/// A session whose client has been silent this long can be taken over by another HELLO.
const STALE_TICKS: u64 = 5 * 60 * timer::TICK_HZ;

static mut PEER: Option<[u8; 6]> = None;
/// The interface the open session's HELLO arrived on: console output goes out of it.
static mut SESSION_NIC: usize = 0;
/// Tick of the last frame from the session's peer.
static mut PEER_ACTIVITY: u64 = 0;
static mut RELAY: [u8; 16384] = [0; 16384];
static mut RELAY_LENGTH: usize = 0;

/// What a received frame means for the session.
pub enum Frame<'a> {
    /// A client opened the session (any earlier one is replaced); the shell shows the connection and a prompt.
    Hello,
    /// Keystrokes from the client, trailing zero bytes removed.
    Data(&'a [u8]),
    /// The client left; the session is closed.
    Bye,
    /// Ours, but nobody should act on it (closed by `deny-term-net`, or someone else's session is alive).
    Ignored,
}

pub fn peer() -> Option<[u8; 6]> {
    unsafe { *core::ptr::addr_of!(PEER) }
}

pub fn session_open() -> bool {
    peer().is_some()
}

/// The interface the remote terminal session runs over, while one is open.
pub fn session_nic() -> Option<usize> {
    session_open().then(|| unsafe { SESSION_NIC })
}

/// Reads one received frame's tag. `None`: not a session frame (the caller keeps looking, a running tape may want it).
pub fn handle<'a>(index: usize, source: [u8; 6], tag: u8, payload: &'a [u8]) -> Option<Frame<'a>> {
    match tag {
        // Closed by the settings file: stay silent.
        TAG_HELLO if netfs::term_denied() => Some(Frame::Ignored),
        // A session is open: another client's HELLO does not steal it (as in v1, where only the waiting state accepts a new
        // peer), unless the owner has been silent for five minutes.
        TAG_HELLO if session_open() && peer() != Some(source) && timer::ticks().wrapping_sub(unsafe { PEER_ACTIVITY }) < STALE_TICKS => {
            Some(Frame::Ignored)
        }
        TAG_HELLO => {
            unsafe {
                *core::ptr::addr_of_mut!(PEER) = Some(source);
                SESSION_NIC = index;
                PEER_ACTIVITY = timer::ticks();
                RELAY_LENGTH = 0;
            }
            Some(Frame::Hello)
        }
        TAG_DATA if session_open() && peer() == Some(source) && unsafe { SESSION_NIC } == index => {
            // The frame has no length field and Ethernet pads a short frame to 60 bytes with zeros, so a single typed key
            // arrives followed by dozens of NULs. Drop the trailing zeros: a tape that read them saw "any other key" right
            // behind every key. A NUL a person really typed (Ctrl-@) is lost, which nobody misses.
            let end = payload.iter().rposition(|&byte| byte != 0).map_or(0, |last| last + 1);
            unsafe { PEER_ACTIVITY = timer::ticks() };
            Some(Frame::Data(&payload[..end]))
        }
        TAG_BYE if peer() == Some(source) => {
            flush();
            unsafe { *core::ptr::addr_of_mut!(PEER) = None };
            Some(Frame::Bye)
        }
        _ => None,
    }
}

/// Called by `print` for every piece of console text; queues it for the client.
pub fn relay(text: &str) {
    if !session_open() {
        return;
    }
    for &byte in text.as_bytes() {
        unsafe {
            let ring = &mut *core::ptr::addr_of_mut!(RELAY);
            if RELAY_LENGTH == ring.len() {
                flush();
            }
            if RELAY_LENGTH < ring.len() {
                ring[RELAY_LENGTH] = byte;
                RELAY_LENGTH += 1;
            }
        }
    }
    if unsafe { RELAY_LENGTH } >= DATA_CAPACITY {
        flush();
    }
}

/// Sends everything queued as DATA frames. A frame that cannot be sent is dropped (a terminal can tolerate a torn screen; a
/// stuck NIC must not stall the shell).
pub fn flush() {
    let Some(peer) = peer() else { return };
    unsafe {
        let ring = &*core::ptr::addr_of!(RELAY);
        let mut offset = 0;
        while offset < RELAY_LENGTH {
            let chunk = (RELAY_LENGTH - offset).min(DATA_CAPACITY);
            netfs::send(SESSION_NIC, &peer, TAG_DATA, &ring[offset..offset + chunk]);
            offset += chunk;
        }
        RELAY_LENGTH = 0;
    }
}
