//! Audio devices whose driver lives in the kernel binary rather than in this crate, as `audio` sees them: each is a table
//! of functions the kernel registers, or nothing, in which case the device is never offered.
//!
//! Slot `I2S` is the Pi's I2S audio HAT (`i2s_pi.rs`, through `i2s::set`): it pokes the Pi's clock manager, GPIO and PCM
//! block. Slot `CARD` is a PCI audio card whose driver the server kernel registers itself (through `ext_audio::set`; the
//! optional drivers of docs/private.md use it). Slot `USB_DEVICE` is a USB interface with a driver of its own instead of
//! the class driver (`usb_audio.rs`), registered the same way. docs/big-kernel.md, stage 1b.

use core::sync::atomic::{AtomicPtr, Ordering};
use fosfor_abi::tape::{AudioInfo, AudioStats, MixerControl, MixerInfo, MixerMeter};

pub struct Ops {
    pub present: fn() -> bool,
    pub label: fn() -> &'static str,
    pub start: fn(u32, bool, bool) -> Result<(), &'static str>,
    pub stop: fn(),
    pub poll: fn(),
    pub read: fn(&mut [u8]) -> usize,
    pub write: fn(&[u8]) -> usize,
    pub playback_space: fn() -> usize,
    pub format: fn(bool) -> Option<(u8, u8)>,
    pub select: fn(bool, u32) -> Result<(), &'static str>,
    pub print_modes: fn(),
    pub status: fn(),
    pub fill_info: fn(&mut AudioInfo) -> bool,
    pub fill_stats: fn(&mut AudioStats),
    pub buffer_info: fn() -> (u64, u64),
    /// For `audio_devices`: the `AUDIO_KIND_*` value and the PCI location (`bus << 16 | device << 8 | function`), vendor
    /// and device ID (all 0 for a device that is not on PCI).
    pub identity: fn() -> (u32, u32, u16, u16),
    /// Whether capture (true) or playback carries 32-bit IEEE floats: the running stream's mode, else the chosen one.
    pub float: fn(bool) -> bool,
    /// Tape ABI `audio_state` for a stream that was started: `AUDIO_STATE_RUNNING`, or why the driver ended it (a negative
    /// `AUDIO_STATE_*`).
    pub state: fn() -> i32,
    /// The device's hardware mixer, for the tape ABI `mixer_*` calls (version 41), if the driver offers one.
    pub mixer: Option<&'static dyn Mixer>,
}

/// A device's mixer as the tape ABI `mixer_*` calls (version 41, `abi/src/tape.rs`) see it: a matrix of linear crosspoint
/// gains, masters, meters and named controls. Each method is one tape call with the tape's memory already copied in or out;
/// the results are the ABI's (`MIXER_*`). Every call comes with the audio drivers' lock held (on mf: pinned to CPU 0 under
/// the USB lock), as the other audio calls do.
pub trait Mixer: Sync {
    fn info(&self, out: &mut MixerInfo) -> bool;
    /// The channel's name into `out`: the byte count, or `None` for a channel that does not exist.
    fn name(&self, group: u32, index: u32, out: &mut [u8]) -> Option<usize>;
    fn get(&self, output: u32, source: u32) -> i64;
    fn set(&self, output: u32, source: u32, gain: u32) -> i32;
    fn serial(&self) -> u64;
    fn meters(&self, out: &mut [MixerMeter]) -> usize;
    fn control(&self, index: u32, out: &mut MixerControl) -> bool;
    fn control_item(&self, index: u32, item: u32, out: &mut [u8]) -> Option<usize>;
    fn control_set(&self, index: u32, value: i64) -> i32;
}

