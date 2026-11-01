//! The platform interface of the shared kernel code.
//!
//! `kernel-core` modules (the filesystems first) never name a driver or an architecture: they call the functions here, and
//! whichever core is running has installed a [`Platform`] that answers them. This is the "slow path" half of the
//! plan in `docs/big-kernel.md`: one indirect call per disk transfer or log line costs nothing measurable. Hot paths (port
//! I/O, `micros`) are not here; they stay inline in the code that is generic over its core.
//!
//! The platform is set once, before the first call and before other CPUs run; it is never replaced while in use.

#![no_std]

use core::fmt;

/// Why a block transfer failed (the same three reasons the kernel's disk layer has).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockError {
    NoSuchDisk,
    OutOfRange,
    Device,
}

/// What a core provides. Every method is cheap to implement by forwarding to the core's own modules.
pub trait Platform: Sync {
    /// Reads whole 512-byte sectors (`buffer.len()` a multiple of 512) of disk `disk` starting at `lba`.
    fn block_read(&self, disk: usize, lba: u64, buffer: &mut [u8]) -> Result<(), BlockError>;
    /// Writes whole 512-byte sectors.
    fn block_write(&self, disk: usize, lba: u64, buffer: &[u8]) -> Result<(), BlockError>;
    /// The wall clock as Unix seconds (local reading taken as UTC), 0 when it was never set.
    fn unix_seconds(&self) -> u64;
    /// The size of disk `disk` in 512-byte sectors, `None` when there is no such disk.
    fn block_sectors(&self, disk: usize) -> Option<u64>;
    /// A free-running hardware counter (TSC or the generic timer's count); only its low bits are meant as noise.
    fn timer_counter(&self) -> u64;
    /// Timer interrupt ticks since boot.
    fn timer_ticks(&self) -> u64;
    /// Console text, as `println!` produces it.
    fn print(&self, arguments: fmt::Arguments);
}

static mut PLATFORM: Option<&'static dyn Platform> = None;

/// Installs the platform. Call once, early, from the core's entry code.
pub fn set_platform(platform: &'static dyn Platform) {
    unsafe { *core::ptr::addr_of_mut!(PLATFORM) = Some(platform) };
}

fn platform() -> Option<&'static dyn Platform> {
    unsafe { *core::ptr::addr_of!(PLATFORM) }
}

/// Block devices, as the platform numbers them (`disk1` is 1... as the kernel's `block` module does).
pub mod block {
    use super::{BlockError, platform};

    pub type Error = BlockError;

    pub fn read(disk: usize, lba: u64, buffer: &mut [u8]) -> Result<(), Error> {
        platform().ok_or(BlockError::NoSuchDisk)?.block_read(disk, lba, buffer)
    }

    pub fn write(disk: usize, lba: u64, buffer: &[u8]) -> Result<(), Error> {
        platform().ok_or(BlockError::NoSuchDisk)?.block_write(disk, lba, buffer)
    }

    /// Size of the disk in sectors.
    pub fn sectors(disk: usize) -> Option<u64> {
        platform()?.block_sectors(disk)
    }
}

pub mod timer {
    pub fn counter() -> u64 {
        super::platform().map_or(0, |platform| platform.timer_counter())
    }

    pub fn ticks() -> u64 {
        super::platform().map_or(0, |platform| platform.timer_ticks())
    }
}

pub mod clock {
    /// Unix seconds of the wall clock; 0 when unset or when no platform is installed.
    pub fn unix_seconds() -> u64 {
        super::platform().map_or(0, |platform| platform.unix_seconds())
    }
}

/// `println!` for shared code: formats and hands the text to the platform's console.
pub fn print(arguments: fmt::Arguments) {
    if let Some(platform) = platform() {
        platform.print(arguments);
    }
}

#[macro_export]
macro_rules! println {
    () => { $crate::print(format_args!("\n")) };
    ($($argument:tt)*) => { $crate::print(format_args!("{}\n", format_args!($($argument)*))) };
}
