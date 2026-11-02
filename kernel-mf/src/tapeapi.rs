//! The tape ABI for ring 3 (slice 8c): `fosfor_abi::tape::TapeApi` as system calls.
//!
//! A tape's entry is `extern "C" fn(&TapeApi)`, a table of function pointers. Here every process gets, mapped at fixed user
//! addresses, a *shim page* (read, execute: an entry wrapper and one 16-byte stub per table slot), a *table page* (read only: the
//! `TapeApi` the tape is handed, each function pointer aimed at its stub) and an *args page*. A stub is
//! `mov r10, rcx; mov eax, 0x100 + slot; syscall; ret`: the System V arguments stay where they are (`syscall` clobbers rcx, so the
//! fourth goes through r10) and `process::syscall` hands the number to `call` below. The slot is the table offset divided by
//! eight, so the table's layout stays the single source of truth: every field after the 8-byte version/size header is one word.
//!
//! What a call is worth today: console (`putc`, `puts`), time (`ticks`, `micros`), memory (`alloc`/`free` map zeroed pages into
//! the process; the address is virtual, not physical), `args`, `should_abort` (the process was killed), `fp_level`, `free_pages`,
//! `console_size` and the pixel screen (`gfx_*`). Everything else answers 0 ("none", "failed"), which the tapes read as "this
//! machine has no such device"; the `fs_*` calls answer `FS_NO_VOLUME`. Input, files, NICs, audio, MIDI, touch and GPIO come in
//! later slices (the drivers are unlocked and CPU-0 only; see docs/big-kernel.md). Processes are single-threaded.

use core::mem::{offset_of, size_of};

use fosfor_abi::tape::{FS_NO_VOLUME, KEY_NONE, TAPE_API_VERSION, TapeApi};

use crate::trap::Frame;
use crate::{apic, fb, fpu, pmm, process, sched, vm};

pub const SHIM_BASE: u64 = vm::USER_BASE + 0x200_0000;
pub const TABLE_BASE: u64 = SHIM_BASE + 0x1000;
pub const ARGS_BASE: u64 = SHIM_BASE + 0x2000;
pub const HEAP_BASE: u64 = vm::USER_BASE + 0x4000_0000;
const HEAP_LIMIT: u64 = HEAP_BASE + 0x1000_0000;
pub const ARGS_MAX: usize = 4096;
/// System call numbers from here up are table slots.
pub const CALL_BASE: u64 = 0x100;
const STUBS: usize = 128;
const SLOTS: usize = size_of::<TapeApi>() / 8;
const _: () = assert!(size_of::<TapeApi>() % 8 == 0 && SLOTS <= STUBS);

core::arch::global_asm!(
    r#"
    .text
    .balign 16
    .global tape_shim_start, tape_shim_stubs, tape_shim_end
tape_shim_start:                     # entered with rdi = the table, rsi = the tape's entry point
    call rsi
    mov eax, 1                       # the tape returned: exit(0)
    xor edi, edi
    syscall
    ud2
    .balign 16
tape_shim_stubs:
    .set slot, 0
    .rept 128
    mov r10, rcx
    mov eax, 0x100 + slot
    syscall
    ret
    .balign 16
    .set slot, slot + 1
    .endr
tape_shim_end:
"#
);

unsafe extern "C" {
    static tape_shim_start: u8;
    static tape_shim_stubs: u8;
    static tape_shim_end: u8;
}

const fn slot(offset: usize) -> u64 {
    (offset / 8) as u64
}

