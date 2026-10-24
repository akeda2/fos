//! Loading and running tapes: small ELF64 programs the kernel runs in its own address
//! space (trusted like kernel code; this is not isolation). See `fosfor_abi::tape` for
//! the ABI and `docs/design.md` section 9.
//!
//! A tape is a position-independent executable, so it can land anywhere: the kernel
//! keeps one 4 MiB, 2 MiB-aligned region (made executable, the only such RAM) and one
//! 4 MiB staging buffer holding the raw file. `tape load` puts a file (from FOSFS or
//! the network) into staging and then relocates it into the region; `tape run` calls
//! its entry on a stack of its own with the API table. `tape save` writes the staged
//! file to FOSFS, which is how a tape sent over the network gets installed.

use core::sync::atomic::{AtomicU64, Ordering};

use fosfor_abi::tape::*;
use fosfor_abi::{BootInfo, elf};

use crate::{memory, paging, term, timer};

const HUGE: u64 = 2 * 1024 * 1024;
const REGION_BYTES: u64 = 4 * 1024 * 1024;
pub const STAGE_BYTES: usize = 4 * 1024 * 1024;
const STACK_BYTES: u64 = 1024 * 1024;

/// Slot 0 is the foreground tape's region; slots 1.. belong to services (`tasks.rs`).
static mut REGIONS: [u64; 1 + crate::tasks::MAX_SERVICES] = [0; 1 + crate::tasks::MAX_SERVICES];
static mut STAGING: u64 = 0;
static mut STAGED_LENGTH: usize = 0;
static mut ENTRY: u64 = 0;
static mut LOADED_NAME: [u8; 16] = [0; 16];
static mut LOADED_NAME_LENGTH: usize = 0;
static mut INFO: *const BootInfo = core::ptr::null();
static LOADED_SPAN: AtomicU64 = AtomicU64::new(0);

/// The loader's BootInfo, for diagnostics (`bootinfo`, `memmap`).
pub fn boot_info() -> Option<&'static BootInfo> {
    unsafe { INFO.as_ref() }
}

pub fn set_boot_info(info: &BootInfo) {
    unsafe { INFO = info };
}

/// The staging buffer (allocated on first use).
pub fn staging() -> Option<&'static mut [u8]> {
    unsafe {
        if STAGING == 0 {
            STAGING = memory::allocate_contiguous(STAGE_BYTES as u64 / memory::FRAME, None)?;
        }
        Some(core::slice::from_raw_parts_mut(STAGING as *mut u8, STAGE_BYTES))
    }
}

pub fn set_staged_length(length: usize) {
    unsafe { STAGED_LENGTH = length };
}

/// The tape region: 4 MiB at a 2 MiB boundary, executable. Allocated on first use by
/// over-allocating and handing the unaligned head and tail back.
fn region(slot: usize) -> Option<u64> {
    unsafe {
        let region = &mut (*core::ptr::addr_of_mut!(REGIONS))[slot];
        if *region == 0 {
            let total = REGION_BYTES + HUGE;
            let raw = memory::allocate_contiguous(total / memory::FRAME, None)?;
            let start = (raw + HUGE - 1) & !(HUGE - 1);
            memory::free(raw, (start - raw) / memory::FRAME);
            let end = start + REGION_BYTES;
            memory::free(end, (raw + total - end) / memory::FRAME);
            paging::set_executable(start, REGION_BYTES, true);
            *region = start;
        }
        Some(*region)
    }
}

#[derive(Debug)]
#[allow(dead_code)] // payload is shown through Debug
pub enum LoadError {
    NoMemory,
    TooBig,
    Elf(elf::Error),
}

/// Relocates the staged file into the region of `slot`; returns its entry point and size.
fn load_into(slot: usize) -> Result<(u64, u64), LoadError> {
    let image_length = unsafe { STAGED_LENGTH };
    let staging = staging().ok_or(LoadError::NoMemory)?;
    let image = &staging[..image_length];
    let span = elf::span(image).map_err(LoadError::Elf)? as u64;
    if span > REGION_BYTES {
        return Err(LoadError::TooBig);
    }
    let base = region(slot).ok_or(LoadError::NoMemory)?;
    // Clear the region first so a shorter tape never runs on a longer one's leftovers.
    unsafe { core::ptr::write_bytes(base as *mut u8, 0, REGION_BYTES as usize) };
    let entry = unsafe { elf::load(image, base) }.map_err(LoadError::Elf)?;
    Ok((entry, span))
}

/// A service's tape: staged file into its slot's region (see `tasks::start`).
pub fn load_staged_service(slot: usize, _name: &str) -> Result<u64, LoadError> {
    load_into(slot).map(|(entry, _)| entry)
}

/// Relocates the staged file into the foreground tape region and records its entry point.
pub fn load_staged(name: &str) -> Result<(), LoadError> {
    let (entry, span) = load_into(0)?;
    unsafe {
        ENTRY = entry;
        let take = name.len().min(16);
        LOADED_NAME[..take].copy_from_slice(&name.as_bytes()[..take]);
        LOADED_NAME_LENGTH = take;
    }
    LOADED_SPAN.store(span, Ordering::Relaxed);
    Ok(())
}

pub fn loaded() -> Option<(&'static str, u64, u64)> {
    unsafe {
        (ENTRY != 0).then(|| (core::str::from_utf8(core::slice::from_raw_parts(core::ptr::addr_of!(LOADED_NAME) as *const u8, LOADED_NAME_LENGTH)).unwrap_or("?"), ENTRY, LOADED_SPAN.load(Ordering::Relaxed)))
    }
}

