//! The console and file system the shared shell (`crates/shell`: `ls cat write rm cp mv cmp mkdir`) runs on, for the server
//! kernel: output goes through `print!` (serial and the network terminal) and files through `kernel_core::tape_fs`, the same
//! references, volumes and error codes a tape sees, under the one disk lock (`tapefs::lock`). Same shape as the embedded
//! kernel's `shellhost.rs`; every call takes the lock itself, so a command that copies holds it only per transfer.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use fosfor_abi::tape::{FS_BAD_NAME, FS_NO_VOLUME, FS_NOT_FOUND, FS_TOO_BIG};
use fosfor_shell::{Entry, FsError, Host, Owner, Stamp};
use kernel_core::{mounts, tape_fs};

use crate::{storage, tapefs};

pub struct MfHost;

fn error(code: isize) -> FsError {
    match code {
        FS_NOT_FOUND => FsError::NotFound,
        FS_TOO_BIG => FsError::TooBig,
        FS_BAD_NAME => FsError::BadName,
        FS_NO_VOLUME => FsError::NoVolume,
        FS_READ_ONLY => FsError::Denied,
        _ => FsError::Failed,
    }
}

/// A bare name means the boot volume (`tape_fs` would also look on other disks for a tape's configuration file).
fn pin(reference: &str) -> Vec<u8> {
    let local = !reference.starts_with("//") && (!reference.contains('/') || (tape_fs::split_selector(reference.as_bytes()).0.is_empty() && mounts::lookup(reference.as_bytes()).is_none()));
    match storage::boot_disk().filter(|_| local) {
        Some(boot) => alloc::format!("disk{}/{reference}", boot + 1).into_bytes(),
        None => reference.as_bytes().to_vec(),
    }
}

impl Host for MfHost {
    fn print(&mut self, arguments: fmt::Arguments) {
        print!("{}", arguments);
    }

    fn read(&mut self, reference: &str, buffer: &mut [u8]) -> Result<usize, FsError> {
        let _disk = tapefs::lock();
        match tape_fs::read(&pin(reference), buffer) {
            length if length >= 0 => Ok(length as usize),
            code => Err(error(code)),
        }
    }

    fn length(&mut self, reference: &str) -> Result<u64, FsError> {
        let _disk = tapefs::lock();
        match tape_fs::length(&pin(reference)) {
            length if length >= 0 => Ok(length as u64),
            code => Err(error(code)),
        }
    }

    fn write(&mut self, reference: &str, data: &[u8]) -> Result<(), FsError> {
        let _disk = tapefs::lock();
        let is_fat = tape_fs::is_fat(reference.as_bytes());
        match tape_fs::write(reference.as_bytes(), data) {
            0 => Ok(()),
            _ if is_fat && data.is_empty() => Err(FsError::Empty),
            code => Err(error(code)),
        }
    }

    fn remove(&mut self, reference: &str) -> Result<(), FsError> {
        let _disk = tapefs::lock();
        match tape_fs::remove(reference.as_bytes()) {
            0 => Ok(()),
            code => Err(error(code)),
        }
    }

    fn mkdir(&mut self, reference: &str) -> Result<(), FsError> {
        let _disk = tapefs::lock();
        match tape_fs::mkdir(reference.as_bytes()) {
            0 => Ok(()),
            code => Err(error(code)),
        }
    }

    fn owner_of(&mut self, reference: &str) -> Result<Owner, FsError> {
        let _disk = tapefs::lock();
        let mut stat = fosfor_abi::tape::VfsStat::default();
        match kernel_core::tape_vfs::stat(reference.as_bytes(), &mut stat) {
            0 => Ok(Owner { uid: stat.uid, gid: stat.gid, mode: stat.mode, directory: stat.kind == fosfor_abi::tape::VFS_DIR, size: stat.size, links: stat.links }),
            code => Err(error(code)),
        }
    }

    fn set_owner(&mut self, reference: &str, uid: u32, gid: u32, mode: Option<u16>) -> Result<(), FsError> {
        let _disk = tapefs::lock();
        match kernel_core::tape_vfs::set_owner(reference.as_bytes(), uid, gid, mode.map_or(fosfor_abi::tape::VFS_KEEP_MODE, u32::from)) {
            0 => Ok(()),
            code => Err(error(code)),
        }
    }

    fn rename(&mut self, from: &str, to: &str) -> Result<bool, FsError> {
        let _disk = tapefs::lock();
        tape_fs::rename(from.as_bytes(), to.as_bytes()).map_err(error)
    }

    // The entries are collected under the lock and handed out after it is dropped: `each` prints, which may take a while on a
    // network terminal, and must not keep the disk from other CPUs.
    fn list(&mut self, volume: &str, each: &mut dyn FnMut(&mut dyn Host, Entry)) -> Result<&'static str, FsError> {
        let mut entries: Vec<(Vec<u8>, u64, bool, Option<Stamp>)> = Vec::new();
        let kind = {
            let _disk = tapefs::lock();
            tape_fs::each_entry(volume.as_bytes(), &mut |name, length, directory, modified| {
                let modified = modified.map(|time| Stamp { year: time.year, month: time.month, day: time.day, hour: time.hour, minute: time.minute, second: time.second });
                entries.push((name.to_vec(), length, directory, modified));
            })
            .map_err(error)?
        };
        for (name, length, directory, modified) in &entries {
            each(self, Entry { name, length: *length, directory: *directory, modified: *modified });
        }
        Ok(kind)
    }

    // Probed by listing: a disk number that is not there fails at once.
    fn volumes(&mut self, each: &mut dyn FnMut(&str)) {
        let mut found: Vec<String> = Vec::new();
        {
            let _disk = tapefs::lock();
            for kind in ["disk", "ext"] {
                for number in 1..=8 {
                    let name = alloc::format!("{kind}{number}");
                    if tape_fs::each_entry(name.as_bytes(), &mut |_, _, _, _| {}).is_ok() {
                        found.push(name);
                    }
                }
            }
        }
        for name in &found {
            each(name);
        }
    }

    fn with_buffer(&mut self, size: usize, run: &mut dyn FnMut(&mut dyn Host, &mut [u8])) -> bool {
        let mut buffer: Vec<u8> = Vec::new();
        if buffer.try_reserve_exact(size).is_err() {
            return false;
        }
        buffer.resize(size, 0);
        run(self, &mut buffer);
        true
    }

    fn exists(&mut self, reference: &str) -> bool {
        let _disk = tapefs::lock();
        tape_fs::length(reference.as_bytes()) >= 0
    }

    fn run_script(&mut self, reference: &str, _name: &str, arguments: &str) -> bool {
        crate::script::run_file(reference, arguments)
    }

    fn run_tape(&mut self, line: &str) {
        let (name, args) = fosfor_shell::split_word(line);
        crate::shell::run_tape(name, args);
    }

    fn tape_help(&mut self, reference: &str) -> bool {
        const SIZE: usize = 512 * 1024;
        let mut shown = false;
        self.with_buffer(SIZE, &mut |host, image| {
            let Ok(length) = host.read(reference, image) else { return };
            if let Some(text) = fosfor_abi::elf::section(&image[..length], ".fos_help").and_then(|text| core::str::from_utf8(text).ok()) {
                println!("{text}");
                shown = true;
            }
        });
        shown
    }
}
