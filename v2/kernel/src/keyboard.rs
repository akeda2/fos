//! Local keyboards. Every source (PS/2 here, USB HID later) pushes ASCII into one
//! small queue that the main loop drains into the shell.
//!
//! A keyboard is never a boot dependency: each source reports what it found and a
//! missing or silent one is simply not there. A machine with no keyboard at all
//! still boots, and is driven over the network.
//!
//! PS/2: the 8042 controller sits behind ports 0x60 (data) and 0x64 (status/command).
//! It is switched into "translate to scancode set 1" mode so one decoder suffices, and
//! its IRQ 1 is routed through the I/O APIC to `KEYBOARD_VECTOR`.

use core::sync::atomic::{AtomicUsize, Ordering};
use fos_v2_abi::keymap::Set1Decoder;

use crate::{cpu, ioapic};

pub const KEYBOARD_VECTOR: u8 = 0x41;

const DATA: u16 = 0x60;
const STATUS: u16 = 0x64;
const OUTPUT_FULL: u8 = 1 << 0;
const INPUT_FULL: u8 = 1 << 1;

const CAPACITY: usize = 64;
static mut QUEUE: [u8; CAPACITY] = [0; CAPACITY];
static HEAD: AtomicUsize = AtomicUsize::new(0); // next slot to write (interrupt side)
static TAIL: AtomicUsize = AtomicUsize::new(0); // next slot to read (main loop)
static mut DECODER: Set1Decoder = Set1Decoder::new();
static mut BYTES_SEEN: u64 = 0;

/// Adds a key to the queue (called from interrupt context and by other key sources).
pub fn push(byte: u8) {
    let head = HEAD.load(Ordering::Relaxed);
    let next = (head + 1) % CAPACITY;
    if next == TAIL.load(Ordering::Acquire) {
        return; // full: drop the key
    }
    unsafe { (*core::ptr::addr_of_mut!(QUEUE))[head] = byte };
    HEAD.store(next, Ordering::Release);
}

/// The next typed byte, if any.
pub fn pop() -> Option<u8> {
    let tail = TAIL.load(Ordering::Relaxed);
    if tail == HEAD.load(Ordering::Acquire) {
        return None;
    }
    let byte = unsafe { (*core::ptr::addr_of!(QUEUE))[tail] };
    TAIL.store((tail + 1) % CAPACITY, Ordering::Release);
    Some(byte)
}

fn wait_input_ready() -> bool {
    (0..100_000).any(|_| cpu::inb(STATUS) & INPUT_FULL == 0)
}

fn wait_output_ready() -> bool {
    (0..100_000).any(|_| cpu::inb(STATUS) & OUTPUT_FULL != 0)
}

fn command(byte: u8) -> bool {
    wait_input_ready() && {
        cpu::outb(STATUS, byte);
        true
    }
}

/// What `init_ps2` found, for the boot log.
pub enum Ps2 {
    /// No 8042 answered (status reads as 0xff): common on USB-only boards.
    Absent,
    /// The controller is there but the interrupt could not be routed.
    NoInterrupt,
    Ready { gsi: u32 },
}

/// Brings up the PS/2 keyboard. Never blocks for long and never fails the boot.
pub fn init_ps2() -> Ps2 {
    if cpu::inb(STATUS) == 0xff {
        return Ps2::Absent;
    }
    // Drain anything pending, then read the controller's configuration byte.
    for _ in 0..16 {
        if cpu::inb(STATUS) & OUTPUT_FULL != 0 {
            cpu::inb(DATA);
        }
    }
    if !command(0x20) || !wait_output_ready() {
        return Ps2::Absent;
    }
    let mut configuration = cpu::inb(DATA);
    configuration |= 1 << 0; // enable the first port's interrupt
    configuration |= 1 << 6; // translate scancode set 2 to set 1
    configuration &= !(1 << 4); // first port clock enabled
    if !(command(0x60) && wait_input_ready()) {
        return Ps2::Absent;
    }
    cpu::outb(DATA, configuration);
    command(0xae); // enable the first port
    if wait_input_ready() {
        cpu::outb(DATA, 0xf4); // keyboard: enable scanning (reply 0xfa is ignored by the decoder)
    }
    let routed = ioapic::route_isa(1, KEYBOARD_VECTOR);
    // The keyboard's reply to the enable-scanning command (0xfa) arrived before the
    // interrupt was routed. The 8042 holds one byte and raises IRQ 1 only on a new
    // byte, so an unread reply would silence the keyboard for good: read it now.
    for _ in 0..1000 {
        core::hint::spin_loop();
    }
    interrupt();
    match routed {
        Some(gsi) => Ps2::Ready { gsi },
        None => Ps2::NoInterrupt,
    }
}

/// IRQ 1: read the byte, decode, queue. Reading port 0x60 is what clears the request.
pub fn interrupt() {
    while cpu::inb(STATUS) & OUTPUT_FULL != 0 {
        let byte = cpu::inb(DATA);
        unsafe {
            BYTES_SEEN += 1;
            if let Some(key) = (*core::ptr::addr_of_mut!(DECODER)).feed(byte) {
                push(key);
            }
        }
    }
}

pub fn ps2_bytes_seen() -> u64 {
    unsafe { BYTES_SEEN }
}
