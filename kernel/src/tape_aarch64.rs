//! Loading and running tapes on aarch64: the same position-independent ELF64 programs as on x86 (`fosfor_abi::elf`
//! checks `e_machine` and applies R_AARCH64_RELATIVE), the same `TapeApi` calling convention (x0 = &TapeApi).
//!
//! The table handed over is the full `TapeApi` (tapes call late fields without checking `size`, so a shorter table
//! would send them to address 0). What this port has no driver for yet -- audio, NIC, serial ports, timestamps,
//! -- is a stub that reports "nothing there": zero NICs, no audio device, `AUDIO_NO_DEVICE`.

use core::sync::atomic::{AtomicU64, Ordering};

use fosfor_abi::tape::*;
use fosfor_abi::{elf, BootInfo};

use crate::{cpu, memory, paging, storage, timer};

const HUGE: u64 = 2 * 1024 * 1024;
const REGION_BYTES: u64 = 4 * 1024 * 1024;
const STAGE_BYTES: usize = 4 * 1024 * 1024;
const STACK_BYTES: u64 = 1024 * 1024;

/// Two tape regions: `run A + B` keeps both tapes loaded at once.
static mut REGIONS: [u64; 2 + crate::tasks::MAX_SERVICES] = [0; 2 + crate::tasks::MAX_SERVICES];
static mut INFO: *const BootInfo = core::ptr::null();
static SPAN: AtomicU64 = AtomicU64::new(0);

pub fn set_boot_info(info: &BootInfo) {
    unsafe { INFO = info };
}

/// Defines the stubs for calls this port cannot serve yet; each returns the value that means "not available".
macro_rules! stubs {
    ($($name:ident($($arg:ty),*) $(-> $ret:ty)? = $value:expr;)*) => {
        $(extern "C" fn $name($(_: $arg),*) $(-> $ret)? { $value })*
    };
}

stubs! {
    api_nic_max_frame_len() -> usize = 1514;
    api_nic_active() -> u32 = 0;
    api_nic_ts_enable(usize) -> u32 = 0;
    api_nic_ts_clock(usize) -> u64 = 0;
    api_nic_ts_rx(usize) -> u64 = 0;
    api_nic_send_stamped(usize, *const u8, usize, *mut u64) -> u32 = 0;
    api_nic_ts_adjust(usize, i32) -> u32 = 0;
    api_nic_ts_step(usize, i64) -> u32 = 0;
    api_nic_ts_all(usize, u32) -> u32 = 0;
    api_nic_multicast_join(usize, *const u8, u32) -> u32 = 0;
    api_nic_vlan_filter(usize, u32, u32) -> u32 = 0;
    api_nic_tx_offload(usize, u32) -> u32 = 0;
    api_nic_counters(usize, *mut u64, usize) -> usize = 0;
    api_peer_info(usize, *mut PeerInfo) -> usize = 0;
    api_hostname(usize, *mut u8, usize) -> usize = 0;
    api_disk_format_exfat(usize, *const u8, usize) -> i32 = -1;
    api_serial_count() -> usize = 0;
    api_serial_info(usize, *mut SerialInfo) -> u32 = 0;
    api_serial_open(usize, u32, u32) -> i32 = -1;
    api_serial_close(usize) = ();
    api_serial_read(usize, *mut u8, usize) -> usize = 0;
    api_serial_write(usize, *const u8, usize) -> usize = 0;
    api_serial_lines(usize) -> u32 = 0;
    api_serial_set_lines(usize, u32) = ();
    api_serial_break(usize, u32) = ();
}

fn region(slot: usize) -> Option<u64> {
    unsafe {
        let regions = &mut *core::ptr::addr_of_mut!(REGIONS);
        if regions[slot] == 0 {
            let total = REGION_BYTES + HUGE;
            let raw = memory::allocate_contiguous(total / memory::FRAME, None)?;
            let start = (raw + HUGE - 1) & !(HUGE - 1);
            memory::free(raw, (start - raw) / memory::FRAME);
            let end = start + REGION_BYTES;
            memory::free(end, (raw + total - end) / memory::FRAME);
            paging::set_executable(start, REGION_BYTES, true);
            regions[slot] = start;
        }
        Some(regions[slot])
    }
}

/// Reads `name` from the boot volume, relocates it into the tape region and returns its entry point.
pub fn load(name: &str) -> Result<u64, &'static str> {
    load_slot(name, 0)
}

