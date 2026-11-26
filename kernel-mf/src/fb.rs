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
//! - Drawing outside the print lock: `print` holds the console lock with interrupts off, and a scroll used to repaint the
//!   whole screen under it (up to ~2 ms on rack at 1920x1080), long enough to make an audio card's interrupt a whole DMA
//!   ring late. Now a print only changes `CELLS` and marks the rows it touched; the `console` thread (`start_painter`)
//!   draws them with interrupts on, one row per short hold of `PAINT`, and only the cells that differ from `SHOWN`, the
//!   grid as it is on the screen. A burst of lines becomes one pass. Before the thread runs (boot) and after a crash
//!   (`direct`), a print draws at once, as before: the crash text must reach the screen with no scheduler to rely on.

use fosfor_abi::{Framebuffer, PIXEL_NONE, PIXEL_RGB};

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::sync::SpinLock;
use crate::vm;

#[path = "../../kernel/src/font_data.rs"]
#[allow(dead_code)]
mod font_data;
use font_data::{FONT, FONT_HEIGHT, FONT_WIDTH};

/// The text grid is clamped to this size (4096x2720 pixels); a larger screen leaves its margin black.
const MAX_COLUMNS: usize = 512;
const MAX_ROWS: usize = 170;
/// One cell: the byte in the low half, foreground (0-15) and background (0-15) colour in the high half. Changed only under
/// the console lock (`serial::with_lock`), which `write_str`'s callers hold.
static mut CELLS: [u16; MAX_COLUMNS * MAX_ROWS] = [0; MAX_COLUMNS * MAX_ROWS];
/// The cells as drawn on the screen, under `PAINT`; the caret's underline is drawn over its cell and not recorded here.
static mut SHOWN: [u16; MAX_COLUMNS * MAX_ROWS] = [0; MAX_COLUMNS * MAX_ROWS];
/// Rows of `CELLS` changed since they were drawn, one bit each: set under the console lock, cleared by a pass as it
/// copies the row.
static DIRTY: [AtomicU64; MAX_ROWS.div_ceil(64)] = [const { AtomicU64::new(0) }; MAX_ROWS.div_ceil(64)];
/// The next pass draws every cell, whatever `SHOWN` says (the palette changed, or the screen's pixels are unknown).
static FORCE: AtomicBool = AtomicBool::new(false);
/// Held while pixels are written: by a pass for one row, by the caret, by `gfx_*` and `set_colours`. Taken after the
/// console lock when both are needed, never before it.
static PAINT: SpinLock<()> = SpinLock::new(());
/// The `console` thread draws (set by `start_painter`, cleared for good by `direct`); clear: `write_str` draws itself.
static PAINTER: AtomicBool = AtomicBool::new(false);
/// Set for good by a crash (`direct`): prints draw without `PAINT`, which a painter on another CPU may hold for ever.
static CRASHED: AtomicBool = AtomicBool::new(false);
/// Something changed since the painter's last pass began; it sleeps on this flag's address.
static PENDING: AtomicBool = AtomicBool::new(false);
/// Longest `write_str` while the painter draws (the time a print keeps interrupts off), and longest painter pass, in µs.
static WRITE_MAX_US: AtomicU64 = AtomicU64::new(0);
static PASS_MAX_US: AtomicU64 = AtomicU64::new(0);

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
    console.clear_screen();
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
    console.clear_screen();
    // CRTC register 0x0A bit 5 turns the cursor off: clear it, and give it scan lines 13-15 (an underline-to-block shape).
    outb(0x3d4, 0x0a);
    outb(0x3d5, 13);
    outb(0x3d4, 0x0b);
    outb(0x3d5, 15);
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

/// Under `PAINT` (or before the console exists).
fn shown() -> &'static mut [u16; MAX_COLUMNS * MAX_ROWS] {
    unsafe { &mut *core::ptr::addr_of_mut!(SHOWN) }
}

