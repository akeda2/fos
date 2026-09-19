use core::fmt::{self, Write};

use crate::arch::x86::{inb, outb};

const VGA_BUFFER: *mut u16 = 0xb8000 as *mut u16;
const DEFAULT_VGA_COLOR: u8 = 0x07;
const COM1: u16 = 0x3f8;

const SEQUENCER_INDEX_PORT: u16 = 0x3c4;
const SEQUENCER_DATA_PORT: u16 = 0x3c5;
const GRAPHICS_INDEX_PORT: u16 = 0x3ce;
const GRAPHICS_DATA_PORT: u16 = 0x3cf;
const FONT_MEMORY: *mut u8 = 0xa0000 as *mut u8;
const FONT_MAP_SIZE: usize = 0x2000;
const FONT_GLYPH_STRIDE: usize = 32;
const FONT_8X8_MAP: usize = FONT_MAP_SIZE;
// Character-map selectors are not linear addresses: selector 4 selects the
// 0x2000-0x3fff region in plane 2. Select that map for both attribute halves.
const FONT_8X8_CHARACTER_MAP: u8 = 0x24;

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

/// VGA text modes supported by the kernel console. `Experimental132x25`
/// uses non-standard horizontal timing and is intended primarily for QEMU
/// and VGA-compatible hardware that accepts extended text timings.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum VgaMode {
    Text80x25,
    Text80x50,
    Experimental132x25,
}

impl VgaMode {
    pub const fn width(self) -> usize {
        match self {
            Self::Text80x25 | Self::Text80x50 => 80,
            Self::Experimental132x25 => 132,
        }
    }

    pub const fn height(self) -> usize {
        match self {
            Self::Text80x25 | Self::Experimental132x25 => 25,
            Self::Text80x50 => 50,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Text80x25 => "80x25",
            Self::Text80x50 => "80x50",
            Self::Experimental132x25 => "132x25 (experimental)",
        }
    }
}

static mut ROW: usize = 0;
static mut COLUMN: usize = 0;
static mut TERMINAL_COLOR: u8 = DEFAULT_VGA_COLOR;
static mut TERMINAL_HIDDEN: bool = false;
static mut TERMINAL_SAVED_ROW: usize = 0;
static mut TERMINAL_SAVED_COLUMN: usize = 0;
static mut TERMINAL_CURSOR_VISIBLE: bool = true;
static mut TERMINAL_CURSOR_START: u8 = 0;
static mut TERMINAL_CURSOR_END: u8 = 15;
static mut TERMINAL_SCROLL_TOP: usize = 0;
static mut TERMINAL_SCROLL_BOTTOM: usize = 24;
static mut TERMINAL_ALT_ACTIVE: bool = false;
static mut TERMINAL_ALT_BUFFER: [u16; 80 * 50] = [0; 80 * 50];
static mut SERIAL_READY: bool = false;
static mut CURRENT_VGA_MODE: VgaMode = VgaMode::Text80x25;
static mut BASELINE_CRTC: [u8; 8] = [0; 8];
static mut BASELINE_CURSOR_START: u8 = 0;
static mut BASELINE_CURSOR_END: u8 = 15;
static mut BASELINE_CLOCKING_MODE: u8 = 0;
static mut BASELINE_CHARACTER_MAP: u8 = 0;
static mut BASELINE_CRTC_PROTECT: u8 = 0;
static mut FONT_8X8_READY: bool = false;

#[derive(Clone, Copy)]
enum TerminalVgaState {
    Normal,
    Escape,
    EscapeIntermediate,
    Csi,
    Osc,
    OscEscape,
}

static mut TERMINAL_VGA_STATE: TerminalVgaState = TerminalVgaState::Normal;
static mut TERMINAL_CSI_PRIVATE: bool = false;
const TERMINAL_CSI_PARAM_CAPACITY: usize = 8;
static mut TERMINAL_CSI_PARAMS: [u16; TERMINAL_CSI_PARAM_CAPACITY] =
    [0; TERMINAL_CSI_PARAM_CAPACITY];
static mut TERMINAL_CSI_COUNT: usize = 0;
static mut TERMINAL_CSI_VALUE: u16 = 0;
static mut TERMINAL_CSI_HAS_VALUE: bool = false;

/// Bounded queue of every byte this module writes out (VGA text plus
/// whatever reaches `write_serial`, which `Console::write_str`,
/// `write_bytes`, `clear`, and `erase_last_character` all funnel through)
/// while a "network relay" session is active. `shell::term_net` is the
/// only user: it calls `enable_relay` before entering its remote-terminal
/// loop, then drains this queue every iteration and forwards whatever it
/// finds to the connected peer over Ethernet, so the shell's ordinary
/// `println!`/`print!` calls throughout this whole kernel do not need to
/// know or care that a network client is watching -- the same
/// producer/consumer queue shape `keyboard`'s interrupt-fed scancode
/// queue already uses, just for output instead of input. Writing while
/// inactive (the overwhelmingly common case) costs one branch.
const RELAY_QUEUE_LEN: usize = 4096;
static mut RELAY_QUEUE: [u8; RELAY_QUEUE_LEN] = [0; RELAY_QUEUE_LEN];
static mut RELAY_HEAD: usize = 0;
static mut RELAY_TAIL: usize = 0;
static mut RELAY_ACTIVE: bool = false;

