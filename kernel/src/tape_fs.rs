//! The FOSFS side of the tape file calls (`fs_read`, `fs_write`, `fs_remove`, `fs_stream_*`), shared by both ports so a tape
//! sees the same names, volumes and error codes on x86-64 and aarch64. `tape.rs` adds FAT32/exFAT in front of these (the
//! aarch64 kernel has no FAT driver yet).
//!
//! A reference is `NAME` (the boot volume), `diskN/NAME` or `@LABEL/NAME`.

use fosfor_abi::tape::*;

use crate::fosfs::{Entry, Error, Volume};
use crate::storage;

/// The file a tape is streaming into (`fs_stream_open`): its volume and reserved entry.
static mut STREAM: Option<(Volume, Entry)> = None;

/// Resolves a tape's file reference to its volume and the name on it.
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
        let number = selector.strip_prefix(b"disk").and_then(|digits| core::str::from_utf8(digits).ok()).and_then(|digits| digits.parse::<usize>().ok());
        number.and_then(|number| storage::volume_on(number).ok())
    };
    volume.map(|volume| (volume, name)).ok_or(FS_NO_VOLUME)
}

pub fn read(reference: &[u8], buffer: &mut [u8]) -> isize {
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

/// Forgets the stream in progress (a new `fs_stream_open` replaces it).
pub fn stream_reset() {
    unsafe { STREAM = None };
}

pub fn stream_open(reference: &[u8], max_bytes: u64) -> isize {
    stream_reset();
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
    let Some((volume, entry)) = (unsafe { *(&raw const STREAM) }) else { return FS_ERROR };
    match volume.write_extent(&entry, offset, data, file_length) {
        Ok(()) => 0,
        Err(Error::TooBig) => FS_TOO_BIG,
        Err(_) => FS_ERROR,
    }
}

pub fn stream_close(file_length: u64) -> isize {
    let Some((volume, entry)) = (unsafe { (*(&raw mut STREAM)).take() }) else { return FS_ERROR };
    match volume.trim_extent(&entry, file_length) {
        Ok(()) => 0,
        Err(_) => FS_ERROR,
    }
}
