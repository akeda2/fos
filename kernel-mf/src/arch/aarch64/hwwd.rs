//! The hardware watchdog on a Pi 4: the BCM2711 power-management watchdog (PM_WDOG), the same block `power_arch` resets with.
//! PM_WDOG counts 1/65536 s in a 20-bit field (16 s at most); PM_RSTC's reset-type bits arm it (full reset) or leave it idle.
//! Both registers take the 0x5a password in the top byte. It backs the software watchdog of `debug.rs` for a boot CPU that no
//! longer takes interrupts. Not on QEMU virt (no such block; the software watchdog alone). Only a real Pi 4 shows it resetting.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

const PM_BASE: u64 = 0xfe10_0000;
const PM_RSTC: u64 = PM_BASE + 0x1c;
const PM_WDOG: u64 = PM_BASE + 0x24;
const PM_PASSWORD: u32 = 0x5a00_0000;
const PM_RSTC_FULL_RESET: u32 = 0x20;
const PM_RSTC_CONFIG_MASK: u32 = 0x30;
const MAX_SECONDS: u32 = 15;

static ARMED: AtomicBool = AtomicBool::new(false);
static TIMEOUT: AtomicU64 = AtomicU64::new(0);
static LAST_RELOAD: AtomicU64 = AtomicU64::new(0);

fn is_pi4() -> bool {
    crate::gic::bases().0 == 0xff84_1000
}

fn write(register: u64, value: u32) {
    unsafe { core::ptr::write_volatile(register as *mut u32, PM_PASSWORD | value) };
}

fn rstc() -> u32 {
    unsafe { core::ptr::read_volatile(PM_RSTC as *const u32) }
}

/// Arms the timer for `seconds` (at most 15: the field is 20 bits; `reload` must come more often).
pub fn start(seconds: u32) -> Result<&'static str, &'static str> {
    if !is_pi4() {
        return Err("no BCM2711 power-management watchdog on this machine");
    }
    let seconds = seconds.min(MAX_SECONDS);
    crate::paging::map_device(PM_BASE, 0x1000);
    TIMEOUT.store(u64::from(seconds) * 65536, Ordering::Relaxed);
    write(PM_WDOG, (seconds * 65536) & 0xf_ffff);
    write(PM_RSTC, (rstc() & !PM_RSTC_CONFIG_MASK) | PM_RSTC_FULL_RESET);
    ARMED.store(true, Ordering::Release);
    Ok("BCM2711 PM_WDOG (at most 15 s, fed by the service thread)")
}

/// Feeds the timer; at most once per tick.
pub fn reload() {
    if ARMED.load(Ordering::Acquire) {
        let now = crate::arch::ticks();
        if LAST_RELOAD.swap(now, Ordering::Relaxed) != now {
            write(PM_WDOG, (TIMEOUT.load(Ordering::Relaxed) as u32) & 0xf_ffff);
        }
    }
}

/// Disarms it before a deliberate halt, so a machine left for an operator does not reset.
pub fn stop() {
    if ARMED.swap(false, Ordering::AcqRel) {
        write(PM_RSTC, rstc() & !PM_RSTC_CONFIG_MASK);
    }
}

pub fn running() -> bool {
    ARMED.load(Ordering::Acquire)
}
