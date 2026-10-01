//! A reverse-Polish-notation (RPN) integer calculator tape -- the second
//! tape built specifically to exercise the tape ABI's `read_key`/
//! `args_ptr`/`args_len` additions (see AGENTS.md's "Tape ABI grows
//! again: `read_key` and startup arguments"), and the very gap that
//! motivated adding them in the first place: an earlier attempt at this
//! same calculator idea discovered there was no way for any tape to
//! receive a parameter or read a keystroke at all.
//!
//! RPN (rather than ordinary infix notation) was chosen because it needs
//! no operator-precedence parser, no parentheses, and no lookahead --
//! just a stack and a single left-to-right pass over whitespace-
//! separated tokens, each either a signed decimal integer (pushed) or one
//! of `+ - * / %` (pops two operands, pushes one result). That simplicity
//! is exactly the point: this tape exists as a worked example of the new
//! ABI additions, not as a demonstration of parser design.
//!
//! Two independent ways to use it, both exercising a different half of
//! the new ABI:
//!
//! - **One-shot, via startup arguments**: `tape run 3 4 +` evaluates
//!   `"3 4 +"` (the `args_ptr`/`args_len` the shell passes straight
//!   through from the command line) once, prints the result, and
//!   returns immediately -- no interaction needed at all.
//! - **Interactive, via `read_key`**: plain `tape run` (no arguments)
//!   instead starts a small REPL, reading one line at a time -- from
//!   whichever source started the tape, local keyboard or an active
//!   `term net` session, exactly like every other `read_key`-based tape
//!   -- evaluating and printing a result after each line, until one of
//!   the "give up" keys is pressed (reported by `read_key` as
//!   `TAPE_EVENT_ABORT`, not a separate `should_abort` call -- see the
//!   warning in `tape_main`'s own doc comment below).
//!
//! Like `tapes/vga-colors`, this is a genuinely new tape (not a teaching-
//! ladder example in `docs/rust-tapes-for-dummies.md`), so it depends on
//! `../tape-rt` for `_start`/the panic handler/`print_bytes`/`print_u32`
//! rather than duplicating that boilerplate a third time -- see
//! `tape-rt/src/lib.rs`'s header comment for the reasoning.
#![no_std]
#![no_main]

use tape_rt::{print_bytes, print_u32};

/// Maximum number of RPN tokens/operands this calculator can hold at
/// once. Deliberately small and fixed -- there is no heap, and no single
/// RPN expression typed at a keyboard is ever going to need more than a
/// handful of pending operands. Pushing past this returns
/// `EvalError::StackFull` rather than silently truncating or corrupting
/// memory.
const STACK_CAPACITY: usize = 32;

/// Maximum length of one line of interactive input. Long enough for any
/// reasonable RPN expression typed by hand; a line reaching this limit
/// just stops accepting further characters until Enter is pressed (see
/// `tape_main`'s input loop) rather than wrapping, growing, or corrupting
/// anything.
const LINE_CAPACITY: usize = 120;

/// These three mirror `kernel/src/shell.rs`'s private `TAPE_EVENT_NONE`/
/// `TAPE_EVENT_ABORT`/`TAPE_EVENT_CHAR_BASE` constants exactly -- a tape
/// has no way to `use` the kernel's own crate, so every tape reading
/// `read_key`'s return value redefines this same small, stable encoding
/// itself. See `shell::combined_tape_read_key`'s doc comment for the
/// authoritative definition this must stay in sync with.
const TAPE_EVENT_NONE: u32 = 0;
const TAPE_EVENT_ABORT: u32 = 1;

/// Everything that can go wrong evaluating one line of RPN input. Kept as
/// plain data (no `core::fmt`, no heap-allocated message strings) so
/// `report_result` below can match on it and print a fixed, friendly
/// message through the tape's own `putc` callback.
enum EvalError {
    /// The line contained no tokens at all (or only whitespace).
    Empty,
    /// An operator was seen with fewer than two operands already on the
    /// stack.
    Underflow,
    /// A `/` or `%` was attempted with a zero right-hand operand.
    DivideByZero,
    /// A `+`/`-`/`*`/`/`/`%` result (or a parsed literal) didn't fit in
    /// an `i32`.
    Overflow,
    /// A token was neither a valid signed integer nor a recognized
    /// operator.
    UnknownToken,
    /// Pushing another operand would exceed `STACK_CAPACITY`.
    StackFull,
    /// More than one value remained on the stack once every token was
    /// consumed -- almost always a missing operator. Carries the count
    /// so the message can report exactly how many were left over.
    LeftoverOperands(usize),
}

