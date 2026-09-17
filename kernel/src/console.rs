use core::fmt::{self, Write};

use crate::arch::x86::{inb, outb};

const VGA_BUFFER: *mut u16 = 0xb8000 as *mut u16;
const VGA_WIDTH: usize = 80;
const VGA_HEIGHT: usize = 25;
const VGA_COLOR: u8 = 0x07;
const COM1: u16 = 0x3f8;

/// VGA CRTC index/data ports used to read and move the hardware
/// text-mode cursor. Writing characters into the 0xb8000 framebuffer only
/// changes what is drawn; it never moves the blinking caret, which is
/// separate CRTC state. This must be driven explicitly to behave
/// correctly on real hardware (an emulator may be more forgiving about
/// leaving it stale). At boot, the kernel reads this position back
/// (rather than resetting it) so its own messages continue right after
/// whatever the real-mode loader already printed via BIOS teletype.
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
    // Pick up exactly where real-mode's own BIOS-teletype printing left
    // the hardware cursor, rather than resetting to (0, 0): the screen is
    // never cleared at boot (stage1/stage2 messages stay visible for
    // VGA-only viewers), so continuing from the real cursor position
    // means the kernel's own messages append after them instead of
    // overwriting the top of the screen and leaving stale text below.
    unsafe {
        let (row, column) = read_cursor_position();
        ROW = row;
        COLUMN = column;
    }
}

/// Re-reads the hardware cursor position and updates the kernel's own
/// cached ROW/COLUMN to match. `initialize()` only does this once at boot,
/// because normally the kernel is the only thing moving the hardware
/// cursor, so its own cache never goes stale on its own. A `tape` (see
/// kernel/src/shell.rs and tools/tapes/*.S) breaks that assumption: it
/// writes VGA memory and drives the CRTC cursor directly, with no way to
/// tell the kernel's console code it did so. Call this right after a tape
/// returns so the shell's next line of output continues from wherever the
/// tape actually left the cursor, instead of overwriting the tape's own
/// output from a stale, pre-tape position.
pub fn resync_cursor() {
    unsafe {
        let (row, column) = read_cursor_position();
        ROW = row;
        COLUMN = column;
    }
}

/// Reads the hardware text-mode cursor position back from the CRTC, in
/// (row, column) form. Used once at boot so kernel output continues
/// exactly where the real-mode loader's BIOS-teletype printing left off.
unsafe fn read_cursor_position() -> (usize, usize) {
    unsafe {
        outb(CRTC_INDEX_PORT, CRTC_CURSOR_LOCATION_HIGH);
        let high = inb(CRTC_DATA_PORT);
        outb(CRTC_INDEX_PORT, CRTC_CURSOR_LOCATION_LOW);
        let low = inb(CRTC_DATA_PORT);
        let position = (usize::from(high) << 8 | usize::from(low)).min(VGA_WIDTH * VGA_HEIGHT - 1);
        (position / VGA_WIDTH, position % VGA_WIDTH)
    }
}

unsafe fn initialize_serial() -> bool {
    configure_uart(COM1, UART_CLOCK)
}

/// The UART's fixed input clock, already divided down to the maximum
/// standard baud rate: programming a divisor of `UART_CLOCK / baud`
/// selects any other standard rate exactly, with no rounding error, as
/// long as `baud` evenly divides `UART_CLOCK`.
pub const UART_CLOCK: u32 = 115_200;

fn uart_divisor(baud: u32) -> Option<u16> {
    if baud == 0 || UART_CLOCK % baud != 0 {
        return None;
    }
    u16::try_from(UART_CLOCK / baud).ok()
}

/// Programs one UART for 8 data bits, no parity, one stop bit ("8N1") at
/// `baud`, enables its FIFOs, and asserts DTR/RTS so a real null-modem
/// cable reports the link as up. Returns whether the UART responded
/// (mirrors the loopback-style probe the loader already performs for the
/// primary COM1 console link).
fn configure_uart(base: u16, baud: u32) -> bool {
    let Some(divisor) = uart_divisor(baud) else {
        return false;
    };
    unsafe {
        outb(base + 1, 0);
        outb(base + 3, 0x80);
        outb(base, (divisor & 0xff) as u8);
        outb(base + 1, (divisor >> 8) as u8);
        outb(base + 3, 0x03);
        outb(base + 2, 0xc7);
        outb(base + 4, 0x0b);
        inb(base + 5) != 0xff
    }
}

/// BIOS Data Area addresses of each legacy COM port's base I/O address,
/// in index order COM1..COM4. The BIOS fills these in at POST (0 if a
/// port was not found) -- the same lookup DOS and other real-mode
/// software have always used instead of probing I/O space directly.
/// Reading physical address 0x400 is safe here because the kernel runs
/// with a flat, unpaged 32-bit address space, so that physical address is
/// simply mapped at the same virtual address.
const BDA_COM_PORTS: *const u16 = 0x400 as *const u16;

/// Detects which of COM1..COM4 the BIOS found at boot, returning each
/// one's base I/O port address (or `None` if absent).
pub fn detect_ports() -> [Option<u16>; 4] {
    let mut ports = [None; 4];
    for (index, port) in ports.iter_mut().enumerate() {
        let base = unsafe { BDA_COM_PORTS.add(index).read_volatile() };
        if base != 0 {
            *port = Some(base);
        }
    }
    ports
}

/// Maximum number of PCI-discovered serial ports `detect_pci_ports` will
/// report; matched by `shell::PCI_PORT_NAMES`.
pub const MAX_PCI_SERIAL_PORTS: usize = 4;

/// Finds serial ("16550-compatible UART") controllers on PCI/PCIe add-on
/// cards, via PCI configuration-space enumeration (see the `pci` module),
/// returning their I/O base addresses and how many were found. Unlike
/// `detect_ports`, these are never registered in the BIOS Data Area, so
/// they need this separate, explicit lookup.
pub fn detect_pci_ports() -> ([u16; MAX_PCI_SERIAL_PORTS], usize) {
    let mut ports = [0u16; MAX_PCI_SERIAL_PORTS];
    let count = crate::pci::find_serial_ports(&mut ports);
    (ports, count)
}

/// A single UART, independent of the shared COM1 console link above.
/// `term` uses this for COM2/COM3/COM4: unlike COM1 (which doubles as
/// this shell's own optional serial input/output and so must stay at a
/// fixed, predictable baud rate), those ports are never used by the
/// interactive shell, so a `term` session may freely pick any standard
/// baud rate for them.
pub struct Uart {
    base: u16,
}

impl Uart {
    pub const fn new(base: u16) -> Self {
        Uart { base }
    }

    /// Programs this UART for 8N1 at `baud`. Returns `false` if `baud`
    /// does not evenly divide the UART's clock (see `UART_CLOCK`) or if
    /// the UART did not respond to initialization.
    pub fn configure(&self, baud: u32) -> bool {
        configure_uart(self.base, baud)
    }

    pub fn read_byte(&self) -> Option<u8> {
        unsafe {
            if inb(self.base + 5) & 1 != 0 {
                Some(inb(self.base))
            } else {
                None
            }
        }
    }

    pub fn write_byte(&self, byte: u8) {
        unsafe {
            let mut attempts = 100_000;
            while inb(self.base + 5) & 0x20 == 0 && attempts != 0 {
                attempts -= 1;
            }
            if attempts != 0 {
                outb(self.base, byte);
            }
        }
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

/// Single-byte wrapper around `write_serial`, matching the `Fn(u8)` shape
/// `shell::term_loop` uses uniformly across every port it can drive.
pub fn write_serial_byte(byte: u8) {
    write_serial(&[byte]);
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
