//! A "paper tape" that prints directly to VGA text memory instead of
//! through `console::print_byte`, demonstrating what bypassing this
//! project's usual console abstraction actually looks like -- see
//! AGENTS.md's "vga-colors tape" section for the full story of why this
//! tape exists and what its two-callback ABI (`putc`, `should_abort`,
//! both described in `shell::tape_run`'s doc comment) makes possible.
//!
//! Unlike `primes.S`/`hello-rs` (which run once, print a summary through
//! `putc`, and return), this tape loops indefinitely, redrawing several
//! rows of text in cycling VGA colors, until a local operator presses one
//! of the "give up" keys (F12/F4/F6/F7/F8/F10) -- checked once per frame
//! via the `should_abort` callback. It writes those colored rows straight
//! to VGA text memory (physical `0xb8000`), never through `putc`, so the
//! animation itself never reaches serial or a `term net` relay at all --
//! only one line at the very start and one at the very end go through
//! `putc`, so a remote operator at least sees that the tape started and
//! stopped, without every single frame of the animation flooding their
//! session.
#![no_std]
#![no_main]

core::arch::global_asm!(include_str!("entry.S"));

use core::panic::PanicInfo;

/// Physical address of the standard 80x25 VGA text-mode framebuffer --
/// the exact same address `kernel/src/console.rs`'s own `VGA_BUFFER`
/// points at. Accessed directly here instead of through `console::
/// print_byte`, which is the whole point of this example (see this
/// file's own header comment).
const VGA_BUFFER: *mut u16 = 0xb8000 as *mut u16;
const VGA_WIDTH: usize = 80;
const VGA_HEIGHT: usize = 25;

/// The 15 standard VGA text-mode foreground colors this tape cycles
/// through (background left black, bits 4-6 zero). Color 0 (black) is
/// deliberately excluded -- black text on a black background would just
/// be invisible.
const COLORS: [u8; 15] = [
    0x1, 0x2, 0x3, 0x4, 0x5, 0x6, 0x7, 0x8, 0x9, 0xa, 0xb, 0xc, 0xd, 0xe, 0xf,
];

/// The rows this tape animates on, and the fixed text drawn on each --
/// deliberately below row 3, so it never overlaps whatever the shell had
/// already printed above before `tape run` was typed (see this file's
/// header comment on why the kernel's own cursor position is left alone
/// afterward).
const ROWS: [&[u8]; 15] = [
    b" VGA color demo -- row  0 -- direct writes to physical 0xb8000        ",
    b" VGA color demo -- row  1 -- bypassing console::print_byte entirely  ",
    b" VGA color demo -- row  2 -- no serial or term net relay traffic     ",
    b" VGA color demo -- row  3 -- just this tape and the VGA controller   ",
    b" VGA color demo -- row  4 -- 15 standard text-mode foreground colors ",
    b" VGA color demo -- row  5 -- cycling once per animation frame        ",
    b" VGA color demo -- row  6 -- press F12 (or F4/F6/F7/F8/F10) to stop  ",
    b" VGA color demo -- row  7 -- local escape only -- no keyboard needed ",
    b" VGA color demo -- row  8 -- on the remote end of a term net session ",
    b" VGA color demo -- row  9 -- to abort this tape's own loop           ",
    b" VGA color demo -- row 10 -- without ever disconnecting that session ",
    b" VGA color demo -- row 11 -- see keyboard::tape_should_abort's own   ",
    b" VGA color demo -- row 12 -- doc comment for exactly why that holds  ",
    b" VGA color demo -- row 13 -- fos: a small educational 32-bit x86 OS  ",
    b" VGA color demo -- row 14 -- tools/tapes/vga-colors/src/main.rs      ",
];

fn print_bytes(putc: extern "C" fn(u32), bytes: &[u8]) {
    for &byte in bytes {
        putc(u32::from(byte));
    }
}

/// Writes one character cell directly to VGA text memory at (`row`,
/// `col`), bypassing `console.rs` entirely -- no serial mirroring, no
/// `term net` relay, and no hardware cursor movement (this tape never
/// touches the CRTC cursor ports at all, so wherever the kernel's own
/// cached cursor position was before `tape run` remains exactly where it
/// was after -- see `shell::tape_run`'s doc comment). `unsafe` for the
/// same "raw pointer into memory-mapped video RAM" reason every other
/// direct VGA write in this project already documents.
fn put_cell(row: usize, col: usize, color: u8, byte: u8) {
    if row >= VGA_HEIGHT || col >= VGA_WIDTH {
        return;
    }
    unsafe {
        VGA_BUFFER
            .add(row * VGA_WIDTH + col)
            .write_volatile(u16::from(color) << 8 | u16::from(byte));
    }
}

fn put_row(row: usize, color: u8, text: &[u8]) {
    for (col, &byte) in text.iter().enumerate().take(VGA_WIDTH) {
        put_cell(row, col, color, byte);
    }
}

/// A crude, uncalibrated busy-wait between animation frames -- this
/// tape's ABI has no timer callback (only `putc`/`should_abort`, see
/// `shell::tape_run`'s doc comment), so this exists only to make the
/// color cycling visible to a human eye, not to be an accurate delay
/// (the same "uncalibrated" caveat `primes.S`'s own TSC-based elapsed-
/// cycles readout already documents for the same underlying reason: no
/// calibrated timer is available to a tape).
fn spin_one_frame() {
    for _ in 0..2_000_000 {
        core::hint::spin_loop();
    }
}

/// The tape's entry point: called as `extern "C" fn(putc: extern "C"
/// fn(u32), should_abort: extern "C" fn() -> u32)`, the two-callback ABI
/// every tape may now use (see `shell::tape_run`'s doc comment). Named
/// `tape_main`, not `_start`, because `src/entry.S`'s `_start` is what
/// the linker script actually places first -- it zeroes `.bss` before
/// tail-calling here.
#[unsafe(no_mangle)]
pub extern "C" fn tape_main(putc: extern "C" fn(u32), should_abort: extern "C" fn() -> u32) {
    print_bytes(
        putc,
        b"Running the VGA color demo tape; F12 (or F4/F6/F7/F8/F10) to stop -- \
watch the screen, not this terminal.\n",
    );

    let mut offset: usize = 0;
    loop {
        for (row, text) in ROWS.iter().enumerate() {
            let color = COLORS[(row + offset) % COLORS.len()];
            put_row(row, color, text);
        }
        offset = (offset + 1) % COLORS.len();

        spin_one_frame();
        if should_abort() != 0 {
            break;
        }
    }

    print_bytes(putc, b"VGA color demo stopped.\n");
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    // A tape has nowhere to unwind to and no OS underneath it -- see
    // `panic = "abort"` in Cargo.toml -- so this can only spin forever,
    // exactly like the kernel's own panic handler in kernel/src/main.rs
    // (a tape faulting or hanging is documented as unrecoverable; see
    // `tape_run`'s doc comment).
    loop {}
}
