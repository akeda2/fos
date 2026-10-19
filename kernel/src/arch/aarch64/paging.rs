//! The kernel's own page tables: identity map, 4 KiB granule, 48-bit addresses (four levels).
//!
//! Same policy as the x86-64 `paging.rs`: RAM in 2 MiB blocks and never executable; the kernel image in 4 KiB pages
//! with text read-only+executable, rodata read-only, the rest writable+NX; page 0 and the stack guard unmapped;
//! UEFI runtime code kept executable; device registers mapped on demand (`map_device`) as device-nGnRnE.
//!
//! The firmware leaves the MMU on with its own tables and MAIR/TCR. We build ours from `memory::allocate` frames,
//! then switch in one go (MAIR, TCR, TTBR0, TLB flush) at whichever exception level we were entered (EL1 on QEMU,
//! possibly EL2 on a Pi with edk2). Both levels use TTBR0 only and the same low TCR fields, so one code path serves.

use crate::memory;
use fosfor_abi::BootInfo;

const PAGE: u64 = 4096;
const BLOCK: u64 = 2 * 1024 * 1024;

const VALID: u64 = 1;
const TABLE: u64 = 3; // levels 0-2: next-level table
const PAGE_DESC: u64 = 3; // level 3
const ATTR_NORMAL: u64 = 0 << 2; // MAIR index 0: write-back cacheable
const ATTR_DEVICE: u64 = 1 << 2; // MAIR index 1: device-nGnRnE
const INNER_SHAREABLE: u64 = 3 << 8;
const ACCESS: u64 = 1 << 10;
const READ_ONLY: u64 = 1 << 7;
const PXN: u64 = 1 << 53;
const UXN: u64 = 1 << 54;
const ADDRESS_MASK: u64 = 0x0000_ffff_ffff_f000;

const RAM_DATA: u64 = ATTR_NORMAL | INNER_SHAREABLE | ACCESS | PXN | UXN;
const RAM_CODE: u64 = ATTR_NORMAL | INNER_SHAREABLE | ACCESS | UXN | READ_ONLY;
const RAM_RODATA: u64 = RAM_DATA | READ_ONLY;
const DEVICE: u64 = ATTR_DEVICE | ACCESS | PXN | UXN;

/// MAIR: index 0 = normal write-back (0xff), index 1 = device-nGnRnE (0x00).
const MAIR: u64 = 0xff;

static mut ROOT: u64 = 0;

unsafe extern "C" {
    static __kernel_start: u8;
    static __kernel_end: u8;
    static __text_start: u8;
    static __text_end: u8;
    static __rodata_start: u8;
    static __rodata_end: u8;
    static __stack_guard: u8;
    static __stack_bottom: u8;
}

fn sym(symbol: &u8) -> u64 {
    symbol as *const u8 as u64
}

pub struct Layout {
    pub kernel_start: u64,
    pub kernel_end: u64,
    pub text_start: u64,
    pub text_end: u64,
    pub rodata_start: u64,
    pub rodata_end: u64,
    pub stack_guard: u64,
    pub stack_bottom: u64,
}

pub fn layout() -> Layout {
    unsafe {
        Layout {
            kernel_start: sym(&__kernel_start),
            kernel_end: sym(&__kernel_end),
            text_start: sym(&__text_start),
            text_end: sym(&__text_end),
            rodata_start: sym(&__rodata_start),
            rodata_end: sym(&__rodata_end),
            stack_guard: sym(&__stack_guard),
            stack_bottom: sym(&__stack_bottom),
        }
    }
}

fn entry(table: u64, index: u64) -> *mut u64 {
    (table + index * 8) as *mut u64
}

/// The next-level table under `table[index]`, allocated (zeroed) when absent.
fn next_table(table: u64, index: u64) -> u64 {
    let slot = entry(table, index);
    let value = unsafe { slot.read() };
    if value & VALID == 0 {
        let new = memory::allocate().expect("no frame for a page table");
        unsafe { slot.write(new | TABLE) };
        new
    } else {
        value & ADDRESS_MASK
    }
}

/// The level-3 table covering `address`; a 2 MiB block there is first split into 512 pages with the same attributes.
fn level3(root: u64, address: u64) -> u64 {
    let l1 = next_table(root, (address >> 39) & 0x1ff);
    let l2 = next_table(l1, (address >> 30) & 0x1ff);
    let slot = entry(l2, (address >> 21) & 0x1ff);
    let value = unsafe { slot.read() };
    if value & 3 == 1 {
        let table = memory::allocate().expect("no frame for a page table");
        let attributes = value & !ADDRESS_MASK & !3;
        for page in 0..512u64 {
            unsafe { (table as *mut u64).add(page as usize).write((value & ADDRESS_MASK) + page * PAGE | attributes | PAGE_DESC) };
        }
        unsafe { slot.write(table | TABLE) };
        table
    } else {
        next_table(l2, (address >> 21) & 0x1ff)
    }
}

fn map_page(root: u64, address: u64, attributes: u64) {
    let table = level3(root, address);
    unsafe { entry(table, (address >> 12) & 0x1ff).write(address | attributes | PAGE_DESC) };
}

fn unmap_page(root: u64, address: u64) {
    let table = level3(root, address);
    unsafe { entry(table, (address >> 12) & 0x1ff).write(0) };
}

fn map_block(root: u64, address: u64, attributes: u64) {
    let l1 = next_table(root, (address >> 39) & 0x1ff);
    let l2 = next_table(l1, (address >> 30) & 0x1ff);
    unsafe { entry(l2, (address >> 21) & 0x1ff).write(address | attributes | 1) };
}

