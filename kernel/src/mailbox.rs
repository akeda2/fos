//! The Raspberry Pi VideoCore mailbox, property channel 8 (aarch64 only): the ARM asks the firmware questions and gets answers
//! in the same buffer. Format and tags: the firmware wiki "Mailbox property interface"; the display tags (0x00040013 number of
//! displays, 0x00048013 select display, 0x00040016 display id) are from Linux's `raspberrypi-firmware.h`.
//!
//! Registers (BCM2711, base 0xFE00B880): +0x00 read, +0x18 read status (bit 30 = empty), +0x20 write, +0x38 write status
//! (bit 31 = full). A message is the buffer's bus address (16-byte aligned) ORed with the channel in the low four bits. The
//! buffer lives in the non-cacheable DMA pool, so no cache maintenance is needed; the VideoCore sees ARM memory at
//! +0xC0000000 (the uncached alias; the Pi 4 device tree's `dma-ranges`). **Unverified on hardware** when written: the bus
//! alias and the display-select tags. Every wait is bounded; a failed call returns `None`.

use crate::{memory, timer};

const BASE: u64 = 0xfe00_b880;
const READ: u64 = 0x00;
const READ_STATUS: u64 = 0x18;
const WRITE: u64 = 0x20;
const WRITE_STATUS: u64 = 0x38;
const EMPTY: u32 = 1 << 30;
const FULL: u32 = 1 << 31;
const CHANNEL: u32 = 8;
const BUS_ALIAS: u64 = 0xc000_0000;
const TIMEOUT_US: u64 = 200_000;

pub const GET_NUM_DISPLAYS: u32 = 0x0004_0013;
pub const GET_DISPLAY_ID: u32 = 0x0004_0016;
pub const SET_DISPLAY_NUM: u32 = 0x0004_8013;
pub const GET_PHYSICAL_SIZE: u32 = 0x0004_0003;
pub const GET_VIRTUAL_SIZE: u32 = 0x0004_0004;
pub const GET_DEPTH: u32 = 0x0004_0005;
pub const GET_PITCH: u32 = 0x0004_0008;
/// Property tag 0x00030020 (Linux `RPI_FIRMWARE_GET_EDID_BLOCK`): request the block number; the answer is block, status, 128 bytes.
pub const GET_EDID_BLOCK: u32 = 0x0003_0020;

fn read(offset: u64) -> u32 {
    unsafe { ((BASE + offset) as *const u32).read_volatile() }
}

fn write(offset: u64, value: u32) {
    unsafe { ((BASE + offset) as *mut u32).write_volatile(value) }
}

/// Posts the message in `page` and waits for the answer to the same buffer. Bounded.
fn transact(page: u64) -> bool {
    let message = ((page | BUS_ALIAS) as u32 & !0xf) | CHANNEL;
    let start = timer::micros();
    while read(WRITE_STATUS) & FULL != 0 {
        if timer::micros() - start > TIMEOUT_US {
            return false;
        }
    }
    write(WRITE, message);
    while timer::micros() - start <= TIMEOUT_US {
        if read(READ_STATUS) & EMPTY == 0 && read(READ) == message {
            return unsafe { (page as *const u32).add(1).read_volatile() } == 0x8000_0000;
        }
    }
    false
}

/// Asks the firmware for a 32-bit BGR framebuffer of `width` x `height` on `display` (selected first), and returns its address
/// (ARM side, the bus alias removed), size in bytes, pitch in bytes, and the width and height it really gave.
pub fn framebuffer(display: u32, width: u32, height: u32) -> Option<(u64, u64, u32, u32, u32)> {
    tag(SET_DISPLAY_NUM, &[display], &mut [0u32])?;
    let page = memory::allocate_contiguous(1, Some(1 << 32))?;
    let buffer = page as *mut u32;
    let message: [u32; 34] = [
        34 * 4, 0,
        SET_DISPLAY_NUM, 4, 0, display,
        0x0004_8003, 8, 0, width, height,
        0x0004_8004, 8, 0, width, height,
        0x0004_8005, 4, 0, 32,
        0x0004_8006, 4, 0, 0, // pixel order 0 = BGR: 0x00RRGGBB as a 32-bit word
        0x0004_0001, 8, 0, 4096, 0,
        GET_PITCH, 4, 0, 0,
        0,
    ];
    for (index, value) in message.iter().enumerate() {
        unsafe { buffer.add(index).write_volatile(*value) };
    }
    let ok = transact(page);
    let get = |index: usize| unsafe { buffer.add(index).read_volatile() };
    let result = (ok && get(26) & 0x8000_0000 != 0 && get(27) != 0)
        .then(|| (u64::from(get(27) & 0x3fff_ffff), u64::from(get(28)), get(32), get(9), get(10)));
    memory::free(page, 1);
    result
}

