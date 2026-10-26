//! Loopback audio devices: `COUNT` virtual devices (`loop0`..) behind the audio facade (`audio.rs`), so one tape can
//! play into a device and another capture from it, with no hardware in between.
//!
//! Each device is a plain FIFO. Its *playback end* (`audio_write`, picked with `outdev=loopN` or `audio output`) fills
//! it and its *capture end* (`audio_read`, picked with `device=loopN` or `audio device`) drains it. Each end belongs to the
//! context (foreground or service) that started it: `start` refuses a second holder with "already running" and `stop`
//! leaves an end alone that another context started. That is what lets a service play into a loopback while another tape
//! captures from it, with `device=loopN` on both (the audio claims of `audio.rs` do not apply to loopbacks).
//!
//! The format is fixed: stereo, 32-bit IEEE float, 48000 Hz (`audio_float` reports float for it). There is no clock of
//! its own, so the pace is the player's: it paces itself on its own source (a hardware capture or output, a timer) and
//! the capture end reads what has arrived (0 when nothing has). The FIFO has `RING_BYTES` of room, about 85 ms:
//! `playback_space` tells the player how much fits, and a write returns what fitted. With no capture end running the
//! playing end discards, like a real output with nothing plugged in, so a player never stalls on a missing listener.
//!
//! Everything runs from tape calls and the shell in one cooperative context at a time (no interrupts touch it), so plain
//! statics are enough.

use fosfor_abi::tape::{AudioInfo, AudioStats};

pub const COUNT: usize = 4;
pub const RATE: u32 = 48_000;
/// Bytes per frame: two channels of 4-byte floats.
const FRAME: usize = 8;
/// A power of two and a multiple of `FRAME`, so a frame never straddles the wrap.
const RING_BYTES: usize = 64 * 1024;

struct Loop {
    ring: [u8; RING_BYTES],
    /// Total bytes ever written / read; the FIFO holds `written - read`.
    written: u64,
    read: u64,
    /// The context (`tasks::context()`) that holds each end, if one does.
    capture: Option<usize>,
    playback: Option<usize>,
    /// Writes that did not fit entirely, bytes thrown away with no capture end, and the count of both.
    short_writes: u64,
    discarded: u64,
}

impl Loop {
    const NEW: Loop = Loop { ring: [0; RING_BYTES], written: 0, read: 0, capture: None, playback: None, short_writes: 0, discarded: 0 };

    fn used(&self) -> usize {
        (self.written - self.read) as usize
    }
}

static mut LOOPS: [Loop; COUNT] = [Loop::NEW; COUNT];

fn state(index: usize) -> &'static mut Loop {
    unsafe { &mut (*core::ptr::addr_of_mut!(LOOPS))[index] }
}

pub fn present(index: usize) -> bool {
    index < COUNT
}

/// Starts the ends asked for. Both are checked before either is taken, so a refusal changes nothing. An end that starts
/// while the other is idle first empties the FIFO: nothing left over from an earlier run reaches the new listener.
pub fn start(index: usize, rate: u32, capture: bool, playback: bool) -> Result<(), &'static str> {
    let l = state(index);
    if rate != RATE {
        return Err("the device does not offer that sample rate");
    }
    if (capture && l.capture.is_some()) || (playback && l.playback.is_some()) {
        return Err("already running");
    }
    if (capture && l.playback.is_none()) || (playback && l.capture.is_none()) {
        l.read = l.written;
    }
    let me = crate::tasks::context();
    if capture {
        l.capture = Some(me);
    }
    if playback {
        l.playback = Some(me);
        l.short_writes = 0;
        l.discarded = 0;
    }
    Ok(())
}

/// Stops the ends named that the running context holds. What is still in the FIFO stays for a capture end that is still running.
pub fn stop(index: usize, capture: bool, playback: bool) {
    let l = state(index);
    let me = Some(crate::tasks::context());
    if capture && l.capture == me {
        l.capture = None;
    }
    if playback && l.playback == me {
        l.playback = None;
    }
}

