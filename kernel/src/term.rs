//! The network terminal ("term net") and the small shell behind it.
//!
//! This is the same wire protocol v1 uses, so `tools/term_net.py` and `devloop.py`
//! work unchanged: raw Ethernet frames with ethertype 0x88b6, one tag byte after the
//! header. A client sends HELLO to open a session; either side then sends DATA frames
//! carrying raw bytes (keystrokes one way, console output the other) and BYE to end it.
//! Discovery probes (tag 0x02) are answered with tag 0x03 carrying the hostname, so
//! `nic discover`/`tools/peers.py` see this machine.
//!
//! Console output reaches the client through a relay ring that `print` fills while a
//! session is open; it is flushed to the wire in 1 KiB DATA frames.

use crate::nic::Nic;
use crate::{apic, memory, pci, power};

const ETHERTYPE: [u8; 2] = [0x88, 0xb6];
const TAG_DISCOVER_REQUEST: u8 = 0x02;
const TAG_DISCOVER_REPLY: u8 = 0x03;
const TAG_HELLO: u8 = 0x20;
const TAG_DATA: u8 = 0x21;
const TAG_BYE: u8 = 0x22;
const DATA_CAPACITY: usize = 1024;
// NETFS: files between fos machines (and tools/foscopy.py) over the same frames as v1; see NETWORKING.md.
const TAG_GET_REQUEST: u8 = 0x30;
const TAG_GET_START: u8 = 0x31;
const TAG_GET_DATA: u8 = 0x32;
const TAG_GET_STOP: u8 = 0x33;
const TAG_GET_ERROR: u8 = 0x34;
const TAG_PUT_REQUEST: u8 = 0x35;
const TAG_PUT_DATA: u8 = 0x36;
const TAG_PUT_STOP: u8 = 0x37;
const TAG_PUT_ACK: u8 = 0x38;
const TAG_PUT_ERROR: u8 = 0x39;
const NETFS_NOT_FOUND: u8 = 1;
const NETFS_NO_DISK: u8 = 2;
const NETFS_TOO_LARGE: u8 = 3;
const NETFS_BAD_NAME: u8 = 4;
const NETFS_DIRECTORY_FULL: u8 = 5;
const NETFS_DATA_FULL: u8 = 6;
const NETFS_DISK_ERROR: u8 = 7;
const NETFS_CHECKSUM: u8 = 8;
const NETFS_BUSY: u8 = 9;
const NETFS_CHUNK: usize = 1024;
const TAG_KUP_REQUEST: u8 = 0x3a;
const TAG_KUP_DATA: u8 = 0x3b;
const TAG_KUP_STOP: u8 = 0x3c;
const TAG_KUP_ACK: u8 = 0x3d;
const TAG_KUP_ERROR: u8 = 0x3e;
const KUP_TOO_LARGE: u8 = 1;
const KUP_CHECKSUM_MISMATCH: u8 = 2;
const KUP_DISK_ERROR: u8 = 3;
const KUP_BUSY: u8 = 4;
/// The name announced in discovery replies for interface `index`: `NAME<n>` from the
/// `hostname` file (n is the one-based interface number, as v1 does), or without that file
/// `fos-` and the last three bytes of the interface's MAC address (so that machines that were
/// never named still tell each other apart).
pub fn hostname(index: usize, out: &mut [u8; 20]) -> usize {
    use core::fmt::Write;
    struct Cursor<'a>(&'a mut [u8; 20], usize);
    impl Write for Cursor<'_> {
        fn write_str(&mut self, text: &str) -> core::fmt::Result {
            let take = text.len().min(20 - self.1);
            self.0[self.1..self.1 + take].copy_from_slice(&text.as_bytes()[..take]);
            self.1 += take;
            Ok(())
        }
    }
    let mut cursor = Cursor(out, 0);
    let _ = match (crate::config::hostname(), nic_at(index).map(|nic| nic.mac())) {
        (Some(base), _) => write!(cursor, "{base}{}", index + 1),
        (None, Some(mac)) => write!(cursor, "fos-{:02x}{:02x}{:02x}", mac[3], mac[4], mac[5]),
        (None, None) => write!(cursor, "fos"),
    };
    cursor.1
}

pub const MAX_NICS: usize = 8;
static mut NICS: [Option<Nic>; MAX_NICS] = [const { None }; MAX_NICS];
static mut NIC_COUNT: usize = 0;
/// The interface the open session's HELLO arrived on: console output goes out of it.
static mut SESSION_NIC: usize = 0;
/// Tick of the last frame from the session's peer (for the stale-session takeover rule).
static mut PEER_ACTIVITY: u64 = 0;
static mut PEER: Option<[u8; 6]> = None;
static mut RELAY: [u8; 16384] = [0; 16384];
static mut RELAY_LENGTH: usize = 0;
static mut RUNTIME_SERVICES: u64 = 0;


/// Adds a network interface; returns its zero-based index.
pub fn install(nic: Nic, runtime_services: u64) -> Option<usize> {
    unsafe {
        RUNTIME_SERVICES = runtime_services;
        if NIC_COUNT == MAX_NICS {
            return None;
        }
        (*core::ptr::addr_of_mut!(NICS))[NIC_COUNT] = Some(nic);
        NIC_COUNT += 1;
        Some(NIC_COUNT - 1)
    }
}

/// True when every interface delivers interrupts (MSI), so a frame wakes the CPU and a
/// tape may sleep in `hlt` instead of polling. One polled NIC is enough to say no: its
/// frames would only be noticed at the next timer tick.
pub fn all_interrupt_driven() -> bool {
    let count = nic_count();
    count > 0 && (0..count).all(|index| nic_at(index).is_some_and(|nic| nic.delivery() != crate::e1000::Delivery::Polled))
}

pub fn nic_count() -> usize {
    unsafe { NIC_COUNT }
}

fn nic_at(index: usize) -> Option<&'static mut Nic> {
    unsafe { (*core::ptr::addr_of_mut!(NICS)).get_mut(index)?.as_mut() }
}

/// The session's interface.
fn nic() -> Option<&'static mut Nic> {
    nic_at(unsafe { SESSION_NIC })
}

/// Set while a shell command that runs until Escape is in charge of the input.
static mut BLOCKING: bool = false;
static mut BREAK: bool = false;

/// True once Escape was typed at the machine, on the serial line or by the remote client.
fn break_requested() -> bool {
    while let Some(key) = crate::keyboard::pop() {
        if key == 0x1b {
            unsafe { BREAK = true };
        }
    }
    unsafe { BREAK }
}

fn blocking(on: bool) {
    unsafe {
        BLOCKING = on;
        BREAK = false;
    }
}

/// The NIC the nic subcommands default to (zero-based): `nic use N`.
static mut ACTIVE_NIC: usize = 0;

fn session_open() -> bool {
    unsafe { (*core::ptr::addr_of!(PEER)).is_some() }
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

/// Sends everything queued as DATA frames. A frame that cannot be sent is dropped
/// (a terminal can tolerate a torn screen; a stuck NIC must not stall the shell).
fn flush() {
    let (Some(nic), Some(peer)) = (nic(), unsafe { *core::ptr::addr_of!(PEER) }) else { return };
    unsafe {
        let ring = &*core::ptr::addr_of!(RELAY);
        let mut offset = 0;
        while offset < RELAY_LENGTH {
            let chunk = (RELAY_LENGTH - offset).min(DATA_CAPACITY);
            send(nic, &peer, TAG_DATA, &ring[offset..offset + chunk]);
            offset += chunk;
        }
        RELAY_LENGTH = 0;
    }
}

fn send(nic: &mut Nic, destination: &[u8; 6], tag: u8, payload: &[u8]) -> bool {
    // Up to a full Ethernet frame (NETFS data frames carry 4 + 1024 bytes after the tag).
    let mut frame = [0u8; 1514];
    if payload.len() > 1514 - 15 {
        return false;
    }
    frame[0..6].copy_from_slice(destination);
    frame[6..12].copy_from_slice(&nic.mac());
    frame[12..14].copy_from_slice(&ETHERTYPE);
    frame[14] = tag;
    frame[15..15 + payload.len()].copy_from_slice(payload);
    nic.send(&frame[..15 + payload.len()])
}

/// Set while `nic recv` waits: `poll` then queues every unhandled frame, not only the fos ethertype's.
static mut QUEUE_ALL: bool = false;

/// Handles whatever the NIC has received. Call this from the main loop.
pub fn poll() {
    let mut buffer = [0u8; 1600];
    for index in 0..nic_count() {
        loop {
            let Some(length) = nic_at(index).and_then(|nic| nic.receive(&mut buffer)) else { break };
            // Frames a tape may be about to want (its own load protocol, over the fos ethertype)
            // wait in the queue; the rest of a busy LAN's traffic is not worth keeping.
            if !handle_frame(index, &buffer[..length]) && length >= 14 && (buffer[12..14] == ETHERTYPE || unsafe { QUEUE_ALL }) {
                queue_frame(index, &buffer[..length]);
            }
        }
    }
    announce_service();
    if session_open() {
        flush();
    }
}

/// Seconds between a machine's unsolicited name announcements; the first few come faster so that a machine
/// that has just booted is known to the others (and knows them) within seconds.
const ANNOUNCE_SECONDS: u64 = 10;
static mut NEXT_ANNOUNCE: u64 = 0;
static mut ANNOUNCE_COUNT: u32 = 0;

/// Broadcasts this machine's name on every interface that has a link, the same frame a discovery probe is answered
/// with, so every fos machine on the segment learns it without asking (`handle_frame` files it in the peer table,
/// which tapes read with `peer_info`). Called from the main loop and from a running tape's service loop.
fn announce_service() {
    unsafe {
        if apic::ticks() < NEXT_ANNOUNCE {
            return;
        }
        ANNOUNCE_COUNT = ANNOUNCE_COUNT.saturating_add(1);
        NEXT_ANNOUNCE = apic::ticks() + if ANNOUNCE_COUNT <= 4 { 1 } else { ANNOUNCE_SECONDS } * apic::TICK_HZ;
    }
    for index in 0..nic_count() {
        if let Some(nic) = nic_at(index).filter(|nic| nic.link_up()) {
            let mut name = [0u8; 20];
            let length = hostname(index, &mut name);
            send(nic, &[0xff; 6], TAG_DISCOVER_REPLY, &name[..length]);
        }
    }
}

/// Handles one received frame if it is the terminal's or a discovery probe's business;
/// returns false for anything else (which a running tape may want).
fn handle_frame(index: usize, frame: &[u8]) -> bool {
    if frame.len() < 15 || frame[12..14] != ETHERTYPE {
        return false;
    }
    let source: [u8; 6] = frame[6..12].try_into().unwrap();
    match frame[14] {
        TAG_DISCOVER_REQUEST => {
            if let Some(nic) = nic_at(index) {
                let mut name = [0u8; 20];
                let length = hostname(index, &mut name);
                send(nic, &source, TAG_DISCOVER_REPLY, &name[..length]);
            }
        }
        // A name announcement (or the answer to a probe): remember who it is. A machine we had not heard of may
        // have booted after our last broadcast, so it is told our name straight away (it does the same for us once).
        TAG_DISCOVER_REPLY => {
            if remember_peer(index, source, netfs_name(&frame[15..])) {
                if let Some(nic) = nic_at(index) {
                    let mut name = [0u8; 20];
                    let length = hostname(index, &mut name);
                    send(nic, &source, TAG_DISCOVER_REPLY, &name[..length]);
                }
            }
            // Not consumed: a tape with its own address book (`net-chat`) still sees the frame.
            return false;
        }
        // NETFS is served at the prompt and in `term net`, never while a tape runs (the staging buffer is the tape's) or
        // when the settings file closed the network terminal.
        TAG_GET_REQUEST if netfs_enabled() => netfs_get_request(index, source, &frame[15..]),
        TAG_PUT_REQUEST if netfs_enabled() => netfs_put_request(index, source, &frame[15..]),
        TAG_PUT_DATA if netfs_enabled() => netfs_put_data(source, &frame[15..]),
        TAG_PUT_STOP if netfs_enabled() => netfs_put_stop(index, source),
        TAG_KUP_REQUEST if crate::config::debug().kernel_update => kup_request(index, source, &frame[15..]),
        TAG_KUP_DATA if crate::config::debug().kernel_update => kup_data(source, &frame[15..]),
        TAG_KUP_STOP if crate::config::debug().kernel_update => kup_stop(index, source),
        TAG_HELLO if crate::config::deny_term_net() => {} // closed by the settings file: stay silent
        // A session is open: another client's HELLO does not steal it (as in v1, where only the
        // waiting state accepts a new peer), unless the owner has been silent for five minutes.
        TAG_HELLO if session_open()
            && unsafe { *core::ptr::addr_of!(PEER) } != Some(source)
            && apic::ticks().wrapping_sub(unsafe { PEER_ACTIVITY }) < 5 * 60 * apic::TICK_HZ => {}
        TAG_HELLO => {
            // A new HELLO replaces any earlier session (the old client is gone or
            // restarting); the screen and serial port show who connected.
            unsafe {
                *core::ptr::addr_of_mut!(PEER) = Some(source);
                SESSION_NIC = index;
                PEER_ACTIVITY = apic::ticks();
                RELAY_LENGTH = 0;
            }
            crate::lineedit::reset();
            crate::println!("Remote terminal connected from {} on interface {}.", Mac(&source), index + 1);
            crate::println!("Type `help` for the commands.");
            prompt();
        }
        TAG_DATA if session_open() && unsafe { *core::ptr::addr_of!(PEER) } == Some(source) && unsafe { SESSION_NIC } == index => {
            // The frame has no length field and Ethernet pads a short frame to 60 bytes with zeros, so a single typed key
            // arrives followed by dozens of NULs. Drop the trailing zeros (as the chat and terminal code always did): a tape
            // that read them saw "any other key" right behind every key, and audio-net's peer picker closed the moment it
            // opened. A NUL a person really typed (Ctrl-@) is lost, which nobody misses.
            let payload = &frame[15..];
            let payload = &payload[..payload.iter().rposition(|&byte| byte != 0).map_or(0, |last| last + 1)];
            unsafe { PEER_ACTIVITY = apic::ticks() };
            if unsafe { TAPE_MODE } {
                // A running tape owns the input. A lone Escape byte is "stop".
                if payload.first() == Some(&0x1b) && payload.iter().rposition(|&b| b != 0) == Some(0) {
                    tape_input_push(TAPE_ABORT);
                } else {
                    for &byte in payload {
                        tape_input_push(u16::from(byte));
                    }
                }
            } else if unsafe { BLOCKING } {
                // A shell command that runs until interrupted (nic sniff...) owns the input: Escape stops it.
                if payload.contains(&0x1b) {
                    unsafe { BREAK = true };
                }
            } else {
                for &byte in payload {
                    feed(byte);
                }
            }
        }
        TAG_BYE if unsafe { *core::ptr::addr_of!(PEER) } == Some(source) => {
            flush();
            unsafe { *core::ptr::addr_of_mut!(PEER) = None };
            crate::println!("Remote terminal disconnected.");
            if unsafe { TAPE_MODE } {
                // Only the client that started the tape takes it down with it; a tape from the start file or the
                // local keyboard (`ptp master`) must outlive someone looking in over `term net`.
                if unsafe { *core::ptr::addr_of!(TAPE_OWNER) } == Some(source) {
                    tape_input_push(TAPE_ABORT);
                }
            } else if unsafe { BLOCKING } {
                // A command that runs until Escape (nic sniff...) has nobody left to stop it: end it as Escape would, so the
                // machine is not stuck in it with a client that is gone for good.
                unsafe { BREAK = true };
            } else {
                prompt();
            }
        }
        _ => return false,
    }
    true
}

// ---- KUP: the network kernel-update service ----
//
// Wire format (v1's, so `tools/send_kernel_update.py` works unchanged): REQUEST carries
// the image length (u32 LE) and its RFC 1071 checksum (u16 LE); DATA carries a sequence
// number (u32 LE) and up to 1024 bytes; STOP ends the transfer. The image is staged in
// RAM, verified (length, checksum), and only then written to the *inactive* kernel slot,
// which becomes the pending slot. The loader's A/B state machine gives it exactly one
// try; if it never reaches its milestone the machine reverts to the old kernel by
// itself. Only enabled by the `kernel-update` option in the `debug` file.

struct Kup {
    active: bool,
    source: [u8; 6],
    expected: usize,
    checksum: u16,
    received: usize,
    next_sequence: u32,
    last_activity: u64,
}

static mut KUP: Kup = Kup { active: false, source: [0; 6], expected: 0, checksum: 0, received: 0, next_sequence: 0, last_activity: 0 };
/// Tick at which to reset the machine after an accepted update (0 = none pending).
static mut RESET_AT: u64 = 0;

fn kup_reply(index: usize, destination: &[u8; 6], tag: u8, payload: &[u8]) {
    if let Some(nic) = nic_at(index) {
        send(nic, destination, tag, payload);
    }
}

fn netfs_enabled() -> bool {
    let tape_running = unsafe { TAPE_MODE };
    !tape_running && !crate::config::deny_term_net()
}

/// The name in a request: up to the first zero byte (frames are padded to the Ethernet minimum).
fn netfs_name(bytes: &[u8]) -> &[u8] {
    &bytes[..bytes.iter().position(|&byte| byte == 0).unwrap_or(bytes.len())]
}

struct NetfsPut {
    active: bool,
    source: [u8; 6],
    disk: usize,
    name: [u8; 16],
    name_length: usize,
    expected: usize,
    checksum: u16,
    received: usize,
    next_sequence: u32,
    last_activity: u64,
}

static mut NETFS_PUT: NetfsPut = NetfsPut { active: false, source: [0; 6], disk: 0, name: [0; 16], name_length: 0, expected: 0, checksum: 0, received: 0, next_sequence: 0, last_activity: 0 };

fn netfs_reply(index: usize, destination: &[u8; 6], tag: u8, payload: &[u8]) {
    if let Some(nic) = nic_at(index) {
        send(nic, destination, tag, payload);
    }
}

/// True if the staging buffer is in use by a transfer that is still alive (a PUT in progress, or a kernel update).
fn netfs_busy(source: &[u8; 6]) -> bool {
    let put = unsafe { &*core::ptr::addr_of!(NETFS_PUT) };
    let kup = unsafe { &*core::ptr::addr_of!(KUP) };
    (put.active && put.source != *source && apic::ticks().wrapping_sub(put.last_activity) < 500) || (kup.active && apic::ticks().wrapping_sub(kup.last_activity) < 500)
}

/// A request to read a file: answers with START, numbered DATA chunks and STOP, or an error code.
fn netfs_get_request(index: usize, source: [u8; 6], payload: &[u8]) {
    if payload.len() < 2 {
        return;
    }
    let name = netfs_name(&payload[1..]);
    let reply_error = |code: u8| netfs_reply(index, &source, TAG_GET_ERROR, &[code]);
    if netfs_busy(&source) {
        return reply_error(NETFS_BUSY);
    }
    if !crate::fosfs::valid_name(name) {
        return reply_error(NETFS_BAD_NAME);
    }
    let Ok(volume) = crate::storage::volume_on(usize::from(payload[0])) else { return reply_error(NETFS_NO_DISK) };
    let Some(staging) = crate::tape::staging() else { return reply_error(NETFS_TOO_LARGE) };
    let length = match volume.read(name, staging) {
        Ok(Some(0)) => return reply_error(NETFS_TOO_LARGE),
        Ok(Some(length)) => length,
        Ok(None) => return reply_error(NETFS_NOT_FOUND),
        Err(crate::fosfs::Error::TooBig) => return reply_error(NETFS_TOO_LARGE),
        Err(_) => return reply_error(NETFS_DISK_ERROR),
    };
    let checksum = fosfor_abi::internet_checksum(&staging[..length]);
    let mut start = [0u8; 6];
    start[0..4].copy_from_slice(&(length as u32).to_le_bytes());
    start[4..6].copy_from_slice(&checksum.to_le_bytes());
    netfs_reply(index, &source, TAG_GET_START, &start);
    crate::println!("NETFS: sending {} ({length} bytes) to {}.", core::str::from_utf8(name).unwrap_or("?"), Mac(&source));
    // The receiver reads from a finite ring: one chunk a millisecond is a pace real hardware keeps up with.
    let mut next = apic::micros();
    for (sequence, chunk) in staging[..length].chunks(NETFS_CHUNK).enumerate() {
        let mut frame = [0u8; 4 + NETFS_CHUNK];
        frame[0..4].copy_from_slice(&(sequence as u32).to_le_bytes());
        frame[4..4 + chunk.len()].copy_from_slice(chunk);
        while apic::micros() < next {
            core::hint::spin_loop();
        }
        next = apic::micros() + 1000;
        netfs_reply(index, &source, TAG_GET_DATA, &frame[..4 + chunk.len()]);
        crate::debug::pet();
    }
    netfs_reply(index, &source, TAG_GET_STOP, &[]);
}

/// A request to store a file. Acceptance is silent; a refusal is an error frame.
fn netfs_put_request(index: usize, source: [u8; 6], payload: &[u8]) {
    if payload.len() < 8 {
        return;
    }
    let reply_error = |code: u8| netfs_reply(index, &source, TAG_PUT_ERROR, &[code]);
    if netfs_busy(&source) {
        return reply_error(NETFS_BUSY);
    }
    let expected = u32::from_le_bytes(payload[1..5].try_into().unwrap()) as usize;
    let checksum = u16::from_le_bytes(payload[5..7].try_into().unwrap());
    let name = netfs_name(&payload[7..]);
    if !crate::fosfs::valid_name(name) {
        return reply_error(NETFS_BAD_NAME);
    }
    if crate::storage::volume_on(usize::from(payload[0])).is_err() {
        return reply_error(NETFS_NO_DISK);
    }
    if expected == 0 || expected > crate::tape::STAGE_BYTES || crate::tape::staging().is_none() {
        return reply_error(NETFS_TOO_LARGE);
    }
    let put = unsafe { &mut *core::ptr::addr_of_mut!(NETFS_PUT) };
    *put = NetfsPut { active: true, source, disk: usize::from(payload[0]), name: [0; 16], name_length: name.len(), expected, checksum, received: 0, next_sequence: 0, last_activity: apic::ticks() };
    put.name[..name.len()].copy_from_slice(name);
    crate::println!("NETFS: receiving {} ({expected} bytes) from {}.", core::str::from_utf8(name).unwrap_or("?"), Mac(&source));
}

fn netfs_put_data(source: [u8; 6], payload: &[u8]) {
    let put = unsafe { &mut *core::ptr::addr_of_mut!(NETFS_PUT) };
    if !put.active || put.source != source || payload.len() < 4 {
        return;
    }
    let sequence = u32::from_le_bytes(payload[0..4].try_into().unwrap());
    if sequence != put.next_sequence {
        put.active = false;
        return crate::println!("NETFS: chunk {sequence} arrived where {} was expected; transfer abandoned.", put.next_sequence);
    }
    let Some(staging) = crate::tape::staging() else { return };
    // The chunk's size comes from the declared length, never from the (padded) frame.
    let take = (payload.len() - 4).min(NETFS_CHUNK).min(put.expected - put.received);
    staging[put.received..put.received + take].copy_from_slice(&payload[4..4 + take]);
    put.received += take;
    put.next_sequence += 1;
    put.last_activity = apic::ticks();
}

fn netfs_put_stop(index: usize, source: [u8; 6]) {
    let put = unsafe { &mut *core::ptr::addr_of_mut!(NETFS_PUT) };
    if !put.active || put.source != source {
        return;
    }
    put.active = false;
    let Some(staging) = crate::tape::staging() else { return };
    let reply_error = |code: u8| netfs_reply(index, &source, TAG_PUT_ERROR, &[code]);
    if put.received != put.expected {
        crate::println!("NETFS: transfer ended after {} of {} bytes; abandoned.", put.received, put.expected);
        return reply_error(NETFS_CHECKSUM);
    }
    if fosfor_abi::internet_checksum(&staging[..put.expected]) != put.checksum {
        crate::println!("NETFS: checksum mismatch; file rejected.");
        return reply_error(NETFS_CHECKSUM);
    }
    let Ok(volume) = crate::storage::volume_on(put.disk) else { return reply_error(NETFS_NO_DISK) };
    match volume.write(&put.name[..put.name_length], &staging[..put.expected]) {
        Ok(()) => {
            crate::println!("NETFS: stored {} ({} bytes).", core::str::from_utf8(&put.name[..put.name_length]).unwrap_or("?"), put.expected);
            netfs_reply(index, &source, TAG_PUT_ACK, &[]);
        }
        Err(crate::fosfs::Error::DirectoryFull) => reply_error(NETFS_DIRECTORY_FULL),
        Err(crate::fosfs::Error::DataFull) => reply_error(NETFS_DATA_FULL),
        Err(crate::fosfs::Error::InvalidName) => reply_error(NETFS_BAD_NAME),
        Err(_) => reply_error(NETFS_DISK_ERROR),
    }
}

fn netfs_error_text(code: u8) -> &'static str {
    match code {
        NETFS_NOT_FOUND => "no such file there",
        NETFS_NO_DISK => "that disk is not available on the machine",
        NETFS_TOO_LARGE => "file too large for the machine's transfer buffer (4 MiB), or empty",
        NETFS_BAD_NAME => "invalid file name",
        NETFS_DIRECTORY_FULL => "its FOSFS directory is full",
        NETFS_DATA_FULL => "its FOSFS data area is full",
        NETFS_DISK_ERROR => "disk error on the machine",
        NETFS_CHECKSUM => "checksum mismatch (data was damaged in transit)",
        NETFS_BUSY => "busy: another transfer is in progress there",
        _ => "unknown error",
    }
}