pub fn staged_length() -> usize {
    unsafe { STAGED_LENGTH }
}

// ---- the callbacks a tape sees ----

/// A UTF-8 sequence a tape is printing one `putc` byte at a time, waiting for its last byte.
static mut PUTC_PENDING: [u8; 4] = [0; 4];
static mut PUTC_PENDING_LENGTH: usize = 0;

/// `i2c_speed` / `i2c_transfer` (ABI 23): this kernel has no I2C bus (the Raspberry Pi 4 has one).
extern "C" fn api_i2c_speed(_hz: u32) -> i32 {
    -4
}

extern "C" fn api_i2c_transfer(_address: u32, _out: *const u8, _out_len: u32, _input: *mut u8, _in_len: u32) -> i32 {
    -4
}

extern "C" fn api_putc(byte: u32) {
    let byte = byte as u8;
    let pending = unsafe { &mut *core::ptr::addr_of_mut!(PUTC_PENDING) };
    let length = unsafe { &mut *core::ptr::addr_of_mut!(PUTC_PENDING_LENGTH) };
    if byte < 0x80 {
        if *length > 0 {
            *length = 0;
            crate::print(format_args!("?"));
        }
        let mut utf8 = [0u8; 4];
        crate::print(format_args!("{}", (byte as char).encode_utf8(&mut utf8)));
        return;
    }
    // Bytes above 0x7f are collected until they make one valid UTF-8 character (the console and a remote terminal both
    // want whole characters); a broken sequence is shown as '?'.
    if *length == 0 && byte & 0xc0 == 0x80 || *length == pending.len() {
        *length = 0;
        return crate::print(format_args!("?"));
    }
    pending[*length] = byte;
    *length += 1;
    match core::str::from_utf8(&pending[..*length]) {
        Ok(text) => {
            *length = 0;
            crate::print(format_args!("{text}"));
        }
        Err(error) if error.error_len().is_some() => {
            *length = 0;
            crate::print(format_args!("?"));
        }
        Err(_) => {} // incomplete: wait for the next byte
    }
}
/// A whole string in one console call (one repaint, one relay frame), where `putc` costs a repaint per byte.
extern "C" fn api_puts(text: *const u8, length: usize) {
    let bytes = unsafe { core::slice::from_raw_parts(text, length) };
    match core::str::from_utf8(bytes) {
        Ok(text) => crate::print(format_args!("{text}")),
        Err(_) => {
            // Not valid UTF-8 as a whole: byte by byte, so `putc`'s per-sequence handling decides what is printable.
            for &byte in bytes {
                api_putc(u32::from(byte));
            }
        }
    }
}
extern "C" fn api_should_abort() -> u32 {
    if crate::tasks::in_service() {
        return u32::from(crate::tasks::service_should_abort());
    }
    crate::tasks::maybe_yield();
    u32::from(term::tape_should_abort())
}
extern "C" fn api_read_key() -> u32 {
    if crate::tasks::in_service() {
        return crate::tasks::service_read_key();
    }
    crate::tasks::maybe_yield();
    term::tape_key()
}
extern "C" fn api_set_input_mode(_mode: u32) {}
extern "C" fn api_wait_for_event() {
    use crate::cpu::{spun, Spin};
    if crate::tasks::in_service() {
        return crate::tasks::service_wait();
    }
    term::flush_output();
    if crate::tasks::pass() {
        return; // a service has work: do not sleep
    }
    let begin = timer::micros();
    // Idle time is when queued serial output goes out; a tick alone would drain only 16 bytes per 10 ms.
    if crate::serial::pending() {
        let until = begin + 100;
        while crate::serial::pending() && timer::micros() < until {
            crate::serial::pump();
        }
        spun(Spin::SerialOut, begin);
        return;
    }
    if crate::uart::any_open() {
        // A polled UART: its 16-byte FIFO overflows in 1.4 ms at 115200 baud, a tick later would be too late.
        let until = begin + 100;
        while timer::micros() < until {
            crate::uart::poll();
            core::hint::spin_loop();
        }
        spun(Spin::Uart, begin);
        return;
    }
    if term::nic_count() == 0 || term::all_interrupt_driven() {
        // Nothing to wait for but interrupts: a frame (MSI), a key, the 10 ms tick.
        crate::cpu::sleep();
    } else {
        // A polled NIC would only be noticed at the next 10 ms tick, adding that to every
        // frame. Yield briefly instead (about 100 us).
        let until = begin + 100;
        while timer::micros() < until {
            core::hint::spin_loop();
        }
        spun(Spin::PolledNic, begin);
    }
}
extern "C" fn api_ticks() -> u64 {
    timer::ticks()
}
extern "C" fn api_micros() -> u64 {
    crate::tasks::maybe_yield();
    timer::micros()
}
extern "C" fn api_alloc(bytes: usize) -> usize {
    if bytes == 0 {
        return 0;
    }
    memory::allocate_contiguous((bytes as u64).div_ceil(memory::FRAME), None).unwrap_or(0) as usize
}
extern "C" fn api_free(address: usize, bytes: usize) {
    if address != 0 && bytes != 0 {
        memory::free(address as u64, (bytes as u64).div_ceil(memory::FRAME));
    }
}
/// FOSFS references (`NAME`, `diskN/NAME`, `@LABEL/NAME`) are resolved in `tape_fs`, shared with the aarch64 port.
/// The tape file calls are shared with the aarch64 port: see `tape_fs`.
extern "C" fn api_fs_read(name: *const u8, name_length: usize, buffer: *mut u8, capacity: usize) -> isize {
    let (name, buffer) = unsafe { (core::slice::from_raw_parts(name, name_length), core::slice::from_raw_parts_mut(buffer, capacity)) };
    crate::tape_fs::read(name, buffer)
}
extern "C" fn api_fs_write(name: *const u8, name_length: usize, data: *const u8, length: usize) -> isize {
    let (name, data) = unsafe { (core::slice::from_raw_parts(name, name_length), core::slice::from_raw_parts(data, length)) };
    crate::tape_fs::write(name, data)
}
extern "C" fn api_fs_stream_open(name: *const u8, name_length: usize, max_bytes: u64) -> isize {
    crate::tape_fs::stream_open(unsafe { core::slice::from_raw_parts(name, name_length) }, max_bytes)
}
extern "C" fn api_fs_stream_write(offset: u64, data: *const u8, length: usize, file_length: u64) -> isize {
    crate::tape_fs::stream_write(offset, unsafe { core::slice::from_raw_parts(data, length) }, file_length)
}
extern "C" fn api_fs_stream_close(file_length: u64) -> isize {
    crate::tape_fs::stream_close(file_length)
}
extern "C" fn api_fs_list(volume: *const u8, volume_length: usize, index: usize, out: *mut fosfor_abi::tape::FileInfo) -> isize {
    crate::tape_fs::list(unsafe { core::slice::from_raw_parts(volume, volume_length) }, index, unsafe { &mut *out })
}
extern "C" fn api_fs_remove(name: *const u8, name_length: usize) -> isize {
    crate::tape_fs::remove(unsafe { core::slice::from_raw_parts(name, name_length) })
}
extern "C" fn api_free_pages() -> usize {
    memory::stats().free as usize
}
extern "C" fn api_hostname(index: usize, out: *mut u8, capacity: usize) -> usize {
    let mut name = [0u8; 20];
    let length = term::hostname(index, &mut name);
    if length > capacity {
        return 0;
    }
    unsafe { core::ptr::copy_nonoverlapping(name.as_ptr(), out, length) };
    length
}
extern "C" fn api_peer_info(number: usize, out: *mut fosfor_abi::tape::PeerInfo) -> usize {
    let mut info = fosfor_abi::tape::PeerInfo::default();
    let count = term::peer_info(number, &mut info);
    if number >= 1 && number <= count && !out.is_null() {
        unsafe { out.write(info) };
    }
    count
}
extern "C" fn api_disk_info(number: usize, out: *mut fosfor_abi::tape::DiskInfo) -> u32 {
    crate::tape_fs::disk_info(number, unsafe { &mut *out })
}
extern "C" fn api_disk_format_exfat(number: usize, label: *const u8, length: usize) -> i32 {
    use crate::fatvol::FormatError;
    let label = unsafe { core::slice::from_raw_parts(label, length) };
    match number.checked_sub(1).map_or(Err(FormatError::NoSuchDisk), |disk| crate::fatvol::format_exfat(disk, label)) {
        Ok(_) => 0,
        Err(FormatError::NoSuchDisk) => fosfor_abi::tape::FORMAT_NO_DISK,
        Err(FormatError::BootDisk) => fosfor_abi::tape::FORMAT_BOOT_DISK,
        Err(FormatError::BadLabel) => fosfor_abi::tape::FORMAT_BAD_LABEL,
        Err(FormatError::Failed) => fosfor_abi::tape::FORMAT_FAILED,
    }
}
extern "C" fn api_audio_buffer(out: *mut fosfor_abi::tape::AudioBuffer) {
    let (ring_bytes, available_bytes) = crate::audio::buffer_info();
    unsafe { *out = fosfor_abi::tape::AudioBuffer { ring_bytes, available_bytes } };
}
extern "C" fn api_wall_clock(out: *mut fosfor_abi::tape::WallClock) -> u32 {
    match crate::clock::now() {
        Some(time) => {
            unsafe { *out = fosfor_abi::tape::WallClock { year: time.year, month: time.month, day: time.day, hour: time.hour, minute: time.minute, second: time.second, reserved: 0 } };
            1
        }
        None => 0,
    }
}
extern "C" fn api_wall_clock_set(time: *const fosfor_abi::tape::WallClock) -> u32 {
    let time = unsafe { *time };
    let time = fat::Time { year: time.year, month: time.month, day: time.day, hour: time.hour, minute: time.minute, second: time.second };
    u32::from(crate::clock::set(&time).is_ok())
}
extern "C" fn api_nic_ts_enable(index: usize) -> u32 {
    u32::from(term::nic_ts_enable(index))
}
extern "C" fn api_nic_ts_clock(index: usize) -> u64 {
    term::nic_ts_clock(index)
}
extern "C" fn api_nic_ts_adjust(index: usize, ppb: i32) -> u32 {
    u32::from(term::nic_ts_adjust(index, ppb))
}
extern "C" fn api_nic_ts_step(index: usize, delta: i64) -> u32 {
    u32::from(term::nic_ts_step(index, delta))
}
extern "C" fn api_nic_multicast_join(index: usize, mac: *const u8, on: u32) -> u32 {
    if mac.is_null() {
        return 0;
    }
    let mut address = [0u8; 6];
    unsafe { core::ptr::copy_nonoverlapping(mac, address.as_mut_ptr(), 6) };
    u32::from(term::nic_multicast_join(index, address, on != 0))
}
extern "C" fn api_nic_vlan_filter(index: usize, vid: u32, on: u32) -> u32 {
    u16::try_from(vid).is_ok_and(|vid| term::nic_vlan_filter(index, vid, on != 0)) as u32
}
extern "C" fn api_nic_counters(index: usize, out: *mut u64, count: usize) -> usize {
    if out.is_null() {
        return 0;
    }
    term::nic_counters(index, unsafe { core::slice::from_raw_parts_mut(out, count) })
}
extern "C" fn api_gfx_info() -> u64 {
    crate::framebuffer::gfx_info().map_or(0, |(width, height)| u64::from(width) << 32 | u64::from(height))
}
extern "C" fn api_gfx_open() -> u32 {
    u32::from(crate::framebuffer::gfx_open())
}
extern "C" fn api_gfx_close() {
    crate::framebuffer::gfx_close();
}
extern "C" fn api_gfx_fill(x: u32, y: u32, width: u32, height: u32, rgb: u32) -> u32 {
    u32::from(crate::framebuffer::gfx_fill(x, y, width, height, rgb))
}
extern "C" fn api_gfx_blit(x: u32, y: u32, width: u32, height: u32, pixels: *const u32, stride: u32) -> u32 {
    u32::from(unsafe { crate::framebuffer::gfx_blit(x, y, width, height, pixels, stride) })
}
static mut FP_LEVEL: u32 = 0;
/// Turns SSE/AVX2 off again (nothing running needs them).
pub fn clear_fp_level() {
    crate::cpu::fp_disable();
    unsafe { FP_LEVEL = 0 };
}
/// A service may be a hard-float tape: the units stay on while any service runs.
pub fn enable_fp_level() {
    unsafe { FP_LEVEL = crate::cpu::fp_enable() };
}
extern "C" fn api_fp_level() -> u32 {
    unsafe { FP_LEVEL }
}
extern "C" fn api_nic_tx_offload(index: usize, on: u32) -> u32 {
    u32::from(term::nic_tx_offload(index, on != 0))
}
extern "C" fn api_nic_ts_all(index: usize, on: u32) -> u32 {
    u32::from(term::nic_ts_all(index, on != 0))
}
extern "C" fn api_nic_ts_rx(index: usize) -> u64 {
    term::last_rx_stamp(index)
}
extern "C" fn api_nic_send_stamped(index: usize, frame: *const u8, length: usize, stamp: *mut u64) -> u32 {
    if length > 1518 {
        return 0;
    }
    let (sent, value) = term::nic_send_stamped(index, unsafe { core::slice::from_raw_parts(frame, length) });
    if !stamp.is_null() {
        unsafe { stamp.write(value) };
    }
    u32::from(sent)
}
extern "C" fn api_console_size() -> u32 {
    crate::framebuffer::size().map_or(0, |(columns, rows)| ((columns as u32) << 16) | rows as u32 & 0xffff)
}
extern "C" fn api_serial_count() -> usize {
    crate::uart::count()
}
extern "C" fn api_serial_info(number: usize, out: *mut fosfor_abi::tape::SerialInfo) -> u32 {
    let mut info = fosfor_abi::tape::SerialInfo::default();
    let found = crate::uart::info(number, &mut info);
    if found && !out.is_null() {
        unsafe { out.write(info) };
    }
    u32::from(found)
}
extern "C" fn api_serial_open(number: usize, baud: u32, format: u32) -> i32 {
    crate::uart::open(number, baud, format)
}
extern "C" fn api_serial_close(number: usize) {
    crate::uart::close(number);
}
extern "C" fn api_serial_read(number: usize, buffer: *mut u8, capacity: usize) -> usize {
    crate::uart::read(number, unsafe { core::slice::from_raw_parts_mut(buffer, capacity) })
}
extern "C" fn api_serial_write(number: usize, data: *const u8, length: usize) -> usize {
    crate::uart::write(number, unsafe { core::slice::from_raw_parts(data, length) })
}
extern "C" fn api_serial_lines(number: usize) -> u32 {
    crate::uart::lines(number)
}
extern "C" fn api_serial_set_lines(number: usize, lines: u32) {
    crate::uart::set_lines(number, lines);
}
extern "C" fn api_serial_break(number: usize, on: u32) {
    crate::uart::set_break(number, on != 0);
}
extern "C" fn api_audio_set_capture_ring(bytes: u64) {
    crate::usb_audio::set_capture_ring(bytes); // the capture ring belongs to whichever device starts next
}
extern "C" fn api_nic_count() -> usize {
    term::nic_count()
}
extern "C" fn api_nic_set_promiscuous(index: usize, on: u32) -> u32 {
    u32::from(term::nic_set_promiscuous(index, on != 0))
}
extern "C" fn api_nic_set_multicast(index: usize, on: u32) -> u32 {
    u32::from(term::nic_set_multicast(index, on != 0))
}
extern "C" fn api_nic_mac(index: usize, out: *mut u8) -> u32 {
    let mut mac = [0u8; 6];
    if !term::nic_mac(index, &mut mac) {
        return 0;
    }
    unsafe { core::ptr::copy_nonoverlapping(mac.as_ptr(), out, 6) };
    1
}
extern "C" fn api_nic_send(index: usize, frame: *const u8, length: usize) -> u32 {
    if length > 1518 {
        return 0;
    }
    u32::from(term::nic_send(index, unsafe { core::slice::from_raw_parts(frame, length) }))
}
extern "C" fn api_nic_receive(index: usize, buffer: *mut u8, capacity: usize) -> usize {
    let buffer = unsafe { core::slice::from_raw_parts_mut(buffer, capacity) };
    if crate::tasks::in_service() {
        return term::service_receive(index, buffer);
    }
    term::tape_receive(index, buffer)
}
extern "C" fn api_audio_info(out: *mut fosfor_abi::tape::AudioInfo) -> u32 {
    u32::from(crate::audio::fill_info(unsafe { &mut *out }))
}
extern "C" fn api_audio_start(rate: u32, capture: u32, playback: u32) -> i32 {
    crate::audio::start_for_tape(rate, capture != 0, playback != 0)
}
extern "C" fn api_audio_devices(number: u32, out: *mut fosfor_abi::tape::AudioDevice) -> u32 {
    if !out.is_null() {
        if let Some(device) = crate::audio::device_info(number as usize) {
            unsafe { out.write(device) };
        }
    }
    crate::audio::device_count() as u32
}