fn mark_dirty(row: usize) {
    DIRTY[row / 64].fetch_or(1 << (row % 64), Ordering::Relaxed);
}

fn mark_all_dirty() {
    for word in &DIRTY {
        word.store(u64::MAX, Ordering::Relaxed);
    }
}

/// Clears `row`'s dirty bit; true if it was set.
fn take_dirty(row: usize) -> bool {
    let bit = 1 << (row % 64);
    DIRTY[row / 64].fetch_and(!bit, Ordering::Relaxed) & bit != 0
}

fn is_dirty(row: usize) -> bool {
    DIRTY[row / 64].load(Ordering::Relaxed) & 1 << (row % 64) != 0
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

    /// Every cell blank in the current background; the screen follows with the next pass (`clear_screen`: at once).
    fn clear_all(&mut self) {
        let blank = self.blank();
        cells().fill(blank);
        mark_all_dirty();
    }

    /// The screen painted to match a grid `clear_all` just blanked: the pixel screen filled with the background (its margin
    /// too), the text screen drawn cell by cell. Under `PAINT`, or before the console is installed.
    fn clear_screen(&mut self) {
        if self.text {
            for row in 0..self.rows {
                for column in 0..self.columns {
                    self.draw_cell(column, row);
                }
            }
        } else {
            let background = self.palette[usize::from(self.background)];
            self.fill(0, self.height, background);
        }
        let blank = self.blank();
        shown().fill(blank);
        self.caret_at = usize::MAX;
    }

    /// A text cell's VGA attribute from the grid's (foreground 0-15, background 0-7: the top bit would blink).
    fn vga_attribute(attribute: u16) -> u16 {
        let colour = |ansi: u16| u16::from(ANSI_TO_VGA[usize::from(ansi & 7)]) | (ansi & 8);
        colour(attribute & 15) | colour((attribute >> 4) & 7) << 4
    }

    /// Puts the hardware cursor (CRTC registers 0x0E and 0x0F) on the next character position.
    fn place_cursor(&self) {
        // CRTC register 0x0A bit 5 switches the hardware cursor off (`ESC [ ? 25 l`).
        outb(0x3d4, 0x0a);
        outb(0x3d5, if self.caret_hidden { 0x20 } else { 13 });
        let at = (self.row * self.columns + self.column.min(self.columns - 1)) as u16;
        outb(0x3d4, 0x0e);
        outb(0x3d5, (at >> 8) as u8);
        outb(0x3d4, 0x0f);
        outb(0x3d5, at as u8);
    }

    /// The pixel console's caret: an underline in the cell the next character goes to. A cell that was redrawn has lost it and
    /// a cell it left must be restored, so the old cell is redrawn and the caret drawn afresh each time.
    /// Under `PAINT`; `row`, `column` and `hidden` are the cursor as the grid had it, `SHOWN` what the cells look like.
    fn draw_caret(&mut self, row: usize, column: usize, hidden: bool) {
        let old = self.caret_at;
        let at = row.min(self.rows - 1) * self.columns + column.min(self.columns - 1);
        if old < self.rows * self.columns && (old != at || hidden) {
            let cell = shown()[(old / self.columns) * MAX_COLUMNS + old % self.columns];
            self.draw_value(old % self.columns, old / self.columns, cell);
        }
        if hidden {
            self.caret_at = usize::MAX;
            return;
        }
        let cell = shown()[(at / self.columns) * MAX_COLUMNS + at % self.columns];
        let value = self.palette[usize::from((cell >> 8) & 15)];
        let (x, y) = ((at % self.columns) * FONT_WIDTH, (at / self.columns) * FONT_HEIGHT);
        for line in FONT_HEIGHT - 2..FONT_HEIGHT {
            for bit in 0..FONT_WIDTH {
                unsafe { self.base.add((y + line) * self.stride + x + bit).write_volatile(value) };
            }
        }
        self.caret_at = at;
    }

    /// Draws the cell as the grid has it and records it in `SHOWN`.
    fn draw_cell(&mut self, column: usize, row: usize) {
        let cell = cells()[row * MAX_COLUMNS + column];
        shown()[row * MAX_COLUMNS + column] = cell;
        self.draw_value(column, row, cell);
    }

    fn draw_value(&mut self, column: usize, row: usize, cell: u16) {
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
        mark_dirty(row);
    }

    fn scroll(&mut self) {
        let cells = cells();
        cells.copy_within(MAX_COLUMNS..self.rows * MAX_COLUMNS, 0);
        let blank = self.blank();
        cells[(self.rows - 1) * MAX_COLUMNS..self.rows * MAX_COLUMNS].fill(blank);
        mark_all_dirty();
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

fn console() -> Option<&'static mut Console> {
    unsafe { (*core::ptr::addr_of_mut!(CONSOLE)).as_mut() }
}

/// Called by the console for every piece of text, with the serial lock held; a no-op until `init` succeeded. With the
/// painter running it only updates the grid and wakes it; otherwise it draws the change itself.
pub fn write_str(text: &str) {
    if GFX_OPEN.load(Ordering::Relaxed) {
        return;
    }
    let Some(console) = console() else { return };
    if !PAINTER.load(Ordering::Acquire) {
        for byte in text.bytes() {
            console.put(byte);
        }
        paint(true);
        return;
    }
    let start = crate::time::micros();
    for byte in text.bytes() {
        console.put(byte);
    }
    // The painter blocks on PENDING with the scheduler locked, so setting it before the wake cannot be missed.
    if !PENDING.swap(true, Ordering::AcqRel) {
        crate::sched::wake_all(PENDING.as_ptr() as usize);
    }
    WRITE_MAX_US.fetch_max(crate::time::micros().saturating_sub(start), Ordering::Relaxed);
}

/// One pass over the grid: each dirty row copied from `CELLS` under the console lock (`locked`: the caller holds it), then
/// the cells that differ from `SHOWN` drawn under `PAINT`, one row per hold; last the caret. Interrupts are off only for
/// a row at a time unless the caller holds the console lock.
/// `PAINT`, except after a crash: then nothing is taken (the holder may be stopped mid-row; a few stray pixels are the cost).
fn paint_lock() -> Option<crate::sync::Guard<'static, ()>> {
    (!CRASHED.load(Ordering::Acquire)).then(|| PAINT.lock())
}

