//! A small shared runtime for ordinary 32-bit Rust tapes -- the bits every
//! such tape needs (`_start`, a panic handler, `putc`-based print
//! helpers) but that have nothing to do with any individual tape's own
//! purpose. `tapes/vga-colors` depends on this crate instead of keeping
//! its own copies of these; see below for why it, specifically, was
//! chosen for this and why `tapes/hello-rs`/`tapes/bigsieve` were
//! deliberately left as they were.
//!
//! This mirrors, for ordinary 32-bit tapes, the same "reusable plumbing
//! in one place, swappable per-tape logic elsewhere" split that
//! `tapes/longmode64_shim.S` uses for 64-bit-excursion tapes (see
//! AGENTS.md's "A tape that visits 64-bit long mode and comes back"):
//! there, the shim owns the CPU-mode-switch mechanics and calls out to an
//! external `tape64_body`; here, this crate owns the startup/panic/print
//! mechanics and a tape's own `src/main.rs` supplies `tape_main`. Same
//! idea, one exists in assembly for a privileged mode transition, the
//! other in Rust for ordinary boilerplate.
//!
//! Why only `vga-colors` uses this, and `hello-rs`/`bigsieve` still keep
//! their own local, duplicated copies of `entry.S`/the panic handler/
//! `print_bytes`/`print_u32`: this project's Rust tapes exist primarily
//! as *teaching examples* (see `docs/rust-tapes-for-dummies.md`), and a
//! beginner reading `tapes/hello-rs/src/main.rs` top to bottom is meant
//! to see the *entire* tape -- every line that runs, with nothing
//! imported from an unfamiliar sibling crate -- in one file. Introducing
//! a shared dependency there would trade that self-containedness for a
//! small amount of saved duplication, a bad trade for a from-scratch
//! walkthrough's very first and second examples. `vga-colors` was
//! refactored to use this crate instead specifically to demonstrate the
//! *other* valid pattern -- extracting shared plumbing once duplication
//! starts to feel real -- so a reader curious about it has a working
//! example to study, without disturbing the two simpler ones. Any
//! genuinely new future tape (not a teaching-ladder example) should
//! prefer depending on this crate the way `vga-colors` does, rather than
//! copying `entry.S`/the panic handler/the print helpers a fourth time.
#![no_std]

use core::panic::PanicInfo;

core::arch::global_asm!(include_str!("entry.S"));

/// Prints one byte through the callback the kernel passed in -- see
/// `console::print_byte` in kernel/src/console.rs, the only thing on the
/// other end of this function pointer.
pub fn print_bytes(putc: extern "C" fn(u32), bytes: &[u8]) {
    for &byte in bytes {
        putc(u32::from(byte));
    }
}

/// Prints an unsigned integer in decimal, most significant digit first,
/// without `core::fmt`/allocation -- just a fixed stack buffer, the same
/// spirit as `console.rs`'s own number formatting. Not currently used by
/// `vga-colors` itself, but kept here (matching `hello-rs`/`bigsieve`'s
/// own copies) as a ready-made utility for whatever future tape depends
/// on this crate next.
pub fn print_u32(putc: extern "C" fn(u32), mut value: u32) {
    let mut digits = [0u8; 10];
    let mut count = 0;

    if value == 0 {
        print_bytes(putc, b"0");
        return;
    }

    while value > 0 {
        digits[count] = b'0' + (value % 10) as u8;
        value /= 10;
        count += 1;
    }

    for &digit in digits[..count].iter().rev() {
        putc(u32::from(digit));
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    // A tape has nowhere to unwind to and no OS underneath it -- see
    // `panic = "abort"` in the dependent tape's Cargo.toml -- so this can
    // only spin forever, exactly like the kernel's own panic handler in
    // kernel/src/main.rs (a tape faulting or hanging is documented as
    // unrecoverable; see `tape_run`'s doc comment). Living here instead
    // of in each tape's own `main.rs` is safe because a panic handler is
    // a single global lang item: exactly one may exist anywhere in a
    // binary's whole dependency graph, so any tape depending on this
    // crate must not (and does not) define its own.
    loop {}
}
