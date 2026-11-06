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
//! machine has no such device"; the `fs_*` calls go to `tapefs.rs`, the `nic_*` calls to `tapenet.rs`. Input, files, NICs, USB MIDI/touch and audio (`tapeaudio.rs`) are served; GPIO and I2C answer "no pins" (Pi 4 only) (the drivers are unlocked and CPU-0 only; see docs/big-kernel.md). Processes are single-threaded.

use core::mem::{offset_of, size_of};

use fosfor_abi::tape::{KEY_BASE, KEY_NONE, TAPE_API_VERSION, TapeApi};

use crate::trap::Frame;
use crate::{threads, apic, fb, fpu, pmm, process, sched, tapeaudio, tapefs, tapenet, tapeserial, tapesys, vm};

pub const SHIM_BASE: u64 = vm::USER_BASE + 0x200_0000;
pub const TABLE_BASE: u64 = SHIM_BASE + 0x1000;
pub const ARGS_BASE: u64 = SHIM_BASE + 0x2000;
pub const HEAP_BASE: u64 = vm::USER_BASE + 0x4000_0000;
const HEAP_LIMIT: u64 = HEAP_BASE + 0x100_0000_0000;
/// The one-entry memory map a tape sees (`memory_map`): a single UEFI descriptor, kind 7 (free RAM), as large as what the
/// allocator has free when the process starts. memtest sizes its run from it.
pub const MAP_BASE: u64 = SHIM_BASE + 0x3000;
const MAP_DESCRIPTOR: u64 = 48;
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
    .global tape_shim_start, tape_shim_stubs, tape_shim_end, tape_thread_start
tape_shim_start:                     # entered with rdi = the table, rsi = the tape's entry point
    call rsi
    mov eax, 1                       # the tape returned: exit(0)
    xor edi, edi
    syscall
    ud2
    .balign 16
