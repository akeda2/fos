//! The tape file calls (`fs_read`, `fs_write`, `fs_remove`, `fs_list`, `fs_stream_*`, `disk_info`), shared by both ports so a
//! tape sees the same names, volumes and error codes on x86-64 and aarch64.
//!
//! A reference is `NAME` (the boot volume), `diskN/NAME`, `@LABEL/NAME` or `esp/NAME` (the boot disk's FAT32 EFI System Partition). A disk that holds a FOSFS volume always means
//! FOSFS; a disk with only a FAT32/exFAT volume (a stick or card a PC formatted) is reachable by an explicit `diskN/`.

use fosfor_abi::tape::*;

use crate::fosfs::{Entry, Error, Volume};
use crate::block;
use crate::volumes as storage;

/// A file a tape is streaming into: on FOSFS its volume and reserved entry, on a FAT32/exFAT volume the mounted volume,
/// the stream and the file's name (for `finish`). Every open stream has its own mount of its volume: the only state
/// a mount caches is one FAT sector, which `fat` drops at `create` and `finish` and before it changes any FAT entries,
/// and the free-space search reads the bitmap or FAT afresh, so two streams on one FAT volume cannot hand out the same clusters.
enum Open {
    Fos(Volume, Entry),
    Fat(crate::fatvol::Fs, fat::Stream, [u8; 64], usize),
}

/// The streams of `fs_stream_open2` (handles 0 to `FS_STREAMS` - 1) and, last, the one of the older `fs_stream_open`.
static mut STREAMS: [Option<Open>; FS_STREAMS + 1] = [const { None }; FS_STREAMS + 1];
const LEGACY: usize = FS_STREAMS;

static mut BIN_PATH: [u8; 4 + crate::fosfs::MAX_NAME] = [0; 4 + crate::fosfs::MAX_NAME];

/// Resolves a tape's file reference to its FOSFS volume and the name on it.
///
/// A bare name is the boot volume's; for reading (`reading`) it falls back to the first other disk that has the file, so
/// a configuration file moved to another disk is still found.
pub fn resolve_file(reference: &[u8], reading: bool) -> Result<(Volume, &[u8]), isize> {
    let (selector, name) = split_selector(reference);
    if selector.is_empty() {
        // A path under a mount point goes to the mounted volume (reads: only if it has the file, else the boot
        // volume's own copy shows through); with the volume absent it is the boot volume's folder.
        if let Some((label, rest)) = crate::mounts::lookup(name) {
            if let Some((_, volume)) = storage::volume_labelled(label.as_bytes()) {
                if !reading || matches!(volume.file_length(rest), Ok(Some(_))) {
                    return Ok((volume, rest));
                }
            }
        }
        let found = if reading { storage::volume_with_file(name) } else { None };
        if reading && found.is_none() {
            if let Some(path) = bin_path(name) {
                if let Some(volume) = storage::volume_with_file(path) {
                    return Ok((volume, path));
                }
            }
        }
        return found.or_else(storage::boot_volume).map(|volume| (volume, name)).ok_or(FS_NO_VOLUME);
    }
    let volume = if let Some(label) = selector.strip_prefix(b"@") {
        storage::volume_labelled(label).map(|(_, volume)| volume)
    } else {
        disk_number(selector).and_then(|number| storage::volume_on(number).ok())
    };
    let volume = volume.ok_or(FS_NO_VOLUME)?;
    if reading && !matches!(volume.file_length(name), Ok(Some(_))) {
        if let Some(path) = bin_path(name).filter(|path| matches!(volume.file_length(path), Ok(Some(_)))) {
            return Ok((volume, path));
        }
    }
    Ok((volume, name))
}

/// `bin/NAME` for a bare NAME, in a buffer that outlives the call (file calls are synchronous and the caller uses the
/// name at once). A tape is found in `bin/` when its bare name is not there: `run NAME`, and a tape that loads another
/// one, need not know whether the volume keeps its tapes in a folder.
pub fn bin_path(name: &[u8]) -> Option<&'static [u8]> {
    if name.contains(&b'/') || name.len() > crate::fosfs::MAX_NAME {
        return None;
    }
    let path: &'static mut [u8; 4 + crate::fosfs::MAX_NAME] = unsafe { &mut *core::ptr::addr_of_mut!(BIN_PATH) };
    path[..4].copy_from_slice(b"bin/");
    path[4..4 + name.len()].copy_from_slice(name);
    Some(&path[..4 + name.len()])
}

