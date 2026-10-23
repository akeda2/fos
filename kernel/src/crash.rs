//! aarch64: what `debug.rs` + `config.rs` (the `debug` file) do on x86 -- the crash record, `reboot-on-crash`, the crash-loop
//! guard and the watchdog -- sized for the Pi.
//!
//! **The `debug` file** on the boot volume: `reboot-on-crash`, `reboot-delay N`, `crash-on-boot`, `watchdog [N]`,
//! `kernel-update`, the same lines as on x86.
//!
//! **The crash record** is a small text file, `crash`, on the boot volume: the fatal handler writes it (unless the fault hit in
//! the middle of a disk operation, when a write could damage the file system, so it stays silent) and the next boot prints and
//! deletes it. A RAM page was tried first and does not survive: the Pi's firmware clears RAM across a watchdog reset.
//!
//! **The watchdog** has two halves. The 100 Hz timer tick checks that the foreground has called [`pet`] within N seconds (ten
//! minutes while a tape runs) and, if not, records a crash at the interrupted instruction and resets. Behind it the BCM2711
//! power-management watchdog (PM_WDOG, 16 s at most) is fed by the tick once a second, so even a hang with interrupts masked
//! resets the Pi, just without a record. After three unclean boots in a row the watchdog stays off, as on x86.

use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use fosfor_abi::MAX_CONSECUTIVE_CRASHES;

use crate::{slots, storage, timer};

/// The record's "vector" for a watchdog reset (x86 uses the same number).
const WATCHDOG_VECTOR: u64 = 240;
const TAPE_LIMIT_TICKS: u64 = 10 * 60 * timer::TICK_HZ;
const GOOD_UPTIME_TICKS: u64 = 5 * 60 * timer::TICK_HZ;

const PM_BASE: u64 = 0xfe10_0000;
const PM_RSTC: u64 = PM_BASE + 0x1c;
const PM_WDOG: u64 = PM_BASE + 0x24;
const PM_PASSWORD: u32 = 0x5a00_0000;
const PM_RSTC_FULL_RESET: u32 = 0x20;
const PM_RSTC_CONFIG_MASK: u32 = 0x30;
/// 15 s in the watchdog's 1/65536 s ticks (the field is 20 bits: 16 s at most).
const HARDWARE_TIMEOUT: u32 = 15 * 65536;

pub struct Debug {
    pub reboot_on_crash: bool,
    pub reboot_delay_seconds: u32,
    pub crash_on_boot: bool,
    pub watchdog_seconds: Option<u32>,
    pub kernel_update: bool,
}

static mut DEBUG: Debug = Debug { reboot_on_crash: false, reboot_delay_seconds: 3, crash_on_boot: false, watchdog_seconds: None, kernel_update: false };
static DISK_BUSY: AtomicBool = AtomicBool::new(false);
static mut BOOT_CRASH_COUNT: u32 = 0;
static mut UNCLEAN_BOOTS: u8 = 0;
static mut WATCHDOG_TICKS: u64 = 0;
static mut TAPE_RUNNING: bool = false;
static mut HARDWARE_ARMED: bool = false;
static mut STREAK_CLEARED: bool = false;
static mut STREAK_PENDING: bool = false;
static LAST_PET: AtomicU64 = AtomicU64::new(0);
static LAST_FEED: AtomicU64 = AtomicU64::new(0);

pub fn debug() -> &'static Debug {
    unsafe { &*core::ptr::addr_of!(DEBUG) }
}

/// Right after `memory::init`: remembers the loader's crash count.
pub fn set_boot_crash_count(crash_count: u32) {
    unsafe { BOOT_CRASH_COUNT = crash_count };
}

/// Held by `block::read`/`write`: a fault while it lives must not write the crash file.
pub struct DiskBusy;

pub fn disk_busy() -> DiskBusy {
    pet();
    DISK_BUSY.store(true, Ordering::Relaxed);
    DiskBusy
}

impl Drop for DiskBusy {
    fn drop(&mut self) {
        DISK_BUSY.store(false, Ordering::Relaxed);
    }
}

