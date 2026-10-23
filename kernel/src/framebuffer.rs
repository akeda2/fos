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
    /// ANSI escape parsing: 0 = none, 1 = after ESC, 2 = inside `ESC [` collecting a number, 3 = inside an OSC/DCS string,
    /// 4 = ESC seen inside such a string, 5 = after `ESC (` and the like (one byte left to swallow).
    escape: u8,
    parameter: usize,
    /// The parameter before a `;` (`ESC [ row ; column H`), and whether there was one.
    first_parameter: usize,
    has_first: bool,
    /// The CSI in progress has a private marker (`?`, `>`, `<`, `=`): not ours, swallowed whole.
    csi_private: bool,
    /// A UTF-8 sequence in progress: the code point so far and how many continuation bytes are still due.
    utf8_code: u32,
    utf8_left: u8,
}

/// The largest text grid kept (256 columns x 100 rows covers up to 2048x1600 at 8x16).
const MAX_CELLS: usize = 256 * 100;
static mut CELLS: [u8; MAX_CELLS] = [b' '; MAX_CELLS];
/// What is currently drawn on screen, cell by cell.
static mut PAINTED: [u8; MAX_CELLS] = [b' '; MAX_CELLS];

/// The cell index the pixel console's caret is drawn in (`usize::MAX`: none yet).
static mut CARET_AT: usize = usize::MAX;

static mut CONSOLE: Option<Console> = None;

