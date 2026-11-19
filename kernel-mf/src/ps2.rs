//! PS/2 keyboard for the server kernel: the 8042 controller behind ports 0x60 (data) and 0x64 (status/command), polled from the
//! service thread. No interrupt is routed (the embedded kernel's `keyboard.rs` uses IRQ 1 through the I/O APIC), so this needs
//! no vector, no CPU choice and nothing from the I/O APIC code: a shell keystroke is not latency-critical, and the 8042 keeps
//! the byte until it is read. The controller is switched to "translate to scancode set 1", the decoder is the shared
//! `fosfor_abi::keymap::Set1Decoder` that USB-less machines already use.
//!
//! The second port is a PS/2 mouse when one answers (a laptop's touchpad or TrackPoint is one, in the mouse's own three-byte
//! protocol), polled the same way: its packets move the shared pointer of `kernel_core::usb_pointer`, so tapes see it as
//! touch panel `MOUSE_PANEL` as on the embedded kernel (`kernel/src/keyboard.rs`, whose probe and decoding this follows).
//!
//! Many laptops (the Sandy Bridge `piano`) have only a PS/2 keyboard. A board without an 8042 reads 0xff from the status port
//! and is reported absent; the boot never depends on this.

use crate::cpu;
use fosfor_abi::keymap::{Layout, Set1Decoder};

const DATA: u16 = 0x60;
const STATUS: u16 = 0x64;
const OUTPUT_FULL: u8 = 1 << 0;
const INPUT_FULL: u8 = 1 << 1;
/// Status bit 5: the byte in the output buffer came from the second (mouse) port.
const AUX_DATA: u8 = 1 << 5;

static mut DECODER: Set1Decoder = Set1Decoder::new();
static READY: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// The mouse's packet assembly: three bytes; the first always has bit 3 set, which is how a lost byte is noticed.
static mut PACKET: [u8; 3] = [0; 3];
static mut PACKET_LEN: usize = 0;
/// Decoded packets (dx, dy, button down) waiting for the USB lock, which guards the pointer queue tapes read. The service thread
/// never waits for that lock (`touch monitor` holds it for seconds): whoever reads the pointer delivers them (`deliver_mouse`).
const MOVES: usize = 32;
static PENDING: crate::sync::SpinLock<([(i32, i32, bool); MOVES], usize)> = crate::sync::SpinLock::new(([(0, 0, false); MOVES], 0));

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

fn drain() {
    for _ in 0..16 {
        if cpu::inb(STATUS) & OUTPUT_FULL != 0 {
            cpu::inb(DATA);
        }
    }
}

/// Brings the keyboard port up with its interrupt off. Bounded waits only; says what it found.
pub fn init() {
    if cpu::inb(STATUS) == 0xff {
        println!("PS/2: no 8042 controller.");
        return;
    }
    drain();
    if !command(0x20) || !wait_output_ready() {
        println!("PS/2: the controller did not answer.");
        return;
    }
    let mut configuration = cpu::inb(DATA);
    configuration &= !(1 << 0); // first port's interrupt off: this driver polls
    configuration &= !(1 << 4); // first port clock enabled
    configuration |= 1 << 6; // translate scancode set 2 to set 1
    if !(command(0x60) && wait_input_ready()) {
        println!("PS/2: the controller did not take its configuration.");
        return;
    }
    cpu::outb(DATA, configuration);
    command(0xae); // enable the first port
    if wait_input_ready() {
        cpu::outb(DATA, 0xf4); // keyboard: enable scanning (its 0xfa reply is decoded as nothing)
    }
    READY.store(true, core::sync::atomic::Ordering::Release);
    println!("PS/2: keyboard polled by the service thread.");
    if init_aux() {
        kernel_core::usb_pointer::set_ps2_mouse(true);
        println!("PS/2: mouse on the second port, polled; tapes see it as a touch panel (`touch`).");
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

/// Enables the second port and asks for a mouse: defaults (reports off), then streaming on. Its interrupt stays off (polled).
/// Without a mouse nothing answers and the port is switched off again.
fn init_aux() -> bool {
    drain();
    if !command(0xa8) || !command(0x20) || !wait_output_ready() {
        return false;
    }
    let mut configuration = cpu::inb(DATA);
    configuration &= !(1 << 5); // second port clock enabled
    configuration &= !(1 << 1); // second port's interrupt off
    if !(command(0x60) && wait_input_ready()) {
        return false;
    }
    cpu::outb(DATA, configuration);
    let found = mouse_send(0xf6) && mouse_send(0xf4);
    if !found {
        command(0xa7);
    }
    found
}

/// A mouse byte: a packet is flags (buttons, signs, overflow), dx, dy (up is positive; the screen's Y goes down).
fn mouse_byte(byte: u8) {
    let (packet, length) = unsafe { (&mut *core::ptr::addr_of_mut!(PACKET), &mut *core::ptr::addr_of_mut!(PACKET_LEN)) };
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
    let down = flags & 1 != 0;
    {
        let mut pending = PENDING.lock();
        let (moves, count) = &mut *pending;
        if *count < MOVES {
            moves[*count] = (dx, -dy, down);
            *count += 1;
        } else {
            // Full: fold the move into the last one so no distance is lost.
            let last = &mut moves[MOVES - 1];
            *last = (last.0 + dx, last.1 - dy, down);
        }
    }
    if let Some(_usb) = crate::usb::try_lock() {
        deliver_mouse();
    }
}

/// Moves the shared pointer by the packets that arrived. The caller holds the USB lock.
pub fn deliver_mouse() {
    let mut moves = [(0, 0, false); MOVES];
    let count = {
        let mut pending = PENDING.lock();
        let count = pending.1;
        moves[..count].copy_from_slice(&pending.0[..count]);
        pending.1 = 0;
        count
    };
    for (dx, dy, down) in &moves[..count] {
        kernel_core::usb_pointer::mouse_input(*dx, *dy, *down);
    }
}

/// Reads whatever the controller holds and gives typed bytes to the shell (the service thread calls this).
pub fn poll() {
    if !READY.load(core::sync::atomic::Ordering::Acquire) {
        return;
    }
    let layout = if crate::config::swedish() { Layout::Swedish } else { Layout::Us };
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
        let key = unsafe { (*core::ptr::addr_of_mut!(DECODER)).feed_layout(byte, layout) };
        if let Some(key) = key {
            crate::shell::feed_key(key);
        }
    }
}
