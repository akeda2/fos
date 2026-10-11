//! Leaving: power the machine off. The portable way is the UEFI runtime service
//! `ResetSystem`, which survives `ExitBootServices` by design; the kernel keeps the
//! runtime-services regions mapped for exactly this.

use crate::cpu;

static mut RUNTIME_SERVICES: u64 = 0;

/// Remembers the runtime-services table so a fault handler can reset the machine.
pub fn set_runtime_services(table: u64) {
    unsafe { RUNTIME_SERVICES = table };
}

pub fn runtime_services() -> u64 {
    unsafe { RUNTIME_SERVICES }
}

static mut ACPI_RESET: Option<(u8, u64, u8)> = None;
static mut RESET_ATTEMPTED: u8 = 0;

/// Remembers the FADT's reset register for `reset`.
pub fn set_acpi_reset(register: Option<(u8, u64, u8)>) {
    unsafe { ACPI_RESET = register };
}

/// Resets the machine, trying in order the ways that need nothing from the firmware: the ACPI
/// reset register (if the FADT has one), the PCI reset control port 0xCF9 (a hard reset: the
/// CPU and chipset restart, DRAM keeps its contents, so the crash record survives), the 8042
/// keyboard controller's reset line, and only then the UEFI `ResetSystem(EfiResetWarm)`.
/// The firmware call goes last because on the Skylake it page-faulted inside the firmware (a
/// runtime region the kernel does not map?), and a fault while handling a fault is no way to
/// leave. Does not return.
pub fn reset(runtime_services: u64) -> ! {
    cpu::interrupts_disable();
    crate::serial::flush();
    unsafe {
        // Each method gets one try; a method that did not reset the machine falls through to the next.
        if RESET_ATTEMPTED == 0 {
            RESET_ATTEMPTED = 1;
            if let Some((space, address, value)) = ACPI_RESET {
                match space {
                    1 => cpu::outb(address as u16, value),
                    0 if crate::paging::is_mapped(address) => (address as *mut u8).write_volatile(value),
                    _ => {}
                }
                crate::apic::busy_wait_ms(50);
            }
        }
        if RESET_ATTEMPTED == 1 {
            RESET_ATTEMPTED = 2;
            cpu::outb(0xcf9, 0x02);
            crate::apic::busy_wait_ms(5);
            cpu::outb(0xcf9, 0x06);
            crate::apic::busy_wait_ms(50);
        }
        if RESET_ATTEMPTED == 2 {
            RESET_ATTEMPTED = 3;
            cpu::outb(0x64, 0xfe);
            crate::apic::busy_wait_ms(50);
        }
        if RESET_ATTEMPTED == 3 && runtime_services != 0 {
            RESET_ATTEMPTED = 4;
            let reset_system = ((runtime_services + 24 + 10 * 8) as *const u64).read_unaligned();
            if reset_system != 0 {
                let call: extern "efiapi" fn(kind: u32, status: usize, size: usize, data: *const u8) -> ! = core::mem::transmute(reset_system as usize);
                call(1, 0, 0, core::ptr::null())
            }
        }
    }
    cpu::halt()
}

static mut ACPI_S5: Option<(u16, u16, u8, u8)> = None;

/// Remembers the PM1 control ports and the S5 sleep types for `shutdown` (None if the DSDT had no `_S5_`).
pub fn set_acpi_s5(pm1a: u16, pm1b: u16, s5: Option<(u8, u8)>) {
    unsafe { ACPI_S5 = if pm1a != 0 { s5.map(|(a, b)| (pm1a, pm1b, a, b)) } else { None } };
}

/// Powers the machine off: ACPI soft-off first (SLP_TYP for S5 plus SLP_EN written to PM1a/PM1b
/// control, what the firmware itself does underneath), then the UEFI `ResetSystem(EfiResetShutdown)`.
/// The firmware call goes second because on the Skylake it dereferenced a null pointer inside the
/// firmware's runtime code. Returns only if neither worked (then the caller halts).
pub fn shutdown(runtime_services: u64) {
    cpu::interrupts_disable();
    if let Some((pm1a, pm1b, type_a, type_b)) = unsafe { ACPI_S5 } {
        const SLP_EN: u16 = 1 << 13;
        cpu::outw(pm1a, (u16::from(type_a) << 10) | SLP_EN);
        if pm1b != 0 {
            cpu::outw(pm1b, (u16::from(type_b) << 10) | SLP_EN);
        }
        crate::apic::busy_wait_ms(500);
    }
    if runtime_services == 0 {
        return;
    }
    // EFI_RUNTIME_SERVICES: a 24-byte table header, then ten function pointers
    // (GetTime ... GetNextHighMonotonicCount), then ResetSystem.
    let reset_system = unsafe { ((runtime_services + 24 + 10 * 8) as *const u64).read_unaligned() };
    if reset_system == 0 {
        return;
    }
    let reset: extern "efiapi" fn(kind: u32, status: usize, size: usize, data: *const u8) -> ! =
        unsafe { core::mem::transmute(reset_system as usize) };
    reset(2, 0, 0, core::ptr::null())
}