fn paint(locked: bool) {
    let Some(console) = console() else { return };
    let grid = |f: &mut dyn FnMut()| if locked { f() } else { crate::serial::with_lock(|| f()) };
    let force = FORCE.swap(false, Ordering::AcqRel);
    let mut row_cells = [0u16; MAX_COLUMNS];
    let columns = console.columns;
    for row in 0..console.rows {
        // The painter gives up its pass when a crash made prints draw themselves.
        if !locked && !PAINTER.load(Ordering::Acquire) {
            return;
        }
        if !is_dirty(row) {
            continue;
        }
        let mut copied = false;
        grid(&mut || {
            if take_dirty(row) {
                row_cells[..columns].copy_from_slice(&cells()[row * MAX_COLUMNS..row * MAX_COLUMNS + columns]);
                copied = true;
            }
        });
        if !copied {
            continue;
        }
        let _paint = paint_lock();
        // A tape took the screen since: `gfx_close` repaints it all.
        if GFX_OPEN.load(Ordering::Relaxed) {
            return;
        }
        let shown = &mut shown()[row * MAX_COLUMNS..row * MAX_COLUMNS + columns];
        for column in 0..columns {
            if force || shown[column] != row_cells[column] {
                console.draw_value(column, row, row_cells[column]);
                shown[column] = row_cells[column];
            }
        }
    }
    let mut cursor = (0, 0, false);
    grid(&mut || cursor = (console.row, console.column, console.caret_hidden));
    let _paint = paint_lock();
    if GFX_OPEN.load(Ordering::Relaxed) {
        return;
    }
    if console.text {
        console.place_cursor();
    } else {
        console.draw_caret(cursor.0, cursor.1, cursor.2);
    }
    flush_writes();
}

