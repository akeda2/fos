//! File calls for tapes (`fs_read`, `fs_read_at`, `fs_write`, `fs_remove`, `fs_list`, `fs_stream_*`), on the shared
//! `kernel_core::tape_fs` so a tape sees the same names, volumes and error codes as on the embedded kernel.
//!
//! `tape_fs` and the drivers under it keep their state in statics and know nothing of other CPUs, so every call here runs under one
//! lock (`LOCK`, also taken by the shell when it loads a tape). The lock keeps interrupts off while a disk transfer runs; the
//! drivers poll, so that is only a latency cost. A tape's memory is never handed to a driver: data goes through a kernel buffer
//! (a bounce buffer, at most `CHUNK` bytes for reads and `WRITE_LIMIT` for a whole-file write), as a driver's DMA needs physical
//! addresses the tape's virtual ones are not.
//!
//! The streams of `fs_stream_open*` are one global table in `tape_fs`: the first process to open one owns the table until it
//! ends (`process_ended` resets it), and another process's open fails meanwhile.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use fosfor_abi::tape::{FS_ERROR, FS_STREAMS, FS_TOO_BIG, FileInfo};
use kernel_core::tape_fs;

use crate::sync::SpinLock;
use crate::{process, sched};

pub static LOCK: SpinLock<()> = SpinLock::new(());

const CHUNK: usize = 1 << 20;
const WRITE_LIMIT: usize = 8 << 20;
const NAME_LIMIT: u64 = 128;
static STREAM_OWNER: AtomicU64 = AtomicU64::new(0);

fn name(address: u64, length: u64) -> Option<Vec<u8>> {
    if length > NAME_LIMIT {
        return None;
    }
    process::user_bytes(address, length)
}

/// `fs_read(name, name_len, buffer, capacity)`: the whole file, or `FS_TOO_BIG` when it does not fit `capacity`.
pub fn read(name_at: u64, name_len: u64, buffer: u64, capacity: u64) -> isize {
    let Some(reference) = name(name_at, name_len) else { return FS_ERROR };
    let length = {
        let _guard = LOCK.lock();
        tape_fs::length(&reference)
    };
    if length < 0 {
        return length;
    }
    if length as u64 > capacity {
        return FS_TOO_BIG;
    }
    let mut done = 0u64;
    while done < length as u64 {
        let got = read_at(&reference, done, buffer + done, (length as u64 - done).min(CHUNK as u64));
        if got <= 0 {
            return if got < 0 { got } else { FS_ERROR };
        }
        done += got as u64;
    }
    length
}

/// One kernel-buffered piece of a read: up to `count` bytes at `offset` stored at the user address `to`.
fn read_at(reference: &[u8], offset: u64, to: u64, count: u64) -> isize {
    let mut piece = alloc::vec![0u8; count as usize];
    let got = {
        let _guard = LOCK.lock();
        tape_fs::read_at(reference, offset, &mut piece)
    };
    if got > 0 && !process::copy_to_user(to, &piece[..got as usize]) {
        return FS_ERROR;
    }
    got
}

/// `fs_read_at(name, name_len, offset, buffer, capacity)` (up to `CHUNK` bytes per call; a short count only means a limit or the end).
pub fn read_range(name_at: u64, name_len: u64, offset: u64, buffer: u64, capacity: u64) -> isize {
    let Some(reference) = name(name_at, name_len) else { return FS_ERROR };
    read_at(&reference, offset, buffer, capacity.min(CHUNK as u64))
}

pub fn write(name_at: u64, name_len: u64, data: u64, length: u64) -> isize {
    let Some(reference) = name(name_at, name_len) else { return FS_ERROR };
    if length > WRITE_LIMIT as u64 {
        return FS_TOO_BIG;
    }
    let Some(bytes) = copy_from_user(data, length) else { return FS_ERROR };
    let _guard = LOCK.lock();
    tape_fs::write(&reference, &bytes)
}

pub fn remove(name_at: u64, name_len: u64) -> isize {
    let Some(reference) = name(name_at, name_len) else { return FS_ERROR };
    let _guard = LOCK.lock();
    tape_fs::remove(&reference)
}