const PUTC: u64 = slot(offset_of!(TapeApi, putc));
const SHOULD_ABORT: u64 = slot(offset_of!(TapeApi, should_abort));
const READ_KEY: u64 = slot(offset_of!(TapeApi, read_key));
const SET_INPUT_MODE: u64 = slot(offset_of!(TapeApi, set_input_mode));
const WAIT_FOR_EVENT: u64 = slot(offset_of!(TapeApi, wait_for_event));
const TICKS: u64 = slot(offset_of!(TapeApi, ticks));
const MICROS: u64 = slot(offset_of!(TapeApi, micros));
const ALLOC: u64 = slot(offset_of!(TapeApi, alloc));
const FREE: u64 = slot(offset_of!(TapeApi, free));
const ARGS: u64 = slot(offset_of!(TapeApi, args));
const ARGS_LEN: u64 = slot(offset_of!(TapeApi, args_len));
const FS_READ: u64 = slot(offset_of!(TapeApi, fs_read));
const FS_WRITE: u64 = slot(offset_of!(TapeApi, fs_write));
const FS_REMOVE: u64 = slot(offset_of!(TapeApi, fs_remove));
const FS_LIST: u64 = slot(offset_of!(TapeApi, fs_list));
const FS_READ_AT: u64 = slot(offset_of!(TapeApi, fs_read_at));
const MEMORY_MAP: u64 = slot(offset_of!(TapeApi, memory_map));
const MEMORY_MAP_COUNT: u64 = slot(offset_of!(TapeApi, memory_map_count));
const MEMORY_MAP_DESCRIPTOR_SIZE: u64 = slot(offset_of!(TapeApi, memory_map_descriptor_size));
const FREE_PAGES: u64 = slot(offset_of!(TapeApi, free_pages));
const PUTS: u64 = slot(offset_of!(TapeApi, puts));
const CONSOLE_SIZE: u64 = slot(offset_of!(TapeApi, console_size));
const FP_LEVEL: u64 = slot(offset_of!(TapeApi, fp_level));
const GFX_INFO: u64 = slot(offset_of!(TapeApi, gfx_info));
const GFX_OPEN: u64 = slot(offset_of!(TapeApi, gfx_open));
const GFX_CLOSE: u64 = slot(offset_of!(TapeApi, gfx_close));
const GFX_FILL: u64 = slot(offset_of!(TapeApi, gfx_fill));
const GFX_BLIT: u64 = slot(offset_of!(TapeApi, gfx_blit));
const GFX_SCREENS: u64 = slot(offset_of!(TapeApi, gfx_screens));
const GFX_INFO_ON: u64 = slot(offset_of!(TapeApi, gfx_info_on));
const GFX_OPEN_ON: u64 = slot(offset_of!(TapeApi, gfx_open_on));
const GFX_CLOSE_ON: u64 = slot(offset_of!(TapeApi, gfx_close_on));
const GFX_FILL_ON: u64 = slot(offset_of!(TapeApi, gfx_fill_on));
const GFX_BLIT_ON: u64 = slot(offset_of!(TapeApi, gfx_blit_on));
const AUDIO_FLOAT: u64 = slot(offset_of!(TapeApi, audio_float));

/// The table slots that hold data, not a function.
fn is_data(slot: u64) -> bool {
    [ARGS, ARGS_LEN, MEMORY_MAP, MEMORY_MAP_COUNT, MEMORY_MAP_DESCRIPTOR_SIZE].contains(&slot)
}

/// Where a process's code stub for table slot `slot` lives.
fn stub_address(slot: u64) -> u64 {
    let offset = unsafe { &tape_shim_stubs as *const u8 as usize - &tape_shim_start as *const u8 as usize };
    SHIM_BASE + offset as u64 + 16 * slot
}

/// Maps the shim, the table and the args page into `root`. `Some(())` on success; the frames belong to the address space.
pub fn map_support(root: u64, args: &[u8]) -> Option<()> {
    let shim = unsafe { core::slice::from_raw_parts(&tape_shim_start as *const u8, &tape_shim_end as *const u8 as usize - &tape_shim_start as *const u8 as usize) };
    let code = pmm::allocate(1, None)?;
    unsafe { core::ptr::copy_nonoverlapping(shim.as_ptr(), vm::direct(code) as *mut u8, shim.len()) };
    vm::map_user_page(root, SHIM_BASE, code, false, true).then_some(())?;

    let table = pmm::allocate(1, None)?;
    let words = vm::direct(table) as *mut u64;
    unsafe {
        words.write(u64::from(TAPE_API_VERSION) | (size_of::<TapeApi>() as u64) << 32);
        for index in 1..SLOTS as u64 {
            let value = match index {
                ARGS => ARGS_BASE,
                ARGS_LEN => args.len().min(ARGS_MAX) as u64,
                MEMORY_MAP | MEMORY_MAP_COUNT | MEMORY_MAP_DESCRIPTOR_SIZE => 0,
                _ => stub_address(index),
            };
            words.add(index as usize).write(value);
        }
    }
    vm::map_user_page(root, TABLE_BASE, table, false, false).then_some(())?;

    let page = pmm::allocate(1, None)?;
    let length = args.len().min(ARGS_MAX);
    unsafe { core::ptr::copy_nonoverlapping(args.as_ptr(), vm::direct(page) as *mut u8, length) };
    vm::map_user_page(root, ARGS_BASE, page, false, false).then_some(())
}

fn user_bytes(address: u64, length: u64) -> Option<alloc::vec::Vec<u8>> {
    process::user_bytes(address, length)
}

fn pages(bytes: u64) -> u64 {
    bytes.div_ceil(4096)
}

