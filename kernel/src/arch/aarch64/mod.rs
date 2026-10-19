//! aarch64 (Raspberry Pi 4 and later, QEMU `virt`): boot entry, exception vectors, the generic timer and the PL011
//! console. Grows with the port (docs/aarch64-plan.md): MMU and GIC come next.

pub mod cpu;
pub mod exceptions;
pub mod gic;
pub mod paging;
pub mod pci_config;
pub mod pl011;
pub mod timer;

// Entered from the UEFI loader like a C function: x0 = the BootInfo pointer, MMU and caches on with the firmware's
// identity map, interrupts as the firmware left them (masked here: the kernel has no vectors yet). The loader zeroed
// the BSS and applied the relocations; the image is position independent, so adr/adrp reach everything.
core::arch::global_asm!(
    r#"
    .section .text.entry, "ax"
    .global _start
    _start:
        msr daifset, #0xf              // debug, SError, IRQ, FIQ masked
        adr x1, __stack_top
        mov sp, x1
        mov x29, xzr
        bl kmain                       // x0 still holds the BootInfo pointer
    1:  wfe
        b 1b
    "#
);
