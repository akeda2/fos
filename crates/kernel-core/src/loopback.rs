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
//! playing end discards, like a real output with nothing plugged in: at 48000 frames a second against a ring of the same
//! size (`Idle`), so a player never stalls on a missing listener, nor runs through its whole file at once before a
//! listener that starts a moment later has heard any of it. That ring plays out in whole periods of `PERIOD_FRAMES`, as a
//! card frees its buffer: freed a frame at a time (every 21 us), a player that waits only when a write takes nothing would
//! find room on every pass and, as a service on the embedded kernel, never let the foreground tape run.
//!
//! Each device also has a small software mixer (tape ABI `mixer_*`, version 41), so a mixer tape can be tried and tested
//! with no hardware: its two sources are the two playback channels, its two outputs what the capture end reads, every
//! crosspoint and master a linear gain (0x8000 = 0 dB), plus meters and one control ("Dim", -20 dB on both outputs). It
//! starts as the identity (each playback channel to its own output at 0 dB), and while it is the identity a write copies
//! the bytes untouched, so nothing changes for a tape that never touches the mixer.
//!
//! Everything runs from tape calls and the shell in one cooperative context at a time (no interrupts touch it), so plain
//! statics are enough.

use crate::ext_audio::{copy_text, fill_text, power_cdb, Mixer};
use fosfor_abi::tape::{
    AudioInfo, AudioStats, MixerControl, MixerInfo, MixerMeter, MIXER_BAD, MIXER_CONTROL_BOOL, MIXER_GROUP_OUTPUT,
    MIXER_GROUP_PLAYBACK, MIXER_HAS_MASTERS, MIXER_HAS_METERS, MIXER_KIND_MATRIX, MIXER_MASTER, MIXER_METER_FLOOR,
};

pub const COUNT: usize = 4;
pub const RATE: u32 = 48_000;
/// Bytes per frame: two channels of 4-byte floats.
const FRAME: usize = 8;
/// A power of two and a multiple of `FRAME`, so a frame never straddles the wrap.
const RING_BYTES: usize = 64 * 1024;
/// The idle ring frees room in steps of this many frames (about 5 ms).
const PERIOD_FRAMES: u64 = 256;

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
    /// Reads and writes made by a context that does not hold that end (they return 0): a tape running in the wrong
    /// context shows here (`debug`'s SysRq dump).
    refused_reads: u64,
    refused_writes: u64,
    idle: Idle,
    mix: Mix,
}

/// The playing end with no listener: a ring of `RING_BYTES` that plays out at `RATE` on the `timer::micros` clock. `taken`
/// bytes went in since `since`; what has not played yet is what fills it.
#[derive(Clone, Copy)]
struct Idle {
    since: u64,
    taken: u64,
}

impl Idle {
    const NEW: Idle = Idle { since: 0, taken: 0 };

    fn restart(&mut self) {
        *self = Idle { since: crate::timer::micros(), taken: 0 };
    }

    /// Room in the ring now. Played dry (an underrun), it starts again from empty.
    fn room(&mut self) -> usize {
        let frames = crate::timer::micros().wrapping_sub(self.since) * RATE as u64 / 1_000_000;
        let played = frames / PERIOD_FRAMES * PERIOD_FRAMES * FRAME as u64;
        if played >= self.taken {
            self.restart();
            return RING_BYTES;
        }
        RING_BYTES - ((self.taken - played) as usize).min(RING_BYTES)
    }
}

const UNITY: u32 = 0x8000;
const MAX_GAIN: u32 = 0x10000;
/// "Dim": -20 dB on both outputs.
const DIM: f32 = 0.1;

/// The software mixer: `gain[output][source]`, `master[output]`, the meters' peak and power since the last `mixer_meters`
/// (index 0-1 the playback channels, 2-3 the outputs), and `serial`, bumped by every change.
#[derive(Clone, Copy)]
struct Mix {
    gain: [[u32; 2]; 2],
    master: [u32; 2],
    dim: bool,
    serial: u64,
    peak: [f32; 4],
    power: [f32; 4],
    frames: u32,
    /// A tape has read the meters: measure even while the mix is the identity.
    watched: bool,
}

impl Mix {
    const NEW: Mix = Mix { gain: [[UNITY, 0], [0, UNITY]], master: [UNITY; 2], dim: false, serial: 1, peak: [0.0; 4], power: [0.0; 4], frames: 0, watched: false };

    fn identity(&self) -> bool {
        self.gain == [[UNITY, 0], [0, UNITY]] && self.master == [UNITY; 2] && !self.dim
    }

