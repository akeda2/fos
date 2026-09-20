//! The first paging step: an identity-mapped, supervisor-only address space.
//!
//! The kernel and its drivers still use physical addresses directly, as do
//! DMA descriptors and the BIOS disk trampoline. Mapping a virtual address to
//! the same physical address therefore lets the existing educational code
//! keep working while making page translation, CR3 ownership, and page-fault
//! diagnostics explicit foundations for later isolation work.
//!
//! This is deliberately not userspace protection yet. All mapped pages are
//! supervisor-only and writable, so ring-0 kernel code and trusted tapes keep
//! their current access. The absent 2-3.5 GiB range does, however, turn many
//! wild high pointers into a legible page fault instead of an arbitrary bus
//! access. The usual PCI MMIO window above 3.5 GiB remains mapped so an e1000
//! BAR can retain its physical address as its volatile-pointer address.
//! The low 4 MiB is split into ordinary 4 KiB pages solely so the page
//! immediately below the kernel stack can remain absent as an overflow guard.

use core::arch::asm;

#[cfg(not(feature = "legacy-unsafe-memory"))]
use crate::cpu;

#[cfg(not(feature = "legacy-unsafe-memory"))]
const LARGE_PAGE_SIZE: u32 = 4 * 1024 * 1024;
#[cfg(not(feature = "legacy-unsafe-memory"))]
const IDENTITY_LOW_END: u32 = 2 * 1024 * 1024 * 1024;
#[cfg(not(feature = "legacy-unsafe-memory"))]
const PCI_MMIO_BASE: u32 = 0xe000_0000;

#[cfg(not(feature = "legacy-unsafe-memory"))]
const PAGE_PRESENT: u32 = 1 << 0;
#[cfg(not(feature = "legacy-unsafe-memory"))]
const PAGE_WRITABLE: u32 = 1 << 1;
#[cfg(not(feature = "legacy-unsafe-memory"))]
const PAGE_LARGE: u32 = 1 << 7;
#[cfg(not(feature = "legacy-unsafe-memory"))]
const CR0_WRITE_PROTECT: u32 = 1 << 16;
#[cfg(not(feature = "legacy-unsafe-memory"))]
const CR0_PAGING: u32 = 1 << 31;
#[cfg(not(feature = "legacy-unsafe-memory"))]
const CR4_PAGE_SIZE_EXTENSIONS: u32 = 1 << 4;
#[cfg(not(feature = "legacy-unsafe-memory"))]
const CPUID_PAGE_SIZE_EXTENSIONS: u32 = 1 << 3;

/// A page directory must begin on a 4 KiB boundary because CR3 stores its
/// physical address with the low 12 bits reserved for flags.
#[repr(C, align(4096))]
struct PageDirectory([u32; 1024]);

#[cfg(not(feature = "legacy-unsafe-memory"))]
#[repr(C, align(4096))]
struct PageTable([u32; 1024]);

static mut PAGE_DIRECTORY: PageDirectory = PageDirectory([0; 1024]);
#[cfg(not(feature = "legacy-unsafe-memory"))]
static mut LOW_PAGE_TABLE: PageTable = PageTable([0; 1024]);

#[cfg(not(feature = "legacy-unsafe-memory"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    MissingPageSizeExtensions,
}

/// Builds and enables the one global address space.
///
/// Four-megabyte pages keep this bootstrap table to one 4 KiB directory. The
/// low 2 GiB range covers all physical memory the current allocator can give
/// to the kernel or a tape, plus any firmware holes within that range. The
/// high PCI MMIO window covers memory BARs such as QEMU's e1000. Leaving the
/// intervening region absent gives the page-fault handler a useful signal
/// without breaking those established address ranges.
#[cfg(not(feature = "legacy-unsafe-memory"))]
pub fn initialize() -> Result<(), Error> {
    let (feature_edx, _) = cpu::features();
    if feature_edx & CPUID_PAGE_SIZE_EXTENSIONS == 0 {
        return Err(Error::MissingPageSizeExtensions);
    }

    // Paging is still disabled and interrupts are not initialized, so this
    // one early-boot writer has exclusive access to the static directory.
    let directory = unsafe { core::ptr::addr_of_mut!(PAGE_DIRECTORY.0).cast::<u32>() };
    let low_page_table = unsafe { core::ptr::addr_of_mut!(LOW_PAGE_TABLE.0).cast::<u32>() };
    let stack_guard = crate::stack::guard_page_address();
    for index in 0..1024 {
        let address = (index as u32) * 4096;
        unsafe {
            low_page_table.add(index).write(if address == stack_guard {
                0
            } else {
                address | PAGE_PRESENT | PAGE_WRITABLE
            });
        }
    }

    for index in 0..1024 {
        let address = (index as u32) * LARGE_PAGE_SIZE;
        let mapped = address < IDENTITY_LOW_END || address >= PCI_MMIO_BASE;
        unsafe {
            let entry = if index == 0 {
                low_page_table as u32 | PAGE_PRESENT | PAGE_WRITABLE
            } else if mapped {
                address | PAGE_PRESENT | PAGE_WRITABLE | PAGE_LARGE
            } else {
                0
            };
            directory.add(index).write(entry);
        }
    }

    let directory_address = directory as u32;
    unsafe {
        // Load CR3 before CR0.PG. Until the final write below, these pointers
        // are ordinary physical addresses because paging is still off.
        asm!("mov cr3, {directory_address:e}", directory_address = in(reg) directory_address, options(nostack, preserves_flags));

        let mut cr4: u32;
        asm!("mov {cr4:e}, cr4", cr4 = out(reg) cr4, options(nostack, preserves_flags));
        cr4 |= CR4_PAGE_SIZE_EXTENSIONS;
        asm!("mov cr4, {cr4:e}", cr4 = in(reg) cr4, options(nostack, preserves_flags));

        let mut cr0: u32;
        asm!("mov {cr0:e}, cr0", cr0 = out(reg) cr0, options(nostack, preserves_flags));
        // WP matters only after future mappings become read-only, but setting
        // it now prevents ring 0 from silently bypassing that protection.
        cr0 |= CR0_WRITE_PROTECT | CR0_PAGING;
        asm!("mov cr0, {cr0:e}", cr0 = in(reg) cr0, options(nostack, preserves_flags));
    }
    Ok(())
}

/// Returns the physical address in CR3 for task-state restoration.
pub fn directory_address() -> u32 {
    core::ptr::addr_of!(PAGE_DIRECTORY) as u32
}

/// Returns CR2, the linear address that caused the most recent page fault.
pub fn fault_address() -> u32 {
    let address: u32;
    unsafe {
        asm!("mov {address:e}, cr2", address = out(reg) address, options(nostack, preserves_flags));
    }
    address
}
