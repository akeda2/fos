//! A "paper tape" that proves a tape can use far more RAM than its own
//! fixed 4 MiB static region (`pmem::TAPE_BASE`/`TAPE_LEN`) by using the
//! tape ABI's third and fourth callbacks, `alloc`/`free`
//! (`shell::tape_alloc`/`shell::tape_free`) -- see AGENTS.md's "Tape ABI
//! grows alloc/free: using the rest of the machine's RAM" section for the
//! full story of why these callbacks exist, and
//! `docs/rust-tapes-for-dummies.md` for a beginner-level walkthrough of
//! writing a tape like this one from scratch.
//!
//! `hello-rs` proved a Rust tape works at all by sieving primes up to a
//! small, fixed bound into a `static mut` array -- entirely within its
//! own 4 MiB region. This tape sieves a bound 65,536 times larger by
//! requesting a fresh 64 MiB buffer from the "big pool" at run time
//! instead: proof that a tape's working set is not actually capped at
//! 4 MiB, only its own compiled code/data/`.bss` is -- everything else
//! this machine reports as usable RAM (up to `pmem::BIG_POOL_END`, 2 GiB)
//! is reachable too. Unlike `hello-rs`'s sieve, this one only computes
//! `candidate * candidate` up to `sqrt(SIEVE_LIMIT)` (see the marking
//! loop below) -- `hello-rs`'s bound is small enough that squaring every
//! candidate all the way up never overflows `u32`, but at this size it
//! would.
#![no_std]
#![no_main]

core::arch::global_asm!(include_str!("entry.S"));

use core::panic::PanicInfo;

/// Size of the dynamically allocated sieve buffer, in bytes -- and
/// therefore the largest sieve candidate checked (one byte per
/// candidate, the same layout `hello-rs`'s `static mut` sieve uses, just
/// much bigger). A clean 64 MiB, sixteen times `pmem::TAPE_LEN`'s 4 MiB,
/// so this can never fit in the tape's own fixed region no matter how
/// it's compiled -- it must come from `alloc` instead. `alloc`/`free`
/// round this up to whole `pmem::BIG_CHUNK_SIZE` (1 MiB) chunks; 64 MiB
/// is already an exact multiple, so no rounding actually happens here.
const SIEVE_LIMIT: usize = 64 * 1024 * 1024;

/// Prints one byte through the callback the kernel passed in -- see
/// `console::print_byte` in kernel/src/console.rs, the only thing on the
/// other end of this function pointer.
fn print_bytes(putc: extern "C" fn(u32), bytes: &[u8]) {
    for &byte in bytes {
        putc(u32::from(byte));
    }
}

/// Prints an unsigned integer in decimal, most significant digit first,
/// without `core::fmt`/allocation -- the same approach `hello-rs` uses.
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

/// Prints a `u32` as 8 fixed-width lowercase hex digits (with leading
/// zeros), purely so this tape can show the physical address `alloc`
/// handed back -- there's no `core::fmt` here to do it for us.
fn print_hex_u32(putc: extern "C" fn(u32), value: u32) {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    for shift in (0..8).rev() {
        let nibble = ((value >> (shift * 4)) & 0xf) as usize;
        putc(u32::from(DIGITS[nibble]));
    }
}

/// The tape's entry point: called as `extern "C" fn(putc, should_abort,
/// alloc, free)`, the full four-argument tape ABI (see
/// `shell::tape_run`'s doc comment). This tape runs once and returns, so
/// it never calls `should_abort` -- but it must still declare that
/// parameter (even if unused) to keep `alloc`/`free` lined up in their
/// correct stack positions; a tape may only skip a *trailing* run of
/// parameters, never one in the middle. Named `tape_main`, not `_start`,
/// because `src/entry.S`'s `_start` is what the linker script actually
/// places first -- it zeroes `.bss` before tail-calling here.
#[unsafe(no_mangle)]
pub extern "C" fn tape_main(
    putc: extern "C" fn(u32),
    _should_abort: extern "C" fn() -> u32,
    alloc: extern "C" fn(u32) -> u32,
    free: extern "C" fn(u32, u32),
) {
    print_bytes(putc, b"fos Rust big-memory test tape: Sieve of Eratosthenes up to ");
    print_u32(putc, SIEVE_LIMIT as u32);
    print_bytes(putc, b"\n  requesting a 64 MiB buffer (64 chunks) from the big pool...\n");

    let address = alloc(SIEVE_LIMIT as u32);
    if address == 0 {
        print_bytes(putc, b"  alloc failed: not enough contiguous free memory for this tape's buffer.\n");
        return;
    }
    print_bytes(putc, b"  buffer allocated at physical 0x");
    print_hex_u32(putc, address);
    print_bytes(putc, b"\n");

    // Unlike `hello-rs`'s `static mut` sieve (zeroed for free by the
    // flat binary's own loading, see entry.S), frames handed back by
    // `alloc` are not guaranteed to start zeroed -- they may hold
    // whatever a previous allocation left behind -- so this tape must
    // zero its own buffer before treating "0" as "not yet marked
    // composite".
    let sieve = unsafe { core::slice::from_raw_parts_mut(address as *mut u8, SIEVE_LIMIT) };
    for byte in sieve.iter_mut() {
        *byte = 0;
    }

    // Marking pass: only walk candidates up to sqrt(SIEVE_LIMIT). Every
    // composite has a factor <= its own square root, so a candidate past
    // that point can never be the *smaller* factor of an unmarked
    // multiple -- there is nothing left for it to mark. This isn't just
    // an optimization: it also keeps `candidate * candidate` from ever
    // overflowing u32. If this loop ran all the way to SIEVE_LIMIT (as a
    // naive port of the small `primes` tape's algorithm would), a
    // candidate above 65536 would square to well over 4 billion, silently
    // wrapping around in release mode (`overflow-checks` is off) and
    // corrupting the sieve with bogus composite markings far from where
    // they belong.
    let mut candidate = 2usize;
    while candidate * candidate < SIEVE_LIMIT {
        if sieve[candidate] == 0 {
            let mut multiple = candidate * candidate;
            while multiple < SIEVE_LIMIT {
                sieve[multiple] = 1;
                multiple += candidate;
            }
        }
        candidate += 1;
    }

    // Counting pass: now that every composite below SIEVE_LIMIT has been
    // marked, a single linear scan finds the primes without touching
    // `candidate * candidate` again.
    let mut found = 0u32;
    let mut largest = 0u32;
    for candidate in 2..SIEVE_LIMIT {
        if sieve[candidate] == 0 {
            found += 1;
            largest = candidate as u32;
        }
    }

    print_bytes(putc, b"  primes found:  ");
    print_u32(putc, found);
    print_bytes(putc, b"\n  largest prime: ");
    print_u32(putc, largest);
    print_bytes(putc, b"\n");

    // Give the 64 MiB buffer back to the big pool -- `alloc` without
    // a matching `free` leaks those frames permanently, exactly like a
    // `palloc` from the shell with no matching `pfree` would.
    free(address, SIEVE_LIMIT as u32);
    print_bytes(putc, b"  freed the 64 MiB buffer back to the big pool.\n");
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
