//! Overflow detection for the single kernel stack.
//!
//! `entry.S` reserves an unmapped page directly below the downward-growing
//! stack. A write that reaches it faults instead of corrupting adjacent state;
//! `gdt.rs` installs a double-fault task with an emergency stack so the CPU
//! can still report the escalation. The small canary immediately above that
//! page remains useful for detecting a few overwritten words before the guard
//! page is reached.

#[cfg(feature = "stack-guard-test")]
use core::arch::asm;

const CANARY: [u32; 4] = [0x4b53_544b, 0x43a5_19d7, 0x6f72_6465, 0x7f3c_91b2];

unsafe extern "C" {
    #[cfg(not(feature = "legacy-unsafe-memory"))]
    static kernel_stack_guard: u8;
    static mut kernel_stack_canary: [u32; 4];
}

/// Returns the page-aligned address paging must leave absent.
#[cfg(not(feature = "legacy-unsafe-memory"))]
pub fn guard_page_address() -> u32 {
    core::ptr::addr_of!(kernel_stack_guard) as u32
}

/// Arms the canary after `entry.S` has cleared `.bss`.
pub fn initialize() {
    unsafe {
        core::ptr::write_volatile(
            core::ptr::addr_of_mut!(kernel_stack_canary),
            CANARY,
        );
    }
}

/// Returns false once downward stack growth has reached the reserved tripwire.
pub fn is_intact() -> bool {
    unsafe { core::ptr::read_volatile(core::ptr::addr_of!(kernel_stack_canary)) == CANARY }
}

/// Deliberately overflows into the guard page for `make test-stack-guard`.
///
/// The feature is never enabled in a normal image. Setting ESP four bytes
/// above the guard makes `push` decrement it into the absent page, forcing
/// #PF while no stack remains for its frame and thereby exercising the
/// double-fault task gate.
#[cfg(feature = "stack-guard-test")]
pub fn trigger_guard_page_for_test() -> ! {
    let guard = guard_page_address() + 4;
    unsafe {
        asm!("mov esp, {guard:e}; push eax", guard = in(reg) guard, options(noreturn));
    }
}
