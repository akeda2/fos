//! The kernel heap: a first-fit free list over memory taken from the frame allocator through the direct map.
//!
//! Free blocks are address-ordered and coalesce on free. When no block fits, the heap takes another chunk of contiguous
//! frames (at least `CHUNK`) and carries on, so it grows with use; it never shrinks. There is no lock yet: one CPU runs the
//! kernel until slice 4, which adds the spinlock before the first AP starts.

use crate::{pmm, vm};
use core::alloc::{GlobalAlloc, Layout};

const CHUNK: u64 = 2 * 1024 * 1024;
const MIN_BLOCK: usize = 16;

#[repr(C)]
struct Free {
    size: usize,
    next: *mut Free,
}

struct Heap {
    head: *mut Free,
    total: usize,
    free: usize,
}

static mut HEAP: Heap = Heap { head: core::ptr::null_mut(), total: 0, free: 0 };

fn heap() -> &'static mut Heap {
    unsafe { &mut *core::ptr::addr_of_mut!(HEAP) }
}

fn align_up(value: usize, to: usize) -> usize {
    (value + to - 1) & !(to - 1)
}

impl Heap {
    /// Puts `[at, at + size)` on the free list, merging with its neighbours.
    unsafe fn insert(&mut self, at: usize, size: usize) {
        unsafe {
            let mut previous: *mut Free = core::ptr::null_mut();
            let mut next = self.head;
            while !next.is_null() && (next as usize) < at {
                previous = next;
                next = (*next).next;
            }
            let block = at as *mut Free;
            (*block).size = size;
            (*block).next = next;
            if !next.is_null() && at + size == next as usize {
                (*block).size += (*next).size;
                (*block).next = (*next).next;
            }
            if previous.is_null() {
                self.head = block;
            } else if previous as usize + (*previous).size == at {
                (*previous).size += (*block).size;
                (*previous).next = (*block).next;
            } else {
                (*previous).next = block;
            }
        }
    }

    /// First fit; `None` if nothing is big enough.
    unsafe fn take(&mut self, size: usize, align: usize) -> Option<usize> {
        unsafe {
            let mut previous: *mut Free = core::ptr::null_mut();
            let mut block = self.head;
            while !block.is_null() {
                let (start, length) = (block as usize, (*block).size);
                let mut at = align_up(start, align);
                // A leading remainder too small to be a block cannot be left behind: move on to the next aligned spot.
                if at != start && at - start < MIN_BLOCK {
                    at = align_up(start + MIN_BLOCK, align);
                }
                if at + size <= start + length {
                    let tail = start + length - (at + size);
                    // Likewise a trailing remainder: hand it to the caller (the block is freed with the same rounding).
                    let (size, tail) = if tail != 0 && tail < MIN_BLOCK { (size + tail, 0) } else { (size, tail) };
                    let next = (*block).next;
                    let mut after = next;
                    if tail != 0 {
                        let rest = (at + size) as *mut Free;
                        (*rest).size = tail;
                        (*rest).next = next;
                        after = rest;
                    }
                    let kept = at - start;
                    if kept != 0 {
                        (*block).size = kept;
                        (*block).next = after;
                    } else if previous.is_null() {
                        self.head = after;
                    } else {
                        (*previous).next = after;
                    }
                    self.free -= size;
                    return Some(at);
                }
                previous = block;
                block = (*block).next;
            }
            None
        }
    }

    unsafe fn grow(&mut self, needed: usize) -> bool {
        let bytes = align_up(needed, pmm::FRAME as usize).max(CHUNK as usize);
        let Some(physical) = pmm::allocate_uncleared(bytes as u64 / pmm::FRAME, None) else { return false };
        unsafe { self.insert(vm::direct(physical) as usize, bytes) };
        self.total += bytes;
        self.free += bytes;
        true
    }
}

/// Rounds a request the way both `alloc` and `dealloc` see it, so a block is freed with the size it was given.
fn rounded(layout: &Layout) -> usize {
    align_up(layout.size().max(1), MIN_BLOCK)
}

pub struct Allocator;

unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let (size, align) = (rounded(&layout), layout.align().max(MIN_BLOCK));
        let heap = heap();
        unsafe {
            if let Some(at) = heap.take(size, align) {
                return at as *mut u8;
            }
            if !heap.grow(size + align) {
                return core::ptr::null_mut();
            }
            heap.take(size, align).map_or(core::ptr::null_mut(), |at| at as *mut u8)
        }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        let heap = heap();
        let size = rounded(&layout);
        heap.free += size;
        unsafe { heap.insert(pointer as usize, size) };
    }
}

#[global_allocator]
static ALLOCATOR: Allocator = Allocator;

/// Gives the heap its first chunk.
pub fn init() -> bool {
    unsafe { heap().grow(CHUNK as usize) }
}

pub struct Stats {
    pub total: usize,
    pub free: usize,
}

pub fn stats() -> Stats {
    let h = heap();
    Stats { total: h.total, free: h.free }
}
