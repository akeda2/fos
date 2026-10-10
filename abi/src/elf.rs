//! Loading a position-independent ELF64 executable (`ET_DYN`, as `-pie` links one) into
//! memory the caller has chosen: copy the `PT_LOAD` segments, clear the BSS, apply the
//! `R_X86_64_RELATIVE` relocations. Shared by the UEFI loader (which loads the kernel)
//! and the kernel (which loads tapes), so both agree on what an acceptable image is.
//!
//! Position independence is what makes loading robust: the image is linked at address
//! 0 and lands wherever memory is free, so no fixed address has to be available on
//! every firmware and every machine.

#[derive(Debug)]
pub enum Error {
    NotElf64X86,
    NotAnExecutable,
    OutOfRange,
    /// A relocation type other than R_X86_64_RELATIVE: nothing here resolves symbols.
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

const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;

struct Headers {
    start: usize,
    size: usize,
    count: usize,
}

fn headers(image: &[u8]) -> Result<Headers, Error> {
    const ET_DYN: u16 = 3;
    if image.len() < 64 || image[0..4] != [0x7f, b'E', b'L', b'F'] || image[4] != 2 || image[5] != 1 || read_u16(image, 18) != 62 {
        return Err(Error::NotElf64X86);
    }
    if read_u16(image, 16) != ET_DYN {
        return Err(Error::NotAnExecutable);
    }
    let headers = Headers { start: read_u64(image, 32) as usize, size: read_u16(image, 54) as usize, count: read_u16(image, 56) as usize };
    if headers.size < 56 || headers.start.saturating_add(headers.size.saturating_mul(headers.count)) > image.len() {
        return Err(Error::OutOfRange);
    }
    Ok(headers)
}

impl Headers {
    fn at(&self, index: usize) -> usize {
        self.start + index * self.size
    }
}

/// The bytes of the section called `name` (a tape's `.fos_help` text, say), read from the section header table of the
/// file. None if the file has no such section or its tables are damaged; never panics on a bad file.
pub fn section<'a>(image: &'a [u8], name: &str) -> Option<&'a [u8]> {
    headers(image).ok()?;
    let table = read_u64(image, 40) as usize;
    let (entry, count, names) = (read_u16(image, 58) as usize, read_u16(image, 60) as usize, read_u16(image, 62) as usize);
    let at = |index: usize| -> Option<usize> {
        let start = table.checked_add(index.checked_mul(entry)?)?;
        (entry >= 40 && start.checked_add(entry)? <= image.len()).then_some(start)
    };
    let range = |header: usize| -> Option<(usize, usize)> {
        let (offset, size) = (read_u64(image, header + 24) as usize, read_u64(image, header + 32) as usize);
        (offset.checked_add(size)? <= image.len()).then_some((offset, size))
    };
    let (string_start, string_size) = range(at(names)?)?;
    let strings = &image[string_start..string_start + string_size];
    for index in 0..count {
        let header = at(index)?;
        let from = read_u32(image, header) as usize;
        let Some(tail) = strings.get(from..) else { continue };
        let length = tail.iter().position(|&byte| byte == 0).unwrap_or(tail.len());
        if &tail[..length] == name.as_bytes() {
            let (offset, size) = range(header)?;
            return Some(&image[offset..offset + size]);
        }
    }
    None
}

/// How many bytes of memory the image needs (its highest segment end, page-rounded).
pub fn span(image: &[u8]) -> Result<usize, Error> {
    let headers = headers(image)?;
    let mut span = 0u64;
    for index in 0..headers.count {
        let at = headers.at(index);
        if read_u32(image, at) == PT_LOAD {
            span = span.max(read_u64(image, at + 16).saturating_add(read_u64(image, at + 40)));
        }
    }
    if span == 0 || span > 1 << 32 {
        return Err(Error::OutOfRange);
    }
    Ok(((span + 0xfff) & !0xfff) as usize)
}

/// Loads `image` at `base` (which must have `span(image)` writable bytes) and returns
/// the entry point's address.
///
/// # Safety
/// `base` must point to at least `span(image)` bytes the caller owns.
pub unsafe fn load(image: &[u8], base: u64) -> Result<u64, Error> {
    let headers = headers(image)?;
    let mut dynamic = None;
    for index in 0..headers.count {
        let at = headers.at(index);
        let offset = read_u64(image, at + 8) as usize;
        let virtual_address = read_u64(image, at + 16);
        let file_size = read_u64(image, at + 32) as usize;
        let memory_size = read_u64(image, at + 40) as usize;
        match read_u32(image, at) {
            PT_LOAD => {
                if offset.saturating_add(file_size) > image.len() || file_size > memory_size {
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
        unsafe { apply_relocations(base, dynamic)? };
    }
    Ok(base + read_u64(image, 24))
}

/// Walks the loaded `.dynamic` section for the relocation table (`DT_RELA`, `DT_RELASZ`)
/// and applies every `R_X86_64_RELATIVE` entry: the word at `base + offset` becomes
/// `base + addend`. Those are the only relocations a statically linked
/// position-independent executable needs.
unsafe fn apply_relocations(base: u64, dynamic: u64) -> Result<(), Error> {
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
        if info as u32 != R_X86_64_RELATIVE {
            return Err(Error::UnsupportedRelocation(info as u32));
        }
        unsafe { ((base + offset) as *mut u64).write(base + addend) };
    }
    Ok(())
}