/// `HOST[:diskN]` to the machine's MAC, the interface to reach it on and the disk number. HOST is a name from
/// `nic peers` (looked up with a discovery if the table has no such name yet), a MAC address, or a peer number.
fn netfs_peer(spec: &str) -> Option<([u8; 6], usize, usize)> {
    let (host, disk) = match spec.split_once(":disk") {
        Some((host, number)) => (host, number.parse::<usize>().ok()?),
        None => (spec, 1),
    };
    let parse_mac = |text: &str| -> Option<[u8; 6]> {
        let mut mac = [0u8; 6];
        let mut parts = text.split(':');
        for byte in mac.iter_mut() {
            *byte = u8::from_str_radix(parts.next()?, 16).ok()?;
        }
        parts.next().is_none().then_some(mac)
    };
    if let Some(mac) = parse_mac(host) {
        return Some((mac, unsafe { ACTIVE_NIC }.min(nic_count().saturating_sub(1)), disk));
    }
    let lookup = |host: &str| -> Option<([u8; 6], usize)> {
        let count = unsafe { PEER_COUNT };
        let peers = unsafe { &*core::ptr::addr_of!(PEERS) };
        if let Ok(number) = host.parse::<usize>() {
            return (number >= 1 && number <= count).then(|| (peers[number - 1].0, peers[number - 1].3));
        }
        peers[..count].iter().find(|peer| core::str::from_utf8(&peer.1[..peer.2]).is_ok_and(|name| name.eq_ignore_ascii_case(host))).map(|peer| (peer.0, peer.3))
    };
    if lookup(host).is_none() && host.parse::<usize>().is_err() {
        nic_discover();
    }
    lookup(host).map(|(mac, nic)| (mac, nic, disk))
}

/// Fetches a file from a peer into the staging buffer; returns its length.
fn netfs_fetch(mac: [u8; 6], nic_index: usize, disk: usize, name: &str) -> Result<usize, &'static str> {
    let staging = crate::tape::staging().ok_or("no memory for the transfer buffer")?;
    if netfs_busy(&mac) {
        return Err("a transfer into this machine is in progress");
    }
    let mut request = [0u8; 1 + 16];
    request[0] = disk as u8;
    request[1..1 + name.len()].copy_from_slice(name.as_bytes());
    netfs_reply(nic_index, &mac, TAG_GET_REQUEST, &request[..1 + name.len()]);
    let (mut total, mut checksum, mut received, mut sequence) = (None::<usize>, 0u16, 0usize, 0u32);
    let mut outcome: Option<Result<(), &'static str>> = None;
    let mut deadline = apic::ticks() + 3 * apic::TICK_HZ;
    blocking(true);
    while outcome.is_none() && apic::ticks() < deadline && !break_requested() {
        drain_nics(|_, frame| {
            if frame.len() < 15 || frame[12..14] != ETHERTYPE || frame[6..12] != mac {
                return;
            }
            let payload = &frame[15..];
            match frame[14] {
                TAG_GET_ERROR if !payload.is_empty() => outcome = Some(Err(netfs_error_text(payload[0]))),
                TAG_GET_START if total.is_none() && payload.len() >= 6 => {
                    let length = u32::from_le_bytes(payload[0..4].try_into().unwrap()) as usize;
                    if length == 0 || length > staging.len() {
                        outcome = Some(Err("the file does not fit the transfer buffer"));
                    } else {
                        total = Some(length);
                        checksum = u16::from_le_bytes(payload[4..6].try_into().unwrap());
                    }
                    deadline = apic::ticks() + 3 * apic::TICK_HZ;
                }
                TAG_GET_DATA if payload.len() >= 4 => {
                    if let Some(length) = total {
                        if u32::from_le_bytes(payload[0..4].try_into().unwrap()) != sequence {
                            outcome = Some(Err("lost a frame (there is no retransmission); try again"));
                        } else {
                            let take = (payload.len() - 4).min(NETFS_CHUNK).min(length - received);
                            staging[received..received + take].copy_from_slice(&payload[4..4 + take]);
                            received += take;
                            sequence += 1;
                            deadline = apic::ticks() + 3 * apic::TICK_HZ;
                        }
                    }
                }
                TAG_GET_STOP => {
                    outcome = Some(match total {
                        Some(length) if received == length && fosfor_abi::internet_checksum(&staging[..length]) == checksum => Ok(()),
                        Some(length) if received == length => Err("checksum mismatch; the data was damaged in transit"),
                        _ => Err("the transfer ended early"),
                    });
                }
                _ => {}
            }
        });
        crate::debug::pet();
        unsafe { core::arch::asm!("hlt") };
    }
    blocking(false);
    match outcome {
        Some(Ok(())) => Ok(total.unwrap_or(0)),
        Some(Err(reason)) => Err(reason),
        None => Err("no answer (is the machine at its prompt or in term net, and is the name right?)"),
    }
}

/// Sends the first `length` bytes of the staging buffer to a peer as a file.
fn netfs_store(mac: [u8; 6], nic_index: usize, disk: usize, name: &str, length: usize) -> Result<(), &'static str> {
    let staging = crate::tape::staging().ok_or("no memory for the transfer buffer")?;
    let checksum = fosfor_abi::internet_checksum(&staging[..length]);
    let mut request = [0u8; 7 + 16];
    request[0] = disk as u8;
    request[1..5].copy_from_slice(&(length as u32).to_le_bytes());
    request[5..7].copy_from_slice(&checksum.to_le_bytes());
    request[7..7 + name.len()].copy_from_slice(name.as_bytes());
    netfs_reply(nic_index, &mac, TAG_PUT_REQUEST, &request[..7 + name.len()]);
    blocking(true);
    let mut refusal: Option<&'static str> = None;
    // A refusal arrives at once; acceptance is silent.
    let until = apic::ticks() + apic::TICK_HZ / 2;
    while apic::ticks() < until && refusal.is_none() {
        drain_nics(|_, frame| {
            if frame.len() >= 16 && frame[12..14] == ETHERTYPE && frame[6..12] == mac && frame[14] == TAG_PUT_ERROR {
                refusal = Some(netfs_error_text(frame[15]));
            }
        });
        unsafe { core::arch::asm!("hlt") };
    }
    if let Some(reason) = refusal {
        blocking(false);
        return Err(reason);
    }
    let mut next = apic::micros();
    for (sequence, chunk) in staging[..length].chunks(NETFS_CHUNK).enumerate() {
        let mut frame = [0u8; 4 + NETFS_CHUNK];
        frame[0..4].copy_from_slice(&(sequence as u32).to_le_bytes());
        frame[4..4 + chunk.len()].copy_from_slice(chunk);
        while apic::micros() < next {
            core::hint::spin_loop();
        }
        next = apic::micros() + 1000;
        netfs_reply(nic_index, &mac, TAG_PUT_DATA, &frame[..4 + chunk.len()]);
        crate::debug::pet();
        if break_requested() {
            blocking(false);
            return Err("interrupted");
        }
    }
    netfs_reply(nic_index, &mac, TAG_PUT_STOP, &[]);
    let mut result: Option<Result<(), &'static str>> = None;
    let deadline = apic::ticks() + 10 * apic::TICK_HZ;
    while result.is_none() && apic::ticks() < deadline && !break_requested() {
        drain_nics(|_, frame| {
            if frame.len() >= 15 && frame[12..14] == ETHERTYPE && frame[6..12] == mac {
                match frame[14] {
                    TAG_PUT_ACK => result = Some(Ok(())),
                    TAG_PUT_ERROR if frame.len() >= 16 => result = Some(Err(netfs_error_text(frame[15]))),
                    _ => {}
                }
            }
        });
        unsafe { core::arch::asm!("hlt") };
    }
    blocking(false);
    result.unwrap_or(Err("no reply (a frame may have been lost: there is no retransmission)"))
}

fn kup_request(index: usize, source: [u8; 6], payload: &[u8]) {
    let kup = unsafe { &mut *core::ptr::addr_of_mut!(KUP) };
    // A transfer from someone else that is still alive keeps the staging buffer.
    if kup.active && kup.source != source && apic::ticks().wrapping_sub(kup.last_activity) < 500 {
        return kup_reply(index, &source, TAG_KUP_ERROR, &[KUP_BUSY]);
    }
    if payload.len() < 6 {
        return;
    }
    let expected = u32::from_le_bytes(payload[0..4].try_into().unwrap()) as usize;
    let checksum = u16::from_le_bytes(payload[4..6].try_into().unwrap());
    if expected == 0 || expected > crate::tape::STAGE_BYTES || expected.div_ceil(512) as u64 > fosfor_abi::SLOT_SECTORS || crate::tape::staging().is_none() {
        kup.active = false;
        return kup_reply(index, &source, TAG_KUP_ERROR, &[KUP_TOO_LARGE]);
    }
    *kup = Kup { active: true, source, expected, checksum, received: 0, next_sequence: 0, last_activity: apic::ticks() };
    crate::println!("KUP: receiving a kernel image of {expected} bytes from {}.", Mac(&source));
}

fn kup_data(source: [u8; 6], payload: &[u8]) {
    let kup = unsafe { &mut *core::ptr::addr_of_mut!(KUP) };
    if !kup.active || kup.source != source || payload.len() < 4 {
        return;
    }
    let sequence = u32::from_le_bytes(payload[0..4].try_into().unwrap());
    if sequence != kup.next_sequence {
        // A gap or a reorder abandons the transfer; there is no retransmission.
        kup.active = false;
        return crate::println!("KUP: chunk {sequence} arrived where {} was expected; transfer abandoned.", kup.next_sequence);
    }
    let Some(staging) = crate::tape::staging() else { return };
    // The chunk's size comes from the declared length, never from the (padded) frame.
    let take = (payload.len() - 4).min(1024).min(kup.expected - kup.received);
    staging[kup.received..kup.received + take].copy_from_slice(&payload[4..4 + take]);
    kup.received += take;
    kup.next_sequence += 1;
    kup.last_activity = apic::ticks();
}

