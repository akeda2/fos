//! Stand-in for `hda.rs` on aarch64 (no Intel HD Audio there): zero controllers, so the audio facade only ever sees USB audio.

pub const MAX_CONTROLLERS: usize = 4;

pub fn count() -> usize {
    0
}
pub fn present(_index: usize) -> bool {
    false
}
pub fn rank(_index: usize) -> u32 {
    0
}
pub fn label(_index: usize) -> (u8, u8, u8, u16, u16) {
    (0, 0, 0, 0, 0)
}
pub fn start(_index: usize, _rate: u32, _capture: bool, _playback: bool) -> Result<(), &'static str> {
    Err("no HD Audio on this machine")
}
pub fn stop(_index: usize) {}
pub fn read(_index: usize, _out: &mut [u8]) -> usize {
    0
}
pub fn write(_index: usize, _data: &[u8]) -> usize {
    0
}
pub fn playback_space(_index: usize) -> usize {
    0
}
pub fn format(_index: usize, _capture: bool) -> Option<(u8, u8)> {
    None
}
pub fn poll() {}
pub fn select(_index: usize, _capture: bool, _mode: u32) -> Result<(), &'static str> {
    Err("no HD Audio on this machine")
}
pub fn print_modes(_index: usize) {}
pub fn status(_index: usize) {}
pub fn fill_info(_index: usize, _info: &mut fosfor_abi::tape::AudioInfo) -> bool {
    false
}
pub fn fill_stats(_index: usize, _stats: &mut fosfor_abi::tape::AudioStats) {}
pub fn buffer_info(_index: usize) -> (u64, u64) {
    (0, 0)
}
