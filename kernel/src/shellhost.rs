//! The console and file system the shared shell (`crates/shell`) runs on, for both ports: output goes where `print` goes
//! (the screen, the serial port, a `term net` session) and files are reached through `tape_fs`, the same references and
//! volumes a tape sees.

use core::fmt;

use fosfor_shell::{Entry, FsError, Host, Stamp};

use crate::{memory, tape_fs};

pub struct KernelHost;

/// An `FS_*` code from `tape_fs` as the shell's error.
fn error(code: isize) -> FsError {
    use fosfor_abi::tape::*;
    match code {
        FS_NOT_FOUND => FsError::NotFound,
        FS_TOO_BIG => FsError::TooBig,
        FS_BAD_NAME => FsError::BadName,
        FS_NO_VOLUME => FsError::NoVolume,
        _ => FsError::Failed,
    }
}

/// A bare name means the boot volume. `tape_fs` would also look on other disks for a tape's configuration file; the
/// shell does not, so `cat NAME` says "no such file" for a file that lives on another disk.
fn pin<'a>(reference: &'a str, storage: &'a mut [u8; 96]) -> &'a [u8] {
    let Some(boot) = crate::storage::boot_disk().filter(|_| !reference.contains('/')) else { return reference.as_bytes() };
    let mut cursor = Cursor(storage, 0);
    if fmt::write(&mut cursor, format_args!("disk{}/{reference}", boot + 1)).is_err() {
        return reference.as_bytes();
    }
    let end = cursor.1;
    &storage[..end]
}

struct Cursor<'a>(&'a mut [u8], usize);

impl fmt::Write for Cursor<'_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let end = self.1 + text.len();
        self.0.get_mut(self.1..end).ok_or(fmt::Error)?.copy_from_slice(text.as_bytes());
        self.1 = end;
        Ok(())
    }
}

impl Host for KernelHost {
    fn print(&mut self, arguments: fmt::Arguments) {
        crate::print(arguments);
    }

    fn read(&mut self, reference: &str, buffer: &mut [u8]) -> Result<usize, FsError> {
        match tape_fs::read(pin(reference, &mut [0; 96]), buffer) {
            length if length >= 0 => Ok(length as usize),
            code => Err(error(code)),
        }
    }

    fn length(&mut self, reference: &str) -> Result<u64, FsError> {
        match tape_fs::length(pin(reference, &mut [0; 96])) {
            length if length >= 0 => Ok(length as u64),
            code => Err(error(code)),
        }
    }

    fn write(&mut self, reference: &str, data: &[u8]) -> Result<(), FsError> {
        let is_fat = tape_fs::is_fat(reference.as_bytes());
        match tape_fs::write(reference.as_bytes(), data) {
            0 => Ok(()),
            _ if is_fat && data.is_empty() => Err(FsError::Empty),
            code => Err(error(code)),
        }
    }

    fn remove(&mut self, reference: &str) -> Result<(), FsError> {
        match tape_fs::remove(reference.as_bytes()) {
            0 => Ok(()),
            code => Err(error(code)),
        }
    }

    fn list(&mut self, volume: &str, each: &mut dyn FnMut(&mut dyn Host, Entry)) -> Result<&'static str, FsError> {
        tape_fs::each_entry(volume.as_bytes(), &mut |name, length, directory, modified| {
            let modified = modified.map(|time| Stamp { year: time.year, month: time.month, day: time.day, hour: time.hour, minute: time.minute, second: time.second });
            each(&mut KernelHost, Entry { name, length, directory, modified });
        })
        .map_err(error)
    }

    fn with_buffer(&mut self, size: usize, run: &mut dyn FnMut(&mut dyn Host, &mut [u8])) -> bool {
        let frames = (size as u64).div_ceil(memory::FRAME);
        let Some(address) = memory::allocate_contiguous(frames, None) else { return false };
        run(self, unsafe { core::slice::from_raw_parts_mut(address as *mut u8, size) });
        memory::free(address, frames);
        true
    }

    fn exists(&mut self, reference: &str) -> bool {
        tape_fs::length(reference.as_bytes()) >= 0
    }

    #[cfg(target_arch = "x86_64")]
    fn run_script(&mut self, reference: &str, name: &str, arguments: &str) -> bool {
        crate::term::run_script_file(reference, name, arguments)
    }

    fn run_tape(&mut self, line: &str) {
        #[cfg(target_arch = "x86_64")]
        crate::tape::run_named(line);
        #[cfg(target_arch = "aarch64")]
        crate::shell::run_tape(line);
    }

    fn help_extra(&mut self) {
        #[cfg(target_arch = "x86_64")]
        crate::term::help_extra();
    }

    fn tape_help(&mut self, reference: &str) -> bool {
        const SIZE: usize = 512 * 1024;
        let mut shown = false;
        self.with_buffer(SIZE, &mut |host, image| {
            let Ok(length) = host.read(reference, image) else { return };
            if let Some(text) = fosfor_abi::elf::section(&image[..length], ".fos_help").and_then(|text| core::str::from_utf8(text).ok()) {
                crate::println!("{text}");
                shown = true;
            }
        });
        shown
    }
}
