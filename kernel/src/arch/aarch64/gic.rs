//! GICv2 interrupt controller: the Raspberry Pi 4's GIC-400, and QEMU `virt`'s default GIC.
//!
//! One distributor (routes interrupts, here all to CPU 0) and one CPU interface (acknowledge/end of interrupt). Registers
//! are memory-mapped through `paging::map_device`. The defaults are QEMU's; MADT supplies the real ones (the Pi 4's are GICD 0xFF841000, GICC 0xFF842000) through `set_base`. Only the private peripheral interrupt of the
//! generic physical timer (INTID 30) is wired up so far; it drives a 100 Hz counter, proving the whole IRQ path.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

pub const QEMU_GICD: u64 = 0x0800_0000;
pub const QEMU_GICC: u64 = 0x0801_0000;
const TIMER_INTID: u32 = 30;

static GICD: AtomicU64 = AtomicU64::new(QEMU_GICD);
static GICC: AtomicU64 = AtomicU64::new(QEMU_GICC);
static IRQ_TICKS: AtomicU64 = AtomicU64::new(0);
static RELOAD: AtomicU64 = AtomicU64::new(0);
/// Highest interrupt ID handled (the Pi 4's GIC-400 has 216 SPIs, INTID 32..=255 used; QEMU `virt` up to 160).
const MAX_INTID: usize = 288;
/// Handlers by INTID (0 = none), `fn(intid)`; they run in IRQ context before the end-of-interrupt.
static HANDLERS: [AtomicUsize; MAX_INTID] = [const { AtomicUsize::new(0) }; MAX_INTID];

pub fn set_base(distributor: u64, cpu_interface: u64) {
    GICD.store(distributor, Ordering::Relaxed);
    GICC.store(cpu_interface, Ordering::Relaxed);
}

/// (distributor, CPU interface) as configured.
pub fn bases() -> (u64, u64) {
    (GICD.load(Ordering::Relaxed), GICC.load(Ordering::Relaxed))
}

fn write(base: &AtomicU64, offset: u64, value: u32) {
    unsafe { ((base.load(Ordering::Relaxed) + offset) as *mut u32).write_volatile(value) };
}

fn read(base: &AtomicU64, offset: u64) -> u32 {
    unsafe { ((base.load(Ordering::Relaxed) + offset) as *const u32).read_volatile() }
}

/// Enables the distributor and this CPU's interface (every priority passes), then starts the 100 Hz timer interrupt.
/// Interrupts stay masked in DAIF until the caller enables them.
pub fn init() {
    crate::paging::map_device(GICD.load(Ordering::Relaxed), 0x1000);
    crate::paging::map_device(GICC.load(Ordering::Relaxed), 0x2000);
    write(&GICD, 0x000, 1);
    write(&GICC, 0x004, 0xff);
    write(&GICC, 0x000, 1);
    BOOT_MPIDR.store(mpidr(), Ordering::Relaxed);
    enable(0xffff);
    enable(TIMER_INTID);
    start_timer();
}

/// A secondary CPU's share of `init`: its own banked CPU interface, the timer and the software-generated interrupts.
/// The distributor is already running.
pub fn init_secondary() {
    write(&GICC, 0x004, 0xff);
    write(&GICC, 0x000, 1);
    enable(0xffff);
    enable(TIMER_INTID);
    start_timer();
}

/// This CPU's bit in the distributor's target registers (the first 32 are banked, and each CPU reads its own mask).
pub fn cpu_target() -> u8 {
    (0..8u64).map(|offset| unsafe { ((GICD.load(Ordering::Relaxed) + 0x800 + offset) as *const u8).read_volatile() }).find(|&mask| mask != 0).unwrap_or(1)
}

/// Raises software-generated interrupt `intid` (0..=15) on the CPUs in `targets` (a bit per GIC CPU interface).
pub fn send_sgi(targets: u8, intid: u32) {
    write(&GICD, 0xf00, u32::from(targets) << 16 | intid);
}

/// Calls `handler(intid)` for software-generated interrupt `intid` (0..=15), after its end-of-interrupt; every CPU enables it
/// in `init`/`init_secondary`.
pub fn register_sgi(intid: u32, handler: fn(u32)) {
    if intid < 16 {
        HANDLERS[intid as usize].store(handler as usize, Ordering::Release);
    }
}

