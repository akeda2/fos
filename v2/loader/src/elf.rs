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
    NotElf64X86,
    NotAnExecutable,
    OutOfRange,
    /// The firmware would not give us enough pages.
    PagesUnavailable { status: usize },
    /// A relocation type other than R_X86_64_RELATIVE: the loader does not know it.
    UnsupportedRelocation(u32),
}

fn read_u16(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap())
}
fn read_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}
fn read_u64(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

/// Loads `image` and returns where it ended up.
pub fn load(boot: &BootServices, image: &[u8]) -> Result<LoadedKernel, Error> {
    const ET_DYN: u16 = 3;
    const PT_LOAD: u32 = 1;
    const PT_DYNAMIC: u32 = 2;
    if image.len() < 64 || image[0..4] != [0x7f, b'E', b'L', b'F'] || image[4] != 2 || image[5] != 1 || read_u16(image, 18) != 62 {
        return Err(Error::NotElf64X86);
    }
    if read_u16(image, 16) != ET_DYN {
        return Err(Error::NotAnExecutable);
    }
    let entry = read_u64(image, 24);
    let program_headers = read_u64(image, 32) as usize;
    let header_size = read_u16(image, 54) as usize;
    let header_count = read_u16(image, 56) as usize;
    let header = |index: usize| -> Result<usize, Error> {
        let at = program_headers + index * header_size;
        if at + 56 > image.len() { Err(Error::OutOfRange) } else { Ok(at) }
    };

    // The image's extent in its own (zero-based) address space.
    let mut span = 0u64;
    for index in 0..header_count {
        let at = header(index)?;
        if read_u32(image, at) == PT_LOAD {
            span = span.max(read_u64(image, at + 16) + read_u64(image, at + 40));
        }
    }
    if span == 0 {
        return Err(Error::OutOfRange);
    }
    let span = (span + 0xfff) & !0xfff;
    let mut base = 0u64;
    let status = (boot.allocate_pages)(uefi::ALLOCATE_ANY_PAGES, uefi::LOADER_DATA, (span / 4096) as usize, &mut base);
    if status != uefi::SUCCESS {
        return Err(Error::PagesUnavailable { status });
    }

    let mut dynamic = None;
    for index in 0..header_count {
        let at = header(index)?;
        let kind = read_u32(image, at);
        let offset = read_u64(image, at + 8) as usize;
        let virtual_address = read_u64(image, at + 16);
        let file_size = read_u64(image, at + 32) as usize;
        let memory_size = read_u64(image, at + 40) as usize;
        match kind {
            PT_LOAD => {
                if offset + file_size > image.len() {
                    return Err(Error::OutOfRange);
                }
                unsafe {
                    let target = (base + virtual_address) as *mut u8;
                    core::ptr::copy_nonoverlapping(image.as_ptr().add(offset), target, file_size);
                    core::ptr::write_bytes(target.add(file_size), 0, memory_size - file_size);
                }
            }
            PT_DYNAMIC => dynamic = Some(virtual_address),
            _ => {}
        }
    }
    if let Some(dynamic) = dynamic {
        apply_relocations(base, dynamic)?;
    }
    Ok(LoadedKernel { entry: base + entry, base, size: span })
}

/// Walks the loaded `.dynamic` section for the relocation table (`DT_RELA`,
/// `DT_RELASZ`) and applies every `R_X86_64_RELATIVE` entry: the word at
/// `base + offset` becomes `base + addend`. Those are the only relocations a
/// statically linked position-independent kernel needs.
fn apply_relocations(base: u64, dynamic: u64) -> Result<(), Error> {
    const DT_NULL: u64 = 0;
    const DT_RELA: u64 = 7;
    const DT_RELASZ: u64 = 8;
    const R_X86_64_RELATIVE: u32 = 8;
    let (mut table, mut size) = (0u64, 0u64);
    let mut entry = (base + dynamic) as *const u64;
    loop {
        let (tag, value) = unsafe { (entry.read(), entry.add(1).read()) };
        match tag {
            DT_NULL => break,
            DT_RELA => table = value,
            DT_RELASZ => size = value,
            _ => {}
        }
        entry = unsafe { entry.add(2) };
    }
    for index in 0..(size / 24) {
        let record = (base + table + index * 24) as *const u64;
        let (offset, info, addend) = unsafe { (record.read(), record.add(1).read(), record.add(2).read()) };
        let kind = info as u32;
        if kind != R_X86_64_RELATIVE {
            return Err(Error::UnsupportedRelocation(kind));
        }
        unsafe { ((base + offset) as *mut u64).write(base + addend) };
    }
    Ok(())
}
