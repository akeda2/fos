//! Floating point, SSE and AVX state for processes (slice 8c).
//!
//! The kernel is built without SSE and never touches XMM/YMM registers, so entering the kernel from ring 3 (interrupt or
//! `syscall`) does not disturb a process's state: it stays in the registers until the scheduler switches to another thread.
//! So the only place that saves and restores is the context switch: `save` into the outgoing user thread's area, `restore`
//! from the incoming one's (kernel threads have no area and leave the registers alone; the next user thread loads all of its own).
//!
//! Per CPU `init_cpu` sets CR4.OSFXSR/OSXMMEXCPT (else every SSE instruction is #UD), clears CR0.EM and TS and sets MP, and,
//! when the CPU has XSAVE, CR4.OSXSAVE and XCR0 = x87|SSE|AVX (AVX only where the CPU has it). With XSAVE a thread's area is as
//! large as CPUID 0xD says for that XCR0 (64-byte aligned); without it, the 512-byte FXSAVE image. A new area holds the state
//! the tape ABI promises: x87 control word 0x37f and MXCSR 0x1f80 (all exceptions masked, round to nearest), registers zero.

use alloc::alloc::{Layout, alloc_zeroed, dealloc};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};

use crate::cpu;

const CR0_MP: u64 = 1 << 1;
const CR0_EM: u64 = 1 << 2;
const CR0_TS: u64 = 1 << 3;
const CR4_OSFXSR: u64 = 1 << 9;
const CR4_OSXMMEXCPT: u64 = 1 << 10;
const CR4_OSXSAVE: u64 = 1 << 18;

static USE_XSAVE: AtomicBool = AtomicBool::new(false);
/// XCR0 as set on the boot CPU: the mask `xsave` and `xrstor` use.
static MASK: AtomicU64 = AtomicU64::new(0);
static AREA_BYTES: AtomicU32 = AtomicU32::new(0);
static LEVEL: AtomicU32 = AtomicU32::new(fosfor_abi::tape::FP_NONE);

/// Enables SSE (and AVX where present) on this CPU. The boot CPU, which runs first, also decides the save format and size.
pub fn init_cpu() {
    use fosfor_abi::tape::{FP_AVX2, FP_NONE, FP_SSE2};
    let boot = AREA_BYTES.load(Relaxed) == 0;
    let (_, _, ecx, edx) = cpu::cpuid(1, 0);
    if edx & (1 << 24) == 0 || edx & (1 << 26) == 0 {
        if boot {
            LEVEL.store(FP_NONE, Relaxed);
            AREA_BYTES.store(1, Relaxed);
        }
        return;
    }
    let xsave = ecx & (1 << 26) != 0;
    let avx = xsave && ecx & (1 << 28) != 0;
    unsafe {
        let (mut cr0, mut cr4): (u64, u64);
        core::arch::asm!("mov {}, cr0", out(reg) cr0, options(nomem, nostack, preserves_flags));
        core::arch::asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack, preserves_flags));
        cr0 = (cr0 & !(CR0_EM | CR0_TS)) | CR0_MP;
        cr4 |= CR4_OSFXSR | CR4_OSXMMEXCPT | if xsave { CR4_OSXSAVE } else { 0 };
        core::arch::asm!("mov cr0, {}", in(reg) cr0, options(nomem, nostack, preserves_flags));
        core::arch::asm!("mov cr4, {}", in(reg) cr4, options(nomem, nostack, preserves_flags));
        if xsave {
            let wanted: u64 = if avx { 0b111 } else { 0b011 };
            core::arch::asm!("xsetbv", in("ecx") 0, in("eax") wanted as u32, in("edx") 0u32, options(nomem, nostack, preserves_flags));
        }
    }
    if !boot {
        return;
    }
    let mut level = FP_SSE2;
    let mut bytes = 512;
    if xsave {
        let (low, high): (u32, u32);
        unsafe { core::arch::asm!("xgetbv", in("ecx") 0, out("eax") low, out("edx") high, options(nomem, nostack, preserves_flags)) };
        MASK.store(u64::from(high) << 32 | u64::from(low), Relaxed);
        bytes = cpu::cpuid(0xd, 0).1.max(576);
        USE_XSAVE.store(true, Relaxed);
        let avx2 = cpu::cpuid(7, 0).1 & (1 << 5) != 0 && ecx & (1 << 12) != 0;
        if avx2 && MASK.load(Relaxed) & 0b111 == 0b111 {
            level = FP_AVX2;
        }
    }
    AREA_BYTES.store(bytes, Relaxed);
    LEVEL.store(level, Relaxed);
}

/// What the CPU offers a process (`fosfor_abi::tape::FP_*`).
pub fn level() -> u32 {
    LEVEL.load(Relaxed)
}

/// One thread's saved state.
pub struct Area {
    pointer: *mut u8,
    layout: Layout,
}

// The area is only touched by the CPU that is switching the owning thread in or out.
unsafe impl Send for Area {}

impl Area {
    pub fn new() -> Option<Area> {
        if level() == fosfor_abi::tape::FP_NONE {
            return None;
        }
        let layout = Layout::from_size_align(AREA_BYTES.load(Relaxed) as usize, 64).ok()?;
        let pointer = unsafe { alloc_zeroed(layout) };
        if pointer.is_null() {
            return None;
        }
        unsafe {
            (pointer as *mut u16).write(0x037f); // FCW
            (pointer.add(24) as *mut u32).write(0x1f80); // MXCSR
            if USE_XSAVE.load(Relaxed) {
                (pointer.add(512) as *mut u64).write(0b11); // XSTATE_BV: x87 and SSE are loaded from the legacy region
            }
        }
        Some(Area { pointer, layout })
    }

    /// Stores the registers into the area. Interrupts must be off.
    pub fn save(&self) {
        unsafe {
            if USE_XSAVE.load(Relaxed) {
                let mask = MASK.load(Relaxed);
                core::arch::asm!("xsave64 [{}]", in(reg) self.pointer, in("eax") mask as u32, in("edx") (mask >> 32) as u32, options(nostack, preserves_flags));
            } else {
                core::arch::asm!("fxsave64 [{}]", in(reg) self.pointer, options(nostack, preserves_flags));
            }
        }
    }

    /// Loads the registers from the area. Interrupts must be off.
    pub fn restore(&self) {
        unsafe {
            if USE_XSAVE.load(Relaxed) {
                let mask = MASK.load(Relaxed);
                core::arch::asm!("xrstor64 [{}]", in(reg) self.pointer, in("eax") mask as u32, in("edx") (mask >> 32) as u32, options(nostack, preserves_flags));
            } else {
                core::arch::asm!("fxrstor64 [{}]", in(reg) self.pointer, options(nostack, preserves_flags));
            }
        }
    }
}

impl Drop for Area {
    fn drop(&mut self) {
        unsafe { dealloc(self.pointer, self.layout) };
    }
}