    /// One frame through the matrix, with the meters updated.
    fn frame(&mut self, input: [f32; 2]) -> [f32; 2] {
        let scale = |gain: u32| gain as f32 / UNITY as f32;
        let mut out = [0.0f32; 2];
        for (o, sample) in out.iter_mut().enumerate() {
            let sum = input[0] * scale(self.gain[o][0]) + input[1] * scale(self.gain[o][1]);
            *sample = sum * scale(self.master[o]) * if self.dim { DIM } else { 1.0 };
        }
        self.measure(input, out);
        out
    }

    fn measure(&mut self, input: [f32; 2], out: [f32; 2]) {
        for (meter, value) in input.iter().chain(out.iter()).enumerate() {
            let level = if *value < 0.0 { -*value } else { *value };
            if level > self.peak[meter] {
                self.peak[meter] = level;
            }
            self.power[meter] += value * value;
        }
        self.frames += 1;
    }
}

impl Loop {
    const NEW: Loop = Loop { ring: [0; RING_BYTES], written: 0, read: 0, capture: None, playback: None, short_writes: 0, discarded: 0, refused_reads: 0, refused_writes: 0, idle: Idle::NEW, mix: Mix::NEW };

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
    crate::trace::event(crate::trace::Category::Audio, "lb-start", index as u64, u64::from(capture) | u64::from(playback) << 1);
    if capture {
        l.capture = Some(me);
    }
    if playback {
        l.playback = Some(me);
        l.short_writes = 0;
        l.discarded = 0;
        l.idle.restart();
    }
    Ok(())
}

/// Stops the ends named that the running context holds. What is still in the FIFO stays for a capture end that is still running.
pub fn stop(index: usize, capture: bool, playback: bool) {
    let l = state(index);
    let me = Some(crate::tasks::context());
    crate::trace::event(crate::trace::Category::Audio, "lb-stop", index as u64, u64::from(capture) | u64::from(playback) << 1);
    if capture && l.capture == me {
        l.capture = None;
        l.idle.restart();
    }
    if playback && l.playback == me {
        l.playback = None;
    }
}

pub fn read(index: usize, out: &mut [u8]) -> usize {
    let l = state(index);
    if l.capture != Some(crate::tasks::context()) {
        l.refused_reads += 1;
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
        l.refused_writes += 1;
        return 0;
    }
    let whole = data.len() / FRAME * FRAME;
    if l.capture.is_none() {
        // Nothing listens, but a mixer tape may be watching the meters.
        let count = whole.min(l.idle.room());
        l.idle.taken += count as u64;
        if l.mix.watched {
            for frame in data[..count].chunks_exact(FRAME) {
                let sample = |at: usize| f32::from_le_bytes([frame[at], frame[at + 1], frame[at + 2], frame[at + 3]]);
                l.mix.frame([sample(0), sample(4)]);
            }
        }
        l.discarded += count as u64;
        return count;
    }
    let count = whole.min(RING_BYTES - l.used());
    if count < whole {
        l.short_writes += 1;
    }
    if l.mix.identity() {
        for (offset, &byte) in data[..count].iter().enumerate() {
            l.ring[(l.written as usize + offset) % RING_BYTES] = byte;
        }
        if l.mix.watched {
            for frame in data[..count].chunks_exact(FRAME) {
                let sample = |at: usize| f32::from_le_bytes([frame[at], frame[at + 1], frame[at + 2], frame[at + 3]]);
                let input = [sample(0), sample(4)];
                l.mix.measure(input, input);
            }
        }
    } else {
        for (index, frame) in data[..count].chunks_exact(FRAME).enumerate() {
            let sample = |at: usize| f32::from_le_bytes([frame[at], frame[at + 1], frame[at + 2], frame[at + 3]]);
            let out = l.mix.frame([sample(0), sample(4)]);
            let at = (l.written as usize + index * FRAME) % RING_BYTES;
            l.ring[at..at + 4].copy_from_slice(&out[0].to_le_bytes());
            l.ring[at + 4..at + 8].copy_from_slice(&out[1].to_le_bytes());
        }
    }
    l.written += count as u64;
    count
}