extern "C" fn api_nic_active() -> u32 {
    crate::tasks::current_nic().unwrap_or_else(term::active_nic) as u32
}

extern "C" fn api_audio_use_device(number: u32) -> i32 {
    crate::audio::use_device_for_tape(number)
}

extern "C" fn api_audio_use_output(number: u32) -> i32 {
    crate::audio::use_output_for_tape(number)
}

extern "C" fn api_audio_select(direction: u32, mode: u32) -> i32 {
    crate::audio::select_for_tape(direction == 0, mode)
}
extern "C" fn api_audio_stop() {
    crate::audio::stop_for_tape();
}
extern "C" fn api_audio_read(buffer: *mut u8, length: usize) -> usize {
    term::tape_poll(); // drains the xHCI event ring, which is what moves USB audio
    crate::audio::read(unsafe { core::slice::from_raw_parts_mut(buffer, length) })
}
extern "C" fn api_audio_write(data: *const u8, length: usize) -> usize {
    term::tape_poll();
    crate::audio::write(unsafe { core::slice::from_raw_parts(data, length) })
}
extern "C" fn api_audio_stats(out: *mut fosfor_abi::tape::AudioStats) {
    crate::audio::fill_stats(unsafe { &mut *out });
}

extern "C" fn api_nic_max_frame_len() -> usize {
    1518
}

