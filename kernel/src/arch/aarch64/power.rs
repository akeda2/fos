//! Resetting the machine. On a Pi 4 (the GIC distributor at 0xFF841000 says so) the firmware offers no reset call the kernel
//! can reach at EL2, so the BCM2711 power-management watchdog does it, as Linux's bcm2835_wdt does: PM_WDOG gets a timeout of
//! ten ticks (about 150 us) and PM_RSTC selects a full reset; both writes carry the 0x5a password in the top byte. Anywhere
//! else (QEMU `virt`) the PSCI SYSTEM_RESET call goes through HVC, falling back to SMC.

use core::arch::asm;

const PM_BASE: u64 = 0xfe10_0000;
const PM_RSTC: u64 = PM_BASE + 0x1c;
const PM_WDOG: u64 = PM_BASE + 0x24;
const PM_PASSWORD: u32 = 0x5a00_0000;
const PM_RSTC_FULL_RESET: u32 = 0x20;
const PM_RSTC_CONFIG_MASK: u32 = 0x30;
const PSCI_SYSTEM_RESET: u64 = 0x8400_0009;

pub fn reset() -> ! {
    crate::slots::mark_clean_stop();
    if crate::gic::bases().0 == 0xff84_1000 {
        crate::paging::map_device(PM_BASE, 0x1000);
        unsafe {
            core::ptr::write_volatile(PM_WDOG as *mut u32, PM_PASSWORD | 10);
            let current = core::ptr::read_volatile(PM_RSTC as *const u32);
            core::ptr::write_volatile(PM_RSTC as *mut u32, PM_PASSWORD | (current & !PM_RSTC_CONFIG_MASK) | PM_RSTC_FULL_RESET);
        }
    } else {
        unsafe {
            asm!("hvc #0", inout("x0") PSCI_SYSTEM_RESET => _, options(nostack));
            asm!("smc #0", inout("x0") PSCI_SYSTEM_RESET => _, options(nostack));
        }
    }
    loop {
        crate::cpu::sleep();
    }
}