/// If the firmware drives a second display (Pi 4 with `max_framebuffers=2`), gives it a framebuffer the size of the console's
/// and registers it as tape screen 1 (at the monitor's EDID size when known). Display 0 is selected again afterwards. Silent when there is no second display.
pub fn second_screen() {
    let Some((width, height)) = crate::framebuffer::gfx_info() else { return };
    let mut count = [0u32];
    if tag(GET_NUM_DISPLAYS, &[], &mut count).is_none() || count[0] < 2 {
        return;
    }
    // The monitor's own size when its EDID is readable (capped at 1080p), else the console's.
    let _ = tag(SET_DISPLAY_NUM, &[1], &mut [0u32]);
    let (width, height) = match edid_native_size() {
        Some((w, h)) if (320..=1920).contains(&w) && (200..=1080).contains(&h) => (w, h),
        _ => (width, height),
    };
    let screen = framebuffer(1, width, height);
    let _ = tag(SET_DISPLAY_NUM, &[0], &mut [0u32]);
    let Some((base, size, pitch, got_width, got_height)) = screen else {
        crate::println!("Second screen: the firmware gave no framebuffer for display 1.");
        return;
    };
    crate::paging::map_framebuffer(base, size);
    crate::framebuffer::add_screen(base, got_width, got_height, pitch / 4);
    crate::println!("Second screen: display 1, {got_width}x{got_height} at {base:#x} (tape screen 1).");
}

/// Sends one property message holding a single tag with `request` as its value (the answer may be longer, up to `WORDS`),
/// and returns the first `reply.len()` answer words. `None` if the mailbox does not answer, or the firmware does not know
/// the tag (it then leaves the response bit clear).
pub fn tag(id: u32, request: &[u32], reply: &mut [u32]) -> Option<()> {
    crate::paging::map_device(BASE & !0xfff, 0x1000);
    let words = request.len().max(reply.len());
    let page = memory::allocate_contiguous(1, Some(1 << 32))?;
    let buffer = page as *mut u32;
    let put = |index: usize, value: u32| unsafe { buffer.add(index).write_volatile(value) };
    let get = |index: usize| unsafe { buffer.add(index).read_volatile() };
    put(0, ((6 + words) * 4) as u32);
    put(1, 0);
    put(2, id);
    put(3, (words * 4) as u32);
    put(4, 0);
    for (index, value) in request.iter().enumerate() {
        put(5 + index, *value);
    }
    put(5 + words, 0);
    let ok = transact(page);
    let answered = ok && get(4) & 0x8000_0000 != 0;
    if answered {
        for (index, slot) in reply.iter_mut().enumerate() {
            *slot = get(5 + index);
        }
    }
    memory::free(page, 1);
    answered.then_some(())
}

/// The native (first detailed timing) size from the selected display's EDID block 0, or `None` when the firmware has no EDID
/// (no monitor, the DSI panel) or the block is not valid. Pixel-clock 0 in the descriptor means it is not a timing.
pub fn edid_native_size() -> Option<(u32, u32)> {
    let mut reply = [0u32; 34];
    tag(GET_EDID_BLOCK, &[0], &mut reply)?;
    if reply[1] != 0 {
        return None;
    }
    let mut block = [0u8; 128];
    for (index, byte) in block.iter_mut().enumerate() {
        *byte = (reply[2 + index / 4] >> (8 * (index % 4))) as u8;
    }
    if block[..8] != [0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0] || block[54] | block[55] == 0 {
        return None;
    }
    let width = u32::from(block[56]) | (u32::from(block[58]) & 0xf0) << 4;
    let height = u32::from(block[59]) | (u32::from(block[61]) & 0xf0) << 4;
    Some((width, height))
}

/// The DSI panel is 800x480 (16:9) but the UEFI framebuffer on it is 800x600, so the picture does not fill it. When the
/// firmware drives two displays and display 0 is the main LCD (id 0, the DSI connector), asks it for its own 800x480
/// framebuffer; the console and tape screen 0 then use that. `None` (keep the UEFI one) in every other case.
pub fn dsi_framebuffer(current: &fosfor_abi::Framebuffer) -> Option<fosfor_abi::Framebuffer> {
    let mut count = [0u32];
    tag(GET_NUM_DISPLAYS, &[], &mut count)?;
    let mut id = [0u32];
    tag(GET_DISPLAY_ID, &[0], &mut id)?;
    if count[0] < 2 || id[0] != 0 || (current.width, current.height) != (800, 600) {
        return None;
    }
    let Some((base, size, pitch, width, height)) = framebuffer(0, 800, 480) else {
        crate::println!("DSI panel: the firmware gave no 800x480 framebuffer; keeping the UEFI one.");
        return None;
    };
    crate::println!("DSI panel: {width}x{height} framebuffer at {base:#x} (the UEFI one was {}x{}).", current.width, current.height);
    crate::paging::map_framebuffer(base, size);
    Some(fosfor_abi::Framebuffer { base, size, width, height, stride: pitch / 4, format: fosfor_abi::PIXEL_BGR })
}
