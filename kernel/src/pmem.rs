//! A deliberately small physical-frame allocator.
//!
//! It builds a fixed-size bitmap (one bit per 4 KiB frame) from the E820
//! memory map the loader collected, marks everything below the reserved
//! low-memory kernel region (see `linker.ld`, `__kernel_end < 0x40000`) as
//! permanently unavailable, and hands out/reclaims individual frames. There
//! is no paging here yet -- this only tracks which physical frames are free.

use crate::boot_info::{BootInfo, MemoryRegion, read_u32, read_u64};

pub const FRAME_SIZE: u32 = 4096;

/// Frames are tracked up to this physical address; anything beyond it is
/// simply never marked free. 128 MiB is comfortably above what QEMU's
/// default E820 map and this kernel's own small allocations (disk/NIC
/// buffers, `palloc`/`pfree`) need at this fine 4 KiB granularity. Memory
/// beyond this is still reachable -- see `BIG_POOL_BASE`/`allocate_big`
/// below -- just not tracked bit-per-4-KiB-frame, since every byte of
/// `BITMAP` below is baked directly into the shipped `kernel.bin` (a flat
/// binary's `.bss` is zero-padded into the file by `objcopy -O binary`,
/// not merely reserved at load time), and `boot/stage2.S` loads the whole
/// kernel with a single INT13h extended read capped at 127 sectors /
/// 65024 bytes (see `tools/write_layout.py`) to stay within one real-mode
/// segment. A bitmap fine enough to track a full 1 GiB machine at this
/// same 4 KiB granularity (32 KiB) would blow well past that ceiling by
/// itself.
const TRACKED_BYTES: u32 = 128 * 1024 * 1024;
const MAX_FRAMES: usize = (TRACKED_BYTES / FRAME_SIZE) as usize;
const BITMAP_BYTES: usize = MAX_FRAMES.div_ceil(8);

/// Frames below this physical address are never handed out: it covers the
/// real-mode IVT/BDA, the BootInfo table, and the kernel's own reserved
/// low-memory region.
const RESERVED_BELOW: u32 = 0x40000;

/// Fixed physical address every "tape" (see `shell.rs`'s `tape_run`) is
/// always loaded at, and the size of the region set aside for it. Unlike
/// every other allocation this module hands out, a tape's address must
/// never vary from run to run: that's what lets a tape's own compiled code
/// (assembly, C, or Rust) address itself with ordinary absolute addresses
/// instead of needing to be position-independent and locate its own
/// runtime base first, the same way the kernel itself gets away with plain
/// addressing by always loading at the fixed `0x10000` `linker.ld` picks.
/// 4 MiB is comfortably above `tapes/primes.S`'s ~1 MiB binary (most
/// of which is its embedded sieve array) with headroom for future tapes.
pub const TAPE_BASE: u32 = 0x100000;
pub const TAPE_LEN: u32 = 4 * 1024 * 1024;

/// One bit per frame. `1` means "not available to the allocator" (either
/// outside a usable E820 region, below `RESERVED_BELOW`, or already
/// allocated). Starts fully reserved; `initialize` clears bits for usable
/// frames.
static mut BITMAP: [u8; BITMAP_BYTES] = [0xff; BITMAP_BYTES];
static mut TOTAL_FRAMES: usize = 0;
static mut FREE_FRAMES: usize = 0;
static mut NEXT_SEARCH: usize = 0;

/// A second, much coarser pool, used only by a tape's `alloc`/`free`
/// callbacks (`shell::tape_alloc`/`tape_free`) -- entirely separate from
/// `BITMAP` above, which stays exactly the size the fine 4 KiB frame
/// allocator has always needed (see `TRACKED_BYTES`'s doc comment for why
/// that matters). A tape wanting far more than its own fixed 4 MiB
/// region (a big sieve, a large buffer, ...) only ever wants one large
/// contiguous block at a time, so tracking this pool in whole 1 MiB
/// chunks instead of 4 KiB frames costs 256x less bitmap space for the
/// same amount of RAM -- reaching a full 1 GiB machine costs a little
/// over 100 bytes here, not the 32 KiB the fine bitmap would need, which
/// would have exceeded the old single-transfer kernel-load ceiling (see
/// `TRACKED_BYTES`'s doc comment).
pub const BIG_CHUNK_SIZE: u32 = 1024 * 1024;
/// Starts exactly where the fine-grained pool's own tracked window ends,
/// so the two pools can never both claim the same physical memory.
pub const BIG_POOL_BASE: u32 = TRACKED_BYTES;
/// Total physical address space this pool tracks, `BIG_POOL_BASE`
/// through this address -- 1 GiB, matching the target physical machine
/// this kernel is validated against (see AGENTS.md). A machine with less
/// RAM than this simply never has the extra chunks marked free in the
/// first place (see `initialize`); raising this constant costs only a
/// little more bitmap space, never memory that has to actually exist.
pub const BIG_POOL_END: u32 = 1024 * 1024 * 1024;
const BIG_POOL_CHUNKS: usize = ((BIG_POOL_END - BIG_POOL_BASE) / BIG_CHUNK_SIZE) as usize;
const BIG_BITMAP_BYTES: usize = BIG_POOL_CHUNKS.div_ceil(8);

