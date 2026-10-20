//! COM1: where the kernel narrates, and what every headless machine and every test
//! reads. Programmed directly with port I/O (the firmware's console is gone).

use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

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

/// Received bytes waiting for the shell. With the receive interrupt on, the handler fills this from the UART's 16-byte
/// FIFO (115200 baud is a byte every 87 us, so a pasted line overran the FIFO long before the 10 ms idle tick polled it
/// and characters were lost); `read_byte` empties it.
const RX_SIZE: usize = 4096;
static mut RX: [u8; RX_SIZE] = [0; RX_SIZE];
static RX_HEAD: AtomicUsize = AtomicUsize::new(0);
static RX_TAIL: AtomicUsize = AtomicUsize::new(0);
static RX_INTERRUPT: AtomicBool = AtomicBool::new(false);
pub static RX_DROPPED: AtomicUsize = AtomicUsize::new(0);

pub const VECTOR: u8 = 0x42;

/// Turns on the receive interrupt once the I/O APIC is up. IRQ 4 is routed to `VECTOR`; the UART raises it when its
/// receive FIFO holds 8 bytes, or when bytes have sat there for four character times (so a short line still arrives).
/// MCR bit 3 (OUT2) is what connects a PC UART's interrupt pin to the IRQ line. Not enabled on a machine with no UART
/// (the line-status register reads 0xff) or when the interrupt cannot be routed: then the foreground polls as before.
pub fn enable_interrupt() -> bool {
    if inb(COM1 + 5) == 0xff || crate::ioapic::route_isa(4, VECTOR).is_none() {
        return false;
    }
    locked(|| {
        outb(COM1 + 2, 0x87); // FIFO on, receive trigger level 8, cleared
        outb(COM1 + 4, 0x0b); // DTR + RTS + OUT2
        outb(COM1 + 1, 0x01); // interrupt on received data
        RX_INTERRUPT.store(true, Ordering::Relaxed);
    });
    true
}

/// IRQ 4: move everything in the FIFO into the ring (the handler must not print).
pub fn interrupt() {
    drain();
}

fn drain() {
    for _ in 0..64 {
        let status = inb(COM1 + 5);
        if status == 0xff || status & 1 == 0 {
            break;
        }
        let byte = inb(COM1);
        let head = RX_HEAD.load(Ordering::Relaxed);
        let next = (head + 1) % RX_SIZE;
        if next == RX_TAIL.load(Ordering::Acquire) {
            RX_DROPPED.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        unsafe { (*core::ptr::addr_of_mut!(RX))[head] = byte };
        RX_HEAD.store(next, Ordering::Release);
    }
}

/// A received byte, if one is waiting. A machine with no UART reads 0xff from the line-status register, which must not
/// be mistaken for data. Without the interrupt the foreground polls the UART itself (the 16-byte FIFO covers a 10 ms idle
/// tick at typing speed, not a paste).
pub fn read_byte() -> Option<u8> {
    if !RX_INTERRUPT.load(Ordering::Relaxed) {
        let status = inb(COM1 + 5);
        if status == 0xff || status & 1 == 0 {
            return None;
        }
        return Some(inb(COM1));
    }
    let tail = RX_TAIL.load(Ordering::Relaxed);
    if tail == RX_HEAD.load(Ordering::Acquire) {
        return None;
    }
    let byte = unsafe { (*core::ptr::addr_of!(RX))[tail] };
    RX_TAIL.store((tail + 1) % RX_SIZE, Ordering::Release);
    Some(byte)
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