/// `load` into region `slot` (0 and 1 for `run A + B`, then one per service), so several tapes can be resident together.
pub fn load_slot(name: &str, slot: usize) -> Result<u64, &'static str> {
    let volume = storage::boot_volume().ok_or("no boot volume")?;
    let staging = memory::allocate_contiguous(STAGE_BYTES as u64 / memory::FRAME, None).ok_or("no memory for staging")?;
    let buffer = unsafe { core::slice::from_raw_parts_mut(staging as *mut u8, STAGE_BYTES) };
    let outcome = (|| {
        let length = match volume.read(name.as_bytes(), buffer) {
            Ok(Some(length)) => length,
            Ok(None) => return Err("no such file"),
            Err(_) => return Err("cannot read the file (over 4 MiB?)"),
        };
        let image = &buffer[..length];
        let span = elf::span(image).map_err(|_| "not a tape for this CPU (ELF check failed)")? as u64;
        if span > REGION_BYTES {
            return Err("the tape does not fit the 4 MiB region");
        }
        let base = region(slot).ok_or("no memory for the tape region")?;
        unsafe { core::ptr::write_bytes(base as *mut u8, 0, REGION_BYTES as usize) };
        let entry = unsafe { elf::load(image, base) }.map_err(|_| "the tape's ELF segments are malformed")?;
        cpu::sync_code(base, REGION_BYTES);
        SPAN.store(span, Ordering::Relaxed);
        Ok(entry)
    })();
    memory::free(staging, STAGE_BYTES as u64 / memory::FRAME);
    outcome
}

fn make_api(info: &BootInfo, arguments: &str) -> TapeApi {
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
    }
}

/// The second screen is blanked when a tape ends, unless a service may be drawing on it.
fn close_extra_screens() {
    if crate::tasks::count() == 0 {
        crate::framebuffer::gfx_close_extra();
    }
}

/// Writes a `TapeApi` for a tape started by the service scheduler.
pub unsafe fn write_api(at: u64, arguments: &'static str) {
    if let Some(info) = unsafe { INFO.as_ref() } {
        unsafe { (at as *mut TapeApi).write(make_api(info, arguments)) };
    }
}

/// Calls the tape's entry on a stack of its own with the API table; returns when the tape returns.
pub fn run(entry: u64, arguments: &str) -> Result<(), &'static str> {
    let info = unsafe { INFO.as_ref() }.ok_or("no boot info")?;
    let stack = memory::allocate_contiguous(STACK_BYTES / memory::FRAME, None).ok_or("no memory for the tape's stack")?;
    let api = make_api(info, arguments);
    cpu::fp_enable();
    unsafe {
        // x21 is callee-saved, so the tape preserves it: it keeps our stack pointer across the call.
        core::arch::asm!(
            "mov x21, sp",
            "mov sp, x20",
            "blr {entry}",
            "mov sp, x21",
            entry = in(reg) entry,
            in("x0") &api as *const TapeApi,
            in("x20") stack + STACK_BYTES,
            out("x21") _,
            clobber_abi("C"),
        );
    }
    crate::framebuffer::gfx_close();
    close_extra_screens();
    unsafe { PENDING_LENGTH = 0 };
    memory::free(stack, STACK_BYTES / memory::FRAME);
    Ok(())
}

static mut PENDING: [u8; 4] = [0; 4];
static mut PENDING_LENGTH: usize = 0;

/// Bytes above 0x7f are collected until they make one valid UTF-8 character; a broken sequence prints '?'.
extern "C" fn api_putc(byte: u32) {
    let byte = byte as u8;
    let pending = unsafe { &mut *core::ptr::addr_of_mut!(PENDING) };
    let length = unsafe { &mut *core::ptr::addr_of_mut!(PENDING_LENGTH) };
    if byte < 0x80 {
        if *length > 0 {
            *length = 0;
            crate::print(format_args!("?"));
        }
        crate::print(format_args!("{}", byte as char));
        return;
    }
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
        Err(_) => {}
    }
}

// Screen 0 is the boot framebuffer. The calls take no screen index yet: a second screen will arrive as an append-only
// ABI version (docs/aarch64-plan.md), and these stay the calls for screen 0.
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

