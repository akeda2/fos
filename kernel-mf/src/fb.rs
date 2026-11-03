//! The framebuffer console: the loader's linear framebuffer (UEFI GOP or a VBE mode on BIOS, both through `BootInfo`) drawn
//! as an 8x16 text grid with the shared Latin-9 font. Every line `println!` writes also lands here, so the screen, COM1 and
//! the network terminal show the same console.
//!
//! - ANSI: the colour and cursor escapes a terminal program uses are understood: SGR `m` (0, 1, 22, 30-37, 39, 40-47, 49,
//!   90-97, 100-107), cursor `H`/`f`/`A`/`B`/`C`/`D`, erase `J` (0, 2) and `K` (0-2). Anything else is swallowed silently.
//!   Serial and the network terminal get the raw bytes; their clients interpret the same escapes.
//! - Write-combining: the framebuffer is mapped through the PAT as write-combining where the CPU has one (`cpu::pat_init`,
//!   `vm::map_write_combining`), else uncached. Reading write-combined memory is as slow as uncached, so the console never
//!   reads the screen: it keeps the text grid in `CELLS` (character and colours) and scrolling redraws from that, once per
//!   `write_str` call however many lines it scrolled. A `sfence` ends each call so the combined writes reach the screen.
//!   Only real hardware shows the speed-up; QEMU's framebuffer is plain RAM.

use fosfor_abi::{Framebuffer, PIXEL_NONE, PIXEL_RGB};

use core::sync::atomic::{AtomicBool, Ordering};

use crate::vm;

#[path = "../../kernel/src/font_data.rs"]
#[allow(dead_code)]
mod font_data;
use font_data::{FONT, FONT_HEIGHT, FONT_WIDTH};

/// The text grid is clamped to this size (4096x2720 pixels); a larger screen leaves its margin black.
const MAX_COLUMNS: usize = 512;
const MAX_ROWS: usize = 170;
/// One cell: the byte in the low half, foreground (0-15) and background (0-15) colour in the high half.
static mut CELLS: [u16; MAX_COLUMNS * MAX_ROWS] = [0; MAX_COLUMNS * MAX_ROWS];

/// The 16 ANSI colours as 0xRRGGBB: the usual dark 0-7, then the bright 8-15 (light grey is colour 7, the default text).
const PALETTE: [u32; 16] = [
    0x000000, 0xaa0000, 0x00aa00, 0xaa5500, 0x0000aa, 0xaa00aa, 0x00aaaa, 0xc0c0c0, 0x555555, 0xff5555, 0x55ff55, 0xffff55, 0x5555ff, 0xff55ff, 0x55ffff,
    0xffffff,
];
const DEFAULT_FOREGROUND: u8 = 7;
const DEFAULT_BACKGROUND: u8 = 0;

#[derive(Clone, Copy, PartialEq)]
enum Escape {
    None,
    /// After ESC.
    Started,
    /// Inside `ESC [`, collecting numbers.
    Csi,
}

struct Console {
    base: *mut u32,
    stride: usize,
    width: usize,
    height: usize,
    columns: usize,
    rows: usize,
    column: usize,
    row: usize,
    foreground: u8,
    background: u8,
    bold: bool,
    palette: [u32; 16],
    /// Cells scrolled: the screen is redrawn from `CELLS` at the end of the write.
    stale: bool,
    escape: Escape,
    parameters: [u16; 4],
    parameter_count: usize,
    format: u32,
    /// VGA text mode (BIOS boot without a linear framebuffer): `base` is the 80x25 text window, one `u16` per cell.
    text: bool,
    /// The pixel console's caret: the cell it is drawn in (`usize::MAX`: none drawn), and `ESC [ ? 25 l` (an editor draws its own).
    caret_at: usize,
    caret_hidden: bool,
    private: bool,
}

static mut CONSOLE: Option<Console> = None;

fn pixel(format: u32, red: u8, green: u8, blue: u8) -> u32 {
    if format == PIXEL_RGB {
        u32::from(red) | u32::from(green) << 8 | u32::from(blue) << 16
    } else {
        u32::from(blue) | u32::from(green) << 8 | u32::from(red) << 16
    }
}

