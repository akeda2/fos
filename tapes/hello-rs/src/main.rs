//! A minimal example "tape" written in Rust instead of assembly, proving
//! the fixed tape-load address (see linker.ld/pmem::TAPE_BASE) is enough
//! to make an ordinary, non-position-independent compiled language work
//! here, the same way it lets the kernel itself use plain addressing. See
//! AGENTS.md's "Rust tapes" section for the full story and
//! `tapes/primes.S`'s header comment for how a hand-written
//! position-independent assembly tape had to work before this.
//!
//! Deliberately much simpler than `primes.S`: it prints a greeting, then
//! counts primes up to a small bound with the same Sieve of Eratosthenes
//! idea (proving real computation runs, not just a string literal), and
//! returns. It uses no heap and no OS services beyond the one `putc`
//! callback `tape_run` passes in -- the same narrow convention every tape
//! must follow (see "tape print callback ABI" in AGENTS.md).
#![no_std]
#![no_main]

core::arch::global_asm!(include_str!("entry.S"));

use core::panic::PanicInfo;

/// How many candidates the sieve below checks. Kept tiny on purpose --
/// this tape exists to demonstrate the toolchain, not to race
/// `primes.S`'s much larger 1,000,000-candidate sweep.
const SIEVE_LIMIT: usize = 1_000;

/// Prints one byte through the callback the kernel passed in -- see
/// `console::print_byte` in kernel/src/console.rs, the only thing on the
/// other end of this function pointer.
fn print_bytes(putc: extern "C" fn(u32), bytes: &[u8]) {
    for &byte in bytes {
        putc(u32::from(byte));
    }
}

/// Prints an unsigned integer in decimal, most significant digit first,
/// without `core::fmt`/allocation -- just a fixed stack buffer, the same
/// spirit as `console.rs`'s own number formatting.
fn print_u32(putc: extern "C" fn(u32), mut value: u32) {
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
    for index in (0..count).rev() {
        putc(u32::from(digits[index]));
    }
}

/// The tape's entry point: called as `extern "C" fn(putc: extern "C"
/// fn(u32))`, the same ABI every tape (assembly, C, or Rust) must expose
/// (see `tape_run` in kernel/src/shell.rs). Named `tape_main`, not
/// `_start`, because `src/entry.S`'s `_start` is what the linker script
/// actually places first -- it zeroes `.bss` before tail-calling here.
#[unsafe(no_mangle)]
pub extern "C" fn tape_main(putc: extern "C" fn(u32)) {
    print_bytes(putc, b"fos Rust test tape: Sieve of Eratosthenes up to ");
    print_u32(putc, SIEVE_LIMIT as u32);
    print_bytes(putc, b"\n");

    // `static mut` (rather than a stack array) because SIEVE_LIMIT bytes
    // of stack in a tape running on the kernel's own stack, with no guard
    // page, is one more thing not worth risking -- the same reasoning
    // `primes.S`'s much larger sieve lives in `.bss` for.
    static mut SIEVE: [bool; SIEVE_LIMIT] = [false; SIEVE_LIMIT];
    let sieve = unsafe { &mut *(&raw mut SIEVE) };

    let mut found = 0u32;
    let mut largest = 0u32;
    for candidate in 2..SIEVE_LIMIT {
        if sieve[candidate] {
            continue;
        }
        found += 1;
        largest = candidate as u32;
        let mut multiple = candidate * candidate;
        while multiple < SIEVE_LIMIT {
            sieve[multiple] = true;
            multiple += candidate;
        }
    }

    print_bytes(putc, b"  primes found:  ");
    print_u32(putc, found);
    print_bytes(putc, b"\n  largest prime: ");
    print_u32(putc, largest);
    print_bytes(putc, b"\n");
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
