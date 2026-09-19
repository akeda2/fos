#![no_std]
#![no_main]

//! A 32-bit Rust Morse-code trainer built on the seven-argument tape ABI.
//!
//! With trailing `tape run ARGUMENTS`, the raw argument slice is processed
//! once and the tape returns. With plain `tape run`, each letter or digit
//! received through `read_key` is printed as uppercase text plus its Morse
//! representation, then played on the PC speaker. Interactive input works
//! from either the local PS/2 keyboard or an active `term net` session. The
//! interactive mode intentionally accepts one character at a time instead of
//! buffering lines, so the audible timing and callback-driven input path stay
//! obvious.
//!
//! The speaker uses PIT channel 2 in square-wave mode. PIT channel 0 remains
//! the kernel's 100 Hz timer, and its existing countdown provides the timing
//! units: one Morse unit is approximately 100 ms. A dot lasts one unit, a
//! dash lasts three, symbols are separated by one, and letters by three.

use core::arch::asm;
use tape_rt::print_bytes;

/// PIT command/data ports and the legacy PC-speaker control port.
const PIT_COMMAND: u16 = 0x43;
const PIT_CHANNEL0_DATA: u16 = 0x40;
const PIT_CHANNEL2_DATA: u16 = 0x42;
const SPEAKER_PORT: u16 = 0x61;

/// PIT input clock and audible carrier frequency. Integer divisor rounding
/// makes the resulting tone approximate rather than mathematically exact.
const PIT_INPUT_HZ: u32 = 1_193_182;
const TONE_HZ: u32 = 440;

/// Channel 0 runs at 100 Hz, so one PIT period is approximately 10 ms.
/// Ten periods make one Morse timing unit, approximately 100 ms.
const UNIT_TICKS: u32 = 10;

/// Stable event encoding from `shell::combined_tape_read_key`.
const TAPE_EVENT_NONE: u32 = 0;
const TAPE_EVENT_ABORT: u32 = 1;
const TAPE_EVENT_CHAR_BASE: u32 = 0x100;

/// Morse symbols for A-Z followed by 0-9. Keeping the table as fixed byte
/// slices avoids heap allocation and makes the encoding visible to readers.
const MORSE: [&[u8]; 36] = [
    b".-", b"-...", b"-.-.", b"-..", b".", b"..-.", b"--.", b"....", b"..", b".---", b"-.-",
    b".-..", b"--", b"-.", b"---", b".--.", b"--.-", b".-.", b"...", b"-", b"..-", b"...-", b".--",
    b"-..-", b"-.--", b"--..", b"-----", b".----", b"..---", b"...--", b"....-", b".....",
    b"-....", b"--...", b"---..", b"----.",
];

/// Minimal port-I/O primitives. A tape cannot depend on the kernel's
/// architecture module, so the hardware instruction is kept local and
/// documented here.
#[inline]
unsafe fn inb(port: u16) -> u8 {
    let value: u8;
    unsafe {
        asm!(
            "in al, dx",
            in("dx") port,
            out("al") value,
            options(nomem, nostack, preserves_flags)
        );
    }
    value
}

#[inline]
unsafe fn outb(port: u16, value: u8) {
    unsafe {
        asm!(
            "out dx, al",
            in("dx") port,
            in("al") value,
            options(nomem, nostack, preserves_flags)
        );
    }
}

/// Latches and reads the kernel-configured PIT channel-0 countdown without
/// changing its mode or divisor.
fn pit_channel0_count() -> u16 {
    unsafe {
        outb(PIT_COMMAND, 0x00);
        let low = inb(PIT_CHANNEL0_DATA);
        let high = inb(PIT_CHANNEL0_DATA);
        u16::from_le_bytes([low, high])
    }
}

/// Waits for `ticks` channel-0 countdown wraps. The counter decreases and
/// reloads, so an increase from one sample to the next marks one period.
fn wait_for_ticks(ticks: u32) {
    let mut previous = pit_channel0_count();
    let mut elapsed = 0;
    while elapsed < ticks {
        let current = pit_channel0_count();
        if current > previous {
            elapsed += 1;
        }
        previous = current;
        core::hint::spin_loop();
    }
}

/// Disables both speaker-control bits while preserving unrelated port bits.
fn speaker_off() {
    unsafe {
        let port = inb(SPEAKER_PORT);
        outb(SPEAKER_PORT, port & !0x03);
    }
}

