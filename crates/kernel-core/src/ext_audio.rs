//! Audio devices whose driver lives in the kernel binary rather than in this crate, as `audio` sees them: each is a table
//! of functions the kernel registers, or nothing, in which case the device is never offered.
//!
//! Slot `I2S` is the Pi's I2S audio HAT (`i2s_pi.rs`, through `i2s::set`): it pokes the Pi's clock manager, GPIO and PCM
//! block. Slot `CARD` is a PCI audio card whose driver the server kernel registers itself (through `ext_audio::set`; the
//! optional drivers of docs/private.md use it). docs/big-kernel.md, stage 1b.

use core::sync::atomic::{AtomicPtr, Ordering};
use fosfor_abi::tape::{AudioInfo, AudioStats};

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
}

/// One registered device, or none.
pub struct Slot(AtomicPtr<Ops>);

pub const I2S: usize = 0;
pub const CARD: usize = 1;
pub const COUNT: usize = 2;

static SLOTS: [Slot; COUNT] = [Slot(AtomicPtr::new(core::ptr::null_mut())), Slot(AtomicPtr::new(core::ptr::null_mut()))];

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
}
