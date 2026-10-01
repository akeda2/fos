//! Debug mode: unattended, remote-friendly development on real hardware.
//!
//! Three pieces live here, all controlled by an optional `debug` file (see
//! [`load_config`]) so an ordinary deployment behaves exactly as it always
//! has when the file is absent:
//!
//! - [`SlotMetadata`]: the kernel-side half of the A/B kernel-slot protocol
//!   `boot/stage2.S`'s `select_kernel_slot` implements on the read/fallback
//!   side. This module owns the one place that writes it: [`confirm_boot`]
//!   (called once, from `shell::run`'s main loop, to promote a pending
//!   update that reached a real, working shell) and the network
//!   kernel-update service (writes a fresh `pending_slot`, to be added).
//! - A `CrashRecord` persisted at a fixed low-memory address ([`CRASH_RECORD_ADDR`])
//!   across a reboot-on-crash, so the *next* boot can print what the *last*
//!   boot's fatal exception looked like -- the single most useful thing an
//!   operator debugging a real-hardware-only crash from another room wants
//!   to see.
//! - [`DebugConfig`]: parsed once at boot from the optional `debug` file,
//!   controlling whether a fatal exception reboots (instead of halting
//!   forever) and how long to wait first.
//!
//! All three intentionally share this one module rather than being spread
//! across `shell.rs`/`interrupts.rs`/`gdt.rs`, since they are one feature
//! with one on-disk/in-memory ABI that must stay internally consistent.

use crate::boot_info::BootInfo;
use crate::disk;
use crate::fosfs;
use crate::println;

// `METADATA_LBA`/`SLOT0_LBA`/`SLOT1_LBA`/`SLOT_SECTORS`, `include!()`-ed
// from a file `tools/write_layout.py --rust-slots` generates fresh before
// every kernel build (see the `kernel` `Makefile` target) -- the exact
// same values, computed by the exact same code, that `boot/stage1.S`/
// `boot/stage2.S` get from that script's other output, `build/layout.inc`.
//
// This used to be four hand-copied constants instead, with a doc comment
// (now folded into this one, for the historical record) warning that nothing
// cross-checked them against the loader's own generated values, and that
// a missed manual update after any `KERNEL_RESERVED_END` change would fail
// *silently*, not loudly: `boot/stage2.S` would keep reading a slot back
// from its own correct, freshly generated LBA, while `install_kernel_update`
// below kept *writing* a KUP-pushed kernel to whatever stale LBA was
// hardcoded here -- silently installing it into the wrong disk sectors.
// That warning was not hypothetical: it happened twice. First, stuck at
// the pre-audio-ADC value (`SLOT_SECTORS` 640/`SLOT1_LBA` 2721) after
// `KERNEL_RESERVED_END` grew from `0x60000` to `0x70000` for ES1370 ADC
// capture, causing a real, fully deterministic soft-reboot-after-KUP
// crash (the loader read slot 1 starting 128 sectors into what KUP had
// actually written there, landing a `#UD`-inducing wrong instruction
// stream at the kernel's own entry point) -- fixed at the time by manually
// updating `SLOT1_LBA`/`SLOT_SECTORS` to `2977`/`896`. Second, stuck at
// exactly those `0x80000`-era values after a later, undocumented growth to
// the current `0x98000` went unnoticed until this comment's own
// investigation found it -- discovered before it caused a second crash,
// but only by luck of the current kernel size never yet exceeding what the
// stale, too-small `SLOT_SECTORS` would still have accepted as valid.
// `include!()`-ing a generated file instead of hand-copying, exactly like
// `boot/stage1.S`/`boot/stage2.S` already do, makes this specific class of
// drift structurally impossible rather than merely documented against a
// third time.
include!("generated_slots.rs");

const METADATA_MAGIC: [u8; 4] = *b"FOSM";
const METADATA_VERSION: u8 = 1;
const NO_PENDING_SLOT: u8 = 0xff;

