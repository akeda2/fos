//! Stand-in for `i2s_pi.rs` on x86: no I2S audio device, so the audio facade never offers one.

use fosfor_abi::tape::{AudioInfo, AudioStats};

pub fn present() -> bool {
    false
}
pub fn label() -> &'static str {
    ""
}
pub fn start(_rate: u32, _capture: bool, _playback: bool) -> Result<(), &'static str> {
    Err("no audio device")
}
pub fn stop() {}
pub fn poll() {}
pub fn read(_out: &mut [u8]) -> usize {
    0
}
pub fn write(_data: &[u8]) -> usize {
    0
}
pub fn playback_space() -> usize {
    0
}
pub fn format(_capture: bool) -> Option<(u8, u8)> {
    None
}
pub fn select(_capture: bool, _mode: u32) -> Result<(), &'static str> {
    Err("no audio device")
}
pub fn print_modes() {}
pub fn status() {}
pub fn fill_info(_info: &mut AudioInfo) -> bool {
    false
}
pub fn fill_stats(_stats: &mut AudioStats) {}
pub fn buffer_info() -> (u64, u64) {
    (0, 0)
}
