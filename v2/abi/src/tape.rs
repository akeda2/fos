//! The tape ABI, version 1: what the kernel hands a tape.
//!
//! A tape is a position-independent ELF64 executable whose entry point is
//! `extern "C" fn(api: &TapeApi)` (System V ABI). Everything the kernel offers is a
//! field of one table, so there is one thing to pass and nothing to count: where v1
//! appended up to 43 cdecl arguments, a tape here just reads the fields it needs.
//!
//! Append-only: fields are only ever added at the end, and `size` is the number of
//! bytes the kernel's table has, so a tape built against an older layout is unaffected
//! and a tape built against a newer one can check `size` before using a late field.
//! `read_key` and `should_abort` drain the same input: use one or the other.

/// Version 2 added `puts` (a whole string in one console call). A tape built for 2 checks the table's version before
/// using it, so it still runs on a kernel that only has version 1.
pub const TAPE_API_VERSION: u32 = 2;

/// `read_key` return values: nothing, abort (the operator asked the tape to stop), or
/// `KEY_BASE + byte` for a typed byte.
pub const KEY_NONE: u32 = 0;
pub const KEY_ABORT: u32 = 1;
pub const KEY_BASE: u32 = 0x100;

/// `fs_read`/`fs_write` error returns (negative).
pub const FS_NOT_FOUND: isize = -1;
pub const FS_TOO_BIG: isize = -2;
pub const FS_ERROR: isize = -3;
pub const FS_BAD_NAME: isize = -4;
pub const FS_NO_VOLUME: isize = -5;

