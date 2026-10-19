//! Unattended, remote-friendly operation (milestone M6): the A/B slot metadata, the crash
//! record, the crash-loop guard and the software watchdog. Everything the v1 "debug
//! mode" does, redone for the UEFI boot path.
//!
//! **Slot metadata** (payload sector 0, `fosfor_abi::SlotMetadata`) is shared with the
//! loader. The loader runs the A/B fallback state machine and marks a boot `running = 1`
//! (booting); this module sets `running = 2` at the kernel's *milestone*
//! ([`confirm_boot`]: the point where a person or a remote client could use the machine),
//! confirms a pending kernel, and zeroes the crash count. A clean stop (`shutdown`,
//! `reset`, `reboot`, a kernel update restart) sets `running = 0` first. So at the next
//! boot the loader can tell "crashed before the milestone" (counts toward the crash-loop
//! guard) from "stopped without notice later" (counts toward the watchdog's guard).
//!
//! **The crash record** is one page of RAM at a fixed physical address, written by the
//! fatal-exception handler and read back (and cleared) by the *next* boot, so an operator
//! sees what the last fatal exception looked like without having watched it. RAM does not
//! survive a reset on every machine; then there is simply nothing to report.
//!
//! **The watchdog** is the timer tick checking that the foreground pets it
//! ([`pet`]) within N seconds; if not, the tick records a crash (vector 240 with the RIP
//! the CPU was stuck at) and resets the machine, so a hang leaves evidence and recovers by
//! itself. A running tape gets ten minutes instead. After three unclean boots in a row it
//! stays off (a hang loop must not become a reboot loop), and a boot that runs five
//! minutes clears that streak.

use core::sync::atomic::{AtomicU64, Ordering};

use fosfor_abi::{BootInfo, MAX_CONSECUTIVE_CRASHES, RUNNING_STOPPED, RUNNING_UP, SlotMetadata};

use crate::{block, fault::Frame, paging, timer};

/// Physical address of the crash record page (low memory, below the kernel's own
/// allocations; reserved from the allocator in `init`).
pub const CRASH_RECORD_ADDRESS: u64 = 0x5000;
const CRASH_MAGIC: u64 = u64::from_le_bytes(*b"FOSCRASH");
pub const WATCHDOG_VECTOR: u64 = 240;
/// A tape may run this long between callbacks before the watchdog suspects a hang.
const TAPE_LIMIT_TICKS: u64 = 10 * 60 * timer::TICK_HZ;
const GOOD_UPTIME_TICKS: u64 = 5 * 60 * timer::TICK_HZ;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CrashRecord {
    pub magic: u64,
    pub vector: u64,
    pub error: u64,
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
    pub ticks: u64,
    pub boot_slot: u64,
    pub registers: [u64; 8], // rax rbx rcx rdx rsi rdi rbp r8
    /// CR2: the address a page fault was about.
    pub cr2: u64,
}

static mut PAYLOAD: Option<(usize, u64)> = None; // (disk, first sector of the payload partition)
static mut BOOT_SLOT: u32 = 0;
static mut BOOT_CRASH_COUNT: u32 = 0;
static mut RECORD_USABLE: bool = false;
static mut UNCLEAN_BOOTS: u8 = 0;
static mut CONFIRMED: bool = false;

static LAST_PET: AtomicU64 = AtomicU64::new(0);
static mut WATCHDOG_TICKS: u64 = 0; // 0 = off
static mut TAPE_RUNNING: bool = false;
static mut STREAK_CLEARED: bool = false;
static mut CLEAR_STREAK_PENDING: bool = false;

pub fn init(info: &BootInfo) {
    unsafe {
        BOOT_SLOT = info.boot_slot;
        BOOT_CRASH_COUNT = info.crash_count;
        // The record page must be ordinary mapped RAM; if the firmware's map does not
        // have it (or it is not in the map), crash records are simply off.
        memory_reserve_record_page();
        RECORD_USABLE = paging::is_mapped(CRASH_RECORD_ADDRESS);
        crate::println!("Crash record: {} at physical {:#x} (survives a reset only if the firmware leaves RAM alone).", if RECORD_USABLE { "RAM page reserved" } else { "page not mapped, so no RAM record" }, CRASH_RECORD_ADDRESS);
    }
}

fn memory_reserve_record_page() {
    crate::memory::reserve(CRASH_RECORD_ADDRESS, 4096);
}

/// Tells this module where the payload partition is (storage found it).
pub fn set_payload(disk: usize, start: u64) {
    unsafe { PAYLOAD = Some((disk, start)) };
}

// ---- slot metadata ----

/// Why the slot-metadata sector could not be used.
pub enum MetadataProblem {
    /// The payload partition was never located (no boot disk found).
    NoPayload,
    /// The disk driver failed the read.
    Read(block::Error),
    /// The sector was read but is not valid metadata: its first bytes, for the operator to compare with `"FOSM"` + version 1.
    Invalid([u8; 16]),
}

