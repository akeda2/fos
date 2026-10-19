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

/// `value` as 64 squares (16 px pitch) on row `row` (each row 20 px high, below the progress row).
pub fn bits(row: u32, value: u64) {
    for bit in 0..64u32 {
        let set = value >> (63 - bit) & 1 != 0;
        square(bit * 12, 40 + row * 20, 10, if set { 0x00ff_ffff } else { 0x0040_4040 });
    }
}
