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
    let status = (boot.allocate_pages)(uefi::ALLOCATE_ANY_PAGES, uefi::LOADER_DATA, (span / 4096) as usize, &mut base);
    if status != uefi::SUCCESS {
        return Err(Error::PagesUnavailable { status });
    }
    let entry = unsafe { fosfor_abi::elf::load(image, base) }.map_err(Error::Elf)?;
    Ok(LoadedKernel { entry, base, size: span })
}