/// Consecutive un-confirmed reboot-on-crash cycles allowed before this
/// module gives up rebooting and halts instead -- the loop-guard the plan
/// called for, so a kernel that crashes immediately after every reset (a
/// bug reboot-on-crash itself cannot fix) does not spin forever, chewing
/// power and wearing out a real machine's hardware unattended. Persisted on
/// disk (metadata byte 9, `crash_count`), not in RAM, so it survives even
/// on hardware where a soft reset happens to clear memory.
const MAX_CONSECUTIVE_CRASHES: u8 = 5;

/// The kernel-side view of the one-sector A/B slot metadata block written
/// by `tools/build_image.py` at build time and read/updated by both
/// `boot/stage2.S` (at every boot) and this module (on milestone-confirm
/// and, eventually, a network kernel update). Byte layout (must match both
/// of those exactly):
///
/// ```text
/// +0..4   magic "FOSM"
/// +4      version (1)
/// +5      active_slot (0 or 1)
/// +6      pending_slot (0xff = none, else 0/1)
/// +7      attempted (has the pending slot been booted at least once)
/// +8      confirmed (did that boot reach shell::run's main loop)
/// +9      crash_count (consecutive un-confirmed reboot-on-crash cycles)
/// +10..12 slot0_sectors (u16 LE)
/// +12..14 slot1_sectors (u16 LE)
/// +14..512 reserved/zero
/// ```
#[derive(Clone, Copy, Debug)]
pub struct SlotMetadata {
    pub active_slot: u8,
    pub pending_slot: Option<u8>,
    pub attempted: bool,
    pub confirmed: bool,
    pub crash_count: u8,
    pub slot_sectors: [u16; 2],
}

impl SlotMetadata {
    /// Reads the metadata sector, returning `None` on a bad magic/version or
    /// a BIOS read failure -- the same "fall back to defaults" tolerance
    /// `select_kernel_slot` already applies, so a pre-feature image or a
    /// genuinely corrupted sector never blocks anything in this module.
    pub fn read(info: &BootInfo) -> Option<Self> {
        disk::device_read_sector(info, disk::Device::Boot, METADATA_LBA, |sector| {
            if sector[0..4] != METADATA_MAGIC || sector[4] != METADATA_VERSION {
                return None;
            }
            let pending = sector[6];
            Some(SlotMetadata {
                active_slot: sector[5],
                pending_slot: if pending == NO_PENDING_SLOT {
                    None
                } else {
                    Some(pending)
                },
                attempted: sector[7] != 0,
                confirmed: sector[8] != 0,
                crash_count: sector[9],
                slot_sectors: [
                    u16::from_le_bytes([sector[10], sector[11]]),
                    u16::from_le_bytes([sector[12], sector[13]]),
                ],
            })
        })
        .ok()
        .flatten()
    }

    /// Writes the metadata sector back out. Every reserved byte is
    /// deterministically zeroed rather than left as whatever the shared
    /// sector-scratch buffer happened to hold before this call, since
    /// `disk::device_write_sector` hands the closure that buffer as-is with
    /// no zeroing of its own (unlike the multi-sector `device_write_sectors`
    /// helper).
    pub fn write(&self, info: &BootInfo) -> Result<(), disk::Error> {
        disk::device_write_sector(info, disk::Device::Boot, METADATA_LBA, |sector| {
            sector.fill(0);
            sector[0..4].copy_from_slice(&METADATA_MAGIC);
            sector[4] = METADATA_VERSION;
            sector[5] = self.active_slot;
            sector[6] = self.pending_slot.unwrap_or(NO_PENDING_SLOT);
            sector[7] = self.attempted as u8;
            sector[8] = self.confirmed as u8;
            sector[9] = self.crash_count;
            sector[10..12].copy_from_slice(&self.slot_sectors[0].to_le_bytes());
            sector[12..14].copy_from_slice(&self.slot_sectors[1].to_le_bytes());
        })
    }

    /// LBA of the slot this metadata calls `slot` (0 or 1); any other value
    /// is treated as slot 0, matching `select_kernel_slot`'s own tolerance
    /// for an out-of-range slot number.
    pub fn slot_lba(slot: u8) -> u64 {
        if slot == 1 {
            SLOT1_LBA
        } else {
            SLOT0_LBA
        }
    }
}

