//! Device-operation counts for typical workloads (a stand-in for time: every call is a disk command on real hardware).

use ext4::{Cache, Cached, Device, Error, FormatOptions, Journaled, Stage, Writer};
use std::cell::Cell;
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::rc::Rc;

#[derive(Default)]
struct Counts {
    reads: Cell<u64>,
    writes: Cell<u64>,
}

struct CountingDevice {
    file: std::fs::File,
    counts: Rc<Counts>,
}

impl Device for CountingDevice {
    fn read(&mut self, lba: u64, buffer: &mut [u8]) -> Result<(), Error> {
        self.counts.reads.set(self.counts.reads.get() + 1);
        self.file.seek(SeekFrom::Start(lba * 512)).map_err(|_| Error::Device)?;
        self.file.read_exact(buffer).map_err(|_| Error::Device)
    }
    fn write(&mut self, lba: u64, buffer: &[u8]) -> Result<(), Error> {
        self.counts.writes.set(self.counts.writes.get() + 1);
        self.file.seek(SeekFrom::Start(lba * 512)).map_err(|_| Error::Device)?;
        self.file.write_all(buffer).map_err(|_| Error::Device)
    }
}

#[test]
fn operation_counts() {
    let image = std::env::temp_dir().join(format!("fos-ext4-perf-{}.img", std::process::id()));
    let file = OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&image).unwrap();
    file.set_len(512 << 20).unwrap();
    let counts = Rc::new(Counts::default());
    let options = FormatOptions { label: b"perf", bytes_per_inode: 0, uuid: [1; 16], time: 1_700_000_000, journal: true, checksums: true };
    let stage: &'static mut Stage = Box::leak(Box::new(Stage::new()));
    let device = CountingDevice { file, counts: counts.clone() };
    let slots: usize = std::env::var("SLOTS").ok().and_then(|v| v.parse().ok()).unwrap_or(1);
    let cache: &'static mut Cache = Box::leak(Box::new(Cache::new(
        Box::leak(vec![0u64; slots].into_boxed_slice()),
        Box::leak(vec![0u8; slots * 4096].into_boxed_slice()),
    )));
    let mut w = Writer::format(Cached::new(device, cache), (512 << 20) / 512, &options, stage).unwrap();
    w.set_time(1_700_000_100);
    let report = |what: &str| {
        eprintln!("{what:40} reads {:6} writes {:6}", counts.reads.get(), counts.writes.get());
        counts.reads.set(0);
        counts.writes.set(0);
    };
    report("format");
    let root = w.root().unwrap().number;
    let f = w.create(root, b"big.bin", 0o644).unwrap().number;
    let chunk = vec![0x5au8; 1 << 20];
    for n in 0..64u64 {
        w.write(f, n << 20, &chunk).unwrap();
    }
    w.flush().unwrap();
    report("write 64 MiB in 1 MiB chunks");
    let inode = w.root().unwrap();
    let big = w.lookup(&inode, b"big.bin").unwrap().unwrap();
    report("lookup in root");
    let mut out = vec![0u8; 1 << 20];
    for n in 0..64u64 {
        w.read(&big, n << 20, &mut out).unwrap();
    }
    report("read 64 MiB in 1 MiB chunks");
    for n in 0..16384u64 {
        w.read(&big, n * 4096, &mut out[..4096]).unwrap();
    }
    report("read 64 MiB in 4 KiB chunks");
    let d = w.mkdir(root, b"many", 0o755).unwrap().number;
    for n in 0..2000 {
        w.create(d, format!("entry-with-a-fairly-long-name-{n:05}").as_bytes(), 0o644).unwrap();
    }
    w.flush().unwrap();
    report("mkdir + 2000 creates");
    let dir = w.lookup(&inode, b"many").unwrap().unwrap();
    report("lookup many");
    for n in (0..2000).step_by(100) {
        w.lookup(&dir, format!("entry-with-a-fairly-long-name-{n:05}").as_bytes()).unwrap().unwrap();
    }
    report("20 lookups in a 2000-entry folder");
    let mut names = 0;
    w.list(&dir, &mut |_| { names += 1; false }).unwrap();
    report("list 2000 entries");
    let small = w.create(root, b"s.txt", 0o644).unwrap().number;
    for n in 0..100u64 {
        w.write(small, n * 100, &[1u8; 100]).unwrap();
    }
    w.flush().unwrap();
    report("100 small appends");
    let _ = std::fs::remove_file(&image);
    let _: Option<Journaled<CountingDevice>> = None;
}
