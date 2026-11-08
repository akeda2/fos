//! Assembles a RAID 1 from files or block devices and prints what `mdadm --examine` would say, then checks that every
//! sector of the array reads the same from every member in use (read-only).
//!
//!     cargo run --example probe -- /dev/sdb1 /dev/sdc1 [--dump FILE]
//!
//! `--dump` writes the array's contents to FILE, which `fsck.ext4 -n` or `mount -o ro,loop` can then check.

use md::{Error, Members, SECTOR, raid1::Array};
use std::fs::File;
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
}

fn main() {
    let mut paths = Vec::new();
    let mut dump = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--dump" {
            dump = args.next();
        } else {
            paths.push(arg);
        }
    }
    let mut disks = Disks(Vec::new());
    for path in &paths {
        let mut file = File::open(path).unwrap_or_else(|e| panic!("{path}: {e}"));
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
        for &member in used.iter().skip(1) {
            if let Some(offset) = array_offset(&mut disks, member) {
                disks.read(member, offset + lba, b).expect("read mirror");
                mismatches += (0..count).filter(|&s| a[s * SECTOR..(s + 1) * SECTOR] != b[s * SECTOR..(s + 1) * SECTOR]).count() as u64;
            }
        }
        lba += count as u64;
    }
    println!("{mismatches} sectors differ between mirrors");
}

fn array_offset(disks: &mut Disks, member: usize) -> Option<u64> {
    md::raid1::find_superblock(disks, member).map(|(sb, _)| sb.data_offset)
}
