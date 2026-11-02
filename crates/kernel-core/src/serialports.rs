//! The serial ports a tape sees: the machine's UARTs (`uart.rs`) numbered first, then the USB serial adapters
//! (`usb_serial.rs`). This is the only place that knows both; neither driver calls the other.

use crate::uart;
use crate::usb_serial;
use fosfor_abi::tape::{SerialInfo, SERIAL_NO_PORT};

/// How many ports there are (numbered from 1).
pub fn count() -> usize {
    uart::count() + usb_serial::count()
}

/// The index among the USB serial ports if `number` is past the UARTs.
fn usb(number: usize) -> Option<usize> {
    number.checked_sub(uart::count() + 1)
}

/// Whether a tape has any port open: the tape's wait must then poll instead of sleeping.
pub fn any_open() -> bool {
    uart::any_open() || usb_serial::any_open()
}

/// Services every open port; cheap when none is open.
pub fn poll() {
    usb_serial::poll();
    uart::poll();
}

pub fn info(number: usize, out: &mut SerialInfo) -> bool {
    match usb(number) {
        Some(index) => usb_serial::info(index, out),
        None => uart::info(number, out),
    }
}

pub fn open(number: usize, baud: u32, format: u32) -> i32 {
    match usb(number) {
        Some(index) => usb_serial::open(index, baud, format),
        None => uart::open(number, baud, format),
    }
}

pub fn close(number: usize) {
    match usb(number) {
        Some(index) => usb_serial::close(index),
        None => uart::close(number),
    }
}

/// A tape that ended leaves nothing open.
pub fn close_all() {
    for number in 1..=count() {
        close(number);
    }
}

pub fn read(number: usize, out: &mut [u8]) -> usize {
    match usb(number) {
        Some(index) => usb_serial::read(index, out),
        None => uart::read(number, out),
    }
}

pub fn write(number: usize, data: &[u8]) -> usize {
    match usb(number) {
        Some(index) => usb_serial::write(index, data),
        None => uart::write(number, data),
    }
}

pub fn lines(number: usize) -> u32 {
    match usb(number) {
        Some(index) => usb_serial::lines(index),
        None => uart::lines(number),
    }
}

pub fn set_lines(number: usize, lines: u32) {
    match usb(number) {
        Some(index) => usb_serial::set_lines(index, lines),
        None => uart::set_lines(number, lines),
    }
}

pub fn set_break(number: usize, on: bool) {
    match usb(number) {
        Some(index) => usb_serial::set_break(index, on),
        None => uart::set_break(number, on),
    }
}

/// Gives port `number` to the shell. 0 on success, else a `SERIAL_*` code. Only a UART can take it: no shell on a USB adapter
/// (yet), and the USB ports appear after the boot-time settings are read.
pub fn open_shell(number: usize, baud: u32) -> i32 {
    if usb(number).is_some() {
        return SERIAL_NO_PORT;
    }
    uart::open_shell(number, baud)
}
