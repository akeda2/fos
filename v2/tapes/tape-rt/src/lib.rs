//! The runtime every v2 tape shares: `_start`, a panic handler, `print!`/`println!`
//! over the kernel's `putc`, and a few helpers for the callbacks in `TapeApi`.
//!
//! A tape's `main.rs` is `#![no_std] #![no_main]` and defines
//! `#[unsafe(no_mangle)] pub extern "C" fn tape_main(api: &'static TapeApi)`; this crate
//! supplies the rest. The API pointer is also stored globally (`api()`), so helper
//! functions deep in a tape can print or poll without it being threaded through.
#![no_std]

pub use fos_v2_abi::tape::*;

use core::fmt::Write;
use fos_v2_abi::tape::FS_NOT_FOUND;

core::arch::global_asm!(
    r#"
    .section .text.entry, "ax"
    .global _start
    _start:
        // rdi = &TapeApi, the stack is the tape's own (already 16-byte aligned at the call).
        push rbp                 // re-align for the call below
        mov rbp, rsp
        call tape_main
        pop rbp
        ret
    "#
);

static mut API: *const TapeApi = core::ptr::null();

/// Called first thing by `tape_main`.
pub fn init(api: &'static TapeApi) {
    unsafe { API = api };
}

pub fn api() -> &'static TapeApi {
    unsafe { &*API }
}

/// The text after `tape run`.
pub fn args() -> &'static [u8] {
    let api = api();
    unsafe { core::slice::from_raw_parts(api.args, api.args_len) }
}

pub struct Console;

impl Write for Console {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        for &byte in text.as_bytes() {
            (api().putc)(u32::from(byte));
        }
        Ok(())
    }
}

#[macro_export]
macro_rules! print {
    ($($argument:tt)*) => {{ let _ = core::fmt::Write::write_fmt(&mut $crate::Console, format_args!($($argument)*)); }};
}

#[macro_export]
macro_rules! println {
    () => { $crate::print!("\n") };
    ($($argument:tt)*) => {{ $crate::print!($($argument)*); $crate::print!("\n"); }};
}

/// Prints raw bytes through a `putc` callback (kept for tapes ported from v1, which pass
/// the callback around explicitly).
pub fn print_bytes(putc: extern "C" fn(u32), bytes: &[u8]) {
    for &byte in bytes {
        putc(u32::from(byte));
    }
}

