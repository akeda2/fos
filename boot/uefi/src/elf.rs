//! A minimal ELF64 loader for a position-independent kernel (`ET_DYN`): the loader
//! chooses where in physical memory the image lands, copies the `PT_LOAD` segments
//! there, applies the `R_X86_64_RELATIVE` relocations and reports the entry point.
//!
//! Position independence is what makes the load robust on real firmware: the
//! first version linked the kernel at a fixed address and OVMF refused it (its own
//! boot-services data sits there); a fixed address would fare no better on other
//! machines, whose maps all differ.

use crate::uefi::{self, BootServices};

pub struct LoadedKernel {
    pub entry: u64,
    pub base: u64,
    pub size: u64,
}

// The payloads are only shown through Debug when the loader narrates a failure.
#[allow(dead_code)]
#[derive(Debug)]
pub enum Error {
    Elf(fosfor_abi::elf::Error),
    /// The firmware would not give us enough pages.
    PagesUnavailable { status: usize },
}

/// Loads `image` and returns where it ended up.
pub fn load(boot: &BootServices, image: &[u8]) -> Result<LoadedKernel, Error> {
    let span = fosfor_abi::elf::span(image).map_err(Error::Elf)? as u64;
    let mut base = 0u64;
    // aarch64 firmware may map LoaderData no-execute; the kernel image is code the firmware must allow to run.
    #[cfg(target_arch = "aarch64")]
    let memory_type = uefi::LOADER_CODE;
    #[cfg(not(target_arch = "aarch64"))]
    let memory_type = uefi::LOADER_DATA;
    let status = (boot.allocate_pages)(uefi::ALLOCATE_ANY_PAGES, memory_type, (span / 4096) as usize, &mut base);
    if status != uefi::SUCCESS {
        return Err(Error::PagesUnavailable { status });
    }
    let entry = unsafe { fosfor_abi::elf::load(image, base) }.map_err(Error::Elf)?;
    sync_instruction_cache(base, span);
    Ok(LoadedKernel { entry, base, size: span })
}

/// The loader wrote the kernel's code through the data cache; the CPU fetches instructions through its own. Clean the data
/// cache to the point of unification, invalidate the instruction cache and flush the pipeline, or the jump may run stale bytes.
#[cfg(target_arch = "aarch64")]
fn sync_instruction_cache(start: u64, length: u64) {
    const LINE: u64 = 64;
    let mut address = start & !(LINE - 1);
    while address < start + length {
        unsafe { core::arch::asm!("dc cvau, {}", in(reg) address, options(nostack)) };
        address += LINE;
    }
    unsafe { core::arch::asm!("dsb ish", "ic ialluis", "dsb ish", "isb", options(nostack)) };
}

#[cfg(not(target_arch = "aarch64"))]
fn sync_instruction_cache(_start: u64, _length: u64) {}