/// Called once, at the top of `shell::run`'s main loop, after every other
/// piece of boot-time initialization has succeeded -- the "milestone" the
/// plan settled on, since it is exactly the class of real-hardware-only
/// boot bug (see AGENTS.md's "Real hardware notes") that reboot-on-crash and
/// A/B slots exist to make survivable without physical intervention.
///
/// If `info.booted_slot` is the metadata's still-unconfirmed `pending_slot`,
/// this promotes it (`active_slot = pending_slot`, clears
/// `pending_slot`/`attempted`/`confirmed`) so the *next* boot uses this slot
/// unconditionally, with no more fallback risk. If a stale `pending_slot`
/// somehow does not match the slot we actually booted (stage2 already fell
/// back to `active_slot` for us, per its own state machine), this just
/// clears the stale marker instead of promoting anything. Either way,
/// `crash_count` is reset to 0: reaching a real shell proves this boot
/// was not part of a crash loop, regardless of slot state.
pub fn confirm_boot(info: &BootInfo) {
    let Some(mut metadata) = SlotMetadata::read(info) else {
        return;
    };
    let already_clean = metadata.pending_slot.is_none() && metadata.crash_count == 0;
    if already_clean {
        return;
    }
    if let Some(pending) = metadata.pending_slot {
        if info.booted_slot == pending {
            metadata.active_slot = pending;
        }
        metadata.pending_slot = None;
        metadata.attempted = false;
        metadata.confirmed = false;
    }
    metadata.crash_count = 0;
    if let Err(error) = metadata.write(info) {
        println!("debug: could not confirm this boot on disk: {error:?}");
    }
}

const KERNEL_UPDATE_SECTOR_SIZE: usize = 512;

/// Returns the slot number a network kernel update should write into: the
/// one *not* currently `active_slot`, so the presently-running/known-good
/// kernel is never overwritten until the new one has actually proven
/// itself via [`confirm_boot`]. Any `active_slot` value other than 0 is
/// treated as 1, mirroring `select_kernel_slot`'s/`slot_lba`'s own
/// tolerance for an out-of-range slot number.
pub fn inactive_slot(metadata: &SlotMetadata) -> u8 {
    if metadata.active_slot == 0 { 1 } else { 0 }
}

/// Writes an already checksum-verified kernel image into slot `slot`'s
/// reserved sectors -- in [`disk::MAX_TRANSFER_SECTORS`]-sized chunks, the
/// same bounded-transfer discipline `fosfs.rs`'s own multi-sector loops
/// already use -- then records `pending_slot = slot` with
/// `attempted`/`confirmed` both cleared, the exact state a genuinely fresh,
/// never-yet-booted update needs so `select_kernel_slot`'s existing
/// fallback state machine gives it one real try before falling back.
/// `data.len()` must fit within [`SLOT_SECTORS`] sectors; the caller (the
/// network kernel-update service) is expected to have already rejected an
/// oversized image before ever staging it, so this only re-checks it as a
/// defensive bound, not the primary size gate.
pub fn install_kernel_update(info: &BootInfo, slot: u8, data: &[u8]) -> Result<(), disk::Error> {
    let sector_count = data.len().div_ceil(KERNEL_UPDATE_SECTOR_SIZE);
    if sector_count as u64 > SLOT_SECTORS {
        return Err(disk::Error::OutOfRange);
    }
    let lba = SlotMetadata::slot_lba(slot);
    let mut written_sectors = 0usize;
    while written_sectors < sector_count {
        let chunk_sectors = (sector_count - written_sectors).min(disk::MAX_TRANSFER_SECTORS);
        let byte_offset = written_sectors * KERNEL_UPDATE_SECTOR_SIZE;
        let chunk_bytes = (chunk_sectors * KERNEL_UPDATE_SECTOR_SIZE).min(data.len() - byte_offset);
        disk::device_write_sectors(
            info,
            disk::Device::Boot,
            lba + written_sectors as u64,
            chunk_sectors,
            |region| {
                region[..chunk_bytes].copy_from_slice(&data[byte_offset..byte_offset + chunk_bytes]);
            },
        )?;
        written_sectors += chunk_sectors;
    }
    let Some(mut metadata) = SlotMetadata::read(info) else {
        return Err(disk::Error::Unavailable);
    };
    metadata.pending_slot = Some(slot);
    metadata.attempted = false;
    metadata.confirmed = false;
    metadata.slot_sectors[slot as usize] = sector_count as u16;
    metadata.write(info)
}

