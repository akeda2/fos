//! The chipset's hardware watchdog (Intel TCO timer, in the LPC bridge at 00:1f.0), a backstop behind the software watchdog of
//! `debug.rs`. The software one is the boot CPU's timer tick; a boot CPU that stops taking interrupts (a `term net` shell
//! that hangs, since it runs inside the NIC poll with interrupts off) never reaches it. This one counts in the chipset.
//!
//! How it works (Intel ICH/PCH datasheets, "TCO"; the register layout is the one Linux's `iTCO_wdt` uses for TCO version 2):
//! - The LPC bridge's config register 0x40 holds the ACPI/PM I/O base; the TCO registers sit at that base + 0x60:
//!   +0x00 TCO_RLD (write: reload the counter), +0x04 TCO1_STS, +0x06 TCO2_STS, +0x08 TCO1_CNT (bit 11 TMR_HLT stops the
//!   timer), +0x12 TCO_TMR (the initial count, 0.6 s per tick, 10 bits).
//! - The timer expires twice before it resets the machine (the first expiry only raises TCO1_STS.TIMEOUT), so the reset comes
//!   after 2 x count x 0.6 s.
//! - The chipset's NO_REBOOT strap/bit (General Control, bit 5, in the root complex registers at RCBA + 0x3410, RCBA being
//!   LPC config 0xF0) makes the second expiry do nothing. Firmware often sets it; we clear it, read it back, and report when
//!   the firmware has locked it.
//! - TCO2_STS.SECOND_TO_STS survives the reset it causes, so the next boot can say "the hardware watchdog reset me".
//!
//! Only chipsets known to have this layout are touched (ICH9/ICH10, the 5 to 8 series PCHs and QEMU's q35); on anything else
//! the kernel says so and has just the software watchdog. The timer is stopped before every deliberate reset or halt
//! (`power::reset`, the crash handler's halt), because firmware that starts again must not inherit a running counter.

use core::sync::atomic::{AtomicU16, AtomicU64, Ordering};

use crate::{cpu, pci_config};

const VENDOR_INTEL: u32 = 0x8086;
const TCO_OFFSET: u16 = 0x60;
const TCO_RLD: u16 = 0x00;
const TCO1_STS: u16 = 0x04;
const TCO2_STS: u16 = 0x06;
const TCO1_CNT: u16 = 0x08;
const TCO_TMR: u16 = 0x12;
const TMR_HLT: u16 = 1 << 11;
const TCO1_TIMEOUT: u16 = 1 << 3;
const TCO2_SECOND_TO: u16 = 1 << 1;
const TCO2_BOOT_STS: u16 = 1 << 2;
const GCS_OFFSET: u64 = 0x3410;
const NO_REBOOT: u32 = 1 << 5;
/// One 10-bit count of 0.6 s ticks; the reset comes after two expiries.
const MAX_TICKS: u32 = 0x3ff;

/// The TCO register base, 0 while the hardware watchdog is not running.
static BASE: AtomicU16 = AtomicU16::new(0);
static LAST_RELOAD: AtomicU64 = AtomicU64::new(0);

/// LPC bridge device IDs with the TCO version 2 layout (ranges are the families' ID blocks).
fn known_chipset(device: u32) -> bool {
    matches!(device, 0x2918 | 0x3a10..=0x3a1f | 0x3b00..=0x3b0f | 0x1c40..=0x1c5f | 0x1d40..=0x1d4f | 0x1e40..=0x1e5f | 0x8c40..=0x8c5f)
}

fn seconds_to_ticks(seconds: u32) -> u16 {
    ((seconds * 5 / 6).clamp(4, MAX_TICKS)) as u16
}

/// Starts the hardware watchdog so that it resets the machine about `seconds` after the last [`reload`]. Returns what to tell
/// the operator. Needs the boot CPU's interrupts on or off alike (port I/O and one register page).
pub fn start(seconds: u32) -> Result<&'static str, &'static str> {
    let id = pci_config::read32(0, 0x1f, 0, 0);
    if id & 0xffff != VENDOR_INTEL || !known_chipset(id >> 16) {
        return Err("no known Intel TCO timer in the LPC bridge");
    }
    let pm_base = (pci_config::read32(0, 0x1f, 0, 0x40) & 0xff80) as u16;
    if pm_base == 0 || pci_config::read32(0, 0x1f, 0, 0x44) & 0x80 == 0 {
        return Err("the ACPI/PM I/O range is not enabled");
    }
    let base = pm_base + TCO_OFFSET;
    let rcba = pci_config::read32(0, 0x1f, 0, 0xf0);
    if rcba & 1 == 0 {
        return Err("the root complex registers (RCBA) are not enabled");
    }
    let gcs_physical = u64::from(rcba & !0x3fff) + GCS_OFFSET;
    crate::vm::map_device(gcs_physical, 4);
    let gcs = crate::vm::direct(gcs_physical) as *mut u32;
    unsafe {
        let value = gcs.read_volatile();
        if value & NO_REBOOT != 0 {
            gcs.write_volatile(value & !NO_REBOOT);
            if gcs.read_volatile() & NO_REBOOT != 0 {
                return Err("the firmware keeps NO_REBOOT set (the timer would count but never reset)");
            }
        }
    }
    // Reads the cause of the previous reset before clearing it.
    let second = cpu::inw(base + TCO2_STS) & TCO2_SECOND_TO != 0;
    if second {
        println!("The previous boot was ended by the hardware watchdog (TCO second timeout).");
    }
    cpu::outw(base + TCO1_CNT, cpu::inw(base + TCO1_CNT) | TMR_HLT);
    cpu::outw(base + TCO1_STS, TCO1_TIMEOUT);
    cpu::outw(base + TCO2_STS, TCO2_SECOND_TO | TCO2_BOOT_STS);
    let tmr = cpu::inw(base + TCO_TMR);
    cpu::outw(base + TCO_TMR, (tmr & 0xfc00) | seconds_to_ticks(seconds));
    cpu::outw(base + TCO_RLD, 1);
    cpu::outw(base + TCO1_CNT, cpu::inw(base + TCO1_CNT) & !TMR_HLT);
    BASE.store(base, Ordering::Release);
    Ok("TCO timer running")
}

/// Pets the hardware watchdog (at most once per timer tick: a port write is cheap, but the service loop spins fast).
pub fn reload() {
    let base = BASE.load(Ordering::Acquire);
    if base != 0 {
        let now = crate::apic::ticks();
        if LAST_RELOAD.swap(now, Ordering::Relaxed) != now {
            cpu::outw(base + TCO_RLD, 1);
        }
    }
}

/// Stops the timer: before a deliberate reset or halt.
pub fn stop() {
    let base = BASE.swap(0, Ordering::AcqRel);
    if base != 0 {
        cpu::outw(base + TCO1_CNT, cpu::inw(base + TCO1_CNT) | TMR_HLT);
    }
}

/// True while the timer is running.
pub fn running() -> bool {
    BASE.load(Ordering::Acquire) != 0
}
