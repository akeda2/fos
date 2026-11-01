//! The embedded kernel's answer to `fos_hal::Platform`: shared code (`kernel-core`) reaches disks, the clock and the console
//! through this, so it never names a driver. Installed first thing in `kmain`.

use fos_hal::{BlockError, Platform};

struct Embedded;

fn convert(error: crate::block::Error) -> BlockError {
    match error {
        crate::block::Error::NoSuchDisk => BlockError::NoSuchDisk,
        crate::block::Error::OutOfRange => BlockError::OutOfRange,
        crate::block::Error::Device => BlockError::Device,
    }
}

impl Platform for Embedded {
    fn block_read(&self, disk: usize, lba: u64, buffer: &mut [u8]) -> Result<(), BlockError> {
        crate::block::read(disk, lba, buffer).map_err(convert)
    }

    fn block_write(&self, disk: usize, lba: u64, buffer: &[u8]) -> Result<(), BlockError> {
        crate::block::write(disk, lba, buffer).map_err(convert)
    }

    fn unix_seconds(&self) -> u64 {
        crate::clock::unix_seconds()
    }

    fn print(&self, arguments: core::fmt::Arguments) {
        crate::print(arguments);
    }
}

static EMBEDDED: Embedded = Embedded;

pub fn install() {
    fos_hal::set_platform(&EMBEDDED);
}
