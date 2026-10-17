//! Saving a result from a tape to a PC-readable disk: pick an exFAT (or FAT32) volume, name the files with the date and time
//! so the next save never overwrites this one, and write them with `fs_write("diskN/NAME")`. `Sink` is the `fmt::Write` (and
//! byte) buffer a CSV or PNG is built in before the single write.

use crate::{DISK_EXFAT, DISK_FAT32, DiskInfo, TapeApi};
use core::fmt::Write;

/// A `fmt::Write` over a byte slice that fails when it is full.
pub struct Sink<'a> {
    pub bytes: &'a mut [u8],
    pub length: usize,
}

impl<'a> Sink<'a> {
    pub fn new(bytes: &'a mut [u8]) -> Self {
        Self { bytes, length: 0 }
    }

    /// Appends raw bytes; false (nothing appended) when they do not fit.
    pub fn push(&mut self, data: &[u8]) -> bool {
        let end = self.length + data.len();
        if end > self.bytes.len() {
            return false;
        }
        self.bytes[self.length..end].copy_from_slice(data);
        self.length = end;
        true
    }

    /// What has been written.
    pub fn written(&self) -> &[u8] {
        &self.bytes[..self.length]
    }
}

impl Write for Sink<'_> {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        if self.push(text.as_bytes()) { Ok(()) } else { Err(core::fmt::Error) }
    }
}

/// Room for a text result (a CSV of about 60 bytes a row) and for a PNG of the screen (a flat chart is tens of KB).
pub const BUFFER_BYTES: usize = 1 << 20;

/// Runs `f` with two `BUFFER_BYTES` scratch buffers from the kernel (freed afterwards); None when it has no room.
pub fn with_buffers<R>(api: &TapeApi, f: impl FnOnce(&mut [u8], &mut [u8]) -> R) -> Option<R> {
    let address = (api.alloc)(2 * BUFFER_BYTES);
    if address == 0 {
        return None;
    }
    let (first, second) = unsafe {
        (
            core::slice::from_raw_parts_mut(address as *mut u8, BUFFER_BYTES),
            core::slice::from_raw_parts_mut((address + BUFFER_BYTES) as *mut u8, BUFFER_BYTES),
        )
    };
    let result = f(first, second);
    (api.free)(address, 2 * BUFFER_BYTES);
    Some(result)
}

/// The disk to save on: the one asked for (`save=N`), else the first exFAT disk, else the first FAT32 disk. A kernel without
/// `disk_info` has none.
pub fn find_disk(api: &TapeApi, wanted: Option<usize>) -> Option<usize> {
    let end = core::mem::offset_of!(TapeApi, disk_info) + core::mem::size_of::<usize>();
    if (api.size as usize) < end {
        return None;
    }
    let mut first_fat = None;
    for number in 1..=16 {
        let mut info = DiskInfo::default();
        if (api.disk_info)(number, &mut info) == 0 {
            break;
        }
        if wanted == Some(number) {
            return (info.kind == DISK_EXFAT || info.kind == DISK_FAT32).then_some(number);
        }
        if wanted.is_none() {
            if info.kind == DISK_EXFAT {
                return Some(number);
            }
            if info.kind == DISK_FAT32 && first_fat.is_none() {
                first_fat = Some(number);
            }
        }
    }
    first_fat
}

/// `PREFIX-YYYYMMDD-HHMMSS` from the machine's clock, else `PREFIX-NNN` from a counter. `stamp` gets the readable form
/// (`YYYY-MM-DD HH:MM:SS`, or `measurement N`).
pub fn names(prefix: &str, counter: u32, base: &mut impl Write, stamp: &mut impl Write) {
    match crate::wall_clock() {
        Some(t) if t.year >= 2000 => {
            let _ = write!(base, "{prefix}-{:04}{:02}{:02}-{:02}{:02}{:02}", t.year, t.month, t.day, t.hour, t.minute, t.second);
            let _ = write!(stamp, "{:04}-{:02}-{:02} {:02}:{:02}:{:02}", t.year, t.month, t.day, t.hour, t.minute, t.second);
        }
        _ => {
            let _ = write!(base, "{prefix}-{counter:03}");
            let _ = write!(stamp, "measurement {counter}");
        }
    }
}

/// Writes `diskN/BASE.EXT`; 0 on success, else the tape ABI's `FS_*` code (negative).
pub fn write_file(api: &TapeApi, disk: usize, base: &str, extension: &str, data: &[u8]) -> isize {
    let mut name = [0u8; 64];
    let mut sink = Sink::new(&mut name);
    if write!(sink, "disk{disk}/{base}.{extension}").is_err() {
        return -1;
    }
    let length = sink.length;
    (api.fs_write)(name.as_ptr(), length, data.as_ptr(), data.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sink_refuses_overflow() {
        let mut bytes = [0u8; 4];
        let mut sink = Sink::new(&mut bytes);
        assert!(sink.push(b"ab"));
        assert!(!sink.push(b"cde"));
        assert!(write!(sink, "cd").is_ok());
        assert!(write!(sink, "e").is_err());
        assert_eq!(sink.written(), b"abcd");
    }
}
