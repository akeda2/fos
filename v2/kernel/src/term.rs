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
pub const HOSTNAME: &str = "fos2";

static mut NIC: Option<E1000> = None;
static mut PEER: Option<[u8; 6]> = None;
static mut RELAY: [u8; 16384] = [0; 16384];
static mut RELAY_LENGTH: usize = 0;
static mut RUNTIME_SERVICES: u64 = 0;

static mut LINE: [u8; 120] = [0; 120];
static mut LINE_LENGTH: usize = 0;
static mut LAST_WAS_CR: bool = false;

pub fn install(nic: E1000, runtime_services: u64) {
    unsafe {
        *core::ptr::addr_of_mut!(NIC) = Some(nic);
        RUNTIME_SERVICES = runtime_services;
    }
}

fn nic() -> Option<&'static mut E1000> {
    unsafe { (*core::ptr::addr_of_mut!(NIC)).as_mut() }
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
    loop {
        let Some(length) = nic().and_then(|nic| nic.receive(&mut buffer)) else { break };
        let frame = &buffer[..length];
        if frame.len() < 15 || frame[12..14] != ETHERTYPE {
            continue;
        }
        let source: [u8; 6] = frame[6..12].try_into().unwrap();
        match frame[14] {
            TAG_DISCOVER_REQUEST => {
                if let Some(nic) = nic() {
                    send(nic, &source, TAG_DISCOVER_REPLY, HOSTNAME.as_bytes());
                }
            }
            TAG_HELLO => {
                // A new HELLO replaces any earlier session (the old client is gone or
                // restarting); the screen and serial port show who connected.
                unsafe {
                    *core::ptr::addr_of_mut!(PEER) = Some(source);
                    RELAY_LENGTH = 0;
                    LINE_LENGTH = 0;
                    LAST_WAS_CR = false;
                }
                crate::println!("Remote terminal connected from {}.", Mac(&source));
                crate::println!("{HOSTNAME}: type `help` for the commands.");
                prompt();
            }
            TAG_DATA if session_open() && unsafe { *core::ptr::addr_of!(PEER) } == Some(source) => {
                for &byte in &frame[15..] {
                    feed(byte);
                }
            }
            TAG_BYE if unsafe { *core::ptr::addr_of!(PEER) } == Some(source) => {
                flush();
                unsafe { *core::ptr::addr_of_mut!(PEER) = None };
                crate::println!("Remote terminal disconnected.");
                prompt();
            }
            _ => {}
        }
    }
    if session_open() {
        flush();
    }
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
            crate::println!("usb       USB controllers, devices, keyboards (usb rescan)");
            crate::println!("uptime    time since the timer started");
            crate::println!("dmesg     the boot log");
            crate::println!("shutdown  power off");
            crate::println!("reset     reset the machine");
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
        "nic" => match nic() {
            Some(nic) => {
                crate::println!("e1000 {}  {}", Mac(&nic.mac), if nic.link_up() { "link up" } else { "link DOWN" });
                crate::println!("frames: {} received, {} sent", nic.frames_received, nic.frames_sent);
            }
            None => crate::println!("No network interface."),
        },
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
            power::shutdown(unsafe { RUNTIME_SERVICES });
            crate::println!("Shutdown returned.");
        }
        "reset" => {
            crate::println!("Resetting.");
            flush();
            cpu::outb(0x64, 0xfe);
        }
        other => crate::println!("Unknown command: {other} (try `help`)."),
    }
}