/// A crash-record structure written to fixed physical memory by
/// `record_crash` at the moment of a fatal exception (see
/// `interrupts::report_exception`/`gdt::double_fault_task`), so the
/// *following* boot can print what happened without needing a screen or
/// serial log actually being watched at the moment of the crash.
///
/// This is plain RAM, not disk: writing it must be safe from inside a
/// fault handler with no allocator and (for a double fault) an already
/// exhausted normal stack. It only survives a reset that happens to leave
/// RAM contents intact -- true of every real chipset and QEMU this project
/// has been tested against, but not a guarantee this module can make; if a
/// particular machine's reset path does clear RAM, the crash record is
/// simply empty on the following boot, same as if none had ever crashed.
/// `crash_count` above (persisted on disk, not here) is the loop-guard's
/// actual safety net either way.
pub const CRASH_RECORD_ADDR: usize = 0x5500;
const CRASH_RECORD_MAGIC: u32 = 0x4841_5343; // "CSAH" read little-endian as bytes b"CSAH"
const CRASH_RECORD_VERSION: u16 = 1;

#[repr(C, packed)]
pub struct CrashRecord {
    pub magic: u32,
    pub version: u16,
    pub vector: u8,
    pub has_error_code: u8,
    pub error_code: u32,
    pub code_segment: u32,
    pub instruction_pointer: u32,
    pub eflags: u32,
    pub stack_segment: u32,
    pub stack_pointer: u32,
    /// General-purpose registers, only populated for a double fault (vector
    /// 8), where the CPU's own hardware task switch saves them into the
    /// normal TSS for us (see `gdt::last_task_switch_registers`). An
    /// ordinary exception handler here uses `extern "x86-interrupt"`, which
    /// never exposes the interrupted code's GP registers at all, so those
    /// four fields are simply zero for every other vector.
    pub eax: u32,
    pub ebx: u32,
    pub ecx: u32,
    pub edx: u32,
    pub esi: u32,
    pub edi: u32,
    pub ebp: u32,
    pub has_registers: u8,
    pub booted_slot: u8,
    pub _reserved: [u8; 2],
    pub ticks: u64,
}

/// Registers captured from the normal TSS by `gdt::last_task_switch_registers`
/// (only meaningful right after a double fault's hardware task switch).
pub struct SavedRegisters {
    pub eax: u32,
    pub ebx: u32,
    pub ecx: u32,
    pub edx: u32,
    pub esi: u32,
    pub edi: u32,
    pub ebp: u32,
}

/// Writes a [`CrashRecord`] to [`CRASH_RECORD_ADDR`]. Called from a fault
/// handler, so this must not allocate, touch disk, or otherwise assume
/// anything beyond plain memory writes still working.
pub fn record_crash(
    vector: u8,
    error_code: Option<u32>,
    code_segment: u32,
    instruction_pointer: u32,
    eflags: u32,
    stack_segment: u32,
    stack_pointer: u32,
    registers: Option<SavedRegisters>,
    booted_slot: u8,
) {
    let record = CrashRecord {
        magic: CRASH_RECORD_MAGIC,
        version: CRASH_RECORD_VERSION,
        vector,
        has_error_code: error_code.is_some() as u8,
        error_code: error_code.unwrap_or(0),
        code_segment,
        instruction_pointer,
        eflags,
        stack_segment,
        stack_pointer,
        eax: registers.as_ref().map_or(0, |r| r.eax),
        ebx: registers.as_ref().map_or(0, |r| r.ebx),
        ecx: registers.as_ref().map_or(0, |r| r.ecx),
        edx: registers.as_ref().map_or(0, |r| r.edx),
        esi: registers.as_ref().map_or(0, |r| r.esi),
        edi: registers.as_ref().map_or(0, |r| r.edi),
        ebp: registers.as_ref().map_or(0, |r| r.ebp),
        has_registers: registers.is_some() as u8,
        booted_slot,
        _reserved: [0; 2],
        ticks: crate::interrupts::ticks(),
    };
    unsafe {
        (CRASH_RECORD_ADDR as *mut CrashRecord).write_unaligned(record);
    }
}

