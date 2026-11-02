//! The server kernel's page tables.
//!
//! The firmware leaves the CPU on an identity map of its own. This replaces it with:
//!
//! * a **direct map** of every RAM-like region at `DIRECT_BASE + physical` (read/write, no-execute, 2 MiB pages where a whole
//!   aligned chunk is RAM, 4 KiB at the edges). All physical memory the kernel touches after the switch goes through
//!   `direct()`: the frame bitmap, page tables, the heap, DMA buffers;
//! * the **kernel image**, still at its load address (it is a position-independent image the loader placed, so it keeps its
//!   identity mapping for now), in 4 KiB pages with W^X per section: text R-X, read-only data R--, data/bss/stack RW-. Page 0
//!   and the guard pages under the stacks are unmapped, so a null pointer or a stack overflow faults at once;
//! * the framebuffer (or the BIOS text screen), uncached and no-execute, through the direct map.
//!
//! Everything else in the lower half stays empty: it is for per-process address spaces (slice 8). Not mapped on purpose: UEFI
//! runtime services (this kernel does not call the firmware yet), loader data (copied out by `boot::Machine`), device
//! registers (mapped on demand by their drivers, later).

use crate::boot::Machine;
use crate::pmm;
use fosfor_abi::RAM_LIKE_TYPES;

const PAGE: u64 = 4096;
const HUGE: u64 = 2 * 1024 * 1024;

/// Start of the direct map: the first address of the upper half of the 48-bit space.
pub const DIRECT_BASE: u64 = 0xffff_8000_0000_0000;

const PRESENT: u64 = 1 << 0;
const WRITABLE: u64 = 1 << 1;
const WRITE_THROUGH: u64 = 1 << 3;
const CACHE_DISABLE: u64 = 1 << 4;
const LARGE: u64 = 1 << 7;
const NO_EXECUTE: u64 = 1 << 63;
const ADDRESS_MASK: u64 = 0x000f_ffff_ffff_f000;

const VGA_TEXT_BASE: u64 = 0xb8000;

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

/// What `direct()` adds: 0 until the tables are loaded (the firmware's identity map), `DIRECT_BASE` afterwards.
static mut OFFSET: u64 = 0;
static mut ROOT: u64 = 0;

/// The virtual address through which physical memory `physical` is reached right now.
pub fn direct(physical: u64) -> u64 {
    physical + unsafe { OFFSET }
}

pub fn root() -> u64 {
    unsafe { ROOT }
}

struct Layout {
    image: (u64, u64),
    text: (u64, u64),
    rodata: (u64, u64),
    stack_guard: u64,
    stack_bottom: u64,
    ist_guard: u64,
}

fn layout() -> Layout {
    Layout {
        image: (&raw const __kernel_start as u64, &raw const __kernel_end as u64),
        text: (&raw const __text_start as u64, &raw const __text_end as u64),
        rodata: (&raw const __rodata_start as u64, &raw const __rodata_end as u64),
        stack_guard: &raw const __stack_guard as u64,
        stack_bottom: &raw const __stack_bottom as u64,
        ist_guard: &raw const __ist_guard as u64,
    }
}

fn new_table() -> *mut u64 {
    match pmm::allocate(1, None) {
        Some(address) => direct(address) as *mut u64,
        None => {
            println!("vm: out of frames for page tables: stopping.");
            crate::halt();
        }
    }
}

/// The table an entry points to, creating it if empty. Intermediate entries are permissive; the leaf decides.
unsafe fn next_level(entry: *mut u64) -> *mut u64 {
    unsafe {
        if *entry & PRESENT == 0 {
            let table = new_table() as u64 - direct(0);
            *entry = table | PRESENT | WRITABLE;
        }
        direct(*entry & ADDRESS_MASK) as *mut u64
    }
}

unsafe fn directory_entry(root: u64, address: u64) -> *mut u64 {
    unsafe {
        let pml4 = direct(root) as *mut u64;
        let pdpt = next_level(pml4.add(((address >> 39) & 511) as usize));
        let directory = next_level(pdpt.add(((address >> 30) & 511) as usize));
        directory.add(((address >> 21) & 511) as usize)
    }
}

fn map_huge(root: u64, virtual_address: u64, physical: u64, flags: u64) {
    unsafe { *directory_entry(root, virtual_address) = physical | flags | PRESENT | LARGE };
}

fn map_page(root: u64, virtual_address: u64, physical: u64, flags: u64) {
    unsafe {
        let entry = directory_entry(root, virtual_address);
        assert!(*entry & LARGE == 0, "a 4 KiB mapping inside a 2 MiB page");
        let table = next_level(entry);
        *table.add(((virtual_address >> 12) & 511) as usize) = physical | flags | PRESENT;
    }
}

