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
const ATTR_UNCACHED: u64 = 2 << 2; // MAIR index 2: normal non-cacheable (the display reads RAM behind the CPU cache)
const FRAMEBUFFER: u64 = ATTR_UNCACHED | INNER_SHAREABLE | ACCESS | PXN | UXN;
const DEVICE: u64 = ATTR_DEVICE | ACCESS | PXN | UXN;

/// MAIR: index 0 = normal write-back (0xff), 1 = device-nGnRnE (0x00), 2 = normal non-cacheable (0x44).
const MAIR: u64 = 0xff | (0x44 << 16);

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
pub fn build_and_load(info: &BootInfo, framebuffer: &fosfor_abi::Framebuffer) {
    let root = memory::allocate().expect("no frame for the page-table root");
    unsafe { ROOT = root };
    super::early::mark(10);

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
    // The framebuffer is non-cacheable so the display sees every store without cache maintenance.
    if framebuffer.base != 0 && framebuffer.format < fosfor_abi::PIXEL_NONE {
        let start = framebuffer.base & !(PAGE - 1);
        let end = (framebuffer.base + framebuffer.size + PAGE - 1) & !(PAGE - 1);
        let mut address = start;
        while address < end {
            map_page(root, address, FRAMEBUFFER);
            address += PAGE;
        }
    }
    // The stack guard: one page that faults instead of silently corrupting what lies below the stack.
    unmap_page(root, l.stack_guard);
    // Page 0 stays unmapped so a null dereference faults (if RAM starts there at all).
    unmap_page(root, 0);

    // Hardware bring-up: row 4 = framebuffer base, row 5 = kernel entry stack/level (current EL), row 6 = root.
    super::early::bits(4, framebuffer.base);
    super::early::bits(5, current_el());
    super::early::bits(6, root);
    diagnostics(root, framebuffer.base);
    super::early::mark(11);
    load(root);
    super::early::mark(12);
}

/// The descriptor our tables hold for `address` (0 when unmapped), found by a software walk.
fn walk(root: u64, address: u64) -> u64 {
    let l0 = unsafe { entry(root, (address >> 39) & 0x1ff).read() };
    if l0 & VALID == 0 {
        return 0;
    }
    let l1 = unsafe { entry(l0 & ADDRESS_MASK, (address >> 30) & 0x1ff).read() };
    if l1 & VALID == 0 {
        return 0;
    }
    let l2 = unsafe { entry(l1 & ADDRESS_MASK, (address >> 21) & 0x1ff).read() };
    if l2 & 3 != 3 {
        return l2;
    }
    unsafe { entry(l2 & ADDRESS_MASK, (address >> 12) & 0x1ff).read() }
}

macro_rules! sysreg {
    ($name:literal) => {{
        let v: u64;
        unsafe { core::arch::asm!(concat!("mrs {}, ", $name), out(reg) v, options(nomem, nostack)) };
        v
    }};
}