/// Prints and clears any crash record left by a previous boot. Called once,
/// early in `shell::run`, unconditionally (even with debug mode off): a
/// record can only exist if a previous boot actually wrote one, and seeing
/// it once is harmless and useful even for a machine not currently running
/// with `reboot-on-crash` enabled (e.g. an operator just turned debug mode
/// off after diagnosing the crash it reported).
pub fn report_and_clear_previous_crash() {
    let record = unsafe { (CRASH_RECORD_ADDR as *const CrashRecord).read_unaligned() };
    if record.magic != CRASH_RECORD_MAGIC || record.version != CRASH_RECORD_VERSION {
        return;
    }
    let vector = record.vector;
    let code_segment = record.code_segment;
    let instruction_pointer = record.instruction_pointer;
    let eflags = record.eflags;
    let stack_segment = record.stack_segment;
    let stack_pointer = record.stack_pointer;
    let ticks = record.ticks;
    let booted_slot = record.booted_slot;
    println!(
        "debug: previous boot crashed (vector {vector}) at {code_segment:04x}:{instruction_pointer:08x}, eflags 0x{eflags:08x}, stack {stack_segment:04x}:{stack_pointer:08x}, tick {ticks}, slot {booted_slot}."
    );
    if vector == WATCHDOG_VECTOR {
        println!("debug: that was the software watchdog: the foreground stopped making progress.");
        // A machine that hangs soon after every boot must not be reset in an
        // endless loop. The record's tick is the previous boot's uptime, so a
        // watchdog reset early in a boot leaves this boot unwatched; a hang
        // after a long healthy run (an ordinary hang) re-arms normally.
        if ticks < WATCHDOG_EARLY_RESET_TICKS {
            unsafe { WATCHDOG_GUARD_DISARMED = true };
            println!("debug: reset only {}s after boot, so the watchdog stays off this boot.", ticks / 100);
        }
    }
    if record.has_error_code != 0 {
        let error_code = record.error_code;
        println!("debug: error code 0x{error_code:08x}.");
    }
    if record.has_registers != 0 {
        let eax = record.eax;
        let ebx = record.ebx;
        let ecx = record.ecx;
        let edx = record.edx;
        let esi = record.esi;
        let edi = record.edi;
        let ebp = record.ebp;
        println!(
            "debug: eax={eax:08x} ebx={ebx:08x} ecx={ecx:08x} edx={edx:08x} esi={esi:08x} edi={edi:08x} ebp={ebp:08x}."
        );
    }
    unsafe {
        core::ptr::write_bytes(CRASH_RECORD_ADDR as *mut u8, 0, core::mem::size_of::<CrashRecord>());
    }
}

/// Optional `debug` control file, parsed once at boot by [`load_config`].
/// One option per line, in any order, blank lines and `#`-prefixed comments
/// ignored (mirroring `shell.rs`'s own `start` script convention):
///
/// ```text
/// reboot-on-crash        # reboot (after a short delay) instead of halting on a fatal exception
/// reboot-delay SECONDS   # override the default delay before rebooting (default 3)
/// watchdog [SECONDS]     # reset the machine if the foreground makes no progress for SECONDS (default 60)
/// kernel-update          # serve the network kernel-update protocol from the idle loop
/// crash-on-boot          # test-only: deliberately crash every boot, before the milestone
///                        # confirm point -- lets the consecutive-crash loop guard (which
///                        # only ever counts crashes that happen before a boot reaches its
///                        # own milestone; see `confirm_boot`) be exercised and verified
///                        # without physically induced faults or a real bad kernel push.
/// ```
///
/// Absent entirely, every option defaults off/unchanged -- an ordinary
/// deployment with no `debug` file anywhere behaves exactly as it always
/// has.
#[derive(Clone, Copy, Debug)]
pub struct DebugConfig {
    pub reboot_on_crash: bool,
    pub reboot_delay_ticks: u32,
    pub kernel_update: bool,
    pub crash_on_boot: bool,
    /// Seconds without foreground progress after which the software watchdog
    /// resets the machine; 0 = off (the default). See `watchdog_check`.
    pub watchdog_ticks: u32,
}