pub fn playback_space(index: usize) -> usize {
    let l = state(index);
    match (l.playback, l.capture) {
        (None, _) => 0,
        (Some(_), None) => l.idle.room(),
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

/// One line for the SysRq dump: who holds each end, what waits, and the refused calls. Nothing for a loop never used.
pub fn describe(index: usize) {
    let l = state(index);
    if l.capture.is_none() && l.playback.is_none() && l.written == 0 && l.refused_reads == 0 && l.refused_writes == 0 {
        return;
    }
    struct Holder(Option<usize>);
    impl core::fmt::Display for Holder {
        fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
            match self.0 {
                Some(context) => write!(f, "c{context}"),
                None => f.write_str("none"),
            }
        }
    }
    let holder = Holder;
    crate::println!(
        "loop{index}: play {} capture {}, {} B waiting, refused {} reads {} writes",
        holder(l.playback),
        holder(l.capture),
        l.used(),
        l.refused_reads,
        l.refused_writes
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

/// The mixer of loopback device `index`, for `audio.rs`.
pub fn mixer(index: usize) -> &'static dyn Mixer {
    &MIXERS[index]
}

struct LoopMixer(usize);

static MIXERS: [LoopMixer; COUNT] = [LoopMixer(0), LoopMixer(1), LoopMixer(2), LoopMixer(3)];

impl LoopMixer {
    fn mix(&self) -> &'static mut Mix {
        &mut state(self.0).mix
    }
}

impl Mixer for LoopMixer {
    fn info(&self, out: &mut MixerInfo) -> bool {
        *out = MixerInfo {
            kind: MIXER_KIND_MATRIX,
            flags: MIXER_HAS_MASTERS | MIXER_HAS_METERS,
            inputs: 0,
            playbacks: 2,
            outputs: 2,
            controls: 1,
            unity: UNITY,
            max: MAX_GAIN,
            master_max: MAX_GAIN,
            ..MixerInfo::default()
        };
        let mut name = *b"Loopback 0";
        name[9] = b'0' + self.0 as u8;
        fill_text(&mut out.name, core::str::from_utf8(&name).unwrap_or("Loopback"));
        true
    }

    fn name(&self, group: u32, index: u32, out: &mut [u8]) -> Option<usize> {
        let text = match (group, index) {
            (MIXER_GROUP_PLAYBACK, 0) => "Play L",
            (MIXER_GROUP_PLAYBACK, 1) => "Play R",
            (MIXER_GROUP_OUTPUT, 0) => "Out L",
            (MIXER_GROUP_OUTPUT, 1) => "Out R",
            _ => return None,
        };
        Some(copy_text(out, text))
    }

    fn get(&self, output: u32, source: u32) -> i64 {
        let mix = self.mix();
        match (output, source) {
            (0..=1, MIXER_MASTER) => i64::from(mix.master[output as usize]),
            (0..=1, 0..=1) => i64::from(mix.gain[output as usize][source as usize]),
            _ => i64::from(MIXER_BAD),
        }
    }

    fn set(&self, output: u32, source: u32, gain: u32) -> i32 {
        let mix = self.mix();
        let gain = gain.min(MAX_GAIN);
        match (output, source) {
            (0..=1, MIXER_MASTER) => mix.master[output as usize] = gain,
            (0..=1, 0..=1) => mix.gain[output as usize][source as usize] = gain,
            _ => return MIXER_BAD,
        }
        mix.serial += 1;
        0
    }

    fn serial(&self) -> u64 {
        self.mix().serial
    }

    fn meters(&self, out: &mut [MixerMeter]) -> usize {
        let mix = self.mix();
        mix.watched = true;
        let count = out.len().min(4);
        for (meter, slot) in out[..count].iter_mut().enumerate() {
            *slot = if mix.frames == 0 {
                MixerMeter { peak_cdb: MIXER_METER_FLOOR, rms_cdb: MIXER_METER_FLOOR, ..MixerMeter::default() }
            } else {
                let peak = mix.peak[meter];
                MixerMeter {
                    peak_cdb: power_cdb(peak * peak),
                    rms_cdb: power_cdb(mix.power[meter] / mix.frames as f32),
                    flags: if peak >= 1.0 { fosfor_abi::tape::MIXER_METER_OVER } else { 0 },
                    reserved: 0,
                }
            };
        }
        mix.peak = [0.0; 4];
        mix.power = [0.0; 4];
        mix.frames = 0;
        count
    }

    fn control(&self, index: u32, out: &mut MixerControl) -> bool {
        if index != 0 {
            return false;
        }
        *out = MixerControl { kind: MIXER_CONTROL_BOOL, value: i64::from(self.mix().dim), max: 1, ..MixerControl::default() };
        fill_text(&mut out.name, "Dim");
        true
    }

    fn control_item(&self, _index: u32, _item: u32, _out: &mut [u8]) -> Option<usize> {
        None
    }

    fn control_set(&self, index: u32, value: i64) -> i32 {
        if index != 0 {
            return MIXER_BAD;
        }
        let mix = self.mix();
        mix.dim = value != 0;
        mix.serial += 1;
        0
    }
}
