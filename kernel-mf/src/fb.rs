//! The framebuffer console: the loader's linear framebuffer (UEFI GOP or a VBE mode on BIOS, both through `BootInfo`) drawn
//! as an 8x16 text grid with the shared Latin-9 font, light grey on black. Every line `println!` writes also lands here, so the
//! screen, COM1 and the network terminal show the same console. Deliberately small: `\n`, `\r`, backspace, scrolling; no ANSI
//! escapes (the network terminal client interprets those). The mapping is uncached for now (write-combining through the PAT is
//! still to do), so scrolling a big screen is slow; fine for a server console.

use fosfor_abi::{Framebuffer, PIXEL_NONE, PIXEL_RGB};

use core::sync::atomic::{AtomicBool, Ordering};

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
    format: u32,
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
        format: framebuffer.format,
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
    if GFX_OPEN.load(Ordering::Relaxed) {
        return;
    }
    if let Some(console) = unsafe { (*core::ptr::addr_of_mut!(CONSOLE)).as_mut() } {
        for byte in text.bytes() {
            console.put(byte);
        }
    }
}

/// Graphics for tapes (`TapeApi::gfx_*`): while a process has the screen open the text console stops drawing (serial and the
/// network terminal go on); `gfx_close` clears the screen and the text starts again at the top. One screen, no rotation.
static GFX_OPEN: AtomicBool = AtomicBool::new(false);

/// Width and height of the pixel screen, or None without one.
pub fn gfx_info() -> Option<(u32, u32)> {
    let console = unsafe { (*core::ptr::addr_of!(CONSOLE)).as_ref() }?;
    Some((console.width as u32, console.height as u32))
}

pub fn gfx_open() -> bool {
    let Some(console) = (unsafe { (*core::ptr::addr_of_mut!(CONSOLE)).as_mut() }) else { return false };
    let background = console.background;
    console.fill(0, console.height, background);
    GFX_OPEN.store(true, Ordering::Relaxed);
    true
}

/// Gives the screen back: cleared, text from the top left.
pub fn gfx_close() {
    if !GFX_OPEN.swap(false, Ordering::Relaxed) {
        return;
    }
    let Some(console) = (unsafe { (*core::ptr::addr_of_mut!(CONSOLE)).as_mut() }) else { return };
    let background = console.background;
    console.fill(0, console.height, background);
    console.column = 0;
    console.row = 0;
}

pub fn gfx_is_open() -> bool {
    GFX_OPEN.load(Ordering::Relaxed)
}

/// 0x00RRGGBB to this framebuffer's pixel format.
fn from_rgb(console: &Console, rgb: u32) -> u32 {
    pixel(console.format, (rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

/// A filled rectangle clipped to the screen; false when the screen is not open.
pub fn gfx_fill(x: u32, y: u32, width: u32, height: u32, rgb: u32) -> bool {
    let Some(console) = (unsafe { (*core::ptr::addr_of_mut!(CONSOLE)).as_mut() }).filter(|_| GFX_OPEN.load(Ordering::Relaxed)) else { return false };
    let value = from_rgb(console, rgb);
    let (x, y) = (x as usize, y as usize);
    if x >= console.width || y >= console.height {
        return true;
    }
    let columns = (width as usize).min(console.width - x);
    let rows = (height as usize).min(console.height - y);
    for row in 0..rows {
        for column in 0..columns {
            unsafe { console.base.add((y + row) * console.stride + x + column).write_volatile(value) };
        }
    }
    true
}

/// Copies `height` rows of `width` 0x00RRGGBB pixels (`stride` pixels apart) to (x, y), clipped on the right and bottom.
///
/// # Safety
/// `source` must be readable for `(height - 1) * stride + width` pixels.
pub unsafe fn gfx_blit(x: u32, y: u32, width: u32, height: u32, source: *const u32, stride: u32) -> bool {
    let Some(console) = (unsafe { (*core::ptr::addr_of_mut!(CONSOLE)).as_mut() }).filter(|_| GFX_OPEN.load(Ordering::Relaxed)) else { return false };
    let (x, y, width, height, stride) = (x as usize, y as usize, width as usize, height as usize, stride as usize);
    if source.is_null() || stride < width || x >= console.width || y >= console.height {
        return false;
    }
    let columns = width.min(console.width - x);
    let rows = height.min(console.height - y);
    for row in 0..rows {
        for column in 0..columns {
            let rgb = unsafe { source.add(row * stride + column).read_unaligned() };
            let value = from_rgb(console, rgb);
            unsafe { console.base.add((y + row) * console.stride + x + column).write_volatile(value) };
        }
    }
    true
}