pub fn read(index: usize, out: &mut [u8]) -> usize {
    let l = state(index);
    if l.capture != Some(crate::tasks::context()) {
        return 0;
    }
    let count = l.used().min(out.len()) / FRAME * FRAME;
    for (offset, byte) in out[..count].iter_mut().enumerate() {
        *byte = l.ring[(l.read as usize + offset) % RING_BYTES];
    }
    l.read += count as u64;
    count
}

pub fn write(index: usize, data: &[u8]) -> usize {
    let l = state(index);
    if l.playback != Some(crate::tasks::context()) {
        return 0;
    }
    let whole = data.len() / FRAME * FRAME;
    if l.capture.is_none() {
        l.discarded += whole as u64;
        return whole;
    }
    let count = whole.min(RING_BYTES - l.used());
    if count < whole {
        l.short_writes += 1;
    }
    for (offset, &byte) in data[..count].iter().enumerate() {
        l.ring[(l.written as usize + offset) % RING_BYTES] = byte;
    }
    l.written += count as u64;
    count
}

pub fn playback_space(index: usize) -> usize {
    let l = state(index);
    match (l.playback, l.capture) {
        (None, _) => 0,
        (Some(_), None) => RING_BYTES,
        (Some(_), Some(_)) => RING_BYTES - l.used(),
    }
}

/// (channels, bytes per sample slot) of a running end.
pub fn format(index: usize, capture: bool) -> Option<(u8, u8)> {
    let l = state(index);
    (if capture { l.capture } else { l.playback }).map(|_| (2, 4))
}

/// The one mode is automatic.
pub fn select(_capture: bool, mode: u32) -> Result<(), &'static str> {
    if mode == 0 { Ok(()) } else { Err("no such mode") }
}

pub fn print_modes() {
    crate::println!("Loopback: 2 ch, 32-bit float, 48000 Hz (the only mode).");
}

pub fn status(index: usize) {
    let l = state(index);
    crate::println!("Loopback {index}: virtual, 48000 Hz, 2 ch, 32-bit float; a tape plays into it, another captures from it.");
    crate::println!(
        "  playback end: {}, capture end: {}; {} of {} bytes waiting; {} short writes, {} bytes discarded with no capture end",
        if l.playback.is_some() { "running" } else { "stopped" },
        if l.capture.is_some() { "running" } else { "stopped" },
        l.used(),
        RING_BYTES,
        l.short_writes,
        l.discarded
    );
}

pub fn fill_info(index: usize, info: &mut AudioInfo) {
    let l = state(index);
    let mut out = AudioInfo::default();
    out.playback_channels = 2;
    out.playback_slot_bytes = 4;
    out.playback_bits = 32;
    out.capture_channels = 2;
    out.capture_slot_bytes = 4;
    out.capture_bits = 32;
    out.rate_count = 1;
    out.rates[0] = RATE;
    if l.capture.is_some() {
        out.capture_rate = RATE;
        out.capture_available = l.used() as u32;
    }
    if l.playback.is_some() {
        out.playback_rate = RATE;
        out.playback_space = playback_space(index) as u32;
    }
    *info = out;
}

pub fn fill_stats(index: usize, stats: &mut AudioStats) {
    let l = state(index);
    *stats = AudioStats {
        capture_packets: l.read / FRAME as u64,
        capture_bytes: l.read,
        capture_errors: 0,
        capture_overruns: l.short_writes,
        playback_packets: (l.written + l.discarded) / FRAME as u64,
        playback_errors: 0,
        playback_underruns: 0,
    };
}

pub fn buffer_info(index: usize) -> (u64, u64) {
    let l = state(index);
    if l.capture.is_some() { (RING_BYTES as u64, l.used() as u64) } else { (0, 0) }
}
