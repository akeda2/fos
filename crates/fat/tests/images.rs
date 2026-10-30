//! The driver against real volumes: images made by `mkfs.vfat` / `mkfs.exfat`, written through `Fs`, then checked by
//! `fsck.vfat` / `fsck.exfat` and read back by 7-Zip (an independent implementation of both formats).

use fat::{Device, Error, Fs, Kind, SECTOR, Time};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

struct FileDevice(File);

impl Device for FileDevice {
    fn read(&mut self, lba: u64, buffer: &mut [u8]) -> Result<(), Error> {
        self.0.seek(SeekFrom::Start(lba * SECTOR as u64)).map_err(|_| Error::Device)?;
        self.0.read_exact(buffer).map_err(|_| Error::Device)
    }
    fn write(&mut self, lba: u64, buffer: &[u8]) -> Result<(), Error> {
        self.0.seek(SeekFrom::Start(lba * SECTOR as u64)).map_err(|_| Error::Device)?;
        self.0.write_all(buffer).map_err(|_| Error::Device)
    }
}

fn tool(name: &str) -> bool {
    Command::new(name).arg("--help").output().is_ok() || Path::new("/usr/sbin").join(name).exists()
}

/// Removes an image when its test ends (they can be several GiB, sparse).
struct Cleanup(PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fos-fat-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

fn make_image(path: &Path, megabytes: u64, mkfs: &[&str]) {
    let file = File::create(path).unwrap();
    file.set_len(megabytes << 20).unwrap();
    drop(file);
    let status = Command::new(mkfs[0]).args(&mkfs[1..]).arg(path).output().unwrap();
    assert!(status.status.success(), "{mkfs:?}: {}", String::from_utf8_lossy(&status.stderr));
}

fn open(path: &Path) -> Fs<FileDevice> {
    Fs::mount(FileDevice(OpenOptions::new().read(true).write(true).open(path).unwrap())).unwrap()
}

fn pattern(offset: usize, length: usize) -> Vec<u8> {
    (offset..offset + length).map(|n| (n.wrapping_mul(2654435761) >> 13) as u8).collect()
}

/// Records `total` bytes as `name` in blocks of `block` bytes, with a checkpoint length after every block.
fn record(fs: &mut Fs<FileDevice>, name: &[u8], reserve: u64, total: usize, block: usize) {
    let stream = fs.create(name, reserve).unwrap();
    let mut written = 0;
    while written < total {
        let count = block.min(total - written);
        // Only whole blocks go in the middle, as the recorder does; the last one may be short.
        let data = pattern(written, count);
        fs.write(&stream, written as u64, &data, (written + count) as u64).unwrap();
        written += count;
    }
    fs.finish(&stream, name, total as u64).unwrap();
}

fn verify_read(fs: &mut Fs<FileDevice>, name: &[u8], total: usize) {
    let file = fs.find(name).unwrap().unwrap_or_else(|| panic!("{} not found", String::from_utf8_lossy(name)));
    assert_eq!(file.size, total as u64);
    let mut data = vec![0u8; total];
    assert_eq!(fs.read(&file, 0, &mut data).unwrap(), total);
    assert!(data == pattern(0, total), "content of {} differs", String::from_utf8_lossy(name));
    // And from the middle of a cluster.
    if total <= 3000 {
        return;
    }
    let mut part = vec![0u8; 1000.min(total - 3000)];
    assert_eq!(fs.read(&file, 3000, &mut part).unwrap(), part.len());
    assert!(part == pattern(3000, part.len()));
}

fn fsck(path: &Path, command: &[&str]) {
    let output = Command::new(command[0]).args(&command[1..]).arg(path).output().unwrap();
    assert!(output.status.success(), "{command:?} failed:\n{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
}

/// What 7-Zip reads out of the image under `name` (7-Zip reads FAT but not exFAT, which only `fsck.exfat` checks here).
fn seven_zip(path: &Path, name: &str) -> Option<Vec<u8>> {
    if !tool("7z") {
        return None;
    }
    let output = Command::new("7z").args(["e", "-so", "-y"]).arg(path).arg(name).output().ok()?;
    output.status.success().then_some(output.stdout)
}

fn exercise(path: &Path, kind: Kind, fsck_command: &[&str]) {
    let mut fs = open(path);
    assert_eq!(fs.kind(), kind);
    fs.set_time(Time { year: 2026, month: 10, day: 6, hour: 12, minute: 30, second: 10 });
    // A short name, a long name with spaces and capitals, and a size that is not a whole sector or cluster.
    record(&mut fs, b"take001.wav", 8 << 20, 3_000_017, 262_144);
    record(&mut fs, b"Gig Night 1.WAV", 0, 1_234_567, 65_536);
    record(&mut fs, b"tiny.bin", 1 << 20, 100, 4096);
    verify_read(&mut fs, b"take001.wav", 3_000_017);
    verify_read(&mut fs, b"gig night 1.wav", 1_234_567); // case-insensitive
    verify_read(&mut fs, b"tiny.bin", 100);
    // Replacing a name frees the old file's space.
    record(&mut fs, b"take001.wav", 8 << 20, 700_000, 262_144);
    verify_read(&mut fs, b"take001.wav", 700_000);
    // A file with nothing written disappears again.
    let stream = fs.create(b"empty.wav", 1 << 20).unwrap();
    fs.finish(&stream, b"empty.wav", 0).unwrap();
    assert!(fs.find(b"empty.wav").unwrap().is_none());
    // Many files: forces the root directory to grow past its first cluster/sector.
    for number in 0..40 {
        let name = format!("file number {number:03} with a long name.dat");
        record(&mut fs, name.as_bytes(), 1 << 16, 5000, 4096);
    }
    verify_read(&mut fs, b"file number 039 with a long name.dat", 5000);
    // The listing shows names as written (long ones too), sizes and the time the files were last written.
    let mut listed = Vec::new();
    fs.list(|entry| listed.push((String::from_utf8_lossy(entry.name()).to_string(), entry.size, entry.modified))).unwrap();
    assert_eq!(listed.len(), 3 + 40, "{listed:?}");
    let wanted = Time { year: 2026, month: 10, day: 6, hour: 12, minute: 30, second: 10 };
    assert!(listed.contains(&("take001.wav".to_string(), 700_000, Some(wanted))), "{listed:?}");
    assert!(listed.iter().any(|(name, size, _)| name == "Gig Night 1.WAV" && *size == 1_234_567), "{listed:?}");
    // Removing frees the space and the name.
    assert!(fs.remove(b"tiny.bin").unwrap());
    assert!(!fs.remove(b"tiny.bin").unwrap());
    assert!(fs.find(b"tiny.bin").unwrap().is_none());
    drop(fs);
    fsck(path, fsck_command);
    if let Some(bytes) = seven_zip(path, "take001.wav") {
        assert!(bytes == pattern(0, 700_000), "7-Zip read take001.wav differently");
        let bytes = seven_zip(path, "Gig Night 1.WAV").expect("7-Zip finds the long name");
        assert!(bytes == pattern(0, 1_234_567));
    }
    // A fresh mount sees the same files.
    let mut fs = open(path);
    verify_read(&mut fs, b"take001.wav", 700_000);
}

#[test]
fn fat32_small_clusters() {
    if !tool("mkfs.vfat") {
        return;
    }
    let path = scratch("fat32a.img");
    let _cleanup = Cleanup(path.clone());
    make_image(&path, 128, &["mkfs.vfat", "-F", "32", "-s", "1"]);
    exercise(&path, Kind::Fat32, &["fsck.vfat", "-n"]);
}

#[test]
fn fat32_bigger_clusters_and_two_fats() {
    if !tool("mkfs.vfat") {
        return;
    }
    let path = scratch("fat32b.img");
    let _cleanup = Cleanup(path.clone());
    make_image(&path, 600, &["mkfs.vfat", "-F", "32", "-s", "8", "-f", "2"]);
    exercise(&path, Kind::Fat32, &["fsck.vfat", "-n"]);
}

#[test]
fn exfat_small_clusters() {
    if !tool("mkfs.exfat") {
        return;
    }
    let path = scratch("exfat.img");
    let _cleanup = Cleanup(path.clone());
    make_image(&path, 128, &["mkfs.exfat", "-c", "4096"]);
    exercise(&path, Kind::ExFat, &["fsck.exfat", "-n"]);
}

#[test]
fn exfat_large_clusters() {
    if !tool("mkfs.exfat") {
        return;
    }
    let path = scratch("exfat2.img");
    let _cleanup = Cleanup(path.clone());
    make_image(&path, 512, &["mkfs.exfat", "-c", "131072"]);
    exercise(&path, Kind::ExFat, &["fsck.exfat", "-n"]);
}

#[test]
fn a_file_over_4_gib_on_fat32_is_capped() {
    if !tool("mkfs.vfat") {
        return;
    }
    // 6 GiB sparse image; asking for as much as possible gives at most 4 GiB - 1.
    let path = scratch("fat32c.img");
    let _cleanup = Cleanup(path.clone());
    make_image(&path, 6144, &["mkfs.vfat", "-F", "32", "-s", "8"]);
    let mut fs = open(&path);
    let stream = fs.create(b"big.wav", 0).unwrap();
    // Whole clusters, and never past what the 32-bit size field holds.
    assert!(stream.capacity() <= 0xffff_ffff && stream.capacity() > 0xffff_ffff - 32768);
    fs.finish(&stream, b"big.wav", 0).unwrap();
}

#[test]
fn a_file_over_4_gib_on_exfat_is_not() {
    if !tool("mkfs.exfat") {
        return;
    }
    let path = scratch("exfat3.img");
    let _cleanup = Cleanup(path.clone());
    make_image(&path, 6144, &["mkfs.exfat", "-c", "32768"]);
    let mut fs = open(&path);
    let stream = fs.create(b"big.wav", 0).unwrap();
    assert!(stream.capacity() > 5 << 30);
    // Write a block far into the file, as a long take would, and a length past 4 GiB.
    let data = pattern(0, 4096);
    fs.write(&stream, 5 << 30, &data, (5 << 30) + 4096).unwrap();
    fs.finish(&stream, b"big.wav", (5 << 30) + 4096).unwrap();
    drop(fs);
    fsck(&path, &["fsck.exfat", "-n"]);
    let mut fs = open(&path);
    let file = fs.find(b"big.wav").unwrap().unwrap();
    assert_eq!(file.size, (5 << 30) + 4096);
    let mut check = vec![0u8; 4096];
    fs.read(&file, 5 << 30, &mut check).unwrap();
    assert!(check == data);
}

#[test]
fn exfat_free_space_follows_the_files() {
    if !tool("mkfs.exfat") {
        return;
    }
    let path = scratch("exfat4.img");
    let _cleanup = Cleanup(path.clone());
    make_image(&path, 128, &["mkfs.exfat", "-c", "4096"]);
    let mut fs = open(&path);
    let (before, longest) = fs.free_space().unwrap();
    assert!(before > 100 << 20 && longest <= before && longest > 100 << 20, "{before} {longest}");
    record(&mut fs, b"a.wav", 0, 5 << 20, 262_144); // takes the biggest run, then gives back what it did not use
    let (after, _) = fs.free_space().unwrap();
    assert!(before - after >= 5 << 20 && before - after < (5 << 20) + (64 << 10), "{before} {after}");
    assert!(fs.remove(b"a.wav").unwrap());
    assert_eq!(fs.free_space().unwrap().0, before);
}

#[test]
fn fat32_free_space_is_not_offered() {
    if !tool("mkfs.vfat") {
        return;
    }
    let path = scratch("fat32d.img");
    let _cleanup = Cleanup(path.clone());
    make_image(&path, 128, &["mkfs.vfat", "-F", "32", "-s", "1"]);
    assert_eq!(open(&path).free_space(), Err(Error::Unsupported));
}

#[test]
fn formatted_exfat_volumes_pass_fsck_and_work() {
    if !tool("fsck.exfat") {
        return;
    }
    // Small (4 KiB clusters), medium (32 KiB) and one that needs a multi-cluster bitmap's worth of FAT.
    for (megabytes, name) in [(64u64, "fmt64.img"), (600, "fmt600.img"), (3000, "fmt3000.img")] {
        let path = scratch(name);
        let _cleanup = Cleanup(path.clone());
        let file = File::create(&path).unwrap();
        file.set_len(megabytes << 20).unwrap();
        // Old data that a format must not trust: a stale FAT/bitmap area.
        let mut device = FileDevice(OpenOptions::new().read(true).write(true).open(&path).unwrap());
        device.write(128, &[0xa5u8; 4096]).unwrap();
        let clusters = fat::format_exfat(&mut device, megabytes << 11, 2048, b"FOS TEST", 0x1234_5678, Time::DEFAULT).unwrap();
        assert!(clusters > 1000);
        drop(device);
        fsck(&path, &["fsck.exfat", "-n"]);
        let mut fs = open(&path);
        assert_eq!(fs.kind(), Kind::ExFat);
        let (free, longest) = fs.free_space().unwrap();
        assert!(free > (megabytes << 20) * 9 / 10 && free == longest, "{free} {longest}");
        record(&mut fs, b"take001.wav", 0, 3_000_017, 262_144);
        verify_read(&mut fs, b"take001.wav", 3_000_017);
        drop(fs);
        fsck(&path, &["fsck.exfat", "-n"]);
    }
}

/// Writes `data` as `name` in one go (a few hundred KiB at most: the loader).
fn put(fs: &mut Fs<FileDevice>, name: &[u8], data: &[u8]) {
    let stream = fs.create(name, data.len() as u64).unwrap();
    fs.write(&stream, 0, data, data.len() as u64).unwrap();
    fs.finish(&stream, name, data.len() as u64).unwrap();
}

/// Turns the root entry `name` (11 bytes, space padded) from a file into an empty directory: the crate cannot make
/// directories, and `mkfs.vfat` makes none, so the test makes one by hand (with its "." and ".." entries, as `fsck` expects).
fn make_directory(path: &Path, name: &[u8; 11]) {
    let mut file = OpenOptions::new().read(true).write(true).open(path).unwrap();
    let mut image = vec![0u8; 8 << 20];
    file.read_exact(&mut image).unwrap();
    let at = (0..image.len() - 32).step_by(32).find(|&at| &image[at..at + 11] == name && image[at + 11] == 0x20).expect("the entry");
    image[at + 11] = 0x10; // directory
    image[at + 28..at + 32].fill(0); // a directory's size field is 0
    let cluster = (u32::from(u16::from_le_bytes([image[at + 20], image[at + 21]])) << 16) | u32::from(u16::from_le_bytes([image[at + 26], image[at + 27]]));
    let spc = u64::from(image[13]);
    let data_lba = u64::from(u16::from_le_bytes([image[14], image[15]])) + u64::from(image[16]) * u64::from(u32::from_le_bytes([image[36], image[37], image[38], image[39]]));
    let directory_lba = data_lba + u64::from(cluster - 2) * spc;
    let mut sector = [0u8; 512];
    for (slot, (dots, target)) in [(&b".          ", cluster), (&b"..         ", 0u32)].into_iter().enumerate() {
        let entry = &mut sector[slot * 32..slot * 32 + 32];
        entry[..11].copy_from_slice(*dots);
        entry[11] = 0x10;
        entry[20..22].copy_from_slice(&((target >> 16) as u16).to_le_bytes());
        entry[26..28].copy_from_slice(&(target as u16).to_le_bytes());
    }
    file.seek(SeekFrom::Start((at - at % 512) as u64)).unwrap();
    file.write_all(&image[at - at % 512..at - at % 512 + 512]).unwrap();
    file.seek(SeekFrom::Start(directory_lba * 512)).unwrap();
    file.write_all(&sector).unwrap();
}

/// A FAT32 volume with `\EFI\BOOT\BOOTX64.EFI`, the loader of an EFI System Partition: the directory is entered, a new file
/// written beside the old one and swapped in with `replace`; the volume must stay valid (`fsck.vfat`, 7-Zip) and the old
/// file's clusters must be free again.
#[test]
fn replacing_a_file_in_a_subdirectory_keeps_the_volume_valid() {
    if !tool("mkfs.vfat") {
        return;
    }
    let path = scratch("replace.img");
    let _cleanup = Cleanup(path.clone());
    make_image(&path, 128, &["mkfs.vfat", "-F", "32", "-s", "1"]);
    {
        let mut fs = open(&path);
        let cluster = fs.cluster_bytes() as usize;
        // A directory made from a one-cluster file with zeroed contents.
        put(&mut fs, b"efi", &vec![0u8; cluster]);
        drop(fs);
        make_directory(&path, b"EFI        ");
    }
    let mut fs = open(&path);
    fs.set_time(Time { year: 2026, month: 10, day: 9, hour: 9, minute: 0, second: 0 });
    fs.enter(b"EFI").unwrap();
    assert!(fs.entry(b"efi").unwrap().is_none(), "the directory's own name is not inside it");
    let old = pattern(0, 100_000);
    let new = pattern(7, 180_000); // bigger than the old one: it needs a new, longer run of clusters
    put(&mut fs, b"bootx64.efi", &old);
    put(&mut fs, b"bootx64.new", &new);
    // The root and the subdirectory are separate places.
    fs.enter_root();
    assert!(fs.find(b"bootx64.efi").unwrap().is_none());
    fs.enter(b"EFI").unwrap();

    fs.replace(b"bootx64.efi", b"bootx64.new").unwrap();
    assert!(fs.find(b"bootx64.new").unwrap().is_none(), "the temporary name is gone");
    let file = fs.find(b"BOOTX64.EFI").unwrap().expect("the loader is still there");
    assert_eq!(file.size, 180_000);
    let mut data = vec![0u8; 180_000];
    assert_eq!(fs.read(&file, 0, &mut data).unwrap(), 180_000);
    assert!(data == new, "the loader holds the new contents");
    // Nothing of the old file is left allocated: a new file of the old file's size fits where it was, and the volume checks out.
    put(&mut fs, b"again.bin", &old);
    drop(fs);
    fsck(&path, &["fsck.vfat", "-n"]);
    let listing = Command::new("7z").args(["l"]).arg(&path).output().map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default();
    // 7-Zip lists the names as stored (lower case, from the lower-case names the files were created with).
    if let Some(bytes) = seven_zip(&path, "efi/bootx64.efi") {
        assert!(bytes.len() == new.len() && bytes == new, "7-Zip reads the new loader (got {} bytes, wanted {}, first difference at {:?})\n{listing}", bytes.len(), new.len(), bytes.iter().zip(new.iter()).position(|(a, b)| a != b));
    }
    // A cut after the switch (two names, one chain) is finished by `forget_alias`.
    let mut fs = open(&path);
    fs.enter(b"EFI").unwrap();
    put(&mut fs, b"bootx64.new", &new);
    let first = fs.entry(b"bootx64.new").unwrap().unwrap().first_cluster;
    assert_ne!(first, fs.entry(b"bootx64.efi").unwrap().unwrap().first_cluster);
    assert!(!fs.forget_alias(b"bootx64.new", b"bootx64.efi").unwrap(), "different chains: nothing to forget");
    assert!(fs.find(b"bootx64.new").unwrap().is_some());
}

/// exFAT subdirectories: made, nested, filled until they grow past a cluster (the parent's recorded size grows with them),
/// listed, read, emptied and removed; `fsck.exfat` agrees at every stage, and a fresh mount sees the same tree.
#[test]
fn exfat_subdirectories() {
    if !tool("mkfs.exfat") {
        return;
    }
    let path = scratch("exfat-dirs.img");
    let _cleanup = Cleanup(path.clone());
    make_image(&path, 64, &["mkfs.exfat", "-c", "4096"]);
    let mut fs = open(&path);
    fs.set_time(Time { year: 2026, month: 10, day: 30, hour: 12, minute: 0, second: 0 });
    fs.mkdir(b"rec").unwrap();
    assert_eq!(fs.mkdir(b"REC"), Err(Error::Exists));
    assert!(fs.create(b"rec", 4096).is_err(), "a file cannot replace a directory");
    put(&mut fs, b"top.bin", &pattern(0, 700));
    fs.enter(b"rec").unwrap();
    fs.mkdir(b"session1").unwrap();
    // 4 KiB clusters hold 128 entries; each name below needs 3 or 4, so 60 files force the directory to grow.
    for number in 0..60 {
        let name = format!("take number {number:03} of the long evening.wav");
        put(&mut fs, name.as_bytes(), &pattern(number, 1000 + number));
    }
    fs.enter(b"session1").unwrap();
    put(&mut fs, b"lp1.flac", &pattern(7, 5000));
    fs.enter_root();
    assert!(fs.find(b"lp1.flac").unwrap().is_none() && fs.find(b"top.bin").unwrap().is_some());
    drop(fs);
    fsck(&path, &["fsck.exfat", "-n"]);

    let mut fs = open(&path);
    fs.enter(b"rec").unwrap();
    let mut names = 0;
    let mut subdirectories = 0;
    fs.list(|entry| if entry.is_directory { subdirectories += 1 } else { names += 1 }).unwrap();
    assert_eq!((names, subdirectories), (60, 1));
    let file = fs.find(b"take number 059 of the long evening.wav").unwrap().unwrap();
    let mut data = vec![0u8; file.size as usize];
    fs.read(&file, 0, &mut data).unwrap();
    assert!(data == pattern(59, 1059));
    fs.enter(b"session1").unwrap();
    let file = fs.find(b"LP1.FLAC").unwrap().unwrap();
    let mut data = vec![0u8; 5000];
    fs.read(&file, 0, &mut data).unwrap();
    assert!(data == pattern(7, 5000));
    fs.enter_root();

    // A directory with anything in it stays; an emptied one goes and its space comes back.
    fs.enter(b"rec").unwrap();
    assert_eq!(fs.remove_dir(b"session1"), Err(Error::NotEmpty));
    fs.enter(b"session1").unwrap();
    assert!(fs.remove(b"lp1.flac").unwrap());
    fs.enter_root();
    fs.enter(b"rec").unwrap();
    assert!(fs.remove_dir(b"session1").unwrap());
    assert!(!fs.remove_dir(b"session1").unwrap());
    fs.enter_root();
    assert_eq!(fs.remove_dir(b"rec"), Err(Error::NotEmpty));
    drop(fs);
    fsck(&path, &["fsck.exfat", "-n"]);
}
