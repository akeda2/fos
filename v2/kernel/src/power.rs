//! Leaving: power the machine off. The portable way is the UEFI runtime service
//! `ResetSystem`, which survives `ExitBootServices` by design; the kernel keeps the
//! runtime-services regions mapped for exactly this.

use crate::cpu;

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