/// Starts capturing console output into the relay queue, discarding
/// anything left over from a previous session.
pub fn enable_relay() {
    unsafe {
        RELAY_ACTIVE = true;
        RELAY_HEAD = 0;
        RELAY_TAIL = 0;
    }
}

/// Stops capturing; bytes printed after this call are no longer queued
/// (and, since `enable_relay` resets the queue, are not visible to a
/// later session either).
pub fn disable_relay() {
    unsafe {
        RELAY_ACTIVE = false;
    }
}

/// Whether a `term_net` relay session is currently active. `shell`'s
/// `poll_remote_tape_abort` checks this before touching the network card
/// at all, so a tape started from the local shell (no relay, this is
/// `false`) never pays for polling for a remote "give up" signal that
/// cannot possibly arrive.
pub fn relay_enabled() -> bool {
    unsafe { RELAY_ACTIVE }
}

/// Appends `bytes` to the relay queue if a session is active, silently
/// dropping whatever does not fit rather than blocking or panicking --
/// this is best-effort diagnostic/interactive output, not a guaranteed
/// delivery channel, exactly like every other network feature in this
/// kernel (see `tape_load_net`'s own doc comment on lost frames).
fn relay_push(bytes: &[u8]) {
    unsafe {
        if !RELAY_ACTIVE {
            return;
        }
        for &byte in bytes {
            let next = (RELAY_TAIL + 1) % RELAY_QUEUE_LEN;
            if next == RELAY_HEAD {
                break;
            }
            RELAY_QUEUE[RELAY_TAIL] = byte;
            RELAY_TAIL = next;
        }
    }
}

/// Drains up to `out.len()` queued relay bytes into `out`, returning how
/// many were copied (zero if the queue is currently empty). `term_net`
/// calls this every loop iteration to pick up whatever has been printed
/// since the last drain.
pub fn drain_relay(out: &mut [u8]) -> usize {
    unsafe {
        let mut count = 0;
        while count < out.len() && RELAY_HEAD != RELAY_TAIL {
            out[count] = RELAY_QUEUE[RELAY_HEAD];
            RELAY_HEAD = (RELAY_HEAD + 1) % RELAY_QUEUE_LEN;
            count += 1;
        }
        count
    }
}

