//! The Pi's I2S audio HAT, as `audio` sees it: slot `ext_audio::I2S`, registered by the kernel (`set`), or nothing (x86, or
//! an aarch64 kernel with the driver absent), in which case no I2S device is ever offered. The driver itself (`i2s_pi.rs`)
//! stays in the kernel; docs/big-kernel.md, stage 1b.

pub use crate::ext_audio::Ops;

pub fn set(ops: &'static Ops) {
    crate::ext_audio::set(crate::ext_audio::I2S, ops);
}
