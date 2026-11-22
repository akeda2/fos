use crate::cache::Cache;
use crate::{Disk, SECTOR};

const SECTORS: usize = 64;

/// A RAM disk that counts the commands it gets and can be told to fail writes.
struct Counting {
    data: [u8; SECTOR * SECTORS],
    reads: u32,
    writes: u32,
    flushes: u32,
    fail_writes: bool,
}

impl Counting {
    fn new() -> Counting {
        let mut data = [0u8; SECTOR * SECTORS];
        for (at, byte) in data.iter_mut().enumerate() {
            *byte = (at % 253) as u8;
        }
        Counting { data, reads: 0, writes: 0, flushes: 0, fail_writes: false }
    }
    fn sector(&self, lba: usize) -> &[u8] {
        &self.data[lba * SECTOR..(lba + 1) * SECTOR]
    }
}

impl Disk for Counting {
    fn sectors(&self) -> u64 {
        SECTORS as u64
    }
    fn read(&mut self, lba: u64, buffer: &mut [u8]) -> bool {
        self.reads += 1;
        let at = lba as usize * SECTOR;
        buffer.copy_from_slice(&self.data[at..at + buffer.len()]);
        true
    }
    fn write(&mut self, lba: u64, data: &[u8]) -> bool {
        self.writes += 1;
        if self.fail_writes {
            return false;
        }
        let at = lba as usize * SECTOR;
        self.data[at..at + data.len()].copy_from_slice(data);
        true
    }
    fn flush(&mut self) -> bool {
        self.flushes += 1;
        true
    }
}

fn pattern(seed: u8, sectors: usize) -> [u8; SECTOR * 8] {
    let mut out = [0u8; SECTOR * 8];
    for (at, byte) in out[..sectors * SECTOR].iter_mut().enumerate() {
        *byte = seed.wrapping_add(at as u8).wrapping_mul(7);
    }
    out
}

#[test]
fn sequential_reads_become_few_large_disk_reads() {
    let mut disk = Counting::new();
    let expected = disk.data;
    let (mut read, mut gather) = ([0u8; SECTOR * 16], [0u8; SECTOR * 8]);
    let mut cache = Cache::new(&mut read, &mut gather);
    let mut buffer = [0u8; SECTOR * 2];
    for lba in (0..SECTORS as u64).step_by(2) {
        assert!(cache.on(&mut disk).read(lba, &mut buffer));
        assert_eq!(&buffer[..], &expected[lba as usize * SECTOR..(lba as usize + 2) * SECTOR], "lba {lba}");
    }
    // 32 two-sector reads: the first goes direct, then fills of 16 sectors from lba 2: 2-17, 18-33, 34-49, 50-63 (clipped).
    assert_eq!(disk.reads, 5);
    assert_eq!(cache.stats.read_fills, 4);
    assert_eq!(cache.stats.read_hits, 27);
}

#[test]
fn random_reads_go_straight_to_the_disk() {
    let mut disk = Counting::new();
    let (mut read, mut gather) = ([0u8; SECTOR * 16], [0u8; SECTOR * 8]);
    let mut cache = Cache::new(&mut read, &mut gather);
    let mut buffer = [0u8; SECTOR];
    for lba in [40u64, 3, 17, 60, 8] {
        assert!(cache.on(&mut disk).read(lba, &mut buffer));
        assert_eq!(&buffer[..], disk.sector(lba as usize));
    }
    assert_eq!((disk.reads, cache.stats.read_fills), (5, 0), "one command each, no read-ahead");
}

