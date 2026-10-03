//! Where the loader's narration goes: COM1 and the firmware's own text console.
//!
//! The kernel's `println!` has the same shape (one `write_bytes` choke point
//! mirrored to the serial port) and for the same reason: the serial port is what
//! every test and every headless machine reads. The firmware console is mirrored
//! too, so the text also shows on a screen when there is one.

use crate::uefi::SimpleTextOutput;
use core::fmt;

const COM1: u16 = 0x3f8;

/// Whether to also write straight to COM1. Off in the loader: firmware such as
/// OVMF already mirrors its console to the serial port, so writing both would
/// print every character twice, interleaved. (A machine whose firmware does *not*
/// route the console to a UART shows the narration on its screen only; the kernel,
/// which has no firmware console left, drives COM1 itself.) Flip this to debug
/// firmware that has no console at all.
const MIRROR_TO_COM1: bool = false;

pub struct Console {
    /// The firmware's `ConOut`, or null before it is known.
    pub text_out: *mut SimpleTextOutput,
}

static mut CONSOLE: Console = Console { text_out: core::ptr::null_mut() };

fn outb(port: u16, value: u8) {
    unsafe { core::arch::asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack)) };
}

fn inb(port: u16) -> u8 {
    let value: u8;
    unsafe { core::arch::asm!("in al, dx", in("dx") port, out("al") value, options(nomem, nostack)) };
    value
}

/// 115200 baud, 8 data bits, no parity, one stop bit, FIFOs on: the settings the
/// rest of this project (and its test harness) assumes. QEMU does not care, real
/// UARTs do.
pub fn init_serial() {
    if !MIRROR_TO_COM1 {
        return;
    }
    outb(COM1 + 1, 0x00); // no interrupts
    outb(COM1 + 3, 0x80); // DLAB on: divisor latch
    outb(COM1, 0x01); // divisor 1 = 115200 baud
    outb(COM1 + 1, 0x00);
    outb(COM1 + 3, 0x03); // 8N1, DLAB off
    outb(COM1 + 2, 0xc7); // FIFO on, cleared, 14-byte threshold
    outb(COM1 + 4, 0x03); // DTR + RTS
}

fn serial_byte(byte: u8) {
    // Wait (bounded) for the transmit-holding register to empty.
    for _ in 0..100_000 {
        if inb(COM1 + 5) & 0x20 != 0 {
            break;
        }
    }
    outb(COM1, byte);
}

/// Sends one UTF-16 string (`\n` already expanded to `\r\n`) to the firmware console.
fn firmware_string(text_out: *mut SimpleTextOutput, buffer: &mut [u16; 64], used: usize) {
    if text_out.is_null() || used == 0 {
        return;
    }
    buffer[used] = 0;
    unsafe { ((*text_out).output_string)(text_out, buffer.as_ptr()) };
}

impl fmt::Write for Console {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let mut buffer = [0u16; 64];
        let mut used = 0;
        for character in text.chars() {
            // Serial: ASCII, with CR before every LF; anything else becomes '?'.
            let byte = if character.is_ascii() { character as u8 } else { b'?' };
            if MIRROR_TO_COM1 {
                if byte == b'\n' {
                    serial_byte(b'\r');
                }
                serial_byte(byte);
            }
            // Firmware console: UTF-16, same CR/LF rule, flushed in small batches.
            if byte == b'\n' {
                buffer[used] = u16::from(b'\r');
                used += 1;
            }
            buffer[used] = u16::from(byte);
            used += 1;
            if used >= buffer.len() - 3 {
                firmware_string(self.text_out, &mut buffer, used);
                used = 0;
            }
        }
        firmware_string(self.text_out, &mut buffer, used);
        Ok(())
    }
}

pub fn set_text_out(text_out: *mut SimpleTextOutput) {
    unsafe { (*core::ptr::addr_of_mut!(CONSOLE)).text_out = text_out };
}

pub fn print(arguments: fmt::Arguments) {
    use core::fmt::Write;
    let _ = unsafe { (*core::ptr::addr_of_mut!(CONSOLE)).write_fmt(arguments) };
}

#[macro_export]
macro_rules! print {
    ($($argument:tt)*) => { $crate::console::print(format_args!($($argument)*)) };
}

#[macro_export]
macro_rules! println {
    () => { $crate::print!("\n") };
    ($($argument:tt)*) => {{ $crate::print!($($argument)*); $crate::print!("\n"); }};
}