pub fn initialize(serial_available: bool) {
    unsafe {
        SERIAL_READY = serial_available && initialize_serial();
        capture_vga_baseline();
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

/// Reads the hardware text-mode cursor position back from the CRTC, in
/// (row, column) form. Used once at boot so kernel output continues
/// exactly where the real-mode loader's BIOS-teletype printing left off.
unsafe fn read_cursor_position() -> (usize, usize) {
    unsafe {
        outb(CRTC_INDEX_PORT, CRTC_CURSOR_LOCATION_HIGH);
        let high = inb(CRTC_DATA_PORT);
        outb(CRTC_INDEX_PORT, CRTC_CURSOR_LOCATION_LOW);
        let low = inb(CRTC_DATA_PORT);
        let mode = CURRENT_VGA_MODE;
        let position =
            (usize::from(high) << 8 | usize::from(low)).min(mode.width() * mode.height() - 1);
        (position / mode.width(), position % mode.width())
    }
}

unsafe fn read_indexed(index_port: u16, data_port: u16, index: u8) -> u8 {
    unsafe {
        outb(index_port, index);
        inb(data_port)
    }
}

unsafe fn write_indexed(index_port: u16, data_port: u16, index: u8, value: u8) {
    unsafe {
        outb(index_port, index);
        outb(data_port, value);
    }
}

unsafe fn read_crtc(index: u8) -> u8 {
    unsafe { read_indexed(CRTC_INDEX_PORT, CRTC_DATA_PORT, index) }
}

unsafe fn write_crtc(index: u8, value: u8) {
    unsafe {
        write_indexed(CRTC_INDEX_PORT, CRTC_DATA_PORT, index, value);
    }
}

unsafe fn capture_vga_baseline() {
    unsafe {
        BASELINE_CRTC = [
            read_crtc(0x00),
            read_crtc(0x01),
            read_crtc(0x02),
            read_crtc(0x03),
            read_crtc(0x04),
            read_crtc(0x05),
            read_crtc(0x09),
            read_crtc(0x13),
        ];
        BASELINE_CURSOR_START = read_crtc(0x0a);
        BASELINE_CURSOR_END = read_crtc(0x0b);
        TERMINAL_CURSOR_START = BASELINE_CURSOR_START & 0x1f;
        TERMINAL_CURSOR_END = BASELINE_CURSOR_END & 0x1f;
        TERMINAL_CURSOR_VISIBLE = BASELINE_CURSOR_START & 0x20 == 0;
        BASELINE_CLOCKING_MODE = read_indexed(SEQUENCER_INDEX_PORT, SEQUENCER_DATA_PORT, 0x01);
        BASELINE_CHARACTER_MAP = read_indexed(SEQUENCER_INDEX_PORT, SEQUENCER_DATA_PORT, 0x03);
        BASELINE_CRTC_PROTECT = read_crtc(0x11);
    }
}

/// Copies the BIOS-provided 8x16 font into a second VGA font map, retaining
/// its first eight scan lines for 8x8 modes. Keeping the original map intact
/// lets the console switch back to 80x25 without needing a baked-in font
/// table, which matters because the flat kernel image has a deliberately
/// small BIOS loader size budget.
unsafe fn prepare_8x8_font() {
    unsafe {
        if FONT_8X8_READY {
            return;
        }

        let saved_seq2 = read_indexed(SEQUENCER_INDEX_PORT, SEQUENCER_DATA_PORT, 0x02);
        let saved_seq4 = read_indexed(SEQUENCER_INDEX_PORT, SEQUENCER_DATA_PORT, 0x04);
        let saved_gfx4 = read_indexed(GRAPHICS_INDEX_PORT, GRAPHICS_DATA_PORT, 0x04);
        let saved_gfx5 = read_indexed(GRAPHICS_INDEX_PORT, GRAPHICS_DATA_PORT, 0x05);
        let saved_gfx6 = read_indexed(GRAPHICS_INDEX_PORT, GRAPHICS_DATA_PORT, 0x06);

        write_indexed(SEQUENCER_INDEX_PORT, SEQUENCER_DATA_PORT, 0x02, 0x04);
        write_indexed(SEQUENCER_INDEX_PORT, SEQUENCER_DATA_PORT, 0x04, 0x07);
        write_indexed(GRAPHICS_INDEX_PORT, GRAPHICS_DATA_PORT, 0x04, 0x02);
        write_indexed(GRAPHICS_INDEX_PORT, GRAPHICS_DATA_PORT, 0x05, 0x00);
        write_indexed(GRAPHICS_INDEX_PORT, GRAPHICS_DATA_PORT, 0x06, 0x04);

        for character in 0..256 {
            let source = FONT_MEMORY.add(character * FONT_GLYPH_STRIDE);
            let destination = FONT_MEMORY.add(FONT_8X8_MAP + character * FONT_GLYPH_STRIDE);
            for row in 0..8 {
                destination
                    .add(row)
                    .write_volatile(source.add(row).read_volatile());
            }
            for row in 8..FONT_GLYPH_STRIDE {
                destination.add(row).write_volatile(0);
            }
        }

        write_indexed(SEQUENCER_INDEX_PORT, SEQUENCER_DATA_PORT, 0x02, saved_seq2);
        write_indexed(SEQUENCER_INDEX_PORT, SEQUENCER_DATA_PORT, 0x04, saved_seq4);
        write_indexed(GRAPHICS_INDEX_PORT, GRAPHICS_DATA_PORT, 0x04, saved_gfx4);
        write_indexed(GRAPHICS_INDEX_PORT, GRAPHICS_DATA_PORT, 0x05, saved_gfx5);
        write_indexed(GRAPHICS_INDEX_PORT, GRAPHICS_DATA_PORT, 0x06, saved_gfx6);
        FONT_8X8_READY = true;
    }
}

unsafe fn restore_80x25_hardware() {
    unsafe {
        write_indexed(
            SEQUENCER_INDEX_PORT,
            SEQUENCER_DATA_PORT,
            0x01,
            BASELINE_CLOCKING_MODE,
        );
        write_indexed(
            SEQUENCER_INDEX_PORT,
            SEQUENCER_DATA_PORT,
            0x03,
            BASELINE_CHARACTER_MAP,
        );
        let protect = BASELINE_CRTC_PROTECT;
        write_crtc(0x11, protect & 0x7f);
        for (index, value) in [
            (0x00, BASELINE_CRTC[0]),
            (0x01, BASELINE_CRTC[1]),
            (0x02, BASELINE_CRTC[2]),
            (0x03, BASELINE_CRTC[3]),
            (0x04, BASELINE_CRTC[4]),
            (0x05, BASELINE_CRTC[5]),
            (0x09, BASELINE_CRTC[6]),
            (0x13, BASELINE_CRTC[7]),
        ] {
            write_crtc(index, value);
        }
        write_crtc(0x11, protect);
    }
}

unsafe fn program_8x8_mode(width: usize) {
    unsafe {
        prepare_8x8_font();
        let clocking_mode = if width == 132 {
            BASELINE_CLOCKING_MODE | 0x01
        } else {
            BASELINE_CLOCKING_MODE
        };
        write_indexed(
            SEQUENCER_INDEX_PORT,
            SEQUENCER_DATA_PORT,
            0x01,
            clocking_mode,
        );
        // Select the copied font map for both attribute-selected font halves.
        write_indexed(
            SEQUENCER_INDEX_PORT,
            SEQUENCER_DATA_PORT,
            0x03,
            FONT_8X8_CHARACTER_MAP,
        );

        let protect = BASELINE_CRTC_PROTECT;
        write_crtc(0x11, protect & 0x7f);
        if width == 132 {
            // This deliberately non-standard timing is useful on QEMU and
            // some SVGA-compatible adapters, but is not guaranteed by
            // baseline VGA hardware or every attached monitor.
            for (index, value) in [
                (0x00, 0x9b), // 160 character clocks per scan line
                (0x01, 0x83), // 132 visible character clocks
                (0x02, 0x84), // horizontal blanking begins after display
                (0x03, 0x92), // extended horizontal blanking interval
                (0x04, 0x8c), // horizontal retrace begins in blanking
                (0x05, 0x96), // horizontal retrace ends
                (0x09, 0x07), // eight scan lines per character
                (0x13, 0x42), // 132 cells * 2 bytes / 4
            ] {
                write_crtc(index, value);
            }
        } else {
            write_crtc(0x09, 0x07);
        }
        write_crtc(0x11, protect);
    }
}

pub fn current_vga_mode() -> VgaMode {
    unsafe { CURRENT_VGA_MODE }
}

/// Changes the local VGA text mode, clears its framebuffer, and resets the
/// kernel's cursor state. Serial output remains independent and continues to
/// use the same raw byte stream as before.
pub fn set_vga_mode(mode: VgaMode) {
    unsafe {
        match mode {
            VgaMode::Text80x25 => restore_80x25_hardware(),
            VgaMode::Text80x50 => program_8x8_mode(80),
            VgaMode::Experimental132x25 => program_8x8_mode(132),
        }
        CURRENT_VGA_MODE = mode;
        ROW = 0;
        COLUMN = 0;
        TERMINAL_VGA_STATE = TerminalVgaState::Normal;
        TERMINAL_COLOR = DEFAULT_VGA_COLOR;
        TERMINAL_HIDDEN = false;
        TERMINAL_SCROLL_TOP = 0;
        TERMINAL_SCROLL_BOTTOM = mode.height() - 1;
        TERMINAL_CURSOR_VISIBLE = true;
        if mode == VgaMode::Text80x25 {
            TERMINAL_CURSOR_START = BASELINE_CURSOR_START & 0x1f;
            TERMINAL_CURSOR_END = BASELINE_CURSOR_END & 0x1f;
        } else {
            TERMINAL_CURSOR_START = 0;
            TERMINAL_CURSOR_END = 7;
        }
        TERMINAL_ALT_ACTIVE = false;
    }
    clear();
    terminal_set_cursor_visible(true);
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
        let mode = CURRENT_VGA_MODE;
        let position = (ROW * mode.width() + COLUMN) as u16;
        outb(CRTC_INDEX_PORT, CRTC_CURSOR_LOCATION_LOW);
        outb(CRTC_DATA_PORT, (position & 0xff) as u8);
        outb(CRTC_INDEX_PORT, CRTC_CURSOR_LOCATION_HIGH);
        outb(CRTC_DATA_PORT, (position >> 8) as u8);
    }
}

pub fn clear() {
    unsafe {
        let mode = CURRENT_VGA_MODE;
        for i in 0..mode.width() * mode.height() {
            VGA_BUFFER
                .add(i)
                .write_volatile(u16::from(DEFAULT_VGA_COLOR) << 8 | u16::from(b' '));
        }
        ROW = 0;
        COLUMN = 0;
        TERMINAL_COLOR = DEFAULT_VGA_COLOR;
        TERMINAL_SCROLL_TOP = 0;
        TERMINAL_SCROLL_BOTTOM = mode.height() - 1;
    }
    update_cursor();
    write_serial(b"\x1b[2J\x1b[H");
}

pub fn erase_last_character() {
    unsafe {
        let mode = CURRENT_VGA_MODE;
        if COLUMN == 0 && ROW > 0 {
            ROW -= 1;
            COLUMN = mode.width();
        }
        if COLUMN > 0 {
            COLUMN -= 1;
            VGA_BUFFER
                .add(ROW * mode.width() + COLUMN)
                .write_volatile(u16::from(DEFAULT_VGA_COLOR) << 8 | u16::from(b' '));
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
    relay_push(bytes);
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
        let mode = CURRENT_VGA_MODE;
        COLUMN = 0;
        ROW += 1;
        if ROW == mode.height() {
            for row in 1..mode.height() {
                for column in 0..mode.width() {
                    let source = VGA_BUFFER.add(row * mode.width() + column).read_volatile();
                    VGA_BUFFER
                        .add((row - 1) * mode.width() + column)
                        .write_volatile(source);
                }
            }
            for column in 0..mode.width() {
                VGA_BUFFER
                    .add((mode.height() - 1) * mode.width() + column)
                    .write_volatile(u16::from(DEFAULT_VGA_COLOR) << 8 | u16::from(b' '));
            }
            ROW = mode.height() - 1;
        }
    }
}

fn write_byte(byte: u8) {
    match byte {
        b'\n' => newline(),
        b'\r' => unsafe { COLUMN = 0 },
        0x20..=0x7e => unsafe {
            let mode = CURRENT_VGA_MODE;
            VGA_BUFFER
                .add(ROW * mode.width() + COLUMN)
                .write_volatile(u16::from(DEFAULT_VGA_COLOR) << 8 | u16::from(byte));
            COLUMN += 1;
            if COLUMN == mode.width() {
                newline();
            }
        },
        _ => {}
    }
    update_cursor();
}

fn terminal_parameter(index: usize, default: u16) -> u16 {
    unsafe {
        if index < TERMINAL_CSI_COUNT {
            TERMINAL_CSI_PARAMS[index]
        } else {
            default
        }
    }
}

fn terminal_set_cursor(row: usize, column: usize) {
    unsafe {
        let mode = CURRENT_VGA_MODE;
        ROW = row.min(mode.height().saturating_sub(1));
        COLUMN = column.min(mode.width().saturating_sub(1));
    }
}

fn terminal_scroll() {
    unsafe {
        let mode = CURRENT_VGA_MODE;
        if TERMINAL_SCROLL_TOP >= mode.height() || TERMINAL_SCROLL_BOTTOM >= mode.height() {
            TERMINAL_SCROLL_TOP = 0;
            TERMINAL_SCROLL_BOTTOM = mode.height() - 1;
        }
        if TERMINAL_SCROLL_TOP >= TERMINAL_SCROLL_BOTTOM {
            ROW = TERMINAL_SCROLL_TOP;
            COLUMN = 0;
            return;
        }
        for row in TERMINAL_SCROLL_TOP + 1..=TERMINAL_SCROLL_BOTTOM {
            for column in 0..mode.width() {
                let source = VGA_BUFFER.add(row * mode.width() + column).read_volatile();
                VGA_BUFFER
                    .add((row - 1) * mode.width() + column)
                    .write_volatile(source);
            }
        }
        for column in 0..mode.width() {
            VGA_BUFFER
                .add(TERMINAL_SCROLL_BOTTOM * mode.width() + column)
                .write_volatile(u16::from(TERMINAL_COLOR) << 8 | u16::from(b' '));
        }
        ROW = TERMINAL_SCROLL_BOTTOM;
        COLUMN = 0;
    }
}

fn terminal_newline() {
    unsafe {
        COLUMN = 0;
        if ROW >= TERMINAL_SCROLL_BOTTOM {
            terminal_scroll();
        } else {
            ROW += 1;
        }
    }
}

fn terminal_write_byte(byte: u8) {
    match byte {
        b'\n' => terminal_newline(),
        b'\r' => unsafe { COLUMN = 0 },
        8 => unsafe {
            if COLUMN > 0 {
                COLUMN -= 1;
            }
        },
        0x20..=0x7e => unsafe {
            let mode = CURRENT_VGA_MODE;
            VGA_BUFFER
                .add(ROW * mode.width() + COLUMN)
                .write_volatile(u16::from(terminal_current_color()) << 8 | u16::from(byte));
            COLUMN += 1;
            if COLUMN == mode.width() {
                terminal_newline();
            }
        },
        _ => {}
    }
}

fn terminal_current_color() -> u8 {
    unsafe {
        if TERMINAL_HIDDEN {
            (TERMINAL_COLOR & 0xf0) | ((TERMINAL_COLOR >> 4) & 0x0f)
        } else {
            TERMINAL_COLOR
        }
    }
}

fn terminal_ansi_color(value: u16) -> u8 {
    match value {
        0 => 0,
        1 => 4,
        2 => 2,
        3 => 6,
        4 => 1,
        5 => 5,
        6 => 3,
        7 => 7,
        8..=15 => (value - 8) as u8,
        16..=231 => {
            let index = (value - 16) as u8;
            let red = index / 36;
            let green = (index / 6) % 6;
            let blue = index % 6;
            let mut result = 0;
            if red >= 3 {
                result |= 4;
            }
            if green >= 3 {
                result |= 2;
            }
            if blue >= 3 {
                result |= 1;
            }
            if red == 5 || green == 5 || blue == 5 {
                result |= 8;
            }
            result
        }
        _ => {
            if value < 244 {
                7
            } else {
                0
            }
        }
    }
}

fn terminal_sgr() {
    unsafe {
        let mut index = 0;
        if TERMINAL_CSI_COUNT == 0 {
            TERMINAL_COLOR = DEFAULT_VGA_COLOR;
            TERMINAL_HIDDEN = false;
            return;
        }
        while index < TERMINAL_CSI_COUNT {
            let value = TERMINAL_CSI_PARAMS[index];
            match value {
                0 => {
                    TERMINAL_COLOR = DEFAULT_VGA_COLOR;
                    TERMINAL_HIDDEN = false;
                }
                1 => TERMINAL_COLOR |= 0x08,
                7 => TERMINAL_COLOR = TERMINAL_COLOR.rotate_left(4),
                22 => TERMINAL_COLOR &= 0xf7,
                8 => TERMINAL_HIDDEN = true,
                28 => TERMINAL_HIDDEN = false,
                27 => TERMINAL_COLOR = TERMINAL_COLOR.rotate_left(4),
                30..=37 => {
                    TERMINAL_COLOR = (TERMINAL_COLOR & 0xf0) | terminal_ansi_color(value - 30)
                }
                39 => TERMINAL_COLOR = (TERMINAL_COLOR & 0xf0) | (DEFAULT_VGA_COLOR & 0x0f),
                40..=47 => {
                    TERMINAL_COLOR =
                        (TERMINAL_COLOR & 0x0f) | (terminal_ansi_color(value - 40) << 4)
                }
                49 => TERMINAL_COLOR = (TERMINAL_COLOR & 0x0f) | (DEFAULT_VGA_COLOR & 0xf0),
                90..=97 => {
                    TERMINAL_COLOR = (TERMINAL_COLOR & 0xf0) | terminal_ansi_color(value - 90 + 8)
                }
                100..=107 => {
                    TERMINAL_COLOR =
                        (TERMINAL_COLOR & 0x0f) | (terminal_ansi_color(value - 100 + 8) << 4)
                }
                38 | 48
                    if index + 2 < TERMINAL_CSI_COUNT && TERMINAL_CSI_PARAMS[index + 1] == 5 =>
                {
                    let color = terminal_ansi_color(TERMINAL_CSI_PARAMS[index + 2]);
                    if value == 38 {
                        TERMINAL_COLOR = (TERMINAL_COLOR & 0xf0) | color;
                    } else {
                        TERMINAL_COLOR = (TERMINAL_COLOR & 0x0f) | (color << 4);
                    }
                    index += 2;
                }
                _ => {}
            }
            index += 1;
        }
    }
}

fn terminal_erase_line(mode: u16) {
    unsafe {
        let current_mode = CURRENT_VGA_MODE;
        let start = match mode {
            1 => 0,
            2 => 0,
            _ => COLUMN,
        };
        let end = match mode {
            1 => COLUMN + 1,
            2 => current_mode.width(),
            _ => current_mode.width(),
        };
        for column in start..end {
            VGA_BUFFER
                .add(ROW * current_mode.width() + column)
                .write_volatile(u16::from(TERMINAL_COLOR) << 8 | u16::from(b' '));
        }
    }
}

fn terminal_erase_display(mode: u16) {
    unsafe {
        let current_mode = CURRENT_VGA_MODE;
        match mode {
            2 | 3 => {
                for row in 0..current_mode.height() {
                    for column in 0..current_mode.width() {
                        VGA_BUFFER
                            .add(row * current_mode.width() + column)
                            .write_volatile(u16::from(TERMINAL_COLOR) << 8 | u16::from(b' '));
                    }
                }
            }
            1 => {
                for row in 0..=ROW {
                    let end = if row == ROW {
                        COLUMN + 1
                    } else {
                        current_mode.width()
                    };
                    for column in 0..end {
                        VGA_BUFFER
                            .add(row * current_mode.width() + column)
                            .write_volatile(u16::from(TERMINAL_COLOR) << 8 | u16::from(b' '));
                    }
                }
            }
            _ => {
                for row in ROW..current_mode.height() {
                    let start = if row == ROW { COLUMN } else { 0 };
                    for column in start..current_mode.width() {
                        VGA_BUFFER
                            .add(row * current_mode.width() + column)
                            .write_volatile(u16::from(TERMINAL_COLOR) << 8 | u16::from(b' '));
                    }
                }
            }
        }
    }
}

fn terminal_enter_alt_screen() {
    unsafe {
        if TERMINAL_ALT_ACTIVE {
            return;
        }
        let mode = CURRENT_VGA_MODE;
        if mode.width() > 80 || mode.height() > 50 {
            return;
        }
        for row in 0..mode.height() {
            for column in 0..mode.width() {
                TERMINAL_ALT_BUFFER[row * mode.width() + column] =
                    VGA_BUFFER.add(row * mode.width() + column).read_volatile();
            }
        }
        TERMINAL_SAVED_ROW = ROW;
        TERMINAL_SAVED_COLUMN = COLUMN;
        TERMINAL_ALT_ACTIVE = true;
        for row in 0..mode.height() {
            for column in 0..mode.width() {
                VGA_BUFFER
                    .add(row * mode.width() + column)
                    .write_volatile(u16::from(TERMINAL_COLOR) << 8 | u16::from(b' '));
            }
        }
        ROW = 0;
        COLUMN = 0;
    }
}

fn terminal_leave_alt_screen() {
    unsafe {
        if !TERMINAL_ALT_ACTIVE {
            return;
        }
        let mode = CURRENT_VGA_MODE;
        if mode.width() > 80 || mode.height() > 50 {
            TERMINAL_ALT_ACTIVE = false;
            return;
        }
        for row in 0..mode.height() {
            for column in 0..mode.width() {
                VGA_BUFFER
                    .add(row * mode.width() + column)
                    .write_volatile(TERMINAL_ALT_BUFFER[row * mode.width() + column]);
            }
        }
        ROW = TERMINAL_SAVED_ROW.min(mode.height().saturating_sub(1));
        COLUMN = TERMINAL_SAVED_COLUMN.min(mode.width().saturating_sub(1));
        TERMINAL_ALT_ACTIVE = false;
    }
}

fn terminal_set_cursor_visible(visible: bool) {
    unsafe {
        TERMINAL_CURSOR_VISIBLE = visible;
        outb(CRTC_INDEX_PORT, 0x0a);
        outb(
            CRTC_DATA_PORT,
            if visible {
                TERMINAL_CURSOR_START & 0x1f
            } else {
                TERMINAL_CURSOR_START | 0x20
            },
        );
        outb(CRTC_INDEX_PORT, 0x0b);
        outb(CRTC_DATA_PORT, TERMINAL_CURSOR_END & 0x1f);
    }
}

fn terminal_handle_csi(final_byte: u8) {
    unsafe {
        let first = terminal_parameter(0, 0);
        match final_byte {
            b'A' => terminal_set_cursor(ROW.saturating_sub(first.max(1) as usize), COLUMN),
            b'B' => terminal_set_cursor(ROW + first.max(1) as usize, COLUMN),
            b'C' => terminal_set_cursor(ROW, COLUMN + first.max(1) as usize),
            b'D' => terminal_set_cursor(ROW, COLUMN.saturating_sub(first.max(1) as usize)),
            b'E' => terminal_set_cursor(ROW + first.max(1) as usize, 0),
            b'F' => terminal_set_cursor(ROW.saturating_sub(first.max(1) as usize), 0),
            b'G' | b'`' => terminal_set_cursor(ROW, first.max(1) as usize - 1),
            b'd' => terminal_set_cursor(first.max(1) as usize - 1, COLUMN),
            b'H' | b'f' => terminal_set_cursor(
                terminal_parameter(0, 1).max(1) as usize - 1,
                terminal_parameter(1, 1).max(1) as usize - 1,
            ),
            b'J' => terminal_erase_display(first),
            b'K' => terminal_erase_line(first),
            b'm' => terminal_sgr(),
            b's' => {
                TERMINAL_SAVED_ROW = ROW;
                TERMINAL_SAVED_COLUMN = COLUMN;
            }
            b'u' => terminal_set_cursor(TERMINAL_SAVED_ROW, TERMINAL_SAVED_COLUMN),
            b'q' => terminal_set_cursor_shape(first),
            b'r' => {
                let mode = CURRENT_VGA_MODE;
                TERMINAL_SCROLL_TOP = terminal_parameter(0, 1).max(1) as usize - 1;
                TERMINAL_SCROLL_BOTTOM =
                    terminal_parameter(1, mode.height() as u16).max(1) as usize - 1;
                TERMINAL_SCROLL_TOP = TERMINAL_SCROLL_TOP.min(mode.height() - 1);
                TERMINAL_SCROLL_BOTTOM = TERMINAL_SCROLL_BOTTOM.min(mode.height() - 1);
                ROW = TERMINAL_SCROLL_TOP;
                COLUMN = 0;
            }
            b'h' if TERMINAL_CSI_PRIVATE && first == 25 => terminal_set_cursor_visible(true),
            b'l' if TERMINAL_CSI_PRIVATE && first == 25 => terminal_set_cursor_visible(false),
            b'h' if TERMINAL_CSI_PRIVATE && (first == 47 || first == 1047 || first == 1049) => {
                terminal_enter_alt_screen()
            }
            b'l' if TERMINAL_CSI_PRIVATE && (first == 47 || first == 1047 || first == 1049) => {
                terminal_leave_alt_screen()
            }
            _ => {}
        }
    }
}

fn terminal_set_cursor_shape(style: u16) {
    unsafe {
        let end = if CURRENT_VGA_MODE == VgaMode::Text80x25 {
            BASELINE_CURSOR_END & 0x1f
        } else {
            7
        };
        let (start, end) = match style {
            3..=6 => (end.saturating_sub(1), end),
            7..=9 => (0, 0),
            _ => (0, end),
        };
        TERMINAL_CURSOR_START = start;
        TERMINAL_CURSOR_END = end;
        terminal_set_cursor_visible(TERMINAL_CURSOR_VISIBLE);
    }
}

/// Consume common ANSI/VT100 escape sequences for the local VGA view while
/// leaving the byte stream itself untouched for serial and `term net`.
fn write_terminal_vga_byte(byte: u8) {
    unsafe {
        match TERMINAL_VGA_STATE {
            TerminalVgaState::Normal => {
                if byte == 0x1b {
                    TERMINAL_VGA_STATE = TerminalVgaState::Escape;
                } else {
                    terminal_write_byte(byte);
                }
            }
            TerminalVgaState::Escape => match byte {
                b'[' => {
                    TERMINAL_CSI_PRIVATE = false;
                    TERMINAL_CSI_COUNT = 0;
                    TERMINAL_CSI_VALUE = 0;
                    TERMINAL_CSI_HAS_VALUE = false;
                    TERMINAL_VGA_STATE = TerminalVgaState::Csi;
                }
                b']' => TERMINAL_VGA_STATE = TerminalVgaState::Osc,
                0x20..=0x2f => TERMINAL_VGA_STATE = TerminalVgaState::EscapeIntermediate,
                0x30..=0x7e => TERMINAL_VGA_STATE = TerminalVgaState::Normal,
                _ => TERMINAL_VGA_STATE = TerminalVgaState::Normal,
            },
            TerminalVgaState::EscapeIntermediate => {
                if (0x30..=0x7e).contains(&byte) {
                    TERMINAL_VGA_STATE = TerminalVgaState::Normal;
                }
            }
            TerminalVgaState::Csi => {
                if byte == b'?' && TERMINAL_CSI_COUNT == 0 && !TERMINAL_CSI_HAS_VALUE {
                    TERMINAL_CSI_PRIVATE = true;
                } else if byte.is_ascii_digit() {
                    TERMINAL_CSI_HAS_VALUE = true;
                    TERMINAL_CSI_VALUE = TERMINAL_CSI_VALUE
                        .saturating_mul(10)
                        .saturating_add(u16::from(byte - b'0'));
                } else if byte == b';' {
                    if TERMINAL_CSI_COUNT < TERMINAL_CSI_PARAM_CAPACITY {
                        TERMINAL_CSI_PARAMS[TERMINAL_CSI_COUNT] = if TERMINAL_CSI_HAS_VALUE {
                            TERMINAL_CSI_VALUE
                        } else {
                            0
                        };
                        TERMINAL_CSI_COUNT += 1;
                    }
                    TERMINAL_CSI_VALUE = 0;
                    TERMINAL_CSI_HAS_VALUE = false;
                } else if (0x40..=0x7e).contains(&byte) {
                    if TERMINAL_CSI_HAS_VALUE || TERMINAL_CSI_COUNT != 0 {
                        if TERMINAL_CSI_COUNT < TERMINAL_CSI_PARAM_CAPACITY {
                            TERMINAL_CSI_PARAMS[TERMINAL_CSI_COUNT] = if TERMINAL_CSI_HAS_VALUE {
                                TERMINAL_CSI_VALUE
                            } else {
                                0
                            };
                            TERMINAL_CSI_COUNT += 1;
                        }
                    }
                    terminal_handle_csi(byte);
                    TERMINAL_VGA_STATE = TerminalVgaState::Normal;
                } else if byte == 0x1b {
                    TERMINAL_VGA_STATE = TerminalVgaState::Escape;
                }
            }
            TerminalVgaState::Osc => match byte {
                0x07 => TERMINAL_VGA_STATE = TerminalVgaState::Normal,
                0x1b => TERMINAL_VGA_STATE = TerminalVgaState::OscEscape,
                _ => {}
            },
            TerminalVgaState::OscEscape => match byte {
                b'\\' => TERMINAL_VGA_STATE = TerminalVgaState::Normal,
                0x1b => {}
                _ => TERMINAL_VGA_STATE = TerminalVgaState::Osc,
            },
        }
    }
    update_cursor();
}

/// Reset terminal parser and presentation state before the shell resumes.
pub fn reset_terminal_output() {
    unsafe {
        terminal_leave_alt_screen();
        TERMINAL_VGA_STATE = TerminalVgaState::Normal;
        TERMINAL_CSI_PRIVATE = false;
        TERMINAL_CSI_COUNT = 0;
        TERMINAL_CSI_VALUE = 0;
        TERMINAL_CSI_HAS_VALUE = false;
        TERMINAL_COLOR = DEFAULT_VGA_COLOR;
        TERMINAL_HIDDEN = false;
        TERMINAL_SCROLL_TOP = 0;
        TERMINAL_SCROLL_BOTTOM = CURRENT_VGA_MODE.height() - 1;
        if CURRENT_VGA_MODE == VgaMode::Text80x25 {
            TERMINAL_CURSOR_START = BASELINE_CURSOR_START & 0x1f;
            TERMINAL_CURSOR_END = BASELINE_CURSOR_END & 0x1f;
        } else {
            TERMINAL_CURSOR_START = 0;
            TERMINAL_CURSOR_END = 7;
        }
        terminal_set_cursor_visible(true);
    }
    update_cursor();
    write_serial(b"\x1b[0m\x1b[?25h\x1b[?1049l\x1b[?2004l\x1b[?7h\x1b[0 q");
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

/// `extern "C" fn(u32)` callback a tape's entry point can call to print
/// one byte through this exact combined VGA+serial(+relay-queue-if-active)
/// path, instead of touching hardware directly (see
/// `tapes/primes.S`, whose header comment explains why this was
/// worth a narrow exception to "a tape cannot call back into the kernel").
/// `shell::tape_run` passes this directly as the tape's one argument.
/// Takes `u32`, not `u8`, so the calling convention at the assembly side
/// is unambiguous (a promoted, zero-extended 32-bit stack slot) rather
/// than relying on how a given Rust/LLVM version happens to pass a `u8`
/// under cdecl.
pub extern "C" fn print_byte(byte: u32) {
    let byte = byte as u8;
    write_terminal_vga_byte(byte);
    write_serial(&[byte]);
}

/// Returns the ANSI cursor position as a packed 1-based `(row, column)`.
/// The high 16 bits contain the row and the low 16 bits contain the column.
/// This is an append-only tape ABI callback used to answer terminal CPR
/// requests without exposing the VGA framebuffer or CRTC implementation.
pub extern "C" fn terminal_position() -> u32 {
    unsafe { (((ROW + 1) as u32) << 16) | (COLUMN + 1) as u32 }
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
