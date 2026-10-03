//! The loader/kernel contract of fos v2 (see `docs/v2-design.md`, sections 4-5).
//!
//! Two things live here because both sides must agree on them byte for byte:
//! [`BootInfo`], the structure the loader passes to the kernel by pointer, and the
//! layout of the payload partition (a metadata sector, then two kernel slots).
#![cfg_attr(not(test), no_std)]

pub mod keymap;

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
    pub reserved: u32,
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

/// The slot-metadata fields the loader needs. The byte layout is v1's, unchanged
/// (`kernel/src/debug.rs`), so the same host tooling and the same state machine
/// apply: `+0 "FOSM"`, `+4 version`, `+5 active slot`, `+6 pending slot (0xff =
/// none)`, `+7 attempted`, `+8 confirmed`, `+10/+12 slot sizes in sectors (u16 LE)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlotMetadata {
    pub active_slot: u8,
    pub pending_slot: Option<u8>,
    pub attempted: bool,
    pub confirmed: bool,
    pub slot_sectors: [u16; 2],
}

impl SlotMetadata {
    pub fn parse(sector: &[u8]) -> Option<SlotMetadata> {
        if sector.len() < 14 || sector[0..4] != METADATA_MAGIC || sector[4] != METADATA_VERSION {
            return None;
        }
        let pending = sector[6];
        Some(SlotMetadata {
            active_slot: sector[5],
            pending_slot: if pending == NO_PENDING_SLOT { None } else { Some(pending) },
            attempted: sector[7] != 0,
            confirmed: sector[8] != 0,
            slot_sectors: [u16::from_le_bytes([sector[10], sector[11]]), u16::from_le_bytes([sector[12], sector[13]])],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
