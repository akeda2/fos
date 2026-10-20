//! The tape file calls (`fs_read`, `fs_write`, `fs_remove`, `fs_list`, `fs_stream_*`, `disk_info`), shared by both ports so a
//! tape sees the same names, volumes and error codes on x86-64 and aarch64.
//!
//! A reference is `NAME` (the boot volume), `diskN/NAME`, `@LABEL/NAME` or `esp/NAME` (the boot disk's FAT32 EFI System Partition). A disk that holds a FOSFS volume always means
//! FOSFS; a disk with only a FAT32/exFAT volume (a stick or card a PC formatted) is reachable by an explicit `diskN/`.

use fosfor_abi::tape::*;

use crate::fosfs::{Entry, Error, Volume};
use crate::storage;

/// The file a tape is streaming into (`fs_stream_open`): its volume and reserved entry.
static mut STREAM: Option<(Volume, Entry)> = None;
/// Or, on a FAT32/exFAT volume: the mounted volume, the stream and the file's name (for `finish`).
static mut FAT_STREAM: Option<(crate::fatvol::Fs, fat::Stream, [u8; 64], usize)> = None;

/// Resolves a tape's file reference to its FOSFS volume and the name on it.
///
/// A bare name is the boot volume's; for reading (`reading`) it falls back to the first other disk that has the file, so
/// a configuration file moved to another disk is still found.
pub fn resolve_file(reference: &[u8], reading: bool) -> Result<(Volume, &[u8]), isize> {
    let Some(slash) = reference.iter().position(|&byte| byte == b'/') else {
        let found = if reading { storage::volume_with_file(reference) } else { None };
        return found.or_else(storage::boot_volume).map(|volume| (volume, reference)).ok_or(FS_NO_VOLUME);
    };
    let (selector, name) = (&reference[..slash], &reference[slash + 1..]);
    let volume = if let Some(label) = selector.strip_prefix(b"@") {
        storage::volume_labelled(label).map(|(_, volume)| volume)
    } else {
        disk_number(selector).and_then(|number| storage::volume_on(number).ok())
    };
    volume.map(|volume| (volume, name)).ok_or(FS_NO_VOLUME)
}

fn disk_number(selector: &[u8]) -> Option<usize> {
    selector.strip_prefix(b"disk").and_then(|digits| core::str::from_utf8(digits).ok()).and_then(|digits| digits.parse::<usize>().ok())
}

/// The FAT volume of disk `number` (one-based) when it has no FOSFS volume of ours.
fn fat_disk(number: usize) -> Option<crate::fatvol::Fs> {
    if storage::volume_on(number).is_ok() {
        return None;
    }
    crate::fatvol::mount(number.checked_sub(1)?)
}

/// A FAT32/exFAT volume named by a reference `diskN/NAME`.
fn resolve_fat(reference: &[u8]) -> Option<(crate::fatvol::Fs, &[u8])> {
    let slash = reference.iter().position(|&byte| byte == b'/')?;
    if &reference[..slash] == b"esp" {
        return crate::fatvol::mount_esp().map(|fs| (fs, &reference[slash + 1..]));
    }
    let number = disk_number(&reference[..slash])?;
    fat_disk(number).map(|fs| (fs, &reference[slash + 1..]))
}

/// Whether a reference names a FAT32/exFAT volume (`esp/`, or `diskN/` of a disk with no FOSFS volume).
pub fn is_fat(reference: &[u8]) -> bool {
    resolve_fat(reference).is_some()
}

fn fat_error(error: fat::Error) -> isize {
    match error {
        fat::Error::NotFound => FS_NOT_FOUND,
        fat::Error::BadName => FS_BAD_NAME,
        _ => FS_ERROR,
    }
}