impl Default for DebugConfig {
    fn default() -> Self {
        DebugConfig {
            reboot_on_crash: false,
            reboot_delay_ticks: DEFAULT_REBOOT_DELAY_TICKS,
            kernel_update: false,
            crash_on_boot: false,
            watchdog_ticks: 0,
        }
    }
}

const DEBUG_FILE_NAME: &[u8] = b"debug";
const DEBUG_FILE_CAPACITY: usize = 1024;
const DEFAULT_REBOOT_DELAY_SECONDS: u32 = 3;
const DEFAULT_REBOOT_DELAY_TICKS: u32 = DEFAULT_REBOOT_DELAY_SECONDS * 100;

static mut CONFIG: DebugConfig = DebugConfig {
    reboot_on_crash: false,
    reboot_delay_ticks: DEFAULT_REBOOT_DELAY_TICKS,
    kernel_update: false,
    crash_on_boot: false,
    watchdog_ticks: 0,
};

/// Looks for an optional `debug` file, checking the boot disk first and then
/// every secondary disk in order -- the same lookup `shell.rs`'s
/// `load_hostname` already uses, since a debug switch is exactly the kind of
/// per-machine setting that belongs next to a hostname file rather than
/// baked into the boot image. Any error (including an unformatted disk) is
/// silently treated as "no debug file there", for the same reason
/// `load_hostname` already does.
pub fn load_config(info: &BootInfo) {
    let devices = core::iter::once(disk::Device::Boot)
        .chain((0..disk::secondary_count()).map(disk::Device::Secondary));
    for device in devices {
        let length = match fosfs::file_length(info, device, DEBUG_FILE_NAME) {
            Ok(Some(length)) => length,
            _ => continue,
        };
        if length > DEBUG_FILE_CAPACITY as u64 {
            println!(
                "debug: ignoring a {length}-byte debug file (limit is {DEBUG_FILE_CAPACITY} bytes)."
            );
            continue;
        }
        let mut contents = [0u8; DEBUG_FILE_CAPACITY];
        let Ok(Some(read_length)) =
            fosfs::read(info, device, DEBUG_FILE_NAME, &mut contents[..length as usize])
        else {
            continue;
        };
        let config = parse_config(&contents[..read_length as usize]);
        unsafe {
            CONFIG = config;
        }
        println!(
            "debug: debug mode active (reboot-on-crash={}, reboot-delay={}s, kernel-update={}).",
            config.reboot_on_crash,
            config.reboot_delay_ticks / 100,
            config.kernel_update
        );
        return;
    }
}

fn parse_config(contents: &[u8]) -> DebugConfig {
    let mut config = DebugConfig::default();
    for line in contents.split(|&byte| byte == b'\n') {
        let line = trim(line);
        if line.is_empty() || line[0] == b'#' {
            continue;
        }
        let mut words = line.split(|&byte| byte == b' ').filter(|word| !word.is_empty());
        match words.next() {
            Some(b"reboot-on-crash") => config.reboot_on_crash = true,
            Some(b"reboot-delay") => {
                if let Some(seconds) = words.next().and_then(parse_u32) {
                    config.reboot_delay_ticks = seconds.saturating_mul(100);
                }
            }
            Some(b"kernel-update") => config.kernel_update = true,
            Some(b"crash-on-boot") => config.crash_on_boot = true,
            Some(b"watchdog") => {
                let seconds = words.next().and_then(parse_u32).unwrap_or(WATCHDOG_DEFAULT_SECONDS);
                config.watchdog_ticks = seconds.saturating_mul(100);
            }
            _ => {}
        }
    }
    config
}