/// The `console` thread: sleeps until a print changed the grid, then draws it (`paint`).
extern "C" fn painter(_argument: usize) {
    loop {
        crate::sched::wait_while(PENDING.as_ptr() as usize, || !PENDING.load(Ordering::Acquire));
        if !PAINTER.load(Ordering::Acquire) {
            crate::sched::exit();
        }
        PENDING.store(false, Ordering::Release);
        let start = crate::time::micros();
        paint(false);
        PASS_MAX_US.fetch_max(crate::time::micros().saturating_sub(start), Ordering::Relaxed);
    }
}

/// Starts the `console` thread once the scheduler runs; from then on prints leave the drawing to it. Any CPU may run it.
pub fn start_painter() {
    if console().is_none() || PAINTER.load(Ordering::Acquire) {
        return;
    }
    PAINTER.store(true, Ordering::Release);
    if crate::sched::spawn_any("console", painter, 0).is_none() {
        PAINTER.store(false, Ordering::Release);
    }
}

/// For the crash paths (`serial::break_lock`): prints draw at once again, without `PAINT`. Forcing `PAINT` open instead
/// would let a painter that still holds it bump the ticket lock past its next taker, and the crash report would never draw.
pub fn direct() {
    PAINTER.store(false, Ordering::Release);
    CRASHED.store(true, Ordering::Release);
}

/// (longest print with the painter running, longest painter pass), in µs, for `sysrq`.
pub fn timings() -> (u64, u64) {
    (WRITE_MAX_US.load(Ordering::Relaxed), PASS_MAX_US.load(Ordering::Relaxed))
}

/// Makes the write-combining buffers drain, so what was just drawn is on the screen.
fn flush_writes() {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::asm!("sfence", options(nostack, preserves_flags))
    };
    // The framebuffer is mapped non-cacheable (`paging::build_and_load`, `map_framebuffer`); the barrier orders the stores.
    #[cfg(not(target_arch = "x86_64"))]
    unsafe {
        core::arch::asm!("dsb st", options(nostack, preserves_flags))
    };
}

/// The VGA CRTC is x86 port I/O; there is no text screen on aarch64 (`init_text` is never called there).
#[cfg(target_arch = "x86_64")]
fn outb(port: u16, value: u8) {
    crate::cpu::outb(port, value)
}
#[cfg(not(target_arch = "x86_64"))]
fn outb(_port: u16, _value: u8) {}

/// Graphics for tapes (`TapeApi::gfx_*`): while a process has the screen open the text console stops drawing (serial and the
/// network terminal go on); `gfx_close` clears the screen and the text starts again at the top. Tapes draw on an upright
/// logical screen that `rotate` turns onto the framebuffer (the text console is not rotated).
static GFX_OPEN: AtomicBool = AtomicBool::new(false);

pub use kernel_core::screen::Rotate;

/// The `rotate` setting per physical screen (0: the console, 1: the Pi's second HDMI output): how tapes' pixels are turned.
static mut ROTATE: [Option<Rotate>; 2] = [None; 2];

pub fn rotate_default(phys: usize, mode: Option<Rotate>) {
    if phys < 2 {
        unsafe { (*core::ptr::addr_of_mut!(ROTATE))[phys] = mode };
    }
}

/// `rotate=` of a `run` line: up to four running tapes, each with its own pid and turn (0: none, 1-3: `Rotate`, 4: `off`, upright
/// whatever the setting says). It applies to the screens that tape draws on; the pid is the calling tape's.
const RUN_SLOTS: usize = 4;
static RUN_PID: [core::sync::atomic::AtomicU64; RUN_SLOTS] = [const { core::sync::atomic::AtomicU64::new(0) }; RUN_SLOTS];
static RUN_TURN: [core::sync::atomic::AtomicU8; RUN_SLOTS] = [const { core::sync::atomic::AtomicU8::new(0) }; RUN_SLOTS];