pub fn build_api(info: &BootInfo, arguments: &str) -> TapeApi {
    TapeApi {
        version: TAPE_API_VERSION,
        size: core::mem::size_of::<TapeApi>() as u32,
        putc: api_putc,
        should_abort: api_should_abort,
        read_key: api_read_key,
        set_input_mode: api_set_input_mode,
        wait_for_event: api_wait_for_event,
        ticks: api_ticks,
        micros: api_micros,
        alloc: api_alloc,
        free: api_free,
        args: arguments.as_ptr(),
        args_len: arguments.len(),
        fs_read: api_fs_read,
        fs_write: api_fs_write,
        nic_count: api_nic_count,
        nic_mac: api_nic_mac,
        nic_send: api_nic_send,
        nic_receive: api_nic_receive,
        nic_max_frame_len: api_nic_max_frame_len,
        memory_map: info.memory_map as *const u8,
        memory_map_count: info.memory_map_count as usize,
        memory_map_descriptor_size: info.memory_map_descriptor_size as usize,
        free_pages: api_free_pages,
        fs_remove: api_fs_remove,
        nic_set_promiscuous: api_nic_set_promiscuous,
        audio_info: api_audio_info,
        audio_start: api_audio_start,
        audio_stop: api_audio_stop,
        audio_read: api_audio_read,
        audio_write: api_audio_write,
        audio_stats: api_audio_stats,
        nic_set_multicast: api_nic_set_multicast,
        fs_stream_open: api_fs_stream_open,
        fs_stream_write: api_fs_stream_write,
        fs_stream_close: api_fs_stream_close,
        puts: api_puts,
        audio_select: api_audio_select,
        hostname: api_hostname,
        disk_info: api_disk_info,
        disk_format_exfat: api_disk_format_exfat,
        audio_buffer: api_audio_buffer,
        audio_set_capture_ring: api_audio_set_capture_ring,
        wall_clock: api_wall_clock,
        audio_devices: api_audio_devices,
        audio_use_device: api_audio_use_device,
        nic_active: api_nic_active,
        peer_info: api_peer_info,
        wall_clock_set: api_wall_clock_set,
        serial_count: api_serial_count,
        serial_info: api_serial_info,
        serial_open: api_serial_open,
        serial_close: api_serial_close,
        serial_read: api_serial_read,
        serial_write: api_serial_write,
        serial_lines: api_serial_lines,
        serial_set_lines: api_serial_set_lines,
        console_size: api_console_size,
        serial_break: api_serial_break,
        nic_ts_enable: api_nic_ts_enable,
        nic_ts_clock: api_nic_ts_clock,
        nic_ts_rx: api_nic_ts_rx,
        nic_send_stamped: api_nic_send_stamped,
        nic_ts_adjust: api_nic_ts_adjust,
        nic_ts_step: api_nic_ts_step,
        nic_ts_all: api_nic_ts_all,
        nic_multicast_join: api_nic_multicast_join,
        nic_vlan_filter: api_nic_vlan_filter,
        nic_tx_offload: api_nic_tx_offload,
        nic_counters: api_nic_counters,
        fp_level: api_fp_level,
        gfx_info: api_gfx_info,
        gfx_open: api_gfx_open,
        gfx_close: api_gfx_close,
        gfx_fill: api_gfx_fill,
        gfx_blit: api_gfx_blit,
        audio_use_output: api_audio_use_output,
        fs_list: api_fs_list,
        gfx_screens: crate::framebuffer::api_gfx_screens,
        gfx_info_on: crate::framebuffer::api_gfx_info_on,
        gfx_open_on: crate::framebuffer::api_gfx_open_on,
        gfx_close_on: crate::framebuffer::api_gfx_close_on,
        gfx_fill_on: crate::framebuffer::api_gfx_fill_on,
        gfx_blit_on: crate::framebuffer::api_gfx_blit_on,
        audio_float: crate::audio::api_audio_float,
        gpio_pins: crate::gpio::api_gpio_pins,
        gpio_read: crate::gpio::api_gpio_read,
        gpio_write: crate::gpio::api_gpio_write,
        gpio_mode: crate::gpio::api_gpio_mode,
        gpio_pull: crate::gpio::api_gpio_pull,
        gpio_function: crate::gpio::api_gpio_function,
        i2c_speed: api_i2c_speed,
        i2c_transfer: api_i2c_transfer,
        touch_info: crate::usb_pointer::api_touch_info,
        touch_read: crate::usb_pointer::api_touch_read,
        midi_read: crate::usb_midi::api_midi_read,
        midi_send: crate::usb_midi::api_midi_send,
        midi_devices: crate::usb_midi::api_midi_devices,
    }
}

