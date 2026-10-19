//! GICv2 interrupt controller: the Raspberry Pi 4's GIC-400, and QEMU `virt`'s default GIC.
//!
//! One distributor (routes interrupts, here all to CPU 0) and one CPU interface (acknowledge/end of interrupt). Registers
//! are memory-mapped through `paging::map_device`. The defaults are QEMU's; MADT supplies the real ones (the Pi 4's are GICD 0xFF841000, GICC 0xFF842000) through `set_base`. Only the private peripheral interrupt of the
//! generic physical timer (INTID 30) is wired up so far; it drives a 100 Hz counter, proving the whole IRQ path.

use core::sync::atomic::{AtomicU64, Ordering};

pub const QEMU_GICD: u64 = 0x0800_0000;
pub const QEMU_GICC: u64 = 0x0801_0000;
const TIMER_INTID: u32 = 30;

static GICD: AtomicU64 = AtomicU64::new(QEMU_GICD);
static GICC: AtomicU64 = AtomicU64::new(QEMU_GICC);
static IRQ_TICKS: AtomicU64 = AtomicU64::new(0);
static RELOAD: AtomicU64 = AtomicU64::new(0);

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
    enable(TIMER_INTID);
    start_timer();
}

fn enable(intid: u32) {
    write(&GICD, 0x100 + 4 * (intid as u64 / 32), 1 << (intid % 32));
}

fn start_timer() {
    let reload = crate::timer::frequency() / crate::timer::TICK_HZ;
    RELOAD.store(reload, Ordering::Relaxed);
    unsafe {
        core::arch::asm!("msr cntp_tval_el0, {}", "msr cntp_ctl_el0, {}", in(reg) reload, in(reg) 1u64, options(nomem, nostack));
    }
}

/// Timer interrupts counted since `init`.
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
        IRQ_TICKS.fetch_add(1, Ordering::Relaxed);
        let reload = RELOAD.load(Ordering::Relaxed);
        unsafe { core::arch::asm!("msr cntp_tval_el0, {}", in(reg) reload, options(nomem, nostack)) };
    }
    write(&GICC, 0x010, iar);
}