/// Splits a reference into its volume selector (`diskN`, `@LABEL`, `esp`, or empty for the boot volume) and the path
/// on that volume. A first component that is none of those is a folder of the boot volume: `bin/phono`.
pub fn split_selector(reference: &[u8]) -> (&[u8], &[u8]) {
    let (first, rest) = match reference.iter().position(|&byte| byte == b'/') {
        Some(slash) => (&reference[..slash], Some(&reference[slash + 1..])),
        None => (reference, None),
    };
    let is_selector = first == b"esp" || first.strip_prefix(b"@").is_some_and(|label| !label.is_empty()) || disk_number(first).is_some() || ext_number(first).is_some();
    match (is_selector, rest) {
        (true, Some(rest)) => (first, rest),
        (true, None) => (first, &[]),
        (false, _) => (&[], reference),
    }
}

/// `extN`: the ext volume on disk N even when the disk also holds FOSFS or FAT (`diskN` finds ext only on a disk with neither).
pub(crate) fn ext_number(selector: &[u8]) -> Option<usize> {
    selector.strip_prefix(b"ext").and_then(|digits| core::str::from_utf8(digits).ok()).and_then(|digits| digits.parse::<usize>().ok())
}

pub(crate) fn disk_number(selector: &[u8]) -> Option<usize> {
    selector.strip_prefix(b"disk").and_then(|digits| core::str::from_utf8(digits).ok()).and_then(|digits| digits.parse::<usize>().ok())
}

/// The FAT volume of disk `number` (one-based) when it has no FOSFS volume of ours.
pub(crate) fn fat_disk(number: usize) -> Option<crate::fatvol::Fs> {
    if storage::volume_on(number).is_ok() {
        return None;
    }
    crate::fatvol::mount(number.checked_sub(1)?)
}

/// Enters the folders of `path` on an exFAT volume (FAT32 has none here: it stays root-only) and returns the last
/// component. A folder that is missing leaves the volume at its root and the whole path as the name, which the file
/// calls then report as not found or a bad name.
fn enter_path<'a>(fs: &mut crate::fatvol::Fs, path: &'a [u8]) -> &'a [u8] {
    if fs.kind() != fat::Kind::ExFat {
        return path;
    }
    let mut rest = path;
    while let Some(slash) = rest.iter().position(|&byte| byte == b'/') {
        if fs.enter(&rest[..slash]).is_err() {
            fs.enter_root();
            return path;
        }
        rest = &rest[slash + 1..];
    }
    rest
}

/// The label of the exFAT volume on disk `number` (`Some(None)`: an exFAT volume with no label; `None`: no such volume).
pub fn fat_label_of(number: usize) -> Option<Option<([u8; 11], usize)>> {
    let mut fs = fat_disk(number)?;
    (fs.kind() == fat::Kind::ExFat).then(|| fs.label().ok().flatten())
}

/// Sets (empty clears) the label of the exFAT volume on disk `number`.
pub fn set_fat_label(number: usize, label: &[u8]) -> Result<(), fat::Error> {
    let mut fs = fat_disk(number).ok_or(fat::Error::NotFound)?;
    fs.set_label(label)
}

/// The exFAT volume labelled `label` (any case), on a disk that has no FOSFS volume.
pub fn fat_labelled(label: &[u8]) -> Option<(usize, crate::fatvol::Fs)> {
    (1..=block::count()).find_map(|number| {
        let mut fs = fat_disk(number)?;
        let (bytes, length) = fs.label().ok()??;
        bytes[..length].eq_ignore_ascii_case(label).then_some((number, fs))
    })
}

/// A FAT volume reached by label: `@LABEL/path`, or a path under a mount point (`directory`: the path may be the mount
/// point itself) whose label no FOSFS volume has. Returns the volume at its root and what is left of the path. None when
/// the reference is neither, the label is a FOSFS volume's, or no such volume is present.
fn fat_by_label<'a>(selector: &'a [u8], path: &'a [u8], directory: bool) -> Option<(crate::fatvol::Fs, &'a [u8])> {
    let mounted;
    let (label, rest) = match selector.strip_prefix(b"@") {
        Some(label) => (label, path),
        None if selector.is_empty() => {
            mounted = if directory { crate::mounts::lookup_dir(path)? } else { crate::mounts::lookup(path)? };
            (mounted.0.as_bytes(), mounted.1)
        }
        None => return None,
    };
    storage::volume_labelled(label).is_none().then(|| fat_labelled(label)).flatten().map(|(_, fs)| (fs, rest))
}

