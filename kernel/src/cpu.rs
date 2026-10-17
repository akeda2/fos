//! Thin wrappers over the instructions the kernel needs everywhere: port I/O, model
//! specific registers, control registers and CPUID.

pub fn outb(port: u16, value: u8) {
    unsafe { core::arch::asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack)) };
}

/// A write to the unused "POST code" port 0x80: about a microsecond on the ISA bus, which is what the 8259 needs between
/// the bytes of its initialisation sequence (some chipsets' PICs lose a byte sent back to back).
pub fn io_wait() {
    outb(0x80, 0);
}

pub fn outw(port: u16, value: u16) {
    unsafe { core::arch::asm!("out dx, ax", in("dx") port, in("ax") value, options(nomem, nostack)) };
}

pub fn inb(port: u16) -> u8 {
    let value: u8;
    unsafe { core::arch::asm!("in al, dx", in("dx") port, out("al") value, options(nomem, nostack)) };
    value
}

pub fn outl(port: u16, value: u32) {
    unsafe { core::arch::asm!("out dx, eax", in("dx") port, in("eax") value, options(nomem, nostack)) };
}

pub fn inw(port: u16) -> u16 {
    let value: u16;
    unsafe { core::arch::asm!("in ax, dx", out("ax") value, in("dx") port, options(nomem, nostack, preserves_flags)) };
    value
}

pub fn inl(port: u16) -> u32 {
    let value: u32;
    unsafe { core::arch::asm!("in eax, dx", in("dx") port, out("eax") value, options(nomem, nostack)) };
    value
}

pub fn read_msr(msr: u32) -> u64 {
    let (low, high): (u32, u32);
    unsafe { core::arch::asm!("rdmsr", in("ecx") msr, out("eax") low, out("edx") high, options(nomem, nostack, preserves_flags)) };
    u64::from(high) << 32 | u64::from(low)
}

pub fn write_msr(msr: u32, value: u64) {
    unsafe {
        core::arch::asm!("wrmsr", in("ecx") msr, in("eax") value as u32, in("edx") (value >> 32) as u32, options(nomem, nostack, preserves_flags))
    };
}

pub fn read_cr0() -> u64 {
    let value: u64;
    unsafe { core::arch::asm!("mov {}, cr0", out(reg) value, options(nomem, nostack, preserves_flags)) };
    value
}

pub fn write_cr0(value: u64) {
    unsafe { core::arch::asm!("mov cr0, {}", in(reg) value, options(nomem, nostack, preserves_flags)) };
}

pub fn read_cr2() -> u64 {
    let value: u64;
    unsafe { core::arch::asm!("mov {}, cr2", out(reg) value, options(nomem, nostack, preserves_flags)) };
    value
}

pub fn read_cr3() -> u64 {
    let value: u64;
    unsafe { core::arch::asm!("mov {}, cr3", out(reg) value, options(nomem, nostack, preserves_flags)) };
    value
}

pub fn write_cr3(value: u64) {
    unsafe { core::arch::asm!("mov cr3, {}", in(reg) value, options(nostack, preserves_flags)) };
}

pub fn read_cr4() -> u64 {
    let value: u64;
    unsafe { core::arch::asm!("mov {}, cr4", out(reg) value, options(nomem, nostack, preserves_flags)) };
    value
}

fn write_cr4(value: u64) {
    unsafe { core::arch::asm!("mov cr4, {}", in(reg) value, options(nostack, preserves_flags)) };
}

fn xgetbv0() -> u64 {
    let (low, high): (u32, u32);
    unsafe { core::arch::asm!("xgetbv", in("ecx") 0, out("eax") low, out("edx") high, options(nomem, nostack, preserves_flags)) };
    u64::from(high) << 32 | u64::from(low)
}

fn xsetbv0(value: u64) {
    unsafe { core::arch::asm!("xsetbv", in("ecx") 0, in("eax") value as u32, in("edx") (value >> 32) as u32, options(nomem, nostack, preserves_flags)) };
}