/// Runs the loaded tape with `arguments`. The tape gets a 1 MiB stack of its own, so
/// its recursion cannot eat the kernel's.
pub fn run(arguments: &str) -> Result<(), &'static str> {
    let entry = unsafe { ENTRY };
    if entry == 0 {
        return Err("no tape is loaded (tape load NAME)");
    }
    let info = unsafe { INFO.as_ref() }.ok_or("no boot info")?;
    let stack = memory::allocate_contiguous(STACK_BYTES / memory::FRAME, None).ok_or("no memory for the tape's stack")?;
    let api = build_api(info, arguments);
    term::clear_frame_queue();
    term::set_tape_mode(true);
    crate::debug::set_tape_running(true);
    unsafe { FP_LEVEL = crate::cpu::fp_enable() };
    unsafe {
        // Switch to the tape's stack for the call, then back. r12/r13 are callee-saved,
        // so the tape preserves them: r13 keeps our stack pointer across the call.
        core::arch::asm!(
            "mov r13, rsp",
            "mov rsp, r12",
            "call {entry}",
            "mov rsp, r13",
            entry = in(reg) entry,
            in("rdi") &api as *const TapeApi,
            in("r12") stack + STACK_BYTES,
            out("r13") _,
            clobber_abi("C"),
        );
    }
    crate::gpio::tape_ended();
    crate::framebuffer::gfx_close(); // a tape that ended (or was aborted) without closing gives the screen back
    if crate::tasks::count() == 0 {
        clear_fp_level();
    }
    crate::debug::set_tape_running(false);
    crate::uart::close_all();
    crate::audio::stop_for_tape(); // a tape that returns (or was aborted) must not leave an audio stream running
    term::set_tape_mode(false);
    term::nic_multicast_clear_all();
    term::nic_vlan_clear_all();
    term::nic_tx_offload_off_all();
    term::nic_ts_all_off(); // per-frame stamps change the receive buffer format; a tape that ends leaves PTP-only stamping
    memory::free(stack, STACK_BYTES / memory::FRAME);
    crate::println!("Tape returned; back at the fos shell.");
    Ok(())
}

