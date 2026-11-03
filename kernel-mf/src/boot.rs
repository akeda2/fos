//! What the loader handed over, copied out of the loader's memory.
//!
//! The kernel switches to its own page tables, and loader data (the `BootInfo` page, the memory map) is not mapped in
//! them, so everything the kernel still needs afterwards is copied here first.

use fosfor_abi::{BootInfo, Framebuffer, MemoryDescriptor};

/// UEFI maps are a few hundred descriptors; 1024 leaves room for fragmented firmware.
const MAX_DESCRIPTORS: usize = 1024;

pub struct Machine {
    pub kernel_base: u64,
    pub kernel_size: u64,
    pub acpi_rsdp: u64,
    pub framebuffer: Framebuffer,
    /// 0 on a BIOS boot (the loader has no firmware services to leave behind).
    pub runtime_services: u64,
    /// Where the loader keeps `BootInfo` and the map: reserved from the frame allocator.
    pub info_page: u64,
    pub map: (u64, u64),
    count: usize,
    descriptors: [MemoryDescriptor; MAX_DESCRIPTORS],
}

impl Machine {
    /// Copies `info` and its memory map. `None` if the map has more descriptors than fit.
    pub fn from_info(info: &BootInfo) -> Option<Self> {
        let empty = MemoryDescriptor { kind: 0, padding: 0, physical_start: 0, virtual_start: 0, pages: 0, attribute: 0 };
        let mut machine = Machine {
            kernel_base: info.kernel_base,
            kernel_size: info.kernel_size,
            acpi_rsdp: info.acpi_rsdp,
            framebuffer: info.framebuffer,
            runtime_services: info.runtime_services,
            info_page: info as *const BootInfo as u64,
            map: (info.memory_map, info.memory_map_count * info.memory_map_descriptor_size),
            count: 0,
            descriptors: [empty; MAX_DESCRIPTORS],
        };
        for d in info.memory_descriptors() {
            if machine.count == MAX_DESCRIPTORS {
                return None;
            }
            machine.descriptors[machine.count] = d;
            machine.count += 1;
        }
        Some(machine)
    }

    pub fn descriptors(&self) -> impl Iterator<Item = MemoryDescriptor> + '_ {
        self.descriptors[..self.count].iter().copied()
    }
}

static mut REMEMBERED: *const Machine = core::ptr::null();

/// Keeps a pointer to the boot copy for the `memmap` and `bootinfo` shell commands. `kmain` never returns, so its `Machine`
/// lives as long as the kernel does.
pub fn remember(machine: &Machine) {
    unsafe { REMEMBERED = machine };
}

pub fn machine() -> Option<&'static Machine> {
    unsafe { REMEMBERED.as_ref() }
}
