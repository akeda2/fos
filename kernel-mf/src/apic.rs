//! The local APIC and its timer: the kernel's clock tick, and the I/O APIC's masked start.
//!
//! The legacy PIT is not guaranteed on UEFI machines, so the tick comes from the local APIC timer, calibrated once against
//! the ACPI power-management timer (3.579545 MHz on every chipset). The local APIC is reached through memory-mapped
//! registers (xAPIC) or MSRs (x2APIC, which some firmware switches on before handing over; both are handled).
//! The embedded kernel's `arch/x86_64/apic.rs` is the model; this one is written for per-CPU use (slice 4): the calibrated
//! count is kept so an AP can start its own timer without calibrating again.

use crate::{acpi, cpu, vm};
use core::sync::atomic::Ordering;

const REG_ID: u32 = 0x20;
const REG_EOI: u32 = 0xb0;
const REG_SPURIOUS: u32 = 0xf0;
const REG_LVT_TIMER: u32 = 0x320;
const REG_TIMER_INITIAL: u32 = 0x380;
const REG_TIMER_CURRENT: u32 = 0x390;
const REG_TIMER_DIVIDE: u32 = 0x3e0;

pub const TIMER_VECTOR: u8 = 0x40;
pub const SPURIOUS_VECTOR: u8 = 0xff;
/// Where the retired 8259 pair's sixteen lines are moved: vectors 0x30-0x3F, which nothing else uses.
pub const LEGACY_PIC_BASE: u8 = 0x30;
pub const TICK_HZ: u64 = 100;

const MSR_APIC_BASE: u32 = 0x1b;
const APIC_BASE_ENABLE: u64 = 1 << 11;
const APIC_BASE_X2APIC: u64 = 1 << 10;
const PM_TIMER_HZ: u64 = 3_579_545;

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
            (vm::direct(BASE + u64::from(register)) as *const u32).read_volatile()
        }
    }
}

fn write(register: u32, value: u32) {
    unsafe {
        if X2APIC {
            cpu::write_msr(0x800 + (register >> 4), u64::from(value));
        } else {
            (vm::direct(BASE + u64::from(register)) as *mut u32).write_volatile(value);
        }
    }
}

/// This CPU's local APIC id.
pub fn id() -> u32 {
    unsafe { if X2APIC { read(REG_ID) } else { read(REG_ID) >> 24 } }
}

pub fn end_of_interrupt() {
    write(REG_EOI, 0);
}

/// Ticks counted by the boot CPU's timer: the kernel's clock.
pub fn ticks() -> u64 {
    crate::percpu::ticks_of(0)
}

/// Called from the timer interrupt of whichever CPU it is: each counts its own.
pub fn tick() {
    crate::percpu::this().ticks.fetch_add(1, Ordering::Relaxed);
    end_of_interrupt();
}

/// Reads the PM timer, masked to its width.
fn pm_read() -> u64 {
    let raw = u64::from(cpu::inl(unsafe { PM_PORT }));
    if unsafe { PM_32_BIT } { raw } else { raw & 0x00ff_ffff }
}

fn pm_elapsed(start: u64) -> u64 {
    let mask = if unsafe { PM_32_BIT } { 0xffff_ffff } else { 0x00ff_ffff };
    pm_read().wrapping_sub(start) & mask
}

/// Waits `milliseconds` on the PM timer: independent of interrupts, so it also works where the tick is not running.
pub fn busy_wait_ms(milliseconds: u64) {
    let start = pm_read();
    while pm_elapsed(start) < milliseconds * PM_TIMER_HZ / 1000 {
        core::hint::spin_loop();
    }
}

/// The 8259 pair is retired, not just masked: a masked line that already raised INTR makes a real 8259 answer the next
/// acknowledge with a spurious IRQ7 on vector 15 (a CPU exception), which stopped an AMD/nForce board. So: the full
/// ICW1-ICW4 sequence (it also clears pending requests), bases 0x30/0x38 clear of every exception, then mask all sixteen.
/// Interrupts must be off. A stray spurious IRQ7/15 then lands on 0x37/0x3f, which the dispatcher ignores.
fn retire_legacy_pic() {
    cpu::outb(0x20, 0x11);
    cpu::io_wait();
    cpu::outb(0xa0, 0x11);
    cpu::io_wait();
    cpu::outb(0x21, LEGACY_PIC_BASE);
    cpu::io_wait();
    cpu::outb(0xa1, LEGACY_PIC_BASE + 8);
    cpu::io_wait();
    cpu::outb(0x21, 4);
    cpu::io_wait();
    cpu::outb(0xa1, 2);
    cpu::io_wait();
    cpu::outb(0x21, 0x01);
    cpu::io_wait();
    cpu::outb(0xa1, 0x01);
    cpu::io_wait();
    cpu::outb(0x21, 0xff);
    cpu::outb(0xa1, 0xff);
}

pub struct Report {
    pub x2apic: bool,
    pub base: u64,
    pub timer_hz: u64,
}

