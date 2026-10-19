//! x86-64: boot entry, GDT/IDT/TSS, local APIC and I/O APIC, ACPI tables, MSI, 4-level paging, COM1, CPU power.

pub mod acpi;
pub mod apic;
pub mod cpu;
pub mod cpupower;
pub mod gdt;
pub mod idt;
pub mod ioapic;
pub mod msi;
pub mod paging;
pub mod pci_config;
pub mod serial;

/// The system timer as the rest of the kernel sees it (`crate::timer`): a tick counter at `TICK_HZ` and a microsecond clock.
/// On x86 it is the local APIC timer; another architecture provides the same five names from its own timer.
pub mod timer {
    pub use super::apic::{busy_wait_ms, micros, ticks, wait_ticks, TICK_HZ};
}

core::arch::global_asm!(
    r#"
    .section .text.entry, "ax"
    .global _start
    _start:
        cli                              # the firmware's interrupt state is not ours
        cld
        lea rsp, [rip + __stack_top]     # our own stack, at the end of .bss
        xor ebp, ebp
        call kmain                       # rdi still holds the BootInfo pointer
    2:  hlt
        jmp 2b
    "#
);
core::arch::global_asm!(include_str!("isr_stubs.S"));
