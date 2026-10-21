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
use crate::{pci, power, timer};

pub(crate) use crate::netfs::*;
const TAG_HELLO: u8 = 0x20;
const TAG_DATA: u8 = 0x21;
const TAG_BYE: u8 = 0x22;
const DATA_CAPACITY: usize = 1024;
const TAG_KUP_REQUEST: u8 = 0x3a;
const TAG_KUP_DATA: u8 = 0x3b;
const TAG_KUP_STOP: u8 = 0x3c;
const TAG_KUP_ACK: u8 = 0x3d;
const TAG_KUP_ERROR: u8 = 0x3e;
const KUP_TOO_LARGE: u8 = 1;
const KUP_CHECKSUM_MISMATCH: u8 = 2;
const KUP_DISK_ERROR: u8 = 3;
const KUP_BUSY: u8 = 4;
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

pub fn nic_at(index: usize) -> Option<&'static mut Nic> {
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
pub(crate) fn break_requested() -> bool {
    while let Some(key) = crate::keyboard::pop() {
        if key == 0x1b {
            unsafe { BREAK = true };
        }
    }
    unsafe { BREAK }
}

pub(crate) fn blocking(on: bool) {
    unsafe {
        BLOCKING = on;
        BREAK = false;
    }
}

/// The NIC the nic subcommands default to (zero-based): `nic use N`.
static mut ACTIVE_NIC: usize = 0;

/// The interface the remote terminal session runs over, while one is open.
pub(crate) fn session_nic() -> Option<usize> {
    if session_open() { Some(unsafe { SESSION_NIC }) } else { None }
}

/// Frames queued for tapes and frames dropped (all interfaces).
pub(crate) fn frame_queue_counts() -> (u32, u32) {
    unsafe { (FRAMES_QUEUED, (*core::ptr::addr_of!(FRAMES_DROPPED)).iter().sum::<u32>()) }
}

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
pub(crate) fn flush() {
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
            let stamp = nic_at(index).map_or(0, |nic| nic.take_stamp());
            if !handle_frame(index, &buffer[..length]) && length >= 14 && (buffer[12..14] == ETHERTYPE || unsafe { QUEUE_ALL } || crate::tasks::service_owns(index)) {
                queue_frame(index, &buffer[..length], stamp);
            }
        }
    }
    crate::netfs::announce();
    if session_open() {
        flush();
    }
}

