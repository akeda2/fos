//! The aarch64 page-table hardware as the shared `vm` sees it. Memory is identity-mapped in TTBR0 by the embedded kernel's
//! `paging.rs`; kernel thread stacks live in their own region of the same tables, one guard page under each. Per-process user
//! tables (EL0) are not here yet: `user_leaf` finds nothing.

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

/// The kernel runs on one TTBR0 root for now; per-process roots come with EL0 tapes.
pub fn load_root(_root: u64) {}

pub fn current_root() -> u64 {
    root()
}

pub fn user_leaf(_root: u64, _address: u64) -> Option<UserPage> {
    None
}
