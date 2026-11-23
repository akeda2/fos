use crate::cache::Cache;
use crate::filedisk::{FileDisk, Place, Run, covers, extend, zero_sectors};
use crate::{Disk, SECTOR, Target};

const SECTORS: usize = 64;

/// A RAM disk whose sector n is filled with byte n + 1.
struct Ram {
    data: [u8; SECTOR * SECTORS],
    writes: u32,
}

impl Ram {
    fn new() -> Ram {
        let mut data = [0u8; SECTOR * SECTORS];
        for (at, byte) in data.iter_mut().enumerate() {
            *byte = (at / SECTOR + 1) as u8;
        }
        Ram { data, writes: 0 }
    }
    fn sector(&self, lba: usize) -> &[u8] {
        &self.data[lba * SECTOR..(lba + 1) * SECTOR]
    }
}

impl Disk for Ram {
    fn sectors(&self) -> u64 {
        SECTORS as u64
    }
    fn read(&mut self, lba: u64, buffer: &mut [u8]) -> bool {
        let at = lba as usize * SECTOR;
        buffer.copy_from_slice(&self.data[at..at + buffer.len()]);
        true
    }
    fn write(&mut self, lba: u64, data: &[u8]) -> bool {
        self.writes += 1;
        let at = lba as usize * SECTOR;
        self.data[at..at + data.len()].copy_from_slice(data);
        true
    }
    fn flush(&mut self) -> bool {
        true
    }
}

/// File sectors 0-3 at disk 40, 4-5 zeros, 6-9 at disk 10.
const MAP: [Run; 3] = [
    Run { start: 0, sectors: 4, place: Place::Disk(40) },
    Run { start: 4, sectors: 2, place: Place::Zeros },
    Run { start: 6, sectors: 4, place: Place::Disk(10) },
];

fn first_bytes(buffer: &[u8]) -> Vec<u8> {
    buffer.chunks(SECTOR).map(|sector| sector[0]).collect()
}

extern crate std;
use std::vec::Vec;

#[test]
fn reads_follow_the_runs() {
    let mut ram = Ram::new();
    let mut file = FileDisk::new(&mut ram, &MAP, 10).unwrap();
    assert_eq!(file.sectors(), 10);
    let mut buffer = [0xffu8; SECTOR * 10];
    assert!(file.read(0, &mut buffer));
    // Disk sector n holds n + 1.
    assert_eq!(first_bytes(&buffer), [41, 42, 43, 44, 0, 0, 11, 12, 13, 14]);
    assert!(buffer[4 * SECTOR..6 * SECTOR].iter().all(|&byte| byte == 0));
    // A request inside one run, and one that starts in the middle of a run and crosses two boundaries.
    let mut two = [0u8; SECTOR * 2];
    assert!(file.read(7, &mut two));
    assert_eq!(first_bytes(&two), [12, 13]);
    let mut five = [0xffu8; SECTOR * 5];
    assert!(file.read(3, &mut five));
    assert_eq!(first_bytes(&five), [44, 0, 0, 11, 12]);
}

#[test]
fn requests_past_the_end_fail() {
    let mut ram = Ram::new();
    let mut file = FileDisk::new(&mut ram, &MAP, 10).unwrap();
    let mut two = [0u8; SECTOR * 2];
    assert!(!file.read(9, &mut two));
    assert!(!file.read(u64::MAX, &mut two));
    assert!(!file.write(9, &two));
    // Not whole sectors.
    assert!(!file.read(0, &mut two[..100]));
}

#[test]
fn the_size_can_end_inside_a_run() {
    // A file of 7.5 sectors is exported as 7: the runs (whole blocks) cover more than the export serves.
    let mut ram = Ram::new();
    let mut file = FileDisk::new(&mut ram, &MAP, 7).unwrap();
    let mut one = [0u8; SECTOR];
    assert!(file.read(6, &mut one));
    assert!(!file.read(7, &mut one));
}

#[test]
fn writes_land_in_the_runs_and_not_in_zeros() {
    let mut ram = Ram::new();
    {
        let mut file = FileDisk::new(&mut ram, &MAP, 10).unwrap();
        let data = [0xaau8; SECTOR * 2];
        // Sectors 2-3: disk 42-43.
        assert!(file.write(2, &data));
        // Sectors 3-4: 4 is a zero run, so nothing is written, not even sector 3.
        assert!(!file.write(3, &[0xbbu8; SECTOR * 2]));
        // Across 5-6 (zeros, then data): refused; 8-9 inside the last run.
        assert!(!file.write(5, &data));
        assert!(file.write(8, &data));
    }
    assert!(ram.sector(42).iter().all(|&byte| byte == 0xaa));
    assert!(ram.sector(43).iter().all(|&byte| byte == 0xaa));
    // File sectors 8-9: disk 12-13.
    assert!(ram.sector(12).iter().all(|&byte| byte == 0xaa));
    assert!(ram.sector(13).iter().all(|&byte| byte == 0xaa));
    // Nothing else changed: the neighbours of the runs, and the refused writes.
    for (lba, value) in [(39, 40), (40, 41), (41, 42), (44, 45), (11, 12), (14, 15)] {
        assert!(ram.sector(lba).iter().all(|&byte| byte == value), "sector {lba}");
    }
    assert_eq!(ram.writes, 2);
}

#[test]
fn a_write_across_runs_is_split() {
    let mut ram = Ram::new();
    let map = [Run { start: 0, sectors: 1, place: Place::Disk(30) }, Run { start: 1, sectors: 2, place: Place::Disk(5) }];
    {
        let mut file = FileDisk::new(&mut ram, &map, 3).unwrap();
        let mut data = [0u8; SECTOR * 3];
        for (index, sector) in data.chunks_mut(SECTOR).enumerate() {
            sector.fill(0xc0 + index as u8);
        }
        assert!(file.write(0, &data));
    }
    assert_eq!(ram.sector(30)[0], 0xc0);
    assert_eq!(ram.sector(5)[0], 0xc1);
    assert_eq!(ram.sector(6)[0], 0xc2);
    assert_eq!(ram.writes, 2);
}