#[repr(C)]
pub struct TapeApi {
    pub version: u32,
    /// `size_of` the table as the kernel built it.
    pub size: u32,
    /// Prints one byte on the console (screen, serial, and a `term net` client).
    pub putc: extern "C" fn(u32),
    /// 1 if the operator asked the tape to stop.
    pub should_abort: extern "C" fn() -> u32,
    /// Non-blocking: `KEY_NONE`, `KEY_ABORT`, or `KEY_BASE + byte`. Reads the local
    /// keyboard or the `term net` session, whichever has input.
    pub read_key: extern "C" fn() -> u32,
    /// Bit 0: raw keys (Backspace is delivered, not erased on screen by the kernel).
    pub set_input_mode: extern "C" fn(u32),
    /// Sleep until the next interrupt (a frame, a key, the timer tick). Use it when a
    /// polling pass found nothing to do, instead of spinning.
    pub wait_for_event: extern "C" fn(),
    /// Timer ticks since boot (100 per second).
    pub ticks: extern "C" fn() -> u64,
    /// Microseconds since boot.
    pub micros: extern "C" fn() -> u64,
    /// Allocates `bytes` (rounded up to whole 4 KiB pages) of zeroed physical RAM,
    /// identity-mapped; 0 if there is none. One pool, any size, anywhere in RAM.
    pub alloc: extern "C" fn(usize) -> usize,
    /// Returns what `alloc` gave (same address, same size). Nothing is freed
    /// automatically when a tape returns.
    pub free: extern "C" fn(usize, usize),
    /// The tape's command-line arguments: the raw bytes after `tape run`.
    pub args: *const u8,
    pub args_len: usize,
    /// `fs_read(name, name_len, buffer, capacity)`: a whole file from the boot FOSFS
    /// volume; its length, or a negative `FS_*` code. Too small a buffer is
    /// `FS_TOO_BIG`, never a truncation.
    pub fs_read: extern "C" fn(*const u8, usize, *mut u8, usize) -> isize,
    /// `fs_write(name, name_len, data, len)`: create or replace a file; 0 or a negative code.
    pub fs_write: extern "C" fn(*const u8, usize, *const u8, usize) -> isize,
    /// Network interfaces (zero-based indices).
    pub nic_count: extern "C" fn() -> usize,
    /// Writes the 6-byte MAC to the pointer; 1 on success.
    pub nic_mac: extern "C" fn(usize, *mut u8) -> u32,
    /// Sends one raw Ethernet frame; 1 if it was queued.
    pub nic_send: extern "C" fn(usize, *const u8, usize) -> u32,
    /// Copies the next received frame (not one of the terminal's own) into the buffer
    /// and returns its length, 0 if none.
    pub nic_receive: extern "C" fn(usize, *mut u8, usize) -> usize,
    pub nic_max_frame_len: extern "C" fn() -> usize,
    /// The firmware memory map: UEFI descriptors, `memory_map_descriptor_size` bytes
    /// apart (kind 7 is free RAM, kinds 1-6, 9, 10 are RAM in use or reclaimable).
    pub memory_map: *const u8,
    pub memory_map_count: usize,
    pub memory_map_descriptor_size: usize,
    /// Pages (4 KiB) of RAM the kernel's allocator still has free: a soak test compares it
    /// before and after to catch leaks.
    pub free_pages: extern "C" fn() -> usize,
    /// `fs_remove(name, name_len)`: delete a file; 0, or `FS_NOT_FOUND`/another negative code.
    pub fs_remove: extern "C" fn(*const u8, usize) -> isize,
    /// Promiscuous mode on/off for an interface (receive every frame); 1 on success.
    /// A tape that turns it on must turn it off before returning.
    pub nic_set_promiscuous: extern "C" fn(usize, u32) -> u32,
    /// USB audio. Fills `AudioInfo` (what the attached audio device offers and what is running);
    /// returns 1 if there is a device, 0 if not.
    pub audio_info: extern "C" fn(*mut AudioInfo) -> u32,
    /// Starts streams at `rate` Hz: capture and/or playback (non-zero = yes). 0 on success, or a
    /// negative `AUDIO_*` code. A started stream keeps running until `audio_stop`.
    pub audio_start: extern "C" fn(u32, u32, u32) -> i32,
    /// Stops every running stream and releases its buffers.
    pub audio_stop: extern "C" fn(),
    /// Copies up to `len` bytes of captured audio (interleaved frames, the format `AudioInfo` says) into
    /// the buffer; returns how many bytes it had. Never blocks; call it from the tape's loop, which is
    /// also what lets the kernel service the device's isochronous transfers.
    pub audio_read: extern "C" fn(*mut u8, usize) -> usize,
    /// Queues up to `len` bytes of playback audio (interleaved frames); returns how many fitted in the
    /// kernel's ring. A short return means "full: come back later".
    pub audio_write: extern "C" fn(*const u8, usize) -> usize,
    /// Fills `AudioStats` with the counters of the running streams.
    pub audio_stats: extern "C" fn(*mut AudioStats),
    /// Receive every multicast frame on an interface (on) or only broadcasts and our own address (off); 1 on
    /// success. A tape that joins an IGMP group needs this: the NIC drops multicast it has no filter for.
    pub nic_set_multicast: extern "C" fn(usize, u32) -> u32,
    /// `fs_stream_open(name, name_len, max_bytes)`: starts a file that grows as it is written, for recording to disk.
    /// A file is one contiguous run of sectors, so the space is reserved now: `max_bytes`, or the largest free run
    /// when 0. Returns the reserved size in bytes, or a negative code. One stream at a time; opening replaces the last.
    pub fs_stream_open: extern "C" fn(*const u8, usize, u64) -> isize,
    /// `fs_stream_write(offset, data, len, file_length)`: writes at a byte offset (a multiple of 512; the last sector
    /// of the data is padded) and, if `file_length` is not 0, makes that the file's length. 0 or a negative code.
    pub fs_stream_write: extern "C" fn(u64, *const u8, usize, u64) -> isize,
    /// `fs_stream_close(file_length)`: ends the stream, making `file_length` the final length and freeing the rest of
    /// the reserved space. Without it the whole reservation stays allocated to the file.
    pub fs_stream_close: extern "C" fn(u64) -> isize,
    /// `puts(text, length)`: prints a string through the console in one call (version 2). Printing a byte at a time with
    /// `putc` repaints the whole text grid for every byte; a full-screen tape needs this one.
    pub puts: extern "C" fn(*const u8, usize),
    /// `audio_select(direction, mode)`: picks which of the device's modes (alternate settings: channels and bit depth) the next
    /// `audio_start` uses; direction 0 capture, 1 playback; mode is the 1-based number `audio modes` shows, 0 = automatic (the
    /// best one that offers the rate). 0 on success, else an `AUDIO_*` code. It applies to the next start.
    pub audio_select: extern "C" fn(u32, u32) -> i32,
    /// `hostname(nic, buffer, capacity)`: the name this machine announces on interface `nic` (zero-based; the `hostname`
    /// file's name plus the one-based interface number, or `fos2`), copied into the buffer; its length, 0 if it did not fit.
    /// A tape puts it in its DHCP requests so the server learns the machine's name.
    pub hostname: extern "C" fn(usize, *mut u8, usize) -> usize,
    /// `disk_info(number, out)`: what disk `number` (one-based, as `disks` lists them) holds, for a tape that lets the user
    /// choose where to write. 0 if there is no such disk. The kernel mounts the volume to find out, so call it when a
    /// menu opens, not every frame. A tape built against a kernel without this call sees `size` smaller than its own
    /// `TapeApi` and must not call it.
    pub disk_info: extern "C" fn(usize, *mut DiskInfo) -> u32,
}

