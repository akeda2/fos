//! The x86 half of the per-CPU data: `gs:0` holds the address of the CPU's own `Cpu`.
//!
//! Each CPU has its own GDT and TSS (a TSS can be loaded by one CPU only: `ltr` marks it busy) and its own double-fault
//! stack, so a stack overflow on one CPU cannot take the handler of another down with it.

use crate::gdt::CpuGdt;
use crate::percpu::{Cpu, count, get};

const MSR_GS_BASE: u32 = 0xc000_0101;
/// The interrupt-stack area is 16 KiB on every CPU (the linker's `__ist_top` area on the boot CPU, `smp::IST_FRAMES` on the
/// others): 8 KiB for the double fault, 8 KiB below it for the NMI.
pub const IST_HALF: u64 = 8192;

#[repr(C)]
pub struct CpuArch {
    /// `gs:8`: where the `syscall` entry parks the user's stack pointer while it switches to the kernel stack.
    pub scratch_rsp: u64,
    /// `gs:16`: top of the running thread's kernel stack (what `syscall` and, through the TSS, interrupts from ring 3 switch to).
    pub kernel_rsp: u64,
    pub apic_id: u32,
    /// Top of this CPU's interrupt-stack area: the double-fault stack (IST1) is its upper half, the NMI stack (IST2) the lower.
    pub ist_top: u64,
    pub gdt: CpuGdt,
}

impl CpuArch {
    pub const fn new() -> CpuArch {
        CpuArch { scratch_rsp: 0, kernel_rsp: 0, apic_id: 0, ist_top: 0, gdt: CpuGdt::new() }
    }
}

pub fn cpu_pointer() -> *const Cpu {
    let pointer: *const Cpu;
    unsafe { core::arch::asm!("mov {}, gs:[0]", out(reg) pointer, options(nostack, preserves_flags, readonly)) };
    pointer
}

/// Where a thread that may enter ring 3 keeps its kernel stack: interrupts and `syscall` from ring 3 land on it.
pub fn set_kernel_stack(top: u64) {
    let cpu = get(crate::percpu::this().index as usize);
    cpu.arch.kernel_rsp = top;
    crate::gdt::set_rsp0(&mut cpu.arch.gdt, top);
}

/// Loads this CPU's GDT/TSS and points GS at its `Cpu`. The segment reload clears the GS base, so it is set afterwards.
pub fn activate(index: usize, ist_top: u64) {
    let cpu = get(index);
    cpu.self_pointer = cpu as *const Cpu;
    cpu.index = index as u32;
    cpu.arch.ist_top = ist_top;
    crate::gdt::load(&mut cpu.arch.gdt, ist_top);
    crate::gdt::set_nmi_stack(&mut cpu.arch.gdt, ist_top - IST_HALF);
    crate::cpu::write_msr(MSR_GS_BASE, cpu as *const Cpu as u64);
    crate::percpu::mark_ready();
}

/// The APIC id device interrupts are sent to: the CPU of `irq-cpu N` in `settings` if it is running, else the boot CPU.
/// MSI messages and I/O APIC pins both ask here, once, when a device is set up (after `config::load`).
pub fn interrupt_target() -> u32 {
    let wanted = crate::config::irq_cpu();
    if wanted < count() && get(wanted).online.load(core::sync::atomic::Ordering::Acquire) { get(wanted).arch.apic_id } else { get(0).arch.apic_id }
}