/// Enables the boot CPU's local APIC, measures its timer against the PM timer and starts the periodic tick at `TICK_HZ`.
/// Interrupts are not enabled here. `None` if the machine lacks a PM timer or a local APIC.
pub fn start_timer(fadt: &acpi::Fadt, madt_address: u64) -> Option<Report> {
    if fadt.pm_timer_port == 0 || cpu::cpuid(1, 0).3 & (1 << 9) == 0 {
        return None;
    }
    retire_legacy_pic();

    let msr = cpu::read_msr(MSR_APIC_BASE);
    let x2apic = msr & APIC_BASE_X2APIC != 0;
    let base = if madt_address != 0 { madt_address } else { msr & 0x000f_ffff_ffff_f000 };
    if msr & APIC_BASE_ENABLE == 0 {
        cpu::write_msr(MSR_APIC_BASE, msr | APIC_BASE_ENABLE);
    }
    unsafe {
        X2APIC = x2apic;
        BASE = base;
        PM_PORT = fadt.pm_timer_port;
        PM_32_BIT = fadt.pm_timer_32_bit;
    }
    if !x2apic {
        vm::map_device(base, 4096);
    }
    write(REG_SPURIOUS, 0x100 | u32::from(SPURIOUS_VECTOR));

    // Let the timer count down from its maximum for 10 ms of PM-timer time.
    write(REG_TIMER_DIVIDE, 0x3); // bus clock / 16
    write(REG_LVT_TIMER, 1 << 16 | u32::from(TIMER_VECTOR)); // masked, one-shot
    write(REG_TIMER_INITIAL, 0xffff_ffff);
    let start = pm_read();
    while pm_elapsed(start) < PM_TIMER_HZ / 100 {}
    let counted = 0xffff_ffffu64 - u64::from(read(REG_TIMER_CURRENT));
    write(REG_TIMER_INITIAL, 0);
    let per_tick = counted * 100 / TICK_HZ;
    write(REG_LVT_TIMER, 1 << 17 | u32::from(TIMER_VECTOR)); // periodic, unmasked
    unsafe { PER_TICK = per_tick as u32 };
    write(REG_TIMER_INITIAL, per_tick as u32);
    Some(Report { x2apic, base, timer_hz: counted * 100 * 16 })
}

/// Sends an inter-processor interrupt: `low` is the ICR's low word (delivery mode, vector, level). xAPIC: destination in
/// ICR high (0x310), then the low write sends it and delivery status (bit 12) clears when the target has accepted it.
/// x2APIC: one 64-bit MSR write (0x830), no status to wait for.
pub fn send_ipi(apic_id: u32, low: u32) {
    unsafe {
        if X2APIC {
            cpu::write_msr(0x830, u64::from(apic_id) << 32 | u64::from(low));
        } else {
            write(0x310, apic_id << 24);
            write(0x300, low);
            for _ in 0..1_000_000 {
                if read(0x300) & (1 << 12) == 0 {
                    break;
                }
                core::hint::spin_loop();
            }
        }
    }
}

/// Enables the calling CPU's own local APIC the way the boot CPU's was (same mode, same spurious vector). An AP starts with
/// its APIC in xAPIC mode, enabled; when the boot CPU runs x2APIC, the AP is switched over too.
pub fn enable_this_cpu() {
    let msr = cpu::read_msr(MSR_APIC_BASE) | APIC_BASE_ENABLE;
    cpu::write_msr(MSR_APIC_BASE, if unsafe { X2APIC } { msr | APIC_BASE_X2APIC } else { msr });
    write(REG_SPURIOUS, 0x100 | u32::from(SPURIOUS_VECTOR));
}

/// Starts the calling CPU's periodic tick with the boot CPU's calibrated count (every core runs from the same bus clock).
pub fn start_local_timer() {
    write(REG_TIMER_DIVIDE, 0x3);
    write(REG_LVT_TIMER, 1 << 17 | u32::from(TIMER_VECTOR));
    write(REG_TIMER_INITIAL, unsafe { PER_TICK });
}

/// The first I/O APIC, with every pin masked. Pins are routed by the drivers that own them (slice 5). Registers are an
/// index/data pair: the register number to IOREGSEL (+0x00), then the value through IOWIN (+0x10).
pub mod ioapic {
    use crate::{acpi, vm};

    static mut BASE: u64 = 0;

    fn read(register: u32) -> u32 {
        unsafe {
            (vm::direct(BASE) as *mut u32).write_volatile(register);
            (vm::direct(BASE + 0x10) as *const u32).read_volatile()
        }
    }

    fn write(register: u32, value: u32) {
        unsafe {
            (vm::direct(BASE) as *mut u32).write_volatile(register);
            (vm::direct(BASE + 0x10) as *mut u32).write_volatile(value);
        }
    }

    /// Maps the first I/O APIC and masks every pin. Returns the number of pins.
    pub fn init(madt: &acpi::Madt) -> Option<u32> {
        if madt.io_apics == 0 || madt.io_apic_address == 0 {
            return None;
        }
        unsafe { BASE = madt.io_apic_address };
        vm::map_device(madt.io_apic_address, 4096);
        let pins = ((read(1) >> 16) & 0xff) + 1;
        for pin in 0..pins {
            write(0x10 + 2 * pin, 1 << 16);
        }
        Some(pins)
    }
}
