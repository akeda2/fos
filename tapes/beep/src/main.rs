#![no_std]
#![no_main]

//! A 32-bit Rust tape that demonstrates direct PC-speaker programming.
//!
//! The tape uses the newest seven-argument ABI, but only `read_key` is
//! needed beyond the ordinary print callback: it receives input from either
//! the local PS/2 keyboard or an active `term net` session. Each lowercase or
//! shifted `b` starts a 440 Hz tone for approximately 100 ms. One of the
//! documented function-key escape inputs stops the tape and returns to the
//! shell.
//!
//! The speaker tone uses PIT channel 2, while PIT channel 0 remains the
//! kernel's 100 Hz timer. The tone itself is continuous mode 3; its duration
//! is measured by observing channel 0's existing countdown for ten periods.
//! This avoids an uncalibrated CPU-delay loop and leaves the kernel timer
//! configuration intact.

use core::arch::asm;
use tape_rt::print_bytes;

/// PIT command and data ports. Channel 0 belongs to the kernel timer;
/// channel 2 is the independently available PC-speaker oscillator.
const PIT_COMMAND: u16 = 0x43;
const PIT_CHANNEL0_DATA: u16 = 0x40;
const PIT_CHANNEL2_DATA: u16 = 0x42;

/// The legacy PC speaker control port: bit 0 gates PIT channel 2 to the
/// speaker and bit 1 connects the channel-2 output to the speaker amplifier.
const SPEAKER_PORT: u16 = 0x61;

/// The PIT input clock and requested audible tone. The divisor is truncated
/// to the nearest integer PIT count, yielding approximately 440 Hz.
const PIT_INPUT_HZ: u32 = 1_193_182;
const TONE_HZ: u32 = 440;

/// PIT channel 0 runs at 100 Hz, so ten observed counter wraps are about
/// 100 ms. This uses the timer's existing programming rather than changing
/// the kernel's interrupt cadence just to time a beep.
const BEEP_TICKS: u32 = 10;

/// Stable event encoding documented by `shell::combined_tape_read_key`.
const TAPE_EVENT_NONE: u32 = 0;
const TAPE_EVENT_ABORT: u32 = 1;
const TAPE_EVENT_CHAR_BASE: u32 = 0x100;

/// Minimal 8-bit port input primitive. A tape has no kernel crate dependency,
/// so this repeats the small architecture-specific operation locally.
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

/// Minimal 8-bit port output primitive; see `inb` above.
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

/// Latches and reads the kernel-configured PIT channel-0 countdown.
fn pit_channel0_count() -> u16 {
    unsafe {
        outb(PIT_COMMAND, 0x00);
        let low = inb(PIT_CHANNEL0_DATA);
        let high = inb(PIT_CHANNEL0_DATA);
        u16::from_le_bytes([low, high])
    }
}

/// Waits for a number of channel-0 countdown wraps. Since channel 0 is
/// already configured as a 100 Hz mode-3 timer by the kernel, each wrap is
/// approximately 10 ms.
fn wait_for_pit_ticks(ticks: u32) {
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

/// Disconnects both the PIT gate and the speaker amplifier, preserving all
/// unrelated control-port bits.
fn speaker_off() {
    unsafe {
        let port = inb(SPEAKER_PORT);
        outb(SPEAKER_PORT, port & !0x03);
    }
}

/// Programs PIT channel 2 for a 440 Hz square wave, enables the speaker,
/// waits approximately 100 ms using channel 0, then disables the speaker.
fn beep() {
    let divisor = (PIT_INPUT_HZ / TONE_HZ) as u16;
    unsafe {
        outb(PIT_COMMAND, 0xb6);
        outb(PIT_CHANNEL2_DATA, divisor as u8);
        outb(PIT_CHANNEL2_DATA, (divisor >> 8) as u8);

        let port = inb(SPEAKER_PORT);
        outb(SPEAKER_PORT, port | 0x03);
    }
    wait_for_pit_ticks(BEEP_TICKS);
    speaker_off();
}

/// Waits for the lowercase or shifted `b` key and beeps once per key press.
/// `read_key` supplies either local PS/2 input or bytes from an active
/// `term net` session, so the tape behaves the same from either source. The
/// unused ABI parameters remain in the signature because later callbacks
/// cannot be skipped while preserving their cdecl stack positions.
#[unsafe(no_mangle)]
pub extern "C" fn tape_main(
    putc: extern "C" fn(u32),
    _should_abort: extern "C" fn() -> u32,
    _alloc: extern "C" fn(u32) -> u32,
    _free: extern "C" fn(u32, u32),
    read_key: extern "C" fn() -> u32,
    _args_ptr: u32,
    _args_len: u32,
) {
    print_bytes(
        putc,
        b"PC speaker test: press b for a 440 Hz beep; press F12 (or F4/F6/F7/F8/F10) to exit.\n",
    );

    loop {
        match read_key() {
            TAPE_EVENT_NONE => core::hint::spin_loop(),
            TAPE_EVENT_ABORT => break,
            event if event >= TAPE_EVENT_CHAR_BASE => {
                let byte = (event - TAPE_EVENT_CHAR_BASE) as u8;
                if byte == b'b' || byte == b'B' {
                    beep();
                    print_bytes(putc, b"Beep.\n");
                }
            }
            _ => {}
        }
    }

    speaker_off();
    print_bytes(putc, b"PC speaker test stopped.\n");
}
