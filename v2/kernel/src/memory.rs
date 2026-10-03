//! The physical page allocator: one bitmap over every 4 KiB frame of the machine.
//!
//! v1 needed two pools, a 128 MiB fine-grained one and a coarse 1 MiB one up to
//! 2 GiB, because its whole kernel (and so its bitmap) had to fit below 640 KiB.
//! v2 has no such limit: the bitmap is carved out of the first big enough free
//! region at boot and sized for however much RAM the firmware reports, so a 16 GiB
//! machine is one pool like a 16 MiB one. Callers that need memory a 32-bit-DMA
//! device can reach ask for it explicitly (`below`).
//!
//! What counts as free after `ExitBootServices`: conventional memory, plus the
//! firmware's boot-services code and data (nothing uses them any more). Loader
//! code/data stays reserved (it holds the kernel image, `BootInfo` and the memory
//! map), as do the runtime-services regions, ACPI tables, MMIO and everything else.

use fos_v2_abi::BootInfo;

pub const FRAME: u64 = 4096;

/// UEFI memory types the kernel may reuse.
const FREE_TYPES: [u32; 3] = [3, 4, 7];

struct Allocator {
    /// One bit per frame, set = in use / not RAM.
    bitmap: *mut u64,
    frames: u64,
    free: u64,
    /// Where the next search starts (a rolling hint, wraps).
    hint: u64,
}

static mut ALLOCATOR: Allocator = Allocator { bitmap: core::ptr::null_mut(), frames: 0, free: 0, hint: 0 };

fn allocator() -> &'static mut Allocator {
    unsafe { &mut *core::ptr::addr_of_mut!(ALLOCATOR) }
}

impl Allocator {
    fn is_used(&self, frame: u64) -> bool {
        unsafe { self.bitmap.add((frame / 64) as usize).read() & (1 << (frame % 64)) != 0 }
    }

    fn set(&mut self, frame: u64, used: bool) {
        let word = unsafe { &mut *self.bitmap.add((frame / 64) as usize) };
        let was = *word & (1 << (frame % 64)) != 0;
        if used {
            *word |= 1 << (frame % 64);
        } else {
            *word &= !(1 << (frame % 64));
        }
        if was && !used {
            self.free += 1;
        } else if !was && used {
            self.free -= 1;
        }
    }
}

/// Totals for the boot narration.
pub struct Stats {
    pub frames: u64,
    pub free: u64,
}

pub fn stats() -> Stats {
    let a = allocator();
    Stats { frames: a.frames, free: a.free }
}

fn align_up(value: u64, to: u64) -> u64 {
    (value + to - 1) & !(to - 1)
}

/// Builds the allocator from `info`'s memory map. `reserved` lists physical ranges
/// that must stay out of the free pool even if the map calls them free (the kernel
/// image, the `BootInfo`, the memory map itself).
pub fn init(info: &BootInfo, reserved: &[(u64, u64)]) {
    let mut highest = 0u64;
    for descriptor in info.memory_descriptors() {
        if FREE_TYPES.contains(&descriptor.kind) {
            highest = highest.max(descriptor.physical_start + descriptor.pages * FRAME);
        }
    }
    let frames = highest / FRAME;
    let words = frames.div_ceil(64);
    let bitmap_bytes = align_up(words * 8, FRAME);

    // Find a free region above 1 MiB (below it sits legacy hardware and BIOS-era
    // structures) big enough for the bitmap, and put it at the region's start.
    let mut bitmap_at = None;
    for descriptor in info.memory_descriptors() {
        let (start, end) = (descriptor.physical_start.max(0x100000), descriptor.physical_start + descriptor.pages * FRAME);
        if descriptor.kind == 7 && end > start && end - start >= bitmap_bytes {
            bitmap_at = Some(align_up(start, FRAME));
            break;
        }
    }
    let Some(bitmap_at) = bitmap_at else { crate::cpu::halt() };

    let a = allocator();
    a.bitmap = bitmap_at as *mut u64;
    a.frames = frames;
    a.free = 0;
    // Start with everything in use, then free what the map allows.
    for word in 0..words {
        unsafe { a.bitmap.add(word as usize).write(u64::MAX) };
    }
    for descriptor in info.memory_descriptors() {
        if !FREE_TYPES.contains(&descriptor.kind) {
            continue;
        }
        let first = descriptor.physical_start / FRAME;
        for frame in first..(first + descriptor.pages).min(frames) {
            // Never hand out the first MiB: BIOS-era data, and address 0 stays unmapped.
            if frame * FRAME >= 0x100000 {
                a.set(frame, false);
            }
        }
    }
    reserve(bitmap_at, bitmap_bytes);
    for &(start, length) in reserved {
        reserve(start, length);
    }
}

/// Takes `[start, start + length)` out of the free pool.
pub fn reserve(start: u64, length: u64) {
    let a = allocator();
    let first = start / FRAME;
    let last = (start + length).div_ceil(FRAME);
    for frame in first..last.min(a.frames) {
        a.set(frame, true);
    }
}

/// `count` physically contiguous frames, zeroed, optionally entirely below
/// `below` (exclusive). Returns the physical address.
pub fn allocate_contiguous(count: u64, below: Option<u64>) -> Option<u64> {
    let a = allocator();
    let limit = below.map_or(a.frames, |address| (address / FRAME).min(a.frames));
    if count == 0 || count > limit {
        return None;
    }
    // Two passes over the rolling hint: from the hint to the end, then from the start.
    for (from, to) in [(a.hint.min(limit), limit), (0, a.hint.min(limit))] {
        let mut run = 0u64;
        let mut frame = from;
        while frame < to {
            if a.is_used(frame) {
                run = 0;
            } else {
                run += 1;
                if run == count {
                    let first = frame + 1 - count;
                    for taken in first..=frame {
                        a.set(taken, true);
                    }
                    a.hint = frame + 1;
                    unsafe { core::ptr::write_bytes((first * FRAME) as *mut u8, 0, (count * FRAME) as usize) };
                    return Some(first * FRAME);
                }
            }
            frame += 1;
        }
    }
    None
}

/// One zeroed frame anywhere.
pub fn allocate() -> Option<u64> {
    allocate_contiguous(1, None)
}

/// Returns frames to the pool. Frames outside the managed range, or already free,
/// are ignored (and a free of a reserved region would be a kernel bug).
pub fn free(address: u64, count: u64) {
    let a = allocator();
    for frame in address / FRAME..address / FRAME + count {
        if frame < a.frames && a.is_used(frame) {
            a.set(frame, false);
        }
    }
}
