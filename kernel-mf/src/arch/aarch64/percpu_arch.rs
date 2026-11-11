//! The aarch64 half of the per-CPU data: `TPIDR_EL1` holds the address of the CPU's own `Cpu` (EL2 may read and write it too,
//! which matters on a Pi 4 where the firmware enters at EL2).

use crate::percpu::{Cpu, get};

pub struct CpuArch {
    /// MPIDR_EL1's affinity bits, what PSCI `CPU_ON` names a core by.
    pub mpidr: u64,
}

impl CpuArch {
    pub const fn new() -> CpuArch {
        CpuArch { mpidr: 0 }
    }
}

pub fn cpu_pointer() -> *const Cpu {
    let pointer: u64;
    unsafe { core::arch::asm!("mrs {}, tpidr_el1", out(reg) pointer, options(nomem, nostack, preserves_flags)) };
    pointer as *const Cpu
}

/// Points `TPIDR_EL1` at CPU `index`'s `Cpu` and marks it online.
pub fn activate(index: usize) {
    let cpu = get(index);
    cpu.self_pointer = cpu as *const Cpu;
    cpu.index = index as u32;
    let mpidr: u64;
    unsafe { core::arch::asm!("mrs {}, mpidr_el1", out(reg) mpidr, options(nomem, nostack)) };
    cpu.arch.mpidr = mpidr & 0xff_00ff_ffff;
    unsafe { core::arch::asm!("msr tpidr_el1, {}", in(reg) cpu as *const Cpu as u64, options(nomem, nostack)) };
    cpu.online.store(true, core::sync::atomic::Ordering::Release);
}

/// A thread's kernel stack top matters once threads enter EL0 (exceptions from EL0 land on SP_EL1); kernel threads do not need it.
pub fn set_kernel_stack(_top: u64) {}
