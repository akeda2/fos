pub const BOOT_INFO_MAGIC: u32 = 0x464F_5342;
pub const BOOT_INFO_VERSION: u16 = 3;
pub const MAX_MEMORY_REGIONS: usize = 32;

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
