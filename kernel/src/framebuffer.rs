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
    (!console.text).then_some((console.width as u32, console.height as u32))
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
    console.fill(x as usize, y as usize, width as usize, height as usize, value);
    true
}

/// Copies a w x h block of 0x00RRGGBB pixels (`stride` pixels per source row) to the screen at (x, y), clipped on the right
/// and bottom. A BGR framebuffer has the same pixel layout, so each row is one copy; an RGB one swaps red and blue.
///
/// # Safety
/// `source` must be readable for `(height - 1) * stride + width` pixels.
pub unsafe fn gfx_blit(x: u32, y: u32, width: u32, height: u32, source: *const u32, stride: u32) -> bool {
    let Some(console) = (unsafe { (*core::ptr::addr_of_mut!(CONSOLE)).as_mut() }).filter(|_| unsafe { GFX_OPEN }) else { return false };
    let (x, y, width, height, stride) = (x as usize, y as usize, width as usize, height as usize, stride as usize);
    if source.is_null() || stride < width || x >= console.width || y >= console.height {
        return false;
    }
    let columns = width.min(console.width - x);
    let rows = height.min(console.height - y);
    let swap = console.format == PIXEL_RGB;
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

pub extern "C" fn api_gfx_screens() -> u32 {
    u32::from(gfx_info().is_some()) + u32::from(extra().is_some())
}

pub extern "C" fn api_gfx_info_on(screen: u32) -> u64 {
    match screen {
        0 => gfx_info().map_or(0, |(width, height)| u64::from(width) << 32 | u64::from(height)),
        1 => extra().map_or(0, |screen| (screen.width as u64) << 32 | screen.height as u64),
        _ => 0,
    }
}

pub extern "C" fn api_gfx_open_on(screen: u32) -> u32 {
    match screen {
        0 => u32::from(gfx_open()),
        1 => extra().map_or(0, |screen| {
            extra_clear(screen);
            screen.open = true;
            1
        }),
        _ => 0,
    }
}

pub extern "C" fn api_gfx_close_on(screen: u32) {
    match screen {
        0 => gfx_close(),
        1 => gfx_close_extra(),
        _ => {}
    }
}

pub extern "C" fn api_gfx_fill_on(screen: u32, x: u32, y: u32, width: u32, height: u32, rgb: u32) -> u32 {
    match screen {
        0 => u32::from(gfx_fill(x, y, width, height, rgb)),
        1 => {
            let Some(screen) = extra().filter(|screen| screen.open) else { return 0 };
            let (x, y) = (x as usize, y as usize);
            let columns = (width as usize).min(screen.width.saturating_sub(x));
            let rows = (height as usize).min(screen.height.saturating_sub(y));
            for row in 0..rows {
                for column in 0..columns {
                    unsafe { screen.base.add((y + row) * screen.stride + x + column).write_volatile(rgb) };
                }
            }
            1
        }
        _ => 0,
    }
}

pub extern "C" fn api_gfx_blit_on(screen: u32, x: u32, y: u32, width: u32, height: u32, pixels: *const u32, stride: u32) -> u32 {
    match screen {
        0 => u32::from(unsafe { gfx_blit(x, y, width, height, pixels, stride) }),
        1 => {
            let Some(screen) = extra().filter(|screen| screen.open) else { return 0 };
            let (x, y, width, height, stride) = (x as usize, y as usize, width as usize, height as usize, stride as usize);
            if pixels.is_null() || stride < width || x >= screen.width || y >= screen.height {
                return 0;
            }
            for row in 0..height.min(screen.height - y) {
                let from = unsafe { pixels.add(row * stride) };
                let to = unsafe { screen.base.add((y + row) * screen.stride + x) };
                unsafe { core::ptr::copy_nonoverlapping(from, to, width.min(screen.width - x)) };
            }
            1
        }
        _ => 0,
    }
}

pub fn write_str(text: &str) {
    use core::fmt::Write;
    if let Some(console) = unsafe { (*core::ptr::addr_of_mut!(CONSOLE)).as_mut() } {
        let _ = console.write_str(text);
    }
}
