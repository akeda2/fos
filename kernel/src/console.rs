use core::fmt::{self, Write};

use crate::arch::x86::{inb, outb};

const VGA_BUFFER: *mut u16 = 0xb8000 as *mut u16;
const VGA_WIDTH: usize = 80;
const VGA_HEIGHT: usize = 25;
const VGA_COLOR: u8 = 0x07;
const COM1: u16 = 0x3f8;

/// VGA CRTC index/data ports used to move the hardware text-mode cursor.
/// Writing characters into the 0xb8000 framebuffer only changes what is
/// drawn; it never moves the blinking caret, which is a separate piece of
/// CRTC state left wherever the firmware/BIOS put it at boot. Real
/// hardware makes this obvious (the caret sits still, e.g. mid-screen,
/// while text appears elsewhere); some emulators are more forgiving about
/// it, but this must be driven explicitly to behave correctly everywhere.
const CRTC_INDEX_PORT: u16 = 0x3d4;
const CRTC_DATA_PORT: u16 = 0x3d5;
const CRTC_CURSOR_LOCATION_HIGH: u8 = 0x0e;
const CRTC_CURSOR_LOCATION_LOW: u8 = 0x0f;

static mut ROW: usize = 0;
static mut COLUMN: usize = 0;
static mut SERIAL_READY: bool = false;

pub fn initialize(serial_available: bool) {
    unsafe {
        SERIAL_READY = serial_available && initialize_serial();
    }
    // Move the hardware caret to (0, 0) immediately: firmware/BIOS may
    // have left it anywhere on screen (real hardware has been observed
    // leaving it mid-screen), and it otherwise would not move until the
    // first character is printed.
    update_cursor();
}

unsafe fn initialize_serial() -> bool {
    unsafe {
        outb(COM1 + 1, 0);
        outb(COM1 + 3, 0x80);
        outb(COM1, 1);
        outb(COM1 + 1, 0);
        outb(COM1 + 3, 0x03);
        outb(COM1 + 2, 0xc7);
        outb(COM1 + 4, 0x0b);
        inb(COM1 + 5) != 0xff
    }
}

/// Moves the hardware text-mode cursor to match `ROW`/`COLUMN`. Called
/// after every operation that changes the current write position, so the
/// blinking caret always tracks where the next character will be drawn.
fn update_cursor() {
    unsafe {
        let position = (ROW * VGA_WIDTH + COLUMN) as u16;
        outb(CRTC_INDEX_PORT, CRTC_CURSOR_LOCATION_LOW);
        outb(CRTC_DATA_PORT, (position & 0xff) as u8);
        outb(CRTC_INDEX_PORT, CRTC_CURSOR_LOCATION_HIGH);
        outb(CRTC_DATA_PORT, (position >> 8) as u8);
    }
}

pub fn clear() {
    unsafe {
        for i in 0..VGA_WIDTH * VGA_HEIGHT {
            VGA_BUFFER.add(i).write_volatile(u16::from(VGA_COLOR) << 8 | u16::from(b' '));
        }
        ROW = 0;
        COLUMN = 0;
    }
    update_cursor();
    write_serial(b"\x1b[2J\x1b[H");
}

pub fn erase_last_character() {
    unsafe {
        if COLUMN == 0 && ROW > 0 {
            ROW -= 1;
            COLUMN = VGA_WIDTH;
        }
        if COLUMN > 0 {
            COLUMN -= 1;
            VGA_BUFFER.add(ROW * VGA_WIDTH + COLUMN)
                .write_volatile(u16::from(VGA_COLOR) << 8 | u16::from(b' '));
        }
    }
    update_cursor();
    write_serial(b"\x08 \x08");
}

pub fn read_serial_byte() -> Option<u8> {
    unsafe {
        if SERIAL_READY && inb(COM1 + 5) & 1 != 0 {
            Some(inb(COM1))
        } else {
            None
        }
    }
}

pub fn write_serial(bytes: &[u8]) {
    unsafe {
        if !SERIAL_READY {
            return;
        }
        for &byte in bytes {
            let mut attempts = 100_000;
            while inb(COM1 + 5) & 0x20 == 0 && attempts != 0 {
                attempts -= 1;
            }
            if attempts == 0 {
                SERIAL_READY = false;
                return;
            }
            outb(COM1, byte);
        }
    }
}

fn newline() {
    unsafe {
        COLUMN = 0;
        ROW += 1;
        if ROW == VGA_HEIGHT {
            for row in 1..VGA_HEIGHT {
                for column in 0..VGA_WIDTH {
                    let source = VGA_BUFFER.add(row * VGA_WIDTH + column).read_volatile();
                    VGA_BUFFER.add((row - 1) * VGA_WIDTH + column).write_volatile(source);
                }
            }
            for column in 0..VGA_WIDTH {
                VGA_BUFFER.add((VGA_HEIGHT - 1) * VGA_WIDTH + column)
                    .write_volatile(u16::from(VGA_COLOR) << 8 | u16::from(b' '));
            }
            ROW = VGA_HEIGHT - 1;
        }
    }
}

fn write_byte(byte: u8) {
    match byte {
        b'\n' => newline(),
        b'\r' => unsafe { COLUMN = 0 },
        0x20..=0x7e => unsafe {
            VGA_BUFFER.add(ROW * VGA_WIDTH + COLUMN)
                .write_volatile(u16::from(VGA_COLOR) << 8 | u16::from(byte));
            COLUMN += 1;
            if COLUMN == VGA_WIDTH {
                newline();
            }
        },
        _ => {}
    }
    update_cursor();
}

struct Console;
impl Write for Console {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        for byte in value.bytes() {
            write_byte(byte);
        }
        write_serial(value.as_bytes());
        Ok(())
    }
}

pub fn print(arguments: fmt::Arguments<'_>) {
    let _ = Console.write_fmt(arguments);
}

pub fn write_bytes(bytes: &[u8]) {
    for &byte in bytes {
        write_byte(byte);
    }
    write_serial(bytes);
}

/// Writes bytes to the VGA text console only, without echoing them back out
/// over COM1. `term`'s raw pass-through loop uses this for bytes arriving
/// from the remote end: COM1 is the link to that remote machine while
/// `term` is active, so mirroring its own incoming bytes back onto itself
/// would corrupt the conversation.
pub fn write_vga_only(bytes: &[u8]) {
    for &byte in bytes {
        write_byte(byte);
    }
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => ($crate::console::print(core::format_args!($($arg)*)));
}

#[macro_export]
macro_rules! println {
    () => ($crate::print!("\n"));
    ($($arg:tt)*) => ($crate::console::print(core::format_args!("{}\n", core::format_args!($($arg)*))));
}
