//! A text console on the firmware's linear framebuffer: an 8x16 bitmap font drawn
//! pixel by pixel, with a cursor, line wrap and scrolling.
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