// ---- the `tape` shell command ----

const TAG_TAPE_START: u8 = 0x10;
const TAG_TAPE_DATA: u8 = 0x11;
const TAG_TAPE_STOP: u8 = 0x12;

/// `tape load net`: receives a tape over Ethernet (the v1 protocol: START with the total
/// length, numbered DATA chunks, STOP; `tools/send_tape.py` sends it) into the staging
/// buffer. The first sender is locked in; a gap abandons that transfer but the loop keeps
/// listening. Escape (local, or from the term net client) gives up.
fn receive_from_network() -> Result<usize, &'static str> {
    let staging = staging().ok_or("no memory for the staging buffer")?;
    crate::println!("Waiting for a tape over the network (Escape gives up)...");
    term::set_tape_mode(true);
    let mut frame = [0u8; 1600];
    let mut sender: Option<[u8; 6]> = None;
    let (mut expected, mut received, mut next_sequence) = (0usize, 0usize, 0u32);
    let mut transferring = false;
    let mut last_report = timer::ticks();
    let result = loop {
        if term::tape_should_abort() {
            break Err("gave up");
        }
        let length = (0..term::nic_count()).map(|nic| term::tape_receive(nic, &mut frame)).find(|&length| length != 0).unwrap_or(0);
        if length < 15 || frame[12..14] != [0x88, 0xb6] {
            if length == 0 {
                api_wait_for_event();
            }
            continue;
        }
        let source: [u8; 6] = frame[6..12].try_into().unwrap();
        if sender.is_some_and(|locked| locked != source) && transferring {
            continue;
        }
        let payload = &frame[15..length];
        match frame[14] {
            TAG_TAPE_START if payload.len() >= 4 => {
                expected = u32::from_le_bytes(payload[0..4].try_into().unwrap()) as usize;
                if expected == 0 || expected > STAGE_BYTES {
                    crate::println!("tape: a transfer of {expected} bytes does not fit the staging buffer; ignored.");
                    continue;
                }
                sender = Some(source);
                (received, next_sequence, transferring) = (0, 0, true);
                crate::println!("Receiving {expected} bytes from {}.", term::Mac(&source));
            }
            TAG_TAPE_DATA if transferring && payload.len() >= 4 => {
                let sequence = u32::from_le_bytes(payload[0..4].try_into().unwrap());
                if sequence != next_sequence {
                    crate::println!("tape: a chunk was lost (wanted {next_sequence}, got {sequence}); transfer abandoned, still listening.");
                    transferring = false;
                    continue;
                }
                // The chunk's size comes from the declared length, never from the (padded) frame.
                let take = (payload.len() - 4).min(1024).min(expected - received);
                staging[received..received + take].copy_from_slice(&payload[4..4 + take]);
                received += take;
                next_sequence += 1;
                if timer::ticks() - last_report >= 200 {
                    crate::println!("  {received} / {expected} bytes");
                    last_report = timer::ticks();
                }
            }
            TAG_TAPE_STOP if transferring => {
                if received == expected {
                    break Ok(received);
                }
                crate::println!("tape: the transfer ended after {received} of {expected} bytes; abandoned, still listening.");
                transferring = false;
            }
            _ => {}
        }
    };
    term::set_tape_mode(false);
    result
}

