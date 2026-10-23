//! The aarch64 kernel's network service: answers `tools/peers.py --discover` and takes a kernel update over Ethernet (KUP),
//! with the wire format the x86 kernel speaks (`term.rs`, `tools/send_kernel_update.py`): ethertype 0x88b6, tag byte, payload.
//!
//! KUP: REQUEST carries the image length (u32 LE) and its RFC 1071 checksum (u16 LE); DATA a sequence number (u32 LE) and up
//! to 1024 bytes; STOP ends it. The image is staged in RAM, verified, written to the inactive slot (`kup_serial::install`, the
//! same code as `kup serial`) and the board resets; the loader gives the new kernel one try and falls back by itself.
//!
//! Like x86 it is off unless the `debug` file on the boot volume has a `kernel-update` line. It is served from the shell's idle
//! loop only: a running tape owns the NIC and the shell is not at its prompt, so stop the tape first. A service that owns the
//! NIC (`run NAME &1`) keeps its frames too.

use crate::{genet, memory, timer};


use crate::netfs::ETHERTYPE;
use crate::netdump::Mac;
const TAG_KUP_REQUEST: u8 = 0x3a;
const TAG_KUP_DATA: u8 = 0x3b;
const TAG_KUP_STOP: u8 = 0x3c;
const TAG_KUP_ACK: u8 = 0x3d;
const TAG_KUP_ERROR: u8 = 0x3e;
const KUP_TOO_LARGE: u8 = 1;
const KUP_CHECKSUM_MISMATCH: u8 = 2;
const KUP_DISK_ERROR: u8 = 3;
const KUP_BUSY: u8 = 4;

const STAGE_BYTES: usize = 4 * 1024 * 1024;
/// A transfer whose sender has been silent this long (ticks, 100 Hz) gives up its staging buffer.
const STALE_TICKS: u64 = 500;

struct Kup {
    active: bool,
    source: [u8; 6],
    expected: usize,
    checksum: u16,
    received: usize,
    next_sequence: u32,
    last_activity: u64,
    staging: u64,
}

static mut KUP: Kup = Kup { active: false, source: [0; 6], expected: 0, checksum: 0, received: 0, next_sequence: 0, last_activity: 0, staging: 0 };
/// Tick at which to reset after an accepted update (0 = none).
static mut RESET_AT: u64 = 0;

/// `deny-term-net` in `settings`: this machine answers nothing on the network (no announcements, discovery, terminal, file
/// service or kernel update), for a Pi deployed where it must stay silent. Only the serial console and the keyboard remain.
static mut DENIED: bool = false;
/// Set while a shell command is executing: session keys then only count as Escape (they must not nest commands).
static mut BUSY: bool = false;
static mut BREAK: bool = false;

pub fn denied() -> bool {
    unsafe { DENIED }
}

pub fn set_denied(on: bool) {
    unsafe { DENIED = on };
}

/// A command (or a blocking monitor) starts or ends; a new one forgets an old Escape.
pub fn set_busy(on: bool) {
    unsafe {
        BUSY = on;
        BREAK = false;
    }
}

/// True once the network terminal's client sent Escape while a command was running (and clears it).
pub fn take_break() -> bool {
    unsafe {
        let was = BREAK;
        BREAK = false;
        was
    }
}

fn kup() -> &'static mut Kup {
    unsafe { &mut *core::ptr::addr_of_mut!(KUP) }
}

fn send(destination: &[u8; 6], tag: u8, payload: &[u8]) {
    crate::netfs::send(0, destination, tag, payload);
}

/// True when the boot volume's `debug` file has a `kernel-update` line.
pub fn kernel_update_enabled() -> bool {
    crate::crash::debug().kernel_update
}

fn release() {
    let kup = kup();
    if kup.staging != 0 {
        memory::free(kup.staging, STAGE_BYTES as u64 / memory::FRAME);
        kup.staging = 0;
    }
    kup.active = false;
}

fn request(source: [u8; 6], payload: &[u8]) {
    let kup = kup();
    if kup.active && kup.source != source && timer::ticks().wrapping_sub(kup.last_activity) < STALE_TICKS {
        return send(&source, TAG_KUP_ERROR, &[KUP_BUSY]);
    }
    if payload.len() < 6 {
        return;
    }
    release();
    let expected = u32::from_le_bytes(payload[0..4].try_into().unwrap()) as usize;
    let checksum = u16::from_le_bytes(payload[4..6].try_into().unwrap());
    if expected == 0 || expected > STAGE_BYTES || expected.div_ceil(512) as u64 > fosfor_abi::SLOT_SECTORS {
        return send(&source, TAG_KUP_ERROR, &[KUP_TOO_LARGE]);
    }
    let Some(staging) = memory::allocate_contiguous(STAGE_BYTES as u64 / memory::FRAME, None) else {
        return send(&source, TAG_KUP_ERROR, &[KUP_TOO_LARGE]);
    };
    *kup = Kup { active: true, source, expected, checksum, received: 0, next_sequence: 0, last_activity: timer::ticks(), staging };
    crate::println!("KUP: receiving a kernel image of {expected} bytes from {}.", Mac(&source));
}

fn data(source: [u8; 6], payload: &[u8]) {
    let kup = kup();
    if !kup.active || kup.source != source || payload.len() < 4 {
        return;
    }
    let sequence = u32::from_le_bytes(payload[0..4].try_into().unwrap());
    if sequence != kup.next_sequence {
        crate::println!("KUP: chunk {sequence} arrived where {} was expected; transfer abandoned.", kup.next_sequence);
        return release();
    }
    // The chunk's size comes from the declared length, never from the (padded) frame.
    let take = (payload.len() - 4).min(1024).min(kup.expected - kup.received);
    let staging = unsafe { core::slice::from_raw_parts_mut(kup.staging as *mut u8, STAGE_BYTES) };
    staging[kup.received..kup.received + take].copy_from_slice(&payload[4..4 + take]);
    kup.received += take;
    kup.next_sequence += 1;
    kup.last_activity = timer::ticks();
}