pub fn read(reference: &[u8], buffer: &mut [u8]) -> isize {
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

pub fn write(reference: &[u8], data: &[u8]) -> isize {
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
    match volume.write(name, data) {
        Ok(()) => 0,
        Err(Error::InvalidName) => FS_BAD_NAME,
        Err(_) => FS_ERROR,
    }
}

pub fn remove(reference: &[u8]) -> isize {
    if let Some((mut fs, file)) = resolve_fat(reference) {
        return match fs.remove(file) {
            Ok(true) => 0,
            Ok(false) => FS_NOT_FOUND,
            Err(error) => fat_error(error),
        };
    }
    let (volume, name) = match resolve_file(reference, false) {
        Ok(found) => found,
        Err(code) => return code,
    };
    match volume.remove(name) {
        Ok(true) => 0,
        Ok(false) => FS_NOT_FOUND,
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
    let fat = if reference == b"esp" { crate::fatvol::mount_esp() } else { disk_number(reference).and_then(fat_disk) };
    if let Some(mut fs) = fat {
        let kind = if fs.kind() == fat::Kind::ExFat { "exFAT" } else { "FAT32" };
        return fs.list(|entry| each(entry.name(), u64::from(entry.size), entry.is_directory, entry.modified)).map(|()| kind).map_err(fat_error);
    }
    let volume = resolve_volume(reference).ok_or(FS_NO_VOLUME)?;
    volume.for_each(|entry| each(entry.name(), entry.length, false, None)).map(|()| "FOSFS").map_err(|_| FS_ERROR)
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
    let Some(disk) = number.checked_sub(1).and_then(crate::block::get) else { return 0 };
    let mut info = DiskInfo { sectors: disk.sectors, boot: u32::from(storage::boot_disk() == Some(number - 1)), ..Default::default() };
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

/// Forgets the stream in progress (a new `fs_stream_open` replaces it).
pub fn stream_reset() {
    unsafe {
        STREAM = None;
        FAT_STREAM = None;
    }
}

pub fn stream_open(reference: &[u8], max_bytes: u64) -> isize {
    stream_reset();
    if let Some((mut fs, file)) = resolve_fat(reference) {
        if file.len() > 64 {
            return FS_BAD_NAME;
        }
        return match fs.create(file, max_bytes) {
            Ok(stream) => {
                let mut stored = [0u8; 64];
                stored[..file.len()].copy_from_slice(file);
                let capacity = stream.capacity();
                unsafe { FAT_STREAM = Some((fs, stream, stored, file.len())) };
                capacity as isize
            }
            Err(error) => fat_error(error),
        };
    }
    let (volume, name) = match resolve_file(reference, false) {
        Ok(found) => found,
        Err(code) => return code,
    };
    match volume.reserve(name, max_bytes) {
        Ok((entry, capacity)) => {
            unsafe { STREAM = Some((volume, entry)) };
            capacity as isize
        }
        Err(Error::InvalidName) => FS_BAD_NAME,
        Err(_) => FS_ERROR,
    }
}

pub fn stream_write(offset: u64, data: &[u8], file_length: u64) -> isize {
    if let Some((fs, stream, _, _)) = unsafe { (*(&raw mut FAT_STREAM)).as_mut() } {
        crate::fatvol::stamp(fs);
        return match fs.write(stream, offset, data, file_length) {
            Ok(()) => 0,
            Err(fat::Error::OutOfRange) => FS_TOO_BIG,
            Err(error) => fat_error(error),
        };
    }
    let Some((volume, entry)) = (unsafe { *(&raw const STREAM) }) else { return FS_ERROR };
    match volume.write_extent(&entry, offset, data, file_length) {
        Ok(()) => 0,
        Err(Error::TooBig) => FS_TOO_BIG,
        Err(_) => FS_ERROR,
    }
}

pub fn stream_close(file_length: u64) -> isize {
    if let Some((mut fs, stream, name, length)) = unsafe { (*(&raw mut FAT_STREAM)).take() } {
        crate::fatvol::stamp(&mut fs);
        return match fs.finish(&stream, &name[..length], file_length) {
            Ok(()) => 0,
            Err(error) => fat_error(error),
        };
    }
    let Some((volume, entry)) = (unsafe { (*(&raw mut STREAM)).take() }) else { return FS_ERROR };
    match volume.trim_extent(&entry, file_length) {
        Ok(()) => 0,
        Err(_) => FS_ERROR,
    }
}
