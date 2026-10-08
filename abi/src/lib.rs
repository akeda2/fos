//! The loader/kernel contract of fos v2 (see `docs/v2-design.md`, sections 4-5).
//!
//! Two things live here because both sides must agree on them byte for byte:
//! [`BootInfo`], the structure the loader passes to the kernel by pointer, and the
//! layout of the payload partition (a metadata sector, then two kernel slots).
#![cfg_attr(not(test), no_std)]

pub mod elf;
pub mod keymap;
pub mod tape;

/// "FOSV2BI\0" as a little-endian `u64`.
pub const BOOT_INFO_MAGIC: u64 = u64::from_le_bytes(*b"FOSV2BI\0");
pub const BOOT_INFO_VERSION: u32 = 5;

/// The memory map, in the firmware's own format. Entries are `descriptor_size`
/// bytes apart (never `size_of::<MemoryDescriptor>()`: the firmware may use a
/// larger descriptor), and `kind` follows the UEFI memory type numbering.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct MemoryDescriptor {
    pub kind: u32,
    pub padding: u32,
    pub physical_start: u64,
    pub virtual_start: u64,
    pub pages: u64,
    pub attribute: u64,
}

/// UEFI memory types the kernel treats as ordinary RAM it may map (and, once boot
/// services are gone, reuse): loader code/data, boot services code/data, runtime
/// services code/data (kept mapped for `ResetSystem`), conventional memory, ACPI.
pub const RAM_LIKE_TYPES: [u32; 9] = [1, 2, 3, 4, 5, 6, 7, 9, 10];
pub const TYPE_CONVENTIONAL: u32 = 7;

/// Pixel formats of the firmware's linear framebuffer (UEFI `GraphicsPixelFormat`).
pub const PIXEL_RGB: u32 = 0;
pub const PIXEL_BGR: u32 = 1;
pub const PIXEL_BITMASK: u32 = 2;
pub const PIXEL_NONE: u32 = 3;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Framebuffer {
    pub base: u64,
    pub size: u64,
    pub width: u32,
    pub height: u32,
    /// Pixels (not bytes) per scan line; a pixel is 4 bytes in every format the
    /// firmware offers.
    pub stride: u32,
    pub format: u32,
}

/// What the loader hands the kernel (a pointer in `rdi`, System V ABI). Everything
/// it points at stays valid: it lives in memory the loader allocated as "loader
/// data", which the kernel treats as reserved until it has copied what it needs.
#[repr(C)]
pub struct BootInfo {
    pub magic: u64,
    pub version: u32,
    /// `size_of::<BootInfo>()` as the loader saw it: lets the kernel refuse a
    /// loader built against a different layout.
    pub size: u32,
    pub memory_map: u64,
    pub memory_map_count: u64,
    pub memory_map_descriptor_size: u64,
    pub framebuffer: Framebuffer,
    /// Physical address of the ACPI RSDP (0 if the firmware has none).
    pub acpi_rsdp: u64,
    /// The UEFI runtime services table (still callable after `ExitBootServices`).
    pub runtime_services: u64,
    pub kernel_base: u64,
    pub kernel_size: u64,
    /// Which A/B slot the kernel was loaded from.
    pub boot_slot: u32,
    /// Consecutive boots that never reached the kernel's milestone, counting this one
    /// if the previous one crashed (the loader's crash-loop evidence; M6).
    pub crash_count: u32,
    /// The payload partition the loader read the kernel from: its unique GPT GUID
    /// (on-disk byte order) and position. After `ExitBootServices` the kernel has no
    /// firmware to ask which disk that was, so it scans its own drivers' disks for
    /// the partition with this GUID, start and length (docs/v2-design.md, section 8).
    pub payload_guid: [u8; 16],
    pub payload_start: u64,
    pub payload_sectors: u64,
}

impl BootInfo {
    /// Whether this structure is what this crate's version expects.
    pub fn is_valid(&self) -> bool {
        self.magic == BOOT_INFO_MAGIC
            && self.version == BOOT_INFO_VERSION
            && self.size as usize == core::mem::size_of::<BootInfo>()
    }

    /// The memory map as a slice-like iterator over descriptors.
    pub fn memory_descriptors(&self) -> impl Iterator<Item = MemoryDescriptor> + '_ {
        let base = self.memory_map as usize;
        let stride = self.memory_map_descriptor_size as usize;
        (0..self.memory_map_count as usize)
            .map(move |index| unsafe { core::ptr::read_unaligned((base + index * stride) as *const MemoryDescriptor) })
    }
}

// ---- payload partition layout -------------------------------------------------