/// Prints an unsigned integer in decimal through a `putc` callback.
pub fn print_u32(putc: extern "C" fn(u32), mut value: u32) {
    let mut digits = [0u8; 10];
    let mut count = 0;
    loop {
        digits[count] = b'0' + (value % 10) as u8;
        count += 1;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    for &digit in digits[..count].iter().rev() {
        putc(u32::from(digit));
    }
}

/// The v1 tape ABI was a long list of cdecl parameters with 32-bit addresses; tapes ported from
/// it keep their bodies and use these functions, which have the v1 names and shapes but take
/// 64-bit pointers (as `usize`) and go through the v2 table. Not for new tapes: use `api()`.
pub mod compat {
    use super::api;

    pub fn should_abort() -> u32 {
        (api().should_abort)()
    }
    pub fn read_key() -> u32 {
        (api().read_key)()
    }
    pub fn wait_for_event() {
        (api().wait_for_event)();
    }
    pub fn ticks() -> u64 {
        (api().ticks)()
    }
    pub fn micros() -> u64 {
        (api().micros)()
    }
    pub fn alloc(bytes: u32) -> usize {
        (api().alloc)(bytes as usize)
    }
    pub fn free(address: usize, bytes: u32) {
        (api().free)(address, bytes as usize)
    }
    pub fn nic_count() -> u32 {
        (api().nic_count)() as u32
    }
    /// The "active" interface of v1 is interface 0 here.
    pub fn nic_mac(out: usize) -> u32 {
        (api().nic_mac)(0, out as *mut u8)
    }
    pub fn nic_send(frame: usize, length: u32) -> u32 {
        (api().nic_send)(0, frame as *const u8, length as usize)
    }
    pub fn nic_receive(buffer: usize, capacity: u32) -> u32 {
        (api().nic_receive)(0, buffer as *mut u8, capacity as usize) as u32
    }
    pub fn nic_mac_at(nic: u32, out: usize) -> u32 {
        (api().nic_mac)(nic as usize, out as *mut u8)
    }
    pub fn nic_send_at(nic: u32, frame: usize, length: u32) -> u32 {
        (api().nic_send)(nic as usize, frame as *const u8, length as usize)
    }
    pub fn nic_receive_at(nic: u32, buffer: usize, capacity: u32) -> u32 {
        (api().nic_receive)(nic as usize, buffer as *mut u8, capacity as usize) as u32
    }
    pub fn nic_set_promiscuous_at(nic: u32, on: u32) -> u32 {
        (api().nic_set_promiscuous)(nic as usize, on)
    }
    pub fn nic_max_frame_len() -> u32 {
        (api().nic_max_frame_len)() as u32
    }
    pub fn set_input_mode(mode: u32) {
        (api().set_input_mode)(mode)
    }

    /// v1 `fs_write(name, name_len, data, len)`: 0 on success.
    pub fn fs_write(name: usize, name_length: u32, data: usize, length: u32) -> u32 {
        u32::from((api().fs_write)(name as *const u8, name_length as usize, data as *const u8, length as usize) != 0)
    }
    /// v1 `fs_file_length(name, name_len, out_u64)`: status 0 ok, 1 not found (the lease-file
    /// statuses router uses), 2 other failure.
    pub fn fs_file_length(name: usize, name_length: u32, out: usize) -> u32 {
        // v2 has no length call: read into a scratch region to learn it (lease files are small).
        let capacity = 16 * 1024;
        let scratch = (api().alloc)(capacity);
        if scratch == 0 {
            return 2;
        }
        let result = (api().fs_read)(name as *const u8, name_length as usize, scratch as *mut u8, capacity);
        (api().free)(scratch, capacity);
        match result {
            length if length >= 0 => {
                unsafe { (out as *mut u64).write_unaligned(length as u64) };
                0
            }
            FS_NOT_FOUND => 1,
            _ => 2,
        }
    }
    /// v1 `fs_read(name, name_len, buffer, capacity, out_len_u32)`: status 0 ok, 1 not found.
    pub fn fs_read(name: usize, name_length: u32, buffer: usize, capacity: u32, out_length: usize) -> u32 {
        match (api().fs_read)(name as *const u8, name_length as usize, buffer as *mut u8, capacity as usize) {
            length if length >= 0 => {
                unsafe { (out_length as *mut u32).write_unaligned(length as u32) };
                0
            }
            FS_NOT_FOUND => 1,
            _ => 2,
        }
    }
    pub fn fs_remove(name: usize, name_length: u32) -> u32 {
        u32::from((api().fs_remove)(name as *const u8, name_length as usize) != 0)
    }
    pub fn terminal_position() -> u32 {
        0
    }

    // v1 tapes registered hooks with tape-rt; the v2 table makes that unnecessary.
    pub fn set_wait_hook(_hook: extern "C" fn()) {}
    pub fn set_micros_hook(_hook: extern "C" fn() -> u64) {}
}

/// Sleeps until the next interrupt; call it when a polling pass found nothing to do.
pub fn idle() {
    (api().wait_for_event)();
}

pub fn ticks() -> u64 {
    (api().ticks)()
}

pub fn micros() -> u64 {
    (api().micros)()
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    // A tape has nowhere to unwind to: say what happened, then wait for the operator to
    // abort (the kernel's Escape handling is what ends a tape that will not return).
    println!("\ntape panic: {info}");
    loop {
        if (api().should_abort)() != 0 {
            // Cannot return from a panic; park. (The operator resets, or the kernel's
            // watchdog does, from M6.)
            idle();
        }
        idle();
    }
}