/// Maps physical `[start, end)` at `DIRECT_BASE + physical`.
fn map_direct(root: u64, start: u64, end: u64, flags: u64) {
    let mut address = start & !(PAGE - 1);
    while address < end {
        if address % HUGE == 0 && address + HUGE <= end {
            map_huge(root, DIRECT_BASE + address, address, flags);
            address += HUGE;
        } else {
            map_page(root, DIRECT_BASE + address, address, flags);
            address += PAGE;
        }
    }
}

/// Maps the kernel image page by page with W^X permissions; unmapped pages are the guards.
fn map_image(root: u64, layout: &Layout) {
    let mut address = layout.image.0.max(PAGE) & !(PAGE - 1);
    while address < layout.image.1 {
        let guard = (layout.stack_guard..layout.stack_bottom).contains(&address) || (layout.ist_guard..layout.ist_guard + PAGE).contains(&address);
        if !guard {
            let flags = if (layout.text.0..layout.text.1).contains(&address) {
                0 // read + execute
            } else if (layout.rodata.0..layout.rodata.1).contains(&address) {
                NO_EXECUTE
            } else {
                WRITABLE | NO_EXECUTE
            };
            map_page(root, address, address, flags);
        }
        address += PAGE;
    }
}

/// What a lookup found: the physical address and the page's permissions.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Mapping {
    pub physical: u64,
    pub writable: bool,
    pub executable: bool,
}

/// Walks the kernel's tables for `address`.
pub fn translate(address: u64) -> Option<Mapping> {
    let root = root();
    if root == 0 {
        return None;
    }
    unsafe {
        let read = |table: u64, index: u64| (direct(table & ADDRESS_MASK) as *const u64).add(index as usize).read();
        let pml4 = read(root, (address >> 39) & 511);
        if pml4 & PRESENT == 0 {
            return None;
        }
        let pdpt = read(pml4, (address >> 30) & 511);
        if pdpt & PRESENT == 0 {
            return None;
        }
        let directory = read(pdpt, (address >> 21) & 511);
        if directory & PRESENT == 0 {
            return None;
        }
        let (leaf, offset) = if directory & LARGE != 0 {
            (directory, address & (HUGE - 1))
        } else {
            let entry = read(directory, (address >> 12) & 511);
            if entry & PRESENT == 0 {
                return None;
            }
            (entry, address & (PAGE - 1))
        };
        let physical = if directory & LARGE != 0 { (leaf & ADDRESS_MASK & !(HUGE - 1)) + offset } else { (leaf & ADDRESS_MASK) + offset };
        Some(Mapping { physical, writable: leaf & WRITABLE != 0, executable: leaf & NO_EXECUTE == 0 })
    }
}

/// The permissions of the kernel's own image, for the boot self-check: (text, rodata, data, stack guard, null page).
pub fn check_image() -> bool {
    let l = layout();
    let text = translate(l.text.0);
    let rodata = translate(l.rodata.0);
    let data = translate(l.rodata.1);
    let ok = matches!(text, Some(m) if !m.writable && m.executable)
        && matches!(rodata, Some(m) if !m.writable && !m.executable)
        && matches!(data, Some(m) if m.writable && !m.executable)
        && translate(l.stack_guard).is_none()
        && translate(l.ist_guard).is_none()
        && translate(0).is_none();
    ok
}

/// Makes sure `[physical, physical + length)` is reachable through the direct map, mapping any missing 4 KiB page uncached
/// and no-execute: for firmware tables the memory map does not describe, and for device registers (the APICs, later PCI BARs).
/// A page that is already mapped (RAM) is left as it is.
pub fn map_device(physical: u64, length: u64) {
    let root = root();
    let mut page = physical & !(PAGE - 1);
    while page < physical + length.max(1) {
        if translate(direct(page)).is_none() {
            map_page(root, DIRECT_BASE + page, page, WRITABLE | NO_EXECUTE | CACHE_DISABLE | WRITE_THROUGH);
            unsafe { core::arch::asm!("invlpg [{}]", in(reg) DIRECT_BASE + page, options(nostack, preserves_flags)) };
        }
        page += PAGE;
    }
}

/// Maps the 4 KiB page at `address` onto itself, executable and writable, in the kernel's tables: the AP trampoline runs
/// here with paging switched on before it can jump to kernel code. Boot CPU only, while the APs are being started.
pub fn identity_map_page(address: u64) {
    map_page(root(), address, address, WRITABLE);
    unsafe { core::arch::asm!("invlpg [{}]", in(reg) address, options(nostack, preserves_flags)) };
}