/// A FAT32/exFAT volume named by a reference `diskN/NAME`; on exFAT, `diskN/rec/take1` is the file `take1` of the
/// folder `rec`, which is entered, and the name returned is the last component.
fn resolve_fat(reference: &[u8]) -> Option<(crate::fatvol::Fs, &[u8])> {
    let (selector, path) = split_selector(reference);
    if selector.is_empty() || selector.starts_with(b"@") {
        let (mut fs, rest) = fat_by_label(selector, path, false)?;
        let name = enter_path(&mut fs, rest);
        return Some((fs, name));
    }
    let slash = reference.iter().position(|&byte| byte == b'/')?;
    if &reference[..slash] == b"esp" {
        return crate::fatvol::mount_esp().map(|fs| (fs, &reference[slash + 1..]));
    }
    let number = disk_number(&reference[..slash])?;
    let mut fs = fat_disk(number)?;
    let name = enter_path(&mut fs, &reference[slash + 1..]);
    Some((fs, name))
}

/// Whether a reference names a FAT32/exFAT volume (`esp/`, or `diskN/` of a disk with no FOSFS volume).
pub fn is_fat(reference: &[u8]) -> bool {
    resolve_fat(reference).is_some()
}

fn fat_error(error: fat::Error) -> isize {
    match error {
        fat::Error::NotFound => FS_NOT_FOUND,
        fat::Error::BadName => FS_BAD_NAME,
        fat::Error::Exists => FS_BAD_NAME,
        _ => FS_ERROR,
    }
}

pub fn read(reference: &[u8], buffer: &mut [u8]) -> isize {
    if crate::remote::is_remote(reference) {
        return crate::remote::read_whole(reference, buffer);
    }
    if let Some((mut fs, name)) = resolve_fat(reference) {
        return match fs.find(name) {
            Ok(Some(file)) if file.size as usize > buffer.len() => FS_TOO_BIG,
            Ok(Some(file)) => match fs.read(&file, 0, &mut buffer[..file.size as usize]) {
                Ok(length) => length as isize,
                Err(error) => fat_error(error),
            },
            Ok(None) => FS_NOT_FOUND,
            Err(error) => fat_error(error),
        };
    }
    let (volume, name) = match resolve_file(reference, true) {
        Ok(found) => found,
        Err(code) => return code,
    };
    match volume.read(name, buffer) {
        Ok(Some(length)) => length as isize,
        Ok(None) => FS_NOT_FOUND,
        Err(Error::TooBig) => FS_TOO_BIG,
        Err(_) => FS_ERROR,
    }
}

/// `fs_read_at`: up to `buffer.len()` bytes of a file from byte `offset`; the count (0 at or past the end) or an `FS_*` code.
pub fn read_at(reference: &[u8], offset: u64, buffer: &mut [u8]) -> isize {
    if crate::remote::is_remote(reference) {
        return crate::remote::read(reference, offset, buffer);
    }
    if let Some((mut fs, name)) = resolve_fat(reference) {
        return match fs.find(name) {
            Ok(Some(file)) => match fs.read(&file, offset, buffer) {
                Ok(length) => length as isize,
                Err(error) => fat_error(error),
            },
            Ok(None) => FS_NOT_FOUND,
            Err(error) => fat_error(error),
        };
    }
    let (volume, name) = match resolve_file(reference, true) {
        Ok(found) => found,
        Err(code) => return code,
    };
    match volume.read_at(name, offset, buffer) {
        Ok(Some(length)) => length as isize,
        Ok(None) => FS_NOT_FOUND,
        Err(_) => FS_ERROR,
    }
}

/// Tape ABI 29 `fs_read_at(name, name_len, offset, buffer, capacity)`; shared by the x86 and aarch64 tape tables.
pub extern "C" fn api_fs_read_at(name: *const u8, name_length: usize, offset: u64, buffer: *mut u8, capacity: usize) -> isize {
    let (name, buffer) = unsafe { (core::slice::from_raw_parts(name, name_length), core::slice::from_raw_parts_mut(buffer, capacity)) };
    read_at(name, offset, buffer)
}