pub fn read_metadata_checked() -> Result<SlotMetadata, MetadataProblem> {
    let (disk, start) = unsafe { PAYLOAD }.ok_or(MetadataProblem::NoPayload)?;
    let mut sector = [0u8; 512];
    block::read(disk, start, &mut sector).map_err(MetadataProblem::Read)?;
    SlotMetadata::parse(&sector).ok_or_else(|| {
        let mut head = [0u8; 16];
        head.copy_from_slice(&sector[..16]);
        MetadataProblem::Invalid(head)
    })
}

pub fn read_metadata() -> Option<SlotMetadata> {
    read_metadata_checked().ok()
}

pub fn write_metadata(metadata: &SlotMetadata) -> bool {
    let Some((disk, start)) = (unsafe { PAYLOAD }) else { return false };
    let mut sector = [0u8; 512];
    metadata.write_into(&mut sector);
    block::write(disk, start, &sector).is_ok()
}

/// Read-modify-write of the metadata sector.
pub fn update_metadata(change: impl FnOnce(&mut SlotMetadata)) -> Option<SlotMetadata> {
    let mut metadata = read_metadata()?;
    change(&mut metadata);
    write_metadata(&metadata).then_some(metadata)
}

/// The kernel's milestone: it can be used now. Confirms a pending kernel (promoting it
/// at once), clears the crash streak, marks the boot as up, and starts the watchdog if
/// the `debug` file asked for one.
pub fn confirm_boot(watchdog_seconds: Option<u32>) {
    let boot_slot = unsafe { BOOT_SLOT } as u8;
    let updated = update_metadata(|metadata| {
        if metadata.pending_slot == Some(boot_slot) && metadata.attempted {
            crate::println!("Kernel update confirmed: slot {boot_slot} reached its milestone and is now the active slot.");
            metadata.active_slot = boot_slot;
            (metadata.pending_slot, metadata.attempted, metadata.confirmed) = (None, false, false);
        }
        metadata.crash_count = 0;
        metadata.running = RUNNING_UP;
        unsafe { UNCLEAN_BOOTS = metadata.unclean_boots };
    });
    match updated {
        Some(_) => unsafe { CONFIRMED = true },
        None => crate::println!("Slot metadata could not be updated: no A/B fallback or crash tracking this boot."),
    }
    pet();
    // The streak clears after a good run whether or not the watchdog is armed: with three or more unclean boots it stays
    // off, so tying the clearing to the watchdog left the streak (and the disabled watchdog) stuck for ever. Power-button
    // stops count as unclean, so a machine that is simply switched off now and then reaches three easily.
    let unclean = unsafe { UNCLEAN_BOOTS };
    if unclean > 0 {
        unsafe { CLEAR_STREAK_PENDING = true };
    }
    if let Some(seconds) = watchdog_seconds {
        if unclean >= 3 {
            crate::println!("Watchdog stays off: {unclean} unclean boots in a row (a hang must not become a reboot loop).");
            crate::println!("It comes back on the next boot once this one has run five minutes.");
        } else {
            unsafe { WATCHDOG_TICKS = u64::from(seconds.max(5)) * timer::TICK_HZ };
            crate::println!("Watchdog armed: reset after {} s without progress.", seconds.max(5));
        }
    }
}

/// Marks the stop as deliberate, so the next boot does not count it as a crash.
pub fn mark_clean_stop() {
    let _ = update_metadata(|metadata| metadata.running = RUNNING_STOPPED);
}

/// Called from the main loop: clears the unclean-boot streak once the boot has run long
/// enough to be called good.
pub fn housekeeping() {
    unsafe {
        if CLEAR_STREAK_PENDING && !STREAK_CLEARED && timer::ticks() > GOOD_UPTIME_TICKS {
            STREAK_CLEARED = true;
            let _ = update_metadata(|metadata| metadata.unclean_boots = 0);
        }
    }
}

pub fn slot_summary(print: impl Fn(core::fmt::Arguments)) {
    print(format_args!("booted slot: {}", unsafe { BOOT_SLOT }));
    match read_metadata_checked().map_err(|problem| problem) {
        Ok(m) => {
            print(format_args!("active slot: {}, pending: {:?}, attempted: {}, confirmed: {}", m.active_slot, m.pending_slot, m.attempted, m.confirmed));
            print(format_args!("slot sizes (sectors): {} / {}", m.slot_sectors[0], m.slot_sectors[1]));
            print(format_args!("BIOS video mode switch: {} (`debug video skip|auto`), monitor size hint: {} (`debug video size WxH|auto`)",
                if m.flags & fosfor_abi::FLAG_SKIP_VIDEO != 0 { "skipped" } else { "automatic" },
                if m.monitor_hint == (0, 0) { "none" } else { "set" }));
            if m.monitor_hint != (0, 0) {
                print(format_args!("  hint: {}x{}", m.monitor_hint.0, m.monitor_hint.1));
            }
            print(format_args!("crash count: {} (loader saw {} at boot), running: {}, unclean boots: {}", m.crash_count, unsafe { BOOT_CRASH_COUNT }, m.running, m.unclean_boots));
        }
        Err(MetadataProblem::NoPayload) => print(format_args!("slot metadata: not readable (the payload partition was never located)")),
        Err(MetadataProblem::Read(error)) => print(format_args!("slot metadata: not readable (the disk read failed: {error:?})")),
        Err(MetadataProblem::Invalid(head)) => {
            print(format_args!("slot metadata: not readable (the sector is not valid metadata; `sector disk1 N` shows it)"));
            print(format_args!("  it starts: {:02x?}   wanted: 46 4f 53 4d 01 (\"FOSM\", version 1)", head));
        }
    }
    let ticks = unsafe { WATCHDOG_TICKS };
    if ticks == 0 {
        print(format_args!("watchdog: off"));
    } else {
        print(format_args!("watchdog: {} s", ticks / timer::TICK_HZ));
    }
}

