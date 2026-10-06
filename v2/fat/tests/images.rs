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
