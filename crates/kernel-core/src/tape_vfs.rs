//! The tape `vfs_*` calls (ABI 32 reads, ABI 34 changes): what a file or folder is and what is in it, for every volume a tape
//! can name, ext2/3/4 included (`tape_fs` knows FOSFS and FAT only); ext volumes can be changed too. Shared by both kernels.
//!
//! A reference is the one `fs_read` takes. `diskN/path` on a disk with no FOSFS and no FAT volume is that disk's ext volume
//! (found the way the shell's `ext4` command does and kept mounted, `ext4vol::shared`); everything else is answered from the
//! `tape_fs` calls, so FOSFS and FAT volumes show up here too, without owners and times. The caller serialises calls (the
//! kernel's file lock), as for `tape_fs`.

use fosfor_abi::tape::*;

use crate::ext4vol::{self, Cursor, Partition, Shared};
use crate::remote;
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
    if remote::is_remote(reference) {
        return remote::stat(reference, out);
    }
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
    if remote::is_remote(reference) {
        return remote::read_dir(reference, index, out);
    }
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
    if remote::is_remote(reference) {
        return remote::read(reference, offset, buffer);
    }
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

// ---- changes (ABI 34): ext volumes only, each call journaled and committed before it returns ----

fn change_error(error: ext4::Error) -> isize {
    match error {
        ext4::Error::NotFound | ext4::Error::NotADirectory => FS_NOT_FOUND,
        ext4::Error::InvalidName => FS_BAD_NAME,
        ext4::Error::Exists => FS_EXISTS,
        ext4::Error::NoSpace => FS_NO_SPACE,
        ext4::Error::NotEmpty => FS_NOT_EMPTY,
        ext4::Error::IsADirectory => FS_IS_DIR,
        ext4::Error::ReadOnly | ext4::Error::Unsupported => FS_READ_ONLY,
        _ => FS_ERROR,
    }
}

type Writer = ext4::Writer<ext4::Journaled<Partition>>;

/// The ext volume a reference names, as (disk index, partition, path), or `None` for FOSFS, FAT and everything else.
fn ext_target(reference: &[u8]) -> Option<(usize, Partition, &[u8])> {
    let (shared, path) = ext(reference)?;
    Some((shared.disk, shared.partition, path))
}

/// "a/b/c" -> ("a/b", "c"); trailing slashes do not count.
fn split_leaf(path: &[u8]) -> (&[u8], &[u8]) {
    let mut end = path.len();
    while end > 0 && path[end - 1] == b'/' {
        end -= 1;
    }
    let trimmed = &path[..end];
    match trimmed.iter().rposition(|&byte| byte == b'/') {
        Some(at) => (&trimmed[..at], &trimmed[at + 1..]),
        None => (&[], trimmed),
    }
}

fn parent_number(writer: &mut Writer, folder: &[u8]) -> Result<u32, ext4::Error> {
    let inode = if folder.iter().all(|&byte| byte == b'/') { writer.root()? } else { writer.resolve(folder)?.ok_or(ext4::Error::NotFound)? };
    if !inode.is_dir() {
        return Err(ext4::Error::NotADirectory);
    }
    Ok(inode.number)
}

fn file_number(writer: &mut Writer, path: &[u8]) -> Result<ext4::Inode, ext4::Error> {
    writer.resolve(path)?.ok_or(ext4::Error::NotFound)
}

/// `vfs_create`: a file or folder `make` describes at `reference`; 0 or an `FS_*` code.
pub fn create(reference: &[u8], make: &VfsMake) -> isize {
    if remote::is_remote(reference) {
        return remote::create(reference, make);
    }
    let Some((disk, partition, path)) = ext_target(reference) else { return FS_READ_ONLY };
    let (folder, name) = split_leaf(path);
    let done = ext4vol::with_writer(disk, partition, |writer| {
        let parent = parent_number(writer, folder)?;
        let mode = (make.mode & 0o7777) as u16;
        let made = match make.kind {
            VFS_DIR => writer.mkdir(parent, name, mode)?,
            VFS_FILE => writer.create(parent, name, mode)?,
            _ => return Err(ext4::Error::Invalid),
        };
        writer.set_owner(made.number, make.uid, make.gid, None)
    });
    done.map_or_else(change_error, |()| 0)
}

/// `vfs_write`: the bytes written into the existing file at `reference`, or an `FS_*` code.
pub fn write(reference: &[u8], offset: u64, data: &[u8]) -> isize {
    if remote::is_remote(reference) {
        return remote::write(reference, offset, data);
    }
    let Some((disk, partition, path)) = ext_target(reference) else { return FS_READ_ONLY };
    let done = ext4vol::with_writer(disk, partition, |writer| {
        let inode = file_number(writer, path)?;
        writer.write(inode.number, offset, data)
    });
    done.map_or_else(change_error, |count| count as isize)
}