/// Removes that mapping again. Another CPU may keep a stale TLB entry for it; nothing executes there any more.
pub fn identity_unmap_page(address: u64) {
    unsafe {
        let entry = directory_entry(root(), address);
        if *entry & PRESENT != 0 && *entry & LARGE == 0 {
            let table = direct(*entry & ADDRESS_MASK) as *mut u64;
            *table.add(((address >> 12) & 511) as usize) = 0;
        }
        core::arch::asm!("invlpg [{}]", in(reg) address, options(nostack, preserves_flags));
    }
}

/// Where kernel thread stacks live: far above the direct map, 4 KiB pages, each slot one unmapped guard page then the stack.
pub const STACK_REGION: u64 = 0xffff_c000_0000_0000;

static STACK_LOCK: crate::sync::SpinLock<()> = crate::sync::SpinLock::new(());

/// Maps `pages` frames starting at physical `frames` as the stack of slot `slot` (a guard page sits below it, left unmapped
/// so an overflow faults at once). Returns the stack's top (the initial stack pointer).
pub fn map_stack(slot: u64, pages: u64, frames: u64) -> u64 {
    let _guard = STACK_LOCK.lock();
    let base = STACK_REGION + slot * (pages + 1) * PAGE + PAGE;
    for page in 0..pages {
        map_page(root(), base + page * PAGE, frames + page * PAGE, WRITABLE | NO_EXECUTE);
    }
    base + pages * PAGE
}

/// Unmaps what `map_stack` mapped (the caller frees the frames). Other CPUs may keep stale TLB entries, which nothing uses.
pub fn unmap_stack(slot: u64, pages: u64) {
    let _guard = STACK_LOCK.lock();
    let base = STACK_REGION + slot * (pages + 1) * PAGE + PAGE;
    for page in 0..pages {
        identity_unmap_page(base + page * PAGE);
    }
}

/// What a faulting address is, for the exception report.
pub fn describe(address: u64) -> &'static str {
    let l = layout();
    if address < PAGE {
        "the null page (a null pointer was used)"
    } else if (l.stack_guard..l.stack_bottom).contains(&address) {
        "the kernel stack guard page (stack overflow)"
    } else if (l.ist_guard..l.ist_guard + PAGE).contains(&address) {
        "the double-fault stack guard page"
    } else if (l.text.0..l.text.1).contains(&address) {
        "kernel code (read-only)"
    } else if (l.rodata.0..l.rodata.1).contains(&address) {
        "kernel read-only data"
    } else if (l.image.0..l.image.1).contains(&address) {
        "kernel data"
    } else if address >= STACK_REGION {
        "a kernel thread stack region (an unmapped page there is a guard page: stack overflow)"
    } else if address >= DIRECT_BASE {
        "the direct map (physical memory)"
    } else {
        "unmapped memory"
    }
}

/// Builds the tables and switches to them. After this, only what the module comment lists is reachable.
pub fn init(machine: &Machine) -> u64 {
    let layout = layout();
    let root = pmm::allocate(1, Some(1 << 32)).expect("a page-table root below 4 GiB") as u64; // APs load a 32-bit CR3
    let data = WRITABLE | NO_EXECUTE;

    let mut highest = 0;
    for d in machine.descriptors() {
        if RAM_LIKE_TYPES.contains(&d.kind) {
            let end = d.physical_start + d.pages * PAGE;
            map_direct(root, d.physical_start, end, data);
            highest = highest.max(end);
        }
    }
    map_image(root, &layout);
    let fb = &machine.framebuffer;
    if fb.base != 0 {
        map_direct(root, fb.base, fb.base + fb.size, data | CACHE_DISABLE | WRITE_THROUGH);
    } else if machine.runtime_services == 0 {
        // A BIOS boot without a linear framebuffer: the screen is the VGA text window, which is not RAM in the memory map.
        map_direct(root, VGA_TEXT_BASE, VGA_TEXT_BASE + 0x8000, data | CACHE_DISABLE | WRITE_THROUGH);
    }

    // NXE makes the NX bit in entries mean something; WP makes ring 0 respect read-only pages.
    let efer: u64 = {
        let (low, high): (u32, u32);
        unsafe { core::arch::asm!("rdmsr", in("ecx") 0xc000_0080u32, out("eax") low, out("edx") high, options(nomem, nostack)) };
        (high as u64) << 32 | low as u64
    };
    let efer = efer | 1 << 11;
    unsafe {
        core::arch::asm!("wrmsr", in("ecx") 0xc000_0080u32, in("eax") efer as u32, in("edx") (efer >> 32) as u32, options(nomem, nostack));
        let cr0: u64;
        core::arch::asm!("mov {}, cr0", out(reg) cr0, options(nomem, nostack));
        core::arch::asm!("mov cr0, {}", in(reg) cr0 | 1 << 16, options(nomem, nostack));
        ROOT = root;
        core::arch::asm!("mov cr3, {}", in(reg) root, options(nostack));
        OFFSET = DIRECT_BASE;
    }
    highest
}
