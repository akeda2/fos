//! Loading and running tapes: small ELF64 programs the kernel runs in its own address
//! space (trusted like kernel code; this is not isolation). See `fos_v2_abi::tape` for
//! the ABI and `docs/v2-design.md` section 9.
//!
//! A tape is a position-independent executable, so it can land anywhere: the kernel
//! keeps one 4 MiB, 2 MiB-aligned region (made executable, the only such RAM) and one
//! 4 MiB staging buffer holding the raw file. `tape load` puts a file (from FOSFS or
//! the network) into staging and then relocates it into the region; `tape run` calls
//! its entry on a stack of its own with the API table. `tape save` writes the staged
//! file to FOSFS, which is how a tape sent over the network gets installed.

use core::sync::atomic::{AtomicU64, Ordering};

use fos_v2_abi::tape::*;
use fos_v2_abi::{BootInfo, elf};

use crate::{apic, memory, paging, storage, term};

const HUGE: u64 = 2 * 1024 * 1024;
const REGION_BYTES: u64 = 4 * 1024 * 1024;
pub const STAGE_BYTES: usize = 4 * 1024 * 1024;
const STACK_BYTES: u64 = 1024 * 1024;

static mut REGION: u64 = 0;
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

#[derive(Debug)]
pub enum LoadError {
    NoMemory,
    TooBig,
    Elf(elf::Error),
}

