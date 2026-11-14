//! Starting the other CPUs with PSCI `CPU_ON` (the processors come from MADT's GICC entries, the conduit from the FADT).
//!
//! A started CPU arrives at `ap_entry` with its MMU and caches off, at EL2 when the firmware has EL2 (QEMU with
//! `virtualization=on`, a Pi 4) and at EL1 otherwise, with `x0` = the `context` argument: the address of its `Boot` record. The
//! record carries the boot CPU's translation registers, so every CPU runs on the same page tables and vectors; at EL2 the CPU
//! first makes the same drop to EL1 the boot CPU made (`paging::enter_el1`). The record is cleaned to the point of coherency
//! because the new CPU reads it with its caches off. `ap_main` then does what the boot CPU does per CPU (GIC interface, timer,
//! FP/PAN) and joins the scheduler as an idle CPU.

use crate::percpu;
use core::sync::atomic::Ordering;

const STACK_FRAMES: u64 = 8;
const PSCI_CPU_ON: u64 = 0xc400_0003;

/// Read by `ap_entry` by offset: keep the order.
#[repr(C, align(64))]
struct Boot {
    mair: u64,
    tcr: u64,
    ttbr0: u64,
    sctlr: u64,
    vbar: u64,
    stack_top: u64,
    index: u64,
    main: u64,
}

core::arch::global_asm!(
    r#"
    .text
    .global ap_entry
ap_entry:                            // x0 = &Boot; MMU and caches off
    mov x19, x0
    mrs x1, CurrentEL
    lsr x1, x1, #2
    cmp x1, #2
    b.ne 1f
    mov x2, #(1 << 31)               // HCR_EL2.RW: EL1 is AArch64, nothing is trapped or routed
    msr hcr_el2, x2
    mrs x2, cnthctl_el2
    orr x2, x2, #3                   // EL1 may use the physical counter and timer
    msr cnthctl_el2, x2
    msr cntvoff_el2, xzr
    mov x2, #0x33ff
    msr cptr_el2, x2
    msr hstr_el2, xzr
    mrs x2, midr_el1
    msr vpidr_el2, x2
    mrs x2, mpidr_el1
    msr vmpidr_el2, x2
    mov x2, #0x3c5                   // EL1h, DAIF masked
    msr spsr_el2, x2
    adr x2, 1f
    msr elr_el2, x2
    isb
    eret
1:  ldp x1, x2, [x19, #0]            // MAIR, TCR
    ldp x3, x4, [x19, #16]           // TTBR0, SCTLR
    ldr x5, [x19, #32]               // VBAR
    msr mair_el1, x1
    msr tcr_el1, x2
    msr ttbr0_el1, x3
    msr vbar_el1, x5
    isb
    tlbi vmalle1
    dsb nsh
    isb
    msr sctlr_el1, x4                // MMU, caches on: this code is identity-mapped
    isb
    ldr x6, [x19, #40]
    mov sp, x6
    ldr x0, [x19, #48]
    ldr x7, [x19, #56]
    br x7
"#
);

unsafe extern "C" {
    fn ap_entry();
}

fn psci(hvc: bool, function: u64, a: u64, b: u64, c: u64) -> i64 {
    let mut result = function;
    unsafe {
        if hvc {
            core::arch::asm!("hvc #0", inout("x0") result, in("x1") a, in("x2") b, in("x3") c, options(nostack));
        } else {
            core::arch::asm!("smc #0", inout("x0") result, in("x1") a, in("x2") b, in("x3") c, options(nostack));
        }
    }
    result as i64
}

extern "C" fn ap_main(index: u64) -> ! {
    crate::arch::activate(index as usize);
    crate::gic::init_secondary();
    crate::usermode::init_cpu();
    percpu::get(index as usize).arch.gic_target.store(crate::gic::cpu_target(), Ordering::Relaxed);
    crate::cpu::interrupts_enable();
    crate::sched::idle_loop()
}

fn sysreg_values() -> (u64, u64, u64, u64, u64) {
    let (mair, tcr, ttbr0, sctlr, vbar): (u64, u64, u64, u64, u64);
    unsafe {
        core::arch::asm!("mrs {}, mair_el1", "mrs {}, tcr_el1", "mrs {}, ttbr0_el1", "mrs {}, sctlr_el1", "mrs {}, vbar_el1",
            out(reg) mair, out(reg) tcr, out(reg) ttbr0, out(reg) sctlr, out(reg) vbar, options(nomem, nostack));
    }
    (mair, tcr, ttbr0, sctlr, vbar)
}

pub struct Report {
    pub started: usize,
    pub failed: usize,
}

/// Starts every other processor MADT lists. Call on the boot CPU after the page tables and the GIC are up.
pub fn start_all(rsdp: u64) -> Report {
    percpu::get(0).arch.gic_target.store(crate::gic::cpu_target(), Ordering::Relaxed);
    crate::gic::register_sgi(crate::context::WAKE_SGI, |_| crate::sched::wake_interrupt());
    let mut report = Report { started: 0, failed: 0 };
    let mut mpidrs = [0u64; percpu::MAX_CPUS];
    let found = crate::acpi::madt_cpus(rsdp, &mut mpidrs);
    let Some(hvc) = crate::acpi::psci_uses_hvc(rsdp) else {
        if found > 1 {
            println!("SMP: the firmware does not claim PSCI: the other {} CPU(s) stay stopped.", found - 1);
        }
        percpu::set_count(1);
        return report;
    };
    let boot = percpu::get(0).arch.mpidr;
    let (mair, tcr, ttbr0, sctlr, vbar) = sysreg_values();
    let mut count = 1;
    for &mpidr in mpidrs[..found].iter().filter(|&&m| m != boot) {
        if count >= percpu::MAX_CPUS {
            break;
        }
        let Some(stack) = crate::pmm::allocate(STACK_FRAMES, None) else {
            println!("SMP: no memory for the stack of CPU {count}: skipped.");
            report.failed += 1;
            continue;
        };
        let record = alloc::boxed::Box::leak(alloc::boxed::Box::new(Boot { mair, tcr, ttbr0, sctlr, vbar, stack_top: stack + STACK_FRAMES * 4096, index: count as u64, main: ap_main as usize as u64 }));
        // The new CPU reads the record with its caches off: push it to memory first.
        let address = record as *const Boot as u64;
        for line in (0..core::mem::size_of::<Boot>() as u64).step_by(64) {
            unsafe { core::arch::asm!("dc civac, {}", in(reg) address + line, options(nostack)) };
        }
        unsafe { core::arch::asm!("dsb sy", options(nostack)) };
        let cpu = percpu::get(count);
        let status = psci(hvc, PSCI_CPU_ON, mpidr, ap_entry as usize as u64, address);
        if status != 0 {
            println!("SMP: PSCI CPU_ON for MPIDR {mpidr:#x} returned {status}.");
            report.failed += 1;
            continue;
        }
        let mut waited = 0;
        while !cpu.online.load(Ordering::Acquire) && waited < 500 {
            crate::timer::busy_wait_ms(1);
            waited += 1;
        }
        if cpu.online.load(Ordering::Acquire) {
            report.started += 1;
            count += 1;
        } else {
            println!("SMP: MPIDR {mpidr:#x} did not start within 500 ms.");
            report.failed += 1;
        }
    }
    percpu::set_count(count);
    report
}
