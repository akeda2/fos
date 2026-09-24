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
//! access. A PCI memory BAR retains its physical address as its
//! volatile-pointer address only after its driver explicitly registers the
//! needed range.
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
const PAGE_SIZE: u32 = 4096;
#[cfg(not(feature = "legacy-unsafe-memory"))]
const PAGE_DIRECTORY_ENTRIES: usize = 1024;
/// One dynamic table per distinct 4 MiB-aligned high region a `map_mmio`
/// caller has ever registered. Kept at 3 rather than bumped for MSI
/// support (`lapic.rs`), even though the local APIC's own fixed MMIO
/// window (`0xFEE00000`) is one more `map_mmio` consumer alongside the
/// existing e1000/i350/HDA BAR mappings: each additional slot is a whole
/// extra static 4 KiB page table, and the reserved low-memory kernel
/// region (`linker.ld`'s `__kernel_end < 0x90000` assertion) has no spare
/// room for one right now -- confirmed by trying it, not assumed. If this
/// pool is ever exhausted (all 3 slots already claimed by 3 *other*
/// simultaneously-mapped high regions when the LAPIC tries to map its
/// own), `map_mmio` simply returns `Err`, `lapic::initialize()` reports
/// no local APIC available, and every MSI-capable driver transparently
/// falls back to its legacy PIC IRQ path -- the same "cleanly
/// unavailable, not a fault" convention `disk.rs` already uses for a zero
/// `bios_disk_io` pointer. Growing this pool (and the reserved region
/// alongside it, in the same synchronized way `AGENTS.md`'s low-memory
/// growth history describes) is the right fix if a real scenario ever
/// needs 4 simultaneous high MMIO mappings at once; nothing here needs it
/// pre-emptively today.
#[cfg(not(feature = "legacy-unsafe-memory"))]
const MAX_DYNAMIC_PAGE_TABLES: usize = 3;

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
struct PageTable([u32; PAGE_DIRECTORY_ENTRIES]);

static mut PAGE_DIRECTORY: PageDirectory = PageDirectory([0; 1024]);
#[cfg(not(feature = "legacy-unsafe-memory"))]
static mut LOW_PAGE_TABLE: PageTable = PageTable([0; PAGE_DIRECTORY_ENTRIES]);
#[cfg(not(feature = "legacy-unsafe-memory"))]
static mut DYNAMIC_PAGE_TABLES: [PageTable; MAX_DYNAMIC_PAGE_TABLES] =
    [const { PageTable([0; PAGE_DIRECTORY_ENTRIES]) }; MAX_DYNAMIC_PAGE_TABLES];
#[cfg(not(feature = "legacy-unsafe-memory"))]
static mut DYNAMIC_TABLE_PDES: [u16; MAX_DYNAMIC_PAGE_TABLES] =
    [u16::MAX; MAX_DYNAMIC_PAGE_TABLES];

#[cfg(not(feature = "legacy-unsafe-memory"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    MissingPageSizeExtensions,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "legacy-unsafe-memory", allow(dead_code))]
pub enum MapError {
    EmptyRange,
    AddressOverflow,
    PageTableCapacity,
}

#[cfg(not(feature = "legacy-unsafe-memory"))]
unsafe extern "C" {
    static __text_start: u8;
    static __text_end: u8;
    static __rodata_start: u8;
    static __rodata_end: u8;
}

