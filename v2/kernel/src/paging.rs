//! The kernel's page tables, built from the physical allocator and the memory map.
//!
//! The firmware leaves the CPU in long mode with an identity map of its own. The
//! kernel replaces it with one derived from what the machine actually has:
//!
//! * RAM the kernel may use is mapped read/write and **no-execute**, in 2 MiB pages;
//! * the kernel image is mapped in 4 KiB pages with per-section permissions: text
//!   read + execute only, read-only data read-only, data and bss read/write
//!   no-execute (W^X: nothing is both writable and executable);
//! * page 0 and the guard pages below the kernel's stacks are *not mapped*, so a
//!   null pointer or a stack overflow faults at once with a legible report;
//! * UEFI runtime-services regions keep the permissions the firmware expects
//!   (executable), so `ResetSystem` and friends still work;
//! * the framebuffer is mapped write-combining (via the PAT) when the CPU has one,
//!   uncached otherwise; device registers are mapped on demand by `map_device`.
//!
//! Still a single flat address space, still identity-mapped (virtual = physical).

use crate::{cpu, memory};
use fos_v2_abi::{BootInfo, Framebuffer};

const PAGE: u64 = 4096;
const HUGE: u64 = 2 * 1024 * 1024;

const PRESENT: u64 = 1 << 0;
const WRITABLE: u64 = 1 << 1;
const WRITE_THROUGH: u64 = 1 << 3;
const CACHE_DISABLE: u64 = 1 << 4;
const LARGE: u64 = 1 << 7;
const NO_EXECUTE: u64 = 1 << 63;
const ADDRESS_MASK: u64 = 0x000f_ffff_ffff_f000;

/// UEFI sets this bit in a descriptor's attributes for memory the firmware needs at
/// run time (runtime-services code and data, and some device regions).
const ATTRIBUTE_RUNTIME: u64 = 1 << 63;

unsafe extern "C" {
    static __kernel_start: u8;
    static __kernel_end: u8;
    static __text_start: u8;
    static __text_end: u8;
    static __rodata_start: u8;
    static __rodata_end: u8;
    static __stack_guard: u8;
    static __stack_bottom: u8;
    static __ist_guard: u8;
}

fn address_of(symbol: *const u8) -> u64 {
    symbol as u64
}

/// The kernel's own layout, as runtime addresses.
pub struct Layout {
    pub image: (u64, u64),
    pub text: (u64, u64),
    pub rodata: (u64, u64),
    pub stack_guard: u64,
    pub stack_bottom: u64,
    pub ist_guard: u64,
}

pub fn layout() -> Layout {
    unsafe {
        Layout {
            image: (address_of(&__kernel_start), address_of(&__kernel_end)),
            text: (address_of(&__text_start), address_of(&__text_end)),
            rodata: (address_of(&__rodata_start), address_of(&__rodata_end)),
            stack_guard: address_of(&__stack_guard),
            stack_bottom: address_of(&__stack_bottom),
            ist_guard: address_of(&__ist_guard),
        }
    }
}

static mut ROOT: u64 = 0;
static mut WRITE_COMBINING: bool = false;

/// What a faulting address is, for the exception report.
pub fn describe(address: u64) -> &'static str {
    let layout = layout();
    if address < PAGE {
        "the null page (a null pointer was used)"
    } else if (layout.stack_guard..layout.stack_bottom).contains(&address) {
        "the kernel stack guard page (stack overflow)"
    } else if (layout.ist_guard..layout.ist_guard + PAGE).contains(&address) {
        "the double-fault stack guard page"
    } else if (layout.text.0..layout.text.1).contains(&address) {
        "kernel code (read-only)"
    } else if (layout.rodata.0..layout.rodata.1).contains(&address) {
        "kernel read-only data"
    } else if (layout.image.0..layout.image.1).contains(&address) {
        "kernel data"
    } else {
        "unmapped memory"
    }
}

