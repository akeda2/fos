//! A text console on the firmware's linear framebuffer: an 8x16 bitmap font drawn
//! pixel by pixel, with a cursor, line wrap and scrolling.
//!
//! On a BIOS boot whose video mode switch was skipped there is no framebuffer, but the screen is still in the BIOS's 80x25 text
//! mode: the same character grid is then painted into VGA text memory (`init_text`), so the machine is not serial-only.
//!
//! The framebuffer is plain memory the display reads, mapped write-combining: stores are
//! fast, *reads are not* (about uncached speed). Scrolling by copying pixels reads the
//! whole screen per line, which made a 16 KiB `dmesg` take over a minute on real hardware.
//! So text lives in a character grid in ordinary RAM; writing text and scrolling only
//! touch the grid, and one repaint at the end of each print draws just the cells that
//! changed since they were last painted. The framebuffer is only ever written.

use crate::font_data::{FONT, FONT_HEIGHT, FONT_WIDTH};
use core::fmt;
use fosfor_abi::{Framebuffer, PIXEL_RGB};

pub struct Console {
    /// VGA text mode instead of pixels: `base` is then the 80x25 array of character/attribute words at 0xB8000.
    text: bool,
    base: *mut u32,
    stride: usize,
    width: usize,
    height: usize,
    format: u32,
    columns: usize,
    rows: usize,
    column: usize,
    row: usize,
    foreground: u32,
    background: u32,
    /// ANSI escape parsing: 0 = none, 1 = after ESC, 2 = inside `ESC [` collecting a number.
    escape: u8,
    parameter: usize,
    /// The parameter before a `;` (`ESC [ row ; column H`), and whether there was one.
    first_parameter: usize,
    has_first: bool,
}

/// The largest text grid kept (256 columns x 100 rows covers up to 2048x1600 at 8x16).
const MAX_CELLS: usize = 256 * 100;
static mut CELLS: [u8; MAX_CELLS] = [b' '; MAX_CELLS];
/// What is currently drawn on screen, cell by cell.
static mut PAINTED: [u8; MAX_CELLS] = [b' '; MAX_CELLS];

static mut CONSOLE: Option<Console> = None;

impl Console {
    /// A pixel value for the given colour in this framebuffer's byte order.
    fn pixel(&self, red: u8, green: u8, blue: u8) -> u32 {
        let (r, g, b) = (u32::from(red), u32::from(green), u32::from(blue));
        if self.format == PIXEL_RGB { r | g << 8 | b << 16 } else { b | g << 8 | r << 16 }
    }

    fn set_pixel(&mut self, x: usize, y: usize, value: u32) {
        unsafe { self.base.add(y * self.stride + x).write_volatile(value) };
    }

    fn fill(&mut self, x: usize, y: usize, width: usize, height: usize, value: u32) {
        for row in y..(y + height).min(self.height) {
            for column in x..(x + width).min(self.width) {
                self.set_pixel(column, row, value);
            }
        }
    }

    fn draw_glyph(&mut self, character: u8, column: usize, row: usize) {
        let glyph = &FONT[usize::from(character)];
        let (x0, y0) = (column * FONT_WIDTH, row * FONT_HEIGHT);
        for (line, bits) in glyph.iter().enumerate() {
            for bit in 0..FONT_WIDTH {
                let on = bits & (0x80 >> bit) != 0;
                let value = if on { self.foreground } else { self.background };
                self.set_pixel(x0 + bit, y0 + line, value);
            }
        }
    }

    /// Moves every text row up by one in the grid and clears the last (nothing is drawn yet).
    fn scroll(&mut self) {
        let columns = self.columns;
        let cells = unsafe { &mut *core::ptr::addr_of_mut!(CELLS) };
        cells.copy_within(columns..self.rows * columns, 0);
        cells[(self.rows - 1) * columns..self.rows * columns].fill(b' ');
    }

    fn newline(&mut self) {
        self.column = 0;
        if self.row + 1 >= self.rows {
            self.scroll();
        } else {
            self.row += 1;
        }
    }

    /// Draws every cell whose character differs from what is on screen.
    fn repaint(&mut self) {
        if self.text {
            return self.repaint_text();
        }
        let columns = self.columns;
        for row in 0..self.rows {
            for column in 0..columns {
                let index = row * columns + column;
                let wanted = unsafe { CELLS[index] };
                if wanted != unsafe { PAINTED[index] } {
                    self.draw_glyph(wanted, column, row);
                    unsafe { PAINTED[index] = wanted };
                }
            }
        }
    }

