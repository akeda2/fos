//! Unattended operation for the server kernel, the embedded kernel's `debug.rs` redone for it: the crash record, `reboot-on-crash`
//! with the crash-loop guard, and the watchdog. The slot metadata (A/B fallback) is `slots.rs`.
//!
//! **The crash record** is one page of RAM at physical 0x5000 (below the 1 MiB the allocator never hands out; the BIOS stage two
//! leaves it alone). The fatal-exception handler writes it, the next boot prints and clears it. RAM does not survive a reset on
//! every machine; then there is simply nothing to report.
//!
//! **The watchdog**: the service thread calls [`pet`] on every pass. The boot CPU's timer interrupt checks that the last pet is
//! younger than N seconds; if not, the machine is hung (a CPU 0 with interrupts off never reaches this check either, which only a
//! hardware watchdog could catch), so it records a crash (vector 240, the RIP the CPU was at) and resets. Off after three unclean
//! boots in a row, and the streak is cleared by a boot that runs five minutes (`slots.rs`).

use core::sync::atomic::{AtomicU64, Ordering};

use crate::trap::Frame;

pub const WATCHDOG_VECTOR: u64 = 240;
pub const DEADLOCK_VECTOR: u64 = 241;
pub const PANIC_VECTOR: u64 = 242;

static mut BOOT_CRASH_COUNT: u32 = 0;
/// Where the record page is in the direct map, 0 when it is not mapped (no RAM record).
static mut RECORD: u64 = 0;
static LAST_PET: AtomicU64 = AtomicU64::new(0);
/// Watchdog limit in ticks, 0 while off.
static LIMIT_TICKS: AtomicU64 = AtomicU64::new(0);

pub fn init(crash_count: u32) {
    unsafe { BOOT_CRASH_COUNT = crash_count };
    crate::arch::crashrec::init();
}

pub use crate::arch::crashrec::{record_crash, report_previous_crash};

/// The loader counted this many consecutive boots that never reached the milestone: stop rebooting.
pub fn crash_loop_tripped() -> bool {
    unsafe { BOOT_CRASH_COUNT >= u32::from(fosfor_abi::MAX_CONSECUTIVE_CRASHES) }
}

pub fn boot_crash_count() -> u32 {
    unsafe { BOOT_CRASH_COUNT }
}

// `kup_serial.rs` (shared with the embedded kernel) reaches the slot metadata through `debug`.
pub use crate::slots::{read_metadata, update_metadata};

/// Ticks since the service thread last went round its loop (the SysRq rescue's trigger).
pub fn since_pet_ticks() -> u64 {
    crate::time::ticks().saturating_sub(LAST_PET.load(Ordering::Relaxed))
}

/// The service thread is making progress.
pub fn pet() {
    LAST_PET.store(crate::time::ticks(), Ordering::Relaxed);
    crate::hwwd::reload();
}

/// Arms the watchdog at the milestone (`unclean` boots in a row keep it off).
pub fn arm(seconds: Option<u32>, unclean: u8) {
    let Some(seconds) = seconds else { return };
    if unclean >= 3 {
        println!("Watchdog stays off: {unclean} unclean boots in a row (a hang must not become a reboot loop).");
        println!("It comes back on the next boot once this one has run five minutes.");
        return;
    }
    let seconds = seconds.max(5);
    pet();
    LIMIT_TICKS.store(u64::from(seconds) * crate::time::TICK_HZ, Ordering::Relaxed);
    println!("Watchdog armed: reset after {seconds} s without progress.");
    // The chipset timer is the backstop for a boot CPU that no longer takes interrupts; it must outlast the software one and
    // the crash handler's reboot delay.
    let hardware = seconds + crate::config::reboot_delay() + 15;
    match crate::hwwd::start(hardware) {
        Ok(text) => println!("Hardware watchdog: {text}, reset after {hardware} s without progress."),
        Err(reason) => println!("Hardware watchdog: not available ({reason})."),
    }
}

pub fn watchdog_seconds() -> Option<u64> {
    match LIMIT_TICKS.load(Ordering::Relaxed) {
        0 => None,
        ticks => Some(ticks / crate::time::TICK_HZ),
    }
}

/// True once (and disarms the watchdog) when the service thread has not petted for longer than the limit; for ports whose timer
/// handler has no `Frame` (aarch64).
pub fn watchdog_expired() -> bool {
    let limit = LIMIT_TICKS.load(Ordering::Relaxed);
    if limit != 0 && crate::time::ticks().saturating_sub(LAST_PET.load(Ordering::Relaxed)) > limit {
        LIMIT_TICKS.store(0, Ordering::Relaxed);
        return true;
    }
    false
}

/// The boot CPU's timer interrupt: resets a machine whose service thread has stopped petting.
pub fn watchdog_check(frame: &mut Frame) {
    let limit = LIMIT_TICKS.load(Ordering::Relaxed);
    if limit != 0 && crate::time::ticks().saturating_sub(LAST_PET.load(Ordering::Relaxed)) > limit {
        LIMIT_TICKS.store(0, Ordering::Relaxed);
        frame.vector = WATCHDOG_VECTOR;
        crate::trap::fatal_reset(frame);
    }
}