/// Starts the console on the boot framebuffer and clears it. Returns (columns, rows, write-combining), or None when there is no pixel screen
/// (a BIOS boot without one uses `init_text`; headless machines stay serial/network only).
pub fn init(framebuffer: &Framebuffer) -> Option<(usize, usize, bool)> {
    if framebuffer.format >= PIXEL_NONE || framebuffer.base == 0 || framebuffer.width == 0 || framebuffer.height == 0 {
        return None;
    }
    let size = u64::from(framebuffer.stride) * u64::from(framebuffer.height) * 4;
    let combining = vm::map_write_combining(framebuffer.base, size);
    let mut palette = [0u32; 16];
    for (slot, rgb) in palette.iter_mut().zip(PALETTE) {
        *slot = pixel(framebuffer.format, (rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8);
    }
    let mut console = Console {
        base: vm::direct(framebuffer.base) as *mut u32,
        stride: framebuffer.stride as usize,
        width: framebuffer.width as usize,
        height: framebuffer.height as usize,
        columns: (framebuffer.width as usize / FONT_WIDTH).min(MAX_COLUMNS),
        rows: (framebuffer.height as usize / FONT_HEIGHT).min(MAX_ROWS),
        column: 0,
        row: 0,
        foreground: DEFAULT_FOREGROUND,
        background: DEFAULT_BACKGROUND,
        bold: false,
        palette,
        stale: false,
        escape: Escape::None,
        parameters: [0; 4],
        parameter_count: 0,
        format: framebuffer.format,
        text: false,
        caret_at: usize::MAX,
        caret_hidden: false,
        private: false,
    };
    console.clear_all();
    flush_writes();
    let size = (console.columns, console.rows, combining);
    unsafe { *core::ptr::addr_of_mut!(CONSOLE) = Some(console) };
    Some(size)
}

/// Starts the console in the BIOS's VGA text mode (80x25 at 0xB8000, which `vm::init` maps on a BIOS boot without a framebuffer,
/// e.g. after `debug video skip` or Shift at boot): the same grid and escapes as the pixel console, drawn as text cells. Only
/// for a BIOS boot (`runtime_services == 0`); a UEFI machine has no text screen. Bytes above 0x7f show as `?` (the cells use
/// code page 437, the grid Latin-9), and a bright background is shown dark (the top bit would make the cell blink).
pub fn init_text() -> (usize, usize) {
    let mut console = Console {
        base: vm::direct(VGA_TEXT_BASE) as *mut u32,
        stride: TEXT_COLUMNS,
        width: TEXT_COLUMNS,
        height: TEXT_ROWS,
        columns: TEXT_COLUMNS,
        rows: TEXT_ROWS,
        column: 0,
        row: 0,
        foreground: DEFAULT_FOREGROUND,
        background: DEFAULT_BACKGROUND,
        bold: false,
        palette: [0; 16],
        stale: false,
        escape: Escape::None,
        parameters: [0; 4],
        parameter_count: 0,
        format: PIXEL_NONE,
        text: true,
        caret_at: usize::MAX,
        caret_hidden: false,
        private: false,
    };
    console.clear_all();
    // CRTC register 0x0A bit 5 turns the cursor off: clear it, and give it scan lines 13-15 (an underline-to-block shape).
    crate::cpu::outb(0x3d4, 0x0a);
    crate::cpu::outb(0x3d5, 13);
    crate::cpu::outb(0x3d4, 0x0b);
    crate::cpu::outb(0x3d5, 15);
    console.place_cursor();
    unsafe { *core::ptr::addr_of_mut!(CONSOLE) = Some(console) };
    (TEXT_COLUMNS, TEXT_ROWS)
}

const VGA_TEXT_BASE: u64 = 0xb8000;
const TEXT_COLUMNS: usize = 80;
const TEXT_ROWS: usize = 25;
/// ANSI colour number (red = 1, blue = 4) to the VGA attribute's colour number (blue = 1, red = 4).
const ANSI_TO_VGA: [u8; 8] = [0, 4, 2, 6, 1, 5, 3, 7];

fn cells() -> &'static mut [u16; MAX_COLUMNS * MAX_ROWS] {
    unsafe { &mut *core::ptr::addr_of_mut!(CELLS) }
}