/// A tape has the screen for graphics (`gfx_open`): the console keeps its text grid but draws nothing.
static mut GFX_OPEN: bool = false;

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
        if unsafe { GFX_OPEN } {
            return; // a tape owns the pixels; the grid keeps changing and is drawn again by `gfx_close`
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
        // The caret: an underline in the cell the next character goes to. A cell the loop above just redrew has lost
        // it, and a cell the caret left must be restored, so the old cell is redrawn and the caret drawn afresh each time.
        let at = self.row.min(self.rows - 1) * columns + self.column.min(columns - 1);
        let old = unsafe { CARET_AT };
        if old != at && old < self.rows * columns {
            self.draw_glyph(unsafe { CELLS[old] }, old % columns, old / columns);
        }
        let (x, y) = ((at % columns) * FONT_WIDTH, (at / columns) * FONT_HEIGHT);
        let value = self.foreground;
        self.fill(x, y + FONT_HEIGHT - 2, FONT_WIDTH, 2, value);
        unsafe { CARET_AT = at };
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
        #[cfg(target_arch = "x86_64")]
        {
            crate::cpu::outb(0x3d4, 0x0e);
            crate::cpu::outb(0x3d5, (cell >> 8) as u8);
            crate::cpu::outb(0x3d4, 0x0f);
            crate::cpu::outb(0x3d5, cell as u8);
        }
        #[cfg(not(target_arch = "x86_64"))]
        let _ = cell;
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
                self.parameter = 0;
                self.has_first = false;
                self.csi_private = false;
                self.escape = match character {
                    b'[' => 2,
                    // OSC (window title, shell integration), DCS, SOS, PM, APC: a string up to BEL or ESC \.
                    b']' | b'P' | b'X' | b'^' | b'_' => 3,
                    // `ESC ( B` and friends: one more byte names the character set.
                    0x20..=0x2f => 5,
                    0x1b => 1,
                    _ => 0,
                };
                return;
            }
            2 => {
                match character {
                    b'0'..=b'9' => self.parameter = self.parameter.saturating_mul(10).saturating_add(usize::from(character - b'0')),
                    b';' => {
                        (self.first_parameter, self.has_first) = (self.parameter, true);
                        self.parameter = 0;
                    }
                    b':'..=b'?' => self.csi_private = true,
                    0x20..=0x2f | 0x00..=0x1a | 0x1c..=0x1f => {}
                    0x1b => self.escape = 1,
                    last => {
                        self.escape = 0;
                        if !self.csi_private {
                            self.control_sequence(last);
                        }
                    }
                }
                return;
            }
            3 => {
                match character {
                    0x07 => self.escape = 0,
                    0x1b => self.escape = 4,
                    _ => {}
                }
                return;
            }
            4 => {
                // ESC \ ends the string; any other byte after ESC starts a new sequence.
                self.escape = if character == b'\\' { 0 } else { 1 };
                if self.escape == 1 {
                    return self.put(character);
                }
                return;
            }
            5 => {
                self.escape = 0;
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
            // A tab moves to the next multiple of 8 (what `ls` on a Linux host over telnet relies on for its columns).
            b'\t' if self.utf8_left == 0 => {
                for _ in 0..if self.column < self.columns { 8 - self.column % 8 } else { 0 } {
                    self.put_cell(b' ');
                }
            }
            byte => self.put_text_byte(byte),
        }
    }

    /// Text bytes: ASCII directly, anything above 0x7f through a UTF-8 decoder (the shell, `telnet` and file names send
    /// UTF-8). A sequence that is broken, or whose character the font lacks, shows as '?'.
    fn put_text_byte(&mut self, byte: u8) {
        if self.utf8_left > 0 {
            if byte & 0xc0 == 0x80 {
                self.utf8_code = self.utf8_code << 6 | u32::from(byte & 0x3f);
                self.utf8_left -= 1;
                if self.utf8_left == 0 {
                    let cell = self.cell_for(self.utf8_code);
                    self.put_cell(cell);
                }
                return;
            }
            // The sequence was cut short: show it as one '?' and read this byte afresh.
            self.utf8_left = 0;
            self.put_cell(b'?');
        }
        match byte {
            0xc2..=0xdf => (self.utf8_code, self.utf8_left) = (u32::from(byte & 0x1f), 1),
            0xe0..=0xef => (self.utf8_code, self.utf8_left) = (u32::from(byte & 0x0f), 2),
            0xf0..=0xf4 => (self.utf8_code, self.utf8_left) = (u32::from(byte & 0x07), 3),
            b'\t' => self.put_cell(b' '),
            // Other control characters (BEL, SO/SI, NUL after a telnet CR, DEL) draw nothing.
            0x00..=0x1f | 0x7f => {}
            0x20..=0x7e => self.put_cell(byte),
            _ => self.put_cell(b'?'),
        }
    }

    /// The cell value that draws `code point` in this console's font, or '?'. The pixel font is ISO 8859-15 (Latin-9: Latin-1
    /// with the euro sign and the letters Š š Ž ž Œ œ Ÿ in place of eight rarely used symbols); VGA text memory uses the
    /// code page 437 of the video BIOS, which has the Nordic and common Western letters but no Ø, Þ and the like.
    fn cell_for(&self, code: u32) -> u8 {
        match code {
            0x20..=0x7e => code as u8,
            0xa0 => b' ',
            0x2018 | 0x2019 => b'\'',
            0x201c | 0x201d => b'"',
            0x2013 | 0x2014 => b'-',
            _ if self.text => cp437_for(code),
            0xa1..=0xff if !matches!(code, 0xa4 | 0xa6 | 0xa8 | 0xb4 | 0xb8 | 0xbc..=0xbe) => code as u8,
            0x20ac => 0xa4,
            0x0160 => 0xa6,
            0x0161 => 0xa8,
            0x017d => 0xb4,
            0x017e => 0xb8,
            0x0152 => 0xbc,
            0x0153 => 0xbd,
            0x0178 => 0xbe,
            _ => b'?',
        }
    }

    fn put_cell(&mut self, cell: u8) {
        if self.column >= self.columns {
            self.newline();
        }
        unsafe { CELLS[self.row * self.columns + self.column] = cell };
        self.column += 1;
    }
}