/// A file written under a mount point makes the folders on its way, on the mounted volume or (volume absent) the boot
/// volume, so a mount point needs no `mkdir` first. Elsewhere a missing folder stays an error.
fn make_mount_folders(reference: &[u8], volume: &Volume, name: &[u8]) -> Result<(), Error> {
    let (selector, path) = split_selector(reference);
    if selector.is_empty() && crate::mounts::lookup(path).is_some() && volume.version() == 4 {
        return volume.make_parents(name);
    }
    Ok(())
}

pub fn write(reference: &[u8], data: &[u8]) -> isize {
    if crate::remote::is_remote(reference) {
        return crate::remote::write_whole(reference, data);
    }
    // Replace the file the way the shell's `write` does on FAT: reserve its whole length, write it, then finish (the
    // directory entry appears with the final size).
    if let Some((mut fs, file)) = resolve_fat(reference) {
        if data.is_empty() {
            return FS_ERROR; // an empty file cannot be created on a FAT volume here (the shell's `write` says the same)
        }
        crate::fatvol::stamp(&mut fs);
        let outcome = fs.create(file, data.len() as u64).and_then(|stream| {
            fs.write(&stream, 0, data, data.len() as u64)?;
            fs.finish(&stream, file, data.len() as u64)
        });
        return match outcome {
            Ok(()) => 0,
            Err(error) => fat_error(error),
        };
    }
    let (volume, name) = match resolve_file(reference, false) {
        Ok(found) => found,
        Err(code) => return code,
    };
    if make_mount_folders(reference, &volume, name).is_err() {
        return FS_ERROR;
    }
    match volume.write(name, data) {
        Ok(()) => 0,
        Err(Error::InvalidName) => FS_BAD_NAME,
        Err(_) => FS_ERROR,
    }
}

/// Where a pushed file (NETFS put: `foscopy`, `tapesync`) of bare `name` goes: a tape that lives only in `bin/` is
/// replaced there, so an update is not shadowed by a new copy in the root. Not used by the shell's `write` or a tape's
/// own writes, where a bare name means the root even when `bin/` has the same name.
pub fn replace_target<'a>(volume: &Volume, name: &'a [u8]) -> &'a [u8] {
    if matches!(volume.file_length(name), Ok(Some(_))) {
        return name;
    }
    match bin_path(name) {
        Some(path) if matches!(volume.file_length(path), Ok(Some(_))) => path,
        _ => name,
    }
}

pub fn remove(reference: &[u8]) -> isize {
    if crate::remote::is_remote(reference) {
        return crate::remote::remove(reference);
    }
    if let Some((mut fs, file)) = resolve_fat(reference) {
        let outcome = match fs.remove(file) {
            Ok(false) if fs.kind() == fat::Kind::ExFat => fs.remove_dir(file),
            other => other,
        };
        return match outcome {
            Ok(true) => 0,
            Ok(false) => FS_NOT_FOUND,
            Err(error) => fat_error(error),
        };
    }
    let (volume, name) = match resolve_file(reference, false) {
        Ok(found) => found,
        Err(code) => return code,
    };
    let mut outcome = volume.remove(name);
    // A bare name that is not in the root removes the tape in `bin/`, the same file `run NAME` would start.
    if matches!(outcome, Ok(false)) {
        if let Some(path) = bin_path(name) {
            outcome = volume.remove(path);
        }
    }
    match outcome {
        Ok(true) => 0,
        Ok(false) => FS_NOT_FOUND,
        Err(_) => FS_ERROR,
    }
}