pub const SECTOR: usize = 512;
/// GPT partition type GUID of the payload partition (a fos-specific value),
/// in the mixed-endian byte order GPT stores GUIDs in.
pub const PAYLOAD_TYPE_GUID: [u8; 16] = guid_bytes(0xf05b2a01, 0x0000, 0x4f05, [0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xa1]);
/// GPT partition type GUID of the FOSFS data partition (v1's value, so a v1 data
/// partition is recognised as is).
pub const FOSFS_TYPE_GUID: [u8; 16] = guid_bytes(0x0f6f7362, 0x0000, 0x4f53, [0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04]);
/// Sector 0 of the payload partition is the metadata block; slot 0 starts at
/// sector 1 and slot 1 follows it, each `SLOT_SECTORS` long (8 MiB).
pub const SLOT_SECTORS: u64 = 16384;
pub const METADATA_MAGIC: [u8; 4] = *b"FOSM";
pub const METADATA_VERSION: u8 = 1;
pub const NO_PENDING_SLOT: u8 = 0xff;

/// Where the BIOS boot chain (boot/bios) lives on a disk: stage one is the first 446 bytes of sector 0 (the MBR; the
/// protective partition entry and the 0x55aa signature stay), stage two is `BIOS_STAGE2_SECTORS` sectors at LBA
/// `BIOS_STAGE2_LBA`, in the gap a GPT leaves between its entry array (LBA 2-33) and the first partition (LBA 2048).
/// Keep equal to boot/bios/stage1.S and tools/build_image.py.
pub const BIOS_STAGE2_LBA: u64 = 64;
pub const BIOS_STAGE2_SECTORS: u64 = 24;

/// First sector of kernel slot `slot` (0 or 1) within the payload partition.
pub const fn slot_start(slot: u64) -> u64 {
    1 + slot * SLOT_SECTORS
}

/// GUID in GPT's on-disk form: the first three fields little-endian, the rest as is.
pub const fn guid_bytes(a: u32, b: u16, c: u16, d: [u8; 8]) -> [u8; 16] {
    let a = a.to_le_bytes();
    let b = b.to_le_bytes();
    let c = c.to_le_bytes();
    [a[0], a[1], a[2], a[3], b[0], b[1], c[0], c[1], d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]]
}

/// The slot-metadata sector (payload sector 0), v1's byte layout unchanged
/// (`kernel/src/debug.rs`), so the same host tooling and state machine apply:
/// `+0 "FOSM"`, `+4 version`, `+5 active slot`, `+6 pending slot (0xff = none)`,
/// `+7 attempted`, `+8 confirmed`, `+9 crash count`, `+10/+12 slot sizes in sectors
/// (u16 LE)`, `+14 running`, `+15 unclean boots`, `+16 flags` (see `FLAG_SKIP_VIDEO`).
///
/// Meaning of the last three (see docs/v2-design.md, M6): `running` is 1 while a boot
/// has not yet reached the kernel's milestone, 2 once it has, 0 after a clean stop
/// (shutdown, reset, reboot, a kernel update restart). The loader, seeing 1 at the next
/// boot, knows the previous boot crashed or hung *before* the milestone and bumps
/// `crash_count`; the kernel zeroes `crash_count` at the milestone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlotMetadata {
    /// Boot options the BIOS stage two reads before it can read any file (byte 16): `FLAG_*`. The UEFI loader keeps them
    /// untouched.
    pub flags: u8,
    pub active_slot: u8,
    pub pending_slot: Option<u8>,
    pub attempted: bool,
    pub confirmed: bool,
    pub crash_count: u8,
    pub slot_sectors: [u16; 2],
    pub running: u8,
    pub unclean_boots: u8,
}

/// `SlotMetadata::flags` bit 0: the BIOS stage two does not switch the video mode (a video BIOS that hangs in a VBE call);
/// the kernel then runs on serial only. Set with `debug video skip`; holding Shift during a BIOS boot does the same once.
pub const FLAG_SKIP_VIDEO: u8 = 1;

pub const RUNNING_STOPPED: u8 = 0;
pub const RUNNING_BOOTING: u8 = 1;
pub const RUNNING_UP: u8 = 2;
/// Consecutive pre-milestone crashes after which the loader refuses to boot again.
pub const MAX_CONSECUTIVE_CRASHES: u8 = 5;

impl SlotMetadata {
    pub fn parse(sector: &[u8]) -> Option<SlotMetadata> {
        if sector.len() < 16 || sector[0..4] != METADATA_MAGIC || sector[4] != METADATA_VERSION {
            return None;
        }
        let pending = sector[6];
        Some(SlotMetadata {
            flags: sector[16],
            active_slot: sector[5],
            pending_slot: if pending == NO_PENDING_SLOT { None } else { Some(pending) },
            attempted: sector[7] != 0,
            confirmed: sector[8] != 0,
            crash_count: sector[9],
            slot_sectors: [u16::from_le_bytes([sector[10], sector[11]]), u16::from_le_bytes([sector[12], sector[13]])],
            running: sector[14],
            unclean_boots: sector[15],
        })
    }