// ---- the crash record ----

/// Writes the record for a fatal exception (safe from inside a fault handler: no
/// allocation, one page of already-mapped RAM).
pub fn record_crash(frame: &Frame) {
    if !unsafe { RECORD_USABLE } {
        return;
    }
    let record = CrashRecord {
        magic: CRASH_MAGIC,
        vector: frame.vector,
        error: frame.error,
        rip: frame.rip,
        cs: frame.cs,
        rflags: frame.rflags,
        rsp: frame.rsp,
        ss: frame.ss,
        ticks: timer::ticks(),
        boot_slot: u64::from(unsafe { BOOT_SLOT }),
        registers: [frame.rax, frame.rbx, frame.rcx, frame.rdx, frame.rsi, frame.rdi, frame.rbp, frame.r8],
        cr2: crate::cpu::read_cr2(),
    };
    unsafe { (CRASH_RECORD_ADDRESS as *mut CrashRecord).write_volatile(record) };
}

/// At boot: prints the previous boot's crash record, if there is a valid one, and clears it.
pub fn report_previous_crash() {
    if !unsafe { RECORD_USABLE } {
        return;
    }
    let record = unsafe { (CRASH_RECORD_ADDRESS as *const CrashRecord).read_volatile() };
    if record.magic != CRASH_MAGIC {
        return;
    }
    unsafe { (CRASH_RECORD_ADDRESS as *mut u64).write_volatile(0) };
    crate::println!("Previous boot crashed (record found in RAM):");
    crate::println!(
        "  {} (vector {}), error code {:#x}, slot {}, {} ticks after boot",
        crate::fault::exception_name(record.vector),
        record.vector,
        record.error,
        record.boot_slot,
        record.ticks
    );
    crate::println!("  RIP {:#018x}  CS {:#x}  RFLAGS {:#x}  RSP {:#018x}  SS {:#x}  CR2 {:#x}", record.rip, record.cs, record.rflags, record.rsp, record.ss, record.cr2);
    crate::println!(
        "  RAX {:#x} RBX {:#x} RCX {:#x} RDX {:#x} RSI {:#x} RDI {:#x} RBP {:#x} R8 {:#x}",
        record.registers[0], record.registers[1], record.registers[2], record.registers[3], record.registers[4], record.registers[5], record.registers[6], record.registers[7]
    );
}

/// Whether the crash-loop guard says "stop rebooting": the loader counted this many
/// consecutive boots that never reached the milestone.
pub fn crash_loop_tripped() -> bool {
    unsafe { BOOT_CRASH_COUNT >= u32::from(MAX_CONSECUTIVE_CRASHES) }
}

pub fn boot_crash_count() -> u32 {
    unsafe { BOOT_CRASH_COUNT }
}

// ---- the watchdog ----

/// The foreground made progress. Cheap: call it from every wait loop.
pub fn pet() {
    LAST_PET.store(timer::ticks(), Ordering::Relaxed);
}

/// A tape starts or stops running (it gets a longer limit while running).
pub fn set_tape_running(running: bool) {
    unsafe { TAPE_RUNNING = running };
    pet();
}

/// Called from every timer tick with the interrupted frame: fires if the foreground has
/// not petted the watchdog in time.
pub fn watchdog_tick(frame: &mut Frame) {
    let limit = unsafe { if TAPE_RUNNING { TAPE_LIMIT_TICKS.max(WATCHDOG_TICKS) } else { WATCHDOG_TICKS } };
    if limit == 0 || unsafe { WATCHDOG_TICKS } == 0 {
        return;
    }
    if timer::ticks().wrapping_sub(LAST_PET.load(Ordering::Relaxed)) > limit {
        crate::println!("\nWatchdog: no progress for {} s; the CPU was at RIP {:#x}.", limit / timer::TICK_HZ, frame.rip);
        frame.vector = WATCHDOG_VECTOR;
        frame.error = 0;
        crate::fault::fatal_reset(frame);
    }
}