/// Renames or moves `from` to `to` in place when both are on the same FOSFS v4 volume or the same exFAT volume: `Ok(true)`
/// done, `Ok(false)` not possible here (another volume, FAT32, FOSFS v3, a name that exists: the caller copies instead),
/// `Err` a code for a source that is missing.
pub fn rename(from: &[u8], to: &[u8]) -> Result<bool, isize> {
    if crate::remote::is_remote(from) {
        if !crate::remote::is_remote(to) {
            return Ok(false);
        }
        let code = crate::remote::rename(from, to);
        return if code < 0 { Err(code) } else { Ok(true) };
    }
    if crate::remote::is_remote(to) {
        return Ok(false);
    }
    match (resolve_fat(from), resolve_fat(to)) {
        (None, None) => {}
        (Some((mut source, old)), Some((mut target, new))) => return rename_exfat(&mut source, old, &mut target, new),
        _ => return Ok(false),
    }
    let ((source, old), (target, new)) = (resolve_file(from, false)?, resolve_file(to, false)?);
    if (source.disk, source.base) != (target.disk, target.base) || source.version() != 4 {
        return Ok(false);
    }
    // A bare name that is only in `bin/` moves that tape, as `rm` removes it.
    let old = if matches!(source.find(old), Ok(Some(_))) { old } else { bin_path(old).unwrap_or(old) };
    if make_mount_folders(to, &target, new).is_err() {
        return Ok(false);
    }
    match source.rename(old, new) {
        Ok(true) => Ok(true),
        Ok(false) => Err(FS_NOT_FOUND),
        Err(_) => Ok(false),
    }
}

/// `rename` inside one exFAT volume: a new name in the same folder, or a move to another folder of it.
fn rename_exfat(source: &mut crate::fatvol::Fs, old: &[u8], target: &mut crate::fatvol::Fs, new: &[u8]) -> Result<bool, isize> {
    let (a, b) = (source.device(), target.device());
    if source.kind() != fat::Kind::ExFat || target.kind() != fat::Kind::ExFat || (a.disk, a.base) != (b.disk, b.base) {
        return Ok(false);
    }
    crate::fatvol::stamp(target);
    let outcome = if source.same_directory(target) { source.rename(old, new) } else { source.move_to(old, target, new) };
    match outcome {
        Ok(done) => Ok(done),
        Err(fat::Error::NotFound) => Err(FS_NOT_FOUND),
        Err(_) => Ok(false),
    }
}

/// Makes a folder (FOSFS v4 and exFAT volumes only).
pub fn mkdir(reference: &[u8]) -> isize {
    if crate::remote::is_remote(reference) {
        return crate::remote::make_folder(reference);
    }
    if let Some((mut fs, name)) = resolve_fat(reference) {
        crate::fatvol::stamp(&mut fs);
        return match fs.mkdir(name) {
            Ok(()) => 0,
            Err(error) => fat_error(error),
        };
    }
    let (volume, name) = match resolve_file(reference, false) {
        Ok(found) => found,
        Err(code) => return code,
    };
    match volume.mkdir(name) {
        Ok(()) => 0,
        Err(Error::InvalidName) => FS_BAD_NAME,
        Err(_) => FS_ERROR,
    }
}

/// The volume a `fs_list` reference names: empty (boot volume), `diskN` or `@LABEL`.
pub fn resolve_volume(reference: &[u8]) -> Option<Volume> {
    if reference.is_empty() {
        return storage::boot_volume();
    }
    if let Some(label) = reference.strip_prefix(b"@") {
        return storage::volume_labelled(label).map(|(_, volume)| volume);
    }
    storage::volume_on(disk_number(reference)?).ok()
}

/// Every entry of a volume (`""`, `diskN`, `@LABEL`, `esp`): its name, length, whether it is a directory and (FAT) when it
/// was written; the result is the kind of volume (`FOSFS`, `FAT32`, `exFAT`) or an `FS_*` code. A disk with only a
/// FAT32/exFAT volume lists its root directory.
pub fn each_entry(reference: &[u8], each: &mut dyn FnMut(&[u8], u64, bool, Option<fat::Time>)) -> Result<&'static str, isize> {
    if crate::remote::is_remote(reference) {
        return crate::remote::each_entry(reference, each);
    }
    let (selector, path) = split_selector(reference);
    let mut folder = path;
    let mut fat = if reference == b"esp" { crate::fatvol::mount_esp() } else { disk_number(selector).and_then(fat_disk) };
    if fat.is_none() {
        if let Some((fs, rest)) = fat_by_label(selector, path, true) {
            (fat, folder) = (Some(fs), rest);
        }
    }
    if let Some(mut fs) = fat {
        let kind = if fs.kind() == fat::Kind::ExFat { "exFAT" } else { "FAT32" };
        // exFAT: `diskN/rec/session1` lists that folder; FAT32 and `esp` list the root as before.
        if fs.kind() == fat::Kind::ExFat {
            for part in folder.split(|&byte| byte == b'/').filter(|part| !part.is_empty()) {
                if fs.enter(part).is_err() {
                    return Err(FS_NOT_FOUND);
                }
            }
        }
        return fs.list(|entry| each(entry.name(), u64::from(entry.size), entry.is_directory, entry.modified)).map(|()| kind).map_err(fat_error);
    }
    let mut volume = resolve_volume(selector).ok_or(FS_NO_VOLUME)?;
    if selector.is_empty() {
        if let Some((label, rest)) = crate::mounts::lookup_dir(folder) {
            if let Some((_, mounted)) = storage::volume_labelled(label.as_bytes()) {
                (volume, folder) = (mounted, rest);
            }
        }
    }
    volume
        .for_each_in(folder, |entry| each(entry.name(), entry.length, entry.is_dir, (entry.modified != 0).then(|| crate::fatvol::time_at_unix(entry.modified))))
        .map(|()| "FOSFS")
        .map_err(|error| if error == Error::NoDirectory { FS_NOT_FOUND } else { FS_ERROR })
}

