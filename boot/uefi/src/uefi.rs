//! The small slice of the UEFI specification the loader needs, written out by
//! hand instead of pulled in as a dependency: this project exists to be read, and
//! twenty structures are easier to read than a crate.
//!
//! Calling convention: UEFI on x86-64 uses the Microsoft x64 ABI, which Rust
//! spells `extern "efiapi"`. Every function pointer below is a firmware function
//! the loader calls; the structures are `#[repr(C)]` mirrors of the spec's tables,
//! and a table's *field order is the ABI*, so entries the loader never calls are
//! kept as opaque `usize` placeholders rather than dropped.

use core::ffi::c_void;

pub type Handle = *mut c_void;
pub type Status = usize;

pub const SUCCESS: Status = 0;

/// `AllocatePages` types and the memory type the loader allocates as.
pub const ALLOCATE_ANY_PAGES: u32 = 0;
pub const ALLOCATE_ADDRESS: u32 = 2;
pub const LOADER_DATA: u32 = 2;
/// `LocateHandleBuffer` search type: every handle supporting a protocol.
pub const BY_PROTOCOL: u32 = 2;

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Guid(pub u32, pub u16, pub u16, pub [u8; 8]);

pub const BLOCK_IO_PROTOCOL: Guid = Guid(0x964e5b21, 0x6459, 0x11d2, [0x8e, 0x39, 0x00, 0xa0, 0xc9, 0x69, 0x72, 0x3b]);
pub const GRAPHICS_OUTPUT_PROTOCOL: Guid =
    Guid(0x9042a9de, 0x23dc, 0x4a38, [0x96, 0xfb, 0x7a, 0xde, 0xd0, 0x80, 0x51, 0x6a]);
pub const ACPI_20_TABLE: Guid = Guid(0x8868e871, 0xe4f1, 0x11d3, [0xbc, 0x22, 0x00, 0x80, 0xc7, 0x3c, 0x88, 0x81]);
pub const ACPI_10_TABLE: Guid = Guid(0xeb9d2d30, 0x2d88, 0x11d3, [0x9a, 0x16, 0x00, 0x90, 0x27, 0x3f, 0xc1, 0x4d]);

#[repr(C)]
pub struct TableHeader {
    pub signature: u64,
    pub revision: u32,
    pub header_size: u32,
    pub crc32: u32,
    pub reserved: u32,
}

#[repr(C)]
pub struct SystemTable {
    pub header: TableHeader,
    /// UTF-16, NUL-terminated.
    pub firmware_vendor: *const u16,
    pub firmware_revision: u32,
    pub console_in_handle: Handle,
    pub con_in: *mut c_void,
    pub console_out_handle: Handle,
    pub con_out: *mut SimpleTextOutput,
    pub standard_error_handle: Handle,
    pub std_err: *mut c_void,
    pub runtime_services: *mut RuntimeServices,
    pub boot_services: *mut BootServices,
    pub number_of_table_entries: usize,
    pub configuration_table: *mut ConfigurationTable,
}

#[repr(C)]
pub struct ConfigurationTable {
    pub vendor_guid: Guid,
    pub vendor_table: *mut c_void,
}

#[repr(C)]
pub struct SimpleTextOutput {
    pub reset: usize,
    pub output_string: extern "efiapi" fn(this: *mut SimpleTextOutput, string: *const u16) -> Status,
    pub test_string: usize,
    pub query_mode: usize,
    pub set_mode: usize,
    pub set_attribute: usize,
    pub clear_screen: usize,
    pub set_cursor_position: usize,
    pub enable_cursor: usize,
    pub mode: usize,
}

/// Boot services: only valid until `ExitBootServices`. Entries are in the order
/// the specification lists them.
#[repr(C)]
pub struct BootServices {
    pub header: TableHeader,
    pub raise_tpl: usize,
    pub restore_tpl: usize,
    pub allocate_pages: extern "efiapi" fn(kind: u32, memory_type: u32, pages: usize, address: *mut u64) -> Status,
    pub free_pages: usize,
    pub get_memory_map: extern "efiapi" fn(
        size: *mut usize,
        map: *mut MemoryDescriptor,
        key: *mut usize,
        descriptor_size: *mut usize,
        descriptor_version: *mut u32,
    ) -> Status,
    pub allocate_pool: extern "efiapi" fn(memory_type: u32, size: usize, buffer: *mut *mut c_void) -> Status,
    pub free_pool: usize,
    pub create_event: usize,
    pub set_timer: usize,
    pub wait_for_event: usize,
    pub signal_event: usize,
    pub close_event: usize,
    pub check_event: usize,
    pub install_protocol_interface: usize,
    pub reinstall_protocol_interface: usize,
    pub uninstall_protocol_interface: usize,
    pub handle_protocol: extern "efiapi" fn(handle: Handle, protocol: *const Guid, interface: *mut *mut c_void) -> Status,
    pub reserved: usize,
    pub register_protocol_notify: usize,
    pub locate_handle: usize,
    pub locate_device_path: usize,
    pub install_configuration_table: usize,
    pub load_image: usize,
    pub start_image: usize,
    pub exit: usize,
    pub unload_image: usize,
    pub exit_boot_services: extern "efiapi" fn(image: Handle, map_key: usize) -> Status,
    pub get_next_monotonic_count: usize,
    pub stall: usize,
    pub set_watchdog_timer: extern "efiapi" fn(timeout: usize, code: u64, data_size: usize, data: *const u16) -> Status,
    pub connect_controller: usize,
    pub disconnect_controller: usize,
    pub open_protocol: usize,
    pub close_protocol: usize,
    pub open_protocol_information: usize,
    pub protocols_per_handle: usize,
    pub locate_handle_buffer: extern "efiapi" fn(
        search_type: u32,
        protocol: *const Guid,
        search_key: *mut c_void,
        count: *mut usize,
        buffer: *mut *mut Handle,
    ) -> Status,
    pub locate_protocol:
        extern "efiapi" fn(protocol: *const Guid, registration: *mut c_void, interface: *mut *mut c_void) -> Status,
}