/// Splits `line` on ASCII whitespace (space, tab, or anything else at or
/// below `b' '`) and returns the next non-empty token, advancing `pos`
/// past it (and past any whitespace immediately before it). Returns
/// `None` once nothing but whitespace remains. Deliberately hand-rolled
/// instead of using `core::str`/`split_whitespace`: this operates on raw
/// bytes (input may not even be valid UTF-8, e.g. a stray high byte from
/// a mis-typed key), matching the rest of this project's `no_std` byte-
/// oriented parsing (see `kernel/src/shell.rs`'s own `take_word`).
fn next_token<'a>(line: &'a [u8], pos: &mut usize) -> Option<&'a [u8]> {
    while *pos < line.len() && line[*pos] <= b' ' {
        *pos += 1;
    }
    if *pos >= line.len() {
        return None;
    }
    let start = *pos;
    while *pos < line.len() && line[*pos] > b' ' {
        *pos += 1;
    }
    Some(&line[start..*pos])
}

/// Parses a token as a signed decimal integer (an optional leading `-`
/// followed by one or more ASCII digits). Returns `None` for anything
/// else, including an out-of-`i32`-range value -- `checked_mul`/
/// `checked_add` catch that rather than silently wrapping.
fn parse_i32(token: &[u8]) -> Option<i32> {
    let (negative, digits) = match token.split_first() {
        Some((b'-', rest)) => (true, rest),
        _ => (false, token),
    };
    if digits.is_empty() {
        return None;
    }
    let mut magnitude: i32 = 0;
    for &byte in digits {
        if !byte.is_ascii_digit() {
            return None;
        }
        magnitude = magnitude.checked_mul(10)?.checked_add(i32::from(byte - b'0'))?;
    }
    Some(if negative { -magnitude } else { magnitude })
}

/// Evaluates one whole line as an RPN expression: a left-to-right pass
/// pushing numbers and applying operators to the top of a small fixed
/// stack, exactly as RPN is meant to be evaluated. Returns the single
/// remaining value on success, or the first `EvalError` encountered.
fn eval_line(line: &[u8]) -> Result<i32, EvalError> {
    let mut stack = [0i32; STACK_CAPACITY];
    let mut top: usize = 0;
    let mut pos: usize = 0;

    while let Some(token) = next_token(line, &mut pos) {
        if let Some(value) = parse_i32(token) {
            if top >= STACK_CAPACITY {
                return Err(EvalError::StackFull);
            }
            stack[top] = value;
            top += 1;
            continue;
        }

        if token.len() != 1 {
            return Err(EvalError::UnknownToken);
        }
        let operator = token[0];
        if !matches!(operator, b'+' | b'-' | b'*' | b'/' | b'%') {
            return Err(EvalError::UnknownToken);
        }
        if top < 2 {
            return Err(EvalError::Underflow);
        }
        let rhs = stack[top - 1];
        let lhs = stack[top - 2];
        top -= 2;

        let result = match operator {
            b'+' => lhs.checked_add(rhs),
            b'-' => lhs.checked_sub(rhs),
            b'*' => lhs.checked_mul(rhs),
            b'/' => {
                if rhs == 0 {
                    return Err(EvalError::DivideByZero);
                }
                lhs.checked_div(rhs)
            }
            b'%' => {
                if rhs == 0 {
                    return Err(EvalError::DivideByZero);
                }
                lhs.checked_rem(rhs)
            }
            _ => unreachable!("operator already validated above"),
        };
        match result {
            Some(value) => {
                stack[top] = value;
                top += 1;
            }
            None => return Err(EvalError::Overflow),
        }
    }

    match top {
        0 => Err(EvalError::Empty),
        1 => Ok(stack[0]),
        remaining => Err(EvalError::LeftoverOperands(remaining)),
    }
}

