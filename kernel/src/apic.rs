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
/// The interrupt request registers: eight 32-bit words, 0x10 apart; vector V is bit V%32 of word V/32.
const REG_IRR: u32 = 0x200;

pub const TIMER_VECTOR: u8 = 0x40;
pub const SPURIOUS_VECTOR: u8 = 0xff;
/// Where the retired 8259 pair's sixteen lines are moved: vectors 0x30-0x3F, which nothing else uses (the timer is 0x40,
/// the keyboard 0x41, MSI 0x50, legacy INTx 0x60).
pub const LEGACY_PIC_BASE: u8 = 0x30;
/// Interrupts that arrived on those vectors. With every line masked the only ones that can are *spurious*: see
/// [`retire_legacy_pic`].
static LEGACY_PIC_INTERRUPTS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// The legacy 8259 interrupt controllers are not used (the local APIC and the I/O APIC are), but they cannot just be left
/// alone or masked:
///
/// * A BIOS leaves them delivering IRQ0-7 on vectors 8-15 and IRQ8-15 on 0x70-0x77. Vectors 8-15 are CPU exceptions, so a
///   legacy interrupt arriving there is taken for a fault (vector 8 is the double fault).
/// * Masking a line that has already raised the CPU's INT pin makes a real 8259 answer the next interrupt acknowledge
///   with a *spurious IRQ7* (vector 15, "reserved exception"). The kernel enables interrupts moments after this, so the
///   machine stops right there. Emulators do not do this; an AMD/nForce board did.
///
/// So: run the controllers' initialisation sequence (ICW1-ICW4), which also clears any pending request, give them bases
/// 0x30 and 0x38 so that nothing they can deliver collides with an exception, then mask all sixteen lines. A spurious
/// IRQ7 or IRQ15 can still happen occasionally (a glitch on a line); it arrives on 0x37 or 0x3F and
/// [`legacy_pic_interrupt`] ignores it. Interrupts must still be off when this runs.
fn retire_legacy_pic() {
    cpu::outb(0x20, 0x11); // ICW1: initialise, expect ICW4, cascade mode
    cpu::io_wait();
    cpu::outb(0xa0, 0x11);
    cpu::io_wait();
    cpu::outb(0x21, LEGACY_PIC_BASE); // ICW2: the master's vector base (IRQ0-7)
    cpu::io_wait();
    cpu::outb(0xa1, LEGACY_PIC_BASE + 8); // ICW2: the slave's (IRQ8-15)
    cpu::io_wait();
    cpu::outb(0x21, 4); // ICW3: the slave hangs off the master's IRQ2
    cpu::io_wait();
    cpu::outb(0xa1, 2); // ICW3: the slave's cascade identity
    cpu::io_wait();
    cpu::outb(0x21, 0x01); // ICW4: 8086 mode
    cpu::io_wait();
    cpu::outb(0xa1, 0x01);
    cpu::io_wait();
    cpu::outb(0x21, 0xff); // OCW1: mask every line
    cpu::outb(0xa1, 0xff);
}

/// An interrupt on one of the retired PIC's vectors. They are all masked, so this is a spurious one (or a glitch): no
/// end-of-interrupt is sent (a spurious interrupt is not "in service", and sending one would acknowledge a different line).
pub fn legacy_pic_interrupt() {
    LEGACY_PIC_INTERRUPTS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
}

/// How many have been seen since boot.
pub fn legacy_pic_interrupts() -> u64 {
    LEGACY_PIC_INTERRUPTS.load(core::sync::atomic::Ordering::Relaxed)
}
/// The kernel's tick rate.
pub const TICK_HZ: u64 = 100;

const APIC_BASE_ENABLE: u64 = 1 << 11;
const APIC_BASE_X2APIC: u64 = 1 << 10;
/// The ACPI PM timer's frequency in Hz.
const PM_TIMER_HZ: u64 = 3_579_545;

static TICKS: AtomicU64 = AtomicU64::new(0);
/// The last value `micros()` returned: it never goes below it.
static LAST_MICROS: AtomicU64 = AtomicU64::new(0);
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
///
/// When the timer interrupt is late (interrupts disabled for a while, or the virtual CPU stalled) the
/// countdown has already restarted while `ticks()` has not yet advanced, which used to make the clock
/// jump back by up to 10 ms and then forward again. A timer interrupt that is requested but not yet
/// serviced (its bit in the local APIC's IRR) is therefore counted as an elapsed tick, and the result is
/// clamped so the clock never goes backwards whatever else happens.
pub fn micros() -> u64 {
    unsafe {
        if PER_TICK == 0 {
            return 0;
        }
        loop {
            let before = ticks();
            let current = u64::from(read(REG_TIMER_CURRENT));
            let pending = read(REG_IRR + 0x10 * (u32::from(TIMER_VECTOR) / 32)) & (1 << (TIMER_VECTOR % 32)) != 0;
            if ticks() != before {
                continue; // the tick rolled over while reading
            }
            let elapsed = (u64::from(PER_TICK) - current.min(u64::from(PER_TICK))) * 10_000 / u64::from(PER_TICK);
            let value = (before + u64::from(pending)) * 10_000 + elapsed;
            let previous = LAST_MICROS.fetch_max(value, Ordering::Relaxed);
            return value.max(previous);
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
    retire_legacy_pic();

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