/// Hardware bring-up: draws what the firmware left in the MMU registers and what our new tables say about the
/// addresses the kernel needs next (rows 7-15 of `early::bits`, MSB first: tcr, sctlr, ttbr0, mair, hcr, then
/// descriptors for pc, sp, framebuffer and the vector table).
fn diagnostics(root: u64, framebuffer: u64) {
    let (tcr, sctlr, ttbr, mair, hcr, vbar) = if current_el() == 2 {
        (sysreg!("tcr_el2"), sysreg!("sctlr_el2"), sysreg!("ttbr0_el2"), sysreg!("mair_el2"), sysreg!("hcr_el2"), sysreg!("vbar_el2"))
    } else {
        (sysreg!("tcr_el1"), sysreg!("sctlr_el1"), sysreg!("ttbr0_el1"), sysreg!("mair_el1"), 0, sysreg!("vbar_el1"))
    };
    let pc: u64;
    let sp: u64;
    unsafe {
        core::arch::asm!("adr {}, .", out(reg) pc, options(nomem, nostack));
        core::arch::asm!("mov {}, sp", out(reg) sp, options(nomem, nostack));
    }
    let early = super::early::bits;
    early(7, tcr);
    early(8, sctlr);
    early(9, ttbr);
    early(10, mair);
    early(11, hcr);
    early(12, walk(root, pc));
    early(13, walk(root, sp));
    early(14, walk(root, framebuffer));
    early(15, walk(root, vbar));
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
            // MMU off while MAIR/TCR/TTBR0 change: the firmware's TCR may use another T0SZ than ours, and
            // the window between the writes would walk its tables with the wrong geometry. No memory access
            // inside the block (everything is in registers), so the cache state does not matter.
            // Probes: 4x8 white pixels at y=28, x=380/400/420/440, stored straight to the framebuffer (the stores
            // work with the MMU off too): before the switch, MMU off, registers written, MMU back on.
            let (p0, pitch) = super::early::pixel_address(380, 28);
            let (p1, _) = super::early::pixel_address(400, 28);
            let (p2, _) = super::early::pixel_address(420, 28);
            let (p3, _) = super::early::pixel_address(440, 28);
            core::arch::asm!(
                "mov {w:w}, #0xffff", "movk {w:w}, #0xff, lsl #16",
                "mov {c}, #8", "2: stp {w:w}, {w:w}, [{p0}]", "stp {w:w}, {w:w}, [{p0}, #8]", "add {p0}, {p0}, {pitch}", "subs {c}, {c}, #1", "b.ne 2b",
                "mrs {s}, sctlr_el2", "bic {t}, {s}, #1", "msr sctlr_el2, {t}", "isb",
                "mov {c}, #8", "2: stp {w:w}, {w:w}, [{p1}]", "stp {w:w}, {w:w}, [{p1}, #8]", "add {p1}, {p1}, {pitch}", "subs {c}, {c}, #1", "b.ne 2b",
                "msr mair_el2, {mair}", "msr tcr_el2, {tcr}", "msr ttbr0_el2, {root}", "isb",
                "tlbi alle2", "dsb nsh", "isb",
                "mov {c}, #8", "2: stp {w:w}, {w:w}, [{p2}]", "stp {w:w}, {w:w}, [{p2}, #8]", "add {p2}, {p2}, {pitch}", "subs {c}, {c}, #1", "b.ne 2b",
                "msr sctlr_el2, {s}", "isb",
                "mov {c}, #8", "2: stp {w:w}, {w:w}, [{p3}]", "stp {w:w}, {w:w}, [{p3}, #8]", "add {p3}, {p3}, {pitch}", "subs {c}, {c}, #1", "b.ne 2b",
                s = out(reg) _, t = out(reg) _, w = out(reg) _, c = out(reg) _,
                p0 = inout(reg) p0 => _, p1 = inout(reg) p1 => _, p2 = inout(reg) p2 => _, p3 = inout(reg) p3 => _, pitch = in(reg) pitch,
                mair = in(reg) MAIR, tcr = in(reg) tcr, root = in(reg) root, options(nostack)
            );
        } else {
            let tcr = low | (1 << 23) | (pa_range << 32);
            core::arch::asm!(
                "mrs {s}, sctlr_el1", "bic {t}, {s}, #1", "msr sctlr_el1, {t}", "isb",
                "msr mair_el1, {mair}", "msr tcr_el1, {tcr}", "msr ttbr0_el1, {root}", "isb",
                "tlbi vmalle1", "dsb nsh", "isb",
                "msr sctlr_el1, {s}", "isb",
                s = out(reg) _, t = out(reg) _,
                mair = in(reg) MAIR, tcr = in(reg) tcr, root = in(reg) root, options(nostack)
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

/// Makes `[start, start + length)` (2 MiB aligned, RAM) executable or not again: tapes run from one such region. The
/// region stays writable (the loader fills it); the kernel trusts tapes as it trusts itself. Callers sync the caches
/// afterwards (`cpu::sync_code`).
pub fn set_executable(start: u64, length: u64, executable: bool) {
    let root = unsafe { ROOT };
    let mut address = start;
    while address < start + length {
        let l1 = next_table(root, (address >> 39) & 0x1ff);
        let l2 = next_table(l1, (address >> 30) & 0x1ff);
        let slot = entry(l2, (address >> 21) & 0x1ff);
        let value = unsafe { slot.read() };
        assert!(value & 3 == 1, "set_executable: not a 2 MiB RAM block");
        let value = if executable { value & !(PXN | UXN) } else { value | PXN | UXN };
        unsafe { slot.write(value) };
        address += BLOCK;
    }
    unsafe {
        if current_el() == 2 {
            core::arch::asm!("dsb ishst", "tlbi alle2", "dsb ish", "isb", options(nostack));
        } else {
            core::arch::asm!("dsb ishst", "tlbi vmalle1", "dsb ish", "isb", options(nostack));
        }
    }
}