/// Routes shared peripheral interrupt `intid` (32 and up) to CPU 0 and calls `handler(intid)` when it fires. `level` is
/// the trigger type the device uses (PCIe, Genet: level; GICv2m MSIs: edge). The line is enabled at once: the handler
/// must find and clear its cause, or a level interrupt fires again forever.
pub fn register(intid: u32, level: bool, handler: fn(u32)) {
    if (intid as usize) >= MAX_INTID || intid < 32 {
        return;
    }
    HANDLERS[intid as usize].store(handler as usize, Ordering::Release);
    let config = 0xc00 + 4 * (intid as u64 / 16);
    let shift = 2 * (intid % 16) + 1; // bit 1 of each 2-bit field: 1 = edge-triggered
    let value = read(&GICD, config) & !(1 << shift);
    write(&GICD, config, value | u32::from(!level) << shift);
    write_byte(0x400 + intid as u64, 0xa0); // priority
    write_byte(0x800 + intid as u64, 0x01); // target: CPU 0
    enable(intid);
}

fn write_byte(offset: u64, value: u8) {
    unsafe { ((GICD.load(Ordering::Relaxed) + offset) as *mut u8).write_volatile(value) };
}

fn enable(intid: u32) {
    // 0xffff stands for "all sixteen SGIs" (bits 0..=15 of the banked ISENABLER0).
    if intid == 0xffff {
        return write(&GICD, 0x100, 0xffff);
    }
    write(&GICD, 0x100 + 4 * (intid as u64 / 32), 1 << (intid % 32));
}

fn start_timer() {
    let reload = crate::timer::frequency() / crate::timer::TICK_HZ;
    RELOAD.store(reload, Ordering::Relaxed);
    unsafe {
        core::arch::asm!("msr cntp_tval_el0, {}", "msr cntp_ctl_el0, {}", in(reg) reload, in(reg) 1u64, options(nomem, nostack));
    }
}

static BOOT_MPIDR: AtomicU64 = AtomicU64::new(0);

fn mpidr() -> u64 {
    let value: u64;
    unsafe { core::arch::asm!("mrs {}, mpidr_el1", out(reg) value, options(nomem, nostack)) };
    value & 0xff_00ff_ffff
}

/// Timer interrupts the boot CPU has taken since `init`.
pub fn irq_ticks() -> u64 {
    IRQ_TICKS.load(Ordering::Relaxed)
}

/// Runs in IRQ context: acknowledge, handle, signal end of interrupt.
pub fn dispatch() {
    let iar = read(&GICC, 0x00c);
    let intid = iar & 0x3ff;
    if intid >= 1020 {
        return;
    }
    if intid == TIMER_INTID {
        // The clock is the boot CPU's timer alone; the others' ticks would run it several times too fast.
        if mpidr() == BOOT_MPIDR.load(Ordering::Relaxed) {
            IRQ_TICKS.fetch_add(1, Ordering::Relaxed);
        }
        let reload = RELOAD.load(Ordering::Relaxed);
        unsafe { core::arch::asm!("msr cntp_tval_el0, {}", in(reg) reload, options(nomem, nostack)) };
        // The timer is acknowledged before `crash::tick` because the server kernel's scheduler may switch threads inside it: the
        // interrupt must not stay active until this thread runs again (IRQs are masked for the whole handler).
        write(&GICC, 0x010, iar);
        crate::crash::tick();
        return;
    }
    if intid < 16 {
        // Software-generated: acknowledged before the handler, which may switch threads (as the timer's does).
        write(&GICC, 0x010, iar);
        let handler = HANDLERS[intid as usize].load(Ordering::Acquire);
        if handler != 0 {
            let handler: fn(u32) = unsafe { core::mem::transmute(handler) };
            handler(intid);
        }
        return;
    }
    let handler = HANDLERS.get(intid as usize).map_or(0, |h| h.load(Ordering::Acquire));
    if handler != 0 {
        let handler: fn(u32) = unsafe { core::mem::transmute(handler) };
        handler(intid);
    }
    write(&GICC, 0x010, iar);
}
