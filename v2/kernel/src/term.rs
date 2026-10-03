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

use crate::e1000::E1000;
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

pub const MAX_NICS: usize = 4;
static mut NICS: [Option<E1000>; MAX_NICS] = [const { None }; MAX_NICS];
static mut NIC_COUNT: usize = 0;
/// The interface the open session's HELLO arrived on: console output goes out of it.
static mut SESSION_NIC: usize = 0;
static mut PEER: Option<[u8; 6]> = None;
static mut RELAY: [u8; 16384] = [0; 16384];
static mut RELAY_LENGTH: usize = 0;
static mut RUNTIME_SERVICES: u64 = 0;

static mut LINE: [u8; 120] = [0; 120];
static mut LINE_LENGTH: usize = 0;
static mut LAST_WAS_CR: bool = false;

/// Adds a network interface; returns its zero-based index.
pub fn install(nic: E1000, runtime_services: u64) -> Option<usize> {
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

pub fn nic_count() -> usize {
    unsafe { NIC_COUNT }
}

fn nic_at(index: usize) -> Option<&'static mut E1000> {
    unsafe { (*core::ptr::addr_of_mut!(NICS)).get_mut(index)?.as_mut() }
}

/// The session's interface.
fn nic() -> Option<&'static mut E1000> {
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

fn send(nic: &mut E1000, destination: &[u8; 6], tag: u8, payload: &[u8]) -> bool {
    let mut frame = [0u8; 15 + DATA_CAPACITY];
    frame[0..6].copy_from_slice(destination);
    frame[6..12].copy_from_slice(&nic.mac);
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
            handle_frame(index, &buffer[..length]);
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
        TAG_HELLO => {
            // A new HELLO replaces any earlier session (the old client is gone or
            // restarting); the screen and serial port show who connected.
            unsafe {
                *core::ptr::addr_of_mut!(PEER) = Some(source);
                SESSION_NIC = index;
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
        FRAME_HEAD = [0; MAX_NICS];
        FRAME_TAIL = [0; MAX_NICS];
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

/// Everything a tape callback must keep serviced: USB keyboards, local keys, the
/// network (terminal frames handled, the rest queued per interface), console output out.
fn tape_service() {
    crate::debug::pet();
    crate::usb_hid::poll();
    while let Some(key) = crate::keyboard::pop() {
        tape_input_push(if key == 0x1b { TAPE_ABORT } else { u16::from(key) });
    }
    let mut buffer = [0u8; 1600];
    for index in 0..nic_count() {
        loop {
            let Some(length) = nic_at(index).and_then(|nic| nic.receive(&mut buffer)) else { break };
            if handle_frame(index, &buffer[..length]) {
                continue;
            }
            unsafe {
                let head = FRAME_HEAD[index];
                let next = (head + 1) % FRAME_FIFO;
                if next == FRAME_TAIL[index] {
                    FRAME_TAIL[index] = (FRAME_TAIL[index] + 1) % FRAME_FIFO; // full: drop the oldest
                    FRAMES_DROPPED += 1;
                }
                let slot = index * FRAME_FIFO + head;
                core::slice::from_raw_parts_mut((core::ptr::addr_of_mut!(FRAMES) as *mut u8).add(slot * 1600), length).copy_from_slice(&buffer[..length]);
                FRAME_LENGTHS[slot] = length;
                FRAME_HEAD[index] = next;
                FRAMES_QUEUED += 1;
            }
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
            *out = nic.mac;
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
    match word {
        "help" => {
            crate::println!("help      this list");
            crate::println!("about     what this is");
            crate::println!("memory    physical memory statistics");
            crate::println!("pci       list PCI functions");
            crate::println!("nic       the network interface");
            crate::println!("disks     disks found by the drivers");
            crate::println!("ls        list FOSFS files");
            crate::println!("cat NAME  show a file");
            crate::println!("write NAME TEXT   create or replace a file");
            crate::println!("rm NAME   delete a file");
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
            crate::println!("fos v2: UEFI loader, 64-bit kernel. Milestone M3: PCI, e1000, term net.");
            crate::println!("Design: docs/v2-design.md.");
        }
        "memory" => {
            let stats = memory::stats();
            crate::println!("Managed:   {} MiB ({} frames of 4 KiB)", stats.frames * memory::FRAME / (1024 * 1024), stats.frames);
            crate::println!("Free:      {} MiB ({} frames)", stats.free * memory::FRAME / (1024 * 1024), stats.free);
        }
        "pci" => {
            for device in pci::devices() {
                crate::println!(
                    "{:02x}:{:02x}.{} {:04x}:{:04x} {}",
                    device.bus,
                    device.device,
                    device.function,
                    device.vendor,
                    device.id,
                    pci::class_name(device.class, device.subclass)
                );
            }
        }
        "nic" => {
            if nic_count() == 0 {
                crate::println!("No network interface.");
            }
            for index in 0..nic_count() {
                if let Some(nic) = nic_at(index) {
                    crate::println!("{}: e1000 {}  {}{}", index + 1, Mac(&nic.mac), if nic.link_up() { "link up" } else { "link DOWN" }, if unsafe { SESSION_NIC } == index && session_open() { "  (terminal)" } else { "" });
                    crate::println!("   frames: {} received, {} sent", nic.frames_received, nic.frames_sent);
                    if index == 0 {
                        crate::println!("   tape frame queue: {} queued, {} dropped", unsafe { FRAMES_QUEUED }, unsafe { FRAMES_DROPPED });
                    }
                }
            }
        }
        "tape" => crate::tape::command(command.strip_prefix("tape").unwrap_or("")),
        "run" => crate::tape::run_named(command.strip_prefix("run").unwrap_or("")),
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
        "ls" => files(|volume| {
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
            let name = words.next().unwrap_or("");
            files(|volume| match read_file(&volume, name.as_bytes()) {
                Ok(Some(length)) => {
                    let text = unsafe { core::slice::from_raw_parts(core::ptr::addr_of!(FILE_BUFFER) as *const u8, length) };
                    crate::print(format_args!("{}", String8(text)));
                    if !text.ends_with(b"\n") {
                        crate::println!();
                    }
                }
                Ok(None) => crate::println!("cat: no such file."),
                Err(error) => crate::println!("cat: {error:?}"),
            });
        }
        "write" => {
            let name = words.next().unwrap_or("");
            let text = command[command.find(name).map_or(0, |at| at + name.len())..].trim_start();
            files(|volume| match volume.write(name.as_bytes(), text.as_bytes()) {
                Ok(()) => crate::println!("Wrote {} bytes to {name}.", text.len()),
                Err(error) => crate::println!("write: {error:?}"),
            });
        }
        "rm" => {
            let name = words.next().unwrap_or("");
            files(|volume| match volume.remove(name.as_bytes()) {
                Ok(true) => crate::println!("Removed {name}."),
                Ok(false) => crate::println!("rm: no such file."),
                Err(error) => crate::println!("rm: {error:?}"),
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

/// Runs `action` on the boot volume, or says there is none.
fn files(action: impl FnOnce(crate::fosfs::Volume)) {
    match crate::storage::boot_volume() {
        Some(volume) => action(volume),
        None => crate::println!("No FOSFS volume is mounted (see `disks`)."),
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