fn kup_stop(index: usize, source: [u8; 6]) {
    let kup = unsafe { &mut *core::ptr::addr_of_mut!(KUP) };
    if !kup.active || kup.source != source {
        return;
    }
    kup.active = false;
    let Some(staging) = crate::tape::staging() else { return };
    if kup.received != kup.expected {
        return crate::println!("KUP: transfer ended after {} of {} bytes; abandoned.", kup.received, kup.expected);
    }
    // Verify before writing anything: a mismatch leaves the running kernel and the slot
    // metadata completely untouched.
    if fosfor_abi::internet_checksum(&staging[..kup.expected]) != kup.checksum {
        crate::println!("KUP: checksum mismatch; image rejected.");
        return kup_reply(index, &source, TAG_KUP_ERROR, &[KUP_CHECKSUM_MISMATCH]);
    }
    let (Some(metadata), Some(disk), Some(payload)) = (crate::debug::read_metadata(), crate::storage::boot_disk(), crate::storage::payload_start()) else {
        return kup_reply(index, &source, TAG_KUP_ERROR, &[KUP_DISK_ERROR]);
    };
    let slot = 1 - (metadata.active_slot & 1);
    let sectors = kup.expected.div_ceil(512);
    staging[kup.expected..sectors * 512].fill(0);
    if crate::block::write(disk, payload + fosfor_abi::slot_start(u64::from(slot)), &staging[..sectors * 512]).is_err() {
        crate::println!("KUP: writing slot {slot} failed.");
        return kup_reply(index, &source, TAG_KUP_ERROR, &[KUP_DISK_ERROR]);
    }
    let updated = crate::debug::update_metadata(|m| {
        m.pending_slot = Some(slot);
        m.attempted = false;
        m.confirmed = false;
        m.slot_sectors[usize::from(slot)] = sectors as u16;
    });
    if updated.is_none() {
        return kup_reply(index, &source, TAG_KUP_ERROR, &[KUP_DISK_ERROR]);
    }
    crate::println!("KUP: image verified and written to slot {slot} (pending); restarting in about a second to try it.");
    kup_reply(index, &source, TAG_KUP_ACK, &[]);
    flush();
    unsafe { RESET_AT = apic::ticks() + apic::TICK_HZ };
}

/// Main-loop work: restart after an accepted kernel update.
pub fn housekeeping() {
    unsafe {
        if RESET_AT != 0 && apic::ticks() >= RESET_AT {
            RESET_AT = 0;
            crate::debug::mark_clean_stop();
            power::reset(RUNTIME_SERVICES);
        }
    }
}

// ---- support for running tapes (see tape.rs) ----

const TAPE_ABORT: u16 = 0x100;
const TAPE_INPUT_CAPACITY: usize = 256;
static mut TAPE_MODE: bool = false;
/// The `term net` client that was connected when the running tape started; None for a tape of the start file or the keyboard.
static mut TAPE_OWNER: Option<[u8; 6]> = None;
static mut TAPE_INPUT: [u16; TAPE_INPUT_CAPACITY] = [0; TAPE_INPUT_CAPACITY];
static mut TAPE_INPUT_HEAD: usize = 0;
static mut TAPE_INPUT_TAIL: usize = 0;

fn tape_input_push(value: u16) {
    unsafe {
        let next = (TAPE_INPUT_HEAD + 1) % TAPE_INPUT_CAPACITY;
        if next != TAPE_INPUT_TAIL {
            (*core::ptr::addr_of_mut!(TAPE_INPUT))[TAPE_INPUT_HEAD] = value;
            TAPE_INPUT_HEAD = next;
        }
    }
}

/// While a tape runs, input goes to the tape instead of the line editor.
pub fn set_tape_mode(on: bool) {
    unsafe {
        TAPE_MODE = on;
        *core::ptr::addr_of_mut!(TAPE_OWNER) = if on { *core::ptr::addr_of!(PEER) } else { None };
        TAPE_INPUT_HEAD = 0;
        TAPE_INPUT_TAIL = 0;
    }
    crate::serial::defer(on);
    if !on {
        flush();
    }
}

/// Frames that were not the terminal's, kept per interface for the tape's `nic_receive`
/// (a short FIFO: a tape that does not poll loses the oldest traffic, like a NIC ring
/// overrun).
const FRAME_FIFO: usize = 64;
static mut FRAMES: [[u8; 1600]; MAX_NICS * FRAME_FIFO] = [[0; 1600]; MAX_NICS * FRAME_FIFO];
static mut FRAME_LENGTHS: [usize; MAX_NICS * FRAME_FIFO] = [0; MAX_NICS * FRAME_FIFO];
static mut FRAME_HEAD: [usize; MAX_NICS] = [0; MAX_NICS];
static mut FRAME_TAIL: [usize; MAX_NICS] = [0; MAX_NICS];
static mut FRAMES_DROPPED: u32 = 0;
static mut FRAMES_QUEUED: u32 = 0;

/// Keeps a frame that is not the terminal's for a tape (`tape load net` listens for its own
/// protocol, `nic_receive` for everything else). Frames that arrive before a tape is
/// listening wait here instead of being lost.
fn queue_frame(index: usize, frame: &[u8]) {
    unsafe {
        let head = FRAME_HEAD[index];
        let next = (head + 1) % FRAME_FIFO;
        if next == FRAME_TAIL[index] {
            FRAME_TAIL[index] = (FRAME_TAIL[index] + 1) % FRAME_FIFO; // full: drop the oldest
            FRAMES_DROPPED += 1;
        }
        let slot = index * FRAME_FIFO + head;
        core::slice::from_raw_parts_mut((core::ptr::addr_of_mut!(FRAMES) as *mut u8).add(slot * 1600), frame.len()).copy_from_slice(frame);
        FRAME_LENGTHS[slot] = frame.len();
        FRAME_HEAD[index] = next;
        FRAMES_QUEUED += 1;
    }
}

/// Forgets frames queued for tapes (a tape about to run should not see stale traffic).
pub fn clear_frame_queue() {
    unsafe {
        FRAME_HEAD = [0; MAX_NICS];
        FRAME_TAIL = [0; MAX_NICS];
    }
}

/// Everything a tape callback must keep serviced: USB keyboards, local keys, the
/// network (terminal frames handled, the rest queued per interface), console output out.
/// Drains whatever a running tape's loop must keep draining (input, the network, USB events).
pub fn tape_poll() {
    tape_service();
}

fn tape_service() {
    crate::debug::pet();
    crate::usb_hid::poll();
    while let Some(key) = crate::keyboard::pop() {
        if key == 0x1b {
            tape_input_push(TAPE_ABORT);
        } else if let Some(sequence) = fosfor_abi::keymap::ansi_sequence(key) {
            // A cursor key (a private code from the keyboard decoders): the tape sees the bytes a
            // terminal would send, ESC [ A for Up and so on, like a `term net` client's keys.
            tape_input_push(0x1b);
            tape_input_push(u16::from(b'['));
            for &byte in sequence {
                tape_input_push(u16::from(byte));
            }
        } else {
            tape_input_push(u16::from(key));
        }
    }
    let mut buffer = [0u8; 1600];
    for index in 0..nic_count() {
        loop {
            let Some(length) = nic_at(index).and_then(|nic| nic.receive(&mut buffer)) else { break };
            if handle_frame(index, &buffer[..length]) {
                continue;
            }
            queue_frame(index, &buffer[..length]);
        }
    }
    crate::uart::poll();
    announce_service();
    flush();
    crate::serial::pump();
}

/// A tape's `read_key`: 0 none, 1 abort, 0x100 + byte.
pub fn tape_key() -> u32 {
    tape_service();
    unsafe {
        if TAPE_INPUT_TAIL == TAPE_INPUT_HEAD {
            return 0;
        }
        let value = (*core::ptr::addr_of!(TAPE_INPUT))[TAPE_INPUT_TAIL];
        TAPE_INPUT_TAIL = (TAPE_INPUT_TAIL + 1) % TAPE_INPUT_CAPACITY;
        if value == TAPE_ABORT { 1 } else { 0x100 + u32::from(value) }
    }
}

/// A tape's `should_abort`: drains input, true if the operator asked to stop.
pub fn tape_should_abort() -> bool {
    loop {
        match tape_key() {
            0 => return false,
            1 => return true,
            _ => {}
        }
    }
}

/// A tape's `nic_receive`: the next queued frame of interface `index` that is not the
/// terminal's.
pub fn tape_receive(index: usize, out: &mut [u8]) -> usize {
    tape_service();
    pop_frame(index, out)
}

/// The oldest queued non-terminal frame of interface `index`, copied into `out`.
fn pop_frame(index: usize, out: &mut [u8]) -> usize {
    if index >= MAX_NICS {
        return 0;
    }
    unsafe {
        if FRAME_TAIL[index] == FRAME_HEAD[index] {
            return 0;
        }
        let slot = index * FRAME_FIFO + FRAME_TAIL[index];
        let length = FRAME_LENGTHS[slot].min(out.len());
        out[..length].copy_from_slice(core::slice::from_raw_parts((core::ptr::addr_of!(FRAMES) as *const u8).add(slot * 1600), length));
        FRAME_TAIL[index] = (FRAME_TAIL[index] + 1) % FRAME_FIFO;
        length
    }
}

pub fn nic_mac(index: usize, out: &mut [u8; 6]) -> bool {
    match nic_at(index) {
        Some(nic) => {
            *out = nic.mac();
            true
        }
        None => false,
    }
}

pub fn nic_send(index: usize, frame: &[u8]) -> bool {
    nic_at(index).is_some_and(|nic| nic.send(frame))
}

pub fn nic_set_promiscuous(index: usize, on: bool) -> bool {
    match nic_at(index) {
        Some(nic) => {
            nic.set_promiscuous(on);
            true
        }
        None => false,
    }
}

pub fn nic_set_multicast(index: usize, on: bool) -> bool {
    match nic_at(index) {
        Some(nic) => {
            nic.set_multicast(on);
            true
        }
        None => false,
    }
}

/// Flushes queued console output to the client (public for tape callbacks).
pub fn flush_output() {
    flush();
}

/// Runs one command line (used by the `start` script).
pub fn run_line(line: &str) {
    execute(line);
}

/// Starts the local shell: prints the prompt for a keyboard at the machine.
pub fn start_local() {
    prompt();
}

/// A key typed at the machine's own keyboard. It shares the line editor with the
/// network session (mixing the two on one line would garble it, which is fine for a
/// lab machine with one operator at a time).
pub fn local_input(byte: u8) {
    feed(byte);
}

pub struct Mac<'a>(pub &'a [u8; 6]);
impl core::fmt::Display for Mac<'_> {
    fn fmt(&self, formatter: &mut core::fmt::Formatter) -> core::fmt::Result {
        let b = self.0;
        write!(formatter, "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", b[0], b[1], b[2], b[3], b[4], b[5])
    }
}

/// `start`: runs the `start` file again, as the boot does. A start file that itself says `start` would never end, so inside a run it is refused.
fn start_command() {
    static RUNNING: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
    if RUNNING.swap(true, core::sync::atomic::Ordering::Relaxed) {
        crate::println!("start: the start file is already running.");
        return;
    }
    if !crate::config::run_start(run_line) {
        crate::println!("start: no start file on a FOSFS volume (or the FAT/exFAT root).");
    }
    RUNNING.store(false, core::sync::atomic::Ordering::Relaxed);
}

fn prompt() {
    crate::print(format_args!("fos> "));
}

/// One typed byte: the line editor (`lineedit.rs`) handles the keys; a finished line runs here.
fn feed(byte: u8) {
    let mut line = [0u8; crate::lineedit::LINE_MAX];
    if let Some(length) = crate::lineedit::feed(byte, &mut line) {
        if let Ok(text) = core::str::from_utf8(&line[..length]) {
            execute(text.trim());
        }
        prompt();
    }
}

/// The longest script a file may hold, and how deep scripts may call scripts.
const SCRIPT_CAPACITY: usize = 4096;
const MAX_SCRIPT_DEPTH: usize = 4;
static mut SCRIPT_DEPTH: usize = 0;
static mut SCRIPT_TEXT: [[u8; SCRIPT_CAPACITY]; MAX_SCRIPT_DEPTH] = [[0; SCRIPT_CAPACITY]; MAX_SCRIPT_DEPTH];

/// Finds a file by name on the boot volume first, then on the other disks.
fn find_file(name: &[u8]) -> Option<crate::fosfs::Volume> {
    crate::storage::volume_with_file(name)
}

/// A command word that is not built in: `NAME.sh` runs as a script (lines of commands, `#` starts a comment, `$1`..`$9`
/// are the arguments and `$*` all of them), else the tape `NAME` runs. False if there is neither.
fn external_command(word: &str, rest: &str) -> bool {
    // `diskN/NAME` and `@LABEL/NAME` name the volume to run from.
    if let Some((volume_text, file)) = split_volume(word) {
        let Some(Ok((_, volume))) = volume_word(volume_text) else { return false };
        let base = file.strip_suffix(".sh").unwrap_or(file);
        let mut script = [0u8; 24];
        if base.len() + 3 <= script.len() {
            script[..base.len()].copy_from_slice(base.as_bytes());
            script[base.len()..base.len() + 3].copy_from_slice(b".sh");
            let script = &script[..base.len() + 3];
            if crate::fosfs::valid_name(script) && matches!(volume.file_length(script), Ok(Some(_))) {
                run_script(volume, script, base, rest);
                return true;
            }
        }
        if !file.contains('.') && matches!(volume.file_length(file.as_bytes()), Ok(Some(_))) {
            let mut line = [0u8; 160];
            let text = if rest.is_empty() { word.len() } else { word.len() + 1 + rest.len() };
            if text <= line.len() {
                line[..word.len()].copy_from_slice(word.as_bytes());
                if !rest.is_empty() {
                    line[word.len()] = b' ';
                    line[word.len() + 1..text].copy_from_slice(rest.as_bytes());
                }
                crate::tape::run_named(core::str::from_utf8(&line[..text]).unwrap_or(word));
            }
            return true;
        }
        return false;
    }
    // `NAME` and `NAME.sh` both mean the script file NAME.sh.
    let base = word.strip_suffix(".sh").unwrap_or(word);
    let mut file_name = [0u8; 24];
    if base.len() + 3 > file_name.len() {
        return false;
    }
    file_name[..base.len()].copy_from_slice(base.as_bytes());
    file_name[base.len()..base.len() + 3].copy_from_slice(b".sh");
    let script_name = &file_name[..base.len() + 3];
    if crate::fosfs::valid_name(script_name) {
        if let Some(volume) = find_file(script_name) {
            run_script(volume, script_name, base, rest);
            return true;
        }
    }
    // A file with no dot in its name is taken for a tape (data files such as `readme.txt` carry a suffix); if it is not a
    // usable tape `run` says so.
    if !word.contains('.') && crate::fosfs::valid_name(word.as_bytes()) && find_file(word.as_bytes()).is_some() {
        let mut line = [0u8; 160];
        let text = if rest.is_empty() { word.len() } else { word.len() + 1 + rest.len() };
        if text <= line.len() {
            line[..word.len()].copy_from_slice(word.as_bytes());
            if !rest.is_empty() {
                line[word.len()] = b' ';
                line[word.len() + 1..text].copy_from_slice(rest.as_bytes());
            }
            crate::tape::run_named(core::str::from_utf8(&line[..text]).unwrap_or(word));
        }
        return true;
    }
    false
}

/// Runs a script file line by line, echoing each command as `NAME.sh> command`.
fn run_script(volume: crate::fosfs::Volume, file: &[u8], name: &str, arguments: &str) {
    let depth = unsafe { SCRIPT_DEPTH };
    if depth >= MAX_SCRIPT_DEPTH {
        crate::println!("{name}.sh: scripts are nested too deep (limit {MAX_SCRIPT_DEPTH}).");
        return;
    }
    let buffer = unsafe { &mut (*(&raw mut SCRIPT_TEXT))[depth] };
    let length = match volume.read(file, buffer) {
        Ok(Some(length)) => length,
        Ok(None) => return,
        Err(_) => {
            crate::println!("{name}.sh: cannot read it (over {SCRIPT_CAPACITY} bytes?).");
            return;
        }
    };
    let Ok(text) = core::str::from_utf8(&buffer[..length]) else {
        crate::println!("{name}.sh: not text.");
        return;
    };
    // The script text lives in this depth's buffer for the whole run: commands may read other files, nested scripts
    // use the next buffer.
    let text: &'static str = unsafe { core::mem::transmute::<&str, &'static str>(text) };
    unsafe { SCRIPT_DEPTH = depth + 1 };
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut expanded = [0u8; 240];
        let mut used = 0;
        let mut chars = line.chars().peekable();
        while let Some(character) = chars.next() {
            let substitution: Option<&str> = if character == '$' {
                match chars.peek().copied() {
                    Some('*') => {
                        chars.next();
                        Some(arguments)
                    }
                    Some(digit @ '1'..='9') => {
                        chars.next();
                        Some(arguments.split_whitespace().nth(digit as usize - '1' as usize).unwrap_or(""))
                    }
                    _ => None,
                }
            } else {
                None
            };
            let mut utf8 = [0u8; 4];
            let piece = match substitution {
                Some(text) => text,
                None => character.encode_utf8(&mut utf8),
            };
            if used + piece.len() > expanded.len() {
                break;
            }
            expanded[used..used + piece.len()].copy_from_slice(piece.as_bytes());
            used += piece.len();
        }
        let command = core::str::from_utf8(&expanded[..used]).unwrap_or("").trim();
        if command.is_empty() {
            continue;
        }
        crate::println!("{name}.sh> {command}");
        execute(command);
    }
    unsafe { SCRIPT_DEPTH = depth };
}