/// Code page 437 position of a Unicode letter or symbol, for VGA text mode.
fn cp437_for(code: u32) -> u8 {
    match char::from_u32(code).unwrap_or('?') {
        'ü' => 0x81, 'é' => 0x82, 'â' => 0x83, 'ä' => 0x84, 'à' => 0x85, 'å' => 0x86, 'ç' => 0x87, 'ê' => 0x88,
        'ë' => 0x89, 'è' => 0x8a, 'ï' => 0x8b, 'î' => 0x8c, 'ì' => 0x8d, 'Ä' => 0x8e, 'Å' => 0x8f, 'É' => 0x90,
        'æ' => 0x91, 'Æ' => 0x92, 'ô' => 0x93, 'ö' => 0x94, 'ò' => 0x95, 'û' => 0x96, 'ù' => 0x97, 'ÿ' => 0x98,
        'Ö' => 0x99, 'Ü' => 0x9a, '¢' => 0x9b, '£' => 0x9c, '¥' => 0x9d, 'á' => 0xa0, 'í' => 0xa1, 'ó' => 0xa2,
        'ú' => 0xa3, 'ñ' => 0xa4, 'Ñ' => 0xa5, '¿' => 0xa8, 'ß' => 0xe1, 'µ' => 0xe6, '°' => 0xf8, '±' => 0xf1,
        '·' => 0xfa, '²' => 0xfd,
        _ => b'?',
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
        csi_private: false,
        utf8_code: 0,
        utf8_left: 0,
    };
    console.foreground = console.pixel(0xc0, 0xc0, 0xc0);
    console.background = console.pixel(0x00, 0x00, 0x00);
    let (width, height, background) = (console.width, console.height, console.background);
    console.fill(0, 0, width, height, background);
    if console.columns * console.rows > MAX_CELLS {
        return false; // a screen bigger than the grid: serial only
    }
    unsafe {
        (*core::ptr::addr_of_mut!(CELLS)).fill(b' ');
        (*core::ptr::addr_of_mut!(PAINTED)).fill(b' ');
        CARET_AT = usize::MAX; // a console started again (the Pi's DSI one) must not erase the old caret on the new screen
        *core::ptr::addr_of_mut!(CONSOLE) = Some(console);
    }
    true
}

/// Where VGA text memory is, and the colours of its cells: light grey on black, as the pixel console draws.
pub const VGA_TEXT_BASE: u64 = 0xb8000;
const TEXT_ATTRIBUTE: u8 = 0x07;

