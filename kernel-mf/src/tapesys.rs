//! Machine-information calls for tapes: `hostname`, `peer_info`, `disk_info`, `disk_format_exfat`, `wall_clock`,
//! `wall_clock_set` and `console_size`. Each answers what the embedded kernel does, from the same shared code where there is
//! one (`kernel_core::tape_fs::disk_info`, `fatvol::format_exfat`, `kernel/src/clock.rs`); only the copy into the tape's memory
//! is mf's own, since a ring-3 pointer must be checked before the kernel writes through it.

use fosfor_abi::tape::{DiskInfo, FORMAT_BAD_LABEL, FORMAT_BOOT_DISK, FORMAT_FAILED, FORMAT_NO_DISK, PeerInfo, WallClock};
use kernel_core::fatvol::{self, FormatError};

use crate::{clock, fb, net, netclient, process, tapefs};

fn put<T: Copy>(address: u64, value: &T) -> bool {
    let bytes = unsafe { core::slice::from_raw_parts((value as *const T).cast::<u8>(), core::mem::size_of::<T>()) }; // kernel-memory
    process::copy_to_user(address, bytes)
}

/// `hostname(index, out, capacity)`: the name announced on interface `index`; 0 if it does not fit.
pub fn hostname(index: u64, out: u64, capacity: u64) -> u64 {
    let mut name = [0u8; 20];
    let length = net::hostname(index as usize, &mut name);
    if length as u64 > capacity || !process::copy_to_user(out, &name[..length]) {
        return 0;
    }
    length as u64
}

/// `peer_info(number, out)`: how many peers are known; `number` 1..=count also fills `out`.
pub fn peer_info(number: u64, out: u64) -> u64 {
    let mut info = PeerInfo::default();
    let count = netclient::peer_info(number as usize, &mut info);
    if number >= 1 && number as usize <= count && out != 0 {
        put(out, &info);
    }
    count as u64
}

/// `disk_info(number, out)`.
pub fn disk_info(number: u64, out: u64) -> u64 {
    let mut info = DiskInfo::default();
    let found = {
        let _disks = tapefs::lock();
        kernel_core::tape_fs::disk_info(number as usize, &mut info)
    };
    if found != 0 && put(out, &info) { u64::from(found) } else { 0 }
}

/// `disk_format_exfat(number, label, length)`: a `FORMAT_*` code, 0 on success.
pub fn format_exfat(number: u64, label: u64, length: u64) -> i64 {
    let Some(label) = process::user_bytes_up_to(label, length, 64) else { return i64::from(FORMAT_BAD_LABEL) };
    let _disks = tapefs::lock();
    match (number as usize).checked_sub(1).map_or(Err(FormatError::NoSuchDisk), |disk| fatvol::format_exfat(disk, &label)) {
        Ok(_) => 0,
        Err(FormatError::NoSuchDisk) => i64::from(FORMAT_NO_DISK),
        Err(FormatError::BootDisk) => i64::from(FORMAT_BOOT_DISK),
        Err(FormatError::BadLabel) => i64::from(FORMAT_BAD_LABEL),
        Err(FormatError::Failed) => i64::from(FORMAT_FAILED),
    }
}

/// `wall_clock(out)`: 1 and the local time, 0 while the clock is not set.
pub fn wall_clock(out: u64) -> u64 {
    let Some(time) = clock::now() else { return 0 };
    let value = WallClock { year: time.year, month: time.month, day: time.day, hour: time.hour, minute: time.minute, second: time.second, reserved: 0 };
    u64::from(put(out, &value))
}

/// `wall_clock_set(time)`: 1 if the date was valid and the clock and RTC were set.
pub fn wall_clock_set(address: u64) -> u64 {
    let Some(bytes) = process::user_bytes_up_to(address, core::mem::size_of::<WallClock>() as u64, 64) else { return 0 };
    if bytes.len() < core::mem::size_of::<WallClock>() {
        return 0;
    }
    // `bytes` is the kernel's copy of the tape's struct, at least as long as `WallClock`.
    let value = unsafe { core::ptr::read_unaligned(bytes.as_ptr().cast::<WallClock>()) }; // kernel-memory
    let time = fat::Time { year: value.year, month: value.month, day: value.day, hour: value.hour, minute: value.minute, second: value.second };
    u64::from(clock::set(&time).is_ok())
}

/// `console_size()`: columns << 16 | rows, 0 without a console.
pub fn console_size() -> u64 {
    fb::console_size().map_or(0, |(columns, rows)| (columns as u64) << 16 | rows as u64 & 0xffff)
}