/// Prints a signed 32-bit value in decimal -- `tape-rt::print_u32` only
/// handles unsigned magnitudes, so this just prints a leading `-` first
/// when needed. Widening to `i64` before negating sidesteps `i32::MIN`
/// having no positive `i32` counterpart (`-i32::MIN` would overflow);
/// `i32::MIN`'s magnitude (2,147,483,648) still fits comfortably in the
/// `u32` `print_u32` expects.
fn print_i32(putc: extern "C" fn(u32), value: i32) {
    let widened = i64::from(value);
    if widened < 0 {
        putc(u32::from(b'-'));
        print_u32(putc, (-widened) as u32);
    } else {
        print_u32(putc, widened as u32);
    }
}

/// Evaluates one line and prints either `= RESULT` or a friendly `Error:
/// ...` message -- shared by both the one-shot (startup-argument) and
/// interactive (`read_key`) paths below, so the two ways of using this
/// tape report results identically.
fn report_result(putc: extern "C" fn(u32), line: &[u8]) {
    match eval_line(line) {
        Ok(value) => {
            print_bytes(putc, b"= ");
            print_i32(putc, value);
            print_bytes(putc, b"\n");
        }
        Err(EvalError::Empty) => print_bytes(putc, b"Error: empty expression.\n"),
        Err(EvalError::Underflow) => {
            print_bytes(putc, b"Error: not enough operands for that operator.\n")
        }
        Err(EvalError::DivideByZero) => print_bytes(putc, b"Error: division by zero.\n"),
        Err(EvalError::Overflow) => print_bytes(putc, b"Error: arithmetic overflow.\n"),
        Err(EvalError::UnknownToken) => print_bytes(
            putc,
            b"Error: unrecognized token (expected a number or one of + - * / %).\n",
        ),
        Err(EvalError::StackFull) => {
            print_bytes(putc, b"Error: too many operands (stack limit reached).\n")
        }
        Err(EvalError::LeftoverOperands(remaining)) => {
            print_bytes(putc, b"Error: ");
            print_u32(putc, remaining as u32);
            print_bytes(putc, b" values left on the stack -- missing an operator?\n");
        }
    }
}