impl Console {
    fn fill(&mut self, first_line: usize, last_line: usize, value: u32) {
        if self.text {
            return;
        }
        for line in first_line..last_line {
            for x in 0..self.width {
                unsafe { self.base.add(line * self.stride + x).write_volatile(value) };
            }
        }
    }

    /// The colours cell attributes name: bold lifts a dark foreground to its bright twin.
    fn attribute(&self) -> u16 {
        let foreground = if self.bold && self.foreground < 8 { self.foreground + 8 } else { self.foreground };
        u16::from(foreground) | u16::from(self.background) << 4
    }

    fn blank(&self) -> u16 {
        u16::from(b' ') | self.attribute() << 8
    }

    /// Every cell blank in the current background, the whole screen painted with it.
    fn clear_all(&mut self) {
        let blank = self.blank();
        cells().fill(blank);
        if self.text {
            return self.redraw();
        }
        let background = self.palette[usize::from(self.background)];
        self.fill(0, self.height, background);
        self.stale = false;
    }

    /// A text cell's VGA attribute from the grid's (foreground 0-15, background 0-7: the top bit would blink).
    fn vga_attribute(attribute: u16) -> u16 {
        let colour = |ansi: u16| u16::from(ANSI_TO_VGA[usize::from(ansi & 7)]) | (ansi & 8);
        colour(attribute & 15) | colour((attribute >> 4) & 7) << 4
    }

    /// Puts the hardware cursor (CRTC registers 0x0E and 0x0F) on the next character position.
    fn place_cursor(&self) {
        // CRTC register 0x0A bit 5 switches the hardware cursor off (`ESC [ ? 25 l`).
        crate::cpu::outb(0x3d4, 0x0a);
        crate::cpu::outb(0x3d5, if self.caret_hidden { 0x20 } else { 13 });
        let at = (self.row * self.columns + self.column.min(self.columns - 1)) as u16;
        crate::cpu::outb(0x3d4, 0x0e);
        crate::cpu::outb(0x3d5, (at >> 8) as u8);
        crate::cpu::outb(0x3d4, 0x0f);
        crate::cpu::outb(0x3d5, at as u8);
    }

    /// The pixel console's caret: an underline in the cell the next character goes to. A cell that was redrawn has lost it and
    /// a cell it left must be restored, so the old cell is redrawn and the caret drawn afresh each time.
    fn draw_caret(&mut self) {
        let old = self.caret_at;
        let at = self.row.min(self.rows - 1) * self.columns + self.column.min(self.columns - 1);
        if old < self.rows * self.columns && (old != at || self.caret_hidden) {
            self.draw_cell(old % self.columns, old / self.columns);
        }
        if self.caret_hidden {
            self.caret_at = usize::MAX;
            return;
        }
        let cell = cells()[(at / self.columns) * MAX_COLUMNS + at % self.columns];
        let value = self.palette[usize::from((cell >> 8) & 15)];
        let (x, y) = ((at % self.columns) * FONT_WIDTH, (at / self.columns) * FONT_HEIGHT);
        for line in FONT_HEIGHT - 2..FONT_HEIGHT {
            for bit in 0..FONT_WIDTH {
                unsafe { self.base.add((y + line) * self.stride + x + bit).write_volatile(value) };
            }
        }
        self.caret_at = at;
    }

