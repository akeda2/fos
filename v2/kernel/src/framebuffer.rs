//! A text console on the firmware's linear framebuffer: an 8x16 bitmap font drawn
//! pixel by pixel, with a cursor, line wrap and scrolling.
//!
//! The framebuffer is plain memory the display reads. M1 draws to it with ordinary
//! stores through an uncached mapping, which is correct but slow to scroll; a later
//! milestone maps it write-combining (PAT) and keeps a shadow copy.

use crate::font_data::{FONT, FONT_HEIGHT, FONT_WIDTH};
use core::fmt;
use fos_v2_abi::{Framebuffer, PIXEL_RGB};

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
}

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

    /// Moves every text row up by one and clears the last.
    fn scroll(&mut self) {
        let row_pixels = FONT_HEIGHT * self.stride;
        let used = (self.rows - 1) * row_pixels;
        unsafe { core::ptr::copy(self.base.add(row_pixels), self.base, used) };
        let background = self.background;
        self.fill(0, (self.rows - 1) * FONT_HEIGHT, self.width, FONT_HEIGHT, background);
    }

    fn newline(&mut self) {
        self.column = 0;
        if self.row + 1 >= self.rows {
            self.scroll();
        } else {
            self.row += 1;
        }
    }

    fn put(&mut self, character: u8) {
        match character {
            b'\n' => self.newline(),
            b'\r' => self.column = 0,
            byte => {
                if self.column >= self.columns {
                    self.newline();
                }
                // Anything outside the font's printable range shows as '?'.
                let shown = if byte == b'\t' || (0x20..0x7f).contains(&byte) { if byte == b'\t' { b' ' } else { byte } } else { b'?' };
                self.draw_glyph(shown, self.column, self.row);
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
        Ok(())
    }
}

/// Starts the console on `framebuffer` (cleared to the background colour). Does
/// nothing if the firmware gave us no linear framebuffer.
pub fn init(framebuffer: &Framebuffer) -> bool {
    if framebuffer.format >= fos_v2_abi::PIXEL_NONE || framebuffer.base == 0 || framebuffer.width == 0 {
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
    };
    console.foreground = console.pixel(0xc0, 0xc0, 0xc0);
    console.background = console.pixel(0x00, 0x00, 0x20);
    let (width, height, background) = (console.width, console.height, console.background);
    console.fill(0, 0, width, height, background);
    unsafe { *core::ptr::addr_of_mut!(CONSOLE) = Some(console) };
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