fn execute(command: &str) {
    let mut words = command.split_whitespace();
    let Some(word) = words.next() else { return };
    if word != "format" {
        unsafe { PENDING_FORMAT = None }; // a format needs its confirmation as the very next command
    }
    if word != "install" {
        crate::install::cancel(); // and so does an install
    }
    if word != "bootcode" {
        crate::bootcode::cancel(); // and a boot-code update
    }
    match word {
        "help" if words.clone().next().is_some() => {
            // `help NAME` shows the help screen a tape file carries (the same text as `NAME help`).
            crate::tape::show_help(words.next().unwrap_or(""));
        }
        "help" => {
            crate::println!("help      this list (help NAME or NAME --help: a tape's usage)");
            crate::println!("about     what this is");
            crate::println!("memory    physical memory statistics");
            crate::println!("pci       list PCI functions (pci read|write B:D.F ... reaches config space)");
            crate::println!("cpuinfo bootinfo memmap irqstat   what the CPU, loader and firmware report");
            crate::println!("hexdump ADDRESS [LENGTH]   dump mapped memory; io r8|w8.. PORT; mmio r32|w32.. ADDR");
            crate::println!("clear     clear the screen");
            crate::println!("nic       the network interfaces (list); nic use|sniff|discover|peers|map|regs|send|recv");
            crate::println!("disks     disks found by the drivers (the boot disk is disk1)");
            crate::println!("videomodes   the screen modes the BIOS boot path's video BIOS offered (and what the monitor told)");
            crate::println!("bootcode [update] [diskN]   show or replace the BIOS boot code and the UEFI loader (files on disk1)");
            crate::println!("sector diskN LBA [COUNT]   raw read-only dump of disk sectors (a GPT header, the slot metadata)");
            crate::println!("label [diskN|@LABEL [NAME|-]]   show or set a volume label");
            crate::println!("format diskN [NAME] [force]   erase a disk's FOSFS volume (asks to confirm)");
            crate::println!("format diskN exfat [LABEL]    erase a disk and make it one exFAT volume for PCs");
            crate::println!("grow diskN        make a disk's FOSFS volume fill the disk (files are kept)");
            crate::println!("install diskN     copy the boot disk (ESP, kernel, files) onto another disk");
            crate::println!("copy SRC NAME DST [NEWNAME]   copy a file between volumes (diskN or @LABEL;");
            crate::println!("          a FAT32/exFAT disk too, by its diskN: ls cat write rm copy)");
            crate::println!("ls [diskN]        list FOSFS files (a FAT32/exFAT disk too, if named)");
            crate::println!("start     run the start file again (as at boot)");
            crate::println!("date [set YYYY-MM-DD HH:MM]   the clock (local time; the RTC)");
            crate::println!("cat [diskN] NAME  show a file");
            crate::println!("write [diskN] NAME TEXT   create or replace a file");
            crate::println!("rm [diskN] NAME   delete a file");
            crate::println!("tape ...  load NAME|net, run [ARGS], save NAME, status");
            crate::println!("NAME [ARGS]   a command that is not built in runs the script NAME.sh ($1..$9, $*, # comments) or the tape NAME");
            crate::println!("run NAME [ARGS]   load a tape from FOSFS and run it");
            crate::println!("usb       USB controllers, devices, keyboards (usb rescan)");
            crate::println!("uptime    time since the timer started; sleep N waits N seconds");
            crate::println!("dmesg     the boot log");
            crate::println!("shutdown  power off");
            crate::println!("reset     restart through the firmware (reboot is the same)");
            crate::println!("debug ... status | slots | crash | hang");
            crate::println!("Remote: term net, and tools/term_net.py, devloop.py, foscopy.py from a PC.");
        }
        "about" => {
            crate::println!("Fosfor OS (fos) is a small educational 64-bit x86 operating system.");
            crate::println!("One kernel, booted by a UEFI loader or by the BIOS boot code.");
            crate::println!("Design: docs/design.md.");
        }
        "memory" => {
            let stats = memory::stats();
            crate::println!("Managed:   {} MiB ({} frames of 4 KiB)", stats.frames * memory::FRAME / (1024 * 1024), stats.frames);
            crate::println!("Free:      {} MiB ({} frames)", stats.free * memory::FRAME / (1024 * 1024), stats.free);
        }
        "pci" if crate::diag::pci_access(words.clone()) => {}
        "pci" => {
            for device in pci::devices() {
                crate::println!(
                    "{:02x}:{:02x}.{} {:04x}:{:04x} {}{}{}",
                    device.bus,
                    device.device,
                    device.function,
                    device.vendor,
                    device.id,
                    pci::class_name(device.class, device.subclass),
                    if device.find_capability(0x05).is_some() { " msi" } else { "" },
                    if device.find_capability(0x11).is_some() { " msi-x" } else { "" }
                );
            }
        }
        "nic" if matches!(words.clone().next(), Some("use")) => {
            words.next();
            nic_use(words.next());
        }
        "nic" if matches!(words.clone().next(), Some("sniff")) => {
            words.next();
            nic_sniff(words.next());
        }
        "nic" if matches!(words.clone().next(), Some("discover")) => nic_discover(),
        "nic" if matches!(words.clone().next(), Some("peers")) => print_peers(),
        "nic" if matches!(words.clone().next(), Some("map")) => nic_map(),
        "nic" if matches!(words.clone().next(), Some("send")) => nic_send_test(),
        "nic" if matches!(words.clone().next(), Some("recv")) => nic_recv_test(),
        "cpuinfo" => crate::diag::cpuinfo(),
        "bootinfo" => crate::diag::bootinfo(),
        "memmap" => crate::diag::memmap(),
        "irqstat" => crate::diag::irqstat(),
        "io" => crate::diag::io(words),
        "mmio" => crate::diag::mmio(words),
        "clear" => crate::print(format_args!("\x1b[2J")),
        "hexdump" => {
            let address = words.next().and_then(parse_number);
            let length = words.next().and_then(parse_number).unwrap_or(64);
            match address {
                Some(address) if length <= 4096 => hexdump(address, length as usize),
                _ => crate::println!("Usage: hexdump ADDRESS [LENGTH <= 4096]  (decimal or 0x hex)"),
            }
        }
        "sector" => {
            // `sector diskN LBA [COUNT]`: a raw read-only dump, to look at what is really on a disk (a GPT header, the slot
            // metadata, a boot sector) when a driver or loader misbehaves on hardware we cannot attach a debugger to.
            const USAGE: &str = "Usage: sector diskN LBA [COUNT <= 4]   (decimal or 0x hex; read-only)";
            let disk = words.next().and_then(|word| word.strip_prefix("disk")).and_then(|digits| digits.parse::<usize>().ok());
            let lba = words.next().and_then(parse_number);
            let count = words.next().and_then(parse_number).unwrap_or(1);
            let (Some(number), Some(lba)) = (disk, lba) else { return crate::println!("{USAGE}") };
            if number == 0 || count == 0 || count > 4 {
                return crate::println!("{USAGE}");
            }
            for offset in 0..count {
                let mut sector = [0u8; 512];
                if let Err(error) = crate::block::read(number - 1, lba + offset, &mut sector) {
                    return crate::println!("disk{number} sector {}: read failed ({error:?}).", lba + offset);
                }
                crate::println!("disk{number} sector {}:", lba + offset);
                let mut skipped = false;
                for (row, bytes) in sector.chunks(16).enumerate() {
                    // runs of identical rows (zeros, mostly) are folded into one "*" line, as hexdump does
                    if row > 0 && row < 31 && bytes == &sector[(row - 1) * 16..row * 16] && bytes == &sector[(row + 1) * 16..(row + 2) * 16] {
                        if !skipped {
                            crate::println!("*");
                            skipped = true;
                        }
                        continue;
                    }
                    skipped = false;
                    crate::print(format_args!("{:03x}  ", row * 16));
                    for byte in bytes {
                        crate::print(format_args!("{byte:02x} "));
                    }
                    crate::print(format_args!(" "));
                    for &byte in bytes {
                        crate::print(format_args!("{}", if (0x20..0x7f).contains(&byte) { byte as char } else { '.' }));
                    }
                    crate::println!();
                }
            }
        }
        "nic" if matches!(words.clone().next(), Some("regs")) => {
            words.next();
            let wanted = words.next().and_then(|word| word.parse::<usize>().ok());
            for index in 0..nic_count() {
                if wanted.is_some_and(|number| number != index + 1) {
                    continue;
                }
                if let Some(nic) = nic_at(index) {
                    crate::println!("{}: {} {}", index + 1, nic.family(), Mac(&nic.mac()));
                    nic.dump_registers();
                }
            }
        }
        "nic" => {
            if nic_count() == 0 {
                crate::println!("No network interface.");
            }
            for index in 0..nic_count() {
                if let Some(nic) = nic_at(index) {
                    crate::println!("{}: {} {}  {}{}", index + 1, nic.family(), Mac(&nic.mac()), if nic.link_up() { "link up" } else { "link DOWN" }, if unsafe { SESSION_NIC } == index && session_open() { "  (terminal)" } else { "" });
                    let (received, sent, dropped) = nic.counters();
                    crate::println!("   frames: {} received, {} sent{}", received, sent, if dropped != 0 { " (some dropped: no link)" } else { "" });
                    // v1's `nic` reported "NIC IRQn: N notifications"; keep that line (tools grep for it).
                    if let crate::e1000::Delivery::Msi(v) | crate::e1000::Delivery::Msix(v) | crate::e1000::Delivery::Intx { vector: v, .. } = nic.delivery() {
                        crate::println!("   NIC IRQ{v}: {} notifications", crate::e1000::ISR[index].interrupts.load(core::sync::atomic::Ordering::Relaxed));
                    }
                    match nic.delivery() {
                        crate::e1000::Delivery::Polled => crate::println!("   interrupts: polled"),
                        crate::e1000::Delivery::Intx { gsi, vector } => {
                            let state = &crate::e1000::ISR[index];
                            crate::println!(
                                "   interrupts: INTx pin {gsi}, vector {vector:#x}: {} taken ({} storms masked)",
                                state.interrupts.load(core::sync::atomic::Ordering::Relaxed),
                                crate::msi::storms()
                            );
                        }
                        crate::e1000::Delivery::Msi(vector) | crate::e1000::Delivery::Msix(vector) => {
                            let state = &crate::e1000::ISR[index];
                            crate::println!(
                                "   interrupts: {} vector {vector:#x}: {} taken, {} receive overruns, {} link changes",
                                if matches!(nic.delivery(), crate::e1000::Delivery::Msix(_)) { "MSI-X" } else { "MSI" },
                                state.interrupts.load(core::sync::atomic::Ordering::Relaxed),
                                state.overruns.load(core::sync::atomic::Ordering::Relaxed),
                                state.link_changes.load(core::sync::atomic::Ordering::Relaxed)
                            );
                        }
                    }
                    nic.describe();
                    if index == 0 {
                        crate::println!("   tape frame queue: {} queued, {} dropped", unsafe { FRAMES_QUEUED }, unsafe { FRAMES_DROPPED });
                    }
                }
            }
        }
        "tape" => crate::tape::command(command.strip_prefix("tape").unwrap_or("")),
        "run" => crate::tape::run_named(command.strip_prefix("run").unwrap_or("")),
        "label" => label_command(words),
        "audio" => audio_command(words),
        "hda" => crate::hda::describe(),
        "format" => format_command(words),
        "grow" => grow_command(words),
        "install" => crate::install::command(words),
        "bootcode" => crate::bootcode::command(words),
        "videomodes" => videomodes_command(),
        "copy" => copy_command(words),
        "disks" => {
            for index in 0..crate::block::count() {
                let disk = crate::block::get(index).unwrap();
                crate::println!(
                    "disk{}: {} MiB {} {}",
                    index + 1,
                    disk.sectors / 2048,
                    core::str::from_utf8(&disk.origin[..disk.origin_length]).unwrap_or("?"),
                    core::str::from_utf8(&disk.model).unwrap_or("?").trim()
                );
            }
            match crate::storage::boot_disk() {
                Some(disk) => crate::println!("boot disk: disk{}", disk + 1),
                None => crate::println!("boot disk: not identified"),
            }
        }
        "date" => date_command(words),
        "start" => start_command(),
        "cat" if words.clone().next().and_then(fat_disk).is_some() => {
            let first = words.next().unwrap_or("");
            let named = first.split_once('/').map(|(_, name)| name).filter(|name| !name.is_empty()).or_else(|| words.next());
            match (fat_disk(first), named) {
                (Some((_, fs, _)), Some(name)) => fat_cat(fs, name),
                _ => crate::println!("Usage: cat diskN NAME   (or diskN/NAME)"),
            }
        }
        "write" if words.clone().next().and_then(fat_disk).is_some() => {
            let first = words.clone().next().unwrap_or("");
            let has_slash = first.split_once('/').is_some_and(|(_, name)| !name.is_empty());
            let (name, skipped) = if has_slash { (first.split_once('/').map(|(_, name)| name), 2) } else { (words.clone().nth(1), 3) };
            // The text is everything after the volume (and name) words of the original line.
            let mut text = command.trim_start();
            for _ in 0..skipped {
                text = text.trim_start().split_once(char::is_whitespace).map_or("", |(_, rest)| rest);
            }
            match (fat_disk(first), name) {
                (Some((_, fs, _)), Some(name)) => fat_write(fs, name, text.trim_start()),
                _ => crate::println!("Usage: write diskN NAME TEXT   (or diskN/NAME TEXT)"),
            }
        }
        "ls" if words.clone().next().and_then(fat_disk).is_some() => {
            if let Some((number, fs, _)) = words.next().and_then(fat_disk) {
                fat_list(number, fs);
            }
        }
        "rm" if words.clone().next().and_then(fat_disk).is_some() => {
            let first = words.next().unwrap_or("");
            let named = first.split_once('/').map(|(_, name)| name).filter(|name| !name.is_empty()).or_else(|| words.next());
            match (fat_disk(first), named) {
                (Some((_, fs, _)), Some(name)) => fat_remove(fs, name),
                _ => crate::println!("Usage: rm diskN NAME   (or diskN/NAME)"),
            }
        }
        "ls" => files(&mut words, |volume, _, _| {
            let mut any = false;
            let outcome = volume.for_each(|entry| {
                any = true;
                crate::println!("{:<16} {:>8} bytes", core::str::from_utf8(entry.name()).unwrap_or("?"), entry.length);
            });
            match outcome {
                Ok(()) if !any => crate::println!("FOSFS is empty."),
                Ok(()) => {}
                Err(error) => crate::println!("ls: {error:?}"),
            }
        }),
        "cat" => {
            files(&mut words, |volume, name, mut words| {
                let name = name.or_else(|| words.next()).unwrap_or("");
                match read_file(&volume, name.as_bytes()) {
                Ok(Some(length)) => {
                    let text = unsafe { core::slice::from_raw_parts(core::ptr::addr_of!(FILE_BUFFER) as *const u8, length) };
                    crate::print(format_args!("{}", String8(text)));
                    if !text.ends_with(b"\n") {
                        crate::println!();
                    }
                }
                Ok(None) => crate::println!("cat: no such file."),
                Err(error) => crate::println!("cat: {error:?}"),
                }
            });
        }
        "write" => {
            files(&mut words, |volume, name, mut words| {
                let name = name.or_else(|| words.next()).unwrap_or("");
                // The text is everything after the name in the original line.
                let text = command[command.find(name).map_or(0, |at| at + name.len())..].trim_start();
                match volume.write(name.as_bytes(), text.as_bytes()) {
                    Ok(()) => crate::println!("Wrote {} bytes to {name}.", text.len()),
                    Err(error) => crate::println!("write: {error:?}"),
                }
            });
        }
        "rm" => {
            files(&mut words, |volume, name, mut words| {
                let name = name.or_else(|| words.next()).unwrap_or("");
                match volume.remove(name.as_bytes()) {
                    Ok(true) => crate::println!("Removed {name}."),
                    Ok(false) => crate::println!("rm: no such file."),
                    Err(error) => crate::println!("rm: {error:?}"),
                }
            });
        }
        "usb" => {
            let sub = words.next();
            if sub == Some("rescan") {
                let added = crate::usb_hid::probe_all();
                crate::println!("{added} new device(s) (keyboards or disks).");
            } else if sub == Some("abort-test") {
                // A hidden test hook: see `Controller::abort_self_test`.
                match crate::xhci::controller(0) {
                    Some(controller) => crate::println!("xHCI command abort self-test: {}.", if controller.abort_self_test() { "passed (the ring carries on)" } else { "FAILED" }),
                    None => crate::println!("xHCI command abort self-test: no xHCI controller."),
                }
                return;
            } else if sub == Some("try") {
                match words.next().and_then(|word| word.parse::<u8>().ok()) {
                    Some(port) => crate::usb_hid::try_port(port, words.next()),
                    None => crate::println!("Usage: usb try PORT [hot|warm]   (a diagnostic: reset the port by hand, then enumerate it once)"),
                }
                return;
            }
            crate::usb_hid::describe_devices(|arguments| crate::println!("{}", arguments));
        }
        "sleep" => {
            // Blocks the shell for N seconds without polling anything (the watchdog is fed): what a long disk
            // write or any other slow command looks like to the rest of the kernel. USB audio must keep
            // flowing meanwhile (test: `audio start`, `sleep`, `audio`).
            let seconds = words.next().and_then(|word| word.parse::<u64>().ok()).unwrap_or(1).min(60);
            let end = apic::ticks() + seconds * apic::TICK_HZ;
            while apic::ticks() < end {
                crate::debug::pet();
                unsafe { core::arch::asm!("hlt", options(nomem, nostack)) };
            }
            crate::println!("Slept {seconds} s.");
        }
        "uptime" => {
            let ticks = apic::ticks();
            crate::println!("{} ticks at {} Hz ({} s); spurious legacy-PIC interrupts: {}", ticks, apic::TICK_HZ, ticks / apic::TICK_HZ, apic::legacy_pic_interrupts());
        }
        "dmesg" => crate::print(format_args!("{}", crate::log::contents())),
        "shutdown" | "halt" => {
            crate::println!("Powering off.");
            flush();
            crate::debug::mark_clean_stop();
            power::shutdown(unsafe { RUNTIME_SERVICES });
            crate::println!("Shutdown returned.");
        }
        "reset" | "reboot" => {
            // Through the firmware, so the loader runs again (and a pending kernel slot is tried).
            crate::println!("Resetting.");
            flush();
            crate::debug::mark_clean_stop();
            power::reset(unsafe { RUNTIME_SERVICES });
        }
        "debug" => match words.next() {
            Some("status") | Some("slots") | None => {
                let debug = crate::config::debug();
                crate::println!("reboot-on-crash {}, reboot-delay {} s, kernel-update {}, crash-on-boot {}", debug.reboot_on_crash, debug.reboot_delay_seconds, debug.kernel_update, debug.crash_on_boot);
                crate::debug::slot_summary(|arguments| crate::println!("{}", arguments));
            }
            Some("crash") => {
                crate::println!("Raising an invalid-opcode exception on purpose.");
                flush();
                unsafe { core::arch::asm!("ud2") };
            }
            Some("hang") => {
                crate::println!("Spinning with interrupts on and no progress; the watchdog (if armed) should reset the machine.");
                flush();
                loop {
                    core::hint::spin_loop();
                }
            }
            Some("video") => {
                use fosfor_abi::FLAG_SKIP_VIDEO;
                let mut hint: Option<(u16, u16)> = None;
                let change = match words.next() {
                    Some("skip") => Some(true),
                    Some("auto") => Some(false),
                    // `size WxH` tells the BIOS stage two the monitor's size when the video BIOS cannot (no DDC/EDID, as on the
                    // AMD/nForce board's NVIDIA card); `size auto` forgets it.
                    Some("size") => {
                        match words.next() {
                            Some("auto") => hint = Some((0, 0)),
                            Some(text) => match text.split_once('x').and_then(|(w, h)| Some((w.parse::<u16>().ok()?, h.parse::<u16>().ok()?))) {
                                Some((width, height)) if (640..=8192).contains(&width) && (480..=8192).contains(&height) => hint = Some((width, height)),
                                _ => return crate::println!("debug video size WIDTHxHEIGHT (640x480 to 8192x8192) | size auto"),
                            },
                            None => return crate::println!("debug video size WIDTHxHEIGHT | size auto"),
                        }
                        None
                    }
                    None | Some("status") => None,
                    Some(_) => return crate::println!("debug video [skip|auto|size WxH|size auto]"),
                };
                if let Some(size) = hint {
                    if crate::debug::update_metadata(|metadata| metadata.monitor_hint = size).is_none() {
                        return crate::println!("debug video: the slot metadata could not be read or written.");
                    }
                    if size == (0, 0) {
                        crate::println!("Monitor size hint cleared: a BIOS boot trusts the monitor's EDID, else the old 1280x1024 rule.");
                    } else {
                        crate::println!("Monitor size hint {}x{}: a BIOS boot without an EDID picks the VBE mode whose shape fits it best.", size.0, size.1);
                    }
                }
                let updated = match change {
                    Some(skip) => crate::debug::update_metadata(|metadata| {
                        if skip { metadata.flags |= FLAG_SKIP_VIDEO } else { metadata.flags &= !FLAG_SKIP_VIDEO }
                    }),
                    None => crate::debug::read_metadata(),
                };
                match updated {
                    Some(metadata) if metadata.flags & FLAG_SKIP_VIDEO != 0 => crate::println!("BIOS boot: the video mode switch is skipped (the BIOS text screen stays and the kernel writes to it, and to the serial port). `debug video auto` undoes it. UEFI boots ignore this."),
                    Some(_) => crate::println!("BIOS boot: the video mode (VBE framebuffer) is chosen automatically. Holding Shift during a boot skips it once; `debug video skip` skips it for good."),
                    None => crate::println!("debug video: the slot metadata could not be read or written."),
                }
            }
            Some(_) => crate::println!("debug: status | slots | crash | hang | video [skip|auto|size WxH]"),
        },
        other => {
            let rest = command[other.len()..].trim();
            if !external_command(other, rest) {
                crate::println!("Unknown command: {other} (try `help`).");
            }
        }
    }
}