fn trim(mut bytes: &[u8]) -> &[u8] {
    while let [b'\r' | b' ' | b'\t', rest @ ..] = bytes {
        bytes = rest;
    }
    while let [rest @ .., b'\r' | b' ' | b'\t'] = bytes {
        bytes = rest;
    }
    bytes
}

fn parse_u32(bytes: &[u8]) -> Option<u32> {
    if bytes.is_empty() {
        return None;
    }
    let mut value: u32 = 0;
    for &byte in bytes {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add(u32::from(byte - b'0'))?;
    }
    Some(value)
}

pub fn reboot_on_crash() -> bool {
    unsafe { CONFIG.reboot_on_crash }
}

pub fn reboot_delay_ticks() -> u32 {
    unsafe { CONFIG.reboot_delay_ticks }
}

pub fn kernel_update_enabled() -> bool {
    unsafe { CONFIG.kernel_update }
}

/// The crash-record vector used for a watchdog reset: outside the CPU's 0..31
/// exception vectors and the IRQ range, so it cannot be mistaken for either.
const WATCHDOG_VECTOR: u8 = 0xf0;
const WATCHDOG_DEFAULT_SECONDS: u32 = 60;
/// While a tape runs, the limit is at least this long: a compute-bound tape
/// (a big sieve) never calls back into the kernel, but a tape that polls
/// (`should_abort`, `read_key`, `nic_receive`...) pets the watchdog anyway.
const WATCHDOG_TAPE_LIMIT_TICKS: u64 = 10 * 60 * 100;
/// A watchdog reset within this uptime of the boot that was reset is treated as
/// a hang loop and disarms the watchdog for the next boot.
const WATCHDOG_EARLY_RESET_TICKS: u64 = 5 * 60 * 100;

static mut WATCHDOG_ARMED: bool = false;
static mut WATCHDOG_LAST_PET: u64 = 0;
static mut WATCHDOG_TAPE_RUNNING: bool = false;
static mut WATCHDOG_GUARD_DISARMED: bool = false;

/// Records that the foreground is alive. A single memory write, safe to call
/// from anywhere and as often as a loop likes. Called from every blocking
/// loop's escape check (`keyboard::poll_term_event`), `poll_background_work`,
/// the idle prompt loop, each BIOS disk call, and the polling tape callbacks.
pub fn watchdog_pet() {
    unsafe { WATCHDOG_LAST_PET = crate::interrupts::ticks() };
}

/// Marks a tape as running, which relaxes the limit (see
/// `WATCHDOG_TAPE_LIMIT_TICKS`). Also counts as progress.
pub fn watchdog_tape_running(running: bool) {
    unsafe { WATCHDOG_TAPE_RUNNING = running };
    watchdog_pet();
}

/// Arms the watchdog once boot has reached its working milestone (called right
/// after `confirm_boot`, so a hang *during* boot is never reset in a loop), if
/// the `debug` file enabled it and the hang-loop guard did not veto it.
pub fn arm_watchdog() {
    let ticks = unsafe { CONFIG.watchdog_ticks };
    if ticks == 0 {
        return;
    }
    if unsafe { WATCHDOG_GUARD_DISARMED } {
        return;
    }
    watchdog_pet();
    unsafe { WATCHDOG_ARMED = true };
    println!("debug: watchdog armed: reset after {}s without foreground progress.", ticks / 100);
}

pub fn watchdog_seconds() -> u32 {
    unsafe { CONFIG.watchdog_ticks / 100 }
}

pub fn watchdog_armed() -> bool {
    unsafe { WATCHDOG_ARMED }
}