#[test]
fn bad_maps_are_refused() {
    let mut ram = Ram::new();
    // A gap between runs, a run that starts late, an empty run, a run off the disk, too short for the size.
    let gap = [Run { start: 0, sectors: 2, place: Place::Disk(0) }, Run { start: 3, sectors: 2, place: Place::Disk(9) }];
    let late = [Run { start: 1, sectors: 2, place: Place::Disk(0) }];
    let empty = [Run { start: 0, sectors: 0, place: Place::Disk(0) }, Run { start: 0, sectors: 2, place: Place::Disk(0) }];
    let off = [Run { start: 0, sectors: 2, place: Place::Disk(SECTORS as u64 - 1) }];
    assert!(FileDisk::new(&mut ram, &gap, 4).is_none());
    assert!(FileDisk::new(&mut ram, &late, 2).is_none());
    assert!(FileDisk::new(&mut ram, &empty, 2).is_none());
    assert!(FileDisk::new(&mut ram, &off, 2).is_none());
    assert!(FileDisk::new(&mut ram, &MAP, 11).is_none());
    assert!(!covers(&[], 1));
    assert!(covers(&[], 0));
}

#[test]
fn runs_merge_when_they_continue() {
    let mut runs: Vec<Run> = Vec::new();
    let mut add = |runs: &mut Vec<Run>, sectors: u64, place: Place| {
        if !extend(runs.last_mut(), sectors, place) {
            let start = runs.last().map_or(0, |run| run.start + run.sectors);
            runs.push(Run { start, sectors, place });
        }
    };
    add(&mut runs, 8, Place::Disk(100));
    add(&mut runs, 8, Place::Disk(108)); // continues
    add(&mut runs, 8, Place::Disk(200)); // jumps
    add(&mut runs, 8, Place::Zeros);
    add(&mut runs, 8, Place::Zeros); // continues
    add(&mut runs, 8, Place::Disk(208)); // after zeros: new, even though it follows 200..208 on the disk
    assert_eq!(
        runs,
        [
            Run { start: 0, sectors: 16, place: Place::Disk(100) },
            Run { start: 16, sectors: 8, place: Place::Disk(200) },
            Run { start: 24, sectors: 16, place: Place::Zeros },
            Run { start: 40, sectors: 8, place: Place::Disk(208) },
        ]
    );
    assert!(covers(&runs, 48));
    assert_eq!(zero_sectors(&runs, 48), 16);
    assert_eq!(zero_sectors(&runs, 30), 6);
    assert_eq!(zero_sectors(&runs, 20), 0);
}

#[test]
fn many_runs_are_found_by_search() {
    // 32 one-sector runs in reverse disk order: file sector n at disk 63 - n.
    let runs: Vec<Run> = (0..32).map(|n| Run { start: n, sectors: 1, place: Place::Disk(63 - n) }).collect();
    let mut ram = Ram::new();
    let mut file = FileDisk::new(&mut ram, &runs, 32).unwrap();
    let mut buffer = [0u8; SECTOR * 32];
    assert!(file.read(0, &mut buffer));
    let expected: Vec<u8> = (0..32).map(|n| (63 - n + 1) as u8).collect();
    assert_eq!(first_bytes(&buffer), expected);
}

/// The whole path the kernel uses: a target answering frames, through the worker's cache, over a file disk.
#[test]
fn a_target_serves_a_file() {
    let mut ram = Ram::new();
    let mut file = FileDisk::new(&mut ram, &MAP, 10).unwrap();
    let (mut read_buffer, mut gather) = ([0u8; SECTOR * 8], [0u8; SECTOR * 8]);
    let mut cache = Cache::new(&mut read_buffer, &mut gather);
    let mut cached = cache.on(&mut file);
    let mut target = Target::new(7, 3, [2, 0, 0, 0, 0, 1], 1500);
    let mut frame = [0u8; 60];
    // A READ SECTORS EXT of 2 sectors from LBA 3 (the last sector of the first run and the first zero sector).
    frame[0..6].copy_from_slice(&[2, 0, 0, 0, 0, 1]);
    frame[12..14].copy_from_slice(&crate::ETHERTYPE.to_be_bytes());
    frame[14] = 0x10;
    frame[16..18].copy_from_slice(&7u16.to_be_bytes());
    frame[18] = 3;
    frame[19] = crate::COMMAND_ATA;
    frame[20..24].copy_from_slice(&0x1234u32.to_be_bytes());
    frame[24] = 0x40;
    frame[26] = 2;
    frame[27] = 0x24;
    frame[28] = 3;
    let mut reply = [0u8; 1600];
    let length = target.handle(&frame, &mut cached, &mut reply);
    assert_eq!(length, crate::DATA_AT + 2 * SECTOR);
    assert_eq!(reply[27], 0x50, "status OK");
    assert!(reply[crate::DATA_AT..crate::DATA_AT + SECTOR].iter().all(|&byte| byte == 44));
    assert!(reply[crate::DATA_AT + SECTOR..length].iter().all(|&byte| byte == 0));
    // IDENTIFY says 10 sectors.
    frame[27] = 0xec;
    frame[26] = 1;
    let length = target.handle(&frame, &mut cached, &mut reply);
    assert_eq!(length, crate::DATA_AT + SECTOR);
    let words = &reply[crate::DATA_AT..];
    assert_eq!(u64::from_le_bytes(words[200..208].try_into().unwrap()), 10);
}
