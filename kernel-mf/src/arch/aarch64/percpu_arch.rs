//! The aarch64 half of the per-CPU data: `TPIDR_EL1` holds the address of the CPU's own `Cpu` (EL2 may read and write it too,
//! which matters on a Pi 4 where the firmware enters at EL2).

use crate::percpu::{Cpu, get};

pub struct CpuArch {
    /// MPIDR_EL1's affinity bits, what PSCI `CPU_ON` names a core by.
    pub mpidr: u64,
    /// The running thread's kernel stack top: where `enter_user_mode` points SP_EL1, and so where every exception from EL0 lands.
    pub kernel_stack: core::sync::atomic::AtomicU64,
    /// This CPU's bit in the GIC's target lists (what a software-generated interrupt names it by).
    pub gic_target: core::sync::atomic::AtomicU8,
}

impl CpuArch {
    pub const fn new() -> CpuArch {
        CpuArch { mpidr: 0, kernel_stack: core::sync::atomic::AtomicU64::new(0), gic_target: core::sync::atomic::AtomicU8::new(1) }
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
    crate::percpu::mark_ready();
}

/// Records the kernel stack top of the thread now running on this CPU: user threads enter EL0 with SP_EL1 set to it.
pub fn set_kernel_stack(top: u64) {
    crate::percpu::this().arch.kernel_stack.store(top, core::sync::atomic::Ordering::Relaxed);
}
