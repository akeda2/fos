//! Writer tests: format with the crate, change things, and let e2fsck and debugfs judge the result.

use ext4::{Device, Error, FormatOptions, Fs, Writer};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::process::Command;

struct FileDevice(File);

impl Device for FileDevice {
    fn read(&mut self, lba: u64, buffer: &mut [u8]) -> Result<(), Error> {
        self.0.seek(SeekFrom::Start(lba * 512)).map_err(|_| Error::Device)?;
        self.0.read_exact(buffer).map_err(|_| Error::Device)
    }
    fn write(&mut self, lba: u64, buffer: &[u8]) -> Result<(), Error> {
        self.0.seek(SeekFrom::Start(lba * 512)).map_err(|_| Error::Device)?;
        self.0.write_all(buffer).map_err(|_| Error::Device)
    }
}

fn pattern(seed: usize, length: usize) -> Vec<u8> {
    (seed..seed + length).map(|n| (n.wrapping_mul(2654435761) >> 11) as u8).collect()
}

fn have(name: &str) -> bool {
    std::path::Path::new("/usr/sbin").join(name).exists()
}

fn fsck(image: &PathBuf) {
    let out = Command::new("/usr/sbin/e2fsck").arg("-fn").arg(image).output().unwrap();
    assert!(out.status.success(), "e2fsck: {}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
}

fn debugfs_cat(image: &PathBuf, path: &str) -> Vec<u8> {
    Command::new("/usr/sbin/debugfs").arg("-R").arg(format!("cat \"{path}\"")).arg(image).output().unwrap().stdout
}

fn make(name: &str, mb: u64) -> (PathBuf, Writer<FileDevice>) {
    let image = std::env::temp_dir().join(format!("fos-ext4-{name}-{}.img", std::process::id()));
    let file = OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&image).unwrap();
    file.set_len(mb << 20).unwrap();
    let options = FormatOptions { label: b"fostest", bytes_per_inode: 0, uuid: [7; 16], time: 1_700_000_000 };
    let writer = Writer::format(FileDevice(file), (mb << 20) / 512, &options).unwrap();
    (image, writer)
}

#[test]
fn formatted_volume_is_clean_and_usable() {
    if !have("e2fsck") {
        return;
    }
    let (image, mut w) = make("format", 200);
    fsck(&image);
    assert_eq!(w.label(), b"fostest");
    let root = w.root().unwrap().number;
    assert!({ let r = w.root().unwrap(); w.lookup(&r, b"lost+found") }.unwrap().is_some());
    let file = w.create(root, b"hello.txt", 0o644).unwrap();
    assert_eq!(w.write(file.number, 0, b"hello").unwrap(), 5);
    assert!(matches!(w.create(root, b"hello.txt", 0o644), Err(Error::Exists)));
    assert!(matches!(w.create(root, b"a/b", 0o644), Err(Error::InvalidName)));
    w.flush().unwrap();
    fsck(&image);
    assert_eq!(debugfs_cat(&image, "/hello.txt"), b"hello");
    let _ = fs::remove_file(&image);
}

#[test]
fn files_grow_shrink_and_leave_holes() {
    if !have("e2fsck") {
        return;
    }
    let (image, mut w) = make("files", 200);
    let root = w.root().unwrap().number;
    let big = pattern(1, 9 << 20);
    let f = w.create(root, b"big.bin", 0o644).unwrap().number;
    for (i, chunk) in big.chunks(100_003).enumerate() {
        w.write(f, (i * 100_003) as u64, chunk).unwrap();
    }
    w.flush().unwrap();
    fsck(&image);
    assert_eq!(debugfs_cat(&image, "/big.bin"), big);

    // Overwrite across block edges, then a hole, then shrink inside a block.
    let patch = pattern(99, 10_000);
    w.write(f, 4090, &patch).unwrap();
    let mut expected = big.clone();
    expected[4090..14_090].copy_from_slice(&patch);
    w.write(f, (12 << 20) + 5, b"tail").unwrap();
    expected.resize((12 << 20) + 5, 0);
    expected.extend_from_slice(b"tail");
    w.flush().unwrap();
    fsck(&image);
    assert_eq!(debugfs_cat(&image, "/big.bin"), expected);
    let free_before = w.space().1;
    w.truncate(f, 5000).unwrap();
    expected.truncate(5000);
    w.flush().unwrap();
    fsck(&image);
    assert_eq!(debugfs_cat(&image, "/big.bin"), expected);
    assert!(w.space().1 > free_before + 2000);
    w.truncate(f, 0).unwrap();
    w.remove(root, b"big.bin").unwrap();
    w.flush().unwrap();
    fsck(&image);
    let _ = fs::remove_file(&image);
}

