//! FOSFS against a RAM disk: what `make test-fosfs4` shows on a booted machine, here on the host, through the same
//! `fos_hal::Platform` the kernel installs. Each test uses its own disk number, so they may run in parallel.

use fos_hal::{BlockError, Platform};
use kernel_core::fosfs::Volume;
use std::collections::BTreeMap;
use std::sync::{Mutex, Once};

struct RamDisks(Mutex<BTreeMap<usize, Vec<u8>>>);

impl Platform for RamDisks {
    fn block_read(&self, disk: usize, lba: u64, buffer: &mut [u8]) -> Result<(), BlockError> {
        let disks = self.0.lock().unwrap();
        let data = disks.get(&disk).ok_or(BlockError::NoSuchDisk)?;
        let start = lba as usize * 512;
        buffer.copy_from_slice(data.get(start..start + buffer.len()).ok_or(BlockError::OutOfRange)?);
        Ok(())
    }

    fn block_write(&self, disk: usize, lba: u64, buffer: &[u8]) -> Result<(), BlockError> {
        let mut disks = self.0.lock().unwrap();
        let data = disks.get_mut(&disk).ok_or(BlockError::NoSuchDisk)?;
        let start = lba as usize * 512;
        data.get_mut(start..start + buffer.len()).ok_or(BlockError::OutOfRange)?.copy_from_slice(buffer);
        Ok(())
    }

    fn unix_seconds(&self) -> u64 {
        1_700_000_000
    }

    fn block_sectors(&self, disk: usize) -> Option<u64> {
        self.0.lock().unwrap().get(&disk).map(|data| data.len() as u64 / 512)
    }

    fn timer_counter(&self) -> u64 {
        0x1234_5678_9abc
    }

    fn timer_ticks(&self) -> u64 {
        42
    }

    fn print(&self, _: core::fmt::Arguments) {}
}

static DISKS: RamDisks = RamDisks(Mutex::new(BTreeMap::new()));
static INSTALL: Once = Once::new();

const SECTORS: u64 = 20_480; // 10 MiB

fn disk(number: usize) -> usize {
    INSTALL.call_once(|| fos_hal::set_platform(&DISKS));
    DISKS.0.lock().unwrap().insert(number, vec![0; SECTORS as usize * 512]);
    number
}

#[test]
fn v4_files_folders_and_remount() {
    let number = disk(1);
    let volume = Volume::format_version(number, 0, SECTORS, b"test", 4).unwrap();
    volume.mkdir(b"docs").unwrap();
    volume.write(b"docs/Hello.txt", b"hello, fosfs").unwrap();
    volume.write(b"top", &[7u8; 5000]).unwrap();

    let again = Volume::mount(number, 0, SECTORS).unwrap();
    assert_eq!(again.version(), 4);
    let mut buffer = [0u8; 64];
    let length = again.read(b"docs/Hello.txt", &mut buffer).unwrap().unwrap();
    assert_eq!(&buffer[..length], b"hello, fosfs");
    assert_eq!(again.file_length(b"top").unwrap(), Some(5000));
    assert!(again.remove(b"top").unwrap());
    assert_eq!(again.file_length(b"top").unwrap(), None);
    assert_eq!(again.check(|_| {}).unwrap(), (2, 0)); // the folder and one file left, no problems
}

#[test]
fn v3_names_are_restricted() {
    let number = disk(2);
    let volume = Volume::format_version(number, 0, SECTORS, b"old", 3).unwrap();
    assert!(volume.accepts_name(b"plain.txt"));
    assert!(!volume.accepts_name(b"Upper.txt"));
    assert!(volume.write(b"plain.txt", b"x").is_ok());
}

#[test]
fn a_damaged_directory_entry_is_reported() {
    let number = disk(3);
    let volume = Volume::format_version(number, 0, SECTORS, b"crc", 4).unwrap();
    volume.write(b"a", b"one").unwrap();
    volume.write(b"b", b"two").unwrap();
    // v4 directory starts at sector 2; flip a byte inside the first entry's name area (its CRC no longer matches).
    DISKS.0.lock().unwrap().get_mut(&number).unwrap()[2 * 512 + 40] ^= 0xff;
    let (_, problems) = volume.check(|_| {}).unwrap();
    assert_eq!(problems, 1);
}

#[test]
fn no_platform_disk_is_an_error_not_a_crash() {
    disk(4);
    assert!(Volume::mount(99, 0, SECTORS).is_err());
}

#[test]
fn a_partition_table_is_written_found_and_grown() {
    use kernel_core::gpt;
    let number = disk(5);
    assert!(matches!(gpt::inspect(number), Some(gpt::Summary::Blank)));
    let made = gpt::write_single_fosfs_partition(number).unwrap();
    let guid = fosfor_abi::FOSFS_TYPE_GUID;
    let found = gpt::find(number, &guid, |_| true).unwrap();
    assert_eq!((found.start, found.sectors), (made.start, made.sectors));
    assert!(matches!(gpt::inspect(number), Some(gpt::Summary::Gpt { fosfs: Some(_), other: 0 })));
}