/// Relocates the staged file into the tape region and records its entry point.
pub fn load_staged(name: &str) -> Result<(), LoadError> {
    let image_length = unsafe { STAGED_LENGTH };
    let staging = staging().ok_or(LoadError::NoMemory)?;
    let image = &staging[..image_length];
    let span = elf::span(image).map_err(LoadError::Elf)? as u64;
    if span > REGION_BYTES {
        return Err(LoadError::TooBig);
    }
    let base = region().ok_or(LoadError::NoMemory)?;
    // Clear the region first so a shorter tape never runs on a longer one's leftovers.
    unsafe { core::ptr::write_bytes(base as *mut u8, 0, REGION_BYTES as usize) };
    let entry = unsafe { elf::load(image, base) }.map_err(LoadError::Elf)?;
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

extern "C" fn api_putc(byte: u32) {
    let byte = byte as u8;
    let mut utf8 = [0u8; 4];
    let text = if byte < 0x80 { (byte as char).encode_utf8(&mut utf8) } else { "?" };
    crate::print(format_args!("{text}"));
}
/// A whole string in one console call (one repaint, one relay frame), where `putc` costs a repaint per byte.
extern "C" fn api_puts(text: *const u8, length: usize) {
    let bytes = unsafe { core::slice::from_raw_parts(text, length) };
    match core::str::from_utf8(bytes) {
        Ok(text) => crate::print(format_args!("{text}")),
        Err(_) => {
            // Not UTF-8: print what is ASCII and '?' for the rest, as `putc` does.
            for &byte in bytes {
                api_putc(u32::from(byte));
            }
        }
    }
}
extern "C" fn api_should_abort() -> u32 {
    u32::from(term::tape_should_abort())
}
extern "C" fn api_read_key() -> u32 {
    term::tape_key()
}
extern "C" fn api_set_input_mode(_mode: u32) {}
extern "C" fn api_wait_for_event() {
    term::flush_output();
    if term::nic_count() == 0 || term::all_interrupt_driven() {
        // Nothing to wait for but interrupts: a frame (MSI), a key, the 10 ms tick.
        unsafe { core::arch::asm!("hlt") };
    } else {
        // A polled NIC would only be noticed at the next 10 ms tick, adding that to every
        // frame. Yield briefly instead (about 100 us).
        let until = apic::micros() + 100;
        while apic::micros() < until {
            core::hint::spin_loop();
        }
    }
}
extern "C" fn api_ticks() -> u64 {
    apic::ticks()
}
extern "C" fn api_micros() -> u64 {
    apic::micros()
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
/// Resolves a tape's file reference: a plain `NAME` is on the boot volume, `diskN/NAME` on
/// that disk's FOSFS volume and `@LABEL/NAME` on the volume carrying that label.
///
/// A bare name is the boot volume's; for reading (`reading`) it falls back to the first other disk that has the file, so
/// a configuration file moved to another disk is still found.
fn resolve_file(reference: &[u8], reading: bool) -> Result<(crate::fosfs::Volume, &[u8]), isize> {
    let Some(slash) = reference.iter().position(|&byte| byte == b'/') else {
        let found = if reading { storage::volume_with_file(reference) } else { None };
        return found.or_else(storage::boot_volume).map(|volume| (volume, reference)).ok_or(FS_NO_VOLUME);
    };
    let (selector, name) = (&reference[..slash], &reference[slash + 1..]);
    let volume = if let Some(label) = selector.strip_prefix(b"@") {
        storage::volume_labelled(label).map(|(_, volume)| volume)
    } else {
        let number = selector.strip_prefix(b"disk").and_then(|digits| core::str::from_utf8(digits).ok()).and_then(|digits| digits.parse::<usize>().ok());
        number.and_then(|number| storage::volume_on(number).ok())
    };
    volume.map(|volume| (volume, name)).ok_or(FS_NO_VOLUME)
}
/// A FAT32/exFAT volume named by a reference: `diskN/NAME` where disk N holds no FOSFS volume of ours but a FAT one
/// (a stick or card a PC formatted). A disk with a FOSFS partition always means FOSFS.
fn resolve_fat(reference: &[u8]) -> Option<(crate::fatvol::Fs, &[u8])> {
    let slash = reference.iter().position(|&byte| byte == b'/')?;
    let (selector, name) = (&reference[..slash], &reference[slash + 1..]);
    let number = selector.strip_prefix(b"disk").and_then(|digits| core::str::from_utf8(digits).ok()).and_then(|digits| digits.parse::<usize>().ok())?;
    if storage::volume_on(number).is_ok() {
        return None;
    }
    crate::fatvol::mount(number - 1).map(|fs| (fs, name))
}

fn fat_error(error: fat::Error) -> isize {
    match error {
        fat::Error::NotFound => FS_NOT_FOUND,
        fat::Error::BadName => FS_BAD_NAME,
        _ => FS_ERROR,
    }
}

extern "C" fn api_fs_read(name: *const u8, name_length: usize, buffer: *mut u8, capacity: usize) -> isize {
    let reference = unsafe { core::slice::from_raw_parts(name, name_length) };
    if let Some((mut fs, name)) = resolve_fat(reference) {
        return match fs.find(name) {
            Ok(Some(file)) if file.size as usize > capacity => FS_TOO_BIG,
            Ok(Some(file)) => {
                let buffer = unsafe { core::slice::from_raw_parts_mut(buffer, capacity) };
                match fs.read(&file, 0, &mut buffer[..file.size as usize]) {
                    Ok(length) => length as isize,
                    Err(error) => fat_error(error),
                }
            }
            Ok(None) => FS_NOT_FOUND,
            Err(error) => fat_error(error),
        };
    }
    let (volume, name) = match resolve_file(reference, true) {
        Ok(found) => found,
        Err(code) => return code,
    };
    let buffer = unsafe { core::slice::from_raw_parts_mut(buffer, capacity) };
    match volume.read(name, buffer) {
        Ok(Some(length)) => length as isize,
        Ok(None) => FS_NOT_FOUND,
        Err(crate::fosfs::Error::TooBig) => FS_TOO_BIG,
        Err(_) => FS_ERROR,
    }
}
extern "C" fn api_fs_write(name: *const u8, name_length: usize, data: *const u8, length: usize) -> isize {
    let (reference, data) = unsafe { (core::slice::from_raw_parts(name, name_length), core::slice::from_raw_parts(data, length)) };
    let (volume, name) = match resolve_file(reference, false) {
        Ok(found) => found,
        Err(code) => return code,
    };
    match volume.write(name, data) {
        Ok(()) => 0,
        Err(crate::fosfs::Error::InvalidName) => FS_BAD_NAME,
        Err(_) => FS_ERROR,
    }
}
/// The file a tape is streaming into (`fs_stream_open`): its volume and reserved entry.
static mut STREAM: Option<(crate::fosfs::Volume, crate::fosfs::Entry)> = None;
/// Or, on a FAT32/exFAT volume: the mounted volume, the stream and the file's name (for `finish`).
static mut FAT_STREAM: Option<(crate::fatvol::Fs, fat::Stream, [u8; 64], usize)> = None;

extern "C" fn api_fs_stream_open(name: *const u8, name_length: usize, max_bytes: u64) -> isize {
    let reference = unsafe { core::slice::from_raw_parts(name, name_length) };
    unsafe {
        STREAM = None;
        FAT_STREAM = None;
    }
    if let Some((mut fs, file)) = resolve_fat(reference) {
        if file.len() > 64 {
            return FS_BAD_NAME;
        }
        return match fs.create(file, max_bytes) {
            Ok(stream) => {
                let mut stored = [0u8; 64];
                stored[..file.len()].copy_from_slice(file);
                let capacity = stream.capacity();
                unsafe { FAT_STREAM = Some((fs, stream, stored, file.len())) };
                capacity as isize
            }
            Err(error) => fat_error(error),
        };
    }
    let (volume, name) = match resolve_file(reference, false) {
        Ok(found) => found,
        Err(code) => return code,
    };
    match volume.reserve(name, max_bytes) {
        Ok((entry, capacity)) => {
            unsafe { STREAM = Some((volume, entry)) };
            capacity as isize
        }
        Err(crate::fosfs::Error::InvalidName) => FS_BAD_NAME,
        Err(_) => FS_ERROR,
    }
}
extern "C" fn api_fs_stream_write(offset: u64, data: *const u8, length: usize, file_length: u64) -> isize {
    let data = unsafe { core::slice::from_raw_parts(data, length) };
    if let Some((fs, stream, _, _)) = unsafe { (*(&raw mut FAT_STREAM)).as_mut() } {
        crate::fatvol::stamp(fs);
        return match fs.write(stream, offset, data, file_length) {
            Ok(()) => 0,
            Err(fat::Error::OutOfRange) => FS_TOO_BIG,
            Err(error) => fat_error(error),
        };
    }
    let Some((volume, entry)) = (unsafe { *(&raw const STREAM) }) else { return FS_ERROR };
    match volume.write_extent(&entry, offset, data, file_length) {
        Ok(()) => 0,
        Err(crate::fosfs::Error::TooBig) => FS_TOO_BIG,
        Err(_) => FS_ERROR,
    }
}
extern "C" fn api_fs_stream_close(file_length: u64) -> isize {
    if let Some((mut fs, stream, name, length)) = unsafe { (*(&raw mut FAT_STREAM)).take() } {
        crate::fatvol::stamp(&mut fs);
        return match fs.finish(&stream, &name[..length], file_length) {
            Ok(()) => 0,
            Err(error) => fat_error(error),
        };
    }
    let Some((volume, entry)) = (unsafe { (*(&raw mut STREAM)).take() }) else { return FS_ERROR };
    match volume.trim_extent(&entry, file_length) {
        Ok(()) => 0,
        Err(_) => FS_ERROR,
    }
}
extern "C" fn api_free_pages() -> usize {
    memory::stats().free as usize
}
extern "C" fn api_fs_remove(name: *const u8, name_length: usize) -> isize {
    let (volume, name) = match resolve_file(unsafe { core::slice::from_raw_parts(name, name_length) }, false) {
        Ok(found) => found,
        Err(code) => return code,
    };
    match volume.remove(name) {
        Ok(true) => 0,
        Ok(false) => FS_NOT_FOUND,
        Err(_) => FS_ERROR,
    }
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
extern "C" fn api_disk_info(number: usize, out: *mut fos_v2_abi::tape::DiskInfo) -> u32 {
    use fos_v2_abi::tape::{DISK_EXFAT, DISK_FAT32, DISK_FOSFS, DISK_NO_VOLUME};
    let Some(disk) = number.checked_sub(1).and_then(crate::block::get) else { return 0 };
    let mut info = fos_v2_abi::tape::DiskInfo { sectors: disk.sectors, boot: u32::from(storage::boot_disk() == Some(number - 1)), ..Default::default() };
    info.model.copy_from_slice(&disk.model);
    info.kind = match storage::volume_on(number) {
        Ok(volume) => {
            if let Ok(label) = volume.label() {
                let bytes = label.as_bytes();
                info.label[..bytes.len().min(16)].copy_from_slice(&bytes[..bytes.len().min(16)]);
            }
            if let Ok((free, largest)) = volume.free_space() {
                (info.free_bytes, info.largest_bytes) = (free, largest);
            }
            DISK_FOSFS
        }
        Err(_) => match crate::fatvol::mount(number - 1) {
            Some(mut fs) => {
                if let Ok((free, largest)) = fs.free_space() {
                    (info.free_bytes, info.largest_bytes) = (free, largest);
                }
                if fs.kind() == fat::Kind::ExFat { DISK_EXFAT } else { DISK_FAT32 }
            }
            None => DISK_NO_VOLUME,
        },
    };
    unsafe { *out = info };
    1
}
extern "C" fn api_disk_format_exfat(number: usize, label: *const u8, length: usize) -> i32 {
    use crate::fatvol::FormatError;
    let label = unsafe { core::slice::from_raw_parts(label, length) };
    match number.checked_sub(1).map_or(Err(FormatError::NoSuchDisk), |disk| crate::fatvol::format_exfat(disk, label)) {
        Ok(_) => 0,
        Err(FormatError::NoSuchDisk) => fos_v2_abi::tape::FORMAT_NO_DISK,
        Err(FormatError::BootDisk) => fos_v2_abi::tape::FORMAT_BOOT_DISK,
        Err(FormatError::BadLabel) => fos_v2_abi::tape::FORMAT_BAD_LABEL,
        Err(FormatError::Failed) => fos_v2_abi::tape::FORMAT_FAILED,
    }
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
    if length > 1514 {
        return 0;
    }
    u32::from(term::nic_send(index, unsafe { core::slice::from_raw_parts(frame, length) }))
}
extern "C" fn api_nic_receive(index: usize, buffer: *mut u8, capacity: usize) -> usize {
    term::tape_receive(index, unsafe { core::slice::from_raw_parts_mut(buffer, capacity) })
}
extern "C" fn api_audio_info(out: *mut fos_v2_abi::tape::AudioInfo) -> u32 {
    u32::from(crate::usb_audio::fill_info(unsafe { &mut *out }))
}
extern "C" fn api_audio_start(rate: u32, capture: u32, playback: u32) -> i32 {
    crate::usb_audio::start_for_tape(rate, capture != 0, playback != 0)
}
extern "C" fn api_audio_select(direction: u32, mode: u32) -> i32 {
    match crate::usb_audio::select(direction == 0, mode) {
        Ok(()) => 0,
        Err("no USB audio device") => fos_v2_abi::tape::AUDIO_NO_DEVICE,
        Err(_) => fos_v2_abi::tape::AUDIO_FAILED,
    }
}
extern "C" fn api_audio_stop() {
    crate::usb_audio::stop_for_tape();
}
extern "C" fn api_audio_read(buffer: *mut u8, length: usize) -> usize {
    term::tape_poll(); // drains the xHCI event ring, which is what moves audio
    crate::usb_audio::read(unsafe { core::slice::from_raw_parts_mut(buffer, length) })
}
extern "C" fn api_audio_write(data: *const u8, length: usize) -> usize {
    term::tape_poll();
    crate::usb_audio::write(unsafe { core::slice::from_raw_parts(data, length) })
}
extern "C" fn api_audio_stats(out: *mut fos_v2_abi::tape::AudioStats) {
    crate::usb_audio::fill_stats(unsafe { &mut *out });
}

extern "C" fn api_nic_max_frame_len() -> usize {
    1514
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
    };
    term::clear_frame_queue();
    term::set_tape_mode(true);
    crate::debug::set_tape_running(true);
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
    crate::debug::set_tape_running(false);
    crate::usb_audio::stop_for_tape(); // a tape that returns (or was aborted) must not leave an isochronous stream running
    term::set_tape_mode(false);
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
    let mut last_report = apic::ticks();
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
                if apic::ticks() - last_report >= 200 {
                    crate::println!("  {received} / {expected} bytes");
                    last_report = apic::ticks();
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
    let (volume, name) = resolve_file(file_reference(name, &mut spelled).as_bytes(), true).map_err(|_| "no such volume")?;
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
            let Ok((volume, file)) = resolve_file(file_reference(rest, &mut spelled).as_bytes(), false) else { return crate::println!("tape save: no such volume.") };
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

/// `run NAME [ARGS]`: load a tape from FOSFS and run it.
pub fn run_named(arguments: &str) {
    let (name, rest) = arguments.trim().split_once(char::is_whitespace).unwrap_or((arguments.trim(), ""));
    match load_from_file(name) {
        Ok(length) => {
            set_staged_length(length);
            match load_staged(name) {
                Ok(()) => {
                    if let Err(reason) = run(rest.trim()) {
                        crate::println!("run: {reason}.");
                    }
                }
                Err(error) => crate::println!("run: {name} is not a usable tape ({error:?})."),
            }
        }
        Err(reason) => crate::println!("run: {name}: {reason}."),
    }
}
