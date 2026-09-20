//! A low-cost overflow tripwire for the single kernel stack.
//!
//! A true unmapped guard page needs a dedicated double-fault stack: when an
//! overflowing `push` first touches an absent page, the CPU must still push a
//! page-fault frame somewhere safe. This small kernel does not have task-state
//! switching yet, so it uses a canary at the stack's low end instead. IRQ0
//! checks it at 100 Hz and turns a detected overflow into a diagnostic halt
//! rather than continuing with silently corrupted kernel state.

const CANARY: [u32; 4] = [0x4b53_544b, 0x43a5_19d7, 0x6f72_6465, 0x7f3c_91b2];

unsafe extern "C" {
    static mut kernel_stack_canary: [u32; 4];
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