fn alloc_pages(bytes: u64) -> u64 {
    let count = pages(bytes);
    let root = sched::current_root();
    let start = match sched::heap_next() {
        0 => HEAP_BASE,
        next => next,
    };
    if count == 0 || start + count * 4096 > HEAP_LIMIT {
        return 0;
    }
    for page in 0..count {
        let mapped = pmm::allocate(1, None).and_then(|frame| {
            if vm::map_user_page(root, start + page * 4096, frame, true, false) {
                Some(())
            } else {
                pmm::free(frame, 1);
                None
            }
        });
        if mapped.is_none() {
            for undo in 0..page {
                if let Some(frame) = vm::unmap_user_page(root, start + undo * 4096) {
                    pmm::free(frame, 1);
                }
            }
            return 0;
        }
    }
    sched::set_heap_next(start + count * 4096);
    start
}

fn free_pages_at(address: u64, bytes: u64) {
    let root = sched::current_root();
    if address % 4096 != 0 || address < HEAP_BASE || address.saturating_add(bytes) > HEAP_LIMIT {
        return;
    }
    for page in 0..pages(bytes) {
        if let Some(frame) = vm::unmap_user_page(root, address + page * 4096) {
            pmm::free(frame, 1);
        }
    }
}

/// One table call from ring 3: `slot` is the stub's number minus `CALL_BASE`, the arguments are as System V passes them.
pub fn call(frame: &mut Frame) {
    let number = frame.rax - CALL_BASE;
    let (a, b, c, d, e) = (frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8);
    let _ = (c, d, e);
    frame.rax = match number {
        PUTC => {
            if a < 0x80 {
                print!("{}", a as u8 as char);
            }
            0
        }
        PUTS => {
            if let Some(bytes) = user_bytes(a, b.min(4096)) {
                process::console_write(&bytes);
            }
            0
        }
        SHOULD_ABORT => u64::from(sched::current_killed()),
        READ_KEY => u64::from(KEY_NONE),
        SET_INPUT_MODE => 0,
        WAIT_FOR_EVENT => {
            sched::sleep_ticks(1);
            0
        }
        TICKS => apic::ticks(),
        MICROS => apic::micros(),
        ALLOC => alloc_pages(a),
        FREE => {
            free_pages_at(a, b);
            0
        }
        FREE_PAGES => pmm::stats().free,
        FS_READ | FS_WRITE | FS_REMOVE | FS_LIST | FS_READ_AT => FS_NO_VOLUME as u64,
        CONSOLE_SIZE => 0,
        FP_LEVEL => u64::from(fpu::level()),
        AUDIO_FLOAT => 0,
        GFX_INFO | GFX_INFO_ON if number == GFX_INFO || a == 0 => fb::gfx_info().map_or(0, |(w, h)| u64::from(w) << 32 | u64::from(h)),
        GFX_SCREENS => u64::from(fb::gfx_info().is_some()),
        GFX_OPEN | GFX_OPEN_ON if number == GFX_OPEN || a == 0 => {
            let opened = fb::gfx_open();
            if opened {
                process::note_screen_open();
            }
            u64::from(opened)
        }
        GFX_CLOSE => {
            fb::gfx_close();
            0
        }
        GFX_CLOSE_ON if a == 0 => {
            fb::gfx_close();
            0
        }
        GFX_FILL => u64::from(fb::gfx_fill(a as u32, b as u32, c as u32, d as u32, e as u32)),
        GFX_FILL_ON if a == 0 => u64::from(fb::gfx_fill(b as u32, c as u32, d as u32, e as u32, frame.r9 as u32)),
        GFX_BLIT => blit(a as u32, b as u32, c as u32, d as u32, e, frame.r9),
        // The seventh argument (stride) is on the user's stack, above the return address the stub's caller pushed.
        GFX_BLIT_ON if a == 0 => match process::user_u64(frame.rsp + 8) {
            Some(stride) => blit(b as u32, c as u32, d as u32, e as u32, frame.r9, stride),
            None => 0,
        },
        _ => 0,
    };
}

/// `gfx_blit`: `pixels` (rdi-style argument 5) and `stride` (argument 6) are checked as user memory before the copy.
fn blit(x: u32, y: u32, width: u32, height: u32, pixels: u64, stride: u64) -> u64 {
    let (stride, rows) = (stride as u32, u64::from(height));
    if pixels == 0 || stride < width || width == 0 || rows == 0 {
        return 0;
    }
    let bytes = ((rows - 1) * u64::from(stride) + u64::from(width)) * 4;
    if pixels % 4 != 0 || !vm::user_range_ok(sched::current_root(), pixels, bytes, false) {
        return 0;
    }
    u64::from(unsafe { fb::gfx_blit(x, y, width, height, pixels as *const u32, stride) })
}