static mut BIG_BITMAP: [u8; BIG_BITMAP_BYTES] = [0xff; BIG_BITMAP_BYTES];
static mut BIG_TOTAL_CHUNKS: usize = 0;
static mut BIG_FREE_CHUNKS: usize = 0;

fn big_chunk_index(address: u32) -> usize {
    ((address - BIG_POOL_BASE) / BIG_CHUNK_SIZE) as usize
}

fn big_is_free(index: usize) -> bool {
    unsafe { BIG_BITMAP[index / 8] & (1 << (index % 8)) == 0 }
}

fn big_set_free(index: usize, free: bool) {
    unsafe {
        if free {
            BIG_BITMAP[index / 8] &= !(1 << (index % 8));
        } else {
            BIG_BITMAP[index / 8] |= 1 << (index % 8);
        }
    }
}

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
        let region: MemoryRegion =
            unsafe { core::ptr::read_unaligned(core::ptr::addr_of!(info.memory_regions[index])) };
        if read_u32(core::ptr::addr_of!(region.kind)) != 1 {
            continue; // only "usable" (type 1) regions are ever free.
        }
        let base = read_u64(core::ptr::addr_of!(region.base));
        let length = read_u64(core::ptr::addr_of!(region.length));
        mark_region_free(base, length);
        mark_big_region_free(base, length);
    }
    // Carve the fixed tape region back out of the general pool, the same
    // way frames below RESERVED_BELOW never entered it: this must happen
    // after the E820-driven marking above (which would otherwise mark
    // these frames free again) and before any allocation, so
    // `allocate`/`allocate_contiguous`/`palloc` can never hand these
    // frames to anything else.
    reserve_range(TAPE_BASE, TAPE_LEN);
    let (total, _) = stats();
    let (big_total, _) = big_stats();
    crate::println!(
        "Physical memory: {total} frames ({} KiB) tracked above 0x{RESERVED_BELOW:05x}, excluding 0x{TAPE_BASE:05x}-0x{:05x} reserved for tapes; use 'pmem'/'palloc'/'pfree' to inspect.",
        u64::from(total as u32) * u64::from(FRAME_SIZE) / 1024,
        TAPE_BASE + TAPE_LEN
    );
    crate::println!(
        "Additionally tracked in 1 MiB chunks from 0x{BIG_POOL_BASE:08x}-0x{BIG_POOL_END:08x} for a tape's own 'alloc'/'free' callbacks: {big_total} MiB usable so far this boot."
    );
}