/// A file as a command line spells it, as the reference `resolve_file` takes: `NAME`, `diskN/NAME` and `@LABEL/NAME` stay
/// as they are, and the v1 spelling `diskN NAME` (or `@LABEL NAME`) becomes `diskN/NAME` in `buffer`.
fn file_reference<'a>(text: &'a str, buffer: &'a mut [u8; 64]) -> &'a str {
    let text = text.trim();
    if let Some((volume, name)) = text.split_once(char::is_whitespace) {
        let name = name.trim();
        let is_volume = volume.strip_prefix("disk").is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())) || volume.starts_with('@');
        if is_volume && !name.is_empty() && volume.len() + 1 + name.len() <= buffer.len() {
            buffer[..volume.len()].copy_from_slice(volume.as_bytes());
            buffer[volume.len()] = b'/';
            buffer[volume.len() + 1..volume.len() + 1 + name.len()].copy_from_slice(name.as_bytes());
            return core::str::from_utf8(&buffer[..volume.len() + 1 + name.len()]).unwrap_or(text);
        }
    }
    text
}

/// True for a word naming a local volume: `diskN` or `@LABEL`.
fn is_volume_word(word: &str) -> bool {
    word.strip_prefix("disk").is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())) || word.starts_with('@')
}

/// `HOST[:diskN] NAME` (a machine, not a local volume) split into its two words.
fn remote_reference(text: &str) -> Option<(&str, &str)> {
    let (host, name) = text.trim().split_once(char::is_whitespace)?;
    let name = name.trim();
    (!is_volume_word(host) && host != "disk" && !name.is_empty() && !name.contains(char::is_whitespace)).then_some((host, name))
}

fn load_from_file(name: &str) -> Result<usize, &'static str> {
    let mut spelled = [0u8; 64];
    let (volume, name) = crate::tape_fs::resolve_file(file_reference(name, &mut spelled).as_bytes(), true).map_err(|_| "no such volume")?;
    let staging = staging().ok_or("no memory for the staging buffer")?;
    match volume.read(name, staging) {
        Ok(Some(length)) => Ok(length),
        Ok(None) => Err("no such file"),
        Err(crate::fosfs::Error::TooBig) => Err("the file does not fit the 4 MiB staging buffer"),
        Err(_) => Err("disk error"),
    }
}

/// Everything after `tape ` on the command line.
pub fn command(arguments: &str) {
    let arguments = arguments.trim();
    let (word, rest) = arguments.split_once(char::is_whitespace).unwrap_or((arguments, ""));
    let rest = rest.trim();
    match word {
        "load" => {
            // v1 spelled it `tape load disk NAME`: accept that too.
            let rest = match rest.strip_prefix("disk") {
                Some(after) if after.starts_with(char::is_whitespace) => after.trim_start(),
                _ => rest,
            };
            let (name, outcome) = if rest == "net" {
                ("network", receive_from_network())
            } else if let Some((host, file)) = remote_reference(rest) {
                (file, term::tape_fetch(host, file))
            } else {
                (rest, load_from_file(rest))
            };
            match outcome {
                Ok(length) => {
                    set_staged_length(length);
                    match load_staged(name) {
                        Ok(()) => crate::println!("Tape loaded: {length} bytes from {name}; type 'tape run' to execute it."),
                        Err(error) => crate::println!("tape: not a usable tape ({error:?})."),
                    }
                }
                Err(reason) => crate::println!("tape load: {reason}."),
            }
        }
        "run" => {
            if let Err(reason) = run(rest) {
                crate::println!("tape run: {reason}.");
            }
        }
        "save" => {
            let length = staged_length();
            if length == 0 {
                return crate::println!("tape save: nothing is staged (tape load first).");
            }
            if let Some((host, file)) = remote_reference(rest) {
                return match term::tape_store(host, file, length) {
                    Ok(()) => crate::println!("Saved {length} bytes to {host} as {file}."),
                    Err(reason) => crate::println!("tape save: {reason}."),
                };
            }
            let mut spelled = [0u8; 64];
            let Ok((volume, file)) = crate::tape_fs::resolve_file(file_reference(rest, &mut spelled).as_bytes(), false) else { return crate::println!("tape save: no such volume.") };
            let Some(staging) = staging() else { return };
            if length == 0 {
                return crate::println!("tape save: nothing is staged (tape load first).");
            }
            match volume.write(file, &staging[..length]) {
                Ok(()) => crate::println!("Saved {length} bytes as {rest}."),
                Err(error) => crate::println!("tape save: {error:?}"),
            }
        }
        "send" => {
            // `tape send [diskN] NAME PEER`: a file from a disk, as a tape for a machine waiting in `tape load net`.
            let Some((file, peer)) = rest.rsplit_once(char::is_whitespace) else { return crate::println!("Usage: tape send [diskN] NAME PEER   (PEER: a name or number from `nic peers`, or a MAC)") };
            match load_from_file(file) {
                Ok(length) => {
                    if let Err(reason) = term::tape_send(peer.trim(), length) {
                        return crate::println!("tape send: {reason}.");
                    }
                    crate::println!("Sent {length} bytes.");
                }
                Err(reason) => crate::println!("tape send: {file}: {reason}."),
            }
        }
        "status" | "" => match loaded() {
            Some((name, entry, span)) => crate::println!("Tape {name}: entry {entry:#x}, {span} bytes in memory, {} bytes staged.", staged_length()),
            None => crate::println!("No tape loaded."),
        },
        _ => crate::println!("tape: load [diskN] NAME|HOST[:diskN] NAME|net, run [ARGS], save NAME|HOST[:diskN] NAME, send [diskN] NAME PEER, status."),
    }
}