const TAPE_ABORT: u16 = 0x100;
const INPUT_CAPACITY: usize = 32;
static mut INPUT: [u16; INPUT_CAPACITY] = [0; INPUT_CAPACITY];
static mut INPUT_HEAD: usize = 0;
static mut INPUT_TAIL: usize = 0;
/// When a lone ESC byte arrived on the serial console and may still start a cursor-key sequence (0 = none pending).
static mut SERIAL_ESCAPE_AT: u64 = 0;
/// How long a serial Escape waits for the `[` of a sequence; a terminal sends the bytes back to back.
const SERIAL_ESCAPE_US: u64 = 50_000;

fn input_push(value: u16) {
    unsafe {
        let next = (INPUT_HEAD + 1) % INPUT_CAPACITY;
        if next != INPUT_TAIL {
            (*core::ptr::addr_of_mut!(INPUT))[INPUT_HEAD] = value;
            INPUT_HEAD = next;
        }
    }
}

/// Moves what the keyboards typed into the tape's input queue, as on x86 (`term.rs` `tape_service`): a USB cursor key becomes
/// the bytes a terminal sends (ESC [ A), å ä ö become UTF-8, a lone Escape is "stop" (`TAPE_ABORT`). On the serial console an
/// ESC followed within 50 ms by `[` or `O` starts a sequence; any other ESC is "stop".
fn input_pump() {
    crate::usb_kbd::poll();
    while let Some(key) = crate::usb_kbd::pop() {
        if key == 0x1b {
            input_push(TAPE_ABORT);
        } else if let Some(sequence) = fosfor_abi::keymap::ansi_sequence(key) {
            input_push(0x1b);
            input_push(u16::from(b'['));
            sequence.iter().for_each(|&byte| input_push(u16::from(byte)));
        } else if let Some(sequence) = fosfor_abi::keymap::utf8_sequence(key) {
            sequence.iter().for_each(|&byte| input_push(u16::from(byte)));
        } else {
            input_push(u16::from(key));
        }
    }
    while let Some(key) = crate::arch::aarch64::pl011::read_byte() {
        unsafe {
            if SERIAL_ESCAPE_AT != 0 {
                SERIAL_ESCAPE_AT = 0;
                if key == b'[' || key == b'O' {
                    input_push(0x1b);
                    input_push(u16::from(key));
                    continue;
                }
                input_push(TAPE_ABORT);
            }
            if key == 0x1b {
                SERIAL_ESCAPE_AT = timer::micros().max(1);
                continue;
            }
        }
        input_push(u16::from(key));
    }
    unsafe {
        if SERIAL_ESCAPE_AT != 0 && timer::micros().wrapping_sub(SERIAL_ESCAPE_AT) > SERIAL_ESCAPE_US {
            SERIAL_ESCAPE_AT = 0;
            input_push(TAPE_ABORT);
        }
    }
}

/// The next typed key (a byte, or `TAPE_ABORT`) from the serial console or the USB keyboard. With two tapes running only the
/// focused one gets keys; Tab passes the focus to the other.
fn next_key() -> Option<u16> {
    let tasks = tasks();
    if tasks.active && tasks.current != tasks.focus {
        return None;
    }
    input_pump();
    unsafe {
        if INPUT_TAIL == INPUT_HEAD {
            return None;
        }
        let value = (*core::ptr::addr_of!(INPUT))[INPUT_TAIL];
        INPUT_TAIL = (INPUT_TAIL + 1) % INPUT_CAPACITY;
        if tasks.active && value == 0x09 {
            if let Some(other) = tasks.next_live(tasks.focus) {
                tasks.focus = other;
            }
            INPUT_HEAD = 0;
            INPUT_TAIL = 0;
            return None;
        }
        Some(value)
    }
}

extern "C" fn api_read_key() -> u32 {
    if crate::tasks::in_service() {
        return crate::tasks::service_read_key();
    }
    crate::tasks::maybe_yield();
    match next_key() {
        None => KEY_NONE,
        Some(TAPE_ABORT) => KEY_ABORT,
        Some(byte) => KEY_BASE + u32::from(byte),
    }
}

/// Escape stops a tape; any other key typed meanwhile is dropped (`should_abort` and `read_key` share one input).
extern "C" fn api_should_abort() -> u32 {
    if crate::tasks::in_service() {
        return u32::from(crate::tasks::service_should_abort());
    }
    crate::tasks::maybe_yield();
    while let Some(key) = next_key() {
        if key == TAPE_ABORT {
            return 1;
        }
    }
    0
}

extern "C" fn api_set_input_mode(_mode: u32) {}