/// Starts the console in VGA text mode (80x25 at 0xB8000, which `paging` maps on a BIOS boot without a framebuffer): the
/// screen is cleared, and the cursor made visible. Only for a BIOS boot, where the BIOS left the screen in text mode; a UEFI
/// machine without a framebuffer has no text screen.
pub fn init_text() -> bool {
    let console = Console {
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
        csi_private: false,
        utf8_code: 0,
        utf8_left: 0,
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
    #[cfg(target_arch = "x86_64")]
    {
        crate::cpu::outb(0x3d4, 0x0a);
        crate::cpu::outb(0x3d5, 13);
        crate::cpu::outb(0x3d4, 0x0b);
        crate::cpu::outb(0x3d5, 15);
    }
    true
}

/// Text grid size, once the console exists.
/// `#rrggbb` (or `rrggbb`) as RGB.
fn hex_color(name: &str) -> Option<(u8, u8, u8)> {
    let digits = name.strip_prefix('#').unwrap_or(name);
    if digits.len() != 6 {
        return None;
    }
    let value = u32::from_str_radix(digits, 16).ok()?;
    Some(((value >> 16) as u8, (value >> 8) as u8, value as u8))
}

/// The colours the `foreground` setting accepts (a name, or #rrggbb), as RGB.
fn foreground_color(name: &str) -> Option<(u8, u8, u8)> {
    Some(match name {
        "gray" | "grey" => (0xc0, 0xc0, 0xc0),
        "green" => (0x33, 0xff, 0x33),
        "amber" => (0xff, 0xb0, 0x00),
        "black" => (0x00, 0x00, 0x00),
        _ => return hex_color(name),
    })
}

/// The colours the `background` setting accepts (a name, or #rrggbb), as RGB.
fn background_color(name: &str) -> Option<(u8, u8, u8)> {
    Some(match name {
        "black" => (0x00, 0x00, 0x00),
        "blue" => (0x00, 0x00, 0xaa),
        "gray" | "grey" => (0xc0, 0xc0, 0xc0),
        _ => return hex_color(name),
    })
}

/// Changes the pixel console's colours (`None` keeps the current one) and redraws the whole
/// screen, since `repaint` only draws cells that changed. Returns false for an unknown name or when there is no pixel
/// console: the VGA text screen keeps its fixed grey on black.
pub fn set_colors(foreground: Option<&str>, background: Option<&str>) -> bool {
    let Some(console) = (unsafe { (*core::ptr::addr_of_mut!(CONSOLE)).as_mut() }) else { return false };
    if console.text {
        return false;
    }
    let (Some(foreground), Some(background)) = (
        foreground.map_or(Some(None), |name| foreground_color(name).map(Some)),
        background.map_or(Some(None), |name| background_color(name).map(Some)),
    ) else {
        return false;
    };
    if let Some((red, green, blue)) = foreground {
        console.foreground = console.pixel(red, green, blue);
    }
    if let Some((red, green, blue)) = background {
        console.background = console.pixel(red, green, blue);
    }
    let (width, height, background) = (console.width, console.height, console.background);
    console.fill(0, 0, width, height, background);
    unsafe { (*core::ptr::addr_of_mut!(PAINTED)).fill(b' ') };
    console.repaint();
    true
}

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

/// What a tape sees of the screen: width, height in pixels and whether there is one (a pixel framebuffer; the BIOS text screen
/// has none). Tape pixels are always 0x00RRGGBB; the kernel converts to the screen's byte order.
pub fn gfx_info() -> Option<(u32, u32)> {
    let console = unsafe { (*core::ptr::addr_of!(CONSOLE)).as_ref() }?;
    if console.text {
        return None;
    }
    // A rotated screen is seen by tapes as it is on the monitor: width and height swap.
    Some(match rotation(0) {
        Some(turn) if turn.swaps() => (console.height as u32, console.width as u32),
        _ => (console.width as u32, console.height as u32),
    })
}

/// Which way the monitor is turned (a portrait mount): tapes draw on an upright logical screen and the kernel rotates it
/// onto the framebuffer. The text console is not rotated.
#[derive(Clone, Copy, PartialEq)]
pub enum Rotate {
    /// Clockwise: the logical top-left corner lands at the top right of the framebuffer.
    Right,
    /// Anti-clockwise: the logical top-left corner lands at the bottom left.
    Left,
    /// Upside down (180 degrees): the logical top-left corner lands at the bottom right; width and height stay.
    Flip,
}

impl Rotate {
    /// `r`/`right` or `l`/`left`.
    pub fn parse(word: &str) -> Option<Rotate> {
        match word {
            "r" | "right" => Some(Rotate::Right),
            "l" | "left" => Some(Rotate::Left),
            "f" | "flip" | "180" => Some(Rotate::Flip),
            _ => None,
        }
    }

    /// A quarter turn swaps the logical screen's width and height.
    pub fn swaps(self) -> bool {
        self != Rotate::Flip
    }

    /// The framebuffer pixel (of a `screen_width` x `screen_height` one) under logical pixel (x, y).
    fn map(self, screen_width: usize, screen_height: usize, x: usize, y: usize) -> (usize, usize) {
        match self {
            Rotate::Right => (screen_width - 1 - y, x),
            Rotate::Left => (y, screen_height - 1 - x),
            Rotate::Flip => (screen_width - 1 - x, screen_height - 1 - y),
        }
    }

    /// The logical screen's size on a `screen_width` x `screen_height` framebuffer.
    fn logical(self, screen_width: usize, screen_height: usize) -> (usize, usize) {
        if self.swaps() { (screen_height, screen_width) } else { (screen_width, screen_height) }
    }
}

/// Rotation per physical screen (0: the console, 1: the Pi's second HDMI output).
static mut ROTATE_DEFAULT: [Option<Rotate>; 2] = [None; 2];
static mut ROTATE_NOW: [Option<Rotate>; 2] = [None; 2];

/// The `rotate` (screen 0) and `rotate1` (screen 1) settings: the rotation every tape gets unless a `run` line says otherwise.
pub fn rotate_default(phys: usize, mode: Option<Rotate>) {
    if phys > 1 {
        return;
    }
    unsafe {
        (*core::ptr::addr_of_mut!(ROTATE_DEFAULT))[phys] = mode;
        (*core::ptr::addr_of_mut!(ROTATE_NOW))[phys] = mode;
    }
}

/// A `run` line's own `rotate=` for physical screen `phys` (`None`: the setting's); `rotate_reset` goes back to the settings
/// afterwards.
pub fn rotate_run(phys: usize, mode: Option<Rotate>) {
    if phys > 1 {
        return;
    }
    unsafe { (*core::ptr::addr_of_mut!(ROTATE_NOW))[phys] = mode.or((*core::ptr::addr_of!(ROTATE_DEFAULT))[phys]) };
}

pub fn rotate_reset() {
    unsafe { *core::ptr::addr_of_mut!(ROTATE_NOW) = *core::ptr::addr_of!(ROTATE_DEFAULT) };
}

pub fn rotation(phys: usize) -> Option<Rotate> {
    unsafe { (*core::ptr::addr_of!(ROTATE_NOW))[phys] }
}

/// A rectangle of the logical (rotated) screen as a rectangle of the physical screen (`screen_width` x `screen_height`),
/// clipped to the logical screen (height x width). Returns (x, y, width, height); `None` when it is outside.
fn rotate_rect(turn: Rotate, screen_width: u32, screen_height: u32, x: u32, y: u32, width: u32, height: u32) -> Option<(u32, u32, u32, u32)> {
    let (logical_width, logical_height) = turn.logical(screen_width as usize, screen_height as usize);
    if x as usize >= logical_width || y as usize >= logical_height {
        return None;
    }
    let (width, height) = (width.min(logical_width as u32 - x), height.min(logical_height as u32 - y));
    Some(match turn {
        Rotate::Right => (screen_width - y - height, x, height, width),
        Rotate::Left => (y, screen_height - x - width, height, width),
        Rotate::Flip => (screen_width - x - width, screen_height - y - height, width, height),
    })
}

/// Gives the screen to a tape: the console stops painting (its text grid, serial and `term net` output carry on) and the
/// screen is cleared to the console's background. False without a pixel framebuffer.
pub fn gfx_open() -> bool {
    let Some(console) = (unsafe { (*core::ptr::addr_of_mut!(CONSOLE)).as_mut() }) else { return false };
    if console.text {
        return false;
    }
    let (width, height, background) = (console.width, console.height, console.background);
    console.fill(0, 0, width, height, background);
    unsafe { GFX_OPEN = true };
    true
}

/// Takes the screen back (also done when a tape ends): clears it and draws the whole text grid again.
pub fn gfx_close() {
    if !unsafe { GFX_OPEN } {
        return;
    }
    unsafe { GFX_OPEN = false };
    let Some(console) = (unsafe { (*core::ptr::addr_of_mut!(CONSOLE)).as_mut() }) else { return };
    let (width, height, background) = (console.width, console.height, console.background);
    console.fill(0, 0, width, height, background);
    unsafe {
        (*core::ptr::addr_of_mut!(PAINTED)).fill(b' ');
        CARET_AT = usize::MAX;
    }
    console.repaint();
}

/// A tape's 0x00RRGGBB in this framebuffer's byte order (`Console::pixel` for a packed colour).
fn gfx_pixel(console: &Console, rgb: u32) -> u32 {
    console.pixel((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

/// Fills a rectangle (clipped to the screen) with 0x00RRGGBB. False if no tape has the screen open.
pub fn gfx_fill(x: u32, y: u32, width: u32, height: u32, rgb: u32) -> bool {
    let Some(console) = (unsafe { (*core::ptr::addr_of_mut!(CONSOLE)).as_mut() }).filter(|_| unsafe { GFX_OPEN }) else { return false };
    let value = gfx_pixel(console, rgb);
    fill_logical(console, x, y, width, height, value);
    true
}

/// Fills a rectangle of the logical (rotated) screen with a framebuffer pixel value.
fn fill_logical(console: &mut Console, x: u32, y: u32, width: u32, height: u32, value: u32) {
    let Some(turn) = rotation(0) else {
        console.fill(x as usize, y as usize, width as usize, height as usize, value);
        return;
    };
    if let Some((px, py, pw, ph)) = rotate_rect(turn, console.width as u32, console.height as u32, x, y, width, height) {
        console.fill(px as usize, py as usize, pw as usize, ph as usize, value);
    }
}

/// Copies a w x h block of 0x00RRGGBB pixels (`stride` pixels per source row) to the screen at (x, y), clipped on the right
/// and bottom. A BGR framebuffer has the same pixel layout, so each row is one copy; an RGB one swaps red and blue.
///
/// # Safety
/// `source` must be readable for `(height - 1) * stride + width` pixels.
pub unsafe fn gfx_blit(x: u32, y: u32, width: u32, height: u32, source: *const u32, stride: u32) -> bool {
    let Some(console) = (unsafe { (*core::ptr::addr_of_mut!(CONSOLE)).as_mut() }).filter(|_| unsafe { GFX_OPEN }) else { return false };
    let (x, y, width, height, stride) = (x as usize, y as usize, width as usize, height as usize, stride as usize);
    let swap = console.format == PIXEL_RGB;
    if let Some(turn) = rotation(0) {
        // Logical (x, y) lands on framebuffer (W-1-y, x) turning right, (y, H-1-x) turning left, (W-1-x, H-1-y) flipped.
        let (screen_width, screen_height) = (console.width, console.height);
        let (logical_width, logical_height) = turn.logical(screen_width, screen_height);
        if source.is_null() || stride < width || x >= logical_width || y >= logical_height {
            return false;
        }
        let (columns, rows) = (width.min(logical_width - x), height.min(logical_height - y));
        for row in 0..rows {
            for column in 0..columns {
                let value = unsafe { source.add(row * stride + column).read() };
                let value = if swap { value & 0xff00_ff00 | (value & 0xff) << 16 | (value >> 16) & 0xff } else { value };
                let (px, py) = turn.map(screen_width, screen_height, x + column, y + row);
                unsafe { console.base.add(py * console.stride + px).write_volatile(value) };
            }
        }
        return true;
    }
    if source.is_null() || stride < width || x >= console.width || y >= console.height {
        return false;
    }
    let columns = width.min(console.width - x);
    let rows = height.min(console.height - y);
    for row in 0..rows {
        let from = unsafe { source.add(row * stride) };
        let to = unsafe { console.base.add((y + row) * console.stride + x) };
        if swap {
            for column in 0..columns {
                let value = unsafe { from.add(column).read() };
                let swapped = value & 0xff00_ff00 | (value & 0xff) << 16 | (value >> 16) & 0xff;
                unsafe { to.add(column).write_volatile(swapped) };
            }
        } else {
            unsafe { core::ptr::copy_nonoverlapping(from, to, columns) };
        }
    }
    true
}

/// A screen beyond the console's: only tapes draw on it (Pi 4 second HDMI output, `mailbox::second_screen`). 0x00RRGGBB
/// pixels as on the console, BGR in memory.
struct Extra {
    base: *mut u32,
    stride: usize,
    width: usize,
    height: usize,
    open: bool,
}

static mut EXTRA: Option<Extra> = None;

/// Registers the second screen: `stride` is in pixels, memory already mapped, 32 bits per pixel, blue first in memory.
pub fn add_screen(base: u64, width: u32, height: u32, stride: u32) {
    let extra = Extra { base: base as *mut u32, stride: stride as usize, width: width as usize, height: height as usize, open: false };
    unsafe { *core::ptr::addr_of_mut!(EXTRA) = Some(extra) };
}

fn extra() -> Option<&'static mut Extra> {
    unsafe { (*core::ptr::addr_of_mut!(EXTRA)).as_mut() }
}

fn extra_clear(screen: &Extra) {
    for y in 0..screen.height {
        unsafe { core::ptr::write_bytes(screen.base.add(y * screen.stride), 0, screen.width) };
    }
}

/// Ends a tape's use of the extra screen: black again (there is no text to redraw).
pub fn gfx_close_extra() {
    if let Some(screen) = extra().filter(|screen| screen.open) {
        screen.open = false;
        extra_clear(screen);
    }
}

/// How one physical screen is shared by two tapes (`run A | B`): each tape sees its half as a whole screen and lays itself
/// out to that size.
#[derive(Clone, Copy, PartialEq)]
pub enum Split {
    /// First tape left, second right.
    Sides,
    /// First tape above, second below.
    Stacked,
}

/// Tape screen numbers: 0 and 1 are whole physical screens (the console, the Pi's second HDMI output). While physical screen
/// `p` is split its halves are numbers `2 + 2p` and `3 + 2p`, and the number `p` is not available.
static mut SPLIT: [Option<Split>; 2] = [None; 2];
/// Splits made for background tapes: they end when the services do (`split_services_over`).
static mut SPLIT_SERVICE: [bool; 2] = [false; 2];
static mut REGION_OPEN: [bool; 4] = [false; 4];

/// Splits physical screen `phys` (`None` ends it). Ending does not repaint: the tapes' own close, or `gfx_close`, does.
pub fn split_set(phys: usize, mode: Option<Split>) {
    if phys > 1 {
        return;
    }
    unsafe {
        (*core::ptr::addr_of_mut!(SPLIT))[phys] = mode;
        (*core::ptr::addr_of_mut!(SPLIT_SERVICE))[phys] = false;
        let open = &mut *core::ptr::addr_of_mut!(REGION_OPEN);
        open[2 * phys] = false;
        open[2 * phys + 1] = false;
    }
}

/// `split_set` for tapes started as services: the split goes away when the last service has ended.
pub fn split_set_service(phys: usize, mode: Split) {
    split_set(phys, Some(mode));
    if phys < 2 {
        unsafe { (*core::ptr::addr_of_mut!(SPLIT_SERVICE))[phys] = true };
    }
}

pub fn split_clear_all() {
    for phys in 0..2 {
        split_set(phys, None);
    }
}

pub fn split_services_over() {
    for phys in 0..2 {
        if unsafe { (*core::ptr::addr_of!(SPLIT_SERVICE))[phys] } {
            split_set(phys, None);
        }
    }
}

/// Logical size of physical screen `phys`.
fn phys_info(phys: usize) -> Option<(u32, u32)> {
    match phys {
        0 => gfx_info(),
        _ => extra().map(|screen| match rotation(1) {
            Some(turn) if turn.swaps() => (screen.height as u32, screen.width as u32),
            _ => (screen.width as u32, screen.height as u32),
        }),
    }
}

fn phys_open(phys: usize) -> bool {
    match phys {
        0 => gfx_open(),
        _ => extra().is_some_and(|screen| {
            extra_clear(screen);
            screen.open = true;
            true
        }),
    }
}

fn phys_close(phys: usize) {
    match phys {
        0 => gfx_close(),
        _ => gfx_close_extra(),
    }
}

fn phys_fill(phys: usize, x: u32, y: u32, width: u32, height: u32, rgb: u32) -> bool {
    match phys {
        0 => gfx_fill(x, y, width, height, rgb),
        _ => {
            let Some(screen) = extra().filter(|screen| screen.open) else { return false };
            let (mut x, mut y, mut width, mut height) = (x, y, width, height);
            if let Some(turn) = rotation(1) {
                match rotate_rect(turn, screen.width as u32, screen.height as u32, x, y, width, height) {
                    Some(rect) => (x, y, width, height) = rect,
                    None => return true,
                }
            }
            let (x, y) = (x as usize, y as usize);
            let columns = (width as usize).min(screen.width.saturating_sub(x));
            let rows = (height as usize).min(screen.height.saturating_sub(y));
            for row in 0..rows {
                for column in 0..columns {
                    unsafe { screen.base.add((y + row) * screen.stride + x + column).write_volatile(rgb) };
                }
            }
            true
        }
    }
}

unsafe fn phys_blit(phys: usize, x: u32, y: u32, width: u32, height: u32, pixels: *const u32, stride: u32) -> bool {
    match phys {
        0 => unsafe { gfx_blit(x, y, width, height, pixels, stride) },
        _ => {
            let Some(screen) = extra().filter(|screen| screen.open) else { return false };
            let (x, y, width, height, stride) = (x as usize, y as usize, width as usize, height as usize, stride as usize);
            if let Some(turn) = rotation(1) {
                // Logical (x, y) lands on (W-1-y, x) turning right, (y, H-1-x) turning left, as on the console.
                let (logical_width, logical_height) = turn.logical(screen.width, screen.height);
                if pixels.is_null() || stride < width || x >= logical_width || y >= logical_height {
                    return false;
                }
                for row in 0..height.min(logical_height - y) {
                    for column in 0..width.min(logical_width - x) {
                        let value = unsafe { pixels.add(row * stride + column).read() };
                        let (px, py) = turn.map(screen.width, screen.height, x + column, y + row);
                        unsafe { screen.base.add(py * screen.stride + px).write_volatile(value) };
                    }
                }
                return true;
            }
            if pixels.is_null() || stride < width || x >= screen.width || y >= screen.height {
                return false;
            }
            for row in 0..height.min(screen.height - y) {
                let from = unsafe { pixels.add(row * stride) };
                let to = unsafe { screen.base.add((y + row) * screen.stride + x) };
                unsafe { core::ptr::copy_nonoverlapping(from, to, width.min(screen.width - x)) };
            }
            true
        }
    }
}

/// Clears a rectangle of an open physical screen: the console's background, black on the second screen.
fn phys_clear_rect(phys: usize, x: u32, y: u32, width: u32, height: u32) {
    match phys {
        0 => {
            if let Some(console) = unsafe { (*core::ptr::addr_of_mut!(CONSOLE)).as_mut() } {
                let background = console.background;
                fill_logical(console, x, y, width, height, background);
            }
        }
        _ => {
            phys_fill(phys, x, y, width, height, 0);
        }
    }
}

/// What a tape screen number means right now: the physical screen, and for a half of a split screen which half and its
/// rectangle (x, y, width, height) on the logical screen. `None` for a number that is not available.
fn resolve(screen: u32) -> Option<(usize, Option<(usize, (u32, u32, u32, u32))>)> {
    match screen {
        0 | 1 => unsafe { (*core::ptr::addr_of!(SPLIT))[screen as usize] }.is_none().then_some((screen as usize, None)),
        2..=5 => {
            let (phys, half) = (((screen - 2) / 2) as usize, ((screen - 2) % 2) as usize);
            let mode = unsafe { (*core::ptr::addr_of!(SPLIT))[phys] }?;
            let (width, height) = phys_info(phys)?;
            let rect = match mode {
                Split::Sides => (half as u32 * (width / 2), 0, width / 2, height),
                Split::Stacked => (0, half as u32 * (height / 2), width, height / 2),
            };
            Some((phys, Some((half, rect))))
        }
        _ => None,
    }
}

fn region_open(phys: usize, half: usize) -> bool {
    unsafe { (*core::ptr::addr_of!(REGION_OPEN))[2 * phys + half] }
}

fn set_region_open(phys: usize, half: usize, open: bool) {
    unsafe { (*core::ptr::addr_of_mut!(REGION_OPEN))[2 * phys + half] = open };
}

pub extern "C" fn api_gfx_screens() -> u32 {
    u32::from(gfx_info().is_some()) + u32::from(extra().is_some())
}

pub extern "C" fn api_gfx_info_on(screen: u32) -> u64 {
    let size = match resolve(screen) {
        Some((_, Some((_, (_, _, width, height))))) => Some((width, height)),
        Some((phys, None)) => phys_info(phys),
        None => None,
    };
    size.map_or(0, |(width, height)| u64::from(width) << 32 | u64::from(height))
}

pub extern "C" fn api_gfx_open_on(screen: u32) -> u32 {
    match resolve(screen) {
        Some((phys, None)) => u32::from(phys_open(phys)),
        Some((phys, Some((half, (x, y, width, height))))) => {
            // The first tape takes the whole screen (cleared); the second one only clears its own half.
            let other_open = region_open(phys, 1 - half);
            let ok = if other_open {
                phys_clear_rect(phys, x, y, width, height);
                true
            } else {
                phys_open(phys)
            };
            set_region_open(phys, half, ok);
            u32::from(ok)
        }
        None => 0,
    }
}

pub extern "C" fn api_gfx_close_on(screen: u32) {
    match resolve(screen) {
        Some((phys, None)) => phys_close(phys),
        Some((phys, Some((half, (x, y, width, height))))) => {
            if !region_open(phys, half) {
                return;
            }
            set_region_open(phys, half, false);
            // The screen goes back when the last half is given up; until then the freed half just goes dark.
            if region_open(phys, 1 - half) {
                phys_clear_rect(phys, x, y, width, height);
            } else {
                phys_close(phys);
            }
        }
        None => {}
    }
}

pub extern "C" fn api_gfx_fill_on(screen: u32, x: u32, y: u32, width: u32, height: u32, rgb: u32) -> u32 {
    match resolve(screen) {
        Some((phys, None)) => u32::from(phys_fill(phys, x, y, width, height, rgb)),
        Some((phys, Some((half, (left, top, half_width, half_height))))) => {
            if !region_open(phys, half) {
                return 0;
            }
            if x >= half_width || y >= half_height {
                return 1;
            }
            u32::from(phys_fill(phys, left + x, top + y, width.min(half_width - x), height.min(half_height - y), rgb))
        }
        None => 0,
    }
}

pub extern "C" fn api_gfx_blit_on(screen: u32, x: u32, y: u32, width: u32, height: u32, pixels: *const u32, stride: u32) -> u32 {
    match resolve(screen) {
        Some((phys, None)) => u32::from(unsafe { phys_blit(phys, x, y, width, height, pixels, stride) }),
        Some((phys, Some((half, (left, top, half_width, half_height))))) => {
            if !region_open(phys, half) || x >= half_width || y >= half_height {
                return 0;
            }
            // Clipping to the half keeps the stride check (`stride >= width`) true.
            let (width, height) = (width.min(half_width - x), height.min(half_height - y));
            u32::from(unsafe { phys_blit(phys, left + x, top + y, width, height, pixels, stride) })
        }
        None => 0,
    }
}

pub fn write_str(text: &str) {
    use core::fmt::Write;
    if let Some(console) = unsafe { (*core::ptr::addr_of_mut!(CONSOLE)).as_mut() } {
        let _ = console.write_str(text);
    }
}