/// Runs `action` on the volume the command names -- an optional leading `diskN` word picks
/// that disk's FOSFS data partition (mounted on the spot), `@LABEL` the volume carrying that
/// label (see `label`; unlike a disk number it does not change when the disks are found in
/// another order) -- or on the boot volume. The action gets the rest of the command's words.
///
/// A file may also be written `diskN/NAME` or `@LABEL/NAME` (the form tapes and `copy` use): the action then gets the name
/// as its first argument, the other words after it.
fn files<'a>(words: &mut core::str::SplitWhitespace<'a>, action: impl FnOnce(crate::fosfs::Volume, Option<&'a str>, core::str::SplitWhitespace<'a>)) {
    let mut rest = words.clone();
    let first = rest.next().unwrap_or("");
    if let Some((volume_text, name)) = split_volume(first) {
        match volume_word(volume_text) {
            Some(Ok((_, volume))) => action(volume, Some(name), rest),
            Some(Err(String8Error::NoLabel)) => crate::println!("{volume_text}: no volume carries that label (see `label`)."),
            Some(Err(String8Error::Volume(error))) => crate::println!("{volume_text}: {error}."),
            None => {}
        }
        return;
    }
    if let Some(label) = first.strip_prefix('@') {
        match crate::storage::volume_labelled(label.as_bytes()) {
            Some((_, volume)) => action(volume, None, rest),
            None => crate::println!("@{label}: no volume carries that label (see `label`)."),
        }
        return;
    }
    let disk = first.strip_prefix("disk").and_then(|number| number.parse::<usize>().ok());
    let Some(number) = disk else {
        match crate::storage::boot_volume() {
            Some(volume) => action(volume, None, words.clone()),
            None => crate::println!("No FOSFS volume is mounted (see `disks`)."),
        }
        return;
    };
    match crate::storage::volume_on(number) {
        Ok(volume) => action(volume, None, rest),
        Err(error) => crate::println!("disk{number}: {error}."),
    }
}

/// `audio` shows the audio device (USB audio or HD Audio; `audio device N` picks one when there are several); `audio start RATE [capture|playback|both]` / `audio stop`
/// control streams; `audio capture SECONDS [RATE]` records for a few seconds and prints, per second,
/// the packet counters and the peak level of every channel (a quick way to see that a device's inputs
/// are alive without writing a tape).
fn audio_command(mut words: core::str::SplitWhitespace) {
    use crate::audio as ua;
    match words.next() {
        None => ua::status(),
        Some("modes") => ua::print_modes(),
        Some("use") => {
            let capture = match words.next() {
                Some("capture") => true,
                Some("playback") => false,
                _ => return crate::println!("Usage: audio use capture|playback N|auto   (`audio modes` lists the numbers)"),
            };
            let mode = match words.next() {
                Some("auto") | Some("0") => 0,
                Some(word) => match word.parse::<u32>() {
                    Ok(number) => number,
                    Err(_) => return crate::println!("Usage: audio use capture|playback N|auto"),
                },
                None => return crate::println!("Usage: audio use capture|playback N|auto"),
            };
            match ua::select(capture, mode) {
                Ok(()) => crate::println!("{} mode: {}.", if capture { "Capture" } else { "Playback" }, if mode == 0 { "automatic" } else { "picked (`audio modes` shows which)" }),
                Err(reason) => crate::println!("audio: {reason}."),
            }
        }
        Some("device") => {
            let number = match words.next() {
                Some("auto") | None => None,
                Some(word) => match word.parse::<usize>() {
                    Ok(number) => Some(number),
                    Err(_) => return crate::println!("Usage: audio device N|auto   (`audio` lists the devices)"),
                },
            };
            match ua::choose(number) {
                Ok(()) => ua::status(),
                Err(reason) => crate::println!("audio: {reason}."),
            }
        }
        Some("stop") => {
            ua::stop();
            ua::status();
        }
        Some("start") => {
            let rate = words.next().and_then(|word| word.parse::<u32>().ok()).unwrap_or(48_000);
            let (capture, playback) = match words.next() {
                Some("capture") => (true, false),
                Some("playback") => (false, true),
                _ => (true, true),
            };
            match ua::start(rate, capture, playback) {
                Ok(()) => ua::status(),
                Err(reason) => crate::println!("audio: {reason}."),
            }
        }
        Some("capture") => {
            let seconds = words.next().and_then(|word| word.parse::<u64>().ok()).unwrap_or(3).clamp(1, 30);
            let rate = words.next().and_then(|word| word.parse::<u32>().ok()).unwrap_or(48_000);
            if let Err(reason) = ua::start(rate, true, false) {
                return crate::println!("audio: {reason}.");
            }
            let Some((channels, slot)) = ua::capture_format() else { return };
            let (channels, slot) = (usize::from(channels), usize::from(slot));
            let mut data = [0u8; 4096];
            for second in 1..=seconds {
                let mut peaks = [0u32; 32];
                let mut carry = 0usize; // bytes of an incomplete frame left from the previous read
                let end = apic::ticks() + apic::TICK_HZ;
                let mut frames = 0u64;
                while apic::ticks() < end {
                    crate::debug::pet();
                    crate::usb_hid::poll();
                    ua::poll();
                    let frame_bytes = channels * slot;
                    let room = (4096 - carry) / frame_bytes * frame_bytes;
                    let got = ua::read(&mut data[carry..carry + room]);
                    let total = carry + got;
                    let whole = total / frame_bytes * frame_bytes;
                    for frame in data[..whole].chunks(frame_bytes) {
                        for (channel, sample) in frame.chunks(slot).enumerate().take(32) {
                            // little-endian signed PCM in `slot` bytes: sign-extend, take the magnitude
                            let mut value = 0i64;
                            for (index, &byte) in sample.iter().enumerate() {
                                value |= i64::from(byte) << (8 * index);
                            }
                            let bits = 8 * slot as u32;
                            if value >> (bits - 1) != 0 {
                                value -= 1 << bits;
                            }
                            peaks[channel] = peaks[channel].max(value.unsigned_abs() as u32);
                        }
                        frames += 1;
                    }
                    data.copy_within(whole..total, 0);
                    carry = total - whole;
                }
                // Peaks, eight channels per line, and the loudest channel named (devices have up to 32 here).
                let shown = channels.min(32);
                let loudest = (0..shown).max_by_key(|&channel| peaks[channel]).unwrap_or(0);
                crate::println!("[{second:>2} s] {frames} frames; loudest channel {} (peak {})", loudest + 1, peaks[loudest]);
                for (line, group) in peaks[..shown].chunks(8).enumerate() {
                    crate::print(format_args!("   ch {:>2}-{:>2}:", line * 8 + 1, line * 8 + group.len()));
                    for peak in group {
                        crate::print(format_args!(" {peak:>8}"));
                    }
                    crate::println!();
                }
                flush();
            }
            ua::status();
            ua::stop();
        }
        Some("duplex") => {
            // Capture and playback at once (the playback side gets silence, the capture side is discarded):
            // a load test of the shared xHCI event ring and both isochronous endpoints.
            let seconds = words.next().and_then(|word| word.parse::<u64>().ok()).unwrap_or(5).clamp(1, 60);
            let rate = words.next().and_then(|word| word.parse::<u32>().ok()).unwrap_or(48_000);
            if let Err(reason) = ua::start(rate, true, true) {
                return crate::println!("audio: {reason}.");
            }
            let end = apic::ticks() + seconds * apic::TICK_HZ;
            let mut scratch = [0u8; 4096];
            let silence = [0u8; 2048];
            while apic::ticks() < end {
                crate::debug::pet();
                crate::usb_hid::poll();
                    ua::poll();
                while ua::read(&mut scratch) > 0 {}
                if ua::playback_space() >= silence.len() {
                    ua::write(&silence);
                }
            }
            ua::status();
            ua::stop();
        }
        Some("tone") => {
            let frequency = words.next().and_then(|word| word.parse::<u32>().ok()).unwrap_or(440).clamp(20, 20_000);
            let seconds = words.next().and_then(|word| word.parse::<u64>().ok()).unwrap_or(3).clamp(1, 60);
            let rate = words.next().and_then(|word| word.parse::<u32>().ok()).unwrap_or(48_000);
            if let Err(reason) = ua::start(rate, false, true) {
                return crate::println!("audio: {reason}.");
            }
            let Some((channels, slot)) = ua::playback_format() else { return };
            let (channels, slot) = (usize::from(channels), usize::from(slot));
            // A phase accumulator in 1/65536 of a cycle; a parabola per half cycle stands in for a sine
            // (integer-only, and plenty to hear or to measure the fundamental).
            let step = (u64::from(frequency) << 16) / u64::from(rate);
            let mut phase = 0u64;
            let end = apic::ticks() + seconds * apic::TICK_HZ;
            let frame_bytes = channels * slot;
            let mut chunk = [0u8; 2048];
            while apic::ticks() < end {
                crate::debug::pet();
                crate::usb_hid::poll();
                    ua::poll();
                // Only as many frames as the ring has room for, so the tone never has a gap.
                let frames = (chunk.len() / frame_bytes).min(ua::playback_space() / frame_bytes);
                if frames == 0 {
                    continue;
                }
                for frame in chunk[..frames * frame_bytes].chunks_mut(frame_bytes) {
                    let half = (phase & 0x7fff) as i64;
                    let magnitude = (half * (0x8000 - half)) >> 13; // 0..32768
                    let sample = (if phase & 0x8000 != 0 { -magnitude } else { magnitude }) / 4; // a quarter of full scale
                    phase = (phase + step) & 0xffff;
                    let value = sample << (8 * slot as u32 - 16); // scale 16-bit to the slot's width
                    for sample_slot in frame.chunks_mut(slot) {
                        for (index, byte) in sample_slot.iter_mut().enumerate() {
                            *byte = (value >> (8 * index)) as u8;
                        }
                    }
                }
                ua::write(&chunk[..frames * frame_bytes]);
            }
            ua::status();
            ua::stop();
        }
        Some(_) => crate::println!("Usage: audio | audio device N|auto | audio modes | audio use capture|playback N|auto | audio start RATE [capture|playback|both] | audio stop | audio capture SECONDS [RATE] | audio tone [HZ] [SECONDS] [RATE] | audio duplex [SECONDS] [RATE]"),
    }
}

/// `diskN/NAME` or `@LABEL/NAME` split into the volume word and the file name. `None` for anything else (a plain name, or
/// `HOST/NAME`, which names another machine).
fn split_volume(word: &str) -> Option<(&str, &str)> {
    let (volume, name) = word.split_once('/')?;
    let is_volume = volume.strip_prefix("disk").is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())) || volume.strip_prefix('@').is_some_and(|label| !label.is_empty());
    (is_volume && !name.is_empty()).then_some((volume, name))
}

/// A volume named by a command word: `diskN` or `@LABEL`. `None` if the word is neither.
fn volume_word(word: &str) -> Option<Result<(usize, crate::fosfs::Volume), String8Error>> {
    if let Some(label) = word.strip_prefix('@') {
        return Some(crate::storage::volume_labelled(label.as_bytes()).ok_or(String8Error::NoLabel));
    }
    let number = word.strip_prefix("disk")?.parse::<usize>().ok()?;
    Some(crate::storage::volume_on(number).map(|volume| (number, volume)).map_err(String8Error::Volume))
}

enum String8Error {
    NoLabel,
    Volume(crate::storage::VolumeError),
}

/// `diskN` (or `diskN/NAME`) naming a disk that has a FAT32/exFAT volume and no FOSFS one: the volume and the file
/// name after the slash, if any. FAT is only ever reached by naming its disk; a bare name, `@LABEL` and every search
/// the shell does for its own files (`start`, scripts, tapes) look at FOSFS volumes alone.
fn fat_disk(word: &str) -> Option<(usize, crate::fatvol::Fs, Option<&str>)> {
    let (disk, name) = match word.split_once('/') {
        Some((disk, name)) => (disk, Some(name)),
        None => (word, None),
    };
    let number = disk.strip_prefix("disk")?.parse::<usize>().ok()?;
    if number == 0 || crate::storage::volume_on(number).is_ok() {
        return None;
    }
    crate::fatvol::mount(number - 1).map(|fs| (number, fs, name))
}

fn fat_error_text(error: fat::Error) -> &'static str {
    match error {
        fat::Error::Device => "the disk failed",
        fat::Error::NoSpace => "no room on the volume",
        fat::Error::BadName => "that name cannot be used on a FAT volume",
        fat::Error::NotFound => "no such file",
        fat::Error::Corrupt => "the file system is damaged (check it on a PC)",
        _ => "the volume cannot be used",
    }
}

/// `ls diskN` on a FAT volume: names, sizes and the time each was last written.
fn fat_list(number: usize, mut fs: crate::fatvol::Fs) {
    let kind = if fs.kind() == fat::Kind::ExFat { "exFAT" } else { "FAT32" };
    let mut any = false;
    let outcome = fs.list(|entry| {
        any = true;
        let name = core::str::from_utf8(entry.name()).unwrap_or("?");
        match entry.modified {
            Some(time) => {
                if entry.is_directory {
                    crate::println!("{name:<28} {:>10}        {}", "<dir>", crate::clock::Display(time));
                } else {
                    crate::println!("{name:<28} {:>10} bytes  {}", entry.size, crate::clock::Display(time));
                }
            }
            None if entry.is_directory => crate::println!("{name:<28} {:>10}", "<dir>"),
            None => crate::println!("{name:<28} {:>10} bytes", entry.size),
        }
    });
    match outcome {
        Ok(()) if !any => crate::println!("disk{number} ({kind}) is empty."),
        Ok(()) => {}
        Err(error) => crate::println!("ls: disk{number}: {}.", fat_error_text(error)),
    }
}