/// Runtime services stay valid after `ExitBootServices`.
#[repr(C)]
pub struct RuntimeServices {
    pub header: TableHeader,
    pub get_time: usize,
    pub set_time: usize,
    pub get_wakeup_time: usize,
    pub set_wakeup_time: usize,
    pub set_virtual_address_map: usize,
    pub convert_pointer: usize,
    pub get_variable: usize,
    pub get_next_variable_name: usize,
    pub set_variable: usize,
    pub get_next_high_monotonic_count: usize,
    pub reset_system: extern "efiapi" fn(kind: u32, status: Status, data_size: usize, data: *const c_void) -> !,
}

pub const RESET_SHUTDOWN: u32 = 2;

/// One entry of the memory map. `GetMemoryMap` reports the stride between
/// entries separately: the firmware may use a larger descriptor than this
/// structure, so entries must be walked by that stride, never by `size_of`.
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

/// Block I/O: sector reads on any block device the firmware has a driver for,
/// including every disk and partition. Only valid before `ExitBootServices`: the
/// kernel needs its own storage drivers (docs/design.md, section 8).
#[repr(C)]
pub struct BlockIo {
    pub revision: u64,
    pub media: *mut BlockIoMedia,
    pub reset: usize,
    pub read_blocks:
        extern "efiapi" fn(this: *mut BlockIo, media_id: u32, lba: u64, size: usize, buffer: *mut c_void) -> Status,
    pub write_blocks:
        extern "efiapi" fn(this: *mut BlockIo, media_id: u32, lba: u64, size: usize, buffer: *const c_void) -> Status,
    pub flush_blocks: extern "efiapi" fn(this: *mut BlockIo) -> Status,
}

#[repr(C)]
pub struct BlockIoMedia {
    pub media_id: u32,
    pub removable_media: bool,
    pub media_present: bool,
    /// True for a partition handle, false for a whole disk (the loader wants the
    /// whole disk, to read the GPT and any partition by LBA).
    pub logical_partition: bool,
    pub read_only: bool,
    pub write_caching: bool,
    pub block_size: u32,
    pub io_align: u32,
    pub last_block: u64,
}

#[repr(C)]
pub struct GraphicsOutput {
    pub query_mode: usize,
    pub set_mode: usize,
    pub blt: usize,
    pub mode: *mut GraphicsMode,
}

#[repr(C)]
pub struct GraphicsMode {
    pub max_mode: u32,
    pub mode: u32,
    pub info: *mut GraphicsModeInfo,
    pub info_size: usize,
    pub frame_buffer_base: u64,
    pub frame_buffer_size: usize,
}

#[repr(C)]
pub struct GraphicsModeInfo {
    pub version: u32,
    pub horizontal_resolution: u32,
    pub vertical_resolution: u32,
    /// 0 = red-green-blue-reserved, 1 = blue-green-red-reserved, 2 = a bit mask
    /// (see `pixel_information`), 3 = no linear framebuffer at all.
    pub pixel_format: u32,
    pub pixel_information: [u32; 4],
    pub pixels_per_scan_line: u32,
}

/// Human-readable name of a memory descriptor type (the loader narrates the map).
pub fn memory_type_name(kind: u32) -> &'static str {
    match kind {
        0 => "reserved",
        1 => "loader code",
        2 => "loader data",
        3 => "boot services code",
        4 => "boot services data",
        5 => "runtime services code",
        6 => "runtime services data",
        7 => "usable RAM",
        8 => "unusable (errors)",
        9 => "ACPI reclaimable",
        10 => "ACPI NVS",
        11 => "memory-mapped I/O",
        12 => "memory-mapped I/O port space",
        13 => "firmware (PAL code)",
        14 => "persistent memory",
        _ => "unknown",
    }
}