/// Removes `[base, base + length)` from the free pool (and from the total
/// tracked count) without ever having offered it to `allocate`/
/// `allocate_contiguous` -- `mark_region_free`'s opposite, used once at
/// boot for the fixed tape region.
fn reserve_range(base: u32, length: u32) {
    let start = frame_index(base);
    let end = frame_index(base + length);
    for index in start..end.min(MAX_FRAMES) {
        if is_free(index) {
            set_free(index, false);
            unsafe {
                TOTAL_FRAMES -= 1;
                FREE_FRAMES -= 1;
            }
        }
    }
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

/// `mark_region_free`'s exact counterpart for the coarse "big pool" above
/// -- only whole 1 MiB chunks fully inside the region, and inside
/// `[BIG_POOL_BASE, BIG_POOL_END)`, are marked free; partial edge chunks
/// are conservatively left reserved, same reasoning as `mark_region_free`.
fn mark_big_region_free(base: u64, length: u64) {
    let start = base.div_ceil(u64::from(BIG_CHUNK_SIZE)) * u64::from(BIG_CHUNK_SIZE);
    let end = (base + length) / u64::from(BIG_CHUNK_SIZE) * u64::from(BIG_CHUNK_SIZE);
    if end <= start {
        return;
    }
    let start = start.max(u64::from(BIG_POOL_BASE));
    let end = end.min(u64::from(BIG_POOL_END));
    let mut address = start;
    while address < end {
        let index = big_chunk_index(address as u32);
        if !big_is_free(index) {
            big_set_free(index, true);
            unsafe {
                BIG_TOTAL_CHUNKS += 1;
                BIG_FREE_CHUNKS += 1;
            }
        }
        address += u64::from(BIG_CHUNK_SIZE);
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

/// Allocates `count` *physically contiguous* frames, returning the base
/// address of the first one, or `None` if no run of that length is free.
/// Nothing before this needed contiguity (the disk code moves one sector
/// at a time; each UART is a single fixed register set) -- but a NIC's
/// receive ring is one buffer the hardware writes into with its own DMA
/// engine, so it must be one unbroken block of physical memory.
pub fn allocate_contiguous(count: usize) -> Option<u32> {
    if count == 0 || count > MAX_FRAMES {
        return None;
    }
    unsafe {
        let mut run_start = 0usize;
        let mut run_length = 0usize;
        for index in 0..MAX_FRAMES {
            if is_free(index) {
                if run_length == 0 {
                    run_start = index;
                }
                run_length += 1;
                if run_length == count {
                    for offset in 0..count {
                        set_free(run_start + offset, false);
                    }
                    FREE_FRAMES -= count;
                    NEXT_SEARCH = (run_start + count) % MAX_FRAMES;
                    return Some(run_start as u32 * FRAME_SIZE);
                }
            } else {
                run_length = 0;
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

/// Allocates `count` *physically contiguous* 1 MiB chunks from the coarse
/// "big pool" (see its own doc comment above), returning the base address
/// of the first one, or `None` if no run of that length is free. Used
/// only by `shell::tape_alloc`, for a tape needing far more RAM than its
/// own fixed 4 MiB region -- entirely separate storage from
/// `allocate_contiguous` above, so the two pools never contend with each
/// other or with each other's callers.
pub fn allocate_big_contiguous(count: usize) -> Option<u32> {
    if count == 0 || count > BIG_POOL_CHUNKS {
        return None;
    }
    unsafe {
        let mut run_start = 0usize;
        let mut run_length = 0usize;
        for index in 0..BIG_POOL_CHUNKS {
            if big_is_free(index) {
                if run_length == 0 {
                    run_start = index;
                }
                run_length += 1;
                if run_length == count {
                    for offset in 0..count {
                        big_set_free(run_start + offset, false);
                    }
                    BIG_FREE_CHUNKS -= count;
                    return Some(BIG_POOL_BASE + run_start as u32 * BIG_CHUNK_SIZE);
                }
            } else {
                run_length = 0;
            }
        }
    }
    None
}

/// Returns `count` previously allocated 1 MiB chunks (starting at
/// `address`) to the big pool's free list. Chunks outside the tracked
/// range, or already free, are ignored rather than corrupting accounting
/// -- the same lenient contract `free` above already has for the fine
/// frame pool.
pub fn free_big(address: u32, count: usize) {
    if address < BIG_POOL_BASE || (address - BIG_POOL_BASE) % BIG_CHUNK_SIZE != 0 {
        return;
    }
    let start = big_chunk_index(address);
    for offset in 0..count {
        let index = start + offset;
        if index >= BIG_POOL_CHUNKS || big_is_free(index) {
            continue;
        }
        big_set_free(index, true);
        unsafe {
            BIG_FREE_CHUNKS += 1;
        }
    }
}

/// Returns `(total tracked big-pool chunks, free chunks)` -- each chunk
/// is exactly 1 MiB (`BIG_CHUNK_SIZE`), so these numbers double as MiB.
pub fn big_stats() -> (usize, usize) {
    unsafe { (BIG_TOTAL_CHUNKS, BIG_FREE_CHUNKS) }
}

fn read_u16(pointer: *const u16) -> u16 {
    unsafe { core::ptr::read_unaligned(pointer) }
}
