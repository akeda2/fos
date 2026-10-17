//! Saving a measurement: a CSV of the four views and a PNG of the screen, side by side on an exFAT (or FAT32) disk under a
//! name with the date and time, so a PC can open them and the next save never overwrites this one.

use core::fmt::Write;
use tape_rt::{DISK_EXFAT, DISK_FAT32, DiskInfo, TapeApi};

/// Room for the CSV (about 60 bytes a row, at most 8193 rows) and for the PNG (a flat chart is tens of KB).
pub const BUFFER_BYTES: usize = 1 << 20;

/// A `fmt::Write` over a byte slice that fails when it is full.
pub struct Sink<'a> {
    pub bytes: &'a mut [u8],
    pub length: usize,
}

impl Write for Sink<'_> {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        let end = self.length + text.len();
        if end > self.bytes.len() {
            return Err(core::fmt::Error);
        }
        self.bytes[self.length..end].copy_from_slice(text.as_bytes());
        self.length = end;
        Ok(())
    }
}

/// The disk to save on: the one asked for with `save=N`, else the first exFAT disk, else the first FAT32 disk.
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

/// `ltas-YYYYMMDD-HHMMSS` from the machine's clock, else `ltas-NNN` from a counter; `stamp` gets the readable form.
pub fn names(counter: u32, base: &mut impl Write, stamp: &mut impl Write) {
    match tape_rt::wall_clock() {
        Some(t) if t.year >= 2000 => {
            let _ = write!(base, "ltas-{:04}{:02}{:02}-{:02}{:02}{:02}", t.year, t.month, t.day, t.hour, t.minute, t.second);
            let _ = write!(stamp, "{:04}-{:02}-{:02} {:02}:{:02}:{:02}", t.year, t.month, t.day, t.hour, t.minute, t.second);
        }
        _ => {
            let _ = write!(base, "ltas-{counter:03}");
            let _ = write!(stamp, "measurement {counter}");
        }
    }
}

/// Writes `diskN/BASE.EXT`; 0 on success, else the tape ABI's `FS_*` code (negative).
pub fn write_file(api: &TapeApi, disk: usize, base: &str, extension: &str, data: &[u8]) -> isize {
    let mut name = [0u8; 48];
    let mut sink = Sink { bytes: &mut name, length: 0 };
    if write!(sink, "disk{disk}/{base}.{extension}").is_err() {
        return -1;
    }
    let length = sink.length;
    (api.fs_write)(name.as_ptr(), length, data.as_ptr(), data.len())
}