    fn draw_cell(&mut self, column: usize, row: usize) {
        let cell = cells()[row * MAX_COLUMNS + column];
        if self.text {
            let character = if (cell as u8) < 0x80 { cell as u8 } else { b'?' };
            let value = u16::from(character) | Self::vga_attribute(cell >> 8) << 8;
            unsafe { self.base.cast::<u16>().add(row * self.stride + column).write_volatile(value) };
            return;
        }
        let foreground = self.palette[usize::from((cell >> 8) & 15)];
        let background = self.palette[usize::from((cell >> 12) & 15)];
        for (line, bits) in FONT[usize::from(cell as u8)].iter().enumerate() {
            for bit in 0..FONT_WIDTH {
                let value = if bits & (0x80 >> bit) != 0 { foreground } else { background };
                let offset = (row * FONT_HEIGHT + line) * self.stride + column * FONT_WIDTH + bit;
                unsafe { self.base.add(offset).write_volatile(value) };
            }
        }
    }

    fn set_cell(&mut self, column: usize, row: usize, cell: u16) {
        cells()[row * MAX_COLUMNS + column] = cell;
        if !self.stale {
            self.draw_cell(column, row);
        }
    }

    fn redraw(&mut self) {
        for row in 0..self.rows {
            for column in 0..self.columns {
                self.draw_cell(column, row);
            }
        }
        self.stale = false;
    }

    fn scroll(&mut self) {
        let cells = cells();
        cells.copy_within(MAX_COLUMNS..self.rows * MAX_COLUMNS, 0);
        let blank = self.blank();
        cells[(self.rows - 1) * MAX_COLUMNS..self.rows * MAX_COLUMNS].fill(blank);
        self.stale = true;
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
        match self.escape {
            Escape::Started => {
                self.escape = Escape::None;
                if byte == b'[' {
                    self.escape = Escape::Csi;
                    self.private = false;
                    self.parameters = [0; 4];
                    self.parameter_count = 0;
                }
                return;
            }
            Escape::Csi => return self.csi(byte),
            Escape::None => {}
        }
        match byte {
            0x1b => self.escape = Escape::Started,
            b'\n' => self.newline(),
            b'\r' => self.column = 0,
            0x08 => self.column = self.column.saturating_sub(1),
            // Only Latin-9 glyphs are drawn: UTF-8 continuation bytes would show as stray glyphs.
            0x80..=0xbf => {}
            _ => {
                if self.column >= self.columns {
                    self.newline();
                }
                let cell = u16::from(byte) | self.attribute() << 8;
                let (column, row) = (self.column, self.row);
                self.set_cell(column, row, cell);
                self.column += 1;
            }
        }
    }

    /// One byte of `ESC [ ...`: digits and `;` collect parameters, a final byte (0x40-0x7e) runs the command.
    fn csi(&mut self, byte: u8) {
        match byte {
            b'0'..=b'9' => {
                let slot = self.parameter_count.saturating_sub(1);
                self.parameters[slot] = self.parameters[slot].saturating_mul(10).saturating_add(u16::from(byte - b'0')).min(9999);
                self.parameter_count = self.parameter_count.max(slot + 1);
            }
            b';' => self.parameter_count = (self.parameter_count.max(1) + 1).min(4),
            0x40..=0x7e => {
                self.escape = Escape::None;
                self.command(byte);
            }
            b'?' => self.private = true,
            // Space and the other intermediate bytes of private sequences: keep collecting up to the final byte.
            _ => {}
        }
    }

    /// Parameter `index`, or `default` when it was left out or is 0 (the cursor commands treat 0 as 1).
    fn parameter(&self, index: usize, default: usize) -> usize {
        if index < self.parameter_count && self.parameters[index] != 0 { usize::from(self.parameters[index]) } else { default }
    }