tape_thread_start:                   # another thread: rdi = its entry point, rsi = its argument (the stack is 16-aligned)
    mov rax, rdi
    mov rdi, rsi
    call rax
    mov eax, 5                       # the entry returned: end this thread only
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
    static tape_thread_start: u8;
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
const HOSTNAME: u64 = slot(offset_of!(TapeApi, hostname));
const PEER_INFO: u64 = slot(offset_of!(TapeApi, peer_info));
const DISK_INFO: u64 = slot(offset_of!(TapeApi, disk_info));
const DISK_FORMAT_EXFAT: u64 = slot(offset_of!(TapeApi, disk_format_exfat));
const WALL_CLOCK: u64 = slot(offset_of!(TapeApi, wall_clock));
const WALL_CLOCK_SET: u64 = slot(offset_of!(TapeApi, wall_clock_set));
const SERIAL_COUNT: u64 = slot(offset_of!(TapeApi, serial_count));
const SERIAL_INFO: u64 = slot(offset_of!(TapeApi, serial_info));
const SERIAL_OPEN: u64 = slot(offset_of!(TapeApi, serial_open));
const SERIAL_CLOSE: u64 = slot(offset_of!(TapeApi, serial_close));
const SERIAL_READ: u64 = slot(offset_of!(TapeApi, serial_read));
const SERIAL_WRITE: u64 = slot(offset_of!(TapeApi, serial_write));
const SERIAL_LINES: u64 = slot(offset_of!(TapeApi, serial_lines));
const SERIAL_SET_LINES: u64 = slot(offset_of!(TapeApi, serial_set_lines));
const SERIAL_BREAK: u64 = slot(offset_of!(TapeApi, serial_break));
const NIC_COUNT: u64 = slot(offset_of!(TapeApi, nic_count));
const NIC_MAC: u64 = slot(offset_of!(TapeApi, nic_mac));
const NIC_SEND: u64 = slot(offset_of!(TapeApi, nic_send));
const NIC_RECEIVE: u64 = slot(offset_of!(TapeApi, nic_receive));
const NIC_MAX_FRAME_LEN: u64 = slot(offset_of!(TapeApi, nic_max_frame_len));
const NIC_SET_PROMISCUOUS: u64 = slot(offset_of!(TapeApi, nic_set_promiscuous));
const NIC_SET_MULTICAST: u64 = slot(offset_of!(TapeApi, nic_set_multicast));
const NIC_ACTIVE: u64 = slot(offset_of!(TapeApi, nic_active));
const NIC_OWNED: u64 = slot(offset_of!(TapeApi, nic_owned));
const NIC_TS_ENABLE: u64 = slot(offset_of!(TapeApi, nic_ts_enable));
const NIC_TS_CLOCK: u64 = slot(offset_of!(TapeApi, nic_ts_clock));
const NIC_TS_RX: u64 = slot(offset_of!(TapeApi, nic_ts_rx));
const NIC_SEND_STAMPED: u64 = slot(offset_of!(TapeApi, nic_send_stamped));
const NIC_TS_ADJUST: u64 = slot(offset_of!(TapeApi, nic_ts_adjust));
const NIC_TS_STEP: u64 = slot(offset_of!(TapeApi, nic_ts_step));
const NIC_TS_ALL: u64 = slot(offset_of!(TapeApi, nic_ts_all));
const NIC_MULTICAST_JOIN: u64 = slot(offset_of!(TapeApi, nic_multicast_join));
const NIC_VLAN_FILTER: u64 = slot(offset_of!(TapeApi, nic_vlan_filter));
const NIC_TX_OFFLOAD: u64 = slot(offset_of!(TapeApi, nic_tx_offload));
const NIC_COUNTERS: u64 = slot(offset_of!(TapeApi, nic_counters));
const GPIO_PINS: u64 = slot(offset_of!(TapeApi, gpio_pins));
const GPIO_READ: u64 = slot(offset_of!(TapeApi, gpio_read));
const GPIO_WRITE: u64 = slot(offset_of!(TapeApi, gpio_write));
const GPIO_MODE: u64 = slot(offset_of!(TapeApi, gpio_mode));
const GPIO_PULL: u64 = slot(offset_of!(TapeApi, gpio_pull));
const GPIO_FUNCTION: u64 = slot(offset_of!(TapeApi, gpio_function));
const GPIO_PULL_GET: u64 = slot(offset_of!(TapeApi, gpio_pull_get));
const I2C_SPEED: u64 = slot(offset_of!(TapeApi, i2c_speed));
const I2C_TRANSFER: u64 = slot(offset_of!(TapeApi, i2c_transfer));
const TOUCH_INFO: u64 = slot(offset_of!(TapeApi, touch_info));
const TOUCH_READ: u64 = slot(offset_of!(TapeApi, touch_read));
const MIDI_READ: u64 = slot(offset_of!(TapeApi, midi_read));
const MIDI_SEND: u64 = slot(offset_of!(TapeApi, midi_send));
const MIDI_DEVICES: u64 = slot(offset_of!(TapeApi, midi_devices));
const AUDIO_INFO: u64 = slot(offset_of!(TapeApi, audio_info));
const AUDIO_START: u64 = slot(offset_of!(TapeApi, audio_start));
const AUDIO_STOP: u64 = slot(offset_of!(TapeApi, audio_stop));
const AUDIO_READ: u64 = slot(offset_of!(TapeApi, audio_read));
const AUDIO_WRITE: u64 = slot(offset_of!(TapeApi, audio_write));
const AUDIO_STATS: u64 = slot(offset_of!(TapeApi, audio_stats));
const AUDIO_SELECT: u64 = slot(offset_of!(TapeApi, audio_select));
const AUDIO_BUFFER: u64 = slot(offset_of!(TapeApi, audio_buffer));
const AUDIO_SET_CAPTURE_RING: u64 = slot(offset_of!(TapeApi, audio_set_capture_ring));
const AUDIO_DEVICES: u64 = slot(offset_of!(TapeApi, audio_devices));
const AUDIO_USE_DEVICE: u64 = slot(offset_of!(TapeApi, audio_use_device));
const AUDIO_USE_OUTPUT: u64 = slot(offset_of!(TapeApi, audio_use_output));
const AUDIO_FLOAT: u64 = slot(offset_of!(TapeApi, audio_float));
const FS_STREAM_OPEN: u64 = slot(offset_of!(TapeApi, fs_stream_open));
const FS_STREAM_WRITE: u64 = slot(offset_of!(TapeApi, fs_stream_write));
const FS_STREAM_CLOSE: u64 = slot(offset_of!(TapeApi, fs_stream_close));
const FS_STREAM_OPEN2: u64 = slot(offset_of!(TapeApi, fs_stream_open2));
const FS_STREAM_WRITE2: u64 = slot(offset_of!(TapeApi, fs_stream_write2));
const FS_STREAM_CLOSE2: u64 = slot(offset_of!(TapeApi, fs_stream_close2));
const PHYS_ADDR: u64 = slot(offset_of!(TapeApi, phys_addr));
const VFS_STAT: u64 = slot(offset_of!(TapeApi, vfs_stat));
const VFS_READ_DIR: u64 = slot(offset_of!(TapeApi, vfs_read_dir));
const VFS_READ: u64 = slot(offset_of!(TapeApi, vfs_read));
const RANDOM: u64 = slot(offset_of!(TapeApi, random));
const VFS_CREATE: u64 = slot(offset_of!(TapeApi, vfs_create));
const VFS_WRITE: u64 = slot(offset_of!(TapeApi, vfs_write));
const VFS_TRUNCATE: u64 = slot(offset_of!(TapeApi, vfs_truncate));
const VFS_REMOVE: u64 = slot(offset_of!(TapeApi, vfs_remove));
const VFS_RENAME: u64 = slot(offset_of!(TapeApi, vfs_rename));
const VFS_SET_OWNER: u64 = slot(offset_of!(TapeApi, vfs_set_owner));
const VFS_SYNC: u64 = slot(offset_of!(TapeApi, vfs_sync));
const THREAD_SPAWN: u64 = slot(offset_of!(TapeApi, thread_spawn));
const THREAD_JOIN: u64 = slot(offset_of!(TapeApi, thread_join));
const FUTEX_WAIT: u64 = slot(offset_of!(TapeApi, futex_wait));
const FUTEX_WAKE: u64 = slot(offset_of!(TapeApi, futex_wake));
const SESSION_OPEN: u64 = slot(offset_of!(TapeApi, session_open));
const SESSION_WRITE: u64 = slot(offset_of!(TapeApi, session_write));
const SESSION_READ: u64 = slot(offset_of!(TapeApi, session_read));
const SESSION_CLOSE: u64 = slot(offset_of!(TapeApi, session_close));
const REMOTE_REGISTER: u64 = slot(offset_of!(TapeApi, remote_register));
const REMOTE_UNREGISTER: u64 = slot(offset_of!(TapeApi, remote_unregister));
const REMOTE_POLL: u64 = slot(offset_of!(TapeApi, remote_poll));
const REMOTE_REPLY: u64 = slot(offset_of!(TapeApi, remote_reply));
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

