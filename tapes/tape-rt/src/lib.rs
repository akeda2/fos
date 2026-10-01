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

/// The kernel's `wait_for_event` callback (the last tape ABI slot), stored by
/// `set_wait_hook` so any polling loop in a tape -- including deep inside
/// helper functions that were never passed the callback -- can call `idle()`
/// without threading another parameter through every signature. Tapes are
/// single-threaded and zero-initialized `.bss` leaves this `None` until set, so
/// a tape that never sets it simply never sleeps.
static mut WAIT_HOOK: Option<extern "C" fn()> = None;

/// Remembers the kernel's `wait_for_event` callback; call once from
/// `tape_main` before any polling loop.
pub fn set_wait_hook(hook: extern "C" fn()) {
    unsafe {
        WAIT_HOOK = Some(hook);
    }
}

/// Sleeps until the next interrupt (a received frame, a keypress, or the
/// 10 ms timer tick). Call it when a polling pass found nothing to do instead
/// of spinning; the sleep may end early, so just poll again. Does nothing if
/// no hook was set.
pub fn idle() {
    if let Some(hook) = unsafe { WAIT_HOOK } {
        hook();
    }
}

/// The kernel's `micros` callback (ABI slot 38): microseconds since boot, read
/// from the PIT's own counter, so deadlines and latencies are not limited to the
/// 10 ms tick.
static mut MICROS_HOOK: Option<extern "C" fn() -> u64> = None;

/// Remembers the kernel's `micros` callback; call once from `tape_main`.
pub fn set_micros_hook(hook: extern "C" fn() -> u64) {
    unsafe {
        MICROS_HOOK = Some(hook);
    }
}

/// Microseconds since boot, or 0 if no hook was set.
pub fn micros() -> u64 {
    match unsafe { MICROS_HOOK } {
        Some(hook) => hook(),
        None => 0,
    }
}

/// The kernel's `set_input_mode` callback (ABI slot 37), stored like the wait
/// hook so `set_raw_keys` can be called from anywhere.
static mut INPUT_MODE_HOOK: Option<extern "C" fn(u32)> = None;

/// Remembers the kernel's `set_input_mode` callback; call once from
/// `tape_main` before `set_raw_keys`.
pub fn set_input_mode_hook(hook: extern "C" fn(u32)) {
    unsafe {
        INPUT_MODE_HOOK = Some(hook);
    }
}

/// Chooses how the kernel's `read_key` treats Backspace. With `raw` false (the
/// default every tape starts in) the kernel erases the previous character on
/// screen itself whenever it returns Backspace, which suits a simple line
/// reader that only ever edits the end of its line. With `raw` true the kernel
/// does nothing: a tape that moves the cursor (see `KeyDecoder`) must redraw on
/// its own, using ANSI sequences such as `ESC [ 3 D` (cursor left) and
/// `ESC [ K` (erase to end of line), which the kernel's VGA console and a
/// serial/`term net` terminal both understand.
pub fn set_raw_keys(raw: bool) {
    if let Some(hook) = unsafe { INPUT_MODE_HOOK } {
        hook(u32::from(raw));
    }
}

/// A decoded key. Cursor keys arrive from `read_key` as ANSI escape sequences
/// (`ESC [ D` is Left), identically from the local PS/2 keyboard and from a
/// remote `term net` terminal; `KeyDecoder` turns the byte stream into these.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    /// A printable ASCII character.
    Char(u8),
    Enter,
    Backspace,
    Tab,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    Delete,
    Insert,
    PageUp,
    PageDown,
    /// Another control byte (`1` is Ctrl-A, `5` Ctrl-E, ...).
    Control(u8),
}

#[derive(Clone, Copy)]
enum DecoderState {
    Ground,
    Escape,
    /// Inside `ESC [`, accumulating the numeric parameter.
    Csi(u16),
    /// After `ESC O` (application-mode cursor keys, which some terminals send).
    Ss3,
}

/// Incremental ANSI key decoder: feed it one byte per `read_key` event. A
/// sequence can span several calls, so it keeps its state between them and
/// needs no timeouts -- a lone Escape never reaches a tape (the kernel turns
/// it into the abort event), so every `0x1b` byte starts a sequence.
pub struct KeyDecoder {
    state: DecoderState,
}

impl KeyDecoder {
    pub const fn new() -> Self {
        Self {
            state: DecoderState::Ground,
        }
    }

    /// Returns a key once `byte` completes one; `None` while mid-sequence or
    /// for a sequence this decoder does not know (which is dropped).
    pub fn feed(&mut self, byte: u8) -> Option<Key> {
        match self.state {
            DecoderState::Ground => match byte {
                0x1b => {
                    self.state = DecoderState::Escape;
                    None
                }
                b'\r' | b'\n' => Some(Key::Enter),
                8 | 127 => Some(Key::Backspace),
                b'\t' => Some(Key::Tab),
                32..=126 => Some(Key::Char(byte)),
                _ => Some(Key::Control(byte)),
            },
            DecoderState::Escape => match byte {
                b'[' => {
                    self.state = DecoderState::Csi(0);
                    None
                }
                b'O' => {
                    self.state = DecoderState::Ss3;
                    None
                }
                _ => {
                    // Not a sequence we know: forget the Escape and treat the
                    // byte as an ordinary key.
                    self.state = DecoderState::Ground;
                    self.feed(byte)
                }
            },
            DecoderState::Csi(parameter) => match byte {
                b'0'..=b'9' => {
                    self.state = DecoderState::Csi(
                        parameter.saturating_mul(10).saturating_add(u16::from(byte - b'0')),
                    );
                    None
                }
                // Modifier parameters (`ESC [ 1 ; 5 C`): keep only the last
                // number's final key, ignoring which modifier was held.
                b';' => {
                    self.state = DecoderState::Csi(0);
                    None
                }
                _ => {
                    self.state = DecoderState::Ground;
                    match byte {
                        b'A' => Some(Key::Up),
                        b'B' => Some(Key::Down),
                        b'C' => Some(Key::Right),
                        b'D' => Some(Key::Left),
                        b'H' => Some(Key::Home),
                        b'F' => Some(Key::End),
                        b'~' => match parameter {
                            1 | 7 => Some(Key::Home),
                            2 => Some(Key::Insert),
                            3 => Some(Key::Delete),
                            4 | 8 => Some(Key::End),
                            5 => Some(Key::PageUp),
                            6 => Some(Key::PageDown),
                            _ => None,
                        },
                        _ => None,
                    }
                }
            },
            DecoderState::Ss3 => {
                self.state = DecoderState::Ground;
                match byte {
                    b'A' => Some(Key::Up),
                    b'B' => Some(Key::Down),
                    b'C' => Some(Key::Right),
                    b'D' => Some(Key::Left),
                    b'H' => Some(Key::Home),
                    b'F' => Some(Key::End),
                    _ => None,
                }
            }
        }
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
