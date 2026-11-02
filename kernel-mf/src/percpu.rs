//! Per-CPU state, reached through the GS base: `gs:0` holds the address of the CPU's own `Cpu`.
//!
//! Each CPU has its own GDT and TSS (a TSS can be loaded by one CPU only: `ltr` marks it busy) and its own double-fault
//! stack, so a stack overflow on one CPU cannot take the handler of another down with it.

use crate::gdt::CpuGdt;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

pub const MAX_CPUS: usize = 128;
const MSR_GS_BASE: u32 = 0xc000_0101;

#[repr(C)]
pub struct Cpu {
    /// Must stay first: `gs:0` is this address.
    pub self_pointer: *const Cpu,
    pub index: u32,
    pub apic_id: u32,
    pub node: u32,
    pub ticks: AtomicU64,
    pub online: AtomicBool,
    /// Top of this CPU's double-fault stack (what `activate` puts in IST1).
    pub ist_top: u64,
    pub gdt: CpuGdt,
}

const EMPTY: Cpu = Cpu { self_pointer: core::ptr::null(), index: 0, apic_id: 0, node: 0, ticks: AtomicU64::new(0), online: AtomicBool::new(false), ist_top: 0, gdt: CpuGdt::new() };

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

/// The calling CPU's data. Only valid once `activate` has run on this CPU.
pub fn this() -> &'static Cpu {
    let pointer: *const Cpu;
    unsafe { core::arch::asm!("mov {}, gs:[0]", out(reg) pointer, options(nostack, preserves_flags, readonly)) };
    unsafe { &*pointer }
}

/// Loads this CPU's GDT/TSS and points GS at its `Cpu`. The segment reload clears the GS base, so it is set afterwards.
pub fn activate(index: usize, ist_top: u64) {
    let cpu = get(index);
    cpu.self_pointer = cpu as *const Cpu;
    cpu.index = index as u32;
    cpu.ist_top = ist_top;
    crate::gdt::load(&mut cpu.gdt, ist_top);
    crate::cpu::write_msr(MSR_GS_BASE, cpu as *const Cpu as u64);
}

pub fn ticks_of(index: usize) -> u64 {
    get(index).ticks.load(Ordering::Relaxed)
}