/// The table slots that hold data, not a function.
fn is_data(slot: u64) -> bool {
    [ARGS, ARGS_LEN, MEMORY_MAP, MEMORY_MAP_COUNT, MEMORY_MAP_DESCRIPTOR_SIZE].contains(&slot)
}

/// Where a new thread starts: the shim's `tape_thread_start`, mapped at the same place in every process.
pub fn thread_start() -> u64 {
    SHIM_BASE + unsafe { &tape_thread_start as *const u8 as usize - &tape_shim_start as *const u8 as usize } as u64 // kernel-memory
}

/// Where a process's code stub for table slot `slot` lives.
fn stub_address(slot: u64) -> u64 {
    let offset = unsafe { &tape_shim_stubs as *const u8 as usize - &tape_shim_start as *const u8 as usize }; // kernel-memory
    SHIM_BASE + offset as u64 + 16 * slot
}

/// Maps the shim, the table and the args page into `root`. `Some(())` on success; the frames belong to the address space.
pub fn map_support(root: u64, args: &[u8]) -> Option<()> {
    let shim = unsafe { core::slice::from_raw_parts(&tape_shim_start as *const u8, &tape_shim_end as *const u8 as usize - &tape_shim_start as *const u8 as usize) }; // kernel-memory
    let code = pmm::allocate(1, None)?;
    unsafe { core::ptr::copy_nonoverlapping(shim.as_ptr(), vm::direct(code) as *mut u8, shim.len()) }; // kernel-memory
    vm::map_user_page(root, SHIM_BASE, code, false, true).then_some(())?;

    let table = pmm::allocate(1, None)?;
    let words = vm::direct(table) as *mut u64; // kernel-memory
    unsafe {
        words.write(u64::from(TAPE_API_VERSION) | (size_of::<TapeApi>() as u64) << 32);
        for index in 1..SLOTS as u64 {
            let value = match index {
                ARGS => ARGS_BASE,
                ARGS_LEN => args.len().min(ARGS_MAX) as u64,
                MEMORY_MAP => MAP_BASE,
                MEMORY_MAP_COUNT => 1,
                MEMORY_MAP_DESCRIPTOR_SIZE => MAP_DESCRIPTOR,
                _ => stub_address(index),
            };
            words.add(index as usize).write(value);
        }
    }
    vm::map_user_page(root, TABLE_BASE, table, false, false).then_some(())?;

    let map = pmm::allocate(1, None)?;
    unsafe {
        let descriptor = vm::direct(map) as *mut u8; // kernel-memory
        core::ptr::write_bytes(descriptor, 0, 4096); // kernel-memory
        (descriptor as *mut u32).write(7); // kernel-memory
        (descriptor.add(24) as *mut u64).write(pmm::stats().free); // kernel-memory
    }
    vm::map_user_page(root, MAP_BASE, map, false, false).then_some(())?;

    let page = pmm::allocate(1, None)?;
    let length = args.len().min(ARGS_MAX);
    unsafe { core::ptr::copy_nonoverlapping(args.as_ptr(), vm::direct(page) as *mut u8, length) }; // kernel-memory
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
    let _map = threads::MAP_LOCK.lock();
    let start = match sched::heap_next() {
        0 => HEAP_BASE,
        next => next,
    };
    // `count * 4096` wraps for a size near u64::MAX, which would pass the limit and then allocate until memory runs out.
    if count == 0 || count.checked_mul(4096).and_then(|bytes| bytes.checked_add(start)).is_none_or(|end| end > HEAP_LIMIT) {
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
    let mut frames = alloc::vec::Vec::new();
    {
        let _map = threads::MAP_LOCK.lock();
        for page in 0..pages(bytes) {
            if let Some(frame) = vm::unmap_user_page(root, address + page * 4096) {
                frames.push(frame);
            }
        }
    }
    // Another thread of the process may run on another CPU and still cache the mappings: flush them there before the frames are reused.
    if sched::process_threads() > 1 {
        crate::tlb::shootdown(address, pages(bytes));
    }
    for frame in frames {
        pmm::free(frame, 1);
    }
}

/// One table call from ring 3: `slot` is the stub's number minus `CALL_BASE`, the arguments are as System V passes them.
pub fn call(frame: &mut Frame) {
    let number = frame.rax - CALL_BASE;
    let (a, b, c, d, e) = (frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8);
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
        READ_KEY => match crate::shell::take_key(sched::current_pid()) {
            Some(byte) => u64::from(KEY_BASE + u32::from(byte)),
            None => u64::from(KEY_NONE),
        },
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
        NIC_COUNT => tapenet::count(),
        NIC_MAC => tapenet::mac(a, b),
        NIC_SEND => tapenet::send(a, b, c),
        NIC_RECEIVE => tapenet::receive(a, b, c),
        NIC_MAX_FRAME_LEN => tapenet::MAX_FRAME,
        NIC_ACTIVE => tapenet::active(),
        NIC_OWNED => tapenet::owned(a),
        NIC_SET_PROMISCUOUS => tapenet::setting(a, b, kernel_core::nicctl::nic_set_promiscuous),
        NIC_SET_MULTICAST => tapenet::setting(a, b, kernel_core::nicctl::nic_set_multicast),
        NIC_TS_ENABLE => tapenet::setting(a, 1, |index, _| kernel_core::nicctl::nic_ts_enable(index)),
        NIC_TS_ALL => tapenet::setting(a, b, kernel_core::nicctl::nic_ts_all),
        NIC_TX_OFFLOAD => tapenet::setting(a, b, kernel_core::nicctl::nic_tx_offload),
        NIC_TS_CLOCK => tapenet::ts_clock(a),
        NIC_TS_RX => tapenet::ts_rx(a),
        NIC_TS_ADJUST => tapenet::ts_adjust(a, b),
        NIC_TS_STEP => tapenet::ts_step(a, b),
        NIC_SEND_STAMPED => tapenet::send_stamped(a, b, c, d),
        NIC_MULTICAST_JOIN => tapenet::multicast_join(a, b, c),
        NIC_VLAN_FILTER => tapenet::vlan_filter(a, b, c),
        NIC_COUNTERS => tapenet::counters(a, b, c),
        TOUCH_INFO => crate::usb::touch_info(a),
        TOUCH_READ => crate::usb::touch_read(a) as u64,
        MIDI_READ => crate::usb::midi_read(a) as u64,
        MIDI_SEND => crate::usb::midi_send(a, b, c) as u64,
        MIDI_DEVICES => crate::usb::midi_devices(),
        AUDIO_INFO => tapeaudio::info(a),
        AUDIO_START => tapeaudio::start(a, b, c) as u64,
        AUDIO_STOP => {
            tapeaudio::stop();
            0
        }
        AUDIO_READ => tapeaudio::read(a, b),
        AUDIO_WRITE => tapeaudio::write(a, b),
        AUDIO_STATS => {
            tapeaudio::stats(a);
            0
        }
        AUDIO_SELECT => tapeaudio::select(a, b) as u64,
        AUDIO_BUFFER => {
            tapeaudio::buffer(a);
            0
        }
        AUDIO_SET_CAPTURE_RING => {
            tapeaudio::set_capture_ring(a);
            0
        }
        AUDIO_DEVICES => tapeaudio::devices(a, b),
        AUDIO_USE_DEVICE => tapeaudio::use_device(a) as u64,
        AUDIO_USE_OUTPUT => tapeaudio::use_output(a) as u64,
        AUDIO_FLOAT => tapeaudio::float(),
        FS_READ => tapefs::read(a, b, c, d) as u64,
        VFS_STAT => tapefs::vfs_stat(a, b, c) as u64,
        VFS_READ_DIR => tapefs::vfs_read_dir(a, b, c, d) as u64,
        VFS_READ => tapefs::vfs_read(a, b, c, d, e) as u64,
        VFS_CREATE => tapefs::vfs_create(a, b, c) as u64,
        VFS_WRITE => tapefs::vfs_write(a, b, c, d, e) as u64,
        VFS_TRUNCATE => tapefs::vfs_truncate(a, b, c) as u64,
        VFS_REMOVE => tapefs::vfs_remove(a, b) as u64,
        VFS_RENAME => tapefs::vfs_rename(a, b, c, d) as u64,
        VFS_SET_OWNER => tapefs::vfs_set_owner(a, b, c, d, e) as u64,
        VFS_SYNC => tapefs::vfs_sync() as u64,
        THREAD_SPAWN => threads::spawn(a, b, c) as u64,
        THREAD_JOIN => threads::join(a) as u64,
        FUTEX_WAIT => threads::futex_wait(a, b) as u64,
        FUTEX_WAKE => threads::futex_wake(a, b) as u64,
        SESSION_OPEN => crate::net::tape_session_open(sched::current_pid()) as u64,
        SESSION_WRITE => match user_bytes(b, c.min(4096)) {
            Some(bytes) => crate::net::tape_session_write(sched::current_pid(), a, &bytes) as u64,
            None => (-4i64) as u64,
        },
        SESSION_READ => {
            let mut bytes = alloc::vec![0u8; c.min(4096) as usize];
            let got = crate::net::tape_session_read(sched::current_pid(), a, &mut bytes);
            if got > 0 && !process::copy_to_user(b, &bytes[..got as usize]) {
                (-4i64) as u64
            } else {
                got as u64
            }
        }
        SESSION_CLOSE => crate::net::tape_session_close(sched::current_pid(), a) as u64,
        REMOTE_REGISTER => tapefs::remote_register(a, b) as u64,
        REMOTE_UNREGISTER => tapefs::remote_unregister(a, b) as u64,
        REMOTE_POLL => tapefs::remote_poll(a, b, c, d, e) as u64,
        REMOTE_REPLY => tapefs::remote_reply(a, b, c, d, e) as u64,
        RANDOM => {
            // Filled in kernel memory and then copied: a direct write into user pages faults under SMAP.
            if b > 4096 {
                return frame.rax = 0;
            }
            let mut bytes = alloc::vec![0u8; b as usize];
            let strong = kernel_core::random::fill(&mut bytes);
            u64::from(process::copy_to_user(a, &bytes) && strong)
        }
        FS_READ_AT => tapefs::read_range(a, b, c, d, e) as u64,
        FS_WRITE => tapefs::write(a, b, c, d) as u64,
        FS_REMOVE => tapefs::remove(a, b) as u64,
        FS_LIST => tapefs::list(a, b, c, d) as u64,
        FS_STREAM_OPEN => tapefs::stream_open(a, b, c) as u64,
        FS_STREAM_WRITE => tapefs::stream_write(a, b, c, d) as u64,
        FS_STREAM_CLOSE => tapefs::stream_close(a) as u64,
        FS_STREAM_OPEN2 => tapefs::stream_open2(a, b, c, d) as u64,
        FS_STREAM_WRITE2 => tapefs::stream_write2(a, b, c, d, e) as u64,
        PHYS_ADDR => vm::user_physical(sched::current_root(), a).unwrap_or(0),
        FS_STREAM_CLOSE2 => tapefs::stream_close2(a, b) as u64,
        SERIAL_COUNT => tapeserial::count(),
        SERIAL_INFO => tapeserial::info(a, b),
        SERIAL_OPEN => tapeserial::open(a, b, c) as u64,
        SERIAL_CLOSE => {
            tapeserial::close(a);
            0
        }
        SERIAL_READ => tapeserial::read(a, b, c),
        SERIAL_WRITE => tapeserial::write(a, b, c),
        SERIAL_LINES => tapeserial::lines(a),
        SERIAL_SET_LINES => {
            tapeserial::set_lines(a, b);
            0
        }
        SERIAL_BREAK => {
            tapeserial::set_break(a, b);
            0
        }
        CONSOLE_SIZE => tapesys::console_size(),
        HOSTNAME => tapesys::hostname(a, b, c),
        PEER_INFO => tapesys::peer_info(a, b),
        DISK_INFO => tapesys::disk_info(a, b),
        DISK_FORMAT_EXFAT => tapesys::format_exfat(a, b, c) as u64,
        WALL_CLOCK => tapesys::wall_clock(a),
        WALL_CLOCK_SET => tapesys::wall_clock_set(a),
        // This machine has no GPIO pins or I2C header (the Raspberry Pi 4 has): the answers of the embedded x86 kernel without
        // pins, so a tape can tell. Reads show nothing, every write is refused, mode/pull say "no GPIO", I2C says "no bus".
        GPIO_PINS | GPIO_READ => 0,
        GPIO_WRITE => a,
        GPIO_MODE | GPIO_PULL => (-3i64) as u64,
        GPIO_FUNCTION | GPIO_PULL_GET => (-2i64) as u64,
        I2C_SPEED | I2C_TRANSFER => (-4i64) as u64,
        FP_LEVEL => u64::from(fpu::level()),
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
    u64::from(process::user_access(|| unsafe { fb::gfx_blit(x, y, width, height, pixels as *const u32, stride) })) // kernel-memory
}
