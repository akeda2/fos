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

/// False on a machine with no UART at COM1 (most laptops): the line status register then reads 0xff, which would look like
/// "data received" forever, so reads return nothing and writes skip the transmit wait.
static PRESENT: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);

/// 115200 baud, 8N1, FIFOs on: what the rest of this project assumes.
pub fn init() {
    // The scratch register holds any byte on a real UART and reads back 0xff (or the bus float) on an empty port.
    outb(COM1 + 7, 0x5a);
    let present = inb(COM1 + 7) == 0x5a && inb(COM1 + 5) != 0xff;
    PRESENT.store(present, core::sync::atomic::Ordering::Relaxed);
    if !present {
        return;
    }
    outb(COM1 + 1, 0x00); // no interrupts
    outb(COM1 + 3, 0x80); // DLAB on
    outb(COM1, 0x01); // divisor 1 = 115200 baud
    outb(COM1 + 1, 0x00);
    outb(COM1 + 3, 0x03); // 8N1, DLAB off
    outb(COM1 + 2, 0xc7); // FIFO on, cleared
    outb(COM1 + 4, 0x03); // DTR + RTS
}

/// Output of the shell's commands that a script wants as text (`$(cmd)`, `cmd > file`) or looks through for an error line.
struct Capture {
    owner: usize,
    data: alloc::vec::Vec<u8>,
    /// Keeps the first 4 KiB and still prints: only the status of the command is wanted.
    tee: bool,
}

static CAPTURES: crate::sync::SpinLock<alloc::vec::Vec<Capture>> = crate::sync::SpinLock::new(alloc::vec::Vec::new());
static CAPTURING: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
const CAPTURE_LIMIT: usize = 1 << 20;

/// Starts catching what the calling thread prints (nested captures stack; a tape runs on its own thread and is not caught).
pub fn capture_begin(tee: bool) {
    let owner = crate::sched::current_id();
    CAPTURES.lock().push(Capture { owner, data: alloc::vec::Vec::new(), tee });
    CAPTURING.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
}

/// Ends the newest capture and returns its text.
pub fn capture_end() -> alloc::vec::Vec<u8> {
    let captured = CAPTURES.lock().pop();
    CAPTURING.fetch_sub(1, core::sync::atomic::Ordering::Relaxed);
    captured.map(|capture| capture.data).unwrap_or_default()
}

/// True when the text was taken by a capture and must not be printed.
fn captured(text: &str) -> bool {
    if CAPTURING.load(core::sync::atomic::Ordering::Relaxed) == 0 {
        return false;
    }
    let me = crate::sched::current_id();
    let mut captures = CAPTURES.lock();
    let Some(capture) = captures.iter_mut().rev().find(|capture| capture.owner == me) else { return false };
    let room = if capture.tee { 4096 } else { CAPTURE_LIMIT }.saturating_sub(capture.data.len());
    capture.data.extend_from_slice(&text.as_bytes()[..text.len().min(room)]);
    !capture.tee
}

pub struct Serial;

impl Write for Serial {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        if captured(text) {
            return Ok(());
        }
        crate::log::push(text);
        crate::net::relay(text);
        crate::fb::write_str(text);
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
    if !PRESENT.load(core::sync::atomic::Ordering::Relaxed) {
        return;
    }
    // Bounded wait for the transmit register: a machine with no UART reads 0xff and must not hang the kernel.
    for _ in 0..100_000 {
        if inb(COM1 + 5) & 0x20 != 0 {
            break;
        }
    }
    outb(COM1, byte);
}

/// One byte to COM1 only (not the screen or the network terminal): the serial update protocols answer through this.
pub fn write_byte(byte: u8) {
    put(byte);
}

/// The next received byte, if the UART has one (LSR bit 0).
pub fn read_byte() -> Option<u8> {
    (PRESENT.load(core::sync::atomic::Ordering::Relaxed) && inb(COM1 + 5) & 1 != 0).then(|| inb(COM1))
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

pub(crate) use println;

macro_rules! print {
    ($($argument:tt)*) => { $crate::serial::print(format_args!($($argument)*)) };
}