/// Maps `[start, end)` as RAM with `attributes`: 2 MiB blocks where aligned, pages at the ragged ends.
fn map_range(root: u64, start: u64, end: u64, attributes: u64) {
    let mut address = start & !(PAGE - 1);
    while address < end {
        if address % BLOCK == 0 && address + BLOCK <= end {
            map_block(root, address, attributes);
            address += BLOCK;
        } else {
            map_page(root, address, attributes);
            address += PAGE;
        }
    }
}

fn current_el() -> u64 {
    let el: u64;
    unsafe { core::arch::asm!("mrs {}, CurrentEL", out(reg) el, options(nomem, nostack)) };
    el >> 2
}

/// Builds the tables from the firmware memory map and the kernel's own layout, then switches to them.
pub fn build_and_load(info: &BootInfo) {
    let root = memory::allocate().expect("no frame for the page-table root");
    unsafe { ROOT = root };

    for descriptor in info.memory_descriptors() {
        // 0 reserved, 11/12 memory-mapped I/O: not RAM, and devices are mapped on demand.
        if matches!(descriptor.kind, 0 | 11 | 12) {
            continue;
        }
        let start = descriptor.physical_start & !(BLOCK - 1);
        let end = (descriptor.physical_start + descriptor.pages * PAGE + BLOCK - 1) & !(BLOCK - 1);
        // UEFI runtime code (5) stays executable for the runtime services.
        let attributes = if descriptor.kind == 5 { RAM_DATA & !PXN } else { RAM_DATA };
        let mut address = start;
        while address < end {
            // Neighbouring descriptors can share a block; the first mapping wins unless this one needs exec.
            let first_free = unsafe { next_table_peek(root, address) };
            if !first_free || descriptor.kind == 5 {
                map_block(root, address, attributes);
            }
            address += BLOCK;
        }
    }

    let l = layout();
    map_range(root, l.kernel_start, l.text_start, RAM_DATA);
    map_range(root, l.text_start, l.text_end, RAM_CODE);
    map_range(root, l.rodata_start, l.rodata_end, RAM_RODATA);
    map_range(root, l.rodata_end, l.kernel_end, RAM_DATA);
    // The stack guard: one page that faults instead of silently corrupting what lies below the stack.
    unmap_page(root, l.stack_guard);
    // Page 0 stays unmapped so a null dereference faults (if RAM starts there at all).
    unmap_page(root, 0);

    load(root);
}

/// True when a block or page mapping already exists for `address`.
unsafe fn next_table_peek(root: u64, address: u64) -> bool {
    let l0 = unsafe { entry(root, (address >> 39) & 0x1ff).read() };
    if l0 & VALID == 0 {
        return false;
    }
    let l1 = unsafe { entry(l0 & ADDRESS_MASK, (address >> 30) & 0x1ff).read() };
    if l1 & VALID == 0 {
        return false;
    }
    let l2 = unsafe { entry(l1 & ADDRESS_MASK, (address >> 21) & 0x1ff).read() };
    l2 & VALID != 0
}

fn load(root: u64) {
    let mmfr0: u64;
    unsafe { core::arch::asm!("mrs {}, id_aa64mmfr0_el1", out(reg) mmfr0, options(nomem, nostack)) };
    let pa_range = (mmfr0 & 0xf).min(5);
    // T0SZ 16 (48-bit), inner/outer write-back write-allocate, inner shareable, 4 KiB granule.
    let low = 16 | (1 << 8) | (1 << 10) | (3 << 12);
    unsafe {
        core::arch::asm!("dsb ish", options(nostack));
        if current_el() == 2 {
            let tcr = low | (pa_range << 16) | (1 << 23) | (1 << 31);
            core::arch::asm!(
                "msr mair_el2, {mair}", "msr tcr_el2, {tcr}", "msr ttbr0_el2, {root}", "isb",
                "tlbi alle2", "dsb ish", "isb",
                mair = in(reg) MAIR | (0x00 << 8), tcr = in(reg) tcr, root = in(reg) root, options(nostack)
            );
        } else {
            let tcr = low | (1 << 23) | (pa_range << 32);
            core::arch::asm!(
                "msr mair_el1, {mair}", "msr tcr_el1, {tcr}", "msr ttbr0_el1, {root}", "isb",
                "tlbi vmalle1", "dsb ish", "isb",
                mair = in(reg) MAIR | (0x00 << 8), tcr = in(reg) tcr, root = in(reg) root, options(nostack)
            );
        }
    }
}

pub fn root() -> u64 {
    unsafe { ROOT }
}

/// Maps `length` bytes of device registers at physical `address` (identity), device-nGnRnE and never executable.
pub fn map_device(address: u64, length: u64) {
    let root = unsafe { ROOT };
    let mut page = address & !(PAGE - 1);
    while page < address + length {
        map_page(root, page, DEVICE);
        page += PAGE;
    }
    unsafe {
        if current_el() == 2 {
            core::arch::asm!("dsb ishst", "tlbi alle2", "dsb ish", "isb", options(nostack));
        } else {
            core::arch::asm!("dsb ishst", "tlbi vmalle1", "dsb ish", "isb", options(nostack));
        }
    }
}

/// True if `address` has a valid mapping in the kernel's tables.
pub fn is_mapped(address: u64) -> bool {
    let root = unsafe { ROOT };
    if root == 0 {
        return true;
    }
    let mut table = root;
    for level in 0..4u64 {
        let value = unsafe { entry(table, (address >> (39 - 9 * level)) & 0x1ff).read() };
        if value & VALID == 0 {
            return false;
        }
        if level == 3 || value & 3 == 1 {
            return true;
        }
        table = value & ADDRESS_MASK;
    }
    false
}
