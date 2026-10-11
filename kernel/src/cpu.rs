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
