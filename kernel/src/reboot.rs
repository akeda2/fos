//! A "soft reboot": warm-restart the machine by dropping back into
//! `boot/stage2.S`'s own boot sequence instead of asking the 8042
//! keyboard controller to pulse the CPU's hardware RESET line (see
//! [`crate::arch::x86::reset`]). This skips BIOS POST -- the CPU never
//! actually resets -- but otherwise repeats an ordinary boot exactly:
//! `boot/stage2.S`'s `_start` re-collects the E820 memory map,
//! re-verifies A20, and re-runs `select_kernel_slot`'s full A/B
//! fallback/confirm state machine before reloading the kernel fresh from
//! whichever slot it currently selects. A kernel update installed by KUP
//! (see `kernel/src/debug.rs`'s `install_kernel_update`) is tried exactly
//! the same way after a soft reboot as after a hardware one.
//!
//! `reboot-on-crash` (`debug::handle_crash`) deliberately does **not** use
//! this path, and never should: its entire point is recovering
//! unattended no matter how corrupted the running kernel's state is after
//! a fatal exception, including the low-memory stage-two code and
//! `BootInfo` this mechanism depends on staying intact. A hardware reset
//! makes no assumptions about surviving state at all; this one makes
//! several (stage two's code at physical `0x8000` is unmodified, `BootInfo`
//! is still valid, the CPU can still safely drop to real mode), so it is
//! only appropriate for a deliberate, operator- or KUP-requested restart
//! of an otherwise healthy, still-running kernel.

use crate::boot_info::{read_u32, BootInfo};
use crate::interrupts;

/// Warm-restarts the machine through `boot/stage2.S`'s `soft_reboot_entry`
/// callback (`BootInfo::soft_reboot`). Falls back to a hardware reset if
/// this image's `BootInfo` predates the callback or never populated it
/// (zero address) -- the same "cleanly unavailable, not a fault" fallback
/// convention `disk.rs` already uses for a zero `bios_disk_io` pointer.
pub fn soft_reboot(info: &BootInfo) -> ! {
    let address = read_u32(core::ptr::addr_of!(info.soft_reboot));
    if address == 0 {
        crate::println!(
            "No soft-reboot trampoline in this BootInfo; falling back to a hardware reset."
        );
        crate::arch::x86::reset();
    }
    let routine: unsafe extern "C" fn() -> ! = unsafe { core::mem::transmute(address as usize) };
    // Mirrors `disk::call_trampoline`'s own real-mode drop discipline: hold
    // interrupts off, and restore the real-mode IVT and BIOS-conventional
    // PIC vector mapping, *before* dropping to real mode. `load_kernel`'s
    // BIOS disk reads inside stage two's `_start` need working BIOS
    // timer-tick delivery for its own disk-timeout logic exactly as much
    // as an ordinary post-boot `bios_disk_io` call does, and the currently
    // running kernel's IDT/PIC state is otherwise still loaded.
    unsafe {
        core::arch::asm!("cli", options(nomem, nostack));
    }
    interrupts::use_real_mode_idt();
    interrupts::use_bios_pic_mapping();
    unsafe { routine() }
}