    /// The text-mode paint: a cell is a 16-bit word, character in the low byte and colours in the high byte, and the hardware
    /// cursor is moved through the CRT controller (index port 0x3D4, data port 0x3D5; registers 0x0E and 0x0F hold the cursor's
    /// cell number, high and low byte). Unlike the pixel framebuffer this memory is cheap to write, but the same
    /// only-what-changed rule keeps a slow bus from being flooded.
    fn repaint_text(&mut self) {
        let columns = self.columns;
        for row in 0..self.rows {
            for column in 0..columns {
                let index = row * columns + column;
                let wanted = unsafe { CELLS[index] };
                if wanted != unsafe { PAINTED[index] } {
                    unsafe { self.base.cast::<u16>().add(index).write_volatile(u16::from(TEXT_ATTRIBUTE) << 8 | u16::from(wanted)) };
                    unsafe { PAINTED[index] = wanted };
                }
            }
        }
        let cell = (self.row.min(self.rows - 1) * columns + self.column.min(columns - 1)) as u16;
        crate::cpu::outb(0x3d4, 0x0e);
        crate::cpu::outb(0x3d5, (cell >> 8) as u8);
        crate::cpu::outb(0x3d4, 0x0f);
        crate::cpu::outb(0x3d5, cell as u8);
    }

    /// The few ANSI sequences a tape's screen editor sends: `ESC [ n C` / `ESC [ n D` (cursor
    /// right/left, stopping at the row's ends), `ESC [ H` / `ESC [ row ; column H` (cursor position,
    /// so a tape can redraw a panel in place), `ESC [ K` (erase to the end of the row) and
    /// `ESC [ 2 J` (clear the screen, cursor home). Anything else is swallowed. A serial or
    /// `term net` terminal gets the same bytes and does these itself.
    fn control_sequence(&mut self, last: u8) {
        let count = self.parameter.max(1);
        match last {
            b'C' => self.column = (self.column + count).min(self.columns - 1),
            b'D' => self.column = self.column.saturating_sub(count),
            b'K' => {
                let start = self.row * self.columns + self.column.min(self.columns - 1);
                let end = (self.row + 1) * self.columns;
                unsafe { (&mut *core::ptr::addr_of_mut!(CELLS))[start..end].fill(b' ') };
            }
            // `ESC [ H` and `ESC [ row ; column H` (1-based): move the cursor, so a tape can redraw a panel in place.
            b'H' | b'f' => {
                let (row, column) = if self.has_first { (self.first_parameter, self.parameter) } else { (self.parameter, 1) };
                self.row = row.max(1).min(self.rows) - 1;
                self.column = column.max(1).min(self.columns) - 1;
            }
            b'J' if self.parameter == 2 => {
                unsafe { (&mut *core::ptr::addr_of_mut!(CELLS))[..self.rows * self.columns].fill(b' ') };
                self.row = 0;
                self.column = 0;
            }
            _ => {}
        }
    }

    fn put(&mut self, character: u8) {
        match self.escape {
            1 => {
                self.escape = if character == b'[' { 2 } else { 0 };
                self.parameter = 0;
                self.has_first = false;
                return;
            }
            2 => {
                match character {
                    b'0'..=b'9' => self.parameter = self.parameter.saturating_mul(10).saturating_add(usize::from(character - b'0')),
                    b';' => {
                        (self.first_parameter, self.has_first) = (self.parameter, true);
                        self.parameter = 0;
                    }
                    last => {
                        self.escape = 0;
                        self.control_sequence(last);
                    }
                }
                return;
            }
            _ => {}
        }
        match character {
            0x1b => self.escape = 1,
            b'\n' => self.newline(),
            b'\r' => self.column = 0,
            // The shell erases a typed character with "\x08 \x08" (back, blank, back): a serial
            // terminal interprets it, so the grid has to as well.
            0x08 => self.column = self.column.saturating_sub(1),
            byte => {
                if self.column >= self.columns {
                    self.newline();
                }
                // Anything outside the font's printable range shows as '?'.
                let shown = if byte == b'\t' || (0x20..0x7f).contains(&byte) { if byte == b'\t' { b' ' } else { byte } } else { b'?' };
                unsafe { CELLS[self.row * self.columns + self.column] = shown };
                self.column += 1;
            }
        }
    }
}

