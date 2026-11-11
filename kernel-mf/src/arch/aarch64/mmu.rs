//! The aarch64 page-table hardware as the shared `vm` sees it. Memory is identity-mapped in TTBR0 by the embedded kernel's
//! `paging.rs`; kernel thread stacks live in their own region of the same tables, one guard page under each. Per-process user
//! tables (EL0) are one TTBR0 root per process: the kernel's low L0 slots (identity map, device windows, thread stacks) is shared,
//! the process owns the slots from KERNEL_SLOTS up. User pages are non-global (nG), `AP[1]` = EL0 may touch them,
//! never executable at EL1 (PXN); TTBR0 is reloaded and the TLB flushed at every switch of address space (no ASIDs yet).

pub const PAGE: u64 = 4096;
pub const DIRECT_BASE: u64 = 0;

/// Where kernel thread stacks live: 32 GiB, above the RAM and the Pi 4's peripheral and PCIe windows (below 24 GiB + 1 GiB of
/// PCIe), so it needs 36 address bits. QEMU's 48-bit setup has them; the firmware's geometry on a Pi 4 is what hardware shows.
pub const STACK_REGION: u64 = 1 << 35;

/// A mapped user page: its frame and whether user mode may use it, and write it.
#[derive(Clone, Copy)]
pub struct UserPage {
    pub physical: u64,
    pub user: bool,
    pub writable: bool,
}

/// Identity map: a physical address is the pointer to it.
pub fn direct(physical: u64) -> u64 {
    physical
}

pub fn root() -> u64 {
    crate::paging::root()
}

pub fn map_kernel_page(address: u64, physical: u64) {
    crate::paging::map_kernel_page(address, physical);
}

pub fn unmap_kernel_page(address: u64) {
    crate::paging::unmap_kernel_page(address);
}

/// Maps device registers (Device-nGnRE, no-execute) at their physical address.
pub fn map_device(physical: u64, length: u64) {
    crate::paging::map_device(physical, length);
}

const VALID: u64 = 1;
const TABLE: u64 = 3;
const PAGE_DESC: u64 = 3;
const INNER_SHAREABLE: u64 = 3 << 8;
const ACCESS: u64 = 1 << 10;
const NOT_GLOBAL: u64 = 1 << 11;
const EL0_ACCESS: u64 = 1 << 6; // AP[1]
const READ_ONLY: u64 = 1 << 7; // AP[2]
const PXN: u64 = 1 << 53;
const UXN: u64 = 1 << 54;
const ADDRESS_MASK: u64 = 0x0000_ffff_ffff_f000;

/// L0 slots 0..KERNEL_SLOTS belong to the kernel and are shared by every root: the identity map, the device windows (QEMU's
/// PCIe high window sits at 512 GiB, in slot 1) and the thread stacks. User space starts above them.
const KERNEL_SLOTS: usize = 64;
pub const USER_BASE: u64 = (KERNEL_SLOTS as u64) << 39;
pub const USER_END: u64 = 1 << 47;

/// Makes the process's tables visible to the walker (a stable inner-shareable write-back walk, but the stores must be ordered).
fn publish() {
    unsafe { core::arch::asm!("dsb ishst", options(nostack)) };
}

/// A new root for a process: the kernel's slot 0, an empty user half. `None` when out of frames.
pub fn create_user_space() -> Option<u64> {
    let root = crate::pmm::allocate(1, None)?;
    unsafe { core::ptr::copy_nonoverlapping(root_kernel() as *const u64, root as *mut u64, KERNEL_SLOTS) };
    publish();
    Some(root)
}

/// Gives the first kernel L0 slots their L1 tables now: a user root copies the slots, so a device window the kernel maps later
/// has to land in a table every root already shares. Call once after the kernel's tables are loaded.
pub fn share_kernel_slots() {
    let root = root_kernel() as *mut u64;
    for slot in 0..8 {
        unsafe { next_level(root.add(slot)) };
    }
    publish();
}

fn root_kernel() -> u64 {
    crate::paging::root()
}

/// The next-level table under `*slot`, allocated (zeroed) when absent.
unsafe fn next_level(slot: *mut u64) -> Option<*mut u64> {
    unsafe {
        if *slot & VALID == 0 {
            let table = crate::pmm::allocate(1, None)?;
            *slot = table | TABLE;
        }
        Some((*slot & ADDRESS_MASK) as *mut u64)
    }
}

