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
use crate::{apic, cpu, memory, pci, power};

const ETHERTYPE: [u8; 2] = [0x88, 0xb6];
const TAG_DISCOVER_REQUEST: u8 = 0x02;
const TAG_DISCOVER_REPLY: u8 = 0x03;
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
/// The name announced in discovery replies for interface `index`: `NAME<n>` from the
/// `hostname` file (n is the one-based interface number, as v1 does), or "fos2" (then
/// "fos2-2", "fos2-3"... for further interfaces).
fn hostname(index: usize, out: &mut [u8; 20]) -> usize {
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
    let _ = match (crate::config::hostname(), index) {
        (Some(base), _) => write!(cursor, "{base}{}", index + 1),
        (None, 0) => write!(cursor, "fos2"),
        (None, _) => write!(cursor, "fos2-{}", index + 1),
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

static mut LINE: [u8; 120] = [0; 120];
static mut LINE_LENGTH: usize = 0;
static mut LAST_WAS_CR: bool = false;

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
    let mut frame = [0u8; 15 + DATA_CAPACITY];
    frame[0..6].copy_from_slice(destination);
    frame[6..12].copy_from_slice(&nic.mac());
    frame[12..14].copy_from_slice(&ETHERTYPE);
    frame[14] = tag;
    frame[15..15 + payload.len()].copy_from_slice(payload);
    nic.send(&frame[..15 + payload.len()])
}

/// Handles whatever the NIC has received. Call this from the main loop.
pub fn poll() {
    let mut buffer = [0u8; 1600];
    for index in 0..nic_count() {
        loop {
            let Some(length) = nic_at(index).and_then(|nic| nic.receive(&mut buffer)) else { break };
            // Frames a tape may be about to want (its own load protocol, over the fos ethertype)
            // wait in the queue; the rest of a busy LAN's traffic is not worth keeping.
            if !handle_frame(index, &buffer[..length]) && length >= 14 && buffer[12..14] == ETHERTYPE {
                queue_frame(index, &buffer[..length]);
            }
        }
    }
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
    match frame[14] {
        TAG_DISCOVER_REQUEST => {
            if let Some(nic) = nic_at(index) {
                let mut name = [0u8; 20];
                let length = hostname(index, &mut name);
                send(nic, &source, TAG_DISCOVER_REPLY, &name[..length]);
            }
        }
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
                LINE_LENGTH = 0;
                LAST_WAS_CR = false;
            }
            crate::println!("Remote terminal connected from {} on interface {}.", Mac(&source), index + 1);
            crate::println!("Type `help` for the commands.");
            prompt();
        }
        TAG_DATA if session_open() && unsafe { *core::ptr::addr_of!(PEER) } == Some(source) && unsafe { SESSION_NIC } == index => {
            let payload = &frame[15..];
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
                tape_input_push(TAPE_ABORT);
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
    if expected == 0 || expected > crate::tape::STAGE_BYTES || expected.div_ceil(512) as u64 > fos_v2_abi::SLOT_SECTORS || crate::tape::staging().is_none() {
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
    if fos_v2_abi::internet_checksum(&staging[..kup.expected]) != kup.checksum {
        crate::println!("KUP: checksum mismatch; image rejected.");
        return kup_reply(index, &source, TAG_KUP_ERROR, &[KUP_CHECKSUM_MISMATCH]);
    }
    let (Some(metadata), Some(disk), Some(payload)) = (crate::debug::read_metadata(), crate::storage::boot_disk(), crate::storage::payload_start()) else {
        return kup_reply(index, &source, TAG_KUP_ERROR, &[KUP_DISK_ERROR]);
    };
    let slot = 1 - (metadata.active_slot & 1);
    let sectors = kup.expected.div_ceil(512);
    staging[kup.expected..sectors * 512].fill(0);
    if crate::block::write(disk, payload + fos_v2_abi::slot_start(u64::from(slot)), &staging[..sectors * 512]).is_err() {
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
        TAPE_INPUT_HEAD = 0;
        TAPE_INPUT_TAIL = 0;
    }
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
fn tape_service() {
    crate::debug::pet();
    crate::usb_hid::poll();
    while let Some(key) = crate::keyboard::pop() {
        if key == 0x1b {
            tape_input_push(TAPE_ABORT);
        } else if let Some(sequence) = fos_v2_abi::keymap::ansi_sequence(key) {
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
    flush();
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

fn prompt() {
    crate::print(format_args!("fos> "));
}

/// One typed byte: echo it, edit the line, run the command on Enter.
fn feed(byte: u8) {
    unsafe {
        let line = &mut *core::ptr::addr_of_mut!(LINE);
        match byte {
            b'\n' if LAST_WAS_CR => LAST_WAS_CR = false,
            b'\r' | b'\n' => {
                LAST_WAS_CR = byte == b'\r';
                crate::println!();
                let length = LINE_LENGTH;
                LINE_LENGTH = 0;
                let mut copy = [0u8; 120];
                copy[..length].copy_from_slice(&line[..length]);
                if let Ok(text) = core::str::from_utf8(&copy[..length]) {
                    execute(text.trim());
                }
                prompt();
            }
            0x08 | 0x7f => {
                LAST_WAS_CR = false;
                if LINE_LENGTH > 0 {
                    LINE_LENGTH -= 1;
                    crate::print(format_args!("\x08 \x08"));
                }
            }
            0x20..=0x7e => {
                LAST_WAS_CR = false;
                if LINE_LENGTH < line.len() {
                    line[LINE_LENGTH] = byte;
                    LINE_LENGTH += 1;
                    crate::print(format_args!("{}", byte as char));
                }
            }
            _ => LAST_WAS_CR = false,
        }
    }
}

fn execute(command: &str) {
    let mut words = command.split_whitespace();
    let Some(word) = words.next() else { return };
    if word != "format" {
        unsafe { PENDING_FORMAT = None }; // a format needs its confirmation as the very next command
    }
    match word {
        "help" => {
            crate::println!("help      this list");
            crate::println!("about     what this is");
            crate::println!("memory    physical memory statistics");
            crate::println!("pci       list PCI functions (pci read|write B:D.F ... reaches config space)");
            crate::println!("cpuinfo bootinfo memmap irqstat   what the CPU, loader and firmware report");
            crate::println!("hexdump ADDRESS [LENGTH]   dump mapped memory; io r8|w8.. PORT; mmio r32|w32.. ADDR");
            crate::println!("clear     clear the screen");
            crate::println!("nic       the network interface");
            crate::println!("disks     disks found by the drivers (the boot disk is disk1)");
            crate::println!("label [diskN|@LABEL [NAME|-]]   show or set a volume label");
            crate::println!("format diskN [NAME] [force]   erase a disk's FOSFS volume (asks to confirm)");
            crate::println!("copy SRC NAME DST [NEWNAME]   copy a file between volumes (diskN or @LABEL)");
            crate::println!("ls [diskN]        list FOSFS files");
            crate::println!("cat [diskN] NAME  show a file");
            crate::println!("write [diskN] NAME TEXT   create or replace a file");
            crate::println!("rm [diskN] NAME   delete a file");
            crate::println!("tape ...  load NAME|net, run [ARGS], save NAME, status");
            crate::println!("run NAME [ARGS]   load a tape from FOSFS and run it");
            crate::println!("usb       USB controllers, devices, keyboards (usb rescan)");
            crate::println!("uptime    time since the timer started");
            crate::println!("dmesg     the boot log");
            crate::println!("shutdown  power off");
            crate::println!("reset     restart through the firmware (reboot is the same)");
            crate::println!("debug ... status | slots | crash | hang");
            crate::println!("Remote: term net, tools/term_net.py, devloop.py work as on v1.");
        }
        "about" => {
            crate::println!("fos is a small educational 64-bit x86 operating system.");
            crate::println!("This is v2: a UEFI loader and a 64-bit kernel.");
            crate::println!("Design: docs/v2-design.md.");
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
        "format" => format_command(words),
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
        "ls" => files(&mut words, |volume, _| {
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
            files(&mut words, |volume, mut words| {
                let name = words.next().unwrap_or("");
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
            files(&mut words, |volume, mut words| {
                let name = words.next().unwrap_or("");
                // The text is everything after the name in the original line.
                let text = command[command.find(name).map_or(0, |at| at + name.len())..].trim_start();
                match volume.write(name.as_bytes(), text.as_bytes()) {
                    Ok(()) => crate::println!("Wrote {} bytes to {name}.", text.len()),
                    Err(error) => crate::println!("write: {error:?}"),
                }
            });
        }
        "rm" => {
            files(&mut words, |volume, mut words| {
                let name = words.next().unwrap_or("");
                match volume.remove(name.as_bytes()) {
                    Ok(true) => crate::println!("Removed {name}."),
                    Ok(false) => crate::println!("rm: no such file."),
                    Err(error) => crate::println!("rm: {error:?}"),
                }
            });
        }
        "usb" => {
            if words.next() == Some("rescan") {
                let added = crate::usb_hid::probe_all();
                crate::println!("{added} new keyboard(s).");
            }
            crate::usb_hid::describe_devices(|arguments| crate::println!("{}", arguments));
        }
        "uptime" => {
            let ticks = apic::ticks();
            crate::println!("{} ticks at {} Hz ({} s)", ticks, apic::TICK_HZ, ticks / apic::TICK_HZ);
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
            Some(_) => crate::println!("debug: status | slots | crash | hang"),
        },
        other => crate::println!("Unknown command: {other} (try `help`)."),
    }
}

/// Runs `action` on the volume the command names -- an optional leading `diskN` word picks
/// that disk's FOSFS data partition (mounted on the spot), `@LABEL` the volume carrying that
/// label (see `label`; unlike a disk number it does not change when the disks are found in
/// another order) -- or on the boot volume. The action gets the rest of the command's words.
fn files<'a>(words: &mut core::str::SplitWhitespace<'a>, action: impl FnOnce(crate::fosfs::Volume, core::str::SplitWhitespace<'a>)) {
    let mut rest = words.clone();
    let first = rest.next().unwrap_or("");
    if let Some(label) = first.strip_prefix('@') {
        match crate::storage::volume_labelled(label.as_bytes()) {
            Some((_, volume)) => action(volume, rest),
            None => crate::println!("@{label}: no volume carries that label (see `label`)."),
        }
        return;
    }
    let disk = first.strip_prefix("disk").and_then(|number| number.parse::<usize>().ok());
    let Some(number) = disk else {
        match crate::storage::boot_volume() {
            Some(volume) => action(volume, words.clone()),
            None => crate::println!("No FOSFS volume is mounted (see `disks`)."),
        }
        return;
    };
    match crate::storage::volume_on(number) {
        Ok(volume) => action(volume, rest),
        Err(error) => crate::println!("disk{number}: {error}."),
    }
}

/// `audio` shows the USB audio device; `audio start RATE [capture|playback|both]` / `audio stop`
/// control streams; `audio capture SECONDS [RATE]` records for a few seconds and prints, per second,
/// the packet counters and the peak level of every channel (a quick way to see that a device's inputs
/// are alive without writing a tape).
fn audio_command(mut words: core::str::SplitWhitespace) {
    use crate::usb_audio as ua;
    match words.next() {
        None => ua::status(),
        Some("stop") => {
            if let Some(controller) = ua::controller() {
                ua::stop(controller);
            }
            ua::status();
        }
        Some("start") => {
            let rate = words.next().and_then(|word| word.parse::<u32>().ok()).unwrap_or(48_000);
            let (capture, playback) = match words.next() {
                Some("capture") => (true, false),
                Some("playback") => (false, true),
                _ => (true, true),
            };
            let Some(controller) = ua::controller() else { return crate::println!("No USB audio device.") };
            match ua::start(controller, rate, capture, playback) {
                Ok(()) => ua::status(),
                Err(reason) => crate::println!("audio: {reason}."),
            }
        }
        Some("capture") => {
            let seconds = words.next().and_then(|word| word.parse::<u64>().ok()).unwrap_or(3).clamp(1, 30);
            let rate = words.next().and_then(|word| word.parse::<u32>().ok()).unwrap_or(48_000);
            let Some(controller) = ua::controller() else { return crate::println!("No USB audio device.") };
            if let Err(reason) = ua::start(controller, rate, true, false) {
                return crate::println!("audio: {reason}.");
            }
            let Some((channels, slot)) = ua::capture_format() else { return };
            let (channels, slot) = (usize::from(channels), usize::from(slot));
            let mut data = [0u8; 4096];
            for second in 1..=seconds {
                let mut peaks = [0u32; 8];
                let mut carry = 0usize; // bytes of an incomplete frame left from the previous read
                let end = apic::ticks() + apic::TICK_HZ;
                let mut frames = 0u64;
                while apic::ticks() < end {
                    crate::debug::pet();
                    crate::usb_hid::poll();
                    let frame_bytes = channels * slot;
                    let room = (4096 - carry) / frame_bytes * frame_bytes;
                    let got = ua::read(&mut data[carry..carry + room]);
                    let total = carry + got;
                    let whole = total / frame_bytes * frame_bytes;
                    for frame in data[..whole].chunks(frame_bytes) {
                        for (channel, sample) in frame.chunks(slot).enumerate().take(8) {
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
                crate::print(format_args!("[{second:>2} s] {frames} frames; peak per channel:"));
                for peak in &peaks[..channels.min(8)] {
                    crate::print(format_args!(" {peak:>8}"));
                }
                crate::println!();
                flush();
            }
            ua::status();
            ua::stop(controller);
        }
        Some("tone") => {
            let frequency = words.next().and_then(|word| word.parse::<u32>().ok()).unwrap_or(440).clamp(20, 20_000);
            let seconds = words.next().and_then(|word| word.parse::<u64>().ok()).unwrap_or(3).clamp(1, 60);
            let rate = words.next().and_then(|word| word.parse::<u32>().ok()).unwrap_or(48_000);
            let Some(controller) = ua::controller() else { return crate::println!("No USB audio device.") };
            if let Err(reason) = ua::start(controller, rate, false, true) {
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
            ua::stop(controller);
        }
        Some(_) => crate::println!("Usage: audio | audio start RATE [capture|playback|both] | audio stop | audio capture SECONDS [RATE] | audio tone [HZ] [SECONDS] [RATE]"),
    }
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

/// `copy SRC NAME DST [NEWNAME]`: one file from one volume to another, where SRC and DST are
/// `diskN` or `@LABEL`. The whole file goes through a buffer taken from the page allocator, so
/// it can be larger than the shell's own 64 KiB file buffer. The source is only read.
fn copy_command(mut words: core::str::SplitWhitespace) {
    let (Some(source), Some(name), Some(destination)) = (words.next(), words.next(), words.next()) else {
        return crate::println!("Usage: copy SRC NAME DST [NEWNAME]   (SRC and DST: diskN or @LABEL)");
    };
    let new_name = words.next().unwrap_or(name);
    let mut resolve = |word: &str| match volume_word(word) {
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

/// `format diskN [NAME] [force]` says what it would erase and waits; `format diskN confirm`
/// (the very next command) does it. Never the boot disk. A disk with a FOSFS partition has
/// only that partition's volume cleared; a blank disk gets a fresh GPT with one FOSFS
/// partition; any other disk needs `force` because its partition table is replaced.
fn format_command(mut words: core::str::SplitWhitespace) {
    let Some(target) = words.next() else {
        return crate::println!("Usage: format diskN [NAME] [force]   (then: format diskN confirm)");
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
    let (mut confirm, mut force, mut label) = (false, false, "");
    for word in words {
        match word {
            "confirm" => confirm = true,
            "force" => force = true,
            name => label = name,
        }
    }
    if !label.is_empty() && !crate::fosfs::valid_name(label.as_bytes()) {
        return crate::println!("format: a label is up to 16 lowercase letters, digits, `.`, `_` or `-`.");
    }
    if confirm {
        let pending = unsafe { (*core::ptr::addr_of_mut!(PENDING_FORMAT)).take() };
        let Some(pending) = pending.filter(|pending| pending.number == number) else {
            return crate::println!("format: nothing is waiting for confirmation on disk{number}; run `format disk{number}` first.");
        };
        return do_format(disk, number, &pending.label[..pending.label_length], pending.force);
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
    let mut stored = PendingFormat { number, label: [0; crate::fosfs::MAX_NAME], label_length: label.len(), force };
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
    let deadline = apic::ticks() + 3 * apic::TICK_HZ;
    while apic::ticks() < deadline {
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