    /// Writes the fields into a metadata sector (zeroing the reserved bytes).
    pub fn write_into(&self, sector: &mut [u8]) {
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
        sector[14] = self.running;
        sector[15] = self.unclean_boots;
        sector[16] = self.flags;
    }
}

/// What the loader decided about which kernel to boot (the A/B fallback state machine,
/// v1's `select_kernel_slot`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotChoice {
    /// Boot this slot; the metadata may have been changed and must be written back.
    Boot(u8),
}

/// The A/B state machine, run by the loader before it reads a kernel:
/// * no pending slot: boot the active one;
/// * a pending slot not yet attempted: mark it attempted and boot it (**one** try);
/// * attempted and confirmed (the kernel reached its milestone but reset before it could
///   clear the flags): promote it and boot it;
/// * attempted but not confirmed (it crashed, hung or lost power first): give up on it
///   and boot the unchanged active slot.
/// Returns the slot to boot and updates `metadata`; the caller writes it back.
pub fn select_slot(metadata: &mut SlotMetadata) -> SlotChoice {
    match metadata.pending_slot {
        None => SlotChoice::Boot(metadata.active_slot & 1),
        Some(pending) if !metadata.attempted => {
            metadata.attempted = true;
            SlotChoice::Boot(pending & 1)
        }
        Some(pending) if metadata.confirmed => {
            metadata.active_slot = pending & 1;
            (metadata.pending_slot, metadata.attempted, metadata.confirmed) = (None, false, false);
            SlotChoice::Boot(metadata.active_slot)
        }
        Some(_) => {
            (metadata.pending_slot, metadata.attempted, metadata.confirmed) = (None, false, false);
            SlotChoice::Boot(metadata.active_slot & 1)
        }
    }
}