/// `vfs_truncate`.
pub fn truncate(reference: &[u8], size: u64) -> isize {
    if remote::is_remote(reference) {
        return remote::truncate(reference, size);
    }
    let Some((disk, partition, path)) = ext_target(reference) else { return FS_READ_ONLY };
    let done = ext4vol::with_writer(disk, partition, |writer| {
        let inode = file_number(writer, path)?;
        writer.truncate(inode.number, size)
    });
    done.map_or_else(change_error, |()| 0)
}

/// `vfs_remove`.
pub fn remove(reference: &[u8]) -> isize {
    if remote::is_remote(reference) {
        return remote::remove(reference);
    }
    let Some((disk, partition, path)) = ext_target(reference) else { return FS_READ_ONLY };
    let (folder, name) = split_leaf(path);
    let done = ext4vol::with_writer(disk, partition, |writer| {
        let parent = parent_number(writer, folder)?;
        writer.remove(parent, name)
    });
    done.map_or_else(change_error, |()| 0)
}

/// `vfs_rename`: both references must name the same ext volume.
pub fn rename(from: &[u8], to: &[u8]) -> isize {
    if remote::is_remote(from) {
        return remote::rename(from, to);
    }
    let (Some((disk, partition, from_path)), Some((to_disk, _, to_path))) = (ext_target(from), ext_target(to)) else { return FS_READ_ONLY };
    if disk != to_disk {
        return FS_ERROR;
    }
    let ((from_folder, from_name), (to_folder, to_name)) = (split_leaf(from_path), split_leaf(to_path));
    let done = ext4vol::with_writer(disk, partition, |writer| {
        let from_parent = parent_number(writer, from_folder)?;
        let to_parent = parent_number(writer, to_folder)?;
        writer.rename(from_parent, from_name, to_parent, to_name)
    });
    done.map_or_else(change_error, |()| 0)
}

/// `vfs_set_owner`.
pub fn set_owner(reference: &[u8], uid: u32, gid: u32, mode: u32) -> isize {
    if remote::is_remote(reference) {
        return remote::set_owner(reference, uid, gid, mode);
    }
    let Some((disk, partition, path)) = ext_target(reference) else { return FS_READ_ONLY };
    let done = ext4vol::with_writer(disk, partition, |writer| {
        let inode = file_number(writer, path)?;
        writer.set_owner(inode.number, uid, gid, (mode != VFS_KEEP_MODE).then_some((mode & 0o7777) as u16))
    });
    done.map_or_else(change_error, |()| 0)
}

fn bytes<'a>(at: *const u8, length: usize) -> &'a [u8] {
    unsafe { core::slice::from_raw_parts(at, length) }
}

/// Tape ABI 34 `vfs_create(path, path_len, make)`; shared by the x86 and aarch64 tape tables.
pub extern "C" fn api_vfs_create(path: *const u8, path_length: usize, make: *const VfsMake) -> isize {
    create(bytes(path, path_length), unsafe { &*make })
}

/// Tape ABI 34 `vfs_write(path, path_len, offset, data, len)`.
pub extern "C" fn api_vfs_write(path: *const u8, path_length: usize, offset: u64, data: *const u8, length: usize) -> isize {
    write(bytes(path, path_length), offset, bytes(data, length))
}

/// Tape ABI 34 `vfs_truncate(path, path_len, size)`.
pub extern "C" fn api_vfs_truncate(path: *const u8, path_length: usize, size: u64) -> isize {
    truncate(bytes(path, path_length), size)
}

/// Tape ABI 34 `vfs_remove(path, path_len)`.
pub extern "C" fn api_vfs_remove(path: *const u8, path_length: usize) -> isize {
    remove(bytes(path, path_length))
}

/// Tape ABI 34 `vfs_rename(from, from_len, to, to_len)`.
pub extern "C" fn api_vfs_rename(from: *const u8, from_length: usize, to: *const u8, to_length: usize) -> isize {
    rename(bytes(from, from_length), bytes(to, to_length))
}

/// Tape ABI 34 `vfs_set_owner(path, path_len, uid, gid, mode)`.
pub extern "C" fn api_vfs_set_owner(path: *const u8, path_length: usize, uid: u32, gid: u32, mode: u32) -> isize {
    set_owner(bytes(path, path_length), uid, gid, mode)
}