#[test]
fn consecutive_writes_become_one_disk_write_at_settle() {
    let mut disk = Counting::new();
    let (mut read, mut gather) = ([0u8; SECTOR * 16], [0u8; SECTOR * 8]);
    let mut cache = Cache::new(&mut read, &mut gather);
    let data = pattern(1, 8);
    {
        let mut cached = cache.on(&mut disk);
        for part in 0..4 {
            assert!(cached.write(10 + part * 2, &data[part as usize * 2 * SECTOR..(part as usize + 1) * 2 * SECTOR]));
        }
    }
    assert_eq!(disk.writes, 0, "nothing reaches the disk before the run is settled");
    assert_eq!(cache.pending(), 8);
    assert!(cache.on(&mut disk).settle());
    assert_eq!(disk.writes, 1, "four frames, one command");
    assert_eq!(&disk.data[10 * SECTOR..18 * SECTOR], &data[..]);
    assert_eq!(cache.pending(), 0);
}

#[test]
fn a_full_run_or_a_jump_writes_the_run_out_in_order() {
    let mut disk = Counting::new();
    let (mut read, mut gather) = ([0u8; SECTOR * 16], [0u8; SECTOR * 4]);
    let mut cache = Cache::new(&mut read, &mut gather);
    let (first, second) = (pattern(3, 2), pattern(9, 2));
    let mut cached = cache.on(&mut disk);
    assert!(cached.write(20, &first[..2 * SECTOR]));
    // The same sectors again: not a continuation, so the first run goes out first and the second wins.
    assert!(cached.write(20, &second[..2 * SECTOR]));
    assert!(cached.write(22, &second[..2 * SECTOR]));
    // The run (4 sectors) is full: the next write starts a new one.
    assert!(cached.write(24, &first[..2 * SECTOR]));
    assert!(cached.settle());
    assert_eq!(disk.writes, 3);
    assert_eq!(&disk.data[20 * SECTOR..22 * SECTOR], &second[..2 * SECTOR]);
    assert_eq!(&disk.data[24 * SECTOR..26 * SECTOR], &first[..2 * SECTOR]);
}

#[test]
fn a_read_of_gathered_sectors_sees_the_new_data() {
    let mut disk = Counting::new();
    let (mut read, mut gather) = ([0u8; SECTOR * 16], [0u8; SECTOR * 8]);
    let mut cache = Cache::new(&mut read, &mut gather);
    let data = pattern(5, 2);
    let mut cached = cache.on(&mut disk);
    assert!(cached.write(30, &data[..2 * SECTOR]));
    let mut buffer = [0u8; SECTOR];
    assert!(cached.read(31, &mut buffer));
    assert_eq!(&buffer[..], &data[SECTOR..2 * SECTOR]);
    assert_eq!(cache.pending(), 0, "the overlapping read wrote the run out first");
}

#[test]
fn the_read_buffer_follows_writes() {
    let mut disk = Counting::new();
    let (mut read, mut gather) = ([0u8; SECTOR * 16], [0u8; SECTOR * 8]);
    let mut cache = Cache::new(&mut read, &mut gather);
    let mut buffer = [0u8; SECTOR * 2];
    // Two sequential reads: the second fills the read buffer with sectors 2-17.
    assert!(cache.on(&mut disk).read(0, &mut buffer));
    assert!(cache.on(&mut disk).read(2, &mut buffer));
    let data = pattern(11, 2);
    assert!(cache.on(&mut disk).write(6, &data[..2 * SECTOR]));
    assert!(cache.on(&mut disk).settle());
    let reads = disk.reads;
    assert!(cache.on(&mut disk).read(4, &mut buffer));
    assert!(cache.on(&mut disk).read(6, &mut buffer));
    assert_eq!(&buffer[..], &data[..2 * SECTOR], "a hit returns the written data");
    assert_eq!(disk.reads, reads, "and both were hits");
}

#[test]
fn invalidate_lets_changes_by_others_through() {
    let mut disk = Counting::new();
    let (mut read, mut gather) = ([0u8; SECTOR * 16], [0u8; SECTOR * 8]);
    let mut cache = Cache::new(&mut read, &mut gather);
    let mut buffer = [0u8; SECTOR];
    assert!(cache.on(&mut disk).read(0, &mut buffer));
    assert!(cache.on(&mut disk).read(1, &mut buffer));
    disk.data[2 * SECTOR] = 0xee; // written behind the cache's back
    cache.invalidate();
    assert!(cache.on(&mut disk).read(2, &mut buffer));
    assert_eq!(buffer[0], 0xee);
}