/// `fs_list(volume, volume_len, index, out)`: 1 and a `FileInfo` at `out`, 0 past the end, or a negative code.
pub fn list(volume_at: u64, volume_len: u64, index: u64, out: u64) -> isize {
    let Some(volume) = name(volume_at, volume_len) else { return FS_ERROR };
    let mut info = FileInfo { length: 0, flags: 0, name_length: 0, name: [0; 64] };
    let found = {
        let _guard = LOCK.lock();
        tape_fs::list(&volume, index as usize, &mut info)
    };
    if found == 1 {
        let bytes = unsafe { core::slice::from_raw_parts(&info as *const FileInfo as *const u8, core::mem::size_of::<FileInfo>()) };
        if !process::copy_to_user(out, bytes) {
            return FS_ERROR;
        }
    }
    found
}

fn claim_streams() -> bool {
    let pid = sched::current_pid();
    STREAM_OWNER.compare_exchange(0, pid, Ordering::AcqRel, Ordering::Acquire).is_ok() || STREAM_OWNER.load(Ordering::Acquire) == pid
}

/// `fs_stream_open(name, name_len, max_bytes)`.
pub fn stream_open(name_at: u64, name_len: u64, max_bytes: u64) -> isize {
    let Some(reference) = name(name_at, name_len) else { return FS_ERROR };
    if !claim_streams() {
        return FS_ERROR;
    }
    let _guard = LOCK.lock();
    tape_fs::stream_open(&reference, max_bytes)
}

/// `fs_stream_open2(name, name_len, max_bytes, capacity_out)`: a handle; the reserved capacity goes to `*capacity_out`.
pub fn stream_open2(name_at: u64, name_len: u64, max_bytes: u64, capacity_out: u64) -> isize {
    let Some(reference) = name(name_at, name_len) else { return FS_ERROR };
    if !claim_streams() {
        return FS_ERROR;
    }
    let mut capacity = 0u64;
    let handle = {
        let _guard = LOCK.lock();
        tape_fs::api_fs_stream_open2(reference.as_ptr(), reference.len(), max_bytes, &mut capacity)
    };
    if handle >= 0 && !process::copy_to_user(capacity_out, &capacity.to_le_bytes()) {
        let _guard = LOCK.lock();
        tape_fs::api_fs_stream_close2(handle, 0);
        return FS_ERROR;
    }
    handle
}

/// A stream write in pieces of `CHUNK` (a multiple of 512, so every piece but the last keeps the sector rule).
fn stream_data(handle: Option<isize>, offset: u64, data: u64, length: u64, file_length: u64) -> isize {
    if STREAM_OWNER.load(Ordering::Acquire) != sched::current_pid() {
        return FS_ERROR;
    }
    let mut done = 0u64;
    loop {
        let piece = (length - done).min(CHUNK as u64);
        let Some(bytes) = copy_from_user(data + done, piece) else { return FS_ERROR };
        let result = {
            let _guard = LOCK.lock();
            match handle {
                Some(handle) => tape_fs::api_fs_stream_write2(handle, offset + done, bytes.as_ptr(), bytes.len(), file_length),
                None => tape_fs::stream_write(offset + done, &bytes, file_length),
            }
        };
        if result < 0 {
            return result;
        }
        done += piece;
        if done >= length {
            return 0;
        }
    }
}

pub fn stream_write(offset: u64, data: u64, length: u64, file_length: u64) -> isize {
    stream_data(None, offset, data, length, file_length)
}

pub fn stream_write2(handle: u64, offset: u64, data: u64, length: u64, file_length: u64) -> isize {
    if handle >= FS_STREAMS as u64 {
        return FS_ERROR;
    }
    stream_data(Some(handle as isize), offset, data, length, file_length)
}

pub fn stream_close(file_length: u64) -> isize {
    if STREAM_OWNER.load(Ordering::Acquire) != sched::current_pid() {
        return FS_ERROR;
    }
    let _guard = LOCK.lock();
    tape_fs::stream_close(file_length)
}

pub fn stream_close2(handle: u64, file_length: u64) -> isize {
    if handle >= FS_STREAMS as u64 || STREAM_OWNER.load(Ordering::Acquire) != sched::current_pid() {
        return FS_ERROR;
    }
    let _guard = LOCK.lock();
    tape_fs::api_fs_stream_close2(handle as isize, file_length)
}

/// The process `pid` ended: drop its streams (a file left open keeps what was written and its reservation).
pub fn process_ended(pid: u64) {
    if STREAM_OWNER.compare_exchange(pid, 0, Ordering::AcqRel, Ordering::Acquire).is_ok() {
        let _guard = LOCK.lock();
        tape_fs::stream_reset();
    }
}

fn copy_from_user(address: u64, length: u64) -> Option<Vec<u8>> {
    process::user_bytes_up_to(address, length, WRITE_LIMIT as u64)
}