impl fmt::Write for Console {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for byte in text.bytes() {
            self.put(byte);
        }
        self.repaint();
        Ok(())
    }
}

/// Starts the console on `framebuffer` (cleared to the background colour). Does
/// nothing if the firmware gave us no linear framebuffer.
pub fn init(framebuffer: &Framebuffer) -> bool {
    if framebuffer.format >= fosfor_abi::PIXEL_NONE || framebuffer.base == 0 || framebuffer.width == 0 {
        return false;
    }
    let mut console = Console {
        text: false,
        base: framebuffer.base as *mut u32,
        stride: framebuffer.stride as usize,
        width: framebuffer.width as usize,
        height: framebuffer.height as usize,
        format: framebuffer.format,
        columns: framebuffer.width as usize / FONT_WIDTH,
        rows: framebuffer.height as usize / FONT_HEIGHT,
        column: 0,
        row: 0,
        foreground: 0,
        background: 0,
        escape: 0,
        parameter: 0,
        first_parameter: 0,
        has_first: false,
    };
    console.foreground = console.pixel(0xc0, 0xc0, 0xc0);
    console.background = console.pixel(0x00, 0x00, 0x20);
    let (width, height, background) = (console.width, console.height, console.background);
    console.fill(0, 0, width, height, background);
    if console.columns * console.rows > MAX_CELLS {
        return false; // a screen bigger than the grid: serial only
    }
    unsafe {
        (*core::ptr::addr_of_mut!(CELLS)).fill(b' ');
        (*core::ptr::addr_of_mut!(PAINTED)).fill(b' ');
        *core::ptr::addr_of_mut!(CONSOLE) = Some(console);
    }
    true
}

/// Where VGA text memory is, and the colours of its cells: light grey on blue, as the pixel console draws.
pub const VGA_TEXT_BASE: u64 = 0xb8000;
const TEXT_ATTRIBUTE: u8 = 0x17;

/// Starts the console in VGA text mode (80x25 at 0xB8000, which `paging` maps on a BIOS boot without a framebuffer): the
/// screen is cleared, and the cursor made visible. Only for a BIOS boot, where the BIOS left the screen in text mode; a UEFI
/// machine without a framebuffer has no text screen.
pub fn init_text() -> bool {
    let mut console = Console {
        text: true,
        base: VGA_TEXT_BASE as *mut u32,
        stride: 80,
        width: 80,
        height: 25,
        format: 0,
        columns: 80,
        rows: 25,
        column: 0,
        row: 0,
        foreground: 0,
        background: 0,
        escape: 0,
        parameter: 0,
        first_parameter: 0,
        has_first: false,
    };
    unsafe {
        let cells = console.base.cast::<u16>();
        for index in 0..80 * 25 {
            cells.add(index).write_volatile(u16::from(TEXT_ATTRIBUTE) << 8 | u16::from(b' '));
        }
        (*core::ptr::addr_of_mut!(CELLS)).fill(b' ');
        (*core::ptr::addr_of_mut!(PAINTED)).fill(b' ');
        *core::ptr::addr_of_mut!(CONSOLE) = Some(console);
    }
    // CRTC register 0x0A: bit 5 turns the cursor off; clear it, and give the cursor a block-ish shape (scan lines 13-15).
    crate::cpu::outb(0x3d4, 0x0a);
    crate::cpu::outb(0x3d5, 13);
    crate::cpu::outb(0x3d4, 0x0b);
    crate::cpu::outb(0x3d5, 15);
    true
}

/// Text grid size, once the console exists.
pub fn size() -> Option<(usize, usize)> {
    unsafe { (*core::ptr::addr_of!(CONSOLE)).as_ref().map(|console| (console.columns, console.rows)) }
}

/// Draws a filled rectangle in an RGB colour given independently of the
/// framebuffer's byte order: a test that red looks red proves the pixel-format
/// handling.
pub fn fill_rect(x: usize, y: usize, width: usize, height: usize, red: u8, green: u8, blue: u8) {
    if let Some(console) = unsafe { (*core::ptr::addr_of_mut!(CONSOLE)).as_mut() } {
        let value = console.pixel(red, green, blue);
        console.fill(x, y, width, height, value);
    }
}

pub fn write_str(text: &str) {
    use core::fmt::Write;
    if let Some(console) = unsafe { (*core::ptr::addr_of_mut!(CONSOLE)).as_mut() } {
        let _ = console.write_str(text);
    }
}