/// `rm diskN NAME` / `rm diskN/NAME` on a FAT volume.
fn fat_remove(mut fs: crate::fatvol::Fs, name: &str) {
    match fs.remove(name.as_bytes()) {
        Ok(true) => crate::println!("Removed {name}."),
        Ok(false) => crate::println!("rm: no such file (directories are not removed here)."),
        Err(error) => crate::println!("rm: {}.", fat_error_text(error)),
    }
}

/// `cat diskN NAME` on a FAT volume (up to the shell's file buffer; anything but printable text shows as dots).
fn fat_cat(mut fs: crate::fatvol::Fs, name: &str) {
    let entry = match fs.entry(name.as_bytes()) {
        Ok(Some(entry)) if !entry.is_directory => entry,
        Ok(_) => return crate::println!("cat: no such file."),
        Err(error) => return crate::println!("cat: {}.", fat_error_text(error)),
    };
    if entry.size as usize > FILE_CAPACITY {
        return crate::println!("cat: {name} is {} bytes; only files up to {FILE_CAPACITY} bytes are shown (copy it to FOSFS or use a PC).", entry.size);
    }
    let buffer = unsafe { &mut *core::ptr::addr_of_mut!(FILE_BUFFER) };
    match fs.read(&entry.file(), 0, &mut buffer[..entry.size as usize]) {
        Ok(length) => {
            let text = &buffer[..length];
            crate::print(format_args!("{}", String8(text)));
            if !text.ends_with(b"\n") {
                crate::println!();
            }
        }
        Err(error) => crate::println!("cat: {}.", fat_error_text(error)),
    }
}

/// `write diskN NAME TEXT` on a FAT volume: creates or replaces the file with the text.
fn fat_write(mut fs: crate::fatvol::Fs, name: &str, text: &str) {
    if text.is_empty() {
        return crate::println!("write: nothing to write (an empty file cannot be created on a FAT volume here).");
    }
    let outcome = fs.create(name.as_bytes(), text.len() as u64).and_then(|stream| {
        fs.write(&stream, 0, text.as_bytes(), text.len() as u64)?;
        fs.finish(&stream, name.as_bytes(), text.len() as u64)
    });
    match outcome {
        Ok(()) => crate::println!("Wrote {} bytes to {name}.", text.len()),
        Err(error) => crate::println!("write: {}.", fat_error_text(error)),
    }
}

/// `videomodes`: what the video BIOS offered when the BIOS boot path picked the screen mode. Stage two (boot/bios/stage2.S) leaves
/// a table in low memory, which stays intact because the kernel treats everything below 1 MiB as reserved loader data: "VBET" at
/// 0x3600, the number of modes at 0x3604, then one 8-byte record per mode from 0x3608 (mode number, width, height, bits per pixel,
/// usable flag); the DDC (monitor data channel) results sit at 0x35f0 (capabilities AX, BX; EDID AX) and the native size read from
/// the EDID at 0x35f8/0x35fc. Nothing there on a UEFI boot (the magic is missing).
fn videomodes_command() {
    let read16 = |address: u64| unsafe { (address as *const u16).read_unaligned() };
    let read32 = |address: u64| unsafe { (address as *const u32).read_unaligned() };
    if !crate::paging::is_mapped(0x3000) || !crate::paging::is_mapped(0x3600) || read32(0x3600) != 0x5445_4256 {
        return crate::println!("No VBE mode table: this machine was not started by the BIOS boot path (or stage two is too old).");
    }
    let count = usize::from(read16(0x3604)).min(62);
    crate::println!("DDC capabilities call: AX={:04x} BX={:04x}   EDID call: AX={:04x}   native size from EDID: {}x{}",
        read16(0x35f0), read16(0x35f2), read16(0x35f4), read32(0x35f8), read32(0x35fc));
    crate::println!("(AX=004f: the call worked; BL bit 1 of the capabilities: DDC2, which the EDID needs.)");
    crate::println!("{count} video mode(s) from the video BIOS ('*' = usable: supported, colour, graphics, linear framebuffer):");
    for index in 0..count {
        let at = 0x3608 + 8 * index as u64;
        let (mode, width, height) = (read16(at), read16(at + 2), read16(at + 4));
        let (bits, usable) = (unsafe { ((at + 6) as *const u8).read() }, unsafe { ((at + 7) as *const u8).read() });
        crate::println!(" {} {:#06x}: {width}x{height}, {bits} bits per pixel", if usable == 1 { '*' } else { ' ' }, mode);
    }
}

/// `date` shows the clock; `date set YYYY-MM-DD HH:MM[:SS]` sets it (local time) and writes the RTC.
fn date_command(mut words: core::str::SplitWhitespace) {
    match words.next() {
        None => match crate::clock::now() {
            Some(time) => crate::println!("{} (local time, from the RTC)", crate::clock::Display(time)),
            None => crate::println!("The clock is not set (the RTC held no valid time). `date set YYYY-MM-DD HH:MM`."),
        },
        Some("set") => {
            let parsed = (|| {
                let date = words.next()?;
                let time = words.next()?;
                let mut parts = date.split('-');
                let (year, month, day) = (parts.next()?.parse().ok()?, parts.next()?.parse().ok()?, parts.next()?.parse().ok()?);
                let mut parts = time.split(':');
                let (hour, minute) = (parts.next()?.parse().ok()?, parts.next()?.parse().ok()?);
                let second = parts.next().map_or(Some(0), |word| word.parse().ok())?;
                Some(fat::Time { year, month, day, hour, minute, second })
            })();
            match parsed {
                None => crate::println!("Usage: date set YYYY-MM-DD HH:MM[:SS]   (local time)"),
                Some(time) => match crate::clock::set(&time) {
                    Ok(()) => crate::println!("The clock is {}.", crate::clock::Display(time)),
                    Err(reason) => crate::println!("date: {reason}"),
                },
            }
        }
        Some(_) => crate::println!("Usage: date | date set YYYY-MM-DD HH:MM[:SS]"),
    }
}

/// A file copied to or from a FAT volume (one of the two ends is FAT). Through memory, like the FOSFS copy.
fn copy_with_fat(source: &str, name: &str, destination: &str, new_name: &str, source_fat: Option<(usize, crate::fatvol::Fs, Option<&str>)>, destination_fat: Option<(usize, crate::fatvol::Fs, Option<&str>)>) {
    const LIMIT: usize = 64 << 20;
    if let (Some((a, _, _)), Some((b, _, _))) = (&source_fat, &destination_fat) {
        if a == b && name == new_name {
            return crate::println!("copy: source and destination are the same file.");
        }
    }
    // The source: its length, and later its bytes.
    let mut source_fosfs = None;
    let mut source_fs = source_fat.map(|(_, fs, _)| fs);
    let mut entry = None;
    let length = if let Some(fs) = source_fs.as_mut() {
        match fs.entry(name.as_bytes()) {
            Ok(Some(found)) if !found.is_directory => {
                entry = Some(found);
                found.size as usize
            }
            Ok(_) => return crate::println!("copy: {name}: no such file on {source}."),
            Err(error) => return crate::println!("copy: {}.", fat_error_text(error)),
        }
    } else {
        match volume_word(source) {
            Some(Ok((_, volume))) => match volume.file_length(name.as_bytes()) {
                Ok(Some(length)) => {
                    source_fosfs = Some(volume);
                    length as usize
                }
                Ok(None) => return crate::println!("copy: {name}: no such file on {source}."),
                Err(error) => return crate::println!("copy: {name}: {error:?}"),
            },
            Some(Err(String8Error::NoLabel)) => return crate::println!("copy: no volume carries the label in {source}."),
            Some(Err(String8Error::Volume(error))) => return crate::println!("copy: {source}: {error}."),
            None => return crate::println!("copy: {source} is not diskN or @LABEL."),
        }
    };
    if destination_fat.is_none() && !crate::fosfs::valid_name(new_name.as_bytes()) {
        return crate::println!("copy: {new_name} is not a FOSFS file name (lowercase letters, digits . _ -, up to 16): give a new name, `copy {source} {name} {destination} NEWNAME`.");
    }
    if length > LIMIT {
        return crate::println!("copy: {name} is {} MiB; the limit is {} MiB.", length >> 20, LIMIT >> 20);
    }
    if length == 0 && destination_fat.is_some() {
        return crate::println!("copy: an empty file cannot be written to a FAT volume here.");
    }
    let destination_volume = if destination_fat.is_none() {
        match volume_word(destination) {
            Some(Ok((_, volume))) => Some(volume),
            Some(Err(String8Error::NoLabel)) => return crate::println!("copy: no volume carries the label in {destination}."),
            Some(Err(String8Error::Volume(error))) => return crate::println!("copy: {destination}: {error}."),
            None => return crate::println!("copy: {destination} is not diskN or @LABEL."),
        }
    } else {
        None
    };
    let frames = (length.max(1) as u64).div_ceil(crate::memory::FRAME);
    let Some(buffer) = crate::memory::allocate_contiguous(frames, None) else {
        return crate::println!("copy: not enough free memory for {length} bytes.");
    };
    let data = unsafe { core::slice::from_raw_parts_mut(buffer as *mut u8, length) };
    let outcome: Result<(), &'static str> = (|| {
        // Read.
        match (source_fs.as_mut(), source_fosfs) {
            (Some(fs), _) => {
                let file = entry.unwrap().file();
                match fs.read(&file, 0, data) {
                    Ok(read) if read == length => {}
                    Ok(_) => return Err("the source changed while it was being read"),
                    Err(error) => return Err(fat_error_text(error)),
                }
            }
            (None, Some(volume)) => match volume.read(name.as_bytes(), data) {
                Ok(Some(read)) if read == length => {}
                Ok(_) => return Err("the source changed while it was being read"),
                Err(_) => return Err("reading the source failed"),
            },
            (None, None) => return Err("no source"),
        }
        // Write.
        if let Some((_, mut fs, _)) = destination_fat {
            let stream = fs.create(new_name.as_bytes(), length as u64).map_err(fat_error_text)?;
            let mut offset = 0;
            while offset < length {
                let count = (1 << 20).min(length - offset);
                fs.write(&stream, offset as u64, &data[offset..offset + count], (offset + count) as u64).map_err(fat_error_text)?;
                offset += count;
            }
            fs.finish(&stream, new_name.as_bytes(), length as u64).map_err(fat_error_text)
        } else {
            destination_volume.unwrap().write(new_name.as_bytes(), data).map_err(|error| copy_error("write", error).0)
        }
    })();
    crate::memory::free(buffer, frames);
    match outcome {
        Ok(()) => crate::println!("Copied {name} ({length} bytes) from {source} to {destination} as {new_name}."),
        Err(reason) => crate::println!("copy: {reason}."),
    }
}

/// `copy SRC NAME DST [NEWNAME]`: one file from one volume to another, where SRC and DST are
/// `diskN` or `@LABEL`. The whole file goes through a buffer taken from the page allocator, so
/// it can be larger than the shell's own 64 KiB file buffer. The source is only read.
fn copy_command(mut words: core::str::SplitWhitespace) {
    let Some(source) = words.next() else {
        return crate::println!("Usage: copy SRC NAME DST [NEWNAME]   (diskN or @LABEL); copy HOST[:diskN]/NAME DST [NEWNAME]; copy SRC NAME HOST[:diskN] [NEWNAME]");
    };
    // `copy diskN/NAME DST[/NEWNAME]` is `copy diskN NAME DST NEWNAME`.
    if let Some((volume, name)) = split_volume(source) {
        let Some(destination) = words.next() else { return crate::println!("copy: where to? (copy diskN/NAME diskM[/NEWNAME])") };
        let (destination, new_name) = match split_volume(destination) {
            Some((volume, new_name)) => (volume, new_name),
            None => (destination, words.next().unwrap_or(name)),
        };
        return copy_between(volume, name, destination, new_name);
    }
    // `copy HOST[:diskN]/NAME DST [NEWNAME]`: fetch a file from another machine.
    if volume_word(source).is_none() && source.contains('/') {
        let Some(destination) = words.next() else { return crate::println!("copy: where to? (copy HOST[:diskN]/NAME DST [NEWNAME])") };
        return copy_from_peer(source, destination, words.next());
    }
    let (Some(name), Some(destination)) = (words.next(), words.next()) else {
        return crate::println!("Usage: copy SRC NAME DST [NEWNAME]   (diskN or @LABEL); copy HOST[:diskN]/NAME DST [NEWNAME]; copy SRC NAME HOST[:diskN] [NEWNAME]");
    };
    let (destination, new_name) = match split_volume(destination) {
        Some((volume, new_name)) => (volume, new_name),
        None => (destination, words.next().unwrap_or(name)),
    };
    // `copy SRC NAME HOST[:diskN] [NEWNAME]`: store a local file on another machine.
    if volume_word(destination).is_none() {
        return copy_to_peer(source, name, destination, new_name);
    }
    copy_between(source, name, destination, new_name)
}

/// The copy of one file between two local volumes (`diskN` or `@LABEL` words).
fn copy_between(source: &str, name: &str, destination: &str, new_name: &str) {
    let (source_fat, destination_fat) = (fat_disk(source), fat_disk(destination));
    if source_fat.is_some() || destination_fat.is_some() {
        return copy_with_fat(source, name, destination, new_name, source_fat, destination_fat);
    }
    let resolve = |word: &str| match volume_word(word) {
        Some(Ok(found)) => Some(found),
        Some(Err(String8Error::NoLabel)) => {
            crate::println!("copy: no volume carries the label in {word}.");
            None
        }
        Some(Err(String8Error::Volume(error))) => {
            crate::println!("copy: {word}: {error}.");
            None
        }
        None => {
            crate::println!("copy: {word} is not diskN or @LABEL.");
            None
        }
    };
    let Some((source_number, source_volume)) = resolve(source) else { return };
    let Some((destination_number, destination_volume)) = resolve(destination) else { return };
    if source_number == destination_number && name == new_name {
        return crate::println!("copy: source and destination are the same file.");
    }
    if !crate::fosfs::valid_name(new_name.as_bytes()) {
        return crate::println!("copy: {new_name} is not a valid file name.");
    }
    let length = match source_volume.file_length(name.as_bytes()) {
        Ok(Some(length)) => length as usize,
        Ok(None) => return crate::println!("copy: {name}: no such file on {source}."),
        Err(error) => return crate::println!("copy: {name}: {error:?}"),
    };
    const LIMIT: usize = 64 << 20;
    if length > LIMIT {
        return crate::println!("copy: {name} is {} MiB; the limit is {} MiB.", length >> 20, LIMIT >> 20);
    }
    let frames = (length.max(1) as u64).div_ceil(crate::memory::FRAME);
    let Some(buffer) = crate::memory::allocate_contiguous(frames, None) else {
        return crate::println!("copy: not enough free memory for {length} bytes.");
    };
    let data = unsafe { core::slice::from_raw_parts_mut(buffer as *mut u8, length) };
    let outcome = match source_volume.read(name.as_bytes(), data) {
        Ok(Some(read)) if read == length => destination_volume.write(new_name.as_bytes(), &data[..length]).map_err(|error| copy_error("write", error)),
        Ok(_) => Err(CopyError("the source changed while it was being read")),
        Err(error) => Err(copy_error("read", error)),
    };
    crate::memory::free(buffer, frames);
    match outcome {
        Ok(()) => crate::println!("Copied {name} ({length} bytes) from {source} to {destination} as {new_name}."),
        Err(CopyError(reason)) => crate::println!("copy: {reason}."),
    }
}

/// `copy HOST[:diskN]/NAME DST [NEWNAME]`: the first word is the remote file, the second the local volume (and the third,
/// the new name, arrives as `destination` here because the parser shifted the words).
fn copy_from_peer(remote: &str, local_volume: &str, new_name: Option<&str>) {
    let Some((spec, name)) = remote.rsplit_once('/') else { return };
    let new_name = new_name.unwrap_or(name);
    let Some(Ok((_, volume))) = volume_word(local_volume) else {
        return crate::println!("copy: {local_volume} is not diskN or @LABEL.");
    };
    if !crate::fosfs::valid_name(name.as_bytes()) || !crate::fosfs::valid_name(new_name.as_bytes()) {
        return crate::println!("copy: invalid file name.");
    }
    let Some((mac, nic, disk)) = netfs_peer(spec) else { return crate::println!("copy: no machine called {spec} answered (see `nic peers`).") };
    crate::println!("Fetching {name} from {} disk{disk}...", Mac(&mac));
    match netfs_fetch(mac, nic, disk, name) {
        Ok(length) => {
            let staging = crate::tape::staging().unwrap();
            match volume.write(new_name.as_bytes(), &staging[..length]) {
                Ok(()) => crate::println!("Copied {name} ({length} bytes) from {spec} to {local_volume} as {new_name}."),
                Err(error) => crate::println!("copy: {}.", copy_error("write", error).0),
            }
        }
        Err(reason) => crate::println!("copy: {reason}."),
    }
}