/// Once the boot volume is mounted: reads the `debug` file and reports (then clears) the previous boot's crash record.
pub fn init() {
    unsafe { UNCLEAN_BOOTS = slots::read_metadata().map_or(0, |metadata| metadata.unclean_boots) };
    report_previous();
    let Some(volume) = storage::boot_volume() else { return };
    let mut buffer = [0u8; 1024];
    let Ok(Some(length)) = volume.read(b"debug", &mut buffer) else { return };
    let debug = unsafe { &mut *core::ptr::addr_of_mut!(DEBUG) };
    for line in core::str::from_utf8(&buffer[..length]).unwrap_or("").lines() {
        let mut words = line.split_whitespace();
        let number = |word: Option<&str>, default: u32| word.and_then(|word| word.parse().ok()).unwrap_or(default);
        match words.next() {
            Some("reboot-on-crash") => debug.reboot_on_crash = true,
            Some("reboot-delay") => debug.reboot_delay_seconds = number(words.next(), 3).min(60),
            Some("crash-on-boot") => debug.crash_on_boot = true,
            Some("watchdog") => debug.watchdog_seconds = Some(number(words.next(), 60)),
            Some("kernel-update") => debug.kernel_update = true,
            Some(_) if line.trim().is_empty() || line.trim().starts_with('#') => {}
            Some(_) => crate::println!("debug: unknown line `{line}` ignored."),
            None => {}
        }
    }
    crate::println!(
        "debug file: reboot-on-crash {}, reboot-delay {} s, crash-on-boot {}, watchdog {:?}, kernel-update {}.",
        debug.reboot_on_crash, debug.reboot_delay_seconds, debug.crash_on_boot, debug.watchdog_seconds, debug.kernel_update
    );
}

fn report_previous() {
    let Some(volume) = storage::boot_volume() else { return };
    let mut buffer = [0u8; 256];
    let Ok(Some(length)) = volume.read(b"crash", &mut buffer) else { return };
    crate::println!("Previous boot crashed (file `crash`, now removed):");
    for line in core::str::from_utf8(&buffer[..length]).unwrap_or("?").lines() {
        crate::println!("  {line}");
    }
    let _ = volume.remove(b"crash");
}

struct Text {
    bytes: [u8; 256],
    length: usize,
}

impl Write for Text {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        let take = text.len().min(self.bytes.len() - self.length);
        self.bytes[self.length..self.length + take].copy_from_slice(&text.as_bytes()[..take]);
        self.length += take;
        Ok(())
    }
}

fn record_crash(kind: u64, esr: u64, elr: u64, far: u64) {
    if DISK_BUSY.load(Ordering::Relaxed) {
        crate::println!("(crash record not written: the fault hit during a disk operation)");
        return;
    }
    let Some(volume) = storage::boot_volume() else { return };
    let mut text = Text { bytes: [0; 256], length: 0 };
    if kind == WATCHDOG_VECTOR {
        let _ = writeln!(text, "watchdog: no progress, the CPU was at {elr:#018x}");
    } else {
        let _ = writeln!(text, "exception {kind}: ESR {esr:#010x} ELR {elr:#018x} FAR {far:#018x}");
    }
    let _ = writeln!(text, "slot {}, {} ticks after boot", storage::boot_slot(), timer::ticks());
    if volume.write(b"crash", &text.bytes[..text.length]).is_ok() {
        crate::println!("Crash record written to the file `crash`.");
    }
}

pub fn crash_loop_tripped() -> bool {
    unsafe { BOOT_CRASH_COUNT >= u32::from(MAX_CONSECUTIVE_CRASHES) }
}

/// The milestone has been reached: `crash-on-boot` is honoured just before it by the caller; this arms the watchdog.
pub fn confirm() {
    pet();
    unsafe {
        if UNCLEAN_BOOTS > 0 {
            STREAK_PENDING = true;
        }
        if let Some(seconds) = debug().watchdog_seconds {
            if UNCLEAN_BOOTS >= 3 {
                crate::println!("Watchdog stays off: {} unclean boots in a row (a hang must not become a reboot loop).", { UNCLEAN_BOOTS });
                crate::println!("It comes back on the next boot once this one has run five minutes.");
            } else {
                WATCHDOG_TICKS = u64::from(seconds.max(5)) * timer::TICK_HZ;
                arm_hardware();
                crate::println!("Watchdog armed: reset after {} s without progress.", seconds.max(5));
            }
        }
    }
}

