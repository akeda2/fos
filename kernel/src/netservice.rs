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

const ETHERTYPE: [u8; 2] = [0x88, 0xb6];
const TAG_DISCOVER_REQUEST: u8 = 0x02;
const TAG_DISCOVER_REPLY: u8 = 0x03;
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

fn kup() -> &'static mut Kup {
    unsafe { &mut *core::ptr::addr_of_mut!(KUP) }
}

fn send(destination: &[u8; 6], tag: u8, payload: &[u8]) {
    let Some(mac) = genet::mac() else { return };
    let mut frame = [0u8; 80];
    let length = (15 + payload.len()).max(60);
    frame[0..6].copy_from_slice(destination);
    frame[6..12].copy_from_slice(&mac);
    frame[12..14].copy_from_slice(&ETHERTYPE);
    frame[14] = tag;
    frame[15..15 + payload.len()].copy_from_slice(payload);
    genet::send(&frame[..length]);
}

/// True when the boot volume's `debug` file has a `kernel-update` line.
fn kernel_update_enabled() -> bool {
    let Some(volume) = crate::storage::boot_volume() else { return false };
    let mut buffer = [0u8; 1024];
    let Ok(Some(length)) = volume.read(b"debug", &mut buffer) else { return false };
    core::str::from_utf8(&buffer[..length]).unwrap_or("").lines().any(|line| line.split_whitespace().next() == Some("kernel-update"))
}

/// The name announced to `peers.py`: the `hostname` file plus the NIC number, else `fos-` and the last MAC bytes.
fn name(out: &mut [u8; 20]) -> usize {
    let mut length = 0;
    if let Some(volume) = crate::storage::boot_volume() {
        let mut buffer = [0u8; 18];
        if let Ok(Some(read)) = volume.read(b"hostname", &mut buffer) {
            let base = core::str::from_utf8(&buffer[..read]).unwrap_or("").trim();
            for byte in base.bytes().chain(core::iter::once(b'1')).take(20) {
                out[length] = byte;
                length += 1;
            }
            return length;
        }
    }
    let mac = genet::mac().unwrap_or([0; 6]);
    for (index, byte) in [b'f', b'o', b's', b'-'].into_iter().chain(mac[3..].iter().flat_map(|byte| [hex(byte >> 4), hex(byte & 15)])).enumerate() {
        out[index] = byte;
        length = index + 1;
    }
    length
}

fn hex(nibble: u8) -> u8 {
    b"0123456789abcdef"[usize::from(nibble)]
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

struct Mac<'a>(&'a [u8; 6]);

impl core::fmt::Display for Mac<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", self.0[0], self.0[1], self.0[2], self.0[3], self.0[4], self.0[5])
    }
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
    let mut buffer = [0u8; 1600];
    let length = genet::receive(&mut buffer);
    if length < 15 || buffer[12..14] != ETHERTYPE {
        return length != 0;
    }
    let source: [u8; 6] = buffer[6..12].try_into().unwrap();
    let payload = &buffer[15..length];
    match buffer[14] {
        TAG_DISCOVER_REQUEST => {
            let mut text = [0u8; 20];
            let length = name(&mut text);
            send(&source, TAG_DISCOVER_REPLY, &text[..length]);
        }
        TAG_KUP_REQUEST if kernel_update_enabled() => request(source, payload),
        TAG_KUP_DATA => data(source, payload),
        TAG_KUP_STOP => stop(source),
        _ => {}
    }
    true
}
