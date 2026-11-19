//! Per-CPU state: one `Cpu` per processor, found through a register the architecture reserves for it (the GS base on x86,
//! `TPIDR_EL1` on aarch64; `arch::cpu_pointer`). The fields every CPU has are here; what only one architecture needs (the GDT and
//! TSS, the APIC id, ...) is `Cpu::arch`.

use crate::arch::CpuArch;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

pub const MAX_CPUS: usize = 128;

#[repr(C)]
pub struct Cpu {
    /// Must stay first: the x86 entry code finds the `Cpu` through `gs:0`.
    pub self_pointer: *const Cpu,
    /// Must stay second: the x86 `syscall` entry uses `gs:8` and `gs:16` (`CpuArch::scratch_rsp`, `kernel_rsp`). `CpuArch`
    /// must therefore have alignment 8 (a field aligned to 16 would push it to offset 16); asserted below.
    pub arch: CpuArch,
    pub index: u32,
    pub node: u32,
    pub ticks: AtomicU64,
    pub online: AtomicBool,
    /// The thread this CPU runs (`sched` keeps it), readable without the scheduler's lock: the debug tools and the trace
    /// ring ask from interrupt handlers and from a machine whose scheduler lock may be stuck.
    pub thread: AtomicUsize,
}

const _: () = assert!(core::mem::offset_of!(Cpu, arch) == 8);

const EMPTY: Cpu = Cpu { self_pointer: core::ptr::null(), arch: CpuArch::new(), index: 0, node: 0, ticks: AtomicU64::new(0), online: AtomicBool::new(false), thread: AtomicUsize::new(0) };

static mut CPUS: [Cpu; MAX_CPUS] = [EMPTY; MAX_CPUS];
static mut COUNT: usize = 1;

pub fn get(index: usize) -> &'static mut Cpu {
    unsafe { &mut (*core::ptr::addr_of_mut!(CPUS))[index] }
}

/// How many CPUs have a `Cpu` entry (started or about to be).
pub fn count() -> usize {
    unsafe { COUNT }
}

pub fn set_count(count: usize) {
    unsafe { COUNT = count.min(MAX_CPUS) };
}

/// The calling CPU's data. Only valid once the CPU has been activated (`arch`).
pub fn this() -> &'static Cpu {
    unsafe { &*crate::arch::cpu_pointer() }
}

/// Set once the boot CPU is activated; application processors activate before they take any lock.
static READY: AtomicBool = AtomicBool::new(false);

pub fn mark_ready() {
    READY.store(true, Ordering::Release);
}

/// The calling CPU's index, or 0 before per-CPU data exists (early boot prints take locks too).
pub fn index() -> usize {
    if READY.load(Ordering::Relaxed) { this().index as usize } else { 0 }
}

pub fn ticks_of(index: usize) -> u64 {
    get(index).ticks.load(Ordering::Relaxed)
}
