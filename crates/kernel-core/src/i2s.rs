//! The Pi's I2S audio HAT, as `audio` sees it: a table of functions the kernel registers (`set`), or nothing (x86, or an
//! aarch64 kernel with the driver absent), in which case no I2S device is ever offered. The driver itself (`i2s_pi.rs`)
//! pokes the Pi's clock manager, GPIO and PCM block and stays in the kernel; docs/big-kernel.md, stage 1b.

use core::ptr::{addr_of, addr_of_mut};
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
}

static mut OPS: Option<&'static Ops> = None;

pub fn set(ops: &'static Ops) {
    unsafe { *addr_of_mut!(OPS) = Some(ops) };
}

fn ops() -> Option<&'static Ops> {
    unsafe { *addr_of!(OPS) }
}

pub fn present() -> bool {
    ops().is_some_and(|ops| (ops.present)())
}
pub fn label() -> &'static str {
    ops().map_or("", |ops| (ops.label)())
}
pub fn start(rate: u32, capture: bool, playback: bool) -> Result<(), &'static str> {
    ops().map_or(Err("no audio device"), |ops| (ops.start)(rate, capture, playback))
}
pub fn stop() {
    if let Some(ops) = ops() {
        (ops.stop)();
    }
}
pub fn poll() {
    if let Some(ops) = ops() {
        (ops.poll)();
    }
}
pub fn read(out: &mut [u8]) -> usize {
    ops().map_or(0, |ops| (ops.read)(out))
}
pub fn write(data: &[u8]) -> usize {
    ops().map_or(0, |ops| (ops.write)(data))
}
pub fn playback_space() -> usize {
    ops().map_or(0, |ops| (ops.playback_space)())
}
pub fn format(capture: bool) -> Option<(u8, u8)> {
    (ops()?.format)(capture)
}
pub fn select(capture: bool, mode: u32) -> Result<(), &'static str> {
    ops().map_or(Err("no audio device"), |ops| (ops.select)(capture, mode))
}
pub fn print_modes() {
    if let Some(ops) = ops() {
        (ops.print_modes)();
    }
}
pub fn status() {
    if let Some(ops) = ops() {
        (ops.status)();
    }
}
pub fn fill_info(info: &mut AudioInfo) -> bool {
    ops().is_some_and(|ops| (ops.fill_info)(info))
}
pub fn fill_stats(stats: &mut AudioStats) {
    if let Some(ops) = ops() {
        (ops.fill_stats)(stats);
    }
}
pub fn buffer_info() -> (u64, u64) {
    ops().map_or((0, 0), |ops| (ops.buffer_info)())
}
