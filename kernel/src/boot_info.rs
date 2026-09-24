pub const BOOT_INFO_MAGIC: u32 = 0x464F_5342;
pub const BOOT_INFO_VERSION: u16 = 5;
pub const MAX_MEMORY_REGIONS: usize = 32;
/// Fixed physical address `boot/stage2.S` writes `BootInfo` at (its own
/// `BOOT_INFO` constant) and `entry.S` passes to `kmain` in a register.
/// Exposed here so a fault handler (which has no `&BootInfo` of its own to
/// thread through) can re-fetch it on demand -- see
/// `kernel/src/debug.rs`'s crash-record path.
pub const BOOT_INFO_ADDRESS: u32 = 0x5000;

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct MemoryRegion {
    pub base: u64,
    pub length: u64,
    pub kind: u32,
    pub attributes: u32,
}

#[repr(C, packed)]
pub struct BootInfo {
    pub magic: u32,
    pub version: u16,
    pub size: u16,
    pub boot_drive: u8,
    pub serial_available: u8,
    pub memory_region_count: u16,
    pub kernel_start: u32,
    pub kernel_end: u32,
    pub bios_disk_io: u32,
    pub disk_sector_count: u64,
    pub data_partition_lba: u64,
    pub data_partition_sectors: u64,
    pub memory_regions: [MemoryRegion; MAX_MEMORY_REGIONS],
    /// Which fixed-size kernel slot (0 or 1) `boot/stage2.S`'s
    /// `select_kernel_slot` actually loaded this boot from -- see
    /// `kernel/src/debug.rs`'s A/B slot metadata for the full state
    /// machine this feeds into (milestone-confirming a pending update, or
    /// noticing a boot into a slot that never got confirmed).
    pub booted_slot: u8,
    /// Reserved padding, kept zero, so `size` stays even.
    pub _reserved: u8,
    /// Physical address of `boot/stage2.S`'s `soft_reboot_entry` -- a
    /// callback that warm-restarts the machine by dropping back into
    /// stage two's own `_start` instead of a hardware reset. Zero on a
    /// `BootInfo` that predates this field (version < 5) or that never
    /// populates it at all (e.g. `boot/netrom.S`'s disk-less variant);
    /// see `kernel/src/reboot.rs`'s own zero-address fallback.
    pub soft_reboot: u32,
}

impl BootInfo {
    pub unsafe fn from_address(address: u32) -> Option<&'static Self> {
        if address == 0 {
            return None;
        }

        let info = unsafe { &*(address as *const Self) };
        if info.magic != BOOT_INFO_MAGIC
            || info.version != BOOT_INFO_VERSION
            || usize::from(info.size) != core::mem::size_of::<Self>()
            || usize::from(info.memory_region_count) > MAX_MEMORY_REGIONS
        {
            return None;
        }
        Some(info)
    }
}

pub fn read_u32(pointer: *const u32) -> u32 {
    unsafe { core::ptr::read_unaligned(pointer) }
}

pub fn read_u64(pointer: *const u64) -> u64 {
    unsafe { core::ptr::read_unaligned(pointer) }
}
