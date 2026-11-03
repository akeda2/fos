//! Interrupts and exceptions: every vector arrives at `interrupt_dispatch` through the shared stubs (`isr_stubs.S`, which the
//! embedded kernel uses too, as are `gdt.rs` and `idt.rs`). An exception the kernel did not expect is reported in words
//! (what, where, and what the faulting address is) and the CPU stops.

use crate::{apic, cpu, vm};
use core::sync::atomic::{AtomicU64, Ordering};

/// The register state at the moment of an interrupt, as `isr_common` builds it.
#[repr(C)]
pub struct Frame {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rbp: u64,
    pub rbx: u64,
    pub rdx: u64,
    pub rcx: u64,
    pub rax: u64,
    pub vector: u64,
    pub error: u64,
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

pub fn exception_name(vector: u64) -> &'static str {
    match vector {
        0 => "#DE divide error",
        1 => "#DB debug",
        2 => "NMI non-maskable interrupt",
        3 => "#BP breakpoint",
        4 => "#OF overflow",
        5 => "#BR bound range exceeded",
        6 => "#UD invalid opcode",
        7 => "#NM device not available",
        8 => "#DF double fault",
        10 => "#TS invalid TSS",
        11 => "#NP segment not present",
        12 => "#SS stack-segment fault",
        13 => "#GP general protection fault",
        14 => "#PF page fault",
        16 => "#MF x87 floating-point error",
        17 => "#AC alignment check",
        18 => "#MC machine check",
        19 => "#XM SIMD floating-point error",
        20 => "#VE virtualization exception",
        crate::debug::WATCHDOG_VECTOR => "watchdog timeout (the service thread made no progress)",
        _ => "reserved exception",
    }
}

pub static BREAKPOINTS: AtomicU64 = AtomicU64::new(0);
static LEGACY_PIC: AtomicU64 = AtomicU64::new(0);

#[unsafe(no_mangle)]
extern "sysv64" fn interrupt_dispatch(frame: &mut Frame) {
    let from_user = frame.cs & 3 == 3;
    if from_user && frame.vector < 32 {
        crate::process::fault(frame);
    }
    match frame.vector {
        crate::process::SYSCALL_VECTOR => crate::process::syscall(frame),
        3 => {
            BREAKPOINTS.fetch_add(1, Ordering::Relaxed);
            println!("Breakpoint exception at {:#x}: handled, execution continues.", frame.rip);
        }
        v if v == u64::from(apic::TIMER_VECTOR) => {
            if crate::percpu::this().index == 0 {
                crate::debug::watchdog_check(frame);
            }
            apic::tick()
        }
        v if v == u64::from(crate::tlb::TLB_VECTOR) => crate::tlb::handle(),
        v if v == u64::from(crate::sched::WAKE_VECTOR) => crate::sched::wake_interrupt(),
        v if v == u64::from(apic::SPURIOUS_VECTOR) => {} // a spurious interrupt needs no EOI
        v if (u64::from(apic::LEGACY_PIC_BASE)..u64::from(apic::LEGACY_PIC_BASE) + 16).contains(&v) => {
            LEGACY_PIC.fetch_add(1, Ordering::Relaxed); // all lines are masked, so this is a spurious one: no EOI
        }
        v if crate::msi::handles(v) => crate::msi::dispatch(v),
        v if crate::msi::handles_intx(v) => crate::msi::dispatch_intx(v),
        v if v < 32 => fatal(frame),
        v => println!("Unexpected interrupt vector {v}."),
    }
    if from_user {
        crate::process::check_killed();
    }
}

fn fatal(frame: &Frame) -> ! {
    report_and_stop(frame, false)
}

/// The watchdog's path: report, record, then reset whatever `reboot-on-crash` says (a hang is what a watchdog is for).
pub fn fatal_reset(frame: &Frame) -> ! {
    report_and_stop(frame, true)
}

fn report_and_stop(frame: &Frame, always_reset: bool) -> ! {
    crate::serial::break_lock();
    println!();
    println!("*** CPU exception {}: {} (error code {:#x}) ***", frame.vector, exception_name(frame.vector), frame.error);
    println!("At rip {:#x}, cs {:#x}, rflags {:#x}, rsp {:#x}, ss {:#x}.", frame.rip, frame.cs, frame.rflags, frame.rsp, frame.ss);
    if frame.vector == 14 {
        let address = cpu::read_cr2();
        println!("Page fault at {address:#x}: {}.", vm::describe(address));
        println!("The access was a {}, {}{}.", if frame.error & 2 != 0 { "write" } else { "read" }, if frame.error & 1 != 0 { "to a present page" } else { "to an unmapped page" }, if frame.error & 16 != 0 { " (instruction fetch)" } else { "" });
    }
    println!("rax {:#x} rbx {:#x} rcx {:#x} rdx {:#x}", frame.rax, frame.rbx, frame.rcx, frame.rdx);
    println!("rsi {:#x} rdi {:#x} rbp {:#x}", frame.rsi, frame.rdi, frame.rbp);
    println!("r8 {:#x} r9 {:#x} r10 {:#x} r11 {:#x}", frame.r8, frame.r9, frame.r10, frame.r11);
    println!("r12 {:#x} r13 {:#x} r14 {:#x} r15 {:#x}", frame.r12, frame.r13, frame.r14, frame.r15);
    println!("cr3 {:#x}.", cpu::read_cr3());
    cpu::interrupts_disable();
    crate::debug::record_crash(frame);
    if crate::config::reboot_on_crash() || always_reset {
        if crate::debug::crash_loop_tripped() {
            crate::hwwd::stop();
            println!("*** crash loop: {} boots in a row never reached the milestone; not resetting again. Halting for an operator.", crate::debug::boot_crash_count());
        } else {
            println!("*** resetting in {} s", crate::config::reboot_delay());
            crate::hwwd::reload();
            apic::busy_wait_ms(u64::from(crate::config::reboot_delay()) * 1000);
            crate::power::reset();
        }
    }
    crate::hwwd::stop();
    println!("*** the kernel stops here");
    crate::halt()
}