/// Copies `text` into a zero-padded fixed field (truncated at a character boundary), for `MixerInfo::name` and friends.
pub fn fill_text(field: &mut [u8], text: &str) {
    field.fill(0);
    let mut end = text.len().min(field.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    field[..end].copy_from_slice(&text.as_bytes()[..end]);
}

/// Copies `text` for a `name` call: the byte count written, truncated at a character boundary.
pub fn copy_text(out: &mut [u8], text: &str) -> usize {
    let mut end = text.len().min(out.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    out[..end].copy_from_slice(&text.as_bytes()[..end]);
    end
}

/// Hundredths of a dB of a linear power ratio (`10 * log10`), without a maths library: log2 from the float's exponent and a
/// quadratic fit to its mantissa (within 0.01 dB). `MIXER_METER_FLOOR` for 0 and below it.
pub fn power_cdb(power: f32) -> i16 {
    if !(power > 0.0) {
        return fosfor_abi::tape::MIXER_METER_FLOOR;
    }
    let bits = power.to_bits();
    let exponent = ((bits >> 23) & 0xff) as i32 - 127;
    let mantissa = f32::from_bits((bits & 0x007f_ffff) | 0x3f80_0000);
    let log2 = exponent as f32 + (mantissa - 1.0) * (1.346_607 - 0.346_607 * mantissa);
    let cdb = log2 * 301.03;
    if cdb < fosfor_abi::tape::MIXER_METER_FLOOR as f32 { fosfor_abi::tape::MIXER_METER_FLOOR } else if cdb > 3000.0 { 3000 } else { cdb as i16 }
}

/// One registered device, or none.
pub struct Slot(AtomicPtr<Ops>);

pub const I2S: usize = 0;
pub const CARD: usize = 1;
pub const USB_DEVICE: usize = 2;
pub const COUNT: usize = 3;

static SLOTS: [Slot; COUNT] = [const { Slot(AtomicPtr::new(core::ptr::null_mut())) }; COUNT];

pub fn slot(index: usize) -> &'static Slot {
    &SLOTS[index]
}

pub fn set(index: usize, ops: &'static Ops) {
    SLOTS[index].0.store(ops as *const Ops as *mut Ops, Ordering::Release);
}

impl Slot {
    fn ops(&self) -> Option<&'static Ops> {
        unsafe { self.0.load(Ordering::Acquire).as_ref() }
    }
    pub fn present(&self) -> bool {
        self.ops().is_some_and(|ops| (ops.present)())
    }
    pub fn label(&self) -> &'static str {
        self.ops().map_or("", |ops| (ops.label)())
    }
    pub fn start(&self, rate: u32, capture: bool, playback: bool) -> Result<(), &'static str> {
        self.ops().map_or(Err("no audio device"), |ops| (ops.start)(rate, capture, playback))
    }
    pub fn stop(&self) {
        if let Some(ops) = self.ops() {
            (ops.stop)();
        }
    }
    pub fn poll(&self) {
        if let Some(ops) = self.ops() {
            (ops.poll)();
        }
    }
    pub fn read(&self, out: &mut [u8]) -> usize {
        self.ops().map_or(0, |ops| (ops.read)(out))
    }
    pub fn write(&self, data: &[u8]) -> usize {
        self.ops().map_or(0, |ops| (ops.write)(data))
    }
    pub fn playback_space(&self) -> usize {
        self.ops().map_or(0, |ops| (ops.playback_space)())
    }
    pub fn format(&self, capture: bool) -> Option<(u8, u8)> {
        (self.ops()?.format)(capture)
    }
    pub fn select(&self, capture: bool, mode: u32) -> Result<(), &'static str> {
        self.ops().map_or(Err("no audio device"), |ops| (ops.select)(capture, mode))
    }
    pub fn print_modes(&self) {
        if let Some(ops) = self.ops() {
            (ops.print_modes)();
        }
    }
    pub fn status(&self) {
        if let Some(ops) = self.ops() {
            (ops.status)();
        }
    }
    pub fn fill_info(&self, info: &mut AudioInfo) -> bool {
        self.ops().is_some_and(|ops| (ops.fill_info)(info))
    }
    pub fn state(&self) -> i32 {
        self.ops().map_or(fosfor_abi::tape::AUDIO_STATE_LOST, |ops| (ops.state)())
    }
    pub fn fill_stats(&self, stats: &mut AudioStats) {
        if let Some(ops) = self.ops() {
            (ops.fill_stats)(stats);
        }
    }
    pub fn buffer_info(&self) -> (u64, u64) {
        self.ops().map_or((0, 0), |ops| (ops.buffer_info)())
    }
    pub fn identity(&self) -> (u32, u32, u16, u16) {
        self.ops().map_or((0, 0, 0, 0), |ops| (ops.identity)())
    }
    pub fn mixer(&self) -> Option<&'static dyn Mixer> {
        self.ops()?.mixer
    }
    pub fn float(&self, capture: bool) -> bool {
        self.ops().is_some_and(|ops| (ops.float)(capture))
    }
}
