//! Resetting the machine, the ways that need nothing from the firmware (the embedded kernel's order): the ACPI reset
//! register (I/O space only), the PCI reset control port 0xCF9 (a hard reset: CPU and chipset restart), the 8042 keyboard
//! controller's reset line, and a deliberate triple fault. Does not return.

use crate::{apic, cpu};

/// The FADT's reset register as (I/O port, value), if it has one in I/O space.
static mut ACPI_RESET: Option<(u16, u8)> = None;

pub fn set_acpi_reset(register: Option<(u16, u8)>) {
    unsafe { ACPI_RESET = register };
}

/// PM1a/PM1b control ports and the S5 sleep types, for `shutdown`.
static mut ACPI_S5: Option<(u16, u16, u8, u8)> = None;

pub fn set_acpi_s5(pm1a: u16, pm1b: u16, s5: Option<(u8, u8)>) {
    unsafe { ACPI_S5 = if pm1a != 0 { s5.map(|(a, b)| (pm1a, pm1b, a, b)) } else { None } };
}

/// ACPI soft-off: SLP_TYP for S5 plus SLP_EN written to PM1a (and PM1b) control, what the firmware does underneath.
/// Returns only if the machine has no `_S5_` or did not power off (the caller says so and stays up).
pub fn shutdown() {
    let Some((pm1a, pm1b, type_a, type_b)) = (unsafe { ACPI_S5 }) else { return };
    cpu::interrupts_disable();
    crate::hwwd::stop();
    const SLP_EN: u16 = 1 << 13;
    cpu::outw(pm1a, (u16::from(type_a) << 10) | SLP_EN);
    if pm1b != 0 {
        cpu::outw(pm1b, (u16::from(type_b) << 10) | SLP_EN);
    }
    apic::busy_wait_ms(500);
    cpu::interrupts_enable();
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
