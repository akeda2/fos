//! The tape calls that are the same on every embedded kernel (x86_64 `tape.rs`, aarch64 `tape_aarch64.rs`): both tables point at
//! these. A call belongs here when its body does not name an architecture; whatever touches a NIC, the serial ports, the wall
//! clock, the watchdog or the idle loop stays in the per-architecture file. `tools/check_abi_support.py` lists which calls each
//! kernel serves.

use crate::{memory, timer};

pub extern "C" fn api_set_input_mode(_mode: u32) {}
pub extern "C" fn api_ticks() -> u64 {
    timer::ticks()
}
pub extern "C" fn api_micros() -> u64 {
    crate::tasks::maybe_yield();
    timer::micros()
}
/// The embedded kernels are identity-mapped: the address a tape sees is the physical one.
pub extern "C" fn api_phys_addr(address: usize) -> usize {
    address
}
pub extern "C" fn api_alloc(bytes: usize) -> usize {
    if bytes == 0 {
        return 0;
    }
    memory::allocate_contiguous((bytes as u64).div_ceil(memory::FRAME), None).unwrap_or(0) as usize
}
pub extern "C" fn api_free(address: usize, bytes: usize) {
    if address != 0 && bytes != 0 {
        memory::free(address as u64, (bytes as u64).div_ceil(memory::FRAME));
    }
}
pub extern "C" fn api_free_pages() -> usize {
    memory::stats().free as usize
}

/// FOSFS references (`NAME`, `diskN/NAME`, `@LABEL/NAME`) are resolved in `tape_fs`.
pub extern "C" fn api_fs_read(name: *const u8, name_length: usize, buffer: *mut u8, capacity: usize) -> isize {
    let (name, buffer) = unsafe { (core::slice::from_raw_parts(name, name_length), core::slice::from_raw_parts_mut(buffer, capacity)) };
    crate::tape_fs::read(name, buffer)
}
pub extern "C" fn api_fs_write(name: *const u8, name_length: usize, data: *const u8, length: usize) -> isize {
    let (name, data) = unsafe { (core::slice::from_raw_parts(name, name_length), core::slice::from_raw_parts(data, length)) };
    crate::tape_fs::write(name, data)
}
pub extern "C" fn api_fs_stream_open(name: *const u8, name_length: usize, max_bytes: u64) -> isize {
    crate::tape_fs::stream_open(unsafe { core::slice::from_raw_parts(name, name_length) }, max_bytes)
}
pub extern "C" fn api_fs_stream_write(offset: u64, data: *const u8, length: usize, file_length: u64) -> isize {
    crate::tape_fs::stream_write(offset, unsafe { core::slice::from_raw_parts(data, length) }, file_length)
}
pub extern "C" fn api_fs_stream_close(file_length: u64) -> isize {
    crate::tape_fs::stream_close(file_length)
}
pub extern "C" fn api_fs_remove(name: *const u8, name_length: usize) -> isize {
    crate::tape_fs::remove(unsafe { core::slice::from_raw_parts(name, name_length) })
}

pub extern "C" fn api_gfx_info() -> u64 {
    crate::framebuffer::gfx_info().map_or(0, |(width, height)| u64::from(width) << 32 | u64::from(height))
}
pub extern "C" fn api_gfx_open() -> u32 {
    u32::from(crate::framebuffer::gfx_open())
}
pub extern "C" fn api_gfx_close() {
    crate::framebuffer::gfx_close();
}
pub extern "C" fn api_gfx_fill(x: u32, y: u32, width: u32, height: u32, rgb: u32) -> u32 {
    u32::from(crate::framebuffer::gfx_fill(x, y, width, height, rgb))
}
pub extern "C" fn api_gfx_blit(x: u32, y: u32, width: u32, height: u32, pixels: *const u32, stride: u32) -> u32 {
    u32::from(unsafe { crate::framebuffer::gfx_blit(x, y, width, height, pixels, stride) })
}
pub extern "C" fn api_console_size() -> u32 {
    crate::framebuffer::size().map_or(0, |(columns, rows)| ((columns as u32) << 16) | rows as u32 & 0xffff)
}

pub extern "C" fn api_audio_start(rate: u32, capture: u32, playback: u32) -> i32 {
    crate::audio::start_for_tape(rate, capture != 0, playback != 0)
}
pub extern "C" fn api_audio_use_device(number: u32) -> i32 {
    crate::audio::use_device_for_tape(number)
}
pub extern "C" fn api_audio_use_output(number: u32) -> i32 {
    crate::audio::use_output_for_tape(number)
}
pub extern "C" fn api_audio_select(direction: u32, mode: u32) -> i32 {
    crate::audio::select_for_tape(direction == 0, mode)
}
pub extern "C" fn api_audio_stop() {
    crate::audio::stop_for_tape();
}
