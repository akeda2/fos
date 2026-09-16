//! A deliberately small physical-frame allocator.
//!
//! It builds a fixed-size bitmap (one bit per 4 KiB frame) from the E820
//! memory map the loader collected, marks everything below the reserved
//! low-memory kernel region (see `linker.ld`, `__kernel_end < 0x20000`) as
//! permanently unavailable, and hands out/reclaims individual frames. There
//! is no paging here yet -- this only tracks which physical frames are free.

use crate::boot_info::{read_u32, read_u64, BootInfo, MemoryRegion};

pub const FRAME_SIZE: u32 = 4096;

/// Frames are tracked up to this physical address; anything beyond it is
/// simply never marked free. 128 MiB is comfortably above what QEMU's
/// default E820 map and this kernel's teaching scope need.
const TRACKED_BYTES: u32 = 128 * 1024 * 1024;
const MAX_FRAMES: usize = (TRACKED_BYTES / FRAME_SIZE) as usize;
const BITMAP_BYTES: usize = MAX_FRAMES.div_ceil(8);

/// Frames below this physical address are never handed out: it covers the
/// real-mode IVT/BDA, the BootInfo table, and the kernel's own reserved
/// low-memory region.
const RESERVED_BELOW: u32 = 0x20000;

/// One bit per frame. `1` means "not available to the allocator" (either
/// outside a usable E820 region, below `RESERVED_BELOW`, or already
/// allocated). Starts fully reserved; `initialize` clears bits for usable
/// frames.
static mut BITMAP: [u8; BITMAP_BYTES] = [0xff; BITMAP_BYTES];
static mut TOTAL_FRAMES: usize = 0;
static mut FREE_FRAMES: usize = 0;
static mut NEXT_SEARCH: usize = 0;

fn frame_index(address: u32) -> usize {
    (address / FRAME_SIZE) as usize
}

fn is_free(index: usize) -> bool {
    unsafe { BITMAP[index / 8] & (1 << (index % 8)) == 0 }
}

fn set_free(index: usize, free: bool) {
    unsafe {
        if free {
            BITMAP[index / 8] &= !(1 << (index % 8));
        } else {
            BITMAP[index / 8] |= 1 << (index % 8);
        }
    }
}

/// Populates the bitmap from the loader-provided E820 map. Idempotent, but
/// only meant to run once, before any allocation.
pub fn initialize(info: &BootInfo) {
    let count = usize::from(read_u16(core::ptr::addr_of!(info.memory_region_count)));
    for index in 0..count {
        let region: MemoryRegion = unsafe { core::ptr::read_unaligned(core::ptr::addr_of!(info.memory_regions[index])) };
        if read_u32(core::ptr::addr_of!(region.kind)) != 1 {
            continue; // only "usable" (type 1) regions are ever free.
        }
        mark_region_free(read_u64(core::ptr::addr_of!(region.base)), read_u64(core::ptr::addr_of!(region.length)));
    }
    let (total, _) = stats();
    crate::println!(
        "Physical memory: {total} frames ({} KiB) tracked above 0x{RESERVED_BELOW:05x}; use 'pmem'/'palloc'/'pfree' to inspect.",
        u64::from(total as u32) * u64::from(FRAME_SIZE) / 1024
    );
}

fn mark_region_free(base: u64, length: u64) {
    // Only whole frames fully inside the region, at or above RESERVED_BELOW,
    // and inside the tracked window are marked free; partial edge frames are
    // conservatively left reserved.
    let start = base.div_ceil(u64::from(FRAME_SIZE)) * u64::from(FRAME_SIZE);
    let end = (base + length) / u64::from(FRAME_SIZE) * u64::from(FRAME_SIZE);
    if end <= start {
        return;
    }
    let start = start.max(u64::from(RESERVED_BELOW));
    let end = end.min(u64::from(TRACKED_BYTES));
    let mut address = start;
    while address < end {
        let index = frame_index(address as u32);
        if !is_free(index) {
            set_free(index, true);
            unsafe {
                TOTAL_FRAMES += 1;
                FREE_FRAMES += 1;
            }
        }
        address += u64::from(FRAME_SIZE);
    }
}

/// Allocates one physical frame, returning its base address, or `None` if
/// every tracked frame is in use.
pub fn allocate() -> Option<u32> {
    unsafe {
        for offset in 0..MAX_FRAMES {
            let index = (NEXT_SEARCH + offset) % MAX_FRAMES;
            if is_free(index) {
                set_free(index, false);
                FREE_FRAMES -= 1;
                NEXT_SEARCH = (index + 1) % MAX_FRAMES;
                return Some(index as u32 * FRAME_SIZE);
            }
        }
    }
    None
}

/// Returns a previously allocated frame to the free pool. Frames outside the
/// tracked/reserved range, or already free, are ignored rather than
/// corrupting accounting.
pub fn free(address: u32) {
    if address < RESERVED_BELOW || address % FRAME_SIZE != 0 {
        return;
    }
    let index = frame_index(address);
    if index >= MAX_FRAMES || is_free(index) {
        return;
    }
    set_free(index, true);
    unsafe {
        FREE_FRAMES += 1;
    }
}

/// Returns `(total tracked frames, free frames)`.
pub fn stats() -> (usize, usize) {
    unsafe { (TOTAL_FRAMES, FREE_FRAMES) }
}

fn read_u16(pointer: *const u16) -> u16 {
    unsafe { core::ptr::read_unaligned(pointer) }
}
