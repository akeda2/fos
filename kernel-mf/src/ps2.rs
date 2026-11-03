//! PS/2 keyboard for the server kernel: the 8042 controller behind ports 0x60 (data) and 0x64 (status/command), polled from the
//! service thread. No interrupt is routed (the embedded kernel's `keyboard.rs` uses IRQ 1 through the I/O APIC), so this needs
//! no vector, no CPU choice and nothing from the I/O APIC code: a shell keystroke is not latency-critical, and the 8042 keeps
//! the byte until it is read. The controller is switched to "translate to scancode set 1", the decoder is the shared
//! `fosfor_abi::keymap::Set1Decoder` that USB-less machines already use. The second (mouse) port is left alone, and any mouse
//! byte that shows up is dropped.
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
            continue;
        }
        let key = unsafe { (*core::ptr::addr_of_mut!(DECODER)).feed_layout(byte, layout) };
        if let Some(key) = key {
            crate::shell::feed(key);
        }
    }
}
