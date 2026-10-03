//! The kernel's page tables: identity-map the machine's RAM (and the framebuffer),
//! then switch to them.
//!
//! The firmware leaves the CPU in long mode with its own identity map, built for
//! its needs. The kernel replaces it with one derived from the memory map, so only
//! memory that exists (and the framebuffer) is mapped, and a wild pointer faults
//! instead of reading nothing. M1 uses statically allocated tables for the first
//! 64 GiB with 2 MiB pages and RWX everywhere; M2 builds the tables from the
//! physical allocator, adds NX / read-only text / guard pages and maps higher
//! memory and device registers on demand.

use fos_v2_abi::{BootInfo, Framebuffer, RAM_LIKE_TYPES};

const PAGE_2M: u64 = 2 * 1024 * 1024;
/// One page-directory page per GiB: this many GiB are mappable in M1.
const MAPPED_GIB: usize = 64;

const PRESENT: u64 = 1 << 0;
const WRITABLE: u64 = 1 << 1;
const WRITE_THROUGH: u64 = 1 << 3;
const CACHE_DISABLE: u64 = 1 << 4;
const LARGE: u64 = 1 << 7;

#[repr(C, align(4096))]
struct Table([u64; 512]);

static mut PML4: Table = Table([0; 512]);
static mut PDPT: Table = Table([0; 512]);
static mut PDS: [Table; MAPPED_GIB] = [const { Table([0; 512]) }; MAPPED_GIB];

pub struct Report {
    pub ram_pages: usize,
    pub skipped: usize,
    pub cr3: u64,
}

/// Maps one 2 MiB page at `address`; `false` if it lies above what M1 can map.
fn map(address: u64, flags: u64) -> bool {
    let (gib, entry) = ((address >> 30) as usize, ((address >> 21) & 511) as usize);
    if gib >= MAPPED_GIB {
        return false;
    }
    unsafe {
        let directory = core::ptr::addr_of_mut!(PDS[gib]);
        (*directory).0[entry] = (address & !(PAGE_2M - 1)) | flags | PRESENT | WRITABLE | LARGE;
        (*core::ptr::addr_of_mut!(PDPT)).0[gib] = directory as u64 | PRESENT | WRITABLE;
    }
    true
}

/// Builds the tables for `info`'s memory map and framebuffer and loads CR3.
pub fn build_and_load(info: &BootInfo, framebuffer: &Framebuffer) -> Report {
    let (mut ram_pages, mut skipped) = (0, 0);
    for descriptor in info.memory_descriptors() {
        if !RAM_LIKE_TYPES.contains(&descriptor.kind) {
            continue;
        }
        let end = descriptor.physical_start + descriptor.pages * 4096;
        let mut address = descriptor.physical_start & !(PAGE_2M - 1);
        while address < end {
            if map(address, 0) {
                ram_pages += 1;
            } else {
                skipped += 1;
            }
            address += PAGE_2M;
        }
    }
    // The framebuffer is device memory, not RAM: uncached (PCD + PWT) so stores go
    // straight to the screen.
    if framebuffer.base != 0 {
        let mut address = framebuffer.base & !(PAGE_2M - 1);
        while address < framebuffer.base + framebuffer.size {
            if !map(address, CACHE_DISABLE | WRITE_THROUGH) {
                skipped += 1;
            }
            address += PAGE_2M;
        }
    }
    unsafe {
        (*core::ptr::addr_of_mut!(PML4)).0[0] = core::ptr::addr_of!(PDPT) as u64 | PRESENT | WRITABLE;
    }
    let cr3 = core::ptr::addr_of!(PML4) as u64;
    unsafe { core::arch::asm!("mov cr3, {}", in(reg) cr3, options(nostack, preserves_flags)) };
    Report { ram_pages, skipped, cr3 }
}

pub fn read_cr0() -> u64 {
    let value: u64;
    unsafe { core::arch::asm!("mov {}, cr0", out(reg) value, options(nomem, nostack, preserves_flags)) };
    value
}

pub fn read_cr4() -> u64 {
    let value: u64;
    unsafe { core::arch::asm!("mov {}, cr4", out(reg) value, options(nomem, nostack, preserves_flags)) };
    value
}

/// IA32_EFER (MSR 0xC0000080): bit 8 LME (long mode enable), bit 10 LMA (active).
pub fn read_efer() -> u64 {
    let (low, high): (u32, u32);
    unsafe { core::arch::asm!("rdmsr", in("ecx") 0xc000_0080u32, out("eax") low, out("edx") high, options(nomem, nostack, preserves_flags)) };
    u64::from(high) << 32 | u64::from(low)
}
