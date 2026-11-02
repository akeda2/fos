//! The physical frame allocator: one bitmap over every 4 KiB frame, built from the loader's memory map.
//!
//! Like the embedded kernel's, but reached through `vm::direct`, so it keeps working after the switch to the kernel's own
//! page tables (the bitmap is a *physical* address; before paging `direct` is the identity, afterwards the direct map).
//! What counts as free: conventional memory plus the firmware's boot-services code and data, above 1 MiB. Everything else
//! (loader data, ACPI, MMIO, runtime services) stays reserved.
//!
//! Not yet: per-NUMA-node free lists (they need the ACPI SRAT, slice 3) and a lock (slice 4, before a second CPU runs).

use crate::boot::Machine;
use crate::vm;

pub const FRAME: u64 = 4096;

/// UEFI memory types the kernel may reuse: boot-services code and data, conventional memory.
const FREE_TYPES: [u32; 3] = [3, 4, 7];

struct State {
    /// Physical address of the bitmap: one bit per frame, set = in use or not RAM.
    bitmap: u64,
    frames: u64,
    free: u64,
    managed: u64,
    hint: u64,
}

static mut STATE: State = State { bitmap: 0, frames: 0, free: 0, managed: 0, hint: 0 };

fn state() -> &'static mut State {
    unsafe { &mut *core::ptr::addr_of_mut!(STATE) }
}

fn align_up(value: u64, to: u64) -> u64 {
    (value + to - 1) & !(to - 1)
}

impl State {
    fn word(&self, frame: u64) -> *mut u64 {
        (vm::direct(self.bitmap) as *mut u64).wrapping_add((frame / 64) as usize)
    }

    fn is_used(&self, frame: u64) -> bool {
        unsafe { self.word(frame).read() & (1 << (frame % 64)) != 0 }
    }

    fn set(&mut self, frame: u64, used: bool) {
        let word = self.word(frame);
        let bit = 1 << (frame % 64);
        unsafe {
            let was = word.read() & bit != 0;
            word.write(if used { word.read() | bit } else { word.read() & !bit });
            if was && !used {
                self.free += 1;
            } else if !was && used {
                self.free -= 1;
            }
        }
    }
}

pub struct Stats {
    /// Frames the allocator was given to manage (the free types at boot, above 1 MiB, minus what was reserved).
    pub managed: u64,
    pub free: u64,
}

pub fn stats() -> Stats {
    let s = state();
    Stats { managed: s.managed, free: s.free }
}

/// Builds the allocator. `reserved` lists physical ranges to keep out of the pool even where the map calls them free.
/// Runs before paging: the bitmap is carved from the first big enough free region and addressed by identity.
pub fn init(machine: &Machine, reserved: &[(u64, u64)]) {
    let mut highest = 0;
    for d in machine.descriptors() {
        if FREE_TYPES.contains(&d.kind) {
            highest = highest.max(d.physical_start + d.pages * FRAME);
        }
    }
    let frames = highest / FRAME;
    let words = frames.div_ceil(64);
    let bytes = align_up(words * 8, FRAME);

    let mut at = None;
    for d in machine.descriptors() {
        let start = d.physical_start.max(0x10_0000);
        let end = d.physical_start + d.pages * FRAME;
        if d.kind == 7 && end > start && end - start >= bytes {
            at = Some(align_up(start, FRAME));
            break;
        }
    }
    let Some(at) = at else {
        println!("pmm: no free region of {bytes} bytes for the frame bitmap: stopping.");
        crate::halt();
    };

    let s = state();
    s.bitmap = at;
    s.frames = frames;
    s.free = 0;
    for word in 0..words {
        unsafe { (at as *mut u64).add(word as usize).write(u64::MAX) };
    }
    for d in machine.descriptors() {
        if !FREE_TYPES.contains(&d.kind) {
            continue;
        }
        let first = d.physical_start / FRAME;
        for frame in first..(first + d.pages).min(frames) {
            // The first MiB is BIOS-era data, and address 0 stays unmapped.
            if frame * FRAME >= 0x10_0000 {
                s.set(frame, false);
            }
        }
    }
    reserve(at, bytes);
    for &(start, length) in reserved {
        reserve(start, length);
    }
    s.managed = s.free;
}

/// Takes `[start, start + length)` out of the free pool.
pub fn reserve(start: u64, length: u64) {
    let s = state();
    for frame in start / FRAME..(start + length).div_ceil(FRAME).min(s.frames) {
        s.set(frame, true);
    }
}

/// `count` physically contiguous frames, optionally entirely below `below` (exclusive): the physical address, not cleared.
pub fn allocate_uncleared(count: u64, below: Option<u64>) -> Option<u64> {
    let s = state();
    let limit = below.map_or(s.frames, |address| (address / FRAME).min(s.frames));
    if count == 0 || count > limit {
        return None;
    }
    let hint = s.hint.min(limit);
    // Two passes over the rolling hint: from the hint to the limit, then from the start.
    for (from, to) in [(hint, limit), (0, hint)] {
        let mut run = 0;
        for frame in from..to {
            if s.is_used(frame) {
                run = 0;
            } else {
                run += 1;
                if run == count {
                    let first = frame + 1 - count;
                    for taken in first..=frame {
                        s.set(taken, true);
                    }
                    s.hint = frame + 1;
                    return Some(first * FRAME);
                }
            }
        }
    }
    None
}

/// The same, zeroed (through the direct map).
pub fn allocate(count: u64, below: Option<u64>) -> Option<u64> {
    let address = allocate_uncleared(count, below)?;
    unsafe { core::ptr::write_bytes(vm::direct(address) as *mut u8, 0, (count * FRAME) as usize) };
    Some(address)
}

/// Returns frames to the pool; frames outside the managed range or already free are ignored.
pub fn free(address: u64, count: u64) {
    let s = state();
    for frame in address / FRAME..address / FRAME + count {
        if frame < s.frames && s.is_used(frame) {
            s.set(frame, false);
        }
    }
}