/// Handles one received frame if it is the terminal's or a discovery probe's business;
/// returns false for anything else (which a running tape may want).
fn handle_frame(index: usize, frame: &[u8]) -> bool {
    if frame.len() < 15 || frame[12..14] != ETHERTYPE {
        return false;
    }
    let source: [u8; 6] = frame[6..12].try_into().unwrap();
    if let Some(consumed) = crate::netfs::handle(index, source, frame[14], &frame[15..]) {
        return consumed;
    }
    match frame[14] {
        TAG_KUP_REQUEST if crate::config::debug().kernel_update => kup_request(index, source, &frame[15..]),
        TAG_KUP_DATA if crate::config::debug().kernel_update => kup_data(source, &frame[15..]),
        TAG_KUP_STOP if crate::config::debug().kernel_update => kup_stop(index, source),
        TAG_HELLO if crate::config::deny_term_net() => {} // closed by the settings file: stay silent
        // A session is open: another client's HELLO does not steal it (as in v1, where only the
        // waiting state accepts a new peer), unless the owner has been silent for five minutes.
        TAG_HELLO if session_open()
            && unsafe { *core::ptr::addr_of!(PEER) } != Some(source)
            && timer::ticks().wrapping_sub(unsafe { PEER_ACTIVITY }) < 5 * 60 * timer::TICK_HZ => {}
        TAG_HELLO => {
            // A new HELLO replaces any earlier session (the old client is gone or
            // restarting); the screen and serial port show who connected.
            unsafe {
                *core::ptr::addr_of_mut!(PEER) = Some(source);
                SESSION_NIC = index;
                PEER_ACTIVITY = timer::ticks();
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
            unsafe { PEER_ACTIVITY = timer::ticks() };
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

/// A kernel update is arriving (the NETFS staging buffer must not be disturbed).
pub(crate) fn kup_transfer_alive() -> bool {
    let kup = unsafe { &*core::ptr::addr_of!(KUP) };
    kup.active && timer::ticks().wrapping_sub(kup.last_activity) < 500
}

fn kup_reply(index: usize, destination: &[u8; 6], tag: u8, payload: &[u8]) {
    if let Some(nic) = nic_at(index) {
        send(nic, destination, tag, payload);
    }
}

fn kup_request(index: usize, source: [u8; 6], payload: &[u8]) {
    let kup = unsafe { &mut *core::ptr::addr_of_mut!(KUP) };
    // A transfer from someone else that is still alive keeps the staging buffer.
    if kup.active && kup.source != source && timer::ticks().wrapping_sub(kup.last_activity) < 500 {
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
    *kup = Kup { active: true, source, expected, checksum, received: 0, next_sequence: 0, last_activity: timer::ticks() };
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
    kup.last_activity = timer::ticks();
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
    unsafe { RESET_AT = timer::ticks() + timer::TICK_HZ };
}

/// Main-loop work: restart after an accepted kernel update.
pub fn housekeeping() {
    unsafe {
        if RESET_AT != 0 && timer::ticks() >= RESET_AT {
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
/// (a FIFO twice the hardware ring: a tape that does not poll loses the oldest traffic, like a NIC ring
/// overrun; 64 slots made `nettest` lose small frames long before the NIC did).
const FRAME_FIFO: usize = 256;
static mut FRAMES: [[u8; 1600]; MAX_NICS * FRAME_FIFO] = [[0; 1600]; MAX_NICS * FRAME_FIFO];
static mut FRAME_LENGTHS: [usize; MAX_NICS * FRAME_FIFO] = [0; MAX_NICS * FRAME_FIFO];
/// Hardware receive stamp of each queued frame (0: none), and of the frame `pop_frame` handed out last.
static mut FRAME_STAMPS: [u64; MAX_NICS * FRAME_FIFO] = [0; MAX_NICS * FRAME_FIFO];
static mut LAST_STAMP: [u64; MAX_NICS] = [0; MAX_NICS];
static mut FRAME_HEAD: [usize; MAX_NICS] = [0; MAX_NICS];
static mut FRAME_TAIL: [usize; MAX_NICS] = [0; MAX_NICS];
static mut FRAMES_DROPPED: [u32; MAX_NICS] = [0; MAX_NICS];
static mut FRAMES_QUEUED: u32 = 0;

/// Keeps a frame that is not the terminal's for a tape (`tape load net` listens for its own
/// protocol, `nic_receive` for everything else). Frames that arrive before a tape is
/// listening wait here instead of being lost.
fn queue_frame(index: usize, frame: &[u8], stamp: u64) {
    unsafe {
        let head = FRAME_HEAD[index];
        let next = (head + 1) % FRAME_FIFO;
        if next == FRAME_TAIL[index] {
            FRAME_TAIL[index] = (FRAME_TAIL[index] + 1) % FRAME_FIFO; // full: drop the oldest
            FRAMES_DROPPED[index] += 1;
        }
        let slot = index * FRAME_FIFO + head;
        core::slice::from_raw_parts_mut((core::ptr::addr_of_mut!(FRAMES) as *mut u8).add(slot * 1600), frame.len()).copy_from_slice(frame);
        FRAME_LENGTHS[slot] = frame.len();
        FRAME_STAMPS[slot] = stamp;
        FRAME_HEAD[index] = next;
        FRAMES_QUEUED += 1;
    }
}

/// Forgets frames queued for tapes (a tape about to run should not see stale traffic).
pub fn clear_frame_queue() {
    for index in 0..MAX_NICS {
        if !crate::tasks::service_owns(index) {
            clear_frame_queue_nic(index);
        }
    }
}

pub fn clear_frame_queue_nic(index: usize) {
    if index < MAX_NICS {
        unsafe {
            FRAME_HEAD[index] = 0;
            FRAME_TAIL[index] = 0;
        }
    }
}

/// Everything a tape callback must keep serviced: USB keyboards, local keys, the
/// network (terminal frames handled, the rest queued per interface), console output out.
/// Drains whatever a running tape's loop must keep draining (input, the network, USB events).
pub fn tape_poll() {
    tape_service();
}

/// Moves every frame waiting in a NIC's hardware ring into the per-interface FIFO (the terminal's own frames are
/// handled on the way). The part of `tape_service` a receive-heavy tape needs on every call; the rest runs on a timer.
fn drain_nics_to_fifo() {
    for index in 0..nic_count() {
        drain_nic_to_fifo(index);
    }
}

fn drain_nic_to_fifo(index: usize) {
    let mut buffer = [0u8; 1600];
    // At most one ring's worth per call: under a line-rate flood the ring never stays empty, and an
    // unbounded loop here would starve the tape that is supposed to consume (and send) the traffic.
    for _ in 0..256 {
        let Some(length) = nic_at(index).and_then(|nic| nic.receive(&mut buffer)) else { break };
        let stamp = nic_at(index).map_or(0, |nic| nic.take_stamp());
        if handle_frame(index, &buffer[..length]) {
            continue;
        }
        queue_frame(index, &buffer[..length], stamp);
    }
}

/// Moves the frames waiting on the NICs that services own into their queues (a service's own `nic_receive` does this
/// too; the shell's `poll` and a foreground tape's service loop keep the rings from overrunning meanwhile).
pub fn service_poll() {
    if crate::tasks::count() == 0 {
        return;
    }
    for index in 0..nic_count() {
        if crate::tasks::service_owns(index) {
            drain_nic_to_fifo(index);
        }
    }
}

/// A service's `nic_receive`: only on its own NIC.
pub fn service_receive(index: usize, out: &mut [u8]) -> usize {
    if crate::tasks::current_nic() != Some(index) {
        return 0;
    }
    if index < MAX_NICS && unsafe { FRAME_TAIL[index] != FRAME_HEAD[index] } {
        return pop_frame(index, out);
    }
    drain_nic_to_fifo(index);
    pop_frame(index, out)
}

/// True while a foreground tape owns the keys and the screen.
pub fn tape_mode() -> bool {
    unsafe { TAPE_MODE }
}

/// A service ended: its NIC goes back to the state a tape leaves it in.
pub fn nic_release(index: usize) {
    if let Some(nic) = nic_at(index) {
        nic.multicast_clear();
        nic.vlan_clear();
        nic.set_tx_offload(false);
        if let Some(card) = nic.as_e1000() {
            if card.ts_all() {
                card.ts_stamp_all(false);
            }
        }
    }
}

/// When a lone ESC byte arrived from a serial port and may still turn out to start a cursor-key sequence (0 = none pending).
static mut SERIAL_ESCAPE_AT: u64 = 0;
/// How long a serial Escape waits for the `[` of a sequence: a terminal sends the bytes back to back (a few ms at 9600 baud).
const SERIAL_ESCAPE_US: u64 = 50_000;

fn tape_service() {
    crate::debug::pet();
    crate::usb_hid::poll();
    while let Some((key, serial)) = crate::keyboard::pop_with_source() {
        if serial {
            // A terminal sends an arrow key as ESC [ A: only an Escape not followed (within SERIAL_ESCAPE_US) by `[` or `O`
            // is "stop", else every cursor key would end the tape.
            let now = timer::micros();
            if unsafe { SERIAL_ESCAPE_AT } != 0 {
                unsafe { SERIAL_ESCAPE_AT = 0 };
                if key == b'[' || key == b'O' {
                    tape_input_push(0x1b);
                    tape_input_push(u16::from(key));
                    continue;
                }
                tape_input_push(TAPE_ABORT);
            }
            if key == 0x1b {
                unsafe { SERIAL_ESCAPE_AT = now.max(1) };
                continue;
            }
            tape_input_push(u16::from(key));
        } else if key == 0x1b {
            tape_input_push(TAPE_ABORT);
        } else if let Some(sequence) = fosfor_abi::keymap::ansi_sequence(key) {
            // A cursor key (a private code from the keyboard decoders): the tape sees the bytes a
            // terminal would send, ESC [ A for Up and so on, like a `term net` client's keys.
            tape_input_push(0x1b);
            tape_input_push(u16::from(b'['));
            for &byte in sequence {
                tape_input_push(u16::from(byte));
            }
        } else if let Some(sequence) = fosfor_abi::keymap::utf8_sequence(key) {
            // å ä ö (and the euro sign) from a Nordic layout: the tape reads UTF-8, like `term net` sends it.
            for &byte in sequence {
                tape_input_push(u16::from(byte));
            }
        } else {
            tape_input_push(u16::from(key));
        }
    }
    if unsafe { SERIAL_ESCAPE_AT } != 0 && timer::micros().wrapping_sub(unsafe { SERIAL_ESCAPE_AT }) > SERIAL_ESCAPE_US {
        unsafe { SERIAL_ESCAPE_AT = 0 };
        tape_input_push(TAPE_ABORT);
    }
    drain_nics_to_fifo();
    crate::uart::poll();
    crate::netfs::announce();
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
///
/// A queued frame is handed out at once. The full service (USB, keys, serial, console) used to run on every call, which
/// at tens of thousands of frames a second cost more than the frames: it now runs only when the queue is empty, and a
/// light drain of the hardware rings every 16 frames keeps them from overrunning meanwhile.
pub fn tape_receive(index: usize, out: &mut [u8]) -> usize {
    static mut SINCE_DRAIN: u32 = 0;
    if crate::tasks::service_owns(index) {
        tape_service();
        return 0;
    }
    if index < MAX_NICS && unsafe { FRAME_TAIL[index] != FRAME_HEAD[index] } {
        unsafe {
            SINCE_DRAIN += 1;
            if SINCE_DRAIN >= 16 {
                SINCE_DRAIN = 0;
                drain_nics_to_fifo();
            }
        }
        return pop_frame(index, out);
    }
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
        LAST_STAMP[index] = FRAME_STAMPS[slot];
        FRAME_TAIL[index] = (FRAME_TAIL[index] + 1) % FRAME_FIFO;
        length
    }
}

/// The hardware receive stamp of the frame `pop_frame` (so a tape's `nic_receive`) returned last on `index`; 0 if none.
pub fn last_rx_stamp(index: usize) -> u64 {
    if index >= MAX_NICS { 0 } else { unsafe { LAST_STAMP[index] } }
}

/// Arms hardware timestamps on interface `index` (index 0-based); false if the card has none.
pub fn nic_ts_enable(index: usize) -> bool {
    nic_at(index).and_then(|nic| nic.as_e1000()).is_some_and(|card| card.ts_enable())
}

/// The card's clock in nanoseconds (0 without timestamps).
pub fn nic_ts_clock(index: usize) -> u64 {
    nic_at(index).and_then(|nic| nic.as_e1000()).filter(|card| card.ts_supported()).map_or(0, |card| card.systim())
}

/// Frequency-adjusts the card's clock by `ppb` (false without timestamps).
pub fn nic_ts_adjust(index: usize, ppb: i32) -> bool {
    nic_at(index).and_then(|nic| nic.as_e1000()).filter(|card| card.ts_supported()).map(|card| card.ts_adjust(ppb)).is_some()
}

/// Stamps every received frame (`on`) or only PTP events again; false without timestamps armed.
pub fn nic_ts_all(index: usize, on: bool) -> bool {
    nic_at(index).and_then(|nic| nic.as_e1000()).is_some_and(|card| card.ts_stamp_all(on))
}

/// Back to stamping only PTP events on every interface (after a tape that asked for every frame).
pub fn nic_ts_all_off() {
    for index in 0..nic_count() {
        if crate::tasks::service_owns(index) {
            continue;
        }
        if let Some(card) = nic_at(index).and_then(|nic| nic.as_e1000()) {
            if card.ts_all() {
                card.ts_stamp_all(false);
            }
        }
    }
}

/// Steps the card's clock by `delta` nanoseconds (false without timestamps).
pub fn nic_ts_step(index: usize, delta: i64) -> bool {
    nic_at(index).and_then(|nic| nic.as_e1000()).filter(|card| card.ts_supported()).map(|card| card.ts_step(delta)).is_some()
}

/// Sends `frame` and returns (sent, transmit stamp); the stamp is 0 when the card has none.
pub fn nic_send_stamped(index: usize, frame: &[u8]) -> (bool, u64) {
    let Some(nic) = nic_at(index) else { return (false, 0) };
    match nic.as_e1000().filter(|card| card.ts_supported()) {
        Some(card) => card.send_stamped(frame),
        None => (nic.send(frame), 0),
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

pub fn nic_multicast_join(index: usize, mac: [u8; 6], on: bool) -> bool {
    nic_at(index).is_some_and(|nic| nic.multicast_join(mac, on))
}

/// Drops every hardware multicast group on every NIC (a tape ended; the shell wants none).
pub fn nic_multicast_clear_all() {
    let mut index = 0;
    while let Some(nic) = nic_at(index) {
        if !crate::tasks::service_owns(index) {
            nic.multicast_clear();
        }
        index += 1;
    }
}

pub fn nic_vlan_filter(index: usize, vid: u16, on: bool) -> bool {
    nic_at(index).is_some_and(|nic| nic.vlan_filter(vid, on))
}

/// Drops every VLAN id (filter off) on every NIC (a tape ended).
pub fn nic_vlan_clear_all() {
    let mut index = 0;
    while let Some(nic) = nic_at(index) {
        if !crate::tasks::service_owns(index) {
            nic.vlan_clear();
        }
        index += 1;
    }
}

/// `TapeApi::nic_counters`: fills `out` with negotiated Mbit/s (0 unknown), full duplex (0/1), the six card error
/// totals (`Nic::error_counters`) and frames the kernel's tape queue dropped; returns how many it wrote.
pub fn nic_counters(index: usize, out: &mut [u64]) -> usize {
    let Some(nic) = nic_at(index) else { return 0 };
    let (speed, duplex) = nic.link_speed().map_or((0, 0), |(speed, full)| (u64::from(speed), u64::from(full)));
    let mut all = [0u64; 9];
    all[0] = speed;
    all[1] = duplex;
    if let Some(errors) = nic.error_counters() {
        all[2..8].copy_from_slice(&errors);
    }
    all[8] = u64::from(unsafe { FRAMES_DROPPED[index] });
    let count = out.len().min(all.len());
    out[..count].copy_from_slice(&all[..count]);
    count
}

pub fn nic_tx_offload(index: usize, on: bool) -> bool {
    nic_at(index).is_some_and(|nic| nic.set_tx_offload(on))
}

/// Turns transmit checksum offload off on every NIC (a tape ended; ordinary frames carry their own checksums).
pub fn nic_tx_offload_off_all() {
    let mut index = 0;
    while let Some(nic) = nic_at(index) {
        if !crate::tasks::service_owns(index) {
            nic.set_tx_offload(false);
        }
        index += 1;
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

pub use crate::netdump::{describe_frame, Line, Mac};

/// `start`: runs the `start` file again, as the boot does. A start file that itself says `start` would never end, so inside a run it is refused.
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

/// Runs the script file `reference` (`NAME.sh`, `diskN/NAME.sh`, `@LABEL/NAME.sh`) if it is a FOSFS file; false if not.
pub(crate) fn run_script_file(reference: &str, name: &str, arguments: &str) -> bool {
    let (volume, file) = match split_volume(reference) {
        Some((volume_text, file)) => match volume_word(volume_text) {
            Some(Ok((_, volume))) => (volume, file),
            _ => return false,
        },
        None => match find_file(reference.as_bytes()) {
            Some(volume) => (volume, reference),
            None => return false,
        },
    };
    if !crate::fosfs::valid_name(file.as_bytes()) || !matches!(volume.file_length(file.as_bytes()), Ok(Some(_))) {
        return false;
    }
    run_script(volume, file.as_bytes(), name, arguments);
    true
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
        crate::formatshell::cancel(); // a format needs its confirmation as the very next command
    }
    if word != "install" {
        crate::install::cancel(); // and so does an install
    }
    if word != "bootcode" {
        crate::bootcode::cancel(); // and a boot-code update
    }
    // The file commands and the common ones (shellcmds.rs) are the shared shell's (`crates/shell`, docs/shell.md); `copy` (other machines too) stays here.
    if matches!(word, "help" | "ls" | "cat" | "write" | "rm" | "cp" | "cmp" | "sleep" | "memory" | "grow" | "sector" | "run" | "start" | "dmesg" | "reset" | "reboot" | "shutdown" | "halt" | "disks" | "clear" | "echo" | "uptime" | "date" | "keymap" | "ps" | "tasks" | "kill" | "about" | "version" | "hexdump" | "fsck" | "kup" | "audio" | "label" | "format" | "debug" | "tapesync") || (word == "usb" && !matches!(words.clone().next(), Some("try" | "abort-test"))) || (word == "nic" && matches!(words.clone().next(), None | Some("list" | "sniff" | "promisc" | "peers" | "discover"))) {
        fosfor_shell::execute(&mut crate::shellhost::KernelHost, &[crate::shellcmds::COMMANDS, fosfor_shell::files::COMMANDS], command);
        return;
    }
    match word {
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
        "nic" if matches!(words.clone().next(), Some("park" | "unpark")) => {
            let park = words.next() == Some("park");
            let Some(index) = words.next().and_then(|word| word.parse::<usize>().ok()).filter(|n| (1..=nic_count()).contains(n)) else {
                crate::println!("nic park|unpark N: power the PHY of an unplugged interface down (see `nic`).");
                return;
            };
            match if park { crate::nicpark::park(index - 1) } else { crate::nicpark::unpark(index - 1) } {
                Ok(()) => crate::println!("nic {index}: {}.", if park { "parked, checked for a cable every 30 s" } else { "unparked, negotiating" }),
                Err(why) => crate::println!("nic {index}: {why}."),
            }
        }
        "nic" if matches!(words.clone().next(), Some("phy")) => {
            words.next();
            nic_phy(words.next());
        }
        "nic" if matches!(words.clone().next(), Some("ts")) => {
            words.next();
            nic_timestamp_test(words.next(), words.next(), words.next());
        }
        "nic" if matches!(words.clone().next(), Some("csum")) => {
            words.next();
            nic_checksum_test(words.next());
        }
        "nic" if matches!(words.clone().next(), Some("vlan")) => {
            words.next();
            nic_vlan_test(words.next(), words.next(), words.next());
        }
        "nic" if matches!(words.clone().next(), Some("mcast")) => {
            words.next();
            nic_multicast_test(words.next(), words.next(), words.next());
        }
        "nic" if matches!(words.clone().next(), Some("map")) => nic_map(),
        "nic" if matches!(words.clone().next(), Some("send")) => nic_send_test(),
        "nic" if matches!(words.clone().next(), Some("recv")) => nic_recv_test(),
        "cpuinfo" => crate::diag::cpuinfo(),
        "bootinfo" => crate::diag::bootinfo(),
        "memmap" => crate::diag::memmap(),
        "irqstat" => crate::diag::irqstat(),
        "io" => crate::diag::io(words),
        "mmio" => crate::diag::mmio(words),
        "gpu" => crate::gpu::command(),
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
        "tape" => crate::tape::command(command.strip_prefix("tape").unwrap_or("")),
        "hda" => crate::hda::describe(),
        "install" => crate::install::command(words),
        "bootcode" => crate::bootcode::command(words),
        "videomodes" => videomodes_command(),
        "copy" => copy_command(words),
        "usb" => match words.next() {
            Some("abort-test") => {
                // A hidden test hook: see `Controller::abort_self_test`.
                match crate::xhci::controller(0) {
                    Some(controller) => crate::println!("xHCI command abort self-test: {}.", if controller.abort_self_test() { "passed (the ring carries on)" } else { "FAILED" }),
                    None => crate::println!("xHCI command abort self-test: no xHCI controller."),
                }
            }
            Some("try") => match words.next().and_then(|word| word.parse::<u8>().ok()) {
                Some(port) => crate::usb_hid::try_port(port, words.next()),
                None => crate::println!("Usage: usb try PORT [hot|warm]   (a diagnostic: reset the port by hand, then enumerate it once)"),
            },
            _ => {}
        },
        "midi" => midi_command(&mut words),
        "power" => match words.next() {
            Some("hwp") => hwp_command(words.next()),
            word => power_command(word.and_then(|word| word.parse::<u64>().ok())),
        },
        other => {
            if !fosfor_shell::external(&mut crate::shellhost::KernelHost, command) {
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

/// `diskN/NAME` or `@LABEL/NAME` split into the volume word and the file name. `None` for anything else (a plain name, or
/// `HOST/NAME`, which names another machine).
fn split_volume(word: &str) -> Option<(&str, &str)> {
    let (volume, name) = word.split_once('/')?;
    let is_volume = volume == "esp" || volume.strip_prefix("disk").is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())) || volume.strip_prefix('@').is_some_and(|label| !label.is_empty());
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
    if disk == "esp" {
        return crate::fatvol::mount_esp().map(|fs| (0, fs, name));
    }
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
        return crate::println!("Usage: copy SRC NAME DST [NEWNAME]   (diskN, @LABEL or esp); copy HOST[:diskN]/NAME DST [NEWNAME]; copy SRC NAME HOST[:diskN] [NEWNAME]");
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
        return crate::println!("Usage: copy SRC NAME DST [NEWNAME]   (diskN, @LABEL or esp); copy HOST[:diskN]/NAME DST [NEWNAME]; copy SRC NAME HOST[:diskN] [NEWNAME]");
    };
    let (destination, new_name) = match split_volume(destination) {
        Some((volume, new_name)) => (volume, new_name),
        None => (destination, words.next().unwrap_or(name)),
    };
    // `copy SRC NAME HOST[:diskN] [NEWNAME]`: store a local file on another machine.
    if volume_word(destination).is_none() && destination != "esp" {
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
    let mut next = timer::micros() + 10_000;
    blocking(true);
    for (sequence, chunk) in staging[..length].chunks(NETFS_CHUNK).enumerate() {
        let mut frame = [0u8; 4 + NETFS_CHUNK];
        frame[0..4].copy_from_slice(&(sequence as u32).to_le_bytes());
        frame[4..4 + chunk.len()].copy_from_slice(chunk);
        // One frame per 3 ms (as tools/send_tape.py): the receiver polls a finite ring (the sender is not told about a lost frame).
        while timer::micros() < next {
            core::hint::spin_loop();
        }
        next = timer::micros() + 3000;
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
/// `debug video [skip|auto|size WxH|size auto]`: BIOS boots only (a flag and a monitor hint in the slot metadata).
pub(crate) fn debug_video(mut words: core::str::SplitWhitespace) {
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

/// The x86 commands that are not in the shared tables yet (docs/shell.md), listed by `help` after the shared ones.
pub(crate) fn help_extra() {
    crate::println!("copy SRC NAME DST [NEWNAME]   copy a file, also between machines (diskN, @LABEL, esp; FAT too)");
    crate::println!("pci       list PCI functions (pci read|write B:D.F ... reaches config space)");
    crate::println!("cpuinfo bootinfo memmap irqstat   what the CPU, loader and firmware report");
    crate::println!("io r8|w8.. PORT; mmio r32|w32.. ADDR   port and memory-mapped I/O");
    crate::println!("nic       the network interfaces (list); nic use|park|unpark|sniff|discover|peers|map|regs|send|recv");
    crate::println!("videomodes   the screen modes the BIOS boot path's video BIOS offered (and what the monitor told)");
    crate::println!("bootcode [update] [diskN]   show or replace the BIOS boot code and the UEFI loader (files on disk1)");
    crate::println!("install diskN     copy the boot disk (ESP, kernel, files) onto another disk");
    crate::println!("date [set YYYY-MM-DD HH:MM]   the clock (local time; the RTC)");
    crate::println!("tape ...  load NAME|net, run [ARGS], save NAME, status");
    crate::println!("NAME [ARGS]   a command that is not built in runs the script NAME.sh ($1..$9, $*, # comments) or the tape NAME");
    crate::println!("usb       USB controllers, devices, keyboards (usb rescan)");
    crate::println!("keymap    [us|sv]  the local keyboard layout (sv: Swedish/Finnish; `keymap sv` in settings keeps it)");
    crate::println!("uptime    time since the timer started; sleep N waits N seconds");
    crate::println!("power    [N]  how much of the time the CPU slept in hlt (N: sample N s at the prompt)");
    crate::println!("power hwp [performance|balanced|power]  CPU frequency scaling (Intel HWP)");
    crate::println!("Remote: term net, and tools/term_net.py, devloop.py, foscopy.py from a PC.");
}

fn parse_number(word: &str) -> Option<u64> {
    match word.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => word.parse().ok(),
    }
}

/// `nic send`: one broadcast test frame (ethertype 0x88b5, v1's TEST_ETHERTYPE) out of interface 1.
/// The interface chosen with `nic use N` (zero-based): what `nic sniff` defaults to and what a tape's single-interface calls use.
pub fn active_nic() -> usize {
    unsafe { ACTIVE_NIC }.min(nic_count().saturating_sub(1))
}

/// `nic use N`: the interface the other nic subcommands, and a tape's single-interface network calls, use.
/// `nic phy N`: what the copper PHY and its link partner advertise. A gigabit pair that links at 100 with a partner
/// offering 1000 has a cable fault (a bad pair); a partner that never offers 1000 is the far port or its settings.
fn nic_phy(arg: Option<&str>) {
    let Some(index) = arg.and_then(|word| word.parse::<usize>().ok()).filter(|n| (1..=nic_count()).contains(n)) else {
        crate::println!("nic phy N: show the copper PHY's advertised and partner abilities.");
        return;
    };
    let Some(nic) = nic_at(index - 1) else { return };
    let (Some(advertised), Some(partner), Some(control_1000), Some(status_1000)) = (nic.phy_read(4), nic.phy_read(5), nic.phy_read(9), nic.phy_read(10)) else {
        crate::println!("nic {index}: this interface's PHY cannot be read.");
        return;
    };
    let speeds = |word: u16| {
        let mut text = [0u8; 24];
        let mut length = 0;
        for (bit, name) in [(5, "10HD "), (6, "10FD "), (7, "100HD "), (8, "100FD ")] {
            if word >> bit & 1 != 0 {
                text[length..length + name.len()].copy_from_slice(name.as_bytes());
                length += name.len();
            }
        }
        (text, length)
    };
    let (ours, ours_length) = speeds(advertised);
    let (theirs, theirs_length) = speeds(partner);
    crate::println!("nic {index}: we offer  {}{}", core::str::from_utf8(&ours[..ours_length]).unwrap_or("?"), if control_1000 & 0x0200 != 0 { "1000FD" } else { "" });
    crate::println!("nic {index}: partner   {}{}", core::str::from_utf8(&theirs[..theirs_length]).unwrap_or("?"), if status_1000 & 0x0800 != 0 { "1000FD" } else { "" });
    crate::println!("nic {index}: 1000BASE-T local receiver {}, remote receiver {}", if status_1000 & 0x2000 != 0 { "OK" } else { "not OK" }, if status_1000 & 0x1000 != 0 { "OK" } else { "not OK" });
}

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
pub(crate) fn drain_nics(mut visit: impl FnMut(usize, &[u8])) {
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
            let until = timer::ticks() + timer::TICK_HZ / 5;
            while timer::ticks() < until {
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

/// `nic csum N`: turns transmit checksum offload on for interface N and broadcasts three IPv4 UDP frames (payloads of 5,
/// 26 and 27 bytes: a padded short frame, even and odd lengths) whose UDP checksum field is 0, so the card must fill it
/// in. A host on the same segment can check the sums (`tools/test_tx_offload.py`). Offload goes off again afterwards.
fn nic_checksum_test(number: Option<&str>) {
    let Some(index) = number.and_then(|word| word.parse::<usize>().ok()).filter(|n| (1..=nic_count()).contains(n)) else {
        return crate::println!("nic csum N: sends UDP frames with checksum 0 through the card's checksum offload (I350, RTL8168G/H).");
    };
    let mut mac = [0u8; 6];
    if !nic_mac(index - 1, &mut mac) {
        return;
    }
    if !nic_tx_offload(index - 1, true) {
        return crate::println!("nic {index}: no transmit checksum offload on this interface (I350, RTL8168G/H).");
    }
    for length in [5usize, 26, 27] {
        let mut frame = [0u8; 14 + 20 + 8 + 27];
        let size = 14 + 20 + 8 + length;
        frame[0..6].copy_from_slice(&[0xff; 6]);
        frame[6..12].copy_from_slice(&mac);
        frame[12..14].copy_from_slice(&[0x08, 0x00]);
        frame[14] = 0x45;
        frame[16..18].copy_from_slice(&((20 + 8 + length) as u16).to_be_bytes());
        frame[22] = 64;
        frame[23] = 17;
        frame[26..30].copy_from_slice(&[10, 99, 0, 1]);
        frame[30..34].copy_from_slice(&[10, 99, 0, 2]);
        let mut sum = 0u32;
        for pair in frame[14..34].chunks(2) {
            sum += u32::from(u16::from_be_bytes([pair[0], pair[1]]));
        }
        while sum >> 16 != 0 {
            sum = (sum & 0xffff) + (sum >> 16);
        }
        frame[24..26].copy_from_slice(&(!(sum as u16)).to_be_bytes());
        frame[34..36].copy_from_slice(&40000u16.to_be_bytes());
        frame[36..38].copy_from_slice(&40001u16.to_be_bytes());
        frame[38..40].copy_from_slice(&((8 + length) as u16).to_be_bytes());
        for (at, byte) in frame[42..size].iter_mut().enumerate() {
            *byte = b'a' + at as u8;
        }
        let sent = nic_send(index - 1, &frame[..size]);
        crate::println!("nic {index}: UDP frame with {length} payload bytes, checksum 0 to the card: {}.", if sent { "sent" } else { "FAILED" });
    }
    nic_tx_offload(index - 1, false);
}

/// `nic vlan N add|del VID | clear`: registers or drops VLAN ids in the card's VLAN filter by hand (the call tapes make,
/// `nic_vlan_filter`). With one id the card drops tagged frames of other ids; untagged frames always arrive.
fn nic_vlan_test(index: Option<&str>, action: Option<&str>, vid: Option<&str>) {
    let usage = "nic vlan N add|del VID | clear: the card's VLAN filter (E1000 family); VID 0-4095.";
    let Some(index) = index.and_then(|word| word.parse::<usize>().ok()).filter(|n| (1..=nic_count()).contains(n)) else {
        return crate::println!("{usage}");
    };
    match (action, vid.and_then(|word| word.parse::<u16>().ok())) {
        (Some("clear"), _) => {
            if let Some(nic) = nic_at(index - 1) {
                nic.vlan_clear();
            }
            crate::println!("nic {index}: VLAN filter off, no ids registered.");
        }
        (Some(action @ ("add" | "del")), Some(vid)) => {
            if nic_vlan_filter(index - 1, vid, action == "add") {
                crate::println!("nic {index}: VLAN {vid} {action} done (hardware filter).");
            } else {
                crate::println!("nic {index}: no VLAN filter for id {vid} (invalid id, or this card has none).");
            }
        }
        _ => crate::println!("{usage}"),
    }
}

/// `nic mcast N join|leave MAC | clear`: joins or leaves one multicast group in the card's hash filter by hand, the
/// call tapes make (`nic_multicast_join`), so a test can see that only the joined groups arrive.
fn nic_multicast_test(index: Option<&str>, action: Option<&str>, address: Option<&str>) {
    let usage = "nic mcast N join|leave MAC | clear: the card's multicast hash filter (E1000 family); MAC like 01:00:5e:01:02:03.";
    let Some(index) = index.and_then(|word| word.parse::<usize>().ok()).filter(|n| (1..=nic_count()).contains(n)) else {
        return crate::println!("{usage}");
    };
    match action {
        Some("clear") => {
            if let Some(nic) = nic_at(index - 1) {
                nic.multicast_clear();
            }
            crate::println!("nic {index}: no multicast groups joined.");
        }
        Some(action @ ("join" | "leave")) => {
            let mut mac = [0u8; 6];
            let mut parts = address.unwrap_or("").split(':');
            for byte in mac.iter_mut() {
                match parts.next().and_then(|part| u8::from_str_radix(part, 16).ok()) {
                    Some(value) => *byte = value,
                    None => return crate::println!("{usage}"),
                }
            }
            if nic_multicast_join(index - 1, mac, action == "join") {
                crate::println!("nic {index}: {action} {} done (hash filter).", Mac(&mac));
            } else {
                crate::println!("nic {index}: no hash filter for {} (not a multicast address, or this card has none).", Mac(&mac));
            }
        }
        _ => crate::println!("{usage}"),
    }
}

/// `nic ts N [send|listen]`: checks the card's IEEE 1588 hardware timestamps (I350). Plain: is SYSTIM running at
/// 1 ns a tick? `send`: transmit a PTP Sync frame and print its stamp against SYSTIM just before and after.
/// `listen`: for 10 s print the stamp of each PTP frame that arrives against SYSTIM when it was noticed.
fn nic_timestamp_test(number: Option<&str>, action: Option<&str>, value: Option<&str>) {
    let Some(index) = number.and_then(|word| word.parse::<usize>().ok()).filter(|n| (1..=nic_count()).contains(n)) else {
        crate::println!("nic ts N [send|listen|all|adjust PPB|step NS]: IEEE 1588 hardware timestamps of interface N (I350).");
        return;
    };
    let Some(nic) = nic_at(index - 1) else { return };
    let mac = nic.mac();
    let Some(card) = nic.as_e1000().filter(|card| card.ts_supported()) else {
        crate::println!("nic {index}: no hardware timestamps known for this interface (I350, 82579, Skylake I219).");
        return;
    };
    card.ts_enable();
    match action {
        None => {
            let (before, start) = (card.systim(), timer::ticks());
            while timer::ticks() < start + timer::TICK_HZ / 10 {
                core::hint::spin_loop();
            }
            let (after, end) = (card.systim(), timer::ticks());
            let ns = after.wrapping_sub(before) & 0xff_ffff_ffff;
            crate::println!("nic {index}: SYSTIM {after} ns; advanced {ns} ns in {} ticks of {} Hz (1 ns a tick: {} expected).", end - start, timer::TICK_HZ, (end - start) * 1_000_000_000 / timer::TICK_HZ);
        }
        Some("send") => {
            let mut frame = [0u8; 60];
            frame[..6].copy_from_slice(&[0xff; 6]); // broadcast: the card filters multicast
            frame[6..12].copy_from_slice(&mac);
            frame[12..14].copy_from_slice(if value == Some("plain") { &[0x88, 0xb5] } else { &[0x88, 0xf7] });
            if value == Some("plain") {
                frame[14..41].copy_from_slice(b"fos raw ethernet test frame");
            } else {
                frame[14] = 0x00; // messageType 0: Sync, an event message
                frame[15] = 0x02; // versionPTP 2
                frame[17] = 44; // messageLength
            }
            if value == Some("burst") {
                frame[12..14].copy_from_slice(&[0x88, 0xb5]);
                for number in 0..16u8 {
                    frame[14] = number;
                    card.send(&frame);
                }
                return crate::println!("nic {index}: 16 plain frames sent back to back.");
            }
            let before = card.systim();
            let (_, stamp) = card.send_stamped(&frame);
            let after = card.systim();
            match Some(stamp).filter(|&stamp| stamp != 0) {
                Some(stamp) => crate::println!("nic {index}: Sync sent; TX stamp {stamp}, SYSTIM before {before}, after {after} (stamp - before = {} ns).", stamp as i64 - before as i64),
                None => crate::println!("nic {index}: no transmit stamp (link down, or the card gave none)."),
            }
        }
        Some("all") => {
            if !card.ts_stamp_all(true) {
                crate::println!("nic {index}: cannot stamp every frame; listing them without stamps.");
            }
            blocking(true);
            unsafe { QUEUE_ALL = true };
            let deadline = timer::ticks() + 5 * timer::TICK_HZ;
            crate::println!("nic {index}: stamping every received frame for 5 s.");
            let (mut frames, mut stamped, mut shown, mut missing, mut previous) = (0u32, 0u32, 0, 0u32, 0u64);
            while timer::ticks() < deadline && !break_requested() {
                poll();
                let mut buffer = [0u8; 1600];
                loop {
                    let length = pop_frame(index - 1, &mut buffer);
                    if length == 0 {
                        break;
                    }
                    frames += 1;
                    let stamp = last_rx_stamp(index - 1);
                    if stamp != 0 {
                        stamped += 1;
                    }
                    let kind = u16::from_be_bytes([buffer[12], buffer[13]]);
                    // The terminal's own 0x88b6 frames would fill the list; the rest is what the caller wants to see.
                    if kind != 0x88b6 && (shown < 40 || (stamp == 0 && missing < 40)) {
                        shown += 1;
                        missing += u32::from(stamp == 0);
                        crate::println!("{length} B to {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} type {kind:#06x} stamp {stamp}, +{} ns", buffer[0], buffer[1], buffer[2], buffer[3], buffer[4], buffer[5], stamp.wrapping_sub(previous) & 0xff_ffff_ffff);
                    }
                    if stamp != 0 {
                        previous = stamp;
                    }
                }
                crate::cpu::sleep();
            }
            unsafe { QUEUE_ALL = false };
            blocking(false);
            if let Some(card) = nic_at(index - 1).and_then(|nic| nic.as_e1000()) {
                card.ts_stamp_all(false);
            }
            crate::println!("{frames} frames received, {stamped} with a stamp.");
        }
        Some("listen") => {
            blocking(true);
            unsafe { QUEUE_ALL = true };
            let deadline = timer::ticks() + 10 * timer::TICK_HZ;
            crate::println!("nic {index}: listening 10 s for PTP-over-Ethernet event frames (ethertype 0x88f7).");
            while timer::ticks() < deadline && !break_requested() {
                poll();
                let mut buffer = [0u8; 1600];
                while pop_frame(index - 1, &mut buffer) != 0 {
                    let stamp = last_rx_stamp(index - 1);
                    if stamp != 0 {
                        let now = nic_ts_clock(index - 1);
                        crate::println!("RX stamp {stamp}; SYSTIM now {now} (noticed {} ns later).", now as i64 - stamp as i64);
                    }
                }
                crate::cpu::sleep();
            }
            unsafe { QUEUE_ALL = false };
            blocking(false);
            if let Some(card) = nic_at(index - 1).and_then(|nic| nic.as_e1000()) {
                crate::println!("{} frames received so far; TSYNCRXCTL {:#x}.", card.frames_received, card.ts_rx_control());
            }
        }
        Some("adjust") | Some("step") => {
            let Some(value) = value.and_then(|word| word.parse::<i64>().ok()) else {
                return crate::println!("nic ts {index} adjust PPB | step NS: a signed number.");
            };
            if action == Some("adjust") {
                let written = card.ts_adjust(value as i32);
                let (before, start) = (card.systim(), timer::micros());
                while timer::micros() < start + 4_000_000 {
                    core::hint::spin_loop();
                }
                let (after, end) = (card.systim(), timer::micros());
                let (ns, expect) = ((after.wrapping_sub(before) & 0xff_ffff_ffff) as i64, ((end - start) * 1000) as i64);
                crate::println!("nic {index}: TIMINCA {written:#x}; over {} ms the card clock ran {} ppb against the CPU's timer (compare with adjust 0).", (end - start) / 1000, (ns - expect) * 1_000_000_000 / expect);
            } else {
                let (before, after) = (card.systim(), { card.ts_step(value); card.systim() });
                crate::println!("nic {index}: SYSTIM {before} -> {after} (asked {value} ns, got {} ns).", after as i64 - before as i64);
            }
        }
        Some(_) => crate::println!("nic ts N [send|listen|all|adjust PPB|step NS]"),
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
    let deadline = timer::ticks() + 3 * timer::TICK_HZ;
    while timer::ticks() < deadline && !break_requested() {
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
                return crate::shellcmds::dump_bytes(0, f);
            }
        }
        crate::debug::pet();
        crate::cpu::sleep();
    }
    crate::println!("No frame received (timed out waiting).");
}

/// `midi` lists the USB-MIDI devices; `midi monitor` prints what they send until Escape; `midi send HEX...` sends one
/// message (`midi send 90 3c 64` is note C4 on) to the first device, `midi send N HEX...` to device N.
fn midi_command<'a>(words: &mut impl Iterator<Item = &'a str>) {
    match words.next() {
        None => {
            crate::usb_midi::describe_devices(|arguments| crate::println!("{}", arguments));
            if crate::usb_midi::count() == 0 {
                crate::println!("midi: no USB MIDI device (`usb rescan` looks again).");
            }
        }
        Some("monitor") => {
            if crate::usb_midi::count() == 0 {
                crate::println!("midi: no USB MIDI device (`usb rescan` looks again).");
                return;
            }
            crate::println!("midi: showing what the devices send; Escape stops.");
            blocking(true);
            while !break_requested() {
                crate::usb_hid::poll();
                crate::debug::pet();
                drain_nics(|_, _| {}); // a `term net` client's Escape (and our output to it) goes through here
                while let Some((device, cable, bytes, count)) = crate::usb_midi::take() {
                    crate::println!("midi {} cable {cable}: {}", device + 1, crate::usb_midi::Describe(&bytes[..count]));
                }
                core::hint::spin_loop();
            }
            blocking(false);
        }
        Some("send") => {
            let mut numbers = [0u8; 4];
            let mut count = 0;
            let mut device = 0;
            let mut first = true;
            for word in words {
                // A leading decimal number below 10 followed by more words is a device number only when written `#N`.
                if first && word.starts_with('#') {
                    device = word[1..].parse::<usize>().unwrap_or(1).saturating_sub(1);
                    first = false;
                    continue;
                }
                first = false;
                match u8::from_str_radix(word.trim_start_matches("0x"), 16) {
                    Ok(byte) if count < numbers.len() => {
                        numbers[count] = byte;
                        count += 1;
                    }
                    _ => {
                        crate::println!("midi send: expected hex bytes, at most 3 (got {word:?}).");
                        return;
                    }
                }
            }
            match crate::usb_midi::send(device, &numbers[..count]) {
                Ok(()) => crate::println!("midi: sent {}.", crate::usb_midi::Describe(&numbers[..count])),
                Err(why) => crate::println!("midi send: {why}."),
            }
        }
        Some(_) => crate::println!("Usage: midi | midi monitor | midi send [#N] HEX HEX [HEX]"),
    }
}

/// Previous `power` reading: (micros, halted, wakeups, spun per reason), so the next call reports the time in between
/// (for instance while a tape ran).
static mut POWER_MARK: (u64, u64, u64, [u64; crate::cpu::SPIN_REASONS]) = (0, 0, 0, [0; crate::cpu::SPIN_REASONS]);

/// `power [N]`: the share of time the CPU slept in `hlt` (it draws and heats least there) and, for the rest, whether
/// it was polling on purpose (and why) or simply busy. With N it first idles at the prompt for N seconds.
fn power_command(sample_seconds: Option<u64>) {
    use crate::cpu::{idle_stats, SPIN_REASONS};
    if let Some(seconds) = sample_seconds {
        let end = timer::ticks() + seconds.min(60) * timer::TICK_HZ;
        while timer::ticks() < end {
            crate::debug::pet();
            crate::cpu::sleep();
        }
    }
    let now = timer::micros();
    let (halted, wakeups, spun) = idle_stats();
    let mark = unsafe { *core::ptr::addr_of!(POWER_MARK) };
    let report = |label: &str, span: u64, halted: u64, wakeups: u64, spun: [u64; SPIN_REASONS]| {
        let span = span.max(1);
        let percent = |part: u64| part * 100 / span;
        crate::println!(
            "{label}: {} s, halted {}%, {} wakeups/s",
            span / 1_000_000,
            percent(halted),
            wakeups * 1_000_000 / span
        );
        let polled: u64 = spun.iter().sum();
        let busy = span.saturating_sub(halted + polled);
        crate::println!(
            "   polling {}% (NIC {}%, UART {}%, serial out {}%), running {}%",
            percent(polled),
            percent(spun[0]),
            percent(spun[1]),
            percent(spun[2]),
            percent(busy)
        );
    };
    report("since boot", now, halted, wakeups, spun);
    if mark.0 != 0 {
        let delta = [0, 1, 2].map(|index| spun[index] - mark.3[index]);
        report("since last power", now - mark.0, halted - mark.1, wakeups - mark.2, delta);
    }
    unsafe { *core::ptr::addr_of_mut!(POWER_MARK) = (now, halted, wakeups, spun) };
    // The five busiest interrupt vectors since boot: a wake-up rate far above the 100 Hz tick plus the NICs' own
    // interrupts means something else keeps the CPU from sleeping.
    let mut top = [(0u64, 0usize); 5];
    for vector in 0..256 {
        let count = crate::cpu::vector_count(vector);
        if count > top[4].0 {
            top[4] = (count, vector);
            top.sort_unstable_by(|a, b| b.0.cmp(&a.0));
        }
    }
    let seconds = (now / 1_000_000).max(1);
    for (count, vector) in top.iter().filter(|entry| entry.0 > 0) {
        crate::println!("   vector {vector:#04x}: {count} interrupts ({}/s since boot)", count / seconds);
    }
    match crate::cpupower::temperature() {
        Some((margin, Some(tjmax))) => crate::println!("CPU temperature: {} C ({margin} C below the throttle point, {tjmax} C)", tjmax.saturating_sub(margin)),
        Some((margin, None)) => crate::println!("CPU temperature: {margin} C below the throttle point"),
        None => crate::println!("CPU temperature: not available (Intel digital sensor only)"),
    }
    if let Some((since_boot, since_last)) = crate::cpupower::speed_percent() {
        crate::println!("CPU speed: {since_boot}% of base since boot, {since_last}% since last power (awake time)");
    }
    if let Some((since_boot, since_last)) = crate::cpupower::power_milliwatts() {
        crate::println!("CPU package power: {}.{:02} W since boot, {}.{:02} W since last power", since_boot / 1000, since_boot % 1000 / 10, since_last / 1000, since_last % 1000 / 10);
    }
    crate::println!("INTx pins masked as storms: {}", crate::msi::storms());
    crate::println!("Not in the sums: time a tape spends busy-waiting without calling idle(), shown as running.");
}

/// `power hwp [performance|balanced|power]`: show or enable the CPU's own frequency scaling.
fn hwp_command(preference: Option<&str>) {
    if let Some(name) = preference {
        let Some(&(_, value)) = crate::cpupower::PREFERENCES.iter().find(|entry| entry.0 == name) else {
            crate::println!("power hwp [performance|balanced|power]: how eagerly the CPU may slow down.");
            return;
        };
        match crate::cpupower::enable_hwp(value) {
            Ok((lowest, highest)) => crate::println!("HWP on, {name}: speed between {lowest} and {highest}, until reset."),
            Err(why) => crate::println!("HWP: {why}."),
        }
        return;
    }
    match crate::cpupower::hwp_status() {
        None => crate::println!("HWP: not available (needs an Intel CPU with it, not a VM)"),
        Some((false, ..)) => crate::println!("HWP: off (firmware speed). `power hwp balanced` turns it on."),
        Some((true, low, high, epp)) => {
            crate::println!("HWP: on, performance range {low}..{high}");
            if let Some(epp) = epp {
                crate::println!("Energy preference {epp} (0 performance, 255 power saving)");
            }
        }
    }
}