#[test]
fn many_extents_use_leaf_blocks() {
    if !have("e2fsck") {
        return;
    }
    let (image, mut w) = make("extents", 200);
    let root = w.root().unwrap().number;
    let f = w.create(root, b"frag.bin", 0o644).unwrap().number;
    // Every other block: each write is its own extent, forcing a depth-1 tree.
    let blocks = 1000u64;
    for n in 0..blocks {
        w.write(f, n * 8192, &pattern(n as usize, 4096)).unwrap();
    }
    w.flush().unwrap();
    fsck(&image);
    let node = w.inode(f).unwrap();
    let mut out = vec![0u8; 4096];
    for n in [0u64, 1, 499, 999] {
        assert_eq!(w.read(&node, n * 8192, &mut out).unwrap(), 4096);
        assert_eq!(out, pattern(n as usize, 4096));
    }
    w.truncate(f, 100 * 8192).unwrap();
    w.flush().unwrap();
    fsck(&image);
    w.remove(root, b"frag.bin").unwrap();
    w.flush().unwrap();
    fsck(&image);
    let _ = fs::remove_file(&image);
}

#[test]
fn folders_rename_and_remove() {
    if !have("e2fsck") {
        return;
    }
    let (image, mut w) = make("folders", 200);
    let root = w.root().unwrap().number;
    let a = w.mkdir(root, b"a", 0o755).unwrap().number;
    let b = w.mkdir(root, b"b", 0o755).unwrap().number;
    let inner = w.mkdir(a, b"inner", 0o755).unwrap().number;
    // Enough names to need several folder blocks.
    for n in 0..400 {
        let name = format!("file-with-a-longish-name-{n:04}-åäö");
        let f = w.create(inner, name.as_bytes(), 0o644).unwrap().number;
        w.write(f, 0, name.as_bytes()).unwrap();
    }
    w.flush().unwrap();
    fsck(&image);
    assert_eq!(debugfs_cat(&image, "/a/inner/file-with-a-longish-name-0123-åäö"), "file-with-a-longish-name-0123-åäö".as_bytes());

    w.rename(a, b"inner", b, b"moved").unwrap();
    assert!(matches!(w.rename(root, b"b", a, b"b-in-a"), Ok(())));
    w.rename(a, b"b-in-a", root, b"b").unwrap();
    w.flush().unwrap();
    fsck(&image);
    let _ = fs::remove_file(&image);
}

#[test]
fn rename_into_itself_is_refused_and_remove_checks() {
    if !have("e2fsck") {
        return;
    }
    let (image, mut w) = make("rename", 200);
    let root = w.root().unwrap().number;
    let a = w.mkdir(root, b"a", 0o755).unwrap().number;
    let c = w.mkdir(a, b"c", 0o755).unwrap().number;
    assert!(matches!(w.rename(root, b"a", c, b"x"), Err(Error::Invalid)));
    w.create(c, b"f", 0o644).unwrap();
    assert!(matches!(w.remove(root, b"a"), Err(Error::NotEmpty)));
    w.rename(c, b"f", root, b"f2").unwrap();
    w.rename(root, b"a", root, b"a2").unwrap();
    let a2 = { let r = w.root().unwrap(); w.lookup(&r, b"a2") }.unwrap().unwrap();
    w.rename(a2.number, b"c", root, b"c2").unwrap();
    w.remove(root, b"a2").unwrap();
    w.remove(root, b"c2").unwrap();
    w.flush().unwrap();
    fsck(&image);
    assert!({ let r = w.root().unwrap(); w.lookup(&r, b"f2") }.unwrap().is_some());
    let _ = fs::remove_file(&image);
}

#[test]
fn a_full_volume_says_so_and_stays_consistent() {
    if !have("e2fsck") {
        return;
    }
    let (image, mut w) = make("full", 40);
    let root = w.root().unwrap().number;
    let f = w.create(root, b"fill", 0o644).unwrap().number;
    let chunk = vec![0xabu8; 1 << 20];
    let mut failed = false;
    for n in 0..60u64 {
        match w.write(f, n << 20, &chunk) {
            Ok(_) => {}
            Err(Error::NoSpace) => {
                failed = true;
                break;
            }
            Err(e) => panic!("{e:?}"),
        }
    }
    assert!(failed);
    w.flush().unwrap();
    fsck(&image);
    w.remove(root, b"fill").unwrap();
    w.flush().unwrap();
    fsck(&image);
    let _ = fs::remove_file(&image);
}

#[test]
fn checksummed_linux_volumes_stay_read_only() {
    if !have("mkfs.ext4") {
        return;
    }
    let image = std::env::temp_dir().join(format!("fos-ext4-csum-{}.img", std::process::id()));
    File::create(&image).unwrap().set_len(20 << 20).unwrap();
    assert!(Command::new("/usr/sbin/mkfs.ext4").args(["-q", "-F"]).arg(&image).status().unwrap().success());
    let fs = Fs::mount(FileDevice(File::open(&image).unwrap())).unwrap();
    assert!(matches!(Writer::new(fs), Err(Error::ReadOnly)));
    let _ = fs::remove_file(&image);
}
