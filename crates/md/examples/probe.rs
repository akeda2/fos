//! Assembles a RAID 1 from files or block devices and prints what `mdadm --examine` would say, then checks that every
//! sector of the array reads the same from every member in use (read-only).
//!
//!     cargo run --example probe -- /dev/sdb1 /dev/sdc1 [--dump FILE]
//!
//! `--dump` writes the array's contents to FILE, which `fsck.ext4 -n` or `mount -o ro,loop` can then check.
//!
//!     cargo run --example probe -- --write-test /dev/sdb1 /dev/sdc1
//!
//! `--write-test` opens the members read-write, starts the array, writes a pattern to its last 8 sectors, reads it
//! back, restores the original bytes and stops cleanly. Afterwards `mdadm --examine` should show the array clean with
//! the event count two higher, and `mdadm --assemble --scan` must still work. Use test disks, or an array whose last
//! sectors are free (ext4 puts nothing user-visible there, but never run it on a mounted array).
//!
//!     cargo run --example probe -- --rebuild N DISK1 DISK2 ...
//!
//! `--rebuild N` (members read-write) fills member number N (0-based, as listed; it may be blank) in as the array's
//! missing member, from the others, and stops the array clean. Mirrors (RAID 1) are compared; striped levels are not.

use md::{Error, Members, SECTOR, array::Array};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};

struct Disks(Vec<(File, u64)>);

impl Members for Disks {
    fn count(&self) -> usize {
        self.0.len()
    }
    fn sectors(&self, member: usize) -> u64 {
        self.0[member].1
    }
    fn read(&mut self, member: usize, lba: u64, buffer: &mut [u8]) -> Result<(), Error> {
        let file = &mut self.0[member].0;
        file.seek(SeekFrom::Start(lba * SECTOR as u64)).map_err(|_| Error::Device)?;
        file.read_exact(buffer).map_err(|_| Error::Device)
    }
    fn write(&mut self, member: usize, lba: u64, buffer: &[u8]) -> Result<(), Error> {
        let file = &mut self.0[member].0;
        file.seek(SeekFrom::Start(lba * SECTOR as u64)).map_err(|_| Error::Device)?;
        file.write_all(buffer).map_err(|_| Error::Device)
    }
    fn flush(&mut self, member: usize) -> Result<(), Error> {
        self.0[member].0.sync_all().map_err(|_| Error::Device)
    }
}

fn main() {
    let mut paths = Vec::new();
    let mut dump = None;
    let mut write_test = false;
    let mut rebuild: Option<usize> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--write-test" {
            write_test = true;
        } else if arg == "--rebuild" {
            rebuild = args.next().and_then(|n| n.parse().ok());
        } else if arg == "--dump" {
            dump = args.next();
        } else {
            paths.push(arg);
        }
    }
    let mut disks = Disks(Vec::new());
    for path in &paths {
        let mut file = OpenOptions::new().read(true).write(write_test || rebuild.is_some()).open(path).unwrap_or_else(|e| panic!("{path}: {e}"));
        let bytes = file.seek(SeekFrom::End(0)).unwrap();
        disks.0.push((file, bytes / SECTOR as u64));
    }
    let array = match Array::assemble(&mut disks, None) {
        Ok(array) => array,
        Err(error) => return println!("not assembled: {error:?}"),
    };
    let uuid: Vec<String> = array.uuid().iter().map(|b| format!("{b:02x}")).collect();
    println!("name {:?}  uuid {}", array.name(), uuid.join(""));
    println!("{} sectors ({} MiB), events {}, members {} of {}{}{}", array.sectors(), array.sectors() / 2048, array.events(),
        array.present(), array.expected(), if array.degraded() { ", DEGRADED" } else { "" }, if array.dirty() { ", dirty" } else { "" });
    let used: Vec<usize> = array.used_members().collect();
    println!("in use: {}", used.iter().map(|&m| paths[m].as_str()).collect::<Vec<_>>().join(" "));
    if let Some(member) = rebuild {
        let mut array = array;
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        array.start(&mut disks, now).expect("start");
        array.begin_rebuild(&mut disks, member, [0x5a; 16]).expect("begin rebuild");
        loop {
            match array.step(&mut disks, 4096, now).expect("step") {
                md::array::Step::Running(_) => {}
                _ => break,
            }
        }
        array.stop(&mut disks, now).expect("stop");
        println!("rebuild ok: {} of {} members, events now {}, array stopped clean", array.present(), array.expected(), array.events());
        return;
    }
    if write_test {
        let mut array = array;
        let at = array.sectors() - 8;
        let (mut saved, mut check) = (vec![0u8; 8 * SECTOR], vec![0u8; 8 * SECTOR]);
        array.read(&mut disks, at, &mut saved).expect("read");
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        array.start(&mut disks, now).expect("start");
        let pattern: Vec<u8> = (0..8 * SECTOR).map(|i| (i * 7 + 13) as u8).collect();
        array.write(&mut disks, at, &pattern, now).expect("write");
        array.read(&mut disks, at, &mut check).expect("read back");
        assert_eq!(check, pattern, "read-back differs");
        array.write(&mut disks, at, &saved, now).expect("restore");
        array.stop(&mut disks, now).expect("stop");
        println!("write test ok: events now {}, array stopped clean", array.events());
        return;
    }
    let mut out = dump.map(|path| File::create(path).unwrap());
    let (mut buf, mut other) = (vec![0u8; 256 * SECTOR], vec![0u8; 256 * SECTOR]);
    let mut lba = 0;
    let mut mismatches = 0u64;
    while lba < array.sectors() {
        let count = ((array.sectors() - lba) as usize).min(256);
        let (a, b) = (&mut buf[..count * SECTOR], &mut other[..count * SECTOR]);
        array.read(&mut disks, lba, a).expect("read");
        if let Some(out) = out.as_mut() {
            out.write_all(a).unwrap();
        }
        // read each other member directly at the same array address, to compare the mirrors
        for &member in used.iter().skip(1).filter(|_| array.level() == 1) {
            if let Some(offset) = array_offset(&mut disks, member) {
                disks.read(member, offset + lba, b).expect("read mirror");
                mismatches += (0..count).filter(|&s| a[s * SECTOR..(s + 1) * SECTOR] != b[s * SECTOR..(s + 1) * SECTOR]).count() as u64;
            }
        }
        lba += count as u64;
    }
    if array.level() == 1 {
        println!("{mismatches} sectors differ between mirrors");
    }
}

fn array_offset(disks: &mut Disks, member: usize) -> Option<u64> {
    md::array::find_superblock(disks, member).map(|(sb, _)| sb.data_offset)
}