const CR0_MP: u64 = 1 << 1;
const CR0_EM: u64 = 1 << 2;
const CR0_TS: u64 = 1 << 3;
const CR4_OSFXSR: u64 = 1 << 9;
const CR4_OSXMMEXCPT: u64 = 1 << 10;
const CR4_OSXSAVE: u64 = 1 << 18;
const XCR0_X87_SSE_AVX: u64 = 0b111;

/// What `fp_enable` changed, so `fp_disable` can put it back (CR0, CR4, XCR0), and the level it reached.
static mut FP_SAVED: Option<(u64, u64, u64)> = None;

/// Switches on the floating point a hard-float tape (target `x86_64-fos-tape-fp`) needs and returns the level
/// (`fosfor_abi::tape::FP_*`). The kernel itself is built without SSE and never touches XMM/YMM state, so nothing here
/// has to be saved or restored around an interrupt: the state belongs to the tape that runs, one at a time.
///
/// Why each bit: CR4.OSFXSR lets SSE instructions run and CR4.OSXMMEXCPT routes an SIMD fault to #XM (they are masked
/// anyway); without them every SSE instruction is #UD. CR0.EM must be clear (it would make them #NM), CR0.MP set and
/// CR0.TS clear (no lazy-switch trap). AVX needs more: CR4.OSXSAVE, then XCR0 = x87|SSE|AVX through `xsetbv`; the CPU
/// only runs VEX instructions when both say so. AVX2 and FMA come from CPUID, and the level is only `FP_AVX2` when
/// `xgetbv` reads the YMM bit back. `fninit` and MXCSR 0x1f80 (all exceptions masked, round to nearest) give the tape the
/// state the ABI promises. Called before every tape starts, so the tape's first instruction may already use an XMM register.
pub fn fp_enable() -> u32 {
    use fosfor_abi::tape::{FP_AVX2, FP_NONE, FP_SSE2};
    let (_, _, ecx, edx) = cpuid(1, 0);
    // FXSR (bit 24) and SSE2 (bit 26) are in every x86-64 CPU; a hypervisor that hides them gets no floating point.
    if edx & (1 << 24) == 0 || edx & (1 << 26) == 0 {
        return FP_NONE;
    }
    fp_disable(); // a second call must not record the already-enabled state as the original
    let (cr0, cr4) = (read_cr0(), read_cr4());
    let mut saved_xcr0 = 0;
    let mut level = FP_SSE2;
    let mut new_cr4 = cr4 | CR4_OSFXSR | CR4_OSXMMEXCPT;
    let xsave_and_avx = ecx & (1 << 26) != 0 && ecx & (1 << 28) != 0;
    if xsave_and_avx {
        new_cr4 |= CR4_OSXSAVE;
    }
    write_cr0((cr0 & !(CR0_EM | CR0_TS)) | CR0_MP);
    write_cr4(new_cr4);
    if xsave_and_avx {
        saved_xcr0 = xgetbv0();
        xsetbv0(saved_xcr0 | XCR0_X87_SSE_AVX);
        let (max_leaf, ..) = cpuid(0, 0);
        let avx2 = max_leaf >= 7 && cpuid(7, 0).1 & (1 << 5) != 0;
        let fma = ecx & (1 << 12) != 0;
        if avx2 && fma && xgetbv0() & XCR0_X87_SSE_AVX == XCR0_X87_SSE_AVX {
            level = FP_AVX2;
        }
    }
    unsafe {
        FP_SAVED = Some((cr0, cr4, saved_xcr0));
        core::arch::asm!("fninit", options(nomem, nostack));
        let mxcsr: u32 = 0x1f80;
        core::arch::asm!("ldmxcsr [{}]", in(reg) &mxcsr, options(readonly, nostack, preserves_flags));
    }
    level
}

/// Undoes `fp_enable` (a tape that ended): XCR0 first (it needs CR4.OSXSAVE), then CR4 and CR0 as they were.
pub fn fp_disable() {
    let Some((cr0, cr4, xcr0)) = (unsafe { (*(&raw mut FP_SAVED)).take() }) else { return };
    if read_cr4() & CR4_OSXSAVE != 0 {
        xsetbv0(xcr0);
    }
    write_cr4(cr4);
    write_cr0(cr0);
}

