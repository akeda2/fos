//! COM1 as the first console: 115200 baud, 8N1, polled. `println!` writes here until the other consoles exist.

use core::fmt::{self, Write};

const COM1: u16 = 0x3f8;

fn outb(port: u16, value: u8) {
    unsafe { core::arch::asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack)) };
}

fn inb(port: u16) -> u8 {
    let value: u8;
    unsafe { core::arch::asm!("in al, dx", in("dx") port, out("al") value, options(nomem, nostack)) };
    value
}

/// 115200 baud, 8N1, FIFOs on: what the rest of this project assumes.
pub fn init() {
    outb(COM1 + 1, 0x00); // no interrupts
    outb(COM1 + 3, 0x80); // DLAB on
    outb(COM1, 0x01); // divisor 1 = 115200 baud
    outb(COM1 + 1, 0x00);
    outb(COM1 + 3, 0x03); // 8N1, DLAB off
    outb(COM1 + 2, 0xc7); // FIFO on, cleared
    outb(COM1 + 4, 0x03); // DTR + RTS
}

pub struct Serial;

impl Write for Serial {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for byte in text.bytes() {
            if byte == b'\n' {
                put(b'\r');
            }
            put(byte);
        }
        Ok(())
    }
}

fn put(byte: u8) {
    // Bounded wait for the transmit register: a machine with no UART reads 0xff and must not hang the kernel.
    for _ in 0..100_000 {
        if inb(COM1 + 5) & 0x20 != 0 {
            break;
        }
    }
    outb(COM1, byte);
}

/// One line at a time: CPUs print whole lines, never interleaved characters.
static LOCK: crate::sync::SpinLock<()> = crate::sync::SpinLock::new(());

pub fn print(arguments: fmt::Arguments) {
    let _guard = LOCK.lock();
    let _ = Serial.write_fmt(arguments);
}

/// For the fault and panic paths: the CPU that was printing may be the one that crashed.
pub fn break_lock() {
    unsafe { LOCK.force_unlock() };
}

macro_rules! println {
    () => { $crate::serial::print(format_args!("\n")) };
    ($($argument:tt)*) => { $crate::serial::print(format_args!("{}\n", format_args!($($argument)*))) };
}