    fn command(&mut self, final_byte: u8) {
        match final_byte {
            b'm' => self.sgr(),
            b'l' | b'h' if self.private => {
                if self.parameters[0] == 25 {
                    self.caret_hidden = final_byte == b'l';
                }
            }
            b'H' | b'f' => {
                self.row = (self.parameter(0, 1) - 1).min(self.rows - 1);
                self.column = (self.parameter(1, 1) - 1).min(self.columns - 1);
            }
            b'A' => self.row = self.row.saturating_sub(self.parameter(0, 1)),
            b'B' => self.row = (self.row + self.parameter(0, 1)).min(self.rows - 1),
            b'C' => self.column = (self.column + self.parameter(0, 1)).min(self.columns - 1),
            b'D' => self.column = self.column.saturating_sub(self.parameter(0, 1)),
            b'J' => match self.parameters[0] {
                0 => {
                    let (row, column) = (self.row, self.column);
                    self.erase(row, column, self.columns);
                    for below in row + 1..self.rows {
                        self.erase(below, 0, self.columns);
                    }
                }
                2 | 3 => self.clear_all(),
                _ => {}
            },
            b'K' => {
                let (row, column) = (self.row, self.column);
                match self.parameters[0] {
                    0 => self.erase(row, column, self.columns),
                    1 => self.erase(row, 0, (column + 1).min(self.columns)),
                    2 => self.erase(row, 0, self.columns),
                    _ => {}
                }
            }
            _ => {}
        }
    }

    /// Blanks cells `from_column..to_column` of `row` in the current background.
    fn erase(&mut self, row: usize, from_column: usize, to_column: usize) {
        let blank = self.blank();
        for column in from_column..to_column {
            self.set_cell(column, row, blank);
        }
    }

    fn sgr(&mut self) {
        for index in 0..self.parameter_count.max(1) {
            match self.parameters[index] {
                0 => {
                    self.foreground = DEFAULT_FOREGROUND;
                    self.background = DEFAULT_BACKGROUND;
                    self.bold = false;
                }
                1 => self.bold = true,
                22 => self.bold = false,
                code @ 30..=37 => self.foreground = (code - 30) as u8,
                39 => self.foreground = DEFAULT_FOREGROUND,
                code @ 40..=47 => self.background = (code - 40) as u8,
                49 => self.background = DEFAULT_BACKGROUND,
                code @ 90..=97 => self.foreground = (code - 90) as u8 + 8,
                code @ 100..=107 => self.background = (code - 100) as u8 + 8,
                _ => {}
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
        if console.stale {
            console.redraw();
        }
        if console.text {
            console.place_cursor();
        } else {
            console.draw_caret();
        }
        flush_writes();
    }
}

/// Makes the write-combining buffers drain, so what was just drawn is on the screen.
fn flush_writes() {
    unsafe { core::arch::asm!("sfence", options(nostack, preserves_flags)) };
}

/// Graphics for tapes (`TapeApi::gfx_*`): while a process has the screen open the text console stops drawing (serial and the
/// network terminal go on); `gfx_close` clears the screen and the text starts again at the top. One screen, no rotation.
static GFX_OPEN: AtomicBool = AtomicBool::new(false);

/// Width and height of the pixel screen, or None without one.
pub fn gfx_info() -> Option<(u32, u32)> {
    let console = unsafe { (*core::ptr::addr_of!(CONSOLE)).as_ref() }.filter(|console| !console.text)?;
    Some((console.width as u32, console.height as u32))
}

pub fn gfx_open() -> bool {
    let Some(console) = (unsafe { (*core::ptr::addr_of_mut!(CONSOLE)).as_mut() }).filter(|console| !console.text) else { return false };
    let black = console.palette[usize::from(DEFAULT_BACKGROUND)];
    console.fill(0, console.height, black);
    console.caret_at = usize::MAX;
    flush_writes();
    GFX_OPEN.store(true, Ordering::Relaxed);
    true
}

/// Gives the screen back: cleared, text from the top left.
pub fn gfx_close() {
    if !GFX_OPEN.swap(false, Ordering::Relaxed) {
        return;
    }
    let Some(console) = (unsafe { (*core::ptr::addr_of_mut!(CONSOLE)).as_mut() }) else { return };
    console.foreground = DEFAULT_FOREGROUND;
    console.background = DEFAULT_BACKGROUND;
    console.bold = false;
    console.escape = Escape::None;
    console.caret_hidden = false;
    console.caret_at = usize::MAX;
    console.clear_all();
    console.column = 0;
    console.row = 0;
    console.draw_caret();
    flush_writes();
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
    flush_writes();
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
    flush_writes();
    true
}