/// `fs_list`: the `index`th file of a volume (`""`, `diskN`, `@LABEL`); a FAT disk lists its root directory.
pub fn list(reference: &[u8], index: usize, out: &mut FileInfo) -> isize {
    let (mut position, mut found) = (0, false);
    let outcome = each_entry(reference, &mut |name, length, directory, _| {
        if position == index && !found {
            found = true;
            let shown = name.len().min(64);
            *out = FileInfo { length, flags: if directory { FILE_DIRECTORY } else { 0 }, name_length: shown as u32, name: [0; 64] };
            out.name[..shown].copy_from_slice(&name[..shown]);
        }
        position += 1;
    });
    match outcome {
        Ok(_) => isize::from(found),
        Err(code) => code,
    }
}

/// The length of a file, or an `FS_*` code.
pub fn length(reference: &[u8]) -> isize {
    if crate::remote::is_remote(reference) {
        return crate::remote::length(reference);
    }
    if let Some((mut fs, name)) = resolve_fat(reference) {
        return match fs.entry(name) {
            Ok(Some(entry)) if !entry.is_directory => entry.size as isize,
            Ok(_) => FS_NOT_FOUND,
            Err(error) => fat_error(error),
        };
    }
    let (volume, name) = match resolve_file(reference, true) {
        Ok(found) => found,
        Err(code) => return code,
    };
    match volume.file_length(name) {
        Ok(Some(length)) => length as isize,
        Ok(None) => FS_NOT_FOUND,
        Err(Error::InvalidName) => FS_BAD_NAME,
        Err(_) => FS_ERROR,
    }
}

/// `disk_info`: what disk `number` (one-based) holds, for a tape that lets the user pick where to save.
pub fn disk_info(number: usize, out: &mut DiskInfo) -> u32 {
    let Some(disk) = number.checked_sub(1).and_then(block::info) else { return 0 };
    let mut info = DiskInfo { sectors: disk.sectors, boot: u32::from(block::boot_disk() == Some(number - 1)), ..Default::default() };
    info.model.copy_from_slice(&disk.model);
    info.kind = match storage::volume_on(number) {
        Ok(volume) => {
            if let Ok(label) = volume.label() {
                let bytes = label.as_bytes();
                info.label[..bytes.len().min(16)].copy_from_slice(&bytes[..bytes.len().min(16)]);
            }
            if let Ok((free, largest)) = volume.free_space() {
                (info.free_bytes, info.largest_bytes) = (free, largest);
            }
            DISK_FOSFS
        }
        Err(_) => match crate::fatvol::mount(number - 1) {
            Some(mut fs) => {
                if let Ok((free, largest)) = fs.free_space() {
                    (info.free_bytes, info.largest_bytes) = (free, largest);
                }
                if fs.kind() == fat::Kind::ExFat { DISK_EXFAT } else { DISK_FAT32 }
            }
            None => DISK_NO_VOLUME,
        },
    };
    *out = info;
    1
}

/// Forgets every stream (done when a tape starts; a file left open keeps what was written and its reservation).
pub fn stream_reset() {
    unsafe { *core::ptr::addr_of_mut!(STREAMS) = [const { None }; FS_STREAMS + 1] };
}

fn streams() -> &'static mut [Option<Open>; FS_STREAMS + 1] {
    unsafe { &mut *core::ptr::addr_of_mut!(STREAMS) }
}