/// Allocates one zeroed page-table page (the physical allocator hands out zeroed frames).
fn new_table() -> *mut u64 {
    match memory::allocate() {
        Some(address) => address as *mut u64,
        None => cpu::halt(),
    }
}

/// The table an entry points to, creating it if the entry is empty.
unsafe fn next_level(entry: *mut u64) -> *mut u64 {
    unsafe {
        if *entry & PRESENT == 0 {
            *entry = new_table() as u64 | PRESENT | WRITABLE;
        }
        (*entry & ADDRESS_MASK) as *mut u64
    }
}

/// Walks to the page-directory entry for `address` (levels 4 -> 3 -> 2).
unsafe fn directory_entry(root: u64, address: u64) -> *mut u64 {
    unsafe {
        let pml4 = root as *mut u64;
        let pdpt = next_level(pml4.add(((address >> 39) & 511) as usize));
        let directory = next_level(pdpt.add(((address >> 30) & 511) as usize));
        directory.add(((address >> 21) & 511) as usize)
    }
}

/// Maps one 2 MiB page.
fn map_huge(root: u64, address: u64, flags: u64) {
    unsafe { *directory_entry(root, address) = address | flags | PRESENT | LARGE };
}

/// Maps one 4 KiB page (`virtual_address` -> `physical`).
fn map_page(root: u64, virtual_address: u64, physical: u64, flags: u64) {
    unsafe {
        let entry = directory_entry(root, virtual_address);
        assert!(*entry & LARGE == 0, "a 4 KiB mapping inside a 2 MiB page");
        let table = next_level(entry);
        *table.add(((virtual_address >> 12) & 511) as usize) = physical | flags | PRESENT;
    }
}

/// Permissions for one 4 KiB page, or `None` if it must stay unmapped. `default` is
/// what the page's memory region would get as a bulk mapping.
fn page_flags(address: u64, default: u64, layout: &Layout) -> Option<u64> {
    if address < PAGE {
        return None; // the null page
    }
    let in_image = (layout.image.0..layout.image.1).contains(&address);
    if !in_image {
        return Some(default);
    }
    if address >= layout.stack_guard && address < layout.stack_bottom || (layout.ist_guard..layout.ist_guard + PAGE).contains(&address) {
        return None; // guard pages
    }
    Some(if (layout.text.0..layout.text.1).contains(&address) {
        0 // read + execute
    } else if (layout.rodata.0..layout.rodata.1).contains(&address) {
        NO_EXECUTE // read only
    } else {
        WRITABLE | NO_EXECUTE // data, bss, stacks
    })
}

/// Maps `[start, end)` of one memory region: 2 MiB pages where a whole aligned
/// chunk is plain, 4 KiB pages at the edges and wherever the kernel image or page 0
/// needs finer permissions.
fn map_region(root: u64, start: u64, end: u64, default: u64, layout: &Layout) {
    let special = |from: u64, to: u64| from < PAGE || (from < layout.image.1 && to > layout.image.0);
    let mut address = start & !(PAGE - 1);
    while address < end {
        if address % HUGE == 0 && address + HUGE <= end && !special(address, address + HUGE) {
            map_huge(root, address, default);
            address += HUGE;
        } else {
            if let Some(flags) = page_flags(address, default, layout) {
                map_page(root, address, address, flags);
            }
            address += PAGE;
        }
    }
}

/// Programs the PAT so that the "PWT only" combination means write-combining
/// instead of write-through, if the CPU has a PAT. Returns whether it did.
fn setup_write_combining() -> bool {
    let (_, _, _, edx) = cpu::cpuid(1, 0);
    if edx & (1 << 16) == 0 {
        return false;
    }
    // Entries (low byte first): WB, WC (was WT), UC-, UC, then the same again.
    unsafe { core::arch::asm!("wbinvd", options(nomem, nostack)) };
    cpu::write_msr(cpu::MSR_PAT, 0x0007_0106_0007_0106);
    true
}

