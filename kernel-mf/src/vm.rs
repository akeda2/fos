//! The server kernel's address-space interface. The page-table hardware (entry formats, TLB, the direct map, user-space roots)
//! is `arch::mmu`; this file is the policy that sits on top and is the same on every CPU: where kernel thread stacks go (a guard
//! page under each), and the check a system call makes of a user pointer.

pub use crate::arch::mmu::*;
use crate::sync::SpinLock;

static STACK_LOCK: SpinLock<()> = SpinLock::new(());

fn slot_base(slot: u64, pages: u64) -> u64 {
    STACK_REGION + slot * (pages + 1) * PAGE + PAGE
}

/// Maps `pages` frames starting at physical `frames` as the stack of slot `slot` (a guard page sits below it, left unmapped
/// so an overflow faults at once). Returns the stack's top (the initial stack pointer).
pub fn map_stack(slot: u64, pages: u64, frames: u64) -> u64 {
    let _guard = STACK_LOCK.lock();
    let base = slot_base(slot, pages);
    for page in 0..pages {
        map_kernel_page(base + page * PAGE, frames + page * PAGE);
    }
    base + pages * PAGE
}

/// The lowest stack page of `slot`.
pub fn stack_base(slot: u64, pages: u64) -> u64 {
    slot_base(slot, pages)
}

/// Unmaps what `map_stack` mapped (the caller frees the frames). Other CPUs may keep stale TLB entries: the caller must run
/// `tlb::shootdown` over the range before the frames or the slot are reused.
pub fn unmap_stack(slot: u64, pages: u64) {
    let _guard = STACK_LOCK.lock();
    let base = slot_base(slot, pages);
    for page in 0..pages {
        unmap_kernel_page(base + page * PAGE);
    }
}

/// The physical address behind `address` in the user space of `root`, if a user page is mapped there.
pub fn user_physical(root: u64, address: u64) -> Option<u64> {
    let page = user_leaf(root, address)?;
    page.user.then(|| page.physical + (address & (PAGE - 1)))
}

/// Can the kernel read (or, with `write`, write) `[address, address + length)' of the process with root `root` through its
/// user mappings? A syscall checks every pointer it is given with this before touching it.
pub fn user_range_ok(root: u64, address: u64, length: u64, write: bool) -> bool {
    let Some(end) = address.checked_add(length) else { return false };
    if length == 0 {
        return true;
    }
    let mut page = address & !(PAGE - 1);
    while page < end {
        match user_leaf(root, page) {
            Some(leaf) if leaf.user && (!write || leaf.writable) => {}
            _ => return false,
        }
        page += PAGE;
    }
    true
}