/// RFC 1071 Internet checksum (the 16-bit ones-complement sum of the data), as KUP and
/// NETFS use it.
pub fn internet_checksum(data: &[u8]) -> u16 {
    // A 64-bit accumulator: with 32 bits, 260 KB of high-entropy data (an average 16-bit word of 32768) overflowed it and
    // the checksum came out wrong for large files (found with a 300 KB NETFS transfer; KUP's mostly-zero kernel images
    // never got near it).
    let mut sum: u64 = 0;
    let mut chunks = data.chunks_exact(2);
    for pair in &mut chunks {
        sum += u64::from(u16::from_be_bytes([pair[0], pair[1]]));
    }
    if let [last] = chunks.remainder() {
        sum += u64::from(*last) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The BIOS boot path (boot/bios/stage2.S) writes `BootInfo` by hand, byte for byte. If this test fails, a field was
    /// added or moved: update the `BI_*` offsets in boot/bios/stage2.S in the same change.
    #[test]
    fn boot_info_layout_matches_the_bios_loader() {
        use core::mem::{offset_of, size_of};
        assert_eq!(size_of::<BootInfo>(), 144);
        assert_eq!(offset_of!(BootInfo, magic), 0);
        assert_eq!(offset_of!(BootInfo, version), 8);
        assert_eq!(offset_of!(BootInfo, size), 12);
        assert_eq!(offset_of!(BootInfo, memory_map), 16);
        assert_eq!(offset_of!(BootInfo, memory_map_count), 24);
        assert_eq!(offset_of!(BootInfo, memory_map_descriptor_size), 32);
        assert_eq!(offset_of!(BootInfo, framebuffer), 40);
        assert_eq!(offset_of!(BootInfo, acpi_rsdp), 72);
        assert_eq!(offset_of!(BootInfo, runtime_services), 80);
        assert_eq!(offset_of!(BootInfo, kernel_base), 88);
        assert_eq!(offset_of!(BootInfo, kernel_size), 96);
        assert_eq!(offset_of!(BootInfo, boot_slot), 104);
        assert_eq!(offset_of!(BootInfo, crash_count), 108);
        assert_eq!(offset_of!(BootInfo, payload_guid), 112);
        assert_eq!(offset_of!(BootInfo, payload_start), 128);
        assert_eq!(offset_of!(BootInfo, payload_sectors), 136);
        assert_eq!(size_of::<MemoryDescriptor>(), 40, "UEFI's descriptor is 40 bytes; the firmware may use a larger stride");
    }

    #[test]
    fn metadata_round_trips_the_v1_layout() {
        let mut sector = [0u8; 512];
        sector[0..4].copy_from_slice(b"FOSM");
        sector[4] = 1;
        sector[5] = 1;
        sector[6] = NO_PENDING_SLOT;
        sector[10..12].copy_from_slice(&300u16.to_le_bytes());
        sector[12..14].copy_from_slice(&512u16.to_le_bytes());
        let metadata = SlotMetadata::parse(&sector).unwrap();
        assert_eq!(metadata.active_slot, 1);
        assert_eq!(metadata.pending_slot, None);
        assert_eq!(metadata.slot_sectors, [300, 512]);
        sector[4] = 2;
        assert!(SlotMetadata::parse(&sector).is_none());
    }

    fn state(pending: Option<u8>, attempted: bool, confirmed: bool) -> SlotMetadata {
        SlotMetadata { flags: 0, active_slot: 0, pending_slot: pending, attempted, confirmed, crash_count: 0, slot_sectors: [1, 1], running: 0, unclean_boots: 0 }
    }

    #[test]
    fn a_b_state_machine_gives_a_pending_kernel_exactly_one_try() {
        let mut m = state(None, false, false);
        assert_eq!(select_slot(&mut m), SlotChoice::Boot(0));
        // fresh pending slot: boot it, mark attempted
        let mut m = state(Some(1), false, false);
        assert_eq!(select_slot(&mut m), SlotChoice::Boot(1));
        assert!(m.attempted && m.pending_slot == Some(1) && m.active_slot == 0);
        // it never confirmed: the next boot falls back and forgets it
        assert_eq!(select_slot(&mut m), SlotChoice::Boot(0));
        assert_eq!((m.pending_slot, m.attempted, m.active_slot), (None, false, 0));
        // it confirmed but reset before clearing: promote
        let mut m = state(Some(1), true, true);
        assert_eq!(select_slot(&mut m), SlotChoice::Boot(1));
        assert_eq!((m.pending_slot, m.active_slot, m.confirmed), (None, 1, false));
    }

    #[test]
    fn metadata_write_into_round_trips() {
        let m = SlotMetadata { flags: FLAG_SKIP_VIDEO, active_slot: 1, pending_slot: Some(0), attempted: true, confirmed: false, crash_count: 3, slot_sectors: [321, 400], running: 2, unclean_boots: 4 };
        let mut sector = [0xaau8; 512];
        m.write_into(&mut sector);
        assert_eq!(SlotMetadata::parse(&sector), Some(m));
        assert_eq!(sector[16], FLAG_SKIP_VIDEO);
        assert!(sector[17..].iter().all(|&b| b == 0), "everything after the flags byte stays zero");
    }

    #[test]
    fn internet_checksum_matches_rfc_1071() {
        // RFC 1071 example: 00 01 f2 03 f4 f5 f6 f7 sums to 0xddf2, checksum 0x220d.
        assert_eq!(internet_checksum(&[0x00, 0x01, 0xf2, 0x03, 0xf4, 0xf5, 0xf6, 0xf7]), 0x220d);
        assert_eq!(internet_checksum(&[0x01]), !0x0100);
    }

    #[test]
    fn guid_bytes_use_gpt_mixed_endian() {
        // The EFI System Partition type GUID, C12A7328-F81F-11D2-BA4B-00A0C93EC93B.
        let bytes = guid_bytes(0xC12A7328, 0xF81F, 0x11D2, [0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B]);
        assert_eq!(bytes[0..4], [0x28, 0x73, 0x2A, 0xC1]);
        assert_eq!(bytes[4..6], [0x1F, 0xF8]);
        assert_eq!(bytes[8..], [0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B]);
    }

    #[test]
    fn slots_follow_the_metadata_sector() {
        assert_eq!(slot_start(0), 1);
        assert_eq!(slot_start(1), 1 + SLOT_SECTORS);
    }

    #[test]
    fn checksum_of_a_large_buffer_does_not_overflow() {
        // 400,000 bytes of 0xff words sum to 200,000 * 65,535, far beyond a 32-bit accumulator.
        let data = [0xffu8; 400_000];
        // The ones-complement sum of all-ones words is 0xffff, whose complement is 0.
        assert_eq!(internet_checksum(&data), 0);
        // And a ramp (high entropy-ish) against a straightforward folding reference.
        let ramp: Vec<u8> = (0..300_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8).collect();
        let mut reference: u64 = 0;
        for pair in ramp.chunks(2) {
            reference += (u64::from(pair[0]) << 8) | u64::from(*pair.get(1).unwrap_or(&0));
        }
        while reference >> 16 != 0 {
            reference = (reference & 0xffff) + (reference >> 16);
        }
        assert_eq!(internet_checksum(&ramp), !(reference as u16));
    }
}