/// `Some(None)` is `rotate=off`; `None` is a word that is not a turn.
pub fn parse_run_turn(word: &str) -> Option<Option<Rotate>> {
    if word == "off" {
        return Some(None);
    }
    Rotate::parse(word).map(Some)
}

fn encode(turn: Option<Rotate>) -> u8 {
    match turn {
        None => 4,
        Some(Rotate::Right) => 1,
        Some(Rotate::Left) => 2,
        Some(Rotate::Flip) => 3,
    }
}

/// Gives process `pid` its own turn for the rest of its life (`rotate_forget` ends it). A full table leaves the setting in force.
pub fn rotate_run(pid: u64, turn: Option<Rotate>) {
    for slot in 0..RUN_SLOTS {
        if RUN_PID[slot].compare_exchange(0, pid, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            RUN_TURN[slot].store(encode(turn), Ordering::Release);
            return;
        }
    }
}

pub fn rotate_forget(pid: u64) {
    for slot in 0..RUN_SLOTS {
        if RUN_PID[slot].load(Ordering::Acquire) == pid {
            RUN_TURN[slot].store(0, Ordering::Release);
            RUN_PID[slot].store(0, Ordering::Release);
        }
    }
}

pub fn rotation(phys: usize) -> Option<Rotate> {
    // A tape's own calls (`gfx_info` comes before `gfx_open`) run as that process; the touch poll runs as no tape, so it asks
    // the process that has the screen open.
    let caller = crate::sched::current_pid();
    let pid = if caller != 0 { caller } else { crate::process::screen_owner() };
    if pid != 0 {
        for slot in 0..RUN_SLOTS {
            if RUN_PID[slot].load(Ordering::Acquire) == pid {
                match RUN_TURN[slot].load(Ordering::Acquire) {
                    1 => return Some(Rotate::Right),
                    2 => return Some(Rotate::Left),
                    3 => return Some(Rotate::Flip),
                    4 => return None,
                    _ => {}
                }
            }
        }
    }
    unsafe { (*core::ptr::addr_of!(ROTATE)).get(phys).copied().flatten() }
}

/// Size of a logical screen: width and height swap for a quarter turn.
fn logical_size(phys: usize, width: usize, height: usize) -> (u32, u32) {
    let (w, h) = rotation(phys).map_or((width, height), |turn| turn.logical(width, height));
    (w as u32, h as u32)
}

/// Writes a clipped rectangle of the logical screen onto a physical `width` x `height` framebuffer, one pixel at a time
/// through `turn`; `pixel(column, row)` gives the value for the rectangle's own coordinates.
fn put_turned(turn: Rotate, base: *mut u32, stride: usize, width: usize, height: usize, (x, y, w, h): (usize, usize, usize, usize), pixel: impl Fn(usize, usize) -> u32) {
    let (lw, lh) = turn.logical(width, height);
    for row in 0..h.min(lh.saturating_sub(y)) {
        for column in 0..w.min(lw.saturating_sub(x)) {
            let (px, py) = turn.map(width, height, x + column, y + row);
            unsafe { base.add(py * stride + px).write_volatile(pixel(column, row)) };
        }
    }
}

/// Columns and rows of the text console (`console_size`), or None before it exists.
pub fn console_size() -> Option<(usize, usize)> {
    unsafe { (*core::ptr::addr_of!(CONSOLE)).as_ref() }.map(|console| (console.columns, console.rows))
}

/// Width and height of the pixel screen, or None without one.
pub fn gfx_info() -> Option<(u32, u32)> {
    let console = unsafe { (*core::ptr::addr_of!(CONSOLE)).as_ref() }.filter(|console| !console.text)?;
    Some(logical_size(0, console.width, console.height))
}