/// Builds and enables the one global address space.
///
/// Four-megabyte pages keep this bootstrap table to one 4 KiB directory. The
/// low 2 GiB range covers all physical memory the current allocator can give
/// to the kernel or a tape, plus any firmware holes within that range. The
/// device MMIO is initially absent and is registered only by the driver that
/// owns its BAR. Leaving every other high range absent gives the page-fault
/// handler a useful signal without breaking established address ranges.
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
    for index in 0..PAGE_DIRECTORY_ENTRIES {
        let address = (index as u32) * PAGE_SIZE;
        unsafe {
            low_page_table.add(index).write(if address == stack_guard {
                0
            } else {
                address | PAGE_PRESENT | PAGE_WRITABLE
            });
        }
    }

    for index in 0..PAGE_DIRECTORY_ENTRIES {
        let address = (index as u32) * LARGE_PAGE_SIZE;
        unsafe {
            let entry = if index == 0 {
                low_page_table as u32 | PAGE_PRESENT | PAGE_WRITABLE
            } else if address < IDENTITY_LOW_END {
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
    mark_kernel_range_read_only(
        linker_address(core::ptr::addr_of!(__text_start)),
        linker_address(core::ptr::addr_of!(__text_end)),
    );
    mark_kernel_range_read_only(
        linker_address(core::ptr::addr_of!(__rodata_start)),
        linker_address(core::ptr::addr_of!(__rodata_end)),
    );
    flush_tlb(directory_address);
    Ok(())
}

/// Maps one PCI memory-BAR range at its physical address.
///
/// Low physical addresses are already identity mapped for the current frame
/// and tape allocators. Addresses above that compatibility range consume one
/// of the small, on-demand page tables instead of making an entire PCI window
/// reachable by accident.
#[cfg(not(feature = "legacy-unsafe-memory"))]
pub fn map_mmio(base: u32, length: u32) -> Result<(), MapError> {
    if length == 0 {
        return Err(MapError::EmptyRange);
    }
    let last = base
        .checked_add(length - 1)
        .ok_or(MapError::AddressOverflow)?;
    let first_page = base & !(PAGE_SIZE - 1);
    let last_page = last & !(PAGE_SIZE - 1);
    let directory = unsafe { core::ptr::addr_of_mut!(PAGE_DIRECTORY.0).cast::<u32>() };

    let mut address = first_page;
    loop {
        if address >= IDENTITY_LOW_END {
            let pde_index = (address / LARGE_PAGE_SIZE) as usize;
            let pte_index = ((address % LARGE_PAGE_SIZE) / PAGE_SIZE) as usize;
            let table = dynamic_page_table(directory, pde_index)?;
            unsafe {
                table
                    .add(pte_index)
                    .write(address | PAGE_PRESENT | PAGE_WRITABLE);
            }
        }
        if address == last_page {
            break;
        }
        address = address
            .checked_add(PAGE_SIZE)
            .ok_or(MapError::AddressOverflow)?;
    }
    flush_tlb(directory as u32);
    Ok(())
}

/// The unpaged teaching image retains physical pointers directly, so no
/// translation work is necessary for a driver's BAR.
#[cfg(feature = "legacy-unsafe-memory")]
pub fn map_mmio(_base: u32, length: u32) -> Result<(), MapError> {
    if length == 0 {
        Err(MapError::EmptyRange)
    } else {
        Ok(())
    }
}

/// Returns the physical address in CR3 for task-state restoration.
pub fn directory_address() -> u32 {
    core::ptr::addr_of!(PAGE_DIRECTORY) as u32
}

/// Labels the most useful paging region for a fatal page-fault diagnostic.
pub fn describe_address(address: u32) -> &'static str {
    #[cfg(feature = "legacy-unsafe-memory")]
    {
        let _ = address;
        return "legacy unpaged physical-address mode";
    }

    #[cfg(not(feature = "legacy-unsafe-memory"))]
    {
        let text_start = linker_address(core::ptr::addr_of!(__text_start));
        let text_end = linker_address(core::ptr::addr_of!(__text_end));
        if (text_start..text_end).contains(&address) {
            return "kernel text (read-only)";
        }
        let rodata_start = linker_address(core::ptr::addr_of!(__rodata_start));
        let rodata_end = linker_address(core::ptr::addr_of!(__rodata_end));
        if (rodata_start..rodata_end).contains(&address) {
            return "kernel read-only data";
        }
        let guard = crate::stack::guard_page_address();
        if (guard..guard + PAGE_SIZE).contains(&address) {
            return "kernel stack guard page";
        }
        if address < IDENTITY_LOW_END {
            return "low physical identity map";
        }
        if is_dynamic_mapping_present(address) {
            return "registered PCI MMIO";
        }
        "unmapped high physical or PCI MMIO range"
    }
}

/// Deliberately writes to kernel text for `make test-page-protection`.
///
/// The test runs only after the IDT is live, so its expected supervisor-write
/// fault exercises both CR0.WP and the text PTEs without risking a fault before
/// diagnostics are installed.
#[cfg(feature = "page-protection-test")]
pub fn trigger_text_write_for_test() {
    let address = linker_address(core::ptr::addr_of!(__text_start));
    unsafe {
        core::ptr::write_volatile(address as *mut u8, 0);
    }
}

/// Returns CR2, the linear address that caused the most recent page fault.
pub fn fault_address() -> u32 {
    let address: u32;
    unsafe {
        asm!("mov {address:e}, cr2", address = out(reg) address, options(nostack, preserves_flags));
    }
    address
}

#[cfg(not(feature = "legacy-unsafe-memory"))]
fn linker_address(symbol: *const u8) -> u32 {
    symbol as u32
}

#[cfg(not(feature = "legacy-unsafe-memory"))]
fn mark_kernel_range_read_only(start: u32, end: u32) {
    let first = start & !(PAGE_SIZE - 1);
    let last = (end - 1) & !(PAGE_SIZE - 1);
    let table = unsafe { core::ptr::addr_of_mut!(LOW_PAGE_TABLE.0).cast::<u32>() };
    let mut address = first;
    loop {
        let index = (address / PAGE_SIZE) as usize;
        unsafe {
            let entry = table.add(index);
            entry.write(entry.read() & !PAGE_WRITABLE);
        }
        if address == last {
            break;
        }
        address += PAGE_SIZE;
    }
}

#[cfg(not(feature = "legacy-unsafe-memory"))]
fn dynamic_page_table(directory: *mut u32, pde_index: usize) -> Result<*mut u32, MapError> {
    unsafe {
        let directory_entry = directory.add(pde_index);
        let existing = directory_entry.read();
        if existing & PAGE_PRESENT != 0 {
            return Ok((existing & 0xffff_f000) as *mut u32);
        }

        for index in 0..MAX_DYNAMIC_PAGE_TABLES {
            if DYNAMIC_TABLE_PDES[index] == u16::MAX {
                let table = core::ptr::addr_of_mut!(DYNAMIC_PAGE_TABLES[index].0).cast::<u32>();
                for entry in 0..PAGE_DIRECTORY_ENTRIES {
                    table.add(entry).write(0);
                }
                DYNAMIC_TABLE_PDES[index] = pde_index as u16;
                directory_entry.write(table as u32 | PAGE_PRESENT | PAGE_WRITABLE);
                return Ok(table);
            }
        }
    }
    Err(MapError::PageTableCapacity)
}

#[cfg(not(feature = "legacy-unsafe-memory"))]
fn is_dynamic_mapping_present(address: u32) -> bool {
    let pde_index = (address / LARGE_PAGE_SIZE) as usize;
    let pte_index = ((address % LARGE_PAGE_SIZE) / PAGE_SIZE) as usize;
    unsafe {
        let directory = core::ptr::addr_of!(PAGE_DIRECTORY.0).cast::<u32>();
        let pde = directory.add(pde_index).read();
        if pde & PAGE_PRESENT == 0 || pde & PAGE_LARGE != 0 {
            return false;
        }
        let table = (pde & 0xffff_f000) as *const u32;
        table.add(pte_index).read() & PAGE_PRESENT != 0
    }
}

#[cfg(not(feature = "legacy-unsafe-memory"))]
fn flush_tlb(directory_address: u32) {
    unsafe {
        asm!("mov cr3, {directory_address:e}", directory_address = in(reg) directory_address, options(nostack, preserves_flags));
    }
}