/// `copy SRC NAME HOST[:diskN] [NEWNAME]`: a local file to another machine.
fn copy_to_peer(source: &str, name: &str, host: &str, new_name: &str) {
    let Some(Ok((_, volume))) = volume_word(source) else {
        return crate::println!("copy: {source} is not diskN or @LABEL.");
    };
    if !crate::fosfs::valid_name(name.as_bytes()) || !crate::fosfs::valid_name(new_name.as_bytes()) {
        return crate::println!("copy: invalid file name.");
    }
    let Some(staging) = crate::tape::staging() else { return crate::println!("copy: no memory for the transfer buffer.") };
    let length = match volume.read(name.as_bytes(), staging) {
        Ok(Some(0)) => return crate::println!("copy: {name} is empty."),
        Ok(Some(length)) => length,
        Ok(None) => return crate::println!("copy: {name}: no such file on {source}."),
        Err(_) => return crate::println!("copy: {name} cannot be read (over 4 MiB?)."),
    };
    let Some((mac, nic, disk)) = netfs_peer(host) else { return crate::println!("copy: no machine called {host} answered (see `nic peers`).") };
    crate::println!("Sending {name} ({length} bytes) to {} disk{disk}...", Mac(&mac));
    match netfs_store(mac, nic, disk, new_name, length) {
        Ok(()) => crate::println!("Copied {name} to {host} as {new_name}."),
        Err(reason) => crate::println!("copy: {reason}."),
    }
}

/// `tape load HOST[:diskN] NAME`: the file from another machine into the tape staging buffer; its length.
pub fn tape_fetch(spec: &str, name: &str) -> Result<usize, &'static str> {
    if !crate::fosfs::valid_name(name.as_bytes()) {
        return Err("invalid file name");
    }
    let (mac, nic, disk) = netfs_peer(spec).ok_or("no such machine answered (see `nic peers`)")?;
    crate::println!("Fetching {name} from {} disk{disk}...", Mac(&mac));
    netfs_fetch(mac, nic, disk, name)
}

/// `tape save HOST[:diskN] NAME`: the first `length` staged bytes to another machine as a file.
pub fn tape_store(spec: &str, name: &str, length: usize) -> Result<(), &'static str> {
    if !crate::fosfs::valid_name(name.as_bytes()) {
        return Err("invalid file name");
    }
    let (mac, nic, disk) = netfs_peer(spec).ok_or("no such machine answered (see `nic peers`)")?;
    crate::println!("Sending {name} ({length} bytes) to {} disk{disk}...", Mac(&mac));
    netfs_store(mac, nic, disk, name, length)
}

/// `tape send FILE PEER`: the first `length` staged bytes as a tape for a machine waiting in `tape load net` (START with
/// the length, numbered DATA chunks, STOP; the receiver does not acknowledge, and there is no retransmission). PEER is a
/// name, a number from `nic peers`, or a MAC address.
pub fn tape_send(peer: &str, length: usize) -> Result<(), &'static str> {
    let (mac, nic, _) = netfs_peer(peer).ok_or("no such machine answered (see `nic peers`)")?;
    let staging = crate::tape::staging().ok_or("no memory for the transfer buffer")?;
    crate::println!("Sending {length} bytes to {} as a tape (it must be in `tape load net`)...", Mac(&mac));
    netfs_reply(nic, &mac, 0x10, &(length as u32).to_le_bytes());
    let mut next = apic::micros() + 10_000;
    blocking(true);
    for (sequence, chunk) in staging[..length].chunks(NETFS_CHUNK).enumerate() {
        let mut frame = [0u8; 4 + NETFS_CHUNK];
        frame[0..4].copy_from_slice(&(sequence as u32).to_le_bytes());
        frame[4..4 + chunk.len()].copy_from_slice(chunk);
        // One frame per 3 ms (as tools/send_tape.py): the receiver polls a finite ring (the sender is not told about a lost frame).
        while apic::micros() < next {
            core::hint::spin_loop();
        }
        next = apic::micros() + 3000;
        netfs_reply(nic, &mac, 0x11, &frame[..4 + chunk.len()]);
        crate::debug::pet();
        if break_requested() {
            blocking(false);
            return Err("interrupted");
        }
    }
    netfs_reply(nic, &mac, 0x12, &[]);
    blocking(false);
    Ok(())
}

struct CopyError(&'static str);

#[allow(non_snake_case)]
fn copy_error(stage: &'static str, error: crate::fosfs::Error) -> CopyError {
    use crate::fosfs::Error;
    CopyError(match (stage, error) {
        ("write", Error::DirectoryFull) => "the destination directory is full",
        ("write", Error::DataFull) => "the destination volume is full",
        ("write", Error::InvalidName) => "the destination name is invalid",
        ("write", _) => "writing the destination failed",
        _ => "reading the source failed",
    })
}

/// A `format` that was announced and awaits `format diskN confirm`.
#[derive(Clone, Copy)]
struct PendingFormat {
    number: usize,
    label: [u8; crate::fosfs::MAX_NAME],
    label_length: usize,
    force: bool,
    /// `format diskN exfat`: a PC-readable exFAT volume instead of FOSFS.
    exfat: bool,
}
static mut PENDING_FORMAT: Option<PendingFormat> = None;

/// What `format` would do to a disk.
enum FormatPlan {
    /// Erase the volume in an existing FOSFS partition; other partitions are left alone.
    Reformat(crate::gpt::Partition),
    /// Replace the partition table with one FOSFS partition covering the disk.
    Partition { replaces: &'static str },
}

fn plan_format(disk: usize, force: bool) -> Result<FormatPlan, &'static str> {
    use crate::gpt::Summary;
    match crate::gpt::inspect(disk).ok_or("cannot read the disk")? {
        Summary::Gpt { fosfs: Some(partition), .. } => Ok(FormatPlan::Reformat(partition)),
        Summary::Blank => Ok(FormatPlan::Partition { replaces: "nothing: the disk is blank" }),
        Summary::Gpt { .. } if force => Ok(FormatPlan::Partition { replaces: "its GPT, with the partitions it lists" }),
        Summary::Unknown if force => Ok(FormatPlan::Partition { replaces: "whatever is in its first sectors (a partition table or file system)" }),
        Summary::Gpt { .. } => Err("it holds a GPT with partitions but no FOSFS one; add `force` to replace the partition table"),
        Summary::Unknown => Err("its first sectors hold data that is not a GPT (an MBR or a file system); add `force` to replace it"),
    }
}

/// `grow diskN`: makes the disk's FOSFS volume fill the disk. A stick flashed from the 96 MiB image keeps a 32 MiB data
/// partition and leaves the rest of the stick unused; this moves the backup partition table to the real end of the disk,
/// extends the FOSFS partition to it and the volume to the partition. Files are kept (a FOSFS volume is extents addressed from
/// its start, so new space is just free space). Only a FOSFS partition that is the last on the disk can grow.
fn grow_command(mut words: core::str::SplitWhitespace) {
    let Some(number) = words.next().and_then(|word| word.strip_prefix("disk")).and_then(|digits| digits.parse::<usize>().ok()) else {
        return crate::println!("Usage: grow diskN   (make the disk's FOSFS volume fill the disk; files are kept)");
    };
    let Some(info) = number.checked_sub(1).and_then(crate::block::get) else {
        return crate::println!("disk{number}: no such disk (see `disks`).");
    };
    let disk = number - 1;
    let before = match crate::storage::volume_on(number) {
        Ok(volume) => volume.sectors,
        Err(error) => return crate::println!("grow: disk{number}: {error}."),
    };
    use crate::gpt::GrowError;
    let partition = match crate::gpt::grow_last_partition(disk, &fosfor_abi::FOSFS_TYPE_GUID) {
        Ok(partition) => partition,
        Err(GrowError::NothingToGrow) => {
            // The partition may already fill the disk while the volume does not (an interrupted grow): finish that.
            match crate::gpt::find(disk, &fosfor_abi::FOSFS_TYPE_GUID, |_| true) {
                Some(partition) if partition.sectors > before + 2048 => partition,
                _ => return crate::println!("disk{number}: the FOSFS volume ({} MiB) already fills the disk.", before / 2048),
            }
        }
        Err(GrowError::NoGpt) => return crate::println!("grow: disk{number} has no GPT this kernel understands."),
        Err(GrowError::NoPartition) => return crate::println!("grow: disk{number} has no FOSFS partition."),
        Err(GrowError::NotLast) => return crate::println!("grow: another partition lies after the FOSFS one; only the last partition can grow."),
        Err(GrowError::Disk) => return crate::println!("grow: disk{number}: reading or writing the partition table failed."),
    };
    let volume = match crate::fosfs::Volume::mount(disk, partition.start, partition.sectors).and_then(|volume| volume.grow(partition.sectors)) {
        Ok(volume) => volume,
        Err(error) => return crate::println!("grow: the partition grew but the volume could not ({error:?}); run `grow disk{number}` again."),
    };
    if crate::storage::boot_disk() == Some(disk) {
        crate::storage::remount_boot();
    }
    let _ = info;
    crate::println!("disk{number}: FOSFS volume grown from {} MiB to {} MiB; the files are unchanged.", before / 2048, volume.sectors / 2048);
}

/// `format diskN [NAME] [force]` says what it would erase and waits; `format diskN confirm`
/// (the very next command) does it. Never the boot disk. A disk with a FOSFS partition has
/// only that partition's volume cleared; a blank disk gets a fresh GPT with one FOSFS
/// partition; any other disk needs `force` because its partition table is replaced.
fn format_command(mut words: core::str::SplitWhitespace) {
    let Some(target) = words.next() else {
        return crate::println!("Usage: format diskN [NAME] [force]   or   format diskN exfat [LABEL]   (then: format diskN confirm)");
    };
    let Some(number) = target.strip_prefix("disk").and_then(|number| number.parse::<usize>().ok()) else {
        return crate::println!("Usage: format diskN [NAME] [force]   (then: format diskN confirm)");
    };
    let Some(info) = number.checked_sub(1).and_then(crate::block::get) else {
        return crate::println!("disk{number}: no such disk (see `disks`).");
    };
    let disk = number - 1;
    if crate::storage::boot_disk() == Some(disk) {
        return crate::println!("disk{number} is the boot disk (the running kernel's); format refuses it. Use `rm` for its files.");
    }
    let (mut confirm, mut force, mut exfat, mut label) = (false, false, false, "");
    for word in words {
        match word {
            "confirm" => confirm = true,
            "force" => force = true,
            "exfat" => exfat = true,
            name => label = name,
        }
    }
    if !exfat && !label.is_empty() && !crate::fosfs::valid_name(label.as_bytes()) {
        return crate::println!("format: a label is up to 16 lowercase letters, digits, `.`, `_` or `-`.");
    }
    if confirm {
        let pending = unsafe { (*core::ptr::addr_of_mut!(PENDING_FORMAT)).take() };
        let Some(pending) = pending.filter(|pending| pending.number == number) else {
            return crate::println!("format: nothing is waiting for confirmation on disk{number}; run `format disk{number}` first.");
        };
        if pending.exfat {
            return match crate::fatvol::format_exfat(disk, &pending.label[..pending.label_length]) {
                Ok(megabytes) => crate::println!("disk{number}: formatted as exFAT ({megabytes} MiB, GPT). A PC can read it; `ls disk{number}`, or record to it."),
                Err(error) => crate::println!("format: disk{number}: {error}."),
            };
        }
        return do_format(disk, number, &pending.label[..pending.label_length], pending.force);
    }
    if exfat {
        // What the disk holds now, so that the user knows what the confirmation will destroy.
        crate::println!("disk{number}: {} MiB, {} ({}).", info.sectors / 2048, core::str::from_utf8(&info.model).unwrap_or("?").trim(), core::str::from_utf8(&info.origin[..info.origin_length]).unwrap_or("?"));
        if label.len() > 11 {
            return crate::println!("format: an exFAT label is up to 11 characters.");
        }
        if let Ok(volume) = crate::storage::volume_on(number) {
            let mut files = 0;
            let _ = volume.for_each(|_| files += 1);
            crate::println!("It holds a FOSFS volume with {files} file(s).");
        } else if let Some(mut fs) = crate::fatvol::mount(disk) {
            let mut files = 0;
            let _ = fs.list(|_| files += 1);
            crate::println!("It holds a {} volume with {files} file(s).", if fs.kind() == fat::Kind::ExFat { "exFAT" } else { "FAT32" });
        } else {
            match crate::gpt::inspect(disk) {
                Some(crate::gpt::Summary::Blank) => crate::println!("It is blank."),
                Some(crate::gpt::Summary::Gpt { .. }) => crate::println!("It holds a GPT partition table (not a volume this kernel knows)."),
                _ => crate::println!("It holds data this kernel does not recognise."),
            }
        }
        crate::println!("ALL of it will be erased: one exFAT partition of about {} MiB replaces it.", info.sectors / 2048 - 1);
        let mut stored = PendingFormat { number, label: [0; crate::fosfs::MAX_NAME], label_length: label.len(), force: false, exfat: true };
        stored.label[..label.len()].copy_from_slice(label.as_bytes());
        unsafe { PENDING_FORMAT = Some(stored) };
        return crate::println!("To go ahead, type exactly: format disk{number} confirm");
    }
    let plan = match plan_format(disk, force) {
        Ok(plan) => plan,
        Err(reason) => return crate::println!("format: disk{number}: {reason}."),
    };
    crate::println!(
        "disk{number}: {} MiB, {} ({}).",
        info.sectors / 2048,
        core::str::from_utf8(&info.model).unwrap_or("?").trim(),
        core::str::from_utf8(&info.origin[..info.origin_length]).unwrap_or("?")
    );
    match plan {
        FormatPlan::Reformat(partition) => {
            let files = crate::fosfs::Volume::mount(disk, partition.start, partition.sectors).ok().and_then(|volume| {
                let mut files = 0;
                volume.for_each(|_| files += 1).ok()?;
                Some(files)
            });
            match files {
                Some(files) => crate::println!("Its FOSFS partition ({} MiB, {files} file(s)) will be erased; any other partition is left alone.", partition.sectors / 2048),
                None => crate::println!("Its FOSFS partition ({} MiB, no valid volume yet) will be given a fresh empty volume.", partition.sectors / 2048),
            }
        }
        FormatPlan::Partition { replaces } => {
            crate::println!("It will be repartitioned as one FOSFS partition of {} MiB, replacing {replaces}.", info.sectors / 2048 - 1);
        }
    }
    let mut stored = PendingFormat { number, label: [0; crate::fosfs::MAX_NAME], label_length: label.len(), force, exfat: false };
    stored.label[..label.len()].copy_from_slice(label.as_bytes());
    unsafe { PENDING_FORMAT = Some(stored) };
    crate::println!("To go ahead, type exactly: format disk{number} confirm");
}

fn do_format(disk: usize, number: usize, label: &[u8], force: bool) {
    let partition = match plan_format(disk, force) {
        Ok(FormatPlan::Reformat(partition)) => partition,
        Ok(FormatPlan::Partition { .. }) => match crate::gpt::write_single_fosfs_partition(disk) {
            Ok(partition) => {
                crate::println!("disk{number}: wrote a new partition table with one FOSFS partition ({} MiB).", partition.sectors / 2048);
                partition
            }
            Err(error) => return crate::println!("format: disk{number}: writing the partition table failed ({error:?})."),
        },
        Err(reason) => return crate::println!("format: disk{number}: {reason}."),
    };
    match crate::fosfs::Volume::format(disk, partition.start, partition.sectors, label) {
        Ok(_) if label.is_empty() => crate::println!("disk{number}: formatted; an empty FOSFS volume of {} MiB.", partition.sectors / 2048),
        Ok(_) => crate::println!("disk{number}: formatted as @{}; an empty FOSFS volume of {} MiB.", core::str::from_utf8(label).unwrap_or("?"), partition.sectors / 2048),
        Err(error) => crate::println!("format: disk{number}: {error:?}"),
    }
}

/// `label` lists every volume's label; `label diskN|@LABEL` shows one; `label diskN|@LABEL
/// NAME` sets it and `label diskN|@LABEL -` clears it. A label may be on one volume only.
fn label_command(mut words: core::str::SplitWhitespace) {
    let Some(target) = words.next() else {
        for number in 1..=crate::block::count() {
            match crate::storage::volume_on(number) {
                Ok(volume) => match volume.label() {
                    Ok(label) if !label.is_empty() => crate::println!("disk{number}: @{}", core::str::from_utf8(label.as_bytes()).unwrap_or("?")),
                    Ok(_) => crate::println!("disk{number}: (no label)"),
                    Err(error) => crate::println!("disk{number}: {error:?}"),
                },
                Err(crate::storage::VolumeError::NoPartition) | Err(crate::storage::VolumeError::NotFormatted) => crate::println!("disk{number}: (no FOSFS volume)"),
                Err(error) => crate::println!("disk{number}: {error}"),
            }
        }
        return;
    };
    let (number, volume) = if let Some(label) = target.strip_prefix('@') {
        match crate::storage::volume_labelled(label.as_bytes()) {
            Some(found) => found,
            None => return crate::println!("@{label}: no volume carries that label."),
        }
    } else if let Some(number) = target.strip_prefix("disk").and_then(|number| number.parse::<usize>().ok()) {
        match crate::storage::volume_on(number) {
            Ok(volume) => (number, volume),
            Err(error) => return crate::println!("disk{number}: {error}."),
        }
    } else {
        return crate::println!("Usage: label [diskN|@LABEL [NAME|-]]");
    };
    let Some(name) = words.next() else {
        match volume.label() {
            Ok(label) if !label.is_empty() => crate::println!("disk{number}: @{}", core::str::from_utf8(label.as_bytes()).unwrap_or("?")),
            Ok(_) => crate::println!("disk{number}: (no label)"),
            Err(error) => crate::println!("disk{number}: {error:?}"),
        }
        return;
    };
    let wanted = if name == "-" { "" } else { name };
    if !wanted.is_empty() {
        if !crate::fosfs::valid_name(wanted.as_bytes()) {
            return crate::println!("label: up to 16 lowercase letters, digits, `.`, `_` or `-`.");
        }
        if let Some((other, _)) = crate::storage::volume_labelled(wanted.as_bytes()).filter(|(other, _)| *other != number) {
            return crate::println!("label: disk{other} already carries @{wanted}.");
        }
    }
    match volume.set_label(wanted.as_bytes()) {
        Ok(()) if wanted.is_empty() => crate::println!("disk{number}: label cleared."),
        Ok(()) => crate::println!("disk{number} is now @{wanted}."),
        Err(error) => crate::println!("label: {error:?}"),
    }
}

const FILE_CAPACITY: usize = 64 * 1024;
static mut FILE_BUFFER: [u8; FILE_CAPACITY] = [0; FILE_CAPACITY];

fn read_file(volume: &crate::fosfs::Volume, name: &[u8]) -> Result<Option<usize>, crate::fosfs::Error> {
    let buffer = unsafe { &mut *core::ptr::addr_of_mut!(FILE_BUFFER) };
    volume.read(name, buffer)
}

/// Prints bytes as text, replacing anything that is not printable ASCII or a newline.
struct String8<'a>(&'a [u8]);
impl core::fmt::Display for String8<'_> {
    fn fmt(&self, formatter: &mut core::fmt::Formatter) -> core::fmt::Result {
        for &byte in self.0 {
            let shown = if byte == b'\n' || (0x20..0x7f).contains(&byte) { byte as char } else { '.' };
            core::fmt::Write::write_char(formatter, shown)?;
        }
        Ok(())
    }
}