/// `DiskInfo::kind`: what a disk's volume is. FAT32 and exFAT disks are reached as `diskN/NAME`, like a FOSFS one.
pub const DISK_NO_VOLUME: u32 = 0;
pub const DISK_FOSFS: u32 = 1;
pub const DISK_FAT32: u32 = 2;
pub const DISK_EXFAT: u32 = 3;

/// What `disk_info` reports about one disk.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DiskInfo {
    pub sectors: u64,
    /// Free bytes in all and in the longest contiguous run (a recording needs one run); `u64::MAX` when not known
    /// (FAT32, or a disk with no volume).
    pub free_bytes: u64,
    pub largest_bytes: u64,
    /// `DISK_*`: the volume a tape can write files to (a disk with none is listed but unusable).
    pub kind: u32,
    /// 1 for the disk the system booted from.
    pub boot: u32,
    /// The drive's model text, space padded, and the FOSFS volume label (zero padded; empty for FAT).
    pub model: [u8; 40],
    pub label: [u8; 16],
}

impl Default for DiskInfo {
    fn default() -> Self {
        DiskInfo { sectors: 0, free_bytes: u64::MAX, largest_bytes: u64::MAX, kind: 0, boot: 0, model: [0; 40], label: [0; 16] }
    }
}

/// What `audio_info` reports. Slot bytes: how many bytes each sample takes in the stream (a 24-bit
/// sample usually travels in 3 bytes, little-endian, signed).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct AudioInfo {
    pub class_version: u32,
    pub playback_channels: u32,
    pub playback_slot_bytes: u32,
    pub playback_bits: u32,
    pub capture_channels: u32,
    pub capture_slot_bytes: u32,
    pub capture_bits: u32,
    pub rate_count: u32,
    pub rates: [u32; 8],
    /// Non-zero while the stream runs: the sample rate it runs at.
    pub capture_rate: u32,
    pub playback_rate: u32,
    /// Bytes of room in the playback ring and bytes waiting in the capture ring.
    pub playback_space: u32,
    pub capture_available: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct AudioStats {
    pub capture_packets: u64,
    pub capture_bytes: u64,
    pub capture_errors: u64,
    /// Captured bytes dropped because the tape did not read them in time.
    pub capture_overruns: u64,
    pub playback_packets: u64,
    pub playback_errors: u64,
    /// Packets sent as silence because the tape had not written audio in time.
    pub playback_underruns: u64,
}

/// `audio_start` errors (negative).
pub const AUDIO_NO_DEVICE: i32 = -1;
pub const AUDIO_BAD_RATE: i32 = -2;
pub const AUDIO_BUSY: i32 = -3;
pub const AUDIO_NO_DIRECTION: i32 = -4;
pub const AUDIO_FAILED: i32 = -5;