/// The help text in the `.fos_help` section of the tape file just staged, if it has one.
fn help_in_staged(length: usize) -> Option<&'static str> {
    let staged: &'static [u8] = staging()?;
    let image = &staged[..length];
    core::str::from_utf8(elf::section(image, ".fos_help")?).ok()
}

/// `help NAME`: shows the help screen the tape file NAME carries, without running it.
pub fn show_help(name: &str) {
    match load_from_file(name) {
        Ok(length) => match help_in_staged(length) {
            Some(text) => crate::println!("{text}"),
            None => crate::println!("help: {name} has no help screen."),
        },
        Err(reason) => crate::println!("help: {name}: {reason}."),
    }
}

/// `run A | B`: the console is split into two screens, B starts as a service on screen 1 and A runs in front on screen 0.
/// False (nothing started) when B is not a tape, since `calc 3 | 4` has a `|` too. B ends with A.
fn run_shared(first: &str, second: &str) -> bool {
    fn split(part: &str) -> (&str, &str) {
        let part = part.trim();
        part.split_once(char::is_whitespace).map_or((part, ""), |(name, arguments)| (name, arguments.trim()))
    }
    let ((one, one_args), (two, two_args)) = (split(first), split(second));
    let (mut one_text, mut two_text) = ([0u8; 256], [0u8; 256]);
    let (Some(one_args), Some(two_args)) = (crate::tasks::with_screen(&mut one_text, one_args, 2, true), crate::tasks::with_screen(&mut two_text, two_args, 3, true)) else {
        return false;
    };
    let Ok(length) = load_from_file(two) else { return false };
    crate::framebuffer::split_set(0, Some(crate::tasks::split_mode(first, second)));
    crate::framebuffer::rotate_run(0, crate::tasks::rotate_option(first, second));
    set_staged_length(length);
    crate::tasks::start(two, two_args, None);
    match load_from_file(one) {
        Ok(length) => {
            set_staged_length(length);
            match load_staged(one) {
                Ok(()) => {
                    if let Err(reason) = run(one_args) {
                        crate::println!("run: {reason}.");
                    }
                }
                Err(error) => crate::println!("run: {one} is not a usable tape ({error:?})."),
            }
        }
        Err(reason) => crate::println!("run: {one}: {reason}."),
    }
    crate::tasks::kill(Some(two));
    crate::framebuffer::split_clear_all();
    crate::framebuffer::rotate_reset();
    true
}

/// `run NAME [ARGS]`: load a tape from FOSFS and run it.
pub fn run_named(arguments: &str) {
    // `run NAME ARGS &N`: a service on NIC N.
    let (arguments, service) = crate::tasks::split_suffix(arguments);
    if service == Some(Err(())) {
        return crate::tasks::usage();
    }
    let (name, rest) = arguments.split_once(char::is_whitespace).unwrap_or((arguments, ""));
    // `run A ARGS | B ARGS [split=h|v]`: two graphic tapes share the screen (B runs as a service without NIC or keys).
    if service.is_none() {
        if let Some((first, second)) = arguments.split_once(" | ") {
            if run_shared(first, second) {
                return;
            }
        }
    }
    match load_from_file(name) {
        Ok(length) => {
            // `NAME help`, `NAME --help`: a tape with a `.fos_help` section shows it instead of running.
            if matches!(rest.trim(), "help" | "-h" | "--help" | "?") {
                if let Some(text) = help_in_staged(length) {
                    crate::println!("{text}");
                    return;
                }
            }
            set_staged_length(length);
            if let Some(Ok(nic)) = service {
                return crate::tasks::start(name, rest.trim(), nic);
            }
            match load_staged(name) {
                Ok(()) => {
                    // `rotate=r|l`: turn the screen the tape draws on for this run, as the `rotate` setting does.
                    let (mut text, mut arguments) = ([0u8; 256], rest.trim());
                    let turn = crate::tasks::own_rotation(arguments);
                    if turn.is_some() {
                        let Some(stripped) = crate::tasks::with_screen(&mut text, arguments, 0, false) else {
                            return crate::println!("run: the arguments of a tape take at most 240 bytes.");
                        };
                        arguments = stripped;
                        crate::framebuffer::rotate_run(0, turn);
                    }
                    let outcome = run(arguments);
                    crate::framebuffer::rotate_reset();
                    if let Err(reason) = outcome {
                        crate::println!("run: {reason}.");
                    }
                }
                Err(error) => crate::println!("run: {name} is not a usable tape ({error:?})."),
            }
        }
        Err(reason) => crate::println!("run: {name}: {reason}."),
    }
}