/// Maps the 4 KiB frame `physical` at user address `address` in `root`. The user half only; false outside it or when out of
/// frames. Code pages are cleaned by the caller's `usermode::sync_code`.
pub fn map_user_page(root: u64, address: u64, physical: u64, writable: bool, executable: bool) -> bool {
    if address < USER_BASE || address >= USER_END {
        return false;
    }
    let flags = physical
        | PAGE_DESC
        | INNER_SHAREABLE
        | ACCESS
        | NOT_GLOBAL
        | EL0_ACCESS
        | PXN
        | if writable { 0 } else { READ_ONLY }
        | if executable { 0 } else { UXN };
    unsafe {
        let Some(l1) = next_level((root as *mut u64).add(((address >> 39) & 511) as usize)) else { return false };
        let Some(l2) = next_level(l1.add(((address >> 30) & 511) as usize)) else { return false };
        let Some(l3) = next_level(l2.add(((address >> 21) & 511) as usize)) else { return false };
        l3.add(((address >> 12) & 511) as usize).write(flags);
    }
    publish();
    true
}

/// The leaf descriptor slot for user `address` in `root`, if the whole walk is present.
fn user_slot(root: u64, address: u64) -> Option<*mut u64> {
    if address < USER_BASE || address >= USER_END {
        return None;
    }
    unsafe {
        let mut table = root as *mut u64;
        for shift in [39, 30, 21] {
            let entry = table.add(((address >> shift) & 511) as usize).read();
            if entry & VALID == 0 {
                return None;
            }
            table = (entry & ADDRESS_MASK) as *mut u64;
        }
        let slot = table.add(((address >> 12) & 511) as usize);
        (slot.read() & VALID != 0).then_some(slot)
    }
}

/// Removes the user page at `address` and returns its frame (the caller frees it); None if nothing was mapped. The entry is
/// dropped from every CPU's TLB (any ASID: processes share ASID 0 and are told apart by the flush at each switch).
pub fn unmap_user_page(root: u64, address: u64) -> Option<u64> {
    let slot = user_slot(root, address)?;
    unsafe {
        let frame = slot.read() & ADDRESS_MASK;
        slot.write(0);
        core::arch::asm!("dsb ishst", "tlbi vaae1is, {}", "dsb ish", "isb", in(reg) address >> 12, options(nostack));
        Some(frame)
    }
}

/// What the user page containing `address` in `root` is, if one is mapped.
pub fn user_leaf(root: u64, address: u64) -> Option<UserPage> {
    let leaf = unsafe { user_slot(root, address & !(PAGE - 1))?.read() };
    Some(UserPage { physical: leaf & ADDRESS_MASK, user: leaf & EL0_ACCESS != 0, writable: leaf & READ_ONLY == 0 })
}

/// Frees a process's user half: every mapped frame and every table below slots KERNEL_SLOTS..255, then the root. Returns the frames freed.
/// The root must not be loaded on any CPU.
pub fn destroy_user_space(root: u64) -> u64 {
    let mut freed = 0;
    unsafe {
        let top = root as *mut u64;
        for slot in KERNEL_SLOTS..256 {
            let l0 = *top.add(slot);
            if l0 & VALID == 0 {
                continue;
            }
            let l1 = (l0 & ADDRESS_MASK) as *mut u64;
            for i in 0..512 {
                let entry = *l1.add(i);
                if entry & VALID == 0 {
                    continue;
                }
                let l2 = (entry & ADDRESS_MASK) as *mut u64;
                for j in 0..512 {
                    let entry = *l2.add(j);
                    if entry & VALID == 0 {
                        continue;
                    }
                    let l3 = (entry & ADDRESS_MASK) as *mut u64;
                    for k in 0..512 {
                        let leaf = *l3.add(k);
                        if leaf & VALID != 0 {
                            crate::pmm::free(leaf & ADDRESS_MASK, 1);
                            freed += 1;
                        }
                    }
                    crate::pmm::free(entry & ADDRESS_MASK, 1);
                    freed += 1;
                }
                crate::pmm::free(entry & ADDRESS_MASK, 1);
                freed += 1;
            }
            crate::pmm::free(l0 & ADDRESS_MASK, 1);
            freed += 1;
        }
    }
    crate::pmm::free(root, 1);
    freed + 1
}

/// Switches this CPU to `root`'s user half (TTBR0_EL1) and flushes the non-global entries of the old one.
pub fn load_root(root: u64) {
    unsafe {
        core::arch::asm!("msr ttbr0_el1, {}", "isb", "tlbi vmalle1", "dsb nsh", "isb", in(reg) root, options(nostack));
    }
}

pub fn current_root() -> u64 {
    let root: u64;
    unsafe { core::arch::asm!("mrs {}, ttbr0_el1", out(reg) root, options(nomem, nostack, preserves_flags)) };
    root & ADDRESS_MASK
}
