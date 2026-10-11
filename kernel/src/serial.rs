//! COM1: where the kernel narrates, and what every headless machine and every test
//! reads. Programmed directly with port I/O (the firmware's console is gone).

use core::fmt;

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

/// While a tape runs, serial output is queued here instead of waiting for the UART. At 115200 baud a byte takes about
/// 87 us, so a panel redrawing a screen line (a few hundred bytes with its escape codes) held the tape's loop up for
/// 20 ms or more, and the USB audio ring ran dry. `pump` moves queued bytes into the 16-byte FIFO whenever the
/// UART has room; `flush` waits for all of it (tape end, panic, halt, reset).
const QUEUE_SIZE: usize = 131072;
static mut QUEUE: [u8; QUEUE_SIZE] = [0; QUEUE_SIZE];
static mut QUEUE_HEAD: usize = 0;
static mut QUEUE_TAIL: usize = 0;
static mut DEFER: bool = false;

/// Runs `body` with interrupts off, so an interrupt handler that prints cannot interleave with the queue update.
fn locked<T>(body: impl FnOnce() -> T) -> T {
    let flags: u64;
    unsafe { core::arch::asm!("pushfq", "pop {}", "cli", out(reg) flags) };
    let result = body();
    if flags & 0x200 != 0 {
        unsafe { core::arch::asm!("sti") };
    }
    result
}

fn write_now(byte: u8) {
    // Wait (bounded) for the transmit register to empty: a machine with no UART
    // reads 0xff and must not hang the kernel.
    for _ in 0..100_000 {
        if inb(COM1 + 5) & 0x20 != 0 {
            break;
        }
    }
    outb(COM1, byte);
}

/// Queues output while a tape runs (`on`), or drains the queue and goes back to writing directly.
pub fn defer(on: bool) {
    if !on {
        flush();
    }
    unsafe { DEFER = on };
}

/// Moves queued bytes to the UART while its transmit register is empty (it then takes a whole FIFO at once).
pub fn pump() {
    locked(|| unsafe {
        let mut room = 16;
        while room > 0 && QUEUE_TAIL != QUEUE_HEAD && inb(COM1 + 5) & 0x20 != 0 {
            let burst = room.min((QUEUE_HEAD + QUEUE_SIZE - QUEUE_TAIL) % QUEUE_SIZE);
            for _ in 0..burst {
                outb(COM1, QUEUE[QUEUE_TAIL]);
                QUEUE_TAIL = (QUEUE_TAIL + 1) % QUEUE_SIZE;
            }
            room = 0;
        }
    });
}

/// True while queued output waits for the UART.
pub fn pending() -> bool {
    unsafe { QUEUE_TAIL != QUEUE_HEAD }
}

/// Writes out everything queued, waiting for the UART as the direct path does.
pub fn flush() {
    locked(|| unsafe {
        while QUEUE_TAIL != QUEUE_HEAD {
            write_now(QUEUE[QUEUE_TAIL]);
            QUEUE_TAIL = (QUEUE_TAIL + 1) % QUEUE_SIZE;
        }
    });
}

pub fn write_byte(byte: u8) {
    if !unsafe { DEFER } {
        write_now(byte);
        return;
    }
    locked(|| unsafe {
        let next = (QUEUE_HEAD + 1) % QUEUE_SIZE;
        if next == QUEUE_TAIL {
            // Full: wait for the oldest byte as the direct path would.
            write_now(QUEUE[QUEUE_TAIL]);
            QUEUE_TAIL = (QUEUE_TAIL + 1) % QUEUE_SIZE;
        }
        QUEUE[QUEUE_HEAD] = byte;
        QUEUE_HEAD = next;
    });
}

/// A received byte, if one is waiting (LSR bit 0). A machine with no UART reads 0xff from
/// the line-status register, which must not be mistaken for data. There is no interrupt: the
/// foreground polls (the 16-byte FIFO covers a 10 ms idle tick at typing speed).
pub fn read_byte() -> Option<u8> {
    let status = inb(COM1 + 5);
    if status == 0xff || status & 1 == 0 {
        return None;
    }
    Some(inb(COM1))
}

pub struct Serial;

impl fmt::Write for Serial {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for byte in text.bytes() {
            if byte == b'\n' {
                write_byte(b'\r');
            }
            write_byte(byte);
        }
        Ok(())
    }
}
