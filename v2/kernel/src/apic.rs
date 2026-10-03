//! The local APIC and its timer: the kernel's clock tick.
//!
//! The legacy PIT is not guaranteed on UEFI machines, so the tick comes from the
//! local APIC timer, calibrated once against the ACPI power-management timer, a
//! 3.579545 MHz counter that runs the same on every chipset. The local APIC is
//! reached through memory-mapped registers (xAPIC mode) or MSRs (x2APIC mode, which
//! some firmware switches on before handing over; both are handled).

use crate::{acpi, cpu, paging};
use core::sync::atomic::{AtomicU64, Ordering};

const REG_EOI: u32 = 0xb0;
const REG_SPURIOUS: u32 = 0xf0;
const REG_LVT_TIMER: u32 = 0x320;
const REG_TIMER_INITIAL: u32 = 0x380;
const REG_TIMER_CURRENT: u32 = 0x390;
const REG_TIMER_DIVIDE: u32 = 0x3e0;

pub const TIMER_VECTOR: u8 = 0x40;
pub const SPURIOUS_VECTOR: u8 = 0xff;
/// The kernel's tick rate.
pub const TICK_HZ: u64 = 100;

const APIC_BASE_ENABLE: u64 = 1 << 11;
const APIC_BASE_X2APIC: u64 = 1 << 10;
/// The ACPI PM timer's frequency in Hz.
const PM_TIMER_HZ: u64 = 3_579_545;

static TICKS: AtomicU64 = AtomicU64::new(0);
static mut BASE: u64 = 0;
static mut X2APIC: bool = false;
static mut PER_TICK: u32 = 0;
static mut PM_PORT: u16 = 0;
static mut PM_32_BIT: bool = false;

fn read(register: u32) -> u32 {
    unsafe {
        if X2APIC {
            cpu::read_msr(0x800 + (register >> 4)) as u32
        } else {
            ((BASE + u64::from(register)) as *const u32).read_volatile()
        }
    }
}

fn write(register: u32, value: u32) {
    unsafe {
        if X2APIC {
            cpu::write_msr(0x800 + (register >> 4), u64::from(value));
        } else {
            ((BASE + u64::from(register)) as *mut u32).write_volatile(value);
        }
    }
}

/// This CPU's local APIC id (the destination an I/O APIC entry names).
pub fn id() -> u32 {
    unsafe { if X2APIC { read(0x20) } else { read(0x20) >> 24 } }
}

/// Acknowledges the interrupt being serviced.
pub fn end_of_interrupt() {
    write(REG_EOI, 0);
}

/// Timer ticks since `start_timer`.
pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

/// Called from the timer interrupt.
pub fn tick() {
    TICKS.fetch_add(1, Ordering::Relaxed);
    end_of_interrupt();
}

/// Microseconds since boot: the tick count refined by how far the local APIC timer has
/// counted down within the current tick.
pub fn micros() -> u64 {
    unsafe {
        if PER_TICK == 0 {
            return 0;
        }
        loop {
            let before = ticks();
            let current = u64::from(read(REG_TIMER_CURRENT));
            if ticks() != before {
                continue; // the tick rolled over while reading
            }
            let elapsed = (u64::from(PER_TICK) - current.min(u64::from(PER_TICK))) * 10_000 / u64::from(PER_TICK);
            return before * 10_000 + elapsed;
        }
    }
}

/// Waits `milliseconds` on the ACPI PM timer, with no interrupts needed (for fatal-error
/// paths, where the tick is not running). Without a PM timer it spins a rough count.
pub fn busy_wait_ms(milliseconds: u64) {
    unsafe {
        if PM_PORT == 0 {
            for _ in 0..milliseconds * 1_000_000 {
                core::hint::spin_loop();
            }
            return;
        }
        let mask: u64 = if PM_32_BIT { 0xffff_ffff } else { 0x00ff_ffff };
        let mut previous = u64::from(cpu::inl(PM_PORT)) & mask;
        let mut remaining = milliseconds * PM_TIMER_HZ / 1000;
        while remaining > 0 {
            let now = u64::from(cpu::inl(PM_PORT)) & mask;
            let elapsed = now.wrapping_sub(previous) & mask;
            previous = now;
            remaining = remaining.saturating_sub(elapsed);
            core::hint::spin_loop();
        }
    }
}

