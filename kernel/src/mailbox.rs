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

fn read(offset: u64) -> u32 {
    unsafe { ((BASE + offset) as *const u32).read_volatile() }
}

fn write(offset: u64, value: u32) {
    unsafe { ((BASE + offset) as *mut u32).write_volatile(value) }
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
    let message = ((page | BUS_ALIAS) as u32 & !0xf) | CHANNEL;
    let start = timer::micros();
    let mut ok = false;
    while read(WRITE_STATUS) & FULL != 0 {
        if timer::micros() - start > TIMEOUT_US {
            memory::free(page, 1);
            return None;
        }
    }
    write(WRITE, message);
    while timer::micros() - start <= TIMEOUT_US {
        if read(READ_STATUS) & EMPTY == 0 && read(READ) == message {
            ok = true;
            break;
        }
    }
    let answered = ok && get(1) == 0x8000_0000 && get(4) & 0x8000_0000 != 0;
    if answered {
        for (index, slot) in reply.iter_mut().enumerate() {
            *slot = get(5 + index);
        }
    }
    memory::free(page, 1);
    answered.then_some(())
}