/// Called from the timer interrupt on every tick with the interrupted code's
/// frame. If the foreground has not petted the watchdog for longer than the
/// limit, this is a hang: a spin with interrupts on, an interrupt storm that
/// starves the foreground, a wait that can never end. It records a crash record
/// carrying where the CPU was stuck (so the next boot can report it), then
/// resets directly. It must not use `handle_crash`: that waits on timer ticks,
/// which cannot advance inside the timer handler, and does disk I/O through the
/// BIOS trampoline, which is unsafe if the foreground is mid-BIOS-call. A hang
/// with interrupts disabled never reaches here at all -- the timer is dead.
pub fn watchdog_check(
    code_segment: u32,
    instruction_pointer: u32,
    eflags: u32,
    stack_segment: u32,
    stack_pointer: u32,
) {
    unsafe {
        if !WATCHDOG_ARMED {
            return;
        }
        let mut limit = u64::from(CONFIG.watchdog_ticks);
        if WATCHDOG_TAPE_RUNNING {
            limit = limit.max(WATCHDOG_TAPE_LIMIT_TICKS);
        }
        let idle = crate::interrupts::ticks().wrapping_sub(WATCHDOG_LAST_PET);
        if idle <= limit {
            return;
        }
        WATCHDOG_ARMED = false; // never re-enter
        let info = crate::boot_info::BootInfo::from_address(crate::boot_info::BOOT_INFO_ADDRESS);
        record_crash(
            WATCHDOG_VECTOR,
            None,
            code_segment,
            instruction_pointer,
            eflags,
            stack_segment,
            stack_pointer,
            None,
            info.map_or(0, |info| info.booted_slot),
        );
        println!(
            "\nWATCHDOG: no foreground progress for {}s; the CPU was at {code_segment:04x}:{instruction_pointer:08x}. Resetting.",
            idle / 100
        );
    }
    crate::arch::x86::reset();
}

/// Test-only: if the `debug` file's `crash-on-boot` option is set, crash
/// immediately (a plain `int3` breakpoint, the same fault `debug crash`
/// injects manually) -- called from `shell::run` right after
/// [`load_config`] but *before* [`confirm_boot`], so a boot never reaches
/// its own milestone. Combined with `reboot-on-crash`, this exercises the
/// real "kernel keeps crashing before ever confirming" scenario the
/// consecutive-crash loop guard in [`handle_crash`] exists for -- something
/// the `debug crash` shell command alone cannot exercise, since a boot must
/// already have reached a working prompt to type it, which itself already
/// confirms the boot and resets the counter.
pub fn maybe_crash_on_boot() {
    if unsafe { CONFIG.crash_on_boot } {
        println!("debug: crash-on-boot is set; crashing before this boot's milestone ...");
        unsafe {
            core::arch::asm!("int3");
        }
    }
}

/// Handles a fatal exception when `reboot-on-crash` is active: records the
/// crash (RAM), bumps and persists the on-disk crash-loop counter, then
/// either waits `reboot_delay_ticks` and resets, or -- once
/// [`MAX_CONSECUTIVE_CRASHES`] is reached -- halts instead, so a kernel that
/// crashes immediately after every single reboot cannot spin forever
/// unattended. `info` may be `None` if the crash happened before `BootInfo`
/// was validated; the loop-guard degrades to "always reboot" in that case,
/// since there is no disk access without it.
pub fn handle_crash(info: Option<&BootInfo>) -> ! {
    let crash_count = info.and_then(|info| {
        let mut metadata = SlotMetadata::read(info)?;
        metadata.crash_count = metadata.crash_count.saturating_add(1);
        let count = metadata.crash_count;
        let _ = metadata.write(info);
        Some(count)
    });
    if let Some(count) = crash_count {
        if count >= MAX_CONSECUTIVE_CRASHES {
            println!(
                "debug: {count} consecutive un-confirmed reboots; halting instead of rebooting again."
            );
            crate::arch::x86::halt_forever();
        }
    }
    let delay = reboot_delay_ticks();
    println!("debug: rebooting in {}s ...", delay / 100);
    let deadline = crate::interrupts::ticks().wrapping_add(u64::from(delay));
    while crate::interrupts::ticks() < deadline {
        core::hint::spin_loop();
    }
    crate::arch::x86::reset();
}
