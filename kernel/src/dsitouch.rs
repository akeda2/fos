//! The touch controller of a Raspberry Pi 4 DSI display: an FT5406-style chip (FocalTech, I2C address 0x38) on the display
//! connector's I2C bus (BSC0 on GPIO44/45; `i2c bus dsi`). The DFRobot 4.5" panel answers like the official 7" screen:
//! the panel's own controller at 0x45 (not used here) and this chip at 0x38. aarch64 only.
//!
//! Register map (as Linux's `edt-ft5x06` / `rpi-ft5406` read it): 0x02 = the number of touches (low 4 bits), then 6 bytes
//! per touch from 0x03: byte 0 = event flags in the top two bits (0 down, 1 lift, 2 contact, 3 none) and X high in the
//! low nibble, byte 1 = X low, byte 2 = touch id in the top nibble and Y high in the low one, byte 3 = Y low. The chip has
//! no interrupt line we use: `poll` reads the registers (rate limited) when something asks for an event, and turns the
//! change since the last read into finger events for `usb_pointer`'s queue, so the shell and tapes see one kind of touch.
//! Coordinates are the chip's, 0..`WIDTH` and 0..`HEIGHT` (the official panel's 800 x 480; checked on the DFRobot), both axes
//! flipped: the DFRobot panel reports them turned 180 degrees from the way the console and tapes draw (corner taps on the Pi).

use crate::usb_pointer::{self, TouchEvent};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

const ADDRESS: u8 = 0x38;
pub const WIDTH: i32 = 800;
pub const HEIGHT: i32 = 480;
const MAX_TOUCHES: usize = 10;
/// At most one read per this many microseconds; at 400 kHz a read of the whole map takes about 1.5 ms.
const INTERVAL_US: u64 = 8_000;
const READ_BYTES: usize = 3 + 6 * MAX_TOUCHES;

static PRESENT: AtomicBool = AtomicBool::new(false);
static LAST_US: AtomicU64 = AtomicU64::new(0);
static mut DOWN: [Option<(i32, i32)>; 16] = [None; 16];
static REPORTS: AtomicU64 = AtomicU64::new(0);

/// Looks for the chip once at boot; true when found.
pub fn init() -> bool {
    let mut one = [0u8; 1];
    let found = crate::i2c::read_dsi(ADDRESS, &mut one, 400_000).is_ok();
    PRESENT.store(found, Ordering::Relaxed);
    found
}

pub fn present() -> bool {
    PRESENT.load(Ordering::Relaxed)
}

pub fn reports() -> u64 {
    REPORTS.load(Ordering::Relaxed)
}

/// Reads the chip if it is time and queues what changed.
pub fn poll(panel: u8) {
    if !present() {
        return;
    }
    let now = crate::timer::micros();
    if now.wrapping_sub(LAST_US.load(Ordering::Relaxed)) < INTERVAL_US {
        return;
    }
    LAST_US.store(now, Ordering::Relaxed);
    let mut map = [0u8; READ_BYTES];
    if crate::i2c::write_read_dsi(ADDRESS, &[0], &mut map, 400_000).is_err() {
        return;
    }
    REPORTS.fetch_add(1, Ordering::Relaxed);
    let touches = usize::from(map[2] & 0x0f);
    if touches > MAX_TOUCHES {
        return; // a garbled read (the chip answers 0xff while it is busy)
    }
    let down = unsafe { &mut *core::ptr::addr_of_mut!(DOWN) };
    let mut seen = [false; 16];
    for touch in map[3..].chunks_exact(6).take(touches) {
        let (flag, id) = (touch[0] >> 6, usize::from(touch[2] >> 4));
        let (x, y) = (WIDTH - ((i32::from(touch[0] & 0x0f) << 8) | i32::from(touch[1])), HEIGHT - ((i32::from(touch[2] & 0x0f) << 8) | i32::from(touch[3])));
        if flag == 1 || flag == 3 {
            continue; // lifted: handled below as "not seen"
        }
        seen[id] = true;
        if down[id] != Some((x, y)) {
            down[id] = Some((x, y));
            usb_pointer::push_event(TouchEvent { panel, id: id as u8, x, y, down: true });
        }
    }
    for (id, entry) in down.iter_mut().enumerate() {
        if let (Some((x, y)), false) = (*entry, seen[id]) {
            *entry = None;
            usb_pointer::push_event(TouchEvent { panel, id: id as u8, x, y, down: false });
        }
    }
}
