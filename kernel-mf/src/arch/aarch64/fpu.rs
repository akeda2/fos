//! FP/SIMD state of user threads. The kernel is built soft-float and never touches the V registers, so an exception from EL0 does
//! not disturb a process's state: it stays in the registers until the scheduler switches to another thread. Only the context
//! switch saves (`save`, into the outgoing user thread's area) and restores (`restore`, from the incoming one's). A new area
//! holds what the tape ABI promises: FPCR 0 (round to nearest, no trapping exceptions), registers zero.

use alloc::boxed::Box;

/// Lets EL0 and EL1 use FP/SIMD on this CPU (CPACR_EL1.FPEN = 3); without it every V-register access traps.
pub fn init_cpu() {
    crate::cpu::fp_enable();
}

/// What the CPU offers a process: NEON is part of every Cortex-A72.
pub fn level() -> u32 {
    fosfor_abi::tape::FP_NEON
}

#[repr(C, align(16))]
struct Registers {
    q: [u128; 32],
    fpcr: u64,
    fpsr: u64,
}

pub struct Area(Box<Registers>);

core::arch::global_asm!(
    r#"
    .arch_extension fp
    .arch_extension simd
    .text
    .global fpu_save, fpu_restore
fpu_save:
    stp q0, q1, [x0, #0]
    stp q2, q3, [x0, #32]
    stp q4, q5, [x0, #64]
    stp q6, q7, [x0, #96]
    stp q8, q9, [x0, #128]
    stp q10, q11, [x0, #160]
    stp q12, q13, [x0, #192]
    stp q14, q15, [x0, #224]
    stp q16, q17, [x0, #256]
    stp q18, q19, [x0, #288]
    stp q20, q21, [x0, #320]
    stp q22, q23, [x0, #352]
    stp q24, q25, [x0, #384]
    stp q26, q27, [x0, #416]
    stp q28, q29, [x0, #448]
    stp q30, q31, [x0, #480]
    mrs x1, fpcr
    mrs x2, fpsr
    add x3, x0, #512
    stp x1, x2, [x3]
    ret
fpu_restore:
    add x3, x0, #512
    ldp x1, x2, [x3]
    msr fpcr, x1
    msr fpsr, x2
    ldp q0, q1, [x0, #0]
    ldp q2, q3, [x0, #32]
    ldp q4, q5, [x0, #64]
    ldp q6, q7, [x0, #96]
    ldp q8, q9, [x0, #128]
    ldp q10, q11, [x0, #160]
    ldp q12, q13, [x0, #192]
    ldp q14, q15, [x0, #224]
    ldp q16, q17, [x0, #256]
    ldp q18, q19, [x0, #288]
    ldp q20, q21, [x0, #320]
    ldp q22, q23, [x0, #352]
    ldp q24, q25, [x0, #384]
    ldp q26, q27, [x0, #416]
    ldp q28, q29, [x0, #448]
    ldp q30, q31, [x0, #480]
    ret
"#
);

unsafe extern "C" {
    fn fpu_save(area: *mut Registers);
    fn fpu_restore(area: *const Registers);
}

impl Area {
    pub fn new() -> Option<Area> {
        Some(Area(Box::new(Registers { q: [0; 32], fpcr: 0, fpsr: 0 })))
    }

    /// Stores the registers into the area. Interrupts must be off.
    pub fn save(&self) {
        unsafe { fpu_save(&*self.0 as *const Registers as *mut Registers) };
    }

    /// Loads the registers from the area. Interrupts must be off.
    pub fn restore(&self) {
        unsafe { fpu_restore(&*self.0) };
    }
}
