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
fn resolve_file(reference: &[u8]) -> Result<(crate::fosfs::Volume, &[u8]), isize> {
    let Some(slash) = reference.iter().position(|&byte| byte == b'/') else {
        return storage::boot_volume().map(|volume| (volume, reference)).ok_or(FS_NO_VOLUME);
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
extern "C" fn api_fs_read(name: *const u8, name_length: usize, buffer: *mut u8, capacity: usize) -> isize {
    let reference = unsafe { core::slice::from_raw_parts(name, name_length) };
    let (volume, name) = match resolve_file(reference) {
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
    let (volume, name) = match resolve_file(reference) {
        Ok(found) => found,
        Err(code) => return code,
    };
    match volume.write(name, data) {
        Ok(()) => 0,
        Err(crate::fosfs::Error::InvalidName) => FS_BAD_NAME,
        Err(_) => FS_ERROR,
    }
}
extern "C" fn api_free_pages() -> usize {
    memory::stats().free as usize
}
extern "C" fn api_fs_remove(name: *const u8, name_length: usize) -> isize {
    let (volume, name) = match resolve_file(unsafe { core::slice::from_raw_parts(name, name_length) }) {
        Ok(found) => found,
        Err(code) => return code,
    };
    match volume.remove(name) {
        Ok(true) => 0,
        Ok(false) => FS_NOT_FOUND,
        Err(_) => FS_ERROR,
    }
}
extern "C" fn api_nic_count() -> usize {
    term::nic_count()
}
extern "C" fn api_nic_set_promiscuous(index: usize, on: u32) -> u32 {
    u32::from(term::nic_set_promiscuous(index, on != 0))
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

fn load_from_file(name: &str) -> Result<usize, &'static str> {
    let volume = storage::boot_volume().ok_or("no FOSFS volume is mounted")?;
    let staging = staging().ok_or("no memory for the staging buffer")?;
    match volume.read(name.as_bytes(), staging) {
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
            let rest = rest.strip_prefix("disk").map_or(rest, str::trim_start);
            let (name, outcome) = if rest == "net" { ("network", receive_from_network()) } else { (rest, load_from_file(rest)) };
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
            let Some(volume) = storage::boot_volume() else { return crate::println!("tape save: no FOSFS volume is mounted.") };
            let Some(staging) = staging() else { return };
            if length == 0 {
                return crate::println!("tape save: nothing is staged (tape load first).");
            }
            match volume.write(rest.as_bytes(), &staging[..length]) {
                Ok(()) => crate::println!("Saved {length} bytes as {rest}."),
                Err(error) => crate::println!("tape save: {error:?}"),
            }
        }
        "status" | "" => match loaded() {
            Some((name, entry, span)) => crate::println!("Tape {name}: entry {entry:#x}, {span} bytes in memory, {} bytes staged.", staged_length()),
            None => crate::println!("No tape loaded."),
        },
        _ => crate::println!("tape: load NAME|net, run [ARGS], save NAME, status."),
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
