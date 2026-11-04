//! `slots` / `debug slots`: what the A/B slot metadata sector says, printed the same by the embedded and the server kernel.

use fosfor_abi::{FLAG_SKIP_VIDEO, SlotMetadata};

/// Why the slot-metadata sector could not be used.
/// `E` is the kernel's own block error type.
pub enum MetadataProblem<E> {
    /// The payload partition was never located (no boot disk found).
    NoPayload,
    /// The disk driver failed the read.
    Read(E),
    /// The sector was read but is not valid metadata: its first bytes, for the operator to compare with `"FOSM"` + version 1.
    Invalid([u8; 16]),
}

/// `boot_crash_count` is what the loader saw at boot (the sector's own count is cleared at the milestone); `watchdog_seconds`
/// is None while the software watchdog is off.
pub fn report<E: core::fmt::Debug>(
    print: impl Fn(core::fmt::Arguments),
    boot_slot: u32,
    boot_crash_count: u32,
    metadata: Result<SlotMetadata, MetadataProblem<E>>,
    watchdog_seconds: Option<u64>,
) {
    print(format_args!("booted slot: {boot_slot}"));
    // fosadm compares this with the tape ABI its build tapes were made for.
    print(format_args!("tape ABI: {}", fosfor_abi::tape::TAPE_API_VERSION));
    match metadata {
        Ok(m) => {
            print(format_args!("active slot: {}, pending: {:?}, attempted: {}, confirmed: {}", m.active_slot, m.pending_slot, m.attempted, m.confirmed));
            print(format_args!("slot sizes (sectors): {} / {}", m.slot_sectors[0], m.slot_sectors[1]));
            print(format_args!(
                "BIOS video mode switch: {} (`debug video skip|auto`), monitor size hint: {} (`debug video size WxH|auto`)",
                if m.flags & FLAG_SKIP_VIDEO != 0 { "skipped" } else { "automatic" },
                if m.monitor_hint == (0, 0) { "none" } else { "set" }
            ));
            if m.monitor_hint != (0, 0) {
                print(format_args!("  hint: {}x{}", m.monitor_hint.0, m.monitor_hint.1));
            }
            print(format_args!("crash count: {} (loader saw {} at boot), running: {}, unclean boots: {}", m.crash_count, boot_crash_count, m.running, m.unclean_boots));
        }
        Err(MetadataProblem::NoPayload) => print(format_args!("slot metadata: not readable (the payload partition was never located)")),
        Err(MetadataProblem::Read(error)) => print(format_args!("slot metadata: not readable (the disk read failed: {error:?})")),
        Err(MetadataProblem::Invalid(head)) => {
            print(format_args!("slot metadata: not readable (the sector is not valid metadata; `sector disk1 N` shows it)"));
            print(format_args!("  it starts: {head:02x?}   wanted: 46 4f 53 4d 01 (\"FOSM\", version 1)"));
        }
    }
    match watchdog_seconds {
        Some(seconds) => print(format_args!("watchdog: {seconds} s")),
        None => print(format_args!("watchdog: off")),
    }
}

/// `debug revert`: makes the other slot the active one (the previous kernel, kept there by the last update) and drops any pending
/// try. The slot must hold a kernel (its size in the metadata is non-zero). Returns the slot that boots next.
pub fn revert(metadata: &mut SlotMetadata) -> Result<u8, &'static str> {
    let other = 1 - metadata.active_slot.min(1);
    if metadata.slot_sectors[other as usize] == 0 {
        return Err("the other slot holds no kernel");
    }
    metadata.active_slot = other;
    (metadata.pending_slot, metadata.attempted, metadata.confirmed) = (None, false, false);
    Ok(other)
}