fn is_pi4() -> bool {
    crate::gic::bases().0 == 0xff84_1000
}

fn arm_hardware() {
    if !is_pi4() {
        return;
    }
    crate::paging::map_device(PM_BASE, 0x1000);
    unsafe {
        feed_hardware();
        let current = core::ptr::read_volatile(PM_RSTC as *const u32);
        core::ptr::write_volatile(PM_RSTC as *mut u32, PM_PASSWORD | (current & !PM_RSTC_CONFIG_MASK) | PM_RSTC_FULL_RESET);
        HARDWARE_ARMED = true;
    }
}

unsafe fn feed_hardware() {
    unsafe { core::ptr::write_volatile(PM_WDOG as *mut u32, PM_PASSWORD | HARDWARE_TIMEOUT) };
}

/// Stops the hardware watchdog from resetting a machine that is deliberately halted for an operator.
fn disarm_hardware() {
    unsafe {
        if HARDWARE_ARMED {
            HARDWARE_ARMED = false;
            let current = core::ptr::read_volatile(PM_RSTC as *const u32);
            core::ptr::write_volatile(PM_RSTC as *mut u32, PM_PASSWORD | (current & !PM_RSTC_CONFIG_MASK));
        }
    }
}

/// The foreground made progress. Cheap: call it from every wait loop.
pub fn pet() {
    LAST_PET.store(timer::ticks(), Ordering::Relaxed);
}

/// A tape starts or stops (it gets a longer limit while it runs).
pub fn set_tape_running(running: bool) {
    unsafe { TAPE_RUNNING = running };
    pet();
}

/// From the shell loop: clears the unclean-boot streak once this boot has run long enough to be called good.
pub fn housekeeping() {
    unsafe {
        if STREAK_PENDING && !STREAK_CLEARED && timer::ticks() > GOOD_UPTIME_TICKS {
            STREAK_CLEARED = true;
            let _ = slots::update_metadata(|metadata| metadata.unclean_boots = 0);
        }
    }
}

/// Every timer tick (IRQ context): checks the foreground's progress and feeds the hardware watchdog once a second.
pub fn tick() {
    unsafe {
        if WATCHDOG_TICKS == 0 {
            return;
        }
        let limit = if TAPE_RUNNING { TAPE_LIMIT_TICKS.max(WATCHDOG_TICKS) } else { WATCHDOG_TICKS };
        let now = timer::ticks();
        if now.wrapping_sub(LAST_PET.load(Ordering::Relaxed)) > limit {
            let elr = crate::arch::aarch64::exceptions::elr();
            crate::println!("\nWatchdog: no progress for {} s; the CPU was at {:#x}.", limit / timer::TICK_HZ, elr);
            fatal(WATCHDOG_VECTOR, 0, elr, 0, true);
        }
        if HARDWARE_ARMED && now.wrapping_sub(LAST_FEED.load(Ordering::Relaxed)) >= timer::TICK_HZ {
            LAST_FEED.store(now, Ordering::Relaxed);
            feed_hardware();
        }
    }
}

/// A fatal exception (or the watchdog): records it, then resets after `reboot-delay` if `reboot-on-crash` is set (always for
/// the watchdog), unless the crash-loop guard says the loader saw too many boots that never reached the milestone.
pub fn fatal(kind: u64, esr: u64, elr: u64, far: u64, always_reset: bool) -> ! {
    crate::cpu::interrupts_disable();
    crate::sdhci::force_polling();
    record_crash(kind, esr, elr, far);
    let settings = debug();
    if settings.reboot_on_crash || always_reset {
        if crash_loop_tripped() {
            crate::println!("*** crash loop: {} boots in a row never reached the milestone; not resetting again. Halting for an operator.", unsafe { BOOT_CRASH_COUNT });
        } else {
            crate::println!("*** resetting in {} s", settings.reboot_delay_seconds);
            timer::busy_wait_ms(u64::from(settings.reboot_delay_seconds) * 1000);
            crate::arch::aarch64::power::crash_reset();
        }
    }
    disarm_hardware();
    crate::println!("*** halting");
    loop {
        unsafe { core::arch::asm!("wfe") };
    }
}
