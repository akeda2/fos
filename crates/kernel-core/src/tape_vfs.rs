//! The tape `vfs_*` calls (ABI 32): what a file or folder is and what is in it, for every volume a tape can name, ext2/3/4
//! included (`tape_fs` knows FOSFS and FAT only). Shared by both kernels.
//!
//! A reference is the one `fs_read` takes. `diskN/path` on a disk with no FOSFS and no FAT volume is that disk's ext volume
//! (found the way the shell's `ext4` command does and kept mounted, `ext4vol::shared`); everything else is answered from the
//! `tape_fs` calls, so FOSFS and FAT volumes show up here too, without owners and times. The caller serialises calls (the
//! kernel's file lock), as for `tape_fs`.

use fosfor_abi::tape::*;

use crate::ext4vol::{self, Cursor, Shared};
use crate::tape_fs;
use crate::volumes as storage;

fn ext4_error(error: ext4::Error) -> isize {
    match error {
        ext4::Error::NotFound | ext4::Error::NotADirectory => FS_NOT_FOUND,
        ext4::Error::InvalidName => FS_BAD_NAME,
        _ => FS_ERROR,
    }
}

/// The ext volume a reference names and the path on it, or `None` when it names something else.
fn ext(reference: &[u8]) -> Option<(&'static mut Shared, &[u8])> {
    let (selector, path) = tape_fs::split_selector(reference);
    let number = tape_fs::disk_number(selector).filter(|&number| number > 0)?;
    // A disk that already is the kept ext volume skips the FOSFS and FAT probes (a few reads each).
    let known = ext4vol::shared_disk() == Some(number - 1);
    if !known && (storage::volume_on(number).is_ok() || tape_fs::fat_disk(number).is_some()) {
        return None;
    }
    ext4vol::shared(number - 1).ok().map(|shared| (shared, path))
}

fn stat_of(inode: &ext4::Inode) -> VfsStat {
    VfsStat {
        kind: if inode.is_dir() {
            VFS_DIR
        } else if inode.is_symlink() {
            VFS_LINK
        } else {
            VFS_FILE
        },
        mode: u32::from(inode.permissions()),
        size: inode.size,
        mtime: u64::from(inode.mtime),
        ctime: u64::from(inode.ctime),
        atime: u64::from(inode.atime),
        uid: inode.uid,
        gid: inode.gid,
        links: u32::from(inode.links),
        reserved: 0,
        inode: u64::from(inode.number),
    }
}

fn plain(directory: bool, size: u64) -> VfsStat {
    VfsStat { kind: if directory { VFS_DIR } else { VFS_FILE }, mode: if directory { 0o755 } else { 0o644 }, size, links: 1, ..VfsStat::default() }
}

/// `vfs_stat`: 0 and the `VfsStat`, or an `FS_*` code.
pub fn stat(reference: &[u8], out: &mut VfsStat) -> isize {
    if let Some((shared, path)) = ext(reference) {
        return match shared.fs.resolve(path) {
            Ok(Some(inode)) => {
                *out = stat_of(&inode);
                0
            }
            Ok(None) => FS_NOT_FOUND,
            Err(error) => ext4_error(error),
        };
    }
    let length = tape_fs::length(reference);
    if length >= 0 {
        *out = plain(false, length as u64);
        return 0;
    }
    match tape_fs::each_entry(reference, &mut |_, _, _, _| {}) {
        Ok(_) => {
            *out = plain(true, 0);
            0
        }
        Err(code) => code,
    }
}

/// `vfs_read_dir`: 1 and the entry, 0 past the end, or an `FS_*` code.
pub fn read_dir(reference: &[u8], index: usize, out: &mut VfsEntry) -> isize {
    if let Some((shared, path)) = ext(reference) {
        return ext_read_dir(shared, path, index, out);
    }
    let (mut position, mut found) = (0, false);
    let outcome = tape_fs::each_entry(reference, &mut |name, length, directory, _| {
        if position == index && !found {
            found = true;
            let shown = name.len().min(VFS_NAME_MAX);
            *out = VfsEntry { stat: plain(directory, length), name_length: shown as u32, ..VfsEntry::default() };
            out.name[..shown].copy_from_slice(&name[..shown]);
        }
        position += 1;
    });
    match outcome {
        Ok(_) => isize::from(found),
        Err(code) => code,
    }
}

