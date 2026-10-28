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
use core::sync::atomic::AtomicBool;
use fosfor_abi::keymap::{Layout, Set1Decoder};

use crate::{cpu, ioapic};

pub const KEYBOARD_VECTOR: u8 = 0x41;
/// IRQ 12, the PS/2 mouse (the 8042's second port).
pub const MOUSE_VECTOR: u8 = 0x43;

const DATA: u16 = 0x60;
const STATUS: u16 = 0x64;
const OUTPUT_FULL: u8 = 1 << 0;
const INPUT_FULL: u8 = 1 << 1;
/// Status bit 5: the byte in the output buffer came from the second (mouse) port.
const AUX_DATA: u8 = 1 << 5;

const CAPACITY: usize = 64;
static mut QUEUE: [u8; CAPACITY] = [0; CAPACITY];
static HEAD: AtomicUsize = AtomicUsize::new(0); // next slot to write (interrupt side)
static TAIL: AtomicUsize = AtomicUsize::new(0); // next slot to read (main loop)
static mut DECODER: Set1Decoder = Set1Decoder::new();
static mut BYTES_SEEN: u64 = 0;
static SWEDISH: AtomicBool = AtomicBool::new(false);

/// Chooses what the keys type (`keymap sv|us`, the `settings` file). US until told otherwise: the layout is a property of the
/// keyboard in front of the machine, which the kernel cannot see.
pub fn set_layout(layout: Layout) {
    SWEDISH.store(layout == Layout::Swedish, Ordering::Relaxed);
}

/// The layout a `keymap` word names.
pub fn layout_named(name: &str) -> Option<Layout> {
    match name {
        "us" => Some(Layout::Us),
        "sv" | "se" | "fi" => Some(Layout::Swedish),
        _ => None,
    }
}

pub fn layout() -> Layout {
    if SWEDISH.load(Ordering::Relaxed) { Layout::Swedish } else { Layout::Us }
}

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

/// The next typed byte, if any: the local keyboards' queue first, then the serial port (so a
/// machine with only a serial console is driven the same way).
pub fn pop() -> Option<u8> {
    pop_keyboard().or_else(crate::serial::read_byte).or_else(crate::uart::shell_read)
}

/// Like `pop`, and says whether the byte came from a serial port (a terminal's bytes, where Escape also starts a cursor-key
/// sequence) rather than a local keyboard.
pub fn pop_with_source() -> Option<(u8, bool)> {
    if let Some(byte) = pop_keyboard() {
        return Some((byte, false));
    }
    crate::serial::read_byte().or_else(crate::uart::shell_read).map(|byte| (byte, true))
}

fn pop_keyboard() -> Option<u8> {
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

/// The PS/2 mouse's packet assembly: three bytes, the first always has bit 3 set (which is how a lost byte is noticed).
static mut PACKET: [u8; 3] = [0; 3];
static mut PACKET_LEN: usize = 0;

/// Mouse byte from port 0x60: a packet is flags (buttons, signs, overflow), dx, dy (up is positive, the screen's Y goes down).
fn mouse_byte(byte: u8) {
    unsafe {
        let (packet, length) = (&mut *core::ptr::addr_of_mut!(PACKET), &mut *core::ptr::addr_of_mut!(PACKET_LEN));
        if *length == 0 && byte & 0x08 == 0 {
            return; // out of step: wait for a first byte
        }
        packet[*length] = byte;
        *length += 1;
        if *length < 3 {
            return;
        }
        *length = 0;
        let flags = packet[0];
        if flags & 0xc0 != 0 {
            return; // overflow: the deltas are meaningless
        }
        let dx = i32::from(packet[1]) - if flags & 0x10 != 0 { 256 } else { 0 };
        let dy = i32::from(packet[2]) - if flags & 0x20 != 0 { 256 } else { 0 };
        crate::usb_pointer::mouse_input(dx, -dy, flags & 1 != 0);
    }
}

/// Throws away whatever the 8042 still holds (a keyboard's late acknowledge, say).
fn drain() {
    for _ in 0..16 {
        if cpu::inb(STATUS) & OUTPUT_FULL != 0 {
            cpu::inb(DATA);
        }
    }
}

/// Sends one byte to the mouse (through the 8042's 0xD4 prefix) and waits for its acknowledge (0xFA).
fn mouse_send(byte: u8) -> bool {
    drain();
    if !(command(0xd4) && wait_input_ready()) {
        return false;
    }
    cpu::outb(DATA, byte);
    wait_output_ready() && cpu::inb(DATA) == 0xfa
}

/// Looks for a PS/2 mouse on the 8042's second port (a laptop's touchpad is one, in the mouse's own protocol) and routes IRQ 12.
/// Call after `init_ps2`. True when a mouse answered. A touchpad stays a plain three-byte relative mouse (tap-to-click and
/// scrolling are Synaptics extensions this does not switch on).
pub fn init_aux() -> bool {
    // The keyboard interrupt is already live and would swallow the controller's replies as scancodes.
    let was = cpu::interrupts_save_and_disable();
    let found = probe_aux();
    cpu::interrupts_restore(was);
    found
}

fn probe_aux() -> bool {
    if cpu::inb(STATUS) == 0xff {
        return false;
    }
    drain();
    if !command(0xa8) || !command(0x20) || !wait_output_ready() {
        return false;
    }
    let mut configuration = cpu::inb(DATA);
    configuration &= !(1 << 5); // second port clock enabled
    if !(command(0x60) && wait_input_ready()) {
        return false;
    }
    cpu::outb(DATA, configuration);
    // Defaults (reports off until enabled), then streaming on. Without a mouse nothing answers and the port is switched off again.
    let found = mouse_send(0xf6) && mouse_send(0xf4);
    if !found {
        command(0xa7);
        return false;
    }
    configuration |= 1 << 1; // second port's interrupt
    if !(command(0x60) && wait_input_ready()) {
        return false;
    }
    cpu::outb(DATA, configuration);
    ioapic::route_isa(12, MOUSE_VECTOR).is_some()
}

/// IRQ 1 or IRQ 12: read the byte, decode, queue. Reading port 0x60 is what clears the request.
pub fn interrupt() {
    loop {
        let status = cpu::inb(STATUS);
        if status & OUTPUT_FULL == 0 {
            break;
        }
        let byte = cpu::inb(DATA);
        if status & AUX_DATA != 0 {
            mouse_byte(byte);
            continue;
        }
        unsafe {
            BYTES_SEEN += 1;
            if let Some(key) = (*core::ptr::addr_of_mut!(DECODER)).feed_layout(byte, layout()) {
                push(key);
            }
        }
    }
}
