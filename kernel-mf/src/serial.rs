//! COM1 as the first console: 115200 baud, 8N1, polled. `println!` writes here until the other consoles exist.

use core::fmt::{self, Write};

#[cfg(target_arch = "x86_64")]
const COM1: u16 = 0x3f8;

#[cfg(target_arch = "x86_64")]
fn outb(port: u16, value: u8) {
    unsafe { core::arch::asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack)) };
}

#[cfg(target_arch = "x86_64")]
fn inb(port: u16) -> u8 {
    let value: u8;
    unsafe { core::arch::asm!("in al, dx", in("dx") port, out("al") value, options(nomem, nostack)) };
    value
}

/// False on a machine with no UART at COM1 (most laptops): the line status register then reads 0xff, which would look like
/// "data received" forever, so reads return nothing and writes skip the transmit wait.
#[cfg(target_arch = "x86_64")]
static PRESENT: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);

/// The PL011 is set up by the firmware and `kmain` finds its address; nothing to do here.
#[cfg(target_arch = "aarch64")]
pub fn init() {}

/// 115200 baud, 8N1, FIFOs on: what the rest of this project assumes.
#[cfg(target_arch = "x86_64")]
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
    let owner = current_thread();
    CAPTURES.lock().push(Capture { owner, data: alloc::vec::Vec::new(), tee });
    CAPTURING.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
}

/// Ends the newest capture and returns its text.
pub fn capture_end() -> alloc::vec::Vec<u8> {
    let captured = CAPTURES.lock().pop();
    CAPTURING.fetch_sub(1, core::sync::atomic::Ordering::Relaxed);
    captured.map(|capture| capture.data).unwrap_or_default()
}

#[cfg(target_arch = "x86_64")]
fn current_thread() -> usize {
    crate::sched::current_id()
}

/// There is no scheduler on aarch64 yet: one thread.
#[cfg(target_arch = "aarch64")]
fn current_thread() -> usize {
    0
}

/// True when the text was taken by a capture and must not be printed.
fn captured(text: &str) -> bool {
    if CAPTURING.load(core::sync::atomic::Ordering::Relaxed) == 0 {
        return false;
    }
    let me = current_thread();
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
        // SAFETY: only reached under LOCK (`print`), which owns the ring's write side.
        let ring = unsafe { &mut *core::ptr::addr_of_mut!(TX) };
        for byte in text.bytes() {
            if byte == b'\n' {
                ring.push(b'\r');
            }
            ring.push(byte);
        }
        Ok(())
    }
}

#[cfg(target_arch = "x86_64")]
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

#[cfg(target_arch = "aarch64")]
fn put(byte: u8) {
    crate::pl011::write_raw(byte);
}

/// One byte to COM1 only (not the screen or the network terminal): the serial update protocols answer through this.
pub fn write_byte(byte: u8) {
    put(byte);
}

/// The next received byte, if the UART has one (LSR bit 0).
#[cfg(target_arch = "x86_64")]
pub fn read_byte() -> Option<u8> {
    (PRESENT.load(core::sync::atomic::Ordering::Relaxed) && inb(COM1 + 5) & 1 != 0).then(|| inb(COM1))
}

#[cfg(target_arch = "aarch64")]
pub fn read_byte() -> Option<u8> {
    crate::pl011::read_byte()
}

/// One line at a time: CPUs print whole lines, never interleaved characters. It keeps interrupts off while held, so it
/// covers only the fast consoles and the copy into `TX`; the UART itself (about 87 µs a byte at 115200 baud, 7 ms for an
/// 80-column line) is fed from `TX` afterwards, with interrupts as the caller had them. Holding interrupts off for the
/// whole UART write stalled the xHCI interrupt long enough to run isochronous audio rings dry.
static LOCK: crate::sync::SpinLock<()> = crate::sync::SpinLock::new(());

/// COM1's backlog: bytes are added under LOCK and taken out by whichever CPU holds DRAINING.
const TX_BYTES: usize = 16 * 1024;

struct TxRing {
    data: [u8; TX_BYTES],
    head: usize,
    tail: usize,
    dropped: u64,
}

impl TxRing {
    /// A full ring drops the new byte: the screen, the log and the network terminal still have the text.
    fn push(&mut self, byte: u8) {
        let next = (self.head + 1) % TX_BYTES;
        if next == self.tail {
            self.dropped += 1;
            return;
        }
        self.data[self.head] = byte;
        self.head = next;
    }
}

static mut TX: TxRing = TxRing { data: [0; TX_BYTES], head: 0, tail: 0, dropped: 0 };
/// Set while a CPU writes `TX` to the UART. Another print only appends: it never waits for the drainer, which may be the
/// very thread an interrupt handler interrupted.
static DRAINING: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

pub fn print(arguments: fmt::Arguments) {
    {
        let _guard = LOCK.lock();
        let _ = Serial.write_fmt(arguments);
    }
    drain();
}

/// Writes the backlog to the UART, up to 64 bytes per turn of LOCK so the lock is never held for the UART's pace.
fn drain() {
    use core::sync::atomic::Ordering;
    loop {
        if DRAINING.swap(true, Ordering::Acquire) {
            return;
        }
        loop {
            let mut chunk = [0u8; 64];
            let length = {
                let _guard = LOCK.lock();
                // SAFETY: LOCK held.
                let ring = unsafe { &mut *core::ptr::addr_of_mut!(TX) };
                let mut length = 0;
                while length < chunk.len() && ring.tail != ring.head {
                    chunk[length] = ring.data[ring.tail];
                    ring.tail = (ring.tail + 1) % TX_BYTES;
                    length += 1;
                }
                length
            };
            if length == 0 {
                break;
            }
            for &byte in &chunk[..length] {
                put(byte);
            }
        }
        DRAINING.store(false, Ordering::Release);
        // A print that came in after the last check but before the flag cleared found DRAINING set and left its bytes.
        let pending = {
            let _guard = LOCK.lock();
            // SAFETY: LOCK held.
            let ring = unsafe { &*core::ptr::addr_of!(TX) };
            ring.tail != ring.head
        };
        if !pending {
            return;
        }
    }
}

/// For the fault and panic paths: the CPU that was printing (or draining) may be the one that crashed.
pub fn break_lock() {
    unsafe { LOCK.force_unlock() };
    DRAINING.store(false, core::sync::atomic::Ordering::Release);
}

macro_rules! println {
    () => { $crate::serial::print(format_args!("\n")) };
    ($($argument:tt)*) => { $crate::serial::print(format_args!("{}\n", format_args!($($argument)*))) };
}

pub(crate) use println;

macro_rules! print {
    ($($argument:tt)*) => { $crate::serial::print(format_args!($($argument)*)) };
}
