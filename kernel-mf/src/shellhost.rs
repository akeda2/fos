//! The console and file system the shared shell (`crates/shell`: `ls cat write rm cp mv cmp mkdir`) runs on, for the server
//! kernel: output goes through `print!` (serial and the network terminal) and files through `kernel_core::tape_fs`, the same
//! references, volumes and error codes a tape sees, under the one disk lock (`tapefs::lock`). Same shape as the embedded
//! kernel's `shellhost.rs`; every call takes the lock itself, so a command that copies holds it only per transfer.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use fosfor_abi::tape::{FS_BAD_NAME, FS_NO_VOLUME, FS_NOT_FOUND, FS_READ_ONLY, FS_TOO_BIG};
use fosfor_shell::{Entry, FsError, Host, Owner, Stamp};
use kernel_core::{mounts, tape_fs};

use crate::{storage, tapefs};

pub struct MfHost;

const STREAM_CHUNK: usize = 1 << 20;

/// The shell's working folder (`cd`): a reference like `@rackdata/www-root`, empty for the boot volume's root. Relative names
/// the file commands take are looked up under it; names with a volume (`diskN/`, `@LABEL/`, `extN/`, `esp/`, `//`) are not.
static CWD: crate::sched::SleepMutex<String> = crate::sched::SleepMutex::new(String::new());
/// Set by a new remote terminal session (the receive path must not take `CWD`, which can sleep): it starts at the root.
pub static CWD_RESET: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

fn current() -> String {
    let mut cwd = CWD.lock();
    if CWD_RESET.swap(false, core::sync::atomic::Ordering::Relaxed) {
        cwd.clear();
    }
    cwd.clone()
}

/// The first component names a volume: `@LABEL`, `diskN`, `extN` or `esp`.
fn names_volume(reference: &str) -> bool {
    let first = reference.split('/').next().unwrap_or("");
    let numbered = |prefix: &str| first.strip_prefix(prefix).is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()));
    first == "esp" || first.strip_prefix('@').is_some_and(|label| !label.is_empty()) || numbered("disk") || numbered("ext")
}

/// `.` and `..` resolved, empty components dropped.
fn normalize(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    parts.join("/")
}

/// `reference` as the file calls should see it: under the working folder when it is a relative name (`/NAME` is the root of the
/// working folder's volume). An empty working folder, `//` references and names that carry a volume stay as typed.
fn here(reference: &str) -> String {
    let cwd = current();
    if cwd.is_empty() || reference.starts_with("//") || names_volume(reference) {
        return String::from(reference);
    }
    if let Some(absolute) = reference.strip_prefix('/') {
        // The root of the working folder's volume: its selector, or the mount point it lies under.
        let volume = if names_volume(&cwd) {
            cwd.split('/').next().unwrap_or("")
        } else {
            mounts::lookup_dir(cwd.as_bytes()).map_or("", |(_, rest)| cwd[..cwd.len() - rest.len()].trim_end_matches('/'))
        };
        return normalize(&alloc::format!("{volume}/{absolute}"));
    }
    if reference.is_empty() {
        return cwd;
    }
    normalize(&alloc::format!("{cwd}/{reference}"))
}

pub fn working_folder() -> String {
    current()
}

/// `cd [FOLDER]`: with no name the boot volume's root. The folder must exist (a listing of it works). Returns the new folder.
pub fn change_folder(target: &str) -> Result<String, FsError> {
    let resolved = if target.is_empty() { String::new() } else { here(target) };
    if !resolved.is_empty() {
        MfHost.list(&resolved, &mut |_, _| {})?;
    }
    CWD_RESET.store(false, core::sync::atomic::Ordering::Relaxed);
    *CWD.lock() = resolved.clone();
    Ok(resolved)
}

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
    pin_raw(&here(reference))
}

