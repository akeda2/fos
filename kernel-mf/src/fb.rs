//! The framebuffer console: the loader's linear framebuffer (UEFI GOP or a VBE mode on BIOS, both through `BootInfo`) drawn
//! as an 8x16 text grid with the shared Latin-9 font, light grey on black. Every line `println!` writes also lands here, so the
//! screen, COM1 and the network terminal show the same console. Deliberately small: `\n`, `\r`, backspace, scrolling; no ANSI
//! escapes (the network terminal client interprets those). The mapping is uncached for now (write-combining through the PAT is
//! still to do), so scrolling a big screen is slow; fine for a server console.

use fosfor_abi::{Framebuffer, PIXEL_NONE, PIXEL_RGB};

use crate::vm;

#[path = "../../kernel/src/font_data.rs"]
#[allow(dead_code)]
mod font_data;
use font_data::{FONT, FONT_HEIGHT, FONT_WIDTH};

struct Console {
    base: *mut u32,
    stride: usize,
    width: usize,
    height: usize,
    columns: usize,
    rows: usize,
    column: usize,
    row: usize,
    foreground: u32,
    background: u32,
}

static mut CONSOLE: Option<Console> = None;

fn pixel(format: u32, red: u8, green: u8, blue: u8) -> u32 {
    if format == PIXEL_RGB {
        u32::from(red) | u32::from(green) << 8 | u32::from(blue) << 16
    } else {
        u32::from(blue) | u32::from(green) << 8 | u32::from(red) << 16
    }
}

/// Starts the console on the boot framebuffer and clears it. Returns (columns, rows), or None when there is no pixel screen
/// (BIOS text mode and headless machines stay serial/network only).
pub fn init(framebuffer: &Framebuffer) -> Option<(usize, usize)> {
    if framebuffer.format >= PIXEL_NONE || framebuffer.base == 0 || framebuffer.width == 0 || framebuffer.height == 0 {
        return None;
    }
    let size = u64::from(framebuffer.stride) * u64::from(framebuffer.height) * 4;
    vm::map_device(framebuffer.base, size);
    let mut console = Console {
        base: vm::direct(framebuffer.base) as *mut u32,
        stride: framebuffer.stride as usize,
        width: framebuffer.width as usize,
        height: framebuffer.height as usize,
        columns: framebuffer.width as usize / FONT_WIDTH,
        rows: framebuffer.height as usize / FONT_HEIGHT,
        column: 0,
        row: 0,
        foreground: pixel(framebuffer.format, 0xc0, 0xc0, 0xc0),
        background: pixel(framebuffer.format, 0, 0, 0),
    };
    let background = console.background;
    console.fill(0, console.height, background);
    let size = (console.columns, console.rows);
    unsafe { *core::ptr::addr_of_mut!(CONSOLE) = Some(console) };
    Some(size)
}

impl Console {
    fn fill(&mut self, first_line: usize, last_line: usize, value: u32) {
        for line in first_line..last_line {
            for x in 0..self.width {
                unsafe { self.base.add(line * self.stride + x).write_volatile(value) };
            }
        }
    }

    fn glyph(&mut self, byte: u8, column: usize, row: usize) {
        for (line, bits) in FONT[usize::from(byte)].iter().enumerate() {
            for bit in 0..FONT_WIDTH {
                let value = if bits & (0x80 >> bit) != 0 { self.foreground } else { self.background };
                let offset = (row * FONT_HEIGHT + line) * self.stride + column * FONT_WIDTH + bit;
                unsafe { self.base.add(offset).write_volatile(value) };
            }
        }
    }

    fn scroll(&mut self) {
        let text_lines = self.rows * FONT_HEIGHT;
        for line in FONT_HEIGHT..text_lines {
            unsafe { core::ptr::copy_nonoverlapping(self.base.add(line * self.stride), self.base.add((line - FONT_HEIGHT) * self.stride), self.width) };
        }
        let background = self.background;
        self.fill(text_lines - FONT_HEIGHT, text_lines, background);
    }

    fn newline(&mut self) {
        self.column = 0;
        if self.row + 1 < self.rows {
            self.row += 1;
        } else {
            self.scroll();
        }
    }

    fn put(&mut self, byte: u8) {
        match byte {
            b'\n' => self.newline(),
            b'\r' => self.column = 0,
            0x08 => self.column = self.column.saturating_sub(1),
            // Only Latin-9 glyphs are drawn: UTF-8 continuation bytes would show as stray glyphs.
            0x80..=0xbf => {}
            _ => {
                if self.column >= self.columns {
                    self.newline();
                }
                let (column, row) = (self.column, self.row);
                self.glyph(byte, column, row);
                self.column += 1;
            }
        }
    }
}

/// Called by the console for every piece of text, with the serial lock held; a no-op until `init` succeeded.
pub fn write_str(text: &str) {
    if let Some(console) = unsafe { (*core::ptr::addr_of_mut!(CONSOLE)).as_mut() } {
        for byte in text.bytes() {
            console.put(byte);
        }
    }
}