pub fn gfx_open() -> bool {
    let Some(console) = (unsafe { (*core::ptr::addr_of_mut!(CONSOLE)).as_mut() }).filter(|console| !console.text) else { return false };
    let _paint = PAINT.lock();
    GFX_OPEN.store(true, Ordering::Relaxed);
    let black = console.palette[usize::from(DEFAULT_BACKGROUND)];
    console.fill(0, console.height, black);
    console.caret_at = usize::MAX;
    flush_writes();
    true
}

/// Gives the screen back: cleared, text from the top left.
pub fn gfx_close() {
    if !GFX_OPEN.swap(false, Ordering::Relaxed) {
        return;
    }
    let Some(console) = (unsafe { (*core::ptr::addr_of_mut!(CONSOLE)).as_mut() }) else { return };
    crate::serial::with_lock(|| {
        let _paint = PAINT.lock();
        console.foreground = DEFAULT_FOREGROUND;
        console.background = DEFAULT_BACKGROUND;
        console.bold = false;
        console.escape = Escape::None;
        console.caret_hidden = false;
        console.clear_all();
        console.clear_screen();
        console.column = 0;
        console.row = 0;
        console.draw_caret(0, 0, false);
        flush_writes();
    });
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
    if let Some(turn) = rotation(0) {
        put_turned(turn, console.base, console.stride, console.width, console.height, (x, y, width as usize, height as usize), |_, _| value);
        flush_writes();
        return true;
    }
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
    if let Some(turn) = rotation(0) {
        let (lw, lh) = turn.logical(console.width, console.height);
        if source.is_null() || stride < width || x >= lw || y >= lh {
            return false;
        }
        let console = &*console;
        put_turned(turn, console.base, console.stride, console.width, console.height, (x, y, width, height), |column, row| from_rgb(console, unsafe { source.add(row * stride + column).read_unaligned() }));
        flush_writes();
        return true;
    }
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

fn hex_colour(name: &str) -> Option<(u8, u8, u8)> {
    let digits = name.strip_prefix('#').unwrap_or(name);
    if digits.len() != 6 {
        return None;
    }
    let value = u32::from_str_radix(digits, 16).ok()?;
    Some(((value >> 16) as u8, (value >> 8) as u8, value as u8))
}

/// The colours the `foreground` setting accepts (a name, or #rrggbb), as in the embedded kernel.
fn foreground_colour(name: &str) -> Option<(u8, u8, u8)> {
    Some(match name {
        "gray" | "grey" => (0xc0, 0xc0, 0xc0),
        "green" => (0x33, 0xff, 0x33),
        "amber" => (0xff, 0xb0, 0x00),
        "black" => (0x00, 0x00, 0x00),
        _ => return hex_colour(name),
    })
}

fn background_colour(name: &str) -> Option<(u8, u8, u8)> {
    Some(match name {
        "black" => (0x00, 0x00, 0x00),
        "blue" => (0x00, 0x00, 0xaa),
        "gray" | "grey" => (0xc0, 0xc0, 0xc0),
        _ => return hex_colour(name),
    })
}

/// The `foreground` and `background` settings: the console's default text and background colours (palette entries 7 and 0, what
/// text without an explicit colour uses). `None` keeps the current one. Redraws the whole screen. False for an unknown name or
/// when there is no pixel console (the VGA text screen keeps its fixed grey on black).
pub fn set_colours(foreground: Option<&str>, background: Option<&str>) -> bool {
    let Some(console) = (unsafe { (*core::ptr::addr_of_mut!(CONSOLE)).as_mut() }).filter(|console| !console.text) else { return false };
    let (Some(foreground), Some(background)) = (
        foreground.map_or(Some(None), |name| foreground_colour(name).map(Some)),
        background.map_or(Some(None), |name| background_colour(name).map(Some)),
    ) else {
        return false;
    };
    crate::serial::with_lock(|| {
        let _paint = PAINT.lock();
        if let Some((red, green, blue)) = foreground {
            console.palette[usize::from(DEFAULT_FOREGROUND)] = pixel(console.format, red, green, blue);
        }
        if let Some((red, green, blue)) = background {
            console.palette[usize::from(DEFAULT_BACKGROUND)] = pixel(console.format, red, green, blue);
        }
        let (height, black) = (console.height, console.palette[usize::from(DEFAULT_BACKGROUND)]);
        console.fill(0, height, black);
        console.caret_at = usize::MAX;
        FORCE.store(true, Ordering::Release);
        mark_all_dirty();
    });
    // Every cell in the new colours: now if prints draw themselves, else by the painter.
    if !PAINTER.load(Ordering::Acquire) {
        crate::serial::with_lock(|| paint(true));
    } else if !PENDING.swap(true, Ordering::AcqRel) {
        crate::sched::wake_all(PENDING.as_ptr() as usize);
    }
    true
}

/// A screen beyond the console's (the Pi 4's second HDMI output, `mailbox::second_screen`): only tapes draw on it, tape screen 1.
/// 0x00RRGGBB pixels, blue first in memory (the GPU is asked for BGR), mapped non-cacheable by the caller.
struct Extra {
    base: *mut u32,
    stride: usize,
    width: usize,
    height: usize,
    open: bool,
}

static mut EXTRA: Option<Extra> = None;

/// Registers the second screen: `stride` is in pixels.
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

pub fn extra_info() -> Option<(u32, u32)> {
    extra().map(|screen| logical_size(1, screen.width, screen.height))
}

pub fn extra_open() -> bool {
    let Some(screen) = extra() else { return false };
    extra_clear(screen);
    screen.open = true;
    flush_writes();
    true
}

/// Ends a tape's use of the second screen: black again (there is no text to redraw).
pub fn extra_close() {
    if let Some(screen) = extra().filter(|screen| screen.open) {
        screen.open = false;
        extra_clear(screen);
        flush_writes();
    }
}

pub fn extra_fill(x: u32, y: u32, width: u32, height: u32, rgb: u32) -> bool {
    let Some(screen) = extra().filter(|screen| screen.open) else { return false };
    let (x, y) = (x as usize, y as usize);
    if let Some(turn) = rotation(1) {
        put_turned(turn, screen.base, screen.stride, screen.width, screen.height, (x, y, width as usize, height as usize), |_, _| rgb);
        flush_writes();
        return true;
    }
    if x >= screen.width || y >= screen.height {
        return true;
    }
    let columns = (width as usize).min(screen.width - x);
    for row in 0..(height as usize).min(screen.height - y) {
        for column in 0..columns {
            unsafe { screen.base.add((y + row) * screen.stride + x + column).write_volatile(rgb) };
        }
    }
    flush_writes();
    true
}

/// # Safety
/// `source` must be readable for `(height - 1) * stride + width` pixels.
pub unsafe fn extra_blit(x: u32, y: u32, width: u32, height: u32, source: *const u32, stride: u32) -> bool {
    let Some(screen) = extra().filter(|screen| screen.open) else { return false };
    let (x, y, width, height, stride) = (x as usize, y as usize, width as usize, height as usize, stride as usize);
    if let Some(turn) = rotation(1) {
        let (lw, lh) = turn.logical(screen.width, screen.height);
        if source.is_null() || stride < width || x >= lw || y >= lh {
            return false;
        }
        put_turned(turn, screen.base, screen.stride, screen.width, screen.height, (x, y, width, height), |column, row| unsafe { source.add(row * stride + column).read_unaligned() });
        flush_writes();
        return true;
    }
    if source.is_null() || stride < width || x >= screen.width || y >= screen.height {
        return false;
    }
    let columns = width.min(screen.width - x);
    for row in 0..height.min(screen.height - y) {
        for column in 0..columns {
            let rgb = unsafe { source.add(row * stride + column).read_unaligned() };
            unsafe { screen.base.add((y + row) * screen.stride + x + column).write_volatile(rgb) };
        }
    }
    flush_writes();
    true
}
