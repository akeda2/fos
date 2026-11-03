//! The A/B slot metadata sector, the server kernel's side: what the loader's slot state machine leaves for the kernel
//! (`abi::SlotMetadata`, the first sector of the payload partition). A pushed kernel gets exactly one try: it must reach
//! `confirm_boot`, or the loader falls back to the old slot at the next boot. Same rules as the embedded kernel's `debug.rs`
//! (the crash record, `reboot-on-crash` and the watchdog are `debug.rs`).

use fosfor_abi::{BootInfo, RUNNING_STOPPED, RUNNING_UP, SlotMetadata};

use crate::block;

static mut PAYLOAD: Option<(usize, u64)> = None;
static mut BOOT_SLOT: u32 = 0;
static mut CLEAR_STREAK_PENDING: bool = false;
static mut STREAK_CLEARED: bool = false;
/// A boot that has run this long counts as good: the unclean-boot streak is cleared (5 minutes at 100 Hz).
const GOOD_UPTIME_TICKS: u64 = 5 * 60 * 100;

pub fn init(info: &BootInfo, disk: usize) {
    unsafe {
        BOOT_SLOT = info.boot_slot;
        PAYLOAD = Some((disk, info.payload_start));
    }
}

pub fn payload() -> Option<(usize, u64)> {
    unsafe { PAYLOAD }
}

pub fn read_metadata() -> Option<SlotMetadata> {
    let (disk, start) = payload()?;
    let mut sector = [0u8; 512];
    block::read(disk, start, &mut sector).ok()?;
    SlotMetadata::parse(&sector)
}

fn write_metadata(metadata: &SlotMetadata) -> bool {
    let Some((disk, start)) = payload() else { return false };
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

/// The kernel's milestone: it can be used now. Confirms a pending kernel (promoting it at once), clears the crash count and
/// marks the boot as up.
pub fn confirm_boot() {
    let boot_slot = unsafe { BOOT_SLOT } as u8;
    let mut unclean = 0;
    let updated = update_metadata(|metadata| {
        if metadata.pending_slot == Some(boot_slot) && metadata.attempted {
            println!("Kernel update confirmed: slot {boot_slot} reached its milestone and is now the active slot.");
            metadata.active_slot = boot_slot;
            (metadata.pending_slot, metadata.attempted, metadata.confirmed) = (None, false, false);
        }
        metadata.crash_count = 0;
        metadata.running = RUNNING_UP;
        unclean = metadata.unclean_boots;
    });
    if updated.is_none() {
        println!("Slot metadata could not be updated: no A/B fallback or crash tracking this boot.");
    }
    unsafe { CLEAR_STREAK_PENDING = unclean > 0 };
    crate::debug::arm(crate::config::watchdog_seconds(), unclean);
}

/// Marks the stop as deliberate, so the next boot does not count it as a crash.
pub fn mark_clean_stop() {
    let _ = update_metadata(|metadata| metadata.running = RUNNING_STOPPED);
}

/// Service-loop work: clears the unclean-boot streak once the boot has run long enough to be called good.
pub fn housekeeping() {
    unsafe {
        if CLEAR_STREAK_PENDING && !STREAK_CLEARED && crate::apic::ticks() > GOOD_UPTIME_TICKS {
            STREAK_CLEARED = true;
            let _ = update_metadata(|metadata| metadata.unclean_boots = 0);
        }
    }
}

pub fn boot_slot() -> u32 {
    unsafe { BOOT_SLOT }
}