/// The tape's entry point, using the full seven-parameter ABI (see
/// `shell::tape_run`'s doc comment): `putc`/`should_abort`/`alloc`/
/// `free` from the earlier ABI generations, plus `read_key`/`args_ptr`/
/// `args_len` from the latest one. `should_abort` is intentionally never
/// called here -- this tape reads input via `read_key` instead, and
/// `read_key`'s own `TAPE_EVENT_ABORT` already reports every "give up"
/// signal `should_abort` would have, so calling both would race them for
/// the same underlying input sources and intermittently drop keystrokes
/// (see AGENTS.md's "Tape ABI grows again" section).
#[unsafe(no_mangle)]
pub extern "C" fn tape_main(
    putc: extern "C" fn(u32),
    _should_abort: extern "C" fn() -> u32,
    _alloc: extern "C" fn(u32) -> u32,
    _free: extern "C" fn(u32, u32),
    read_key: extern "C" fn() -> u32,
    args_ptr: u32,
    args_len: u32,
    _nic_mac: extern "C" fn(u32) -> u32,
    _nic_send: extern "C" fn(u32, u32) -> u32,
    _nic_receive: extern "C" fn(u32, u32) -> u32,
    _nic_max_frame_len: extern "C" fn() -> u32,
    _ticks: extern "C" fn() -> u64,
    _nic_count: extern "C" fn() -> u32,
    _nic_mac_at: extern "C" fn(u32, u32) -> u32,
    _nic_send_at: extern "C" fn(u32, u32, u32) -> u32,
    _nic_receive_at: extern "C" fn(u32, u32, u32) -> u32,
    _nic_set_promiscuous_at: extern "C" fn(u32, u32) -> u32,
    _terminal_position: extern "C" fn() -> u32,
    _fs_file_length: extern "C" fn(u32, u32, u32) -> u32,
    _fs_read: extern "C" fn(u32, u32, u32, u32, u32) -> u32,
    _fs_write: extern "C" fn(u32, u32, u32, u32) -> u32,
    _fs_remove: extern "C" fn(u32, u32) -> u32,
    _audio_sample_rate: extern "C" fn() -> u32,
    _audio_channels_and_bits: extern "C" fn() -> u32,
    _audio_submit: extern "C" fn(u32, u32) -> u32,
    _audio_capture: extern "C" fn(u32, u32) -> u32,
    _audio_count: extern "C" fn() -> u32,
    _audio_sample_rate_at: extern "C" fn(u32) -> u32,
    _audio_channels_and_bits_at: extern "C" fn(u32) -> u32,
    _audio_submit_at: extern "C" fn(u32, u32, u32) -> u32,
    _audio_capture_at: extern "C" fn(u32, u32, u32) -> u32,
    _audio_capture_sample_rate: extern "C" fn() -> u32,
    _audio_capture_channels_and_bits: extern "C" fn() -> u32,
    _audio_capture_sample_rate_at: extern "C" fn(u32) -> u32,
    _audio_capture_channels_and_bits_at: extern "C" fn(u32) -> u32,
    // Sleeps until the next interrupt; call after a pass that found no work.
    wait_for_event: extern "C" fn(),
) {
    tape_rt::set_wait_hook(wait_for_event);
    // Safety: `args_ptr`/`args_len` come straight from `shell::tape_run`,
    // which always passes a valid (possibly empty) slice into its own
    // already-parsed command-line buffer for the entire duration of this
    // synchronous call -- see that function's doc comment.
    let arguments: &[u8] =
        unsafe { core::slice::from_raw_parts(args_ptr as *const u8, args_len as usize) };

    print_bytes(
        putc,
        b"fos RPN calculator -- 32-bit signed integers, reverse Polish notation.\n",
    );

    if !arguments.is_empty() {
        // One-shot mode: `tape run 3 4 +` evaluates "3 4 +" once and
        // returns immediately, no interaction needed.
        print_bytes(putc, b"> ");
        print_bytes(putc, arguments);
        print_bytes(putc, b"\n");
        report_result(putc, arguments);
        return;
    }

    // Interactive mode: no arguments were given, so read and evaluate
    // one line at a time until told to give up.
    print_bytes(
        putc,
        b"Enter an RPN expression (e.g. 3 4 +) and press Enter.\n\
Operators: + - * / % (integer division and remainder).\n\
Press F12 (or F4/F6/F7/F8/F10) to exit.\n",
    );

    let mut line = [0u8; LINE_CAPACITY];
    let mut length: usize = 0;
    print_bytes(putc, b"> ");
    loop {
        match read_key() {
            // No key yet: sleep until the next interrupt (keyboard, remote
            // `term net` frame, or the timer tick) instead of spinning.
            TAPE_EVENT_NONE => {
                tape_rt::idle();
                continue;
            }
            TAPE_EVENT_ABORT => {
                print_bytes(putc, b"\nExiting the calculator.\n");
                break;
            }
            event => {
                // `read_key` already erased the on-screen character for
                // a backspace event (see its own doc comment) -- this
                // side only needs to shrink its own buffer to match.
                let byte = (event & 0xff) as u8;
                match byte {
                    8 => {
                        if length > 0 {
                            length -= 1;
                        }
                    }
                    b'\r' | b'\n' => {
                        putc(u32::from(b'\n'));
                        report_result(putc, &line[..length]);
                        length = 0;
                        print_bytes(putc, b"> ");
                    }
                    printable if (0x20..0x7f).contains(&printable) => {
                        if length < LINE_CAPACITY {
                            line[length] = printable;
                            length += 1;
                            putc(u32::from(printable));
                        }
                        // A line at its capacity simply stops accepting
                        // more characters until Enter is pressed --
                        // no silent truncation of what's already typed,
                        // no wraparound, no reallocation (there is no
                        // heap to reallocate into anyway).
                    }
                    _ => {
                        // Any other control byte (e.g. a stray escape
                        // sequence fragment) is ignored rather than
                        // inserted into the line.
                    }
                }
            }
        }
    }
}