/// Builds the tables and switches to them. After this, the firmware's mappings are
/// gone: only what is described above is reachable.
pub fn build_and_load(info: &BootInfo, framebuffer: &Framebuffer) {
    let layout = layout();
    let write_combining = setup_write_combining();
    unsafe { WRITE_COMBINING = write_combining };
    let root = new_table() as u64;

    for descriptor in info.memory_descriptors() {
        let start = descriptor.physical_start;
        let end = start + descriptor.pages * PAGE;
        let runtime = descriptor.attribute & ATTRIBUTE_RUNTIME != 0;
        let flags = match descriptor.kind {
            // Device memory the firmware needs at run time (variable store flash...).
            11 | 12 if runtime => WRITABLE | NO_EXECUTE | CACHE_DISABLE | WRITE_THROUGH,
            11 | 12 => continue, // other device memory is mapped by its driver
            // Runtime-services code and data: left executable, as the firmware expects.
            5 | 6 => WRITABLE,
            1..=4 | 7 | 9 | 10 => WRITABLE | NO_EXECUTE,
            _ => continue, // reserved, unusable, persistent...: not ours to touch
        };
        map_region(root, start, end, flags, &layout);
    }
    if framebuffer.base != 0 {
        let flags = WRITABLE | NO_EXECUTE | if write_combining { WRITE_THROUGH } else { CACHE_DISABLE | WRITE_THROUGH };
        map_region(root, framebuffer.base, framebuffer.base + framebuffer.size, flags, &layout);
    }

    // NXE lets the NX bit in page entries mean something (otherwise it is a reserved
    // bit and any entry with it set faults); WP makes ring 0 respect read-only pages.
    cpu::write_msr(cpu::MSR_EFER, cpu::read_msr(cpu::MSR_EFER) | (1 << 11));
    cpu::write_cr0(cpu::read_cr0() | (1 << 16));
    unsafe { ROOT = root };
    cpu::write_cr3(root);
}

/// Makes `[start, start + length)` executable (or not): clears or sets the NX bit of every
/// mapping in the range. Both ends must be 2 MiB aligned. Used for the tape region, the
/// one place the kernel runs code it did not link (tapes are trusted like kernel code;
/// this is not isolation).
pub fn set_executable(start: u64, length: u64, executable: bool) {
    assert!(start % HUGE == 0 && length % HUGE == 0, "tape region must be 2 MiB aligned");
    let root = unsafe { ROOT };
    let apply = |entry: *mut u64| unsafe {
        if executable {
            *entry &= !NO_EXECUTE;
        } else {
            *entry |= NO_EXECUTE;
        }
    };
    let mut address = start;
    while address < start + length {
        unsafe {
            let directory = directory_entry(root, address);
            if *directory & LARGE != 0 {
                apply(directory);
            } else {
                // Mapped with 4 KiB pages (it shares a 2 MiB chunk with the kernel image):
                // the directory entry must allow execution too; the pages decide.
                *directory &= !NO_EXECUTE;
                let table = (*directory & ADDRESS_MASK) as *mut u64;
                for index in 0..512 {
                    if *table.add(index) & PRESENT != 0 {
                        apply(table.add(index));
                    }
                }
            }
        }
        address += HUGE;
    }
    cpu::write_cr3(cpu::read_cr3()); // flush the TLB
}

pub fn root() -> u64 {
    unsafe { ROOT }
}

pub fn write_combining_enabled() -> bool {
    unsafe { WRITE_COMBINING }
}

/// Maps `length` bytes of device registers at physical `address`, uncached and
/// no-execute (identity: the virtual address is the physical one). The registers must
/// not lie inside a 2 MiB page the kernel already maps as RAM.
pub fn map_device(address: u64, length: u64) {
    let root = unsafe { ROOT };
    let mut page = address & !(PAGE - 1);
    while page < address + length {
        map_page(root, page, page, WRITABLE | NO_EXECUTE | CACHE_DISABLE | WRITE_THROUGH);
        unsafe { core::arch::asm!("invlpg [{}]", in(reg) page, options(nostack, preserves_flags)) };
        page += PAGE;
    }
}
