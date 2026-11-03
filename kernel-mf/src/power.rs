//! Resetting the machine, the ways that need nothing from the firmware (the embedded kernel's order): the ACPI reset
//! register (I/O space only), the PCI reset control port 0xCF9 (a hard reset: CPU and chipset restart), the 8042 keyboard
//! controller's reset line, and a deliberate triple fault. Does not return.

use crate::{apic, cpu};

/// The FADT's reset register as (I/O port, value), if it has one in I/O space.
static mut ACPI_RESET: Option<(u16, u8)> = None;

pub fn set_acpi_reset(register: Option<(u16, u8)>) {
    unsafe { ACPI_RESET = register };
}

pub fn reset() -> ! {
    cpu::interrupts_disable();
    crate::hwwd::stop();
    if let Some((port, value)) = unsafe { ACPI_RESET } {
        cpu::outb(port, value);
        apic::busy_wait_ms(50);
    }
    cpu::outb(0xcf9, 0x02);
    apic::busy_wait_ms(5);
    cpu::outb(0xcf9, 0x06);
    apic::busy_wait_ms(50);
    cpu::outb(0x64, 0xfe);
    apic::busy_wait_ms(50);
    // An empty IDT and a breakpoint: the fault cannot be delivered, then the double fault cannot either: a triple fault.
    let empty: [u8; 10] = [0; 10];
    unsafe { core::arch::asm!("lidt [{}]", "int3", in(reg) empty.as_ptr(), options(noreturn)) }
}