fn stop(source: [u8; 6]) {
    let kup = kup();
    if !kup.active || kup.source != source {
        return;
    }
    let (expected, received, checksum) = (kup.expected, kup.received, kup.checksum);
    let staging = unsafe { core::slice::from_raw_parts_mut(kup.staging as *mut u8, STAGE_BYTES) };
    if received != expected {
        crate::println!("KUP: transfer ended after {received} of {expected} bytes; abandoned.");
        return release();
    }
    if fosfor_abi::internet_checksum(&staging[..expected]) != checksum {
        crate::println!("KUP: checksum mismatch; image rejected.");
        release();
        return send(&source, TAG_KUP_ERROR, &[KUP_CHECKSUM_MISMATCH]);
    }
    match crate::kup_serial::install(staging, expected) {
        Ok(slot) => {
            crate::println!("KUP: image verified and written to slot {slot} (pending); restarting in about a second to try it.");
            send(&source, TAG_KUP_ACK, &[]);
            unsafe { RESET_AT = timer::ticks() + 100 };
        }
        Err(message) => {
            crate::println!("KUP: {message}.");
            send(&source, TAG_KUP_ERROR, &[KUP_DISK_ERROR]);
        }
    }
    release();
}

/// From the shell's idle loop: serves the frames the Genet has received. Returns true if there was one (call again).
pub fn poll() -> bool {
    unsafe {
        if RESET_AT != 0 && timer::ticks() >= RESET_AT {
            RESET_AT = 0;
            crate::arch::aarch64::power::reset();
        }
    }
    if !genet::present() || crate::tasks::service_owns(0) {
        return false;
    }
    let kup = kup();
    if kup.active && timer::ticks().wrapping_sub(kup.last_activity) >= STALE_TICKS {
        crate::println!("KUP: the sender went quiet; transfer abandoned.");
        release();
    }
    if denied() {
        // Nothing is announced or served, but the NIC is drained so its ring does not fill.
        let mut buffer = [0u8; 1600];
        return genet::receive(&mut buffer) != 0;
    }
    crate::netfs::announce();
    let mut buffer = [0u8; 1600];
    let length = genet::receive(&mut buffer);
    if length != 0 {
        serve_frame(&buffer[..length]);
    }
    length != 0
}

/// A KUP transfer is arriving (NETFS must not take the shared RAM meanwhile).
pub fn kup_transfer_alive() -> bool {
    let kup = kup();
    kup.active && timer::ticks().wrapping_sub(kup.last_activity) < STALE_TICKS
}

/// Serves one received frame: discovery, NETFS and KUP. False if it is not ours (a tape or `nic sniff` may want it).
pub fn serve_frame(frame: &[u8]) -> bool {
    if frame.len() < 15 || frame[12..14] != ETHERTYPE {
        return false;
    }
    if denied() {
        return false;
    }
    let source: [u8; 6] = frame[6..12].try_into().unwrap();
    let payload = &frame[15..];
    if session_frame(frame) {
        return true;
    }
    if let Some(consumed) = crate::netfs::handle(0, source, frame[14], payload) {
        return consumed;
    }
    match frame[14] {
        TAG_KUP_REQUEST if kernel_update_enabled() => request(source, payload),
        TAG_KUP_DATA => data(source, payload),
        TAG_KUP_STOP => stop(source),
        _ => return false,
    }
    true
}

/// The network terminal's frames (HELLO, DATA, BYE). The session is the shared one (`termsession.rs`); what it means here:
/// at the prompt the keys feed the line editor, while a command runs Escape ends it, and while a tape runs they are the
/// tape's input (a lone Escape stops it). Also called from the tape's `nic_receive`, so the session keeps working then.
/// False: not a session frame.
pub fn session_frame(frame: &[u8]) -> bool {
    use crate::termsession::Frame;
    if frame.len() < 15 || frame[12..14] != ETHERTYPE || !(0x20..=0x22).contains(&frame[14]) {
        return false;
    }
    let source: [u8; 6] = frame[6..12].try_into().unwrap();
    if denied() {
        return true;
    }
    match crate::termsession::handle(0, source, frame[14], &frame[15..]) {
        Some(Frame::Hello) => {
            crate::lineedit::reset();
            crate::println!("Remote terminal connected from {}.", Mac(&source));
            crate::println!("Type `help` for the commands.");
            crate::print(format_args!("fos> "));
        }
        Some(Frame::Data(keys)) => {
            if crate::tape_aarch64::running() {
                crate::tape_aarch64::remote_keys(keys);
            } else if unsafe { BUSY } {
                if keys.contains(&0x1b) {
                    unsafe { BREAK = true };
                }
            } else {
                for &key in keys {
                    crate::shell::feed_byte(key);
                }
            }
        }
        Some(Frame::Bye) => {
            crate::println!("Remote terminal disconnected.");
            if crate::tape_aarch64::running() {
                crate::tape_aarch64::remote_keys(&[0x1b]);
            } else if unsafe { BUSY } {
                unsafe { BREAK = true };
            } else {
                crate::lineedit::reset();
                crate::print(format_args!("fos> "));
            }
        }
        Some(Frame::Ignored) | None => {}
    }
    true
}