/// Opens `reference` into `slot`: the reserved capacity in bytes, or a negative code.
fn open_into(slot: usize, reference: &[u8], max_bytes: u64) -> isize {
    streams()[slot] = None;
    if let Some((mut fs, file)) = resolve_fat(reference) {
        if file.len() > 64 {
            return FS_BAD_NAME;
        }
        return match fs.create(file, max_bytes) {
            Ok(stream) => {
                let mut stored = [0u8; 64];
                stored[..file.len()].copy_from_slice(file);
                let capacity = stream.capacity();
                streams()[slot] = Some(Open::Fat(fs, stream, stored, file.len()));
                capacity as isize
            }
            Err(error) => fat_error(error),
        };
    }
    let (volume, name) = match resolve_file(reference, false) {
        Ok(found) => found,
        Err(code) => return code,
    };
    if make_mount_folders(reference, &volume, name).is_err() {
        return FS_ERROR;
    }
    match volume.reserve(name, max_bytes) {
        Ok((entry, capacity)) => {
            streams()[slot] = Some(Open::Fos(volume, entry));
            capacity as isize
        }
        Err(Error::InvalidName) => FS_BAD_NAME,
        Err(_) => FS_ERROR,
    }
}

fn write_to(slot: usize, offset: u64, data: &[u8], file_length: u64) -> isize {
    match streams().get_mut(slot).and_then(Option::as_mut) {
        Some(Open::Fat(fs, stream, _, _)) => {
            crate::fatvol::stamp(fs);
            match fs.write(stream, offset, data, file_length) {
                Ok(()) => 0,
                Err(fat::Error::OutOfRange) => FS_TOO_BIG,
                Err(error) => fat_error(error),
            }
        }
        Some(Open::Fos(volume, entry)) => match volume.write_extent(entry, offset, data, file_length) {
            Ok(()) => 0,
            Err(Error::TooBig) => FS_TOO_BIG,
            Err(_) => FS_ERROR,
        },
        None => FS_ERROR,
    }
}

fn close_slot(slot: usize, file_length: u64) -> isize {
    match streams().get_mut(slot).and_then(Option::take) {
        Some(Open::Fat(mut fs, stream, name, length)) => {
            crate::fatvol::stamp(&mut fs);
            match fs.finish(&stream, &name[..length], file_length) {
                Ok(()) => 0,
                Err(error) => fat_error(error),
            }
        }
        Some(Open::Fos(volume, entry)) => match volume.trim_extent(&entry, file_length) {
            Ok(()) => 0,
            Err(_) => FS_ERROR,
        },
        None => FS_ERROR,
    }
}

/// `fs_stream_open`: the one stream of the older calls; opening replaces the last.
pub fn stream_open(reference: &[u8], max_bytes: u64) -> isize {
    open_into(LEGACY, reference, max_bytes)
}

pub fn stream_write(offset: u64, data: &[u8], file_length: u64) -> isize {
    write_to(LEGACY, offset, data, file_length)
}

pub fn stream_close(file_length: u64) -> isize {
    close_slot(LEGACY, file_length)
}

/// `fs_stream_open2`: the first free handle, with the capacity stored in `*capacity_out`.
pub extern "C" fn api_fs_stream_open2(name: *const u8, name_length: usize, max_bytes: u64, capacity_out: *mut u64) -> isize {
    let reference = unsafe { core::slice::from_raw_parts(name, name_length) };
    let Some(handle) = streams()[..FS_STREAMS].iter().position(Option::is_none) else { return FS_ERROR };
    let capacity = open_into(handle, reference, max_bytes);
    if capacity < 0 {
        return capacity;
    }
    unsafe { *capacity_out = capacity as u64 };
    handle as isize
}

pub extern "C" fn api_fs_stream_write2(handle: isize, offset: u64, data: *const u8, length: usize, file_length: u64) -> isize {
    if !(0..FS_STREAMS as isize).contains(&handle) {
        return FS_ERROR;
    }
    write_to(handle as usize, offset, unsafe { core::slice::from_raw_parts(data, length) }, file_length)
}

pub extern "C" fn api_fs_stream_close2(handle: isize, file_length: u64) -> isize {
    if !(0..FS_STREAMS as isize).contains(&handle) {
        return FS_ERROR;
    }
    close_slot(handle as usize, file_length)
}