/// Plays one square-wave tone for the requested number of Morse timing units.
fn play_tone(units: u32) {
    let divisor = (PIT_INPUT_HZ / TONE_HZ) as u16;
    unsafe {
        outb(PIT_COMMAND, 0xb6);
        outb(PIT_CHANNEL2_DATA, divisor as u8);
        outb(PIT_CHANNEL2_DATA, (divisor >> 8) as u8);

        let port = inb(SPEAKER_PORT);
        outb(SPEAKER_PORT, port | 0x03);
    }
    wait_for_ticks(units * UNIT_TICKS);
    speaker_off();
}

/// Looks up a supported input byte and returns its uppercase display byte and
/// the corresponding table index. Unsupported punctuation is ignored rather
/// than being assigned a misleading Morse representation.
fn morse_for(byte: u8) -> Option<(u8, usize)> {
    let uppercase = match byte {
        b'a'..=b'z' => byte - (b'a' - b'A'),
        b'A'..=b'Z' | b'0'..=b'9' => byte,
        _ => return None,
    };
    let index = match uppercase {
        b'A'..=b'Z' => (uppercase - b'A') as usize,
        b'0'..=b'9' => 26 + (uppercase - b'0') as usize,
        _ => return None,
    };
    Some((uppercase, index))
}

/// Prints and plays one Morse character. The output is also the observable
/// diagnostic used by the E2E test because QEMU's host audio is not captured.
fn announce_and_play(putc: extern "C" fn(u32), display: u8, code: &[u8]) {
    putc(u32::from(display));
    print_bytes(putc, b": ");
    print_bytes(putc, code);
    putc(u32::from(b'\n'));

    for (index, &symbol) in code.iter().enumerate() {
        play_tone(if symbol == b'.' { 1 } else { 3 });
        if index + 1 < code.len() {
            wait_for_ticks(UNIT_TICKS);
        }
    }
    wait_for_ticks(3 * UNIT_TICKS);
}

/// Processes a raw byte slice from either the startup-argument path or the
/// interactive path. Spaces and unsupported punctuation are ignored; this
/// keeps `tape run "sos!"` useful without inventing Morse for punctuation the
/// table does not document.
fn play_sequence(putc: extern "C" fn(u32), bytes: &[u8]) {
    for &byte in bytes {
        if let Some((display, index)) = morse_for(byte) {
            announce_and_play(putc, display, MORSE[index]);
        }
    }
}

/// Full seven-argument entry point. Only `putc` and `read_key` are used, but
/// the earlier parameters remain declared so the later callback keeps its
/// documented cdecl stack position. `args_ptr`/`args_len` select the
/// non-interactive mode when the shell supplied trailing text after `tape
/// run`; the slice remains valid for this synchronous call.
#[unsafe(no_mangle)]
pub extern "C" fn tape_main(
    putc: extern "C" fn(u32),
    _should_abort: extern "C" fn() -> u32,
    _alloc: extern "C" fn(u32) -> u32,
    _free: extern "C" fn(u32, u32),
    read_key: extern "C" fn() -> u32,
    args_ptr: u32,
    args_len: u32,
) {
    print_bytes(
        putc,
        b"Morse trainer: type A-Z or 0-9; dots/dashes beep at 440 Hz. Press F12 (or F4/F6/F7/F8/F10) to exit.\n",
    );

    let arguments =
        unsafe { core::slice::from_raw_parts(args_ptr as *const u8, args_len as usize) };
    if !arguments.is_empty() {
        print_bytes(putc, b"One-shot Morse for: ");
        print_bytes(putc, arguments);
        putc(u32::from(b'\n'));
        play_sequence(putc, arguments);
        print_bytes(putc, b"Morse one-shot complete.\n");
        return;
    }

    print_bytes(
        putc,
        b"Interactive mode; type letters or digits one at a time.\n",
    );
    loop {
        match read_key() {
            TAPE_EVENT_NONE => core::hint::spin_loop(),
            TAPE_EVENT_ABORT => break,
            event if event >= TAPE_EVENT_CHAR_BASE => {
                play_sequence(putc, &[(event - TAPE_EVENT_CHAR_BASE) as u8]);
            }
            _ => {}
        }
    }

    speaker_off();
    print_bytes(putc, b"Morse trainer stopped.\n");
}