fn pin_raw(reference: &str) -> Vec<u8> {
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
        let reference = &here(reference);
        let _disk = tapefs::lock();
        let is_fat = tape_fs::is_fat(reference.as_bytes());
        match tape_fs::write(reference.as_bytes(), data) {
            0 => Ok(()),
            _ if is_fat && data.is_empty() => Err(FsError::Empty),
            code => Err(error(code)),
        }
    }

    fn remove(&mut self, reference: &str) -> Result<(), FsError> {
        let reference = &here(reference);
        let _disk = tapefs::lock();
        if kernel_core::tape_vfs::is_ext_reference(reference.as_bytes()) {
            return match kernel_core::tape_vfs::remove(reference.as_bytes()) {
                0 => Ok(()),
                code => Err(error(code)),
            };
        }
        match tape_fs::remove(reference.as_bytes()) {
            0 => Ok(()),
            code => Err(error(code)),
        }
    }

    fn mkdir(&mut self, reference: &str) -> Result<(), FsError> {
        let reference = &here(reference);
        let _disk = tapefs::lock();
        match tape_fs::mkdir(reference.as_bytes()) {
            0 => Ok(()),
            code => Err(error(code)),
        }
    }

    fn owner_of(&mut self, reference: &str) -> Result<Owner, FsError> {
        let reference = &here(reference);
        let _disk = tapefs::lock();
        let mut stat = fosfor_abi::tape::VfsStat::default();
        match kernel_core::tape_vfs::stat(reference.as_bytes(), &mut stat) {
            0 => Ok(Owner { uid: stat.uid, gid: stat.gid, mode: stat.mode, directory: stat.kind == fosfor_abi::tape::VFS_DIR, size: stat.size, links: stat.links }),
            code => Err(error(code)),
        }
    }

    fn set_owner(&mut self, reference: &str, uid: u32, gid: u32, mode: Option<u16>) -> Result<(), FsError> {
        let reference = &here(reference);
        let _disk = tapefs::lock();
        match kernel_core::tape_vfs::set_owner(reference.as_bytes(), uid, gid, mode.map_or(fosfor_abi::tape::VFS_KEEP_MODE, u32::from)) {
            0 => Ok(()),
            code => Err(error(code)),
        }
    }

    fn rename(&mut self, from: &str, to: &str) -> Result<bool, FsError> {
        let (from, to) = (&here(from), &here(to));
        let _disk = tapefs::lock();
        if kernel_core::tape_vfs::is_ext_reference(from.as_bytes()) && kernel_core::tape_vfs::is_ext_reference(to.as_bytes()) {
            return match kernel_core::tape_vfs::rename(from.as_bytes(), to.as_bytes()) {
                0 => Ok(true),
                code => Err(error(code)),
            };
        }
        tape_fs::rename(from.as_bytes(), to.as_bytes()).map_err(error)
    }

    // The entries are collected under the lock and handed out after it is dropped: `each` prints, which may take a while on a
    // network terminal, and must not keep the disk from other CPUs.
    fn list(&mut self, volume: &str, each: &mut dyn FnMut(&mut dyn Host, Entry)) -> Result<&'static str, FsError> {
        let volume = &here(volume);
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

    // Big files go piece by piece (1 MiB), the disk lock taken per piece. The destination must be an ext volume (the only
    // one `vfs_write` reaches); the source can be on any volume `vfs_read` serves.
    fn copy_stream(&mut self, from: &str, to: &str) -> Option<Result<u64, FsError>> {
        use fosfor_abi::tape::{VFS_FILE, VfsMake, VfsStat};
        use kernel_core::tape_vfs;
        let to = &here(to);
        if !tape_vfs::is_ext_reference(to.as_bytes()) {
            return None;
        }
        let from = pin(from);
        let mut chunk: Vec<u8> = Vec::new();
        if chunk.try_reserve_exact(STREAM_CHUNK).is_err() {
            return Some(Err(FsError::Failed));
        }
        chunk.resize(STREAM_CHUNK, 0);
        let prepared = {
            let _disk = tapefs::lock();
            let mut info = VfsStat::default();
            match tape_vfs::stat(to.as_bytes(), &mut info) {
                0 if info.kind == VFS_FILE => tape_vfs::truncate(to.as_bytes(), 0),
                0 => fosfor_abi::tape::FS_ERROR,
                FS_NOT_FOUND => tape_vfs::create(to.as_bytes(), &VfsMake { kind: VFS_FILE, mode: 0o644, uid: 0, gid: 0 }),
                code => code,
            }
        };
        if prepared < 0 {
            return Some(Err(error(prepared)));
        }
        let mut offset = 0u64;
        loop {
            let _disk = tapefs::lock();
            let count = match tape_vfs::read(&from, offset, &mut chunk) {
                count if count < 0 => return Some(Err(error(count))),
                0 => break,
                count => count as usize,
            };
            let mut done = 0;
            while done < count {
                match tape_vfs::write(to.as_bytes(), offset + done as u64, &chunk[done..count]) {
                    written if written <= 0 => return Some(Err(if written < 0 { error(written) } else { FsError::Failed })),
                    written => done += written as usize,
                }
            }
            offset += count as u64;
        }
        let _disk = tapefs::lock();
        match tape_vfs::api_vfs_sync() {
            code if code < 0 => Some(Err(error(code))),
            _ => Some(Ok(offset)),
        }
    }

    fn compare_stream(&mut self, a: &str, b: &str) -> Option<Result<bool, FsError>> {
        use kernel_core::tape_vfs;
        let (a, b) = (pin(a), pin(b));
        let mut left: Vec<u8> = Vec::new();
        let mut right: Vec<u8> = Vec::new();
        if left.try_reserve_exact(STREAM_CHUNK).is_err() || right.try_reserve_exact(STREAM_CHUNK).is_err() {
            return Some(Err(FsError::Failed));
        }
        left.resize(STREAM_CHUNK, 0);
        right.resize(STREAM_CHUNK, 0);
        let mut offset = 0u64;
        loop {
            let _disk = tapefs::lock();
            let (x, y) = (tape_vfs::read(&a, offset, &mut left), tape_vfs::read(&b, offset, &mut right));
            if x < 0 || y < 0 {
                return Some(Err(error(x.min(y))));
            }
            if x != y || left[..x as usize] != right[..y as usize] {
                return Some(Ok(false));
            }
            if x == 0 {
                return Some(Ok(true));
            }
            offset += x as u64;
        }
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
        const SIZE: usize = 4 * 1024 * 1024;
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