#[test]
fn a_failed_run_makes_the_next_flush_fail_once() {
    let mut disk = Counting::new();
    let (mut read, mut gather) = ([0u8; SECTOR * 16], [0u8; SECTOR * 8]);
    let mut cache = Cache::new(&mut read, &mut gather);
    let data = pattern(2, 1);
    assert!(cache.on(&mut disk).write(5, &data[..SECTOR]), "acknowledged into the run");
    disk.fail_writes = true;
    assert!(!cache.on(&mut disk).settle());
    disk.fail_writes = false;
    assert!(!cache.on(&mut disk).flush(), "the initiator hears of it at FLUSH");
    assert!(cache.on(&mut disk).flush(), "once");
    assert_eq!(cache.stats.write_errors, 1);
}

#[test]
fn a_write_larger_than_the_run_goes_straight_through() {
    let mut disk = Counting::new();
    let (mut read, mut gather) = ([0u8; SECTOR * 16], [0u8; SECTOR * 2]);
    let mut cache = Cache::new(&mut read, &mut gather);
    let data = pattern(4, 4);
    assert!(cache.on(&mut disk).write(40, &data[..4 * SECTOR]));
    assert_eq!((disk.writes, cache.pending()), (1, 0));
    assert_eq!(&disk.data[40 * SECTOR..44 * SECTOR], &data[..4 * SECTOR]);
}

#[test]
fn through_the_target_writes_and_reads_still_answer_correctly() {
    use crate::Target;
    let mut disk = Counting::new();
    let (mut read, mut gather) = ([0u8; SECTOR * 16], [0u8; SECTOR * 8]);
    let mut cache = Cache::new(&mut read, &mut gather);
    let mut target = Target::new(1, 1, [2, 0, 0, 0, 0, 1], 1500);
    target.read_only = false;
    let mut frame = [0u8; 2000];
    let mut out = [0u8; 2000];
    let header = |frame: &mut [u8; 2000], command: u8, lba: u64, count: u8, write: bool| {
        frame[..6].copy_from_slice(&[2, 0, 0, 0, 0, 1]);
        frame[6..12].copy_from_slice(&[2, 0, 0, 0, 0, 9]);
        frame[12..14].copy_from_slice(&crate::ETHERTYPE.to_be_bytes());
        frame[14] = 0x10;
        frame[16..18].copy_from_slice(&1u16.to_be_bytes());
        frame[18] = 1;
        frame[19] = 0;
        frame[24] = 0x40 | u8::from(write);
        frame[26] = count;
        frame[27] = command;
        frame[28..34].copy_from_slice(&lba.to_le_bytes()[..6]);
    };
    let data = pattern(8, 8);
    for part in 0..4u64 {
        header(&mut frame, 0x34, 8 + part * 2, 2, true);
        frame[36..36 + 2 * SECTOR].copy_from_slice(&data[part as usize * 2 * SECTOR..(part as usize + 1) * 2 * SECTOR]);
        assert_ne!(target.handle(&frame[..36 + 2 * SECTOR], &mut cache.on(&mut disk), &mut out), 0);
        assert_eq!(out[27], 0x50);
    }
    assert!(cache.on(&mut disk).settle());
    assert_eq!(disk.writes, 1);
    header(&mut frame, 0x24, 10, 2, false);
    let length = target.handle(&frame[..36], &mut cache.on(&mut disk), &mut out);
    assert_eq!(&out[36..length], &data[2 * SECTOR..4 * SECTOR]);
    header(&mut frame, 0xea, 0, 0, false);
    target.handle(&frame[..36], &mut cache.on(&mut disk), &mut out);
    assert_eq!((out[27], disk.flushes), (0x50, 1));
}
