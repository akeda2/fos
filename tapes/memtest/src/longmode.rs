//! Glue for the 64-bit half of memtest (`longmode.S`): page tables, the job
//! block, and a CPUID check. The 32-bit kernel cannot address RAM above 4 GiB,
//! so the tape makes short excursions into long mode, one primitive operation
//! over a window of at most a few tens of MiB at a time.
//!
//! The tables are static (they live in the tape's own 4 MiB region, below
//! 4 GiB as `CR3` requires) and identity-map physical memory with 2 MiB pages:
//! the low 8 MiB always (the tape's code, its data and the kernel stack the
//! excursion runs on), and one window of the memory under test while an
//! operation runs. Nothing else is mapped, so a stray 64-bit pointer faults
//! loudly instead of reading a device.

use core::ptr::addr_of_mut;

const PAGE_2M: u64 = 2 * 1024 * 1024;
/// One page-directory page (512 x 2 MiB = 1 GiB) per GiB the tape can map.
pub const MAX_GIB: usize = 64;
const PRESENT_WRITABLE: u64 = 0x3;
const LARGE: u64 = 0x80;

#[repr(C, align(4096))]
struct Page([u64; 512]);

static mut PML4: Page = Page([0; 512]);
static mut PDPT: Page = Page([0; 512]);
static mut PD: [Page; MAX_GIB] = [const { Page([0; 512]) }; MAX_GIB];

/// Shared with `longmode.S`: the layout (offsets in the assembly's header
/// comment) must not change without changing both.
#[repr(C)]
pub struct Job {
    pub op: u64,
    pub base: u64,
    pub qwords: u64,
    pub arg0: u64,
    pub arg1: u64,
    pub arg2: u64,
    pub errors: u64,
    pub recorded: u64,
    pub records: [[u64; 3]; 16],
    pub flips: [u64; 64],
    pub pml4: u64,
}

pub const OP_FILL_PATTERN: u64 = 0;
pub const OP_VERIFY_PATTERN: u64 = 1;
pub const OP_FILL_ADDRESS: u64 = 2;
pub const OP_VERIFY_ADDRESS: u64 = 3;
pub const OP_FILL_RANDOM: u64 = 4;
pub const OP_VERIFY_RANDOM: u64 = 5;
pub const OP_SWEEP: u64 = 6;
pub const OP_FLIP: u64 = 7;

static mut JOB: Job = Job {
    op: 0,
    base: 0,
    qwords: 0,
    arg0: 0,
    arg1: 0,
    arg2: 0,
    errors: 0,
    recorded: 0,
    records: [[0; 3]; 16],
    flips: [0; 64],
    pml4: 0,
};

core::arch::global_asm!(include_str!("longmode.S"), options(att_syntax));

unsafe extern "C" {
    fn memtest64_call(job: *mut Job);
}

/// Whether the CPU has long mode (CPUID.80000001h:EDX bit 29). A Pentium 4
/// does not; the caller then leaves RAM above 4 GiB untested and says so.
pub fn supported() -> bool {
    let highest = core::arch::x86::__cpuid(0x8000_0000).eax;
    highest >= 0x8000_0001 && core::arch::x86::__cpuid(0x8000_0001).edx & (1 << 29) != 0
}

/// Builds the fixed part of the tables. Call once before `run`.
pub fn prepare() {
    unsafe {
        let pml4 = addr_of_mut!(PML4) as *mut u64;
        let pdpt = addr_of_mut!(PDPT) as *mut u64;
        pml4.write(pdpt as u64 | PRESENT_WRITABLE);
        for gib in 0..MAX_GIB {
            let directory = addr_of_mut!(PD[gib]) as *mut u64;
            pdpt.add(gib).write(directory as u64 | PRESENT_WRITABLE);
        }
        // The low 8 MiB: the tape, its tables, the kernel stack.
        let low = addr_of_mut!(PD[0]) as *mut u64;
        for page in 0..4u64 {
            low.add(page as usize).write(page * PAGE_2M | PRESENT_WRITABLE | LARGE);
        }
        (*addr_of_mut!(JOB)).pml4 = pml4 as u64;
    }
}

/// Maps (or unmaps) `[base, base + bytes)` of physical memory at the same
/// linear address. Both must be multiples of 2 MiB, at or above 8 MiB and
/// below 64 GiB (the engine's `high_ranges` guarantees this).
pub fn map_window(base: u64, bytes: u64, on: bool) {
    debug_assert!(base % PAGE_2M == 0 && bytes % PAGE_2M == 0 && base >= 4 * PAGE_2M);
    let first = base / PAGE_2M;
    let last = (base + bytes) / PAGE_2M;
    for page in first..last {
        let (gib, entry) = ((page / 512) as usize, (page % 512) as usize);
        if gib >= MAX_GIB {
            return;
        }
        unsafe {
            let directory = addr_of_mut!(PD[gib]) as *mut u64;
            directory.add(entry).write(if on { page * PAGE_2M | PRESENT_WRITABLE | LARGE } else { 0 });
        }
    }
}

/// Runs one operation in long mode over `qwords` 64-bit words at `base` (which
/// the caller has mapped) and returns the job block with its results.
pub fn run(op: u64, base: u64, qwords: u64, arg0: u64, arg1: u64, arg2: u64) -> &'static Job {
    unsafe {
        let job = addr_of_mut!(JOB);
        (*job).op = op;
        (*job).base = base;
        (*job).qwords = qwords;
        (*job).arg0 = arg0;
        (*job).arg1 = arg1;
        (*job).arg2 = arg2;
        (*job).errors = 0;
        (*job).recorded = 0;
        (*job).flips = [0; 64];
        memtest64_call(job);
        &*job
    }
}
