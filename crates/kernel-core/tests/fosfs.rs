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

    fn block_info(&self, disk: usize) -> Option<fos_hal::BlockInfo> {
        self.0.lock().unwrap().get(&disk).map(|data| fos_hal::BlockInfo { sectors: data.len() as u64 / 512, model: [b' '; 40] })
    }

    fn block_count(&self) -> usize {
        self.0.lock().unwrap().keys().max().map_or(0, |last| last + 1)
    }

    fn block_register_usb(&self, _: usize, _: u64, _: [u8; 40], _: &str) -> Option<usize> {
        None
    }

    fn block_usb_disk(&self, _: usize) -> Option<usize> {
        None
    }

    fn boot_disk(&self) -> Option<usize> {
        Some(9)
    }

    fn timer_micros(&self) -> u64 {
        7
    }

    fn timer_counter(&self) -> u64 {
        0x1234_5678_9abc
    }

    fn timer_ticks(&self) -> u64 {
        42
    }

    // No hardware on the host: the storage tests never reach these.
    fn allocate_contiguous(&self, _: u64, _: Option<u64>) -> Option<u64> {
        None
    }
    fn map_device(&self, _: u64, _: u64) {}
    fn free_frames(&self, _: u64, _: u64) {}
    fn wait_ticks(&self, _: u64) {}
    fn pci_read32(&self, _: u8, _: u8, _: u8, _: u8) -> u32 {
        u32::MAX
    }
    fn pci_write32(&self, _: u8, _: u8, _: u8, _: u8, _: u32) {}
    fn msi_allocate(&self, _: fn(usize), _: usize) -> Option<u8> {
        None
    }
    fn msi_release(&self, _: u8) {}
    fn msi_count(&self, _: u8) -> u64 {
        0
    }
    fn msi_message(&self, _: u8) -> (u64, u32) {
        (0, 0)
    }
    fn msi_claim(&self) {}
    fn msi_intx_candidates(&self, _: u8) -> [u32; 9] {
        [u32::MAX; 9]
    }
    fn msi_attach_intx(&self, _: u32, _: fn(usize), _: usize) -> Option<u8> {
        None
    }
    fn msi_detach_intx(&self, _: u8, _: usize) {}
    fn interrupt_target(&self) -> u32 {
        0
    }
    fn gsi_range(&self) -> (u32, u32) {
        (0, 0)
    }

    fn print(&self, _: core::fmt::Arguments) {}
}

static DISKS: RamDisks = RamDisks(Mutex::new(BTreeMap::new()));
static INSTALL: Once = Once::new();
static SERIAL: Mutex<()> = Mutex::new(());

thread_local! {
    static HELD: std::cell::RefCell<Option<std::sync::MutexGuard<'static, ()>>> = const { std::cell::RefCell::new(None) };
}

const SECTORS: u64 = 20_480; // 10 MiB

/// Makes a blank disk. The kernel code keeps its scratch tables in statics (one CPU, no preemption), so a test holds a
/// global lock from its first disk until it ends and the tests run one at a time.
fn disk(number: usize) -> usize {
    HELD.with(|held| {
        let mut held = held.borrow_mut();
        if held.is_none() {
            *held = Some(SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner()));
        }
    });
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

#[test]
fn an_exfat_disk_is_formatted_mounted_and_stamped_with_the_clock() {
    use kernel_core::fatvol;
    let number = disk(6);
    let boot = disk(9);
    assert_eq!(fatvol::format_exfat(boot, b"X"), Err(fatvol::FormatError::BootDisk));
    assert!(fatvol::format_exfat(number, b"DEMO").unwrap() > 0);
    let mut fs = fatvol::mount(number).unwrap();
    let stream = fs.create(b"hello.txt", 5).unwrap();
    fs.write(&stream, 0, b"hello", 5).unwrap();
    let file = fs.find(b"hello.txt").unwrap().unwrap();
    let mut buffer = [0u8; 5];
    assert_eq!(fs.read(&file, 0, &mut buffer).unwrap(), 5);
    assert_eq!(&buffer, b"hello");
}

#[test]
fn fstab_fills_the_mount_table_and_the_longest_mount_point_wins() {
    use kernel_core::mounts;
    let number = disk(7);
    let volume = Volume::format_version(number, 0, SECTORS, b"boot", 4).unwrap();
    volume.mkdir(b"settings").unwrap();
    volume.write(b"settings/fstab", b"# comment\nconfig @cfgstick\nconfig/deep @other\nnonsense\n").unwrap();
    assert_eq!(mounts::load_from(Some(volume)), (2, 1));
    let (label, rest) = mounts::lookup(b"config/phono.cfg").unwrap();
    assert_eq!((label.as_bytes(), rest), (&b"cfgstick"[..], &b"phono.cfg"[..]));
    let (label, rest) = mounts::lookup(b"config/deep/x").unwrap();
    assert_eq!((label.as_bytes(), rest), (&b"other"[..], &b"x"[..]));
    assert!(mounts::lookup(b"config").is_none() && mounts::lookup_dir(b"config").is_some());
    assert_eq!(mounts::absent_with(|label| label == b"cfgstick"), 1);
    mounts::clear();
}

#[test]
fn volumes_are_found_by_label_and_the_boot_volume_is_remembered() {
    use kernel_core::volumes;
    let number = disk(8);
    let table = gpt_fosfs(number);
    assert!(table);
    let (found, _) = volumes::volume_labelled(b"findme").unwrap();
    assert_eq!(found, number + 1);
    assert!(volumes::volume_labelled(b"nobody").is_none());
    assert!(matches!(volumes::volume_on(0), Err(volumes::VolumeError::NoSuchDisk)));
    volumes::set_boot_volume(volumes::volume_on(number + 1).unwrap());
    assert!(volumes::boot_volume().is_some());
}

/// Puts a GPT with one FOSFS partition and a labelled volume on the disk.
fn gpt_fosfs(number: usize) -> bool {
    let partition = kernel_core::gpt::write_single_fosfs_partition(number).unwrap();
    Volume::format_version(number, partition.start, partition.sectors, b"findme", 4).is_ok()
}
