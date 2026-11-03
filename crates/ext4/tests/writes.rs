//! Writer tests: format with the crate, change things, and let e2fsck and debugfs judge the result.

use ext4::{Cache, Cached, Device, Error, FormatOptions, Fs, Journaled, Stage, Writer};
use std::cell::Cell;
use std::rc::Rc;
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

fn stage() -> &'static mut Stage {
    Box::leak(Box::new(Stage::new()))
}

fn make(name: &str, mb: u64) -> (PathBuf, Writer<Journaled<FileDevice>>) {
    let image = std::env::temp_dir().join(format!("fos-ext4-{name}-{}.img", std::process::id()));
    let file = OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&image).unwrap();
    file.set_len(mb << 20).unwrap();
    let options = FormatOptions { label: b"fostest", bytes_per_inode: 0, uuid: [7; 16], time: 1_700_000_000, journal: true, checksums: true };
    let writer = Writer::format(FileDevice(file), (mb << 20) / 512, &options, stage()).unwrap();
    (image, writer)
}

#[test]
fn formatted_volume_is_clean_and_usable() {
    if !have("e2fsck") {
        return;
    }
    let (image, mut w) = make("format", 200);
    fsck(&image);
    let dump = Command::new("/usr/sbin/dumpe2fs").arg("-h").arg(&image).output().unwrap();
    assert!(String::from_utf8_lossy(&dump.stdout).contains("metadata_csum"));
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

/// A volume made by Linux's `mkfs.ext4` (`extra` are its options), filled by this writer, judged by e2fsck.
fn linux_volume_workload(name: &str, mb: u64, extra: &[&str]) {
    if !have("mkfs.ext4") || !have("e2fsck") {
        return;
    }
    let image = std::env::temp_dir().join(format!("fos-ext4-{name}-{}.img", std::process::id()));
    File::create(&image).unwrap().set_len(mb << 20).unwrap();
    let made = Command::new("/usr/sbin/mkfs.ext4").args(["-q", "-F"]).args(extra).arg(&image).status().unwrap();
    assert!(made.success());
    fsck(&image);
    let file = OpenOptions::new().read(true).write(true).open(&image).unwrap();
    let mut w = Writer::new(ext4::open(FileDevice(file), stage()).unwrap()).unwrap();
    w.set_time(1_800_000_000);
    let root = w.root().unwrap().number;
    let dir = w.mkdir(root, b"docs", 0o755).unwrap().number;
    let mut expected = Vec::new();
    // A folder that grows to several blocks, and files of several shapes.
    for n in 0..300 {
        let name = format!("file-with-a-longish-name-{n:04}.txt");
        let f = w.create(dir, name.as_bytes(), 0o644).unwrap().number;
        let data = pattern(n, 1 + n * 37);
        w.write(f, 0, &data).unwrap();
        expected.push((name, data));
    }
    let big = pattern(5, 3 << 20);
    let f = w.create(root, b"big.bin", 0o644).unwrap().number;
    for (i, chunk) in big.chunks(65_537).enumerate() {
        w.write(f, (i * 65_537) as u64, chunk).unwrap();
    }
    // Fragmentation: every other block of a file, so extent leaves are needed.
    let frag = w.create(root, b"frag.bin", 0o644).unwrap().number;
    let mut frag_data = Vec::new();
    for n in (0..40).step_by(2) {
        let piece = pattern(n, 4096);
        w.write(frag, n as u64 * 4096, &piece).unwrap();
        frag_data.resize(n * 4096, 0);
        frag_data.extend_from_slice(&piece);
    }
    w.mkdir(dir, b"sub", 0o700).unwrap();
    w.rename(root, b"big.bin", dir, b"moved.bin").unwrap();
    w.rename(root, b"docs", root, b"papers").unwrap();
    for n in 0..100 {
        w.remove(dir, expected[n].0.as_bytes()).unwrap();
    }
    w.truncate(frag, 5000).unwrap();
    frag_data.truncate(5000);
    w.flush().unwrap();
    drop(w);
    fsck(&image);
    assert_eq!(debugfs_cat(&image, "/papers/moved.bin"), big);
    assert_eq!(debugfs_cat(&image, "/frag.bin"), frag_data);
    for (name, data) in expected.iter().skip(100).step_by(37) {
        assert_eq!(&debugfs_cat(&image, &format!("/papers/{name}")), data);
    }
    let _ = fs::remove_file(&image);
}

#[test]
fn linux_default_volume_is_writable() {
    linux_volume_workload("lx-default", 64, &[]);
}

#[test]
fn linux_volume_with_lazy_inode_tables() {
    linux_volume_workload("lx-lazy", 512, &["-E", "lazy_itable_init=1,lazy_journal_init=1"]);
}

#[test]
fn linux_volume_without_orphan_file_and_small_blocks() {
    linux_volume_workload("lx-1k", 64, &["-b", "1024", "-O", "^orphan_file"]);
}

#[test]
fn linux_volume_without_checksums_and_with_many_groups() {
    linux_volume_workload("lx-nocsum", 1024, &["-O", "^metadata_csum,^orphan_file", "-N", "20000"]);
}

#[test]
fn linux_volume_spills_into_uninitialised_groups() {
    if !have("mkfs.ext4") || !have("e2fsck") {
        return;
    }
    let image = std::env::temp_dir().join(format!("fos-ext4-spill-{}.img", std::process::id()));
    File::create(&image).unwrap().set_len(768 << 20).unwrap();
    let args = ["-q", "-F", "-b", "4096", "-O", "^orphan_file", "-E", "lazy_itable_init=1"];
    assert!(Command::new("/usr/sbin/mkfs.ext4").args(args).arg(&image).status().unwrap().success());
    let dump = Command::new("/usr/sbin/dumpe2fs").arg(&image).output().unwrap();
    assert!(String::from_utf8_lossy(&dump.stdout).contains("BLOCK_UNINIT"));
    let file = OpenOptions::new().read(true).write(true).open(&image).unwrap();
    let mut w = Writer::new(ext4::open(FileDevice(file), stage()).unwrap()).unwrap();
    let root = w.root().unwrap().number;
    let big = pattern(3, 1 << 20);
    let f = w.create(root, b"huge.bin", 0o644).unwrap().number;
    for n in 0..300u64 {
        w.write(f, n << 20, &big).unwrap();
    }
    for n in 0..40 {
        w.mkdir(root, format!("d{n}").as_bytes(), 0o755).unwrap();
    }
    w.flush().unwrap();
    drop(w);
    fsck(&image);
    assert_eq!(debugfs_cat(&image, "/huge.bin").len(), 300 << 20);
    let _ = fs::remove_file(&image);
}

#[test]
fn linux_volume_with_gdt_checksums_only_stays_read_only() {
    if !have("mkfs.ext4") {
        return;
    }
    let image = std::env::temp_dir().join(format!("fos-ext4-uninit-{}.img", std::process::id()));
    File::create(&image).unwrap().set_len(20 << 20).unwrap();
    assert!(Command::new("/usr/sbin/mkfs.ext4").args(["-q", "-F", "-O", "^metadata_csum,uninit_bg"]).arg(&image).status().unwrap().success());
    let fs = Fs::mount(FileDevice(File::open(&image).unwrap())).unwrap();
    assert!(matches!(Writer::new(fs), Err(Error::ReadOnly)));
    let _ = fs::remove_file(&image);
}

/// A disk that dies after `budget` more writes (a power cut).
struct CrashDevice {
    file: FileDevice,
    budget: Rc<Cell<u64>>,
}

impl Device for CrashDevice {
    fn read(&mut self, lba: u64, buffer: &mut [u8]) -> Result<(), Error> {
        self.file.read(lba, buffer)
    }
    fn write(&mut self, lba: u64, buffer: &[u8]) -> Result<(), Error> {
        if self.budget.get() == 0 {
            return Err(Error::Device);
        }
        self.budget.set(self.budget.get() - 1);
        self.file.write(lba, buffer)
    }
}

fn script<D: Device>(w: &mut Writer<Journaled<D>>) -> Result<(), Error> {
    w.set_time(1_700_000_100);
    let root = w.root().unwrap().number;
    let dir = w.mkdir(root, b"docs", 0o755)?.number;
    for n in 0..40 {
        let f = w.create(dir, format!("file{n}.txt").as_bytes(), 0o644)?.number;
        w.write(f, 0, &pattern(n, 5000 + n * 100))?;
    }
    let f = w.create(root, b"big.bin", 0o644)?.number;
    w.write(f, 0, &pattern(9, 3 << 20))?;
    w.rename(dir, b"file3.txt", root, b"moved.txt")?;
    w.remove(dir, b"file4.txt")?;
    w.truncate(f, 1000)?;
    w.flush()
}

#[test]
fn a_power_cut_at_any_write_leaves_a_consistent_volume() {
    if !have("e2fsck") {
        return;
    }
    let (base, mut w) = make("crash-base", 64);
    w.flush().unwrap();
    drop(w);
    // How many writes the whole script takes.
    let count = {
        let copy = std::env::temp_dir().join(format!("fos-ext4-crash-count-{}.img", std::process::id()));
        fs::copy(&base, &copy).unwrap();
        let budget = Rc::new(Cell::new(u64::MAX));
        let file = OpenOptions::new().read(true).write(true).open(&copy).unwrap();
        let device = CrashDevice { file: FileDevice(file), budget: budget.clone() };
        let mut w = Writer::new(ext4::open(device, stage()).unwrap()).unwrap();
        script(&mut w).unwrap();
        let _ = fs::remove_file(&copy);
        u64::MAX - budget.get()
    };
    assert!(count > 100, "{count}");
    let step = (count / 150).max(1);
    let mut k = 0;
    while k < count {
        let copy = std::env::temp_dir().join(format!("fos-ext4-crash-{}-{k}.img", std::process::id()));
        fs::copy(&base, &copy).unwrap();
        {
            let file = OpenOptions::new().read(true).write(true).open(&copy).unwrap();
            let device = CrashDevice { file: FileDevice(file), budget: Rc::new(Cell::new(k)) };
            let mut w = Writer::new(ext4::open(device, stage()).unwrap()).unwrap();
            let _ = script(&mut w);
        }
        // Linux's own replay first, on a copy, then ours.
        let linux = copy.with_extension("linux");
        fs::copy(&copy, &linux).unwrap();
        let out = Command::new("/usr/sbin/e2fsck").arg("-fy").arg(&linux).output().unwrap();
        assert!(out.status.code().unwrap() <= 1, "crash {k}: e2fsck -fy: {}", String::from_utf8_lossy(&out.stdout));
        fsck(&linux);
        {
            let file = OpenOptions::new().read(true).write(true).open(&copy).unwrap();
            let mut w = Writer::new(ext4::open(FileDevice(file), stage()).unwrap()).unwrap();
            w.flush().unwrap();
        }
        let out = Command::new("/usr/sbin/e2fsck").arg("-fn").arg(&copy).output().unwrap();
        if !out.status.success() {
            eprintln!("kept {} and {}", copy.display(), linux.display());
        }
        assert!(out.status.success(), "crash {k}: {}", String::from_utf8_lossy(&out.stdout));
        let _ = fs::remove_file(&copy);
        let _ = fs::remove_file(&linux);
        k += step;
    }
    let _ = fs::remove_file(&base);
}

#[test]
fn a_journal_linux_wrote_is_replayed() {
    if !have("debugfs") {
        return;
    }
    let (image, mut w) = make("linux-journal", 64);
    let root = w.root().unwrap().number;
    let f = w.create(root, b"hello.txt", 0o644).unwrap().number;
    w.write(f, 0, &pattern(1, 4096)).unwrap();
    w.flush().unwrap();
    drop(w);
    let out = Command::new("/usr/sbin/debugfs").arg("-R").arg("bmap /hello.txt 0").arg(&image).output().unwrap();
    let block: u64 = String::from_utf8_lossy(&out.stdout).trim().parse().unwrap();
    let new = pattern(77, 4096);
    let payload = image.with_extension("payload");
    fs::write(&payload, &new).unwrap();
    let commands = image.with_extension("cmds");
    fs::write(&commands, format!("jo\njw -b {block} {}\njc\n", payload.display())).unwrap();
    let out = Command::new("/usr/sbin/debugfs").arg("-w").arg("-f").arg(&commands).arg(&image).output().unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    if text.contains("Unknown request") || text.contains("not supported") || text.contains("rror") {
        eprintln!("debugfs cannot write journals here, skipping: {text}");
        return;
    }
    assert_eq!(debugfs_cat(&image, "/hello.txt"), pattern(1, 4096), "the transaction is not checkpointed yet");
    let file = OpenOptions::new().read(true).write(true).open(&image).unwrap();
    let mut w = Writer::new(ext4::open(FileDevice(file), stage()).unwrap()).unwrap();
    w.flush().unwrap();
    drop(w);
    assert_eq!(debugfs_cat(&image, "/hello.txt"), new);
    fsck(&image);
    let _ = (fs::remove_file(&image), fs::remove_file(&payload), fs::remove_file(&commands));
}

#[test]
fn a_checksummed_journal_linux_wrote_is_replayed() {
    if !have("debugfs") || !have("mkfs.ext4") || !have("e2fsck") {
        return;
    }
    let image = std::env::temp_dir().join(format!("fos-ext4-csum-journal-{}.img", std::process::id()));
    File::create(&image).unwrap().set_len(64 << 20).unwrap();
    assert!(Command::new("/usr/sbin/mkfs.ext4").args(["-q", "-F", "-O", "^orphan_file"]).arg(&image).status().unwrap().success());
    let file = OpenOptions::new().read(true).write(true).open(&image).unwrap();
    let mut w = Writer::new(ext4::open(FileDevice(file), stage()).unwrap()).unwrap();
    let root = w.root().unwrap().number;
    let f = w.create(root, b"hello.txt", 0o644).unwrap().number;
    w.write(f, 0, &pattern(1, 4096)).unwrap();
    w.flush().unwrap();
    drop(w);
    let out = Command::new("/usr/sbin/debugfs").arg("-R").arg("bmap /hello.txt 0").arg(&image).output().unwrap();
    let block: u64 = String::from_utf8_lossy(&out.stdout).trim().parse().unwrap();
    let new = pattern(78, 4096);
    let payload = image.with_extension("payload");
    fs::write(&payload, &new).unwrap();
    let commands = image.with_extension("cmds");
    fs::write(&commands, format!("jo -c\njw -b {block} {}\njc\n", payload.display())).unwrap();
    let out = Command::new("/usr/sbin/debugfs").arg("-w").arg("-f").arg(&commands).arg(&image).output().unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    if text.contains("Unknown request") || text.contains("not supported") || text.contains("rror") {
        eprintln!("debugfs cannot write journals here, skipping: {text}");
        return;
    }
    let dump = Command::new("/usr/sbin/dumpe2fs").arg("-h").arg(&image).output().unwrap();
    assert!(String::from_utf8_lossy(&dump.stdout).contains("journal_checksum_v3"), "the test needs a v3 journal");
    assert_eq!(debugfs_cat(&image, "/hello.txt"), pattern(1, 4096), "the transaction is not checkpointed yet");
    let file = OpenOptions::new().read(true).write(true).open(&image).unwrap();
    let mut w = Writer::new(ext4::open(FileDevice(file), stage()).unwrap()).unwrap();
    w.flush().unwrap();
    drop(w);
    assert_eq!(debugfs_cat(&image, "/hello.txt"), new);
    fsck(&image);
    // An empty v3 log (what a clean Linux unmount leaves) is accepted too, and our writes then work.
    let file = OpenOptions::new().read(true).write(true).open(&image).unwrap();
    let mut w = Writer::new(ext4::open(FileDevice(file), stage()).unwrap()).unwrap();
    w.create(root, b"after.txt", 0o644).unwrap();
    w.flush().unwrap();
    drop(w);
    fsck(&image);
    let _ = (fs::remove_file(&image), fs::remove_file(&payload), fs::remove_file(&commands));
}

#[test]
fn hash_tree_folders_are_made_linear_on_a_checksummed_volume() {
    if !have("mkfs.ext4") || !have("e2fsck") || !have("debugfs") {
        return;
    }
    let image = std::env::temp_dir().join(format!("fos-ext4-htree-{}.img", std::process::id()));
    File::create(&image).unwrap().set_len(64 << 20).unwrap();
    assert!(Command::new("/usr/sbin/mkfs.ext4").args(["-q", "-F", "-O", "^orphan_file"]).arg(&image).status().unwrap().success());
    let file = OpenOptions::new().read(true).write(true).open(&image).unwrap();
    let mut w = Writer::new(ext4::open(FileDevice(file), stage()).unwrap()).unwrap();
    let root = w.root().unwrap().number;
    let mut dirs = Vec::new();
    for (name, count) in [("small", 150), ("big", 6000)] {
        let d = w.mkdir(root, name.as_bytes(), 0o755).unwrap().number;
        for n in 0..count {
            w.create(d, format!("entry-with-a-fairly-long-name-{n:05}").as_bytes(), 0o644).unwrap();
        }
        dirs.push(d);
    }
    w.flush().unwrap();
    drop(w);
    // e2fsck -D rebuilds the folders as hash trees (one level for `small`, two for `big`).
    Command::new("/usr/sbin/e2fsck").args(["-fyD"]).arg(&image).output().unwrap();
    fsck(&image);
    let stat = |path: &str| String::from_utf8_lossy(&Command::new("/usr/sbin/debugfs").arg("-R").arg(format!("htree_dump {path}")).arg(&image).output().unwrap().stdout).to_string();
    assert!(stat("/small").contains("Number of entries"), "small is a hash tree now");
    assert!(stat("/big").contains("Number of entries"), "big is a hash tree now");
    let file = OpenOptions::new().read(true).write(true).open(&image).unwrap();
    let mut w = Writer::new(ext4::open(FileDevice(file), stage()).unwrap()).unwrap();
    w.set_time(1_800_000_000);
    w.create(dirs[0], b"added.txt", 0o644).unwrap();
    w.remove(dirs[0], b"entry-with-a-fairly-long-name-00007").unwrap();
    w.rename(root, b"big", dirs[0], b"moved-big").unwrap();
    w.create(dirs[1], b"added.txt", 0o644).unwrap();
    w.remove(dirs[1], b"entry-with-a-fairly-long-name-00042").unwrap();
    w.flush().unwrap();
    drop(w);
    fsck(&image);
    assert!(!stat("/small").contains("Number of entries"), "the folder is linear after our change");
    let _ = fs::remove_file(&image);
}

#[test]
fn format_without_checksums_still_works() {
    if !have("e2fsck") {
        return;
    }
    let image = std::env::temp_dir().join(format!("fos-ext4-plain-{}.img", std::process::id()));
    let file = OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&image).unwrap();
    file.set_len(100 << 20).unwrap();
    let options = FormatOptions { label: b"plain", bytes_per_inode: 0, uuid: [9; 16], time: 1_700_000_000, journal: true, checksums: false };
    let mut w = Writer::format(FileDevice(file), (100 << 20) / 512, &options, stage()).unwrap();
    let root = w.root().unwrap().number;
    w.set_time(1_700_000_100);
    let f = w.create(root, b"a.txt", 0o644).unwrap().number;
    w.write(f, 0, b"plain").unwrap();
    w.flush().unwrap();
    drop(w);
    fsck(&image);
    assert_eq!(debugfs_cat(&image, "/a.txt"), b"plain");
    let dump = Command::new("/usr/sbin/dumpe2fs").arg("-h").arg(&image).output().unwrap();
    assert!(!String::from_utf8_lossy(&dump.stdout).contains("metadata_csum"));
    let _ = fs::remove_file(&image);
}

#[test]
fn a_read_cache_under_the_journal_stays_coherent() {
    if !have("e2fsck") {
        return;
    }
    let image = std::env::temp_dir().join(format!("fos-ext4-cache-{}.img", std::process::id()));
    let file = OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&image).unwrap();
    file.set_len(200 << 20).unwrap();
    let options = FormatOptions { label: b"fostest", bytes_per_inode: 0, uuid: [7; 16], time: 1_700_000_000, journal: true, checksums: true };
    // Few slots, so that many units collide and get replaced.
    let cache: &'static mut Cache = Box::leak(Box::new(Cache::new(Box::leak(vec![0u64; 16].into_boxed_slice()), Box::leak(vec![0u8; 16 * 4096].into_boxed_slice()))));
    let mut w = Writer::format(Cached::new(FileDevice(file), cache), (200 << 20) / 512, &options, stage()).unwrap();
    script(&mut w).unwrap();
    drop(w);
    fsck(&image);
    assert_eq!(debugfs_cat(&image, "/big.bin"), pattern(9, 1000));
    let _ = fs::remove_file(&image);
}
