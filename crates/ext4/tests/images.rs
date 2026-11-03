//! The driver against real volumes: images made by `mkfs.ext4` / `mkfs.ext2` from a directory (`-d`), then every
//! file read back through `Fs` and compared with the original.

use ext4::{Device, Error, Fs, Inode, Mapping, SECTOR};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

struct FileDevice(File);

thread_local! {
    static READS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

impl Device for FileDevice {
    fn read(&mut self, lba: u64, buffer: &mut [u8]) -> Result<(), Error> {
        READS.with(|reads| reads.set(reads.get() + 1));
        self.0.seek(SeekFrom::Start(lba * SECTOR as u64)).map_err(|_| Error::Device)?;
        self.0.read_exact(buffer).map_err(|_| Error::Device)
    }
}

fn tool(name: &str) -> bool {
    Path::new("/usr/sbin").join(name).exists() || Command::new(name).arg("-V").output().is_ok()
}

struct Cleanup(PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn pattern(offset: usize, length: usize) -> Vec<u8> {
    (offset..offset + length).map(|n| (n.wrapping_mul(2654435761) >> 13) as u8).collect()
}

/// A tree with what ext4 stores differently: tiny and empty files, a multi-megabyte file, a sparse file, a file of
/// thousands of extents (deep extent tree), a folder of thousands of entries (indexed), nested folders, UTF-8 names.
fn populate(root: &Path) {
    fs::create_dir_all(root.join("docs/deeper/deepest")).unwrap();
    fs::write(root.join("empty"), b"").unwrap();
    fs::write(root.join("small.txt"), b"hello, ext4\n").unwrap();
    fs::write(root.join("docs/deeper/deepest/Räksmörgås 日本語.txt"), "hej på dig\n").unwrap();
    fs::write(root.join("big.bin"), pattern(0, 9 * 1024 * 1024 + 123)).unwrap();
    let mut sparse = File::create(root.join("sparse.bin")).unwrap();
    sparse.seek(SeekFrom::Start(5 << 20)).unwrap();
    sparse.write_all(&pattern(7, 5000)).unwrap();
    sparse.set_len(6 << 20).unwrap();
    // Every second 4 KiB block: one extent each, far more than an inode's four.
    let mut broken = File::create(root.join("fragmented.bin")).unwrap();
    for index in 0..3000usize {
        broken.seek(SeekFrom::Start(index as u64 * 8192)).unwrap();
        broken.write_all(&pattern(index, 4096)).unwrap();
    }
    fs::create_dir(root.join("many")).unwrap();
    for index in 0..3000 {
        fs::write(root.join(format!("many/file-{index:05}-with-a-longish-name.txt")), format!("number {index}\n")).unwrap();
    }
    for index in 0..60 {
        fs::write(root.join(format!("many/räksmörgås-{index:03}.txt")), "x").unwrap();
    }
    std::os::unix::fs::symlink("small.txt", root.join("link")).unwrap();
}

fn compare_tree(fs: &mut Fs<FileDevice>, folder: &Inode, path: &Path, count: &mut usize) {
    let mut names = Vec::new();
    fs.list(folder, &mut |entry| {
        if entry.name != b"." && entry.name != b".." && entry.name != b"lost+found" {
            names.push((entry.name.to_vec(), entry.inode, entry.kind));
        }
        false
    })
    .unwrap();
    let mut expected: Vec<Vec<u8>> = fs::read_dir(path).unwrap().map(|entry| entry.unwrap().file_name().into_encoded_bytes()).collect();
    expected.sort();
    let mut found: Vec<Vec<u8>> = names.iter().map(|(name, _, _)| name.clone()).collect();
    found.sort();
    assert_eq!(found, expected, "listing of {path:?}");
    for (name, number, _) in names {
        let inode = fs.inode(number).unwrap();
        let child = path.join(std::ffi::OsStr::new(unsafe { std::str::from_utf8_unchecked(&name) }));
        let again = fs.lookup(folder, &name).unwrap().expect("lookup finds what list showed");
        assert_eq!(again.number, number);
        *count += 1;
        if inode.is_dir() {
            compare_tree(fs, &inode, &child, count);
        } else if inode.is_symlink() {
            assert_eq!(inode.fast_target().unwrap(), fs::read_link(&child).unwrap().as_os_str().as_encoded_bytes());
        } else {
            let want = fs::read(&child).unwrap();
            assert_eq!(inode.size, want.len() as u64, "size of {child:?}");
            let mut got = vec![0u8; want.len()];
            // Odd chunk sizes: unaligned heads and tails, and runs of whole blocks.
            let mut done = 0;
            let mut chunk = 1;
            while done < got.len() {
                let take = chunk.min(got.len() - done);
                assert_eq!(fs.read(&inode, done as u64, &mut got[done..done + take]).unwrap(), take);
                done += take;
                chunk = chunk * 3 + 1;
                if chunk > 300_000 {
                    chunk = 1;
                }
            }
            assert!(got == want, "content of {child:?}");
            let mut whole = vec![0u8; want.len()];
            assert_eq!(fs.read(&inode, 0, &mut whole).unwrap(), want.len());
            assert!(whole == want, "whole read of {child:?}");
        }
    }
}

fn run(mkfs: &[&str], name: &str) {
    if !tool(mkfs[0]) {
        return;
    }
    let dir = std::env::temp_dir().join(format!("fos-ext4-{}-{name}", std::process::id()));
    let _cleanup = Cleanup(dir.clone());
    let source = dir.join("src");
    fs::create_dir_all(&source).unwrap();
    populate(&source);
    let image = dir.join("image");
    File::create(&image).unwrap().set_len(96 << 20).unwrap();
    let made = Command::new(mkfs[0]).args(&mkfs[1..]).arg("-q").arg("-F").arg("-d").arg(&source).arg(&image).output().unwrap();
    assert!(made.status.success(), "{mkfs:?}: {}", String::from_utf8_lossy(&made.stderr));

    // `mkfs -d` writes linear folders; `e2fsck -D` optimises them into hash trees (exit 1 = fixed something).
    let optimised = Command::new("/usr/sbin/e2fsck").args(["-fyD"]).arg(&image).output().unwrap();
    assert!(optimised.status.code().is_some_and(|code| code <= 1), "{}", String::from_utf8_lossy(&optimised.stdout));

    let mut volume = Fs::mount(FileDevice(File::open(&image).unwrap())).unwrap();
    assert_eq!(volume.label(), b"fostest");
    let root = volume.root().unwrap();
    assert!(root.is_dir());
    let mut count = 0;
    compare_tree(&mut volume, &root, &source, &mut count);
    assert!(count > 3000, "{count} entries compared");
    // The big folder is indexed (mkfs makes a hash tree for it): a lookup reads the tree and one leaf, not the folder.
    let many = volume.resolve(b"many").unwrap().unwrap();
    assert!(many.flags & 0x1000 != 0, "the big folder has an htree index");
    let before = READS.with(|reads| reads.get());
    for index in (0..3000).step_by(7) {
        let name = format!("file-{index:05}-with-a-longish-name.txt");
        let entry = volume.lookup(&many, name.as_bytes()).unwrap().expect("indexed lookup finds the name");
        assert!(volume.inode(entry.number).is_ok());
    }
    for index in 0..60 {
        let name = format!("räksmörgås-{index:03}.txt");
        assert!(volume.lookup(&many, name.as_bytes()).unwrap().is_some(), "{name}");
    }
    assert!(volume.lookup(&many, b"file-99999-nothere").unwrap().is_none());
    assert!(volume.lookup(&many, b"a").unwrap().is_none());
    let reads = READS.with(|reads| reads.get()) - before;
    eprintln!("indexed lookups: {reads} reads");
    assert!(reads < 429 * 8, "{reads} device reads for 429 indexed lookups (a linear scan needs ~40 each)");
    // Resuming at a position `list_from` gave continues the same walk, one pass for the whole folder.
    let mut all = Vec::new();
    volume.list(&many, &mut |entry| { all.push(entry.name.to_vec()); false }).unwrap();
    let mut resumed = Vec::new();
    let mut position = 0;
    loop {
        let mut next = None;
        let mut taken = 0;
        volume.list_from(&many, position, &mut |entry, after| {
            resumed.push(entry.name.to_vec());
            taken += 1;
            next = Some(after);
            taken == 100
        }).unwrap();
        match next {
            Some(after) if taken == 100 => position = after,
            _ => break,
        }
    }
    assert_eq!(all, resumed);
    assert!(all.len() > 3000);
    use std::os::unix::fs::MetadataExt;
    let small = volume.resolve(b"small.txt").unwrap().unwrap();
    let owner = fs::metadata(source.join("small.txt")).unwrap();
    assert_eq!((small.uid, small.gid), (owner.uid(), owner.gid()));
    let found = volume.resolve("docs//deeper/deepest/Räksmörgås 日本語.txt".as_bytes()).unwrap().unwrap();
    assert_eq!(found.size, "hej på dig\n".len() as u64);
    assert!(volume.resolve(b"docs/nothere").unwrap().is_none());
    assert!(volume.resolve(b"small.txt/x").unwrap().is_none());
    // A hole reads as one run; a stored file is mapped in runs.
    let sparse = volume.resolve(b"sparse.bin").unwrap().unwrap();
    assert!(matches!(volume.map(&sparse, 0).unwrap(), Mapping::Hole { blocks } if blocks >= 1));
    let (total, free) = volume.space();
    assert!(total > 90 << 20 && free < total);
}

#[test]
fn ext4_4k_blocks() {
    run(&["mkfs.ext4", "-b", "4096", "-L", "fostest"], "4k");
}

#[test]
fn ext4_1k_blocks_64bit() {
    run(&["mkfs.ext4", "-b", "1024", "-O", "64bit", "-L", "fostest"], "1k");
}

#[test]
fn ext2_indirect_blocks() {
    run(&["mkfs.ext2", "-b", "2048", "-L", "fostest"], "ext2");
}

#[test]
fn not_ext_is_refused() {
    let dir = std::env::temp_dir().join(format!("fos-ext4-{}-blank", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let _cleanup = Cleanup(dir.clone());
    let image = dir.join("blank");
    File::create(&image).unwrap().set_len(1 << 20).unwrap();
    assert!(matches!(Fs::mount(FileDevice(File::open(&image).unwrap())), Err(Error::NotExt)));
}