fn ext_read_dir(shared: &mut Shared, path: &[u8], index: usize, out: &mut VfsEntry) -> isize {
    let folder = match shared.fs.resolve(path) {
        Ok(Some(inode)) if inode.is_dir() => inode,
        Ok(_) => return FS_NOT_FOUND,
        Err(error) => return ext4_error(error),
    };
    let (mut seen, from) = match shared.cursor {
        Some(cursor) if cursor.folder == folder.number && cursor.index <= index => (cursor.index, cursor.position),
        _ => (0, 0),
    };
    let mut hit: Option<(u32, usize, [u8; VFS_NAME_MAX], u64)> = None;
    let walked = shared.fs.list_from(&folder, from, &mut |entry, after| {
        if entry.name == b"." || entry.name == b".." {
            return false;
        }
        if seen == index {
            let mut name = [0u8; VFS_NAME_MAX];
            let length = entry.name.len().min(VFS_NAME_MAX);
            name[..length].copy_from_slice(&entry.name[..length]);
            hit = Some((entry.inode, length, name, after));
            return true;
        }
        seen += 1;
        false
    });
    if let Err(error) = walked {
        return ext4_error(error);
    }
    let Some((number, length, name, after)) = hit else { return 0 };
    let inode = match shared.fs.inode(number) {
        Ok(inode) => inode,
        Err(error) => return ext4_error(error),
    };
    shared.cursor = Some(Cursor { folder: folder.number, index: index + 1, position: after });
    *out = VfsEntry { stat: stat_of(&inode), name_length: length as u32, ..VfsEntry::default() };
    out.name[..length].copy_from_slice(&name[..length]);
    1
}

/// `vfs_read`: up to `buffer.len()` bytes from `offset`; the count (0 at or past the end) or an `FS_*` code.
pub fn read(reference: &[u8], offset: u64, buffer: &mut [u8]) -> isize {
    if let Some((shared, path)) = ext(reference) {
        return match shared.fs.resolve(path) {
            Ok(Some(inode)) if inode.is_file() => match shared.fs.read(&inode, offset, buffer) {
                Ok(count) => count as isize,
                Err(error) => ext4_error(error),
            },
            Ok(_) => FS_NOT_FOUND,
            Err(error) => ext4_error(error),
        };
    }
    tape_fs::read_at(reference, offset, buffer)
}

/// Tape ABI 32 `vfs_stat(path, path_len, out)`; shared by the x86 and aarch64 tape tables.
pub extern "C" fn api_vfs_stat(path: *const u8, path_length: usize, out: *mut VfsStat) -> isize {
    let (path, out) = unsafe { (core::slice::from_raw_parts(path, path_length), &mut *out) };
    stat(path, out)
}

/// Tape ABI 32 `vfs_read_dir(path, path_len, index, out)`.
pub extern "C" fn api_vfs_read_dir(path: *const u8, path_length: usize, index: usize, out: *mut VfsEntry) -> isize {
    let (path, out) = unsafe { (core::slice::from_raw_parts(path, path_length), &mut *out) };
    read_dir(path, index, out)
}

/// Tape ABI 32 `vfs_read(path, path_len, offset, buffer, capacity)`.
pub extern "C" fn api_vfs_read(path: *const u8, path_length: usize, offset: u64, buffer: *mut u8, capacity: usize) -> isize {
    let (path, buffer) = unsafe { (core::slice::from_raw_parts(path, path_length), core::slice::from_raw_parts_mut(buffer, capacity)) };
    read(path, offset, buffer)
}
