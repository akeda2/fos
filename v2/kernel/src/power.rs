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

/// Resets the machine: the UEFI `ResetSystem(EfiResetWarm)` if available, else the 8042
/// keyboard controller's CPU-reset line. Does not return.
pub fn reset(runtime_services: u64) -> ! {
    cpu::interrupts_disable();
    if runtime_services != 0 {
        let reset_system = unsafe { ((runtime_services + 24 + 10 * 8) as *const u64).read_unaligned() };
        if reset_system != 0 {
            let call: extern "efiapi" fn(kind: u32, status: usize, size: usize, data: *const u8) -> ! =
                unsafe { core::mem::transmute(reset_system as usize) };
            // EfiResetWarm (1): on a PC the firmware usually implements it as a CPU reset
            // that skips memory training, which is what lets the crash record in RAM
            // survive. A firmware without warm reset treats it as a cold one.
            call(1, 0, 0, core::ptr::null())
        }
    }
    cpu::outb(0x64, 0xfe);
    cpu::halt()
}

/// Calls `ResetSystem(EfiResetShutdown)` through the runtime-services table. Returns
/// only if the firmware does (then the caller halts).
pub fn shutdown(runtime_services: u64) {
    if runtime_services == 0 {
        return;
    }
    // EFI_RUNTIME_SERVICES: a 24-byte table header, then ten function pointers
    // (GetTime ... GetNextHighMonotonicCount), then ResetSystem.
    let reset_system = unsafe { ((runtime_services + 24 + 10 * 8) as *const u64).read_unaligned() };
    if reset_system == 0 {
        return;
    }
    cpu::interrupts_disable();
    let reset: extern "efiapi" fn(kind: u32, status: usize, size: usize, data: *const u8) -> ! =
        unsafe { core::mem::transmute(reset_system as usize) };
    reset(2, 0, 0, core::ptr::null())
}
