//! Breadcrumbs for a machine whose serial console is not (yet) talking: raw pixel writes into the firmware's GOP framebuffer,
//! which the loader hands over in BootInfo and which is mapped by the firmware's identity map and, later, by our own tables.
//! `mark(n)` lights the n-th 24-pixel white square along the top of the screen, so the count of squares says how far boot got;
//! `bits(row, value)` draws a 64-bit value as white (1) and dark (0) squares, most significant bit first, which the exception
//! handler uses for ESR, ELR and FAR. Depends on nothing else in the kernel.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

static BASE: AtomicU64 = AtomicU64::new(0);
static STRIDE: AtomicU32 = AtomicU32::new(0);
static WIDTH: AtomicU32 = AtomicU32::new(0);
static HEIGHT: AtomicU32 = AtomicU32::new(0);

pub fn init(base: u64, stride: u32, width: u32, height: u32) {
    BASE.store(base, Ordering::Relaxed);
    STRIDE.store(stride, Ordering::Relaxed);
    WIDTH.store(width, Ordering::Relaxed);
    HEIGHT.store(height, Ordering::Relaxed);
}

fn square(x: u32, y: u32, size: u32, color: u32) {
    let (base, stride, width, height) = (BASE.load(Ordering::Relaxed), STRIDE.load(Ordering::Relaxed), WIDTH.load(Ordering::Relaxed), HEIGHT.load(Ordering::Relaxed));
    if base == 0 || x + size > width || y + size > height {
        return;
    }
    for row in 0..size {
        for column in 0..size {
            let at = base + 4 * (u64::from(y + row) * u64::from(stride) + u64::from(x + column));
            unsafe { core::ptr::write_volatile(at as *mut u32, color) };
        }
    }
}

/// The `stage`-th square of the progress row.
pub fn mark(stage: u32) {
    square(stage * 28, 0, 24, 0x00ff_ffff);
}

const FONT: [[u8; 7]; 16] = [
    [0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110],
    [0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
    [0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111],
    [0b11110, 0b00001, 0b00001, 0b01110, 0b00001, 0b00001, 0b11110],
    [0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010],
    [0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110],
    [0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110],
    [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000],
    [0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110],
    [0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100],
    [0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001],
    [0b11110, 0b10001, 0b10001, 0b11110, 0b10001, 0b10001, 0b11110],
    [0b01110, 0b10001, 0b10000, 0b10000, 0b10000, 0b10001, 0b01110],
    [0b11100, 0b10010, 0b10001, 0b10001, 0b10001, 0b10010, 0b11100],
    [0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111],
    [0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000],
];

fn digit(x: u32, y: u32, value: u32, color: u32) {
    for (line, pattern) in FONT[(value & 15) as usize].iter().enumerate() {
        for column in 0..5u32 {
            if pattern >> (4 - column) & 1 != 0 {
                square(x + column * 3, y + line as u32 * 3, 3, color);
            }
        }
    }
}

/// `value` as 16 hex digits (3x scaled 5x7 font) on line `row`: a gray row number, then white digits. Rows 0-7 form the
/// left column, 8-15 the right one, each line 26 px high below the progress row.
pub fn bits(row: u32, value: u64) {
    let x0 = (row / 8) * 380;
    let y = 40 + (row % 8) * 26;
    digit(x0, y, row, 0x0080_8080);
    for index in 0..16u32 {
        digit(x0 + 24 + index * 20, y, (value >> (60 - 4 * index)) as u32, 0x00ff_ffff);
    }
}
