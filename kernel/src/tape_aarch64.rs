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

static mut REGION: u64 = 0;
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
    api_audio_info(*mut AudioInfo) -> u32 = 0;
    api_audio_start(u32, u32, u32) -> i32 = AUDIO_NO_DEVICE;
    api_audio_stop() = ();
    api_audio_read(*mut u8, usize) -> usize = 0;
    api_audio_write(*const u8, usize) -> usize = 0;
    api_audio_stats(*mut AudioStats) = ();
    api_audio_select(u32, u32) -> i32 = AUDIO_NO_DEVICE;
    api_audio_buffer(*mut AudioBuffer) = ();
    api_audio_set_capture_ring(u64) = ();
    api_audio_devices(u32, *mut AudioDevice) -> u32 = 0;
    api_audio_use_device(u32) -> i32 = AUDIO_NO_DEVICE;
    api_audio_use_output(u32) -> i32 = AUDIO_NO_DEVICE;
    api_hostname(usize, *mut u8, usize) -> usize = 0;
    api_disk_info(usize, *mut DiskInfo) -> u32 = 0;
    api_disk_format_exfat(usize, *const u8, usize) -> i32 = -1;
    api_wall_clock(*mut WallClock) -> u32 = 0;
    api_wall_clock_set(*const WallClock) -> u32 = 0;
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

fn region() -> Option<u64> {
    unsafe {
        if REGION == 0 {
            let total = REGION_BYTES + HUGE;
            let raw = memory::allocate_contiguous(total / memory::FRAME, None)?;
            let start = (raw + HUGE - 1) & !(HUGE - 1);
            memory::free(raw, (start - raw) / memory::FRAME);
            let end = start + REGION_BYTES;
            memory::free(end, (raw + total - end) / memory::FRAME);
            paging::set_executable(start, REGION_BYTES, true);
            REGION = start;
        }
        Some(REGION)
    }
}

/// Reads `name` from the boot volume, relocates it into the tape region and returns its entry point.
pub fn load(name: &str) -> Result<u64, &'static str> {
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
        let base = region().ok_or("no memory for the tape region")?;
        unsafe { core::ptr::write_bytes(base as *mut u8, 0, REGION_BYTES as usize) };
        let entry = unsafe { elf::load(image, base) }.map_err(|_| "the tape's ELF segments are malformed")?;
        cpu::sync_code(base, REGION_BYTES);
        SPAN.store(span, Ordering::Relaxed);
        Ok(entry)
    })();
    memory::free(staging, STAGE_BYTES as u64 / memory::FRAME);
    outcome
}

/// Calls the tape's entry on a stack of its own with the API table; returns when the tape returns.
pub fn run(entry: u64, arguments: &str) -> Result<(), &'static str> {
    let info = unsafe { INFO.as_ref() }.ok_or("no boot info")?;
    let stack = memory::allocate_contiguous(STACK_BYTES / memory::FRAME, None).ok_or("no memory for the tape's stack")?;
    let api = TapeApi {
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
    };
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

/// The next typed byte from the serial console or the USB keyboard.
fn next_byte() -> Option<u8> {
    crate::usb_kbd::poll();
    crate::arch::aarch64::pl011::read_byte().or_else(crate::usb_kbd::pop)
}

extern "C" fn api_read_key() -> u32 {
    match next_byte() {
        None => KEY_NONE,
        Some(0x1b) => KEY_ABORT,
        Some(byte) => KEY_BASE + u32::from(byte),
    }
}

/// Escape stops a tape; any other key typed meanwhile is dropped (`should_abort` and `read_key` share one input).
extern "C" fn api_should_abort() -> u32 {
    u32::from(next_byte() == Some(0x1b))
}

extern "C" fn api_set_input_mode(_mode: u32) {}

extern "C" fn api_wait_for_event() {
    cpu::sleep();
}

extern "C" fn api_ticks() -> u64 {
    timer::ticks()
}

extern "C" fn api_micros() -> u64 {
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
    if index != 0 {
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