/// A tape found nothing to do. Alone, the CPU sleeps until the next interrupt. With two tapes each gets a pass before the CPU
/// sleeps, and the other tape runs now.
extern "C" fn api_wait_for_event() {
    if crate::tasks::in_service() {
        return crate::tasks::service_wait();
    }
    // Services get a pass; the CPU sleeps only when they are all idle too.
    let busy = crate::tasks::pass();
    let tasks = tasks();
    if !tasks.active {
        if !busy {
            cpu::sleep();
        }
        return;
    }
    tasks.streak += 1;
    if tasks.streak >= tasks.live() {
        tasks.streak = 0;
        if !busy {
            cpu::sleep();
        }
    }
    yield_to_next();
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

extern "C" fn api_free_pages() -> usize {
    memory::stats().free as usize
}

extern "C" fn api_fs_read(name: *const u8, name_length: usize, buffer: *mut u8, capacity: usize) -> isize {
    let (name, buffer) = unsafe { (core::slice::from_raw_parts(name, name_length), core::slice::from_raw_parts_mut(buffer, capacity)) };
    crate::tape_fs::read(name, buffer)
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

// Network calls: the Pi 4's Genet is NIC 0; elsewhere there is no NIC. Multicast reception is promiscuous mode (see genet.rs).
extern "C" fn api_nic_count() -> usize {
    usize::from(crate::genet::present())
}

extern "C" fn api_nic_mac(index: usize, out: *mut u8) -> u32 {
    match crate::genet::mac().filter(|_| index == 0) {
        Some(mac) => {
            unsafe { core::ptr::copy_nonoverlapping(mac.as_ptr(), out, 6) };
            1
        }
        None => 0,
    }
}

extern "C" fn api_nic_send(index: usize, frame: *const u8, length: usize) -> u32 {
    u32::from(index == 0 && length <= 1518 && crate::genet::send(unsafe { core::slice::from_raw_parts(frame, length) }))
}

extern "C" fn api_nic_receive(index: usize, buffer: *mut u8, capacity: usize) -> usize {
    // A NIC a service owns delivers to the service alone.
    if index != 0 || (crate::tasks::service_owns(index) && !crate::tasks::in_service()) {
        return 0;
    }
    crate::genet::receive(unsafe { core::slice::from_raw_parts_mut(buffer, capacity) })
}

extern "C" fn api_nic_set_promiscuous(index: usize, on: u32) -> u32 {
    u32::from(index == 0 && crate::genet::set_promiscuous(on != 0))
}

extern "C" fn api_nic_set_multicast(index: usize, on: u32) -> u32 {
    api_nic_set_promiscuous(index, on)
}

extern "C" fn api_fs_write(name: *const u8, name_length: usize, data: *const u8, length: usize) -> isize {
    let (name, data) = unsafe { (core::slice::from_raw_parts(name, name_length), core::slice::from_raw_parts(data, length)) };
    crate::tape_fs::write(name, data)
}

extern "C" fn api_fs_list(volume: *const u8, volume_length: usize, index: usize, out: *mut FileInfo) -> isize {
    crate::tape_fs::list(unsafe { core::slice::from_raw_parts(volume, volume_length) }, index, unsafe { &mut *out })
}

extern "C" fn api_audio_info(out: *mut AudioInfo) -> u32 {
    u32::from(crate::audio::fill_info(unsafe { &mut *out }))
}
extern "C" fn api_audio_start(rate: u32, capture: u32, playback: u32) -> i32 {
    crate::audio::start_for_tape(rate, capture != 0, playback != 0)
}
extern "C" fn api_audio_stop() {
    crate::audio::stop_for_tape();
}
/// USB audio moves through the xHCI event ring, which `input_pump` (and so every tape's key poll) drains; the audio calls
/// drain it themselves too, since a tape that only streams may not read keys often.
extern "C" fn api_audio_read(buffer: *mut u8, length: usize) -> usize {
    crate::usb_kbd::poll();
    crate::audio::read(unsafe { core::slice::from_raw_parts_mut(buffer, length) })
}
extern "C" fn api_audio_write(data: *const u8, length: usize) -> usize {
    crate::usb_kbd::poll();
    crate::audio::write(unsafe { core::slice::from_raw_parts(data, length) })
}
extern "C" fn api_audio_stats(out: *mut AudioStats) {
    crate::audio::fill_stats(unsafe { &mut *out });
}
extern "C" fn api_audio_select(direction: u32, mode: u32) -> i32 {
    crate::audio::select_for_tape(direction == 0, mode)
}
extern "C" fn api_audio_buffer(out: *mut AudioBuffer) {
    let (ring_bytes, available_bytes) = crate::audio::buffer_info();
    unsafe { *out = AudioBuffer { ring_bytes, available_bytes } };
}
extern "C" fn api_audio_set_capture_ring(bytes: u64) {
    crate::usb_audio::set_capture_ring(bytes);
}
extern "C" fn api_audio_devices(number: u32, out: *mut AudioDevice) -> u32 {
    if !out.is_null() {
        if let Some(device) = crate::audio::device_info(number as usize) {
            unsafe { out.write(device) };
        }
    }
    crate::audio::device_count() as u32
}
extern "C" fn api_audio_use_device(number: u32) -> i32 {
    crate::audio::use_device_for_tape(number)
}
extern "C" fn api_audio_use_output(number: u32) -> i32 {
    crate::audio::use_output_for_tape(number)
}

extern "C" fn api_wall_clock(out: *mut WallClock) -> u32 {
    match crate::clock::now() {
        Some(time) => {
            unsafe { *out = WallClock { year: time.year, month: time.month, day: time.day, hour: time.hour, minute: time.minute, second: time.second, reserved: 0 } };
            1
        }
        None => 0,
    }
}
extern "C" fn api_wall_clock_set(time: *const WallClock) -> u32 {
    let time = unsafe { *time };
    let time = fat::Time { year: time.year, month: time.month, day: time.day, hour: time.hour, minute: time.minute, second: time.second };
    u32::from(crate::clock::set(&time).is_ok())
}

extern "C" fn api_disk_info(number: usize, out: *mut DiskInfo) -> u32 {
    crate::tape_fs::disk_info(number, unsafe { &mut *out })
}

extern "C" fn api_fs_remove(name: *const u8, name_length: usize) -> isize {
    crate::tape_fs::remove(unsafe { core::slice::from_raw_parts(name, name_length) })
}

extern "C" fn api_puts(text: *const u8, length: usize) {
    let bytes = unsafe { core::slice::from_raw_parts(text, length) };
    match core::str::from_utf8(bytes) {
        Ok(text) => crate::print(format_args!("{text}")),
        Err(_) => bytes.iter().for_each(|&byte| api_putc(u32::from(byte))),
    }
}

extern "C" fn api_console_size() -> u32 {
    crate::framebuffer::size().map_or(0, |(columns, rows)| ((columns as u32) << 16) | rows as u32 & 0xffff)
}

/// Hard-float tapes run on the CPU's baseline FP/SIMD (NEON), which `cpu::fp_enable` switches on before the call;
/// Baseline FP is NEON/FP, which every aarch64 CPU has; there is no AVX2 equivalent to offer, so level 2 is never returned.
extern "C" fn api_fp_level() -> u32 {
    FP_NEON
}

// Two tapes on one core (`run A + B`): cooperative tasks. Each tape has a stack of its own and runs until it calls
// `wait_for_event` (what `tape_rt::idle` does between polling passes), which switches to the other. The kernel is never
// interrupted halfway, so its unlocked state needs no protection; that is why this is not a second core (the kernel runs
// on core 0 only and is not safe to enter from two cores).
core::arch::global_asm!(
    r#"
    .arch_extension fp
    .global fos_task_switch
fos_task_switch:
    sub sp, sp, #176
    stp x19, x20, [sp, #0]
    stp x21, x22, [sp, #16]
    stp x23, x24, [sp, #32]
    stp x25, x26, [sp, #48]
    stp x27, x28, [sp, #64]
    stp x29, x30, [sp, #80]
    stp d8, d9, [sp, #96]
    stp d10, d11, [sp, #112]
    stp d12, d13, [sp, #128]
    stp d14, d15, [sp, #144]
    mov x2, sp
    str x2, [x0]
    mov sp, x1
    ldp x19, x20, [sp, #0]
    ldp x21, x22, [sp, #16]
    ldp x23, x24, [sp, #32]
    ldp x25, x26, [sp, #48]
    ldp x27, x28, [sp, #64]
    ldp x29, x30, [sp, #80]
    ldp d8, d9, [sp, #96]
    ldp d10, d11, [sp, #112]
    ldp d12, d13, [sp, #128]
    ldp d14, d15, [sp, #144]
    add sp, sp, #176
    ret
    .global fos_task_start
fos_task_start:
    mov x0, x20
    blr x19
    bl fos_task_finished
    brk #0
"#
);

unsafe extern "C" {
    /// Saves the callee-saved registers on this stack, stores the stack pointer at `save`, resumes the context at `to`.
    fn fos_task_switch(save: *mut u64, to: u64);
    fn fos_task_start();
}

const TASKS: usize = 2;
/// Bytes `fos_task_switch` pushes: x19-x30 and d8-d15.
const SWITCH_FRAME: u64 = 176;

struct Tasks {
    active: bool,
    sp: [u64; TASKS],
    done: [bool; TASKS],
    /// Where `run_two` waits for the tasks to end.
    main_sp: u64,
    current: usize,
    /// The task that gets the keyboard.
    focus: usize,
    /// Tasks that have idled since the CPU last slept.
    streak: usize,
}

static mut TASKS_STATE: Tasks = Tasks { active: false, sp: [0; TASKS], done: [false; TASKS], main_sp: 0, current: 0, focus: 0, streak: 0 };

fn tasks() -> &'static mut Tasks {
    unsafe { &mut *core::ptr::addr_of_mut!(TASKS_STATE) }
}

impl Tasks {
    fn live(&self) -> usize {
        self.done.iter().filter(|done| !**done).count()
    }

    /// The next task after `from` that has not finished.
    fn next_live(&self, from: usize) -> Option<usize> {
        (1..TASKS).map(|step| (from + step) % TASKS).find(|&task| !self.done[task])
    }
}

fn yield_to_next() {
    let tasks = tasks();
    let me = tasks.current;
    if let Some(next) = tasks.next_live(me) {
        tasks.current = next;
        unsafe { fos_task_switch(&mut tasks.sp[me], tasks.sp[next]) };
    }
}

/// Where a task lands when its tape returns (`fos_task_start`): runs the other task, or hands back to `run_two`.
#[unsafe(no_mangle)]
extern "C" fn fos_task_finished() -> ! {
    let tasks = tasks();
    let me = tasks.current;
    tasks.done[me] = true;
    let next = tasks.next_live(me);
    if tasks.focus == me {
        tasks.focus = next.unwrap_or(me);
    }
    let to = match next {
        Some(next) => {
            tasks.current = next;
            tasks.sp[next]
        }
        None => tasks.main_sp,
    };
    unsafe { fos_task_switch(&mut tasks.sp[me], to) };
    loop {
        cpu::sleep();
    }
}

/// Runs two loaded tapes side by side on this core until both have returned. Tab moves the keyboard between them.
pub fn run_two(tapes: [(u64, &str); TASKS]) -> Result<(), &'static str> {
    let info = unsafe { INFO.as_ref() }.ok_or("no boot info")?;
    let mut stacks = [0u64; TASKS];
    for index in 0..TASKS {
        match memory::allocate_contiguous(STACK_BYTES / memory::FRAME, None) {
            Some(stack) => stacks[index] = stack,
            None => {
                for stack in stacks[..index].iter() {
                    memory::free(*stack, STACK_BYTES / memory::FRAME);
                }
                return Err("no memory for the tapes' stacks");
            }
        }
    }
    cpu::fp_enable();
    let state = tasks();
    for (index, (entry, arguments)) in tapes.iter().enumerate() {
        // The tape's API table sits at the top of its stack; below it a first switch frame that "returns" into
        // fos_task_start with x19 = entry and x20 = the table.
        let api_at = (stacks[index] + STACK_BYTES as u64 - core::mem::size_of::<TapeApi>() as u64) & !15;
        let frame = api_at - SWITCH_FRAME;
        unsafe {
            (api_at as *mut TapeApi).write(make_api(info, arguments));
            core::ptr::write_bytes(frame as *mut u8, 0, SWITCH_FRAME as usize);
            let words = frame as *mut u64;
            words.write(*entry);
            words.add(1).write(api_at);
            words.add(11).write(fos_task_start as usize as u64);
        }
        state.sp[index] = frame;
        state.done[index] = false;
    }
    state.focus = 0;
    state.streak = 0;
    state.current = 0;
    state.active = true;
    unsafe { fos_task_switch(&mut state.main_sp, state.sp[0]) };
    state.active = false;
    crate::framebuffer::gfx_close();
    close_extra_screens();
    unsafe { PENDING_LENGTH = 0 };
    for stack in stacks {
        memory::free(stack, STACK_BYTES / memory::FRAME);
    }
    Ok(())
}