/// Busy-waits `count` timer ticks (10 ms each). Needs interrupts enabled.
pub fn wait_ticks(count: u64) {
    let start = ticks();
    while ticks().wrapping_sub(start) < count {
        crate::debug::pet();
        core::hint::spin_loop();
    }
}

/// Reads the PM timer, masked to its width.
fn pm_read(fadt: &acpi::Fadt) -> u64 {
    let raw = u64::from(cpu::inl(fadt.pm_timer_port));
    if fadt.pm_timer_32_bit { raw } else { raw & 0x00ff_ffff }
}

/// Microseconds elapsed since `start`, on the PM timer.
fn pm_elapsed_ticks(fadt: &acpi::Fadt, start: u64) -> u64 {
    let mask = if fadt.pm_timer_32_bit { 0xffff_ffff } else { 0x00ff_ffff };
    pm_read(fadt).wrapping_sub(start) & mask
}

pub struct Report {
    pub x2apic: bool,
    pub base: u64,
    pub timer_hz: u64,
}

/// Enables the local APIC, measures its timer against the PM timer and starts the
/// periodic tick at `TICK_HZ`. Interrupts are not enabled here (the caller does that
/// once the IDT is loaded). Returns `None` if the machine lacks a usable PM timer or
/// APIC.
pub fn start_timer(fadt: &acpi::Fadt, madt_address: u64) -> Option<Report> {
    if fadt.pm_timer_port == 0 {
        return None;
    }
    let (_, _, _, edx) = cpu::cpuid(1, 0);
    if edx & (1 << 9) == 0 {
        return None; // no local APIC
    }
    // The legacy 8259 pair is not used: mask every line so a stray legacy interrupt
    // cannot reach the CPU.
    cpu::outb(0x21, 0xff);
    cpu::outb(0xa1, 0xff);

    let msr = cpu::read_msr(cpu::MSR_APIC_BASE);
    let x2apic = msr & APIC_BASE_X2APIC != 0;
    let base = if madt_address != 0 { madt_address } else { msr & 0x000f_ffff_ffff_f000 };
    if msr & APIC_BASE_ENABLE == 0 {
        cpu::write_msr(cpu::MSR_APIC_BASE, msr | APIC_BASE_ENABLE);
    }
    unsafe {
        X2APIC = x2apic;
        BASE = base;
    }
    if !x2apic {
        paging::map_device(base, 4096);
    }
    unsafe {
        PM_PORT = fadt.pm_timer_port;
        PM_32_BIT = fadt.pm_timer_32_bit;
    }
    write(REG_SPURIOUS, 0x100 | u32::from(SPURIOUS_VECTOR)); // software-enable the APIC

    // Calibrate: let the timer count down from its maximum for 10 ms of PM-timer time.
    write(REG_TIMER_DIVIDE, 0x3); // divide the bus clock by 16
    write(REG_LVT_TIMER, 1 << 16 | u32::from(TIMER_VECTOR)); // masked, one-shot
    write(REG_TIMER_INITIAL, 0xffff_ffff);
    let start = pm_read(fadt);
    let window = PM_TIMER_HZ / 100;
    while pm_elapsed_ticks(fadt, start) < window {}
    let counted = 0xffff_ffffu64 - u64::from(read(REG_TIMER_CURRENT));
    write(REG_TIMER_INITIAL, 0);
    // `counted` timer ticks took 10 ms, so one 100 Hz tick is exactly that many.
    let per_tick = counted * 100 / TICK_HZ;
    write(REG_LVT_TIMER, 1 << 17 | u32::from(TIMER_VECTOR)); // periodic, unmasked
    unsafe { PER_TICK = per_tick as u32 };
    write(REG_TIMER_INITIAL, per_tick as u32);
    Some(Report { x2apic, base, timer_hz: counted * 100 * 16 })
}