pub fn cpuid(leaf: u32, subleaf: u32) -> (u32, u32, u32, u32) {
    let result = core::arch::x86_64::__cpuid_count(leaf, subleaf);
    (result.eax, result.ebx, result.ecx, result.edx)
}

pub fn interrupts_enable() {
    unsafe { core::arch::asm!("sti", options(nomem, nostack)) };
}

pub fn interrupts_disable() {
    unsafe { core::arch::asm!("cli", options(nomem, nostack)) };
}

use core::sync::atomic::{AtomicU64, Ordering};

/// Why the CPU stayed awake while it had nothing to do: the counters behind the `power` command.
#[derive(Clone, Copy)]
pub enum Spin {
    /// A polled NIC (no interrupt) is watched in 100 us slices.
    PolledNic,
    /// A UART (16-byte FIFO, no interrupt) is polled in 100 us slices.
    Uart,
    /// Queued serial output is being pumped.
    SerialOut,
}

pub const SPIN_REASONS: usize = 3;
static HALT_US: AtomicU64 = AtomicU64::new(0);
static WAKEUPS: AtomicU64 = AtomicU64::new(0);
static SPIN_US: [AtomicU64; SPIN_REASONS] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];

static VECTORS: [AtomicU64; 256] = [const { AtomicU64::new(0) }; 256];

/// Counts one interrupt or exception on `vector` (called first thing from `interrupt_dispatch`).
pub fn count_vector(vector: u64) {
    VECTORS[(vector & 0xff) as usize].fetch_add(1, Ordering::Relaxed);
}

pub fn vector_count(vector: usize) -> u64 {
    VECTORS[vector & 0xff].load(Ordering::Relaxed)
}

/// Idle accounting since boot: (microseconds in `hlt`, wake-ups from it, microseconds spun per `Spin` reason).
pub fn idle_stats() -> (u64, u64, [u64; SPIN_REASONS]) {
    (HALT_US.load(Ordering::Relaxed), WAKEUPS.load(Ordering::Relaxed), [0, 1, 2].map(|index| SPIN_US[index].load(Ordering::Relaxed)))
}

/// `hlt` until the next interrupt (a tick is at most 10 ms away), counting the time asleep. Every idle wait goes
/// through here so `power` can tell a machine that sleeps from one that polls. Interrupts must be enabled.
pub fn sleep() {
    let start = crate::apic::micros();
    unsafe { core::arch::asm!("hlt", options(nomem, nostack)) };
    HALT_US.fetch_add(crate::apic::micros().saturating_sub(start), Ordering::Relaxed);
    WAKEUPS.fetch_add(1, Ordering::Relaxed);
}

/// Counts time spent awake-but-idle for `reason`; wrap a polling wait with the `start` it took.
pub fn spun(reason: Spin, start_micros: u64) {
    SPIN_US[reason as usize].fetch_add(crate::apic::micros().saturating_sub(start_micros), Ordering::Relaxed);
}

pub fn halt() -> ! {
    crate::serial::flush();
    loop {
        unsafe { core::arch::asm!("hlt") };
    }
}

pub const MSR_APIC_BASE: u32 = 0x1b;
pub const MSR_PAT: u32 = 0x277;
pub const MSR_EFER: u32 = 0xc000_0080;

/// Disables interrupts and returns whether they were enabled, for a short critical section that the
/// caller ends with `interrupts_restore(was)` (so nested sections and sections entered with
/// interrupts already off both behave).
pub fn interrupts_save_and_disable() -> bool {
    let flags: u64;
    unsafe { core::arch::asm!("pushfq", "pop {}", "cli", out(reg) flags, options(nomem, preserves_flags)) };
    flags & (1 << 9) != 0
}

pub fn interrupts_restore(was_enabled: bool) {
    if was_enabled {
        unsafe { core::arch::asm!("sti", options(nomem, nostack)) };
    }
}