fn parse_number(word: &str) -> Option<u64> {
    match word.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => word.parse().ok(),
    }
}

/// `xxd`-style dump: offset, hex bytes, printable characters.
fn dump_bytes(base: u64, bytes: &[u8]) {
    for (row, chunk) in bytes.chunks(16).enumerate() {
        crate::print(format_args!("{:#012x}: ", base + (row * 16) as u64));
        for byte in chunk {
            crate::print(format_args!("{byte:02x} "));
        }
        for _ in chunk.len()..16 {
            crate::print(format_args!("   "));
        }
        crate::print(format_args!("|"));
        for byte in chunk {
            crate::print(format_args!("{}", if (0x20..=0x7e).contains(byte) { *byte as char } else { '.' }));
        }
        crate::println!("|");
    }
}

/// Dumps physical memory (identity mapped), refusing pages that are not mapped instead of faulting.
fn hexdump(address: u64, length: usize) {
    let mut at = address;
    let end = address + length as u64;
    while at < end {
        let page_end = ((at & !0xfff) + 0x1000).min(end);
        if !crate::paging::is_mapped(at) {
            return crate::println!("{at:#x}: not mapped.");
        }
        let chunk = unsafe { core::slice::from_raw_parts(at as *const u8, (page_end - at) as usize) };
        dump_bytes(at, chunk);
        at = page_end;
    }
}

/// `nic send`: one broadcast test frame (ethertype 0x88b5, v1's TEST_ETHERTYPE) out of interface 1.
/// The interface chosen with `nic use N` (zero-based): what `nic sniff` defaults to and what a tape's single-interface calls use.
pub fn active_nic() -> usize {
    unsafe { ACTIVE_NIC }.min(nic_count().saturating_sub(1))
}

/// `nic use N`: the interface the other nic subcommands, and a tape's single-interface network calls, use.
fn nic_use(argument: Option<&str>) {
    match argument.and_then(|word| word.parse::<usize>().ok()) {
        Some(number) if number >= 1 && number <= nic_count() => {
            unsafe { ACTIVE_NIC = number - 1 };
            crate::println!("Using NIC {number} for nic sniff and the single-interface tape calls.");
        }
        _ => crate::println!("Usage: nic use N   (N from 1 to {}; `nic` lists them). Now: NIC {}.", nic_count(), unsafe { ACTIVE_NIC } + 1),
    }
}

/// Receives every waiting frame on every interface. The terminal's and discovery's own frames are handled as usual
/// (so the session stays alive while a command runs); everything else goes to `visit(interface, frame)`.
fn drain_nics(mut visit: impl FnMut(usize, &[u8])) {
    let mut buffer = [0u8; 1600];
    for index in 0..nic_count() {
        loop {
            let Some(length) = nic_at(index).and_then(|nic| nic.receive(&mut buffer)) else { break };
            if !handle_frame(index, &buffer[..length]) {
                visit(index, &buffer[..length]);
            }
        }
    }
    if session_open() {
        flush();
    }
}

/// A short line of text on the stack.
struct Line {
    bytes: [u8; 110],
    length: usize,
}

impl Line {
    fn new() -> Line {
        Line { bytes: [0; 110], length: 0 }
    }
    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.bytes[..self.length]).unwrap_or("")
    }
}

impl core::fmt::Write for Line {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        let take = text.len().min(self.bytes.len() - self.length);
        self.bytes[self.length..self.length + take].copy_from_slice(&text.as_bytes()[..take]);
        self.length += take;
        Ok(())
    }
}

/// One line about a frame: the addresses, the type and what is inside, as far as the common protocols go.
fn describe_frame(frame: &[u8], line: &mut Line) {
    use core::fmt::Write;
    if frame.len() < 14 {
        let _ = write!(line, "runt frame");
        return;
    }
    let ip = |at: usize| -> (u8, u8, u8, u8) { (frame[at], frame[at + 1], frame[at + 2], frame[at + 3]) };
    let kind = u16::from_be_bytes([frame[12], frame[13]]);
    let _ = write!(line, "{} > {} ", Mac(&frame[6..12].try_into().unwrap()), Mac(&frame[0..6].try_into().unwrap()));
    match kind {
        0x0806 if frame.len() >= 42 => {
            let (sender, target) = (ip(28), ip(38));
            if frame[21] == 1 {
                let _ = write!(line, "ARP who has {}.{}.{}.{}? tell {}.{}.{}.{}", target.0, target.1, target.2, target.3, sender.0, sender.1, sender.2, sender.3);
            } else {
                let _ = write!(line, "ARP {}.{}.{}.{} is at {}", sender.0, sender.1, sender.2, sender.3, Mac(&frame[22..28].try_into().unwrap()));
            }
        }
        0x0800 if frame.len() >= 34 => {
            let (source, destination) = (ip(26), ip(30));
            let header = usize::from(frame[14] & 15) * 4;
            let _ = write!(line, "IPv4 {}.{}.{}.{} > {}.{}.{}.{} ", source.0, source.1, source.2, source.3, destination.0, destination.1, destination.2, destination.3);
            let ports = |frame: &[u8]| {
                let at = 14 + header;
                (frame.get(at..at + 4).map_or(0, |bytes| u16::from_be_bytes([bytes[0], bytes[1]])), frame.get(at..at + 4).map_or(0, |bytes| u16::from_be_bytes([bytes[2], bytes[3]])))
            };
            match frame[23] {
                1 => {
                    let _ = write!(line, "ICMP type {}", frame.get(14 + header).copied().unwrap_or(0));
                }
                6 => {
                    let (a, b) = ports(frame);
                    let _ = write!(line, "TCP {a} > {b}");
                }
                17 => {
                    let (a, b) = ports(frame);
                    let _ = write!(line, "UDP {a} > {b}");
                    match (a, b) {
                        (67 | 68, _) | (_, 67 | 68) => {
                            let _ = write!(line, " (DHCP)");
                        }
                        (53, _) | (_, 53) => {
                            let _ = write!(line, " (DNS)");
                        }
                        (9875, _) | (_, 9875) => {
                            let _ = write!(line, " (SAP)");
                        }
                        (5004..=5100, _) | (_, 5004..=5100) => {
                            let _ = write!(line, " (RTP?)");
                        }
                        _ => {}
                    }
                }
                other => {
                    let _ = write!(line, "protocol {other}");
                }
            }
        }
        0x86dd => {
            let _ = write!(line, "IPv6");
        }
        0x8100 => {
            let _ = write!(line, "VLAN tag");
        }
        0x22f0 => {
            let _ = write!(line, "AVTP (IEEE 1722)");
        }
        0x88b5 => {
            let _ = write!(line, "fos test frame");
        }
        0x88b6 => {
            let _ = write!(line, "fos terminal/chat tag {:#04x}", frame.get(14).copied().unwrap_or(0));
        }
        0x88ba => {
            let _ = write!(line, "audio-net raw audio");
        }
        0x88cc => {
            let _ = write!(line, "LLDP");
        }
        other => {
            let _ = write!(line, "type {other:#06x}");
        }
    }
}

/// `nic sniff [N]`: shows every frame the interface hears (it listens to everything, not just its own traffic) until
/// Escape. The terminal's own frames are not shown, or the display would feed on itself.
fn nic_sniff(argument: Option<&str>) {
    let index = match argument.and_then(|word| word.parse::<usize>().ok()) {
        Some(number) if number >= 1 && number <= nic_count() => number - 1,
        Some(_) => return crate::println!("nic sniff: no such interface (`nic` lists them)."),
        None => unsafe { ACTIVE_NIC }.min(nic_count().saturating_sub(1)),
    };
    let Some(nic) = nic_at(index) else { return crate::println!("No network interface.") };
    nic.set_promiscuous(true);
    crate::println!("Sniffing NIC {} (promiscuous). Escape stops.", index + 1);
    blocking(true);
    let (mut shown, mut skipped) = (0u32, 0u32);
    while !break_requested() {
        let mut budget = 40u32; // lines per pass: a busy network must not make the screen the bottleneck
        drain_nics(|from, frame| {
            if from != index {
                return;
            }
            if budget == 0 {
                skipped += 1;
                return;
            }
            budget -= 1;
            shown += 1;
            let mut line = Line::new();
            describe_frame(frame, &mut line);
            crate::println!("{shown:>5} {:>4}B {}", frame.len(), line.as_str());
        });
        crate::debug::pet();
        unsafe { core::arch::asm!("hlt") };
    }
    blocking(false);
    if let Some(nic) = nic_at(index) {
        nic.set_promiscuous(false);
    }
    crate::println!("Stopped: {shown} frame(s) shown{}.", if skipped > 0 { ", some skipped to keep up" } else { "" });
}

const MAX_PEERS: usize = 32;
/// Who has announced itself or answered a probe: (MAC, name, name length, interface).
static mut PEERS: [([u8; 6], [u8; 20], usize, usize); MAX_PEERS] = [([0; 6], [0; 20], 0, 0); MAX_PEERS];
static mut PEER_COUNT: usize = 0;

/// Files a peer's name; true if the peer is new. A known peer's name is replaced (its `hostname` file may have changed).
fn remember_peer(index: usize, mac: [u8; 6], name: &[u8]) -> bool {
    // Some virtual networks echo our own frames back; this machine is not its own peer.
    if (0..nic_count()).any(|own| nic_at(own).is_some_and(|nic| nic.mac() == mac)) {
        return false;
    }
    let length = name.len().min(20);
    unsafe {
        let peers = &mut *core::ptr::addr_of_mut!(PEERS);
        let count = PEER_COUNT;
        if let Some(entry) = peers[..count].iter_mut().find(|entry| entry.0 == mac) {
            if length != 0 {
                entry.1 = [0; 20];
                entry.1[..length].copy_from_slice(&name[..length]);
                entry.2 = length;
            }
            entry.3 = index;
            return false;
        }
        if count < MAX_PEERS {
            peers[count] = (mac, [0; 20], length, index);
            peers[count].1[..length].copy_from_slice(&name[..length]);
            PEER_COUNT = count + 1;
            return true;
        }
    }
    false
}

/// A tape's `peer_info`: peer `number` (1-based; 0 only counts) into `out`.
pub fn peer_info(number: usize, out: &mut fosfor_abi::tape::PeerInfo) -> usize {
    let count = unsafe { PEER_COUNT };
    if number >= 1 && number <= count {
        let (mac, name, length, nic) = unsafe { (*core::ptr::addr_of!(PEERS))[number - 1] };
        out.mac = mac;
        out.nic = nic as u8;
        out.name_length = length as u8;
        out.name = name;
    }
    count
}

fn print_peers() {
    let count = unsafe { PEER_COUNT };
    if count == 0 {
        return crate::println!("No peers known yet (`nic discover` looks for them).");
    }
    for number in 0..count {
        let (mac, name, length, nic) = unsafe { (*core::ptr::addr_of!(PEERS))[number] };
        crate::println!("{:>2}: {}  {}  (via NIC {})", number + 1, Mac(&mac), core::str::from_utf8(&name[..length]).unwrap_or("?"), nic + 1);
    }
}

/// `nic discover`: asks every interface who else runs fos (a broadcast probe, repeated each second for three seconds)
/// and lists the answers with their host names. Machines also answer on their own, whenever they are running.
fn nic_discover() {
    if nic_count() == 0 {
        return crate::println!("No network interface.");
    }
    crate::println!("Looking for other fos machines (3 s)...");
    unsafe { PEER_COUNT = 0 };
    blocking(true);
    let started = apic::ticks();
    let mut next_probe = started;
    while apic::ticks() < started + 3 * apic::TICK_HZ && !break_requested() {
        if apic::ticks() >= next_probe {
            next_probe += apic::TICK_HZ;
            for index in 0..nic_count() {
                if let Some(nic) = nic_at(index) {
                    let mut name = [0u8; 20];
                    let length = hostname(index, &mut name);
                    send(nic, &[0xff; 6], TAG_DISCOVER_REQUEST, &name[..length]);
                }
            }
        }
        drain_nics(|index, frame| {
            if frame.len() >= 15 && frame[12..14] == ETHERTYPE && frame[14] == TAG_DISCOVER_REPLY {
                let name = netfs_name(&frame[15..]); // the frame is padded with zeros
                remember_peer(index, frame[6..12].try_into().unwrap(), name);
            }
        });
        crate::debug::pet();
        unsafe { core::arch::asm!("hlt") };
    }
    blocking(false);
    print_peers();
}

/// `nic map`: which interfaces hear which. Every interface with a link sends a probe and the others listen; with
/// two ports cabled together this shows the cable. It retries up to three rounds while nothing is heard (links
/// coming up take a moment).
fn nic_map() {
    let count = nic_count();
    if count == 0 {
        return crate::println!("No network interface.");
    }
    let mut heard = [[false; MAX_NICS]; MAX_NICS];
    blocking(true);
    for round in 0..3 {
        for from in 0..count {
            let Some(nic) = nic_at(from) else { continue };
            if !nic.link_up() {
                continue;
            }
            let mut frame = [0u8; 14 + 8];
            frame[0..6].copy_from_slice(&[0xff; 6]);
            frame[6..12].copy_from_slice(&nic.mac());
            frame[12..14].copy_from_slice(&[0x88, 0xb9]);
            frame[14..20].copy_from_slice(b"FOSMAP");
            frame[20] = from as u8;
            nic.send(&frame);
            let until = apic::ticks() + apic::TICK_HZ / 5;
            while apic::ticks() < until {
                drain_nics(|to, received| {
                    if received.len() >= 21 && received[12..14] == [0x88, 0xb9] && &received[14..20] == b"FOSMAP" {
                        let sender = usize::from(received[20]);
                        // Only our own interfaces' probes count (another machine on the same network sends some too).
                        if sender < MAX_NICS && sender != to && nic_at(sender).is_some_and(|nic| nic.mac() == received[6..12]) {
                            heard[sender][to] = true;
                        }
                    }
                });
                crate::debug::pet();
            }
        }
        if heard.iter().any(|row| row.iter().any(|&seen| seen)) || break_requested() {
            break;
        }
        crate::println!("Nothing heard yet (round {})...", round + 1);
    }
    blocking(false);
    for from in 0..count {
        let Some(nic) = nic_at(from) else { continue };
        crate::print(format_args!("NIC {} {} ({}): ", from + 1, Mac(&nic.mac()), if nic.link_up() { "link up" } else { "link DOWN" }));
        let mut any = false;
        for to in 0..count {
            if heard[from][to] {
                crate::print(format_args!("{}NIC {}", if any { ", " } else { "heard by " }, to + 1));
                any = true;
            }
        }
        crate::println!("{}", if any { "" } else { "heard by nobody" });
    }
}

fn nic_send_test() {
    let mut mac = [0u8; 6];
    if !nic_mac(0, &mut mac) {
        return crate::println!("No network interface.");
    }
    let payload = b"fos raw ethernet test frame";
    let mut frame = [0u8; 14 + 27];
    frame[0..6].copy_from_slice(&[0xff; 6]);
    frame[6..12].copy_from_slice(&mac);
    frame[12..14].copy_from_slice(&[0x88, 0xb5]);
    frame[14..].copy_from_slice(payload);
    if nic_send(0, &frame) {
        crate::println!("Sent {} bytes: broadcast test frame (ethertype 0x88b5).", frame.len());
    } else {
        crate::println!("Send failed.");
    }
}

/// `nic recv`: waits up to three seconds for a frame that is not the terminal's, then dumps it.
fn nic_recv_test() {
    // Blocking mode keeps typed commands that arrive meanwhile from running nested inside this wait
    // (each nesting adds big stack frames); Escape ends the wait instead.
    blocking(true);
    unsafe { QUEUE_ALL = true };
    nic_recv_wait();
    unsafe { QUEUE_ALL = false };
    blocking(false);
}

fn nic_recv_wait() {
    let deadline = apic::ticks() + 3 * apic::TICK_HZ;
    while apic::ticks() < deadline && !break_requested() {
        poll();
        for index in 0..nic_count() {
            let mut buffer = [0u8; 1600];
            let length = pop_frame(index, &mut buffer);
            if length >= 14 {
                let f = &buffer[..length];
                crate::println!(
                    "Received {length} bytes: dst {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} src {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} ethertype 0x{:02x}{:02x}",
                    f[0], f[1], f[2], f[3], f[4], f[5], f[6], f[7], f[8], f[9], f[10], f[11], f[12], f[13]
                );
                return dump_bytes(0, f);
            }
        }
        crate::debug::pet();
        unsafe { core::arch::asm!("hlt") };
    }
    crate::println!("No frame received (timed out waiting).");
}
