//! The aarch64 kernel's side of the A/B slot metadata (payload sector 0, `fosfor_abi::SlotMetadata`). The loader runs the
//! fallback state machine and marks a boot `running = 1`; the kernel's part is the *milestone* (`confirm_boot`: the shell is
//! about to take input), which promotes a pending kernel, clears the crash count and marks the boot up, and the clean stop
//! before a deliberate reset. The x86 kernel does the same in `debug.rs`, which also carries the crash record and watchdog.

use fosfor_abi::{RUNNING_STOPPED, RUNNING_UP, SlotMetadata};

use crate::{block, storage};

fn location() -> Option<(usize, u64)> {
    Some((storage::boot_disk()?, storage::payload_start()?))
}

pub fn read_metadata() -> Option<SlotMetadata> {
    let (disk, start) = location()?;
    let mut sector = [0u8; 512];
    block::read(disk, start, &mut sector).ok()?;
    SlotMetadata::parse(&sector)
}

/// Read-modify-write of the metadata sector.
pub fn update_metadata(change: impl FnOnce(&mut SlotMetadata)) -> Option<SlotMetadata> {
    let (disk, start) = location()?;
    let mut metadata = read_metadata()?;
    change(&mut metadata);
    let mut sector = [0u8; 512];
    metadata.write_into(&mut sector);
    block::write(disk, start, &sector).ok()?;
    Some(metadata)
}

/// The kernel's milestone: confirms a pending kernel (it becomes the active slot at once), clears the crash streak and
/// marks the boot as up, so a later reset without `mark_clean_stop` counts as unclean.
pub fn confirm_boot() {
    let boot_slot = storage::boot_slot() as u8;
    let updated = update_metadata(|metadata| {
        if metadata.pending_slot == Some(boot_slot) && metadata.attempted {
            crate::println!("Kernel update confirmed: slot {boot_slot} reached its milestone and is now the active slot.");
            metadata.active_slot = boot_slot;
            (metadata.pending_slot, metadata.attempted, metadata.confirmed) = (None, false, false);
        }
        metadata.crash_count = 0;
        metadata.running = RUNNING_UP;
    });
    if updated.is_none() {
        crate::println!("Slot metadata could not be updated: no A/B fallback or crash tracking this boot.");
    }
}

/// Marks the stop as deliberate, so the next boot does not count it as a crash.
pub fn mark_clean_stop() {
    let _ = update_metadata(|metadata| metadata.running = RUNNING_STOPPED);
}

pub fn show() {
    crate::println!("booted slot: {}", storage::boot_slot());
    match read_metadata() {
        Some(m) => {
            crate::println!("active slot: {}, pending: {:?}, attempted: {}, confirmed: {}", m.active_slot, m.pending_slot, m.attempted, m.confirmed);
            crate::println!("slot sizes (sectors): {} / {}; crash count {}, unclean boots {}", m.slot_sectors[0], m.slot_sectors[1], m.crash_count, m.unclean_boots);
        }
        None => crate::println!("the slot metadata cannot be read."),
    }
}
