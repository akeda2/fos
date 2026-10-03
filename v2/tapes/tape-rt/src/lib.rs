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
