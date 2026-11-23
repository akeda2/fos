//! Read-ahead against this crate's own `Target`, over a simulated link whose replies wait in an inbox between reads (as
//! they wait in the kernel's reply queue while the caller is busy elsewhere).

extern crate std;

use super::initiator::*;
use super::readahead::*;
use super::*;
use std::vec::Vec;

const US: [u8; 6] = [2, 0, 0, 0, 0, 1];
const THEM: [u8; 6] = [2, 0, 0, 0, 0, 9];
const SECTORS: u64 = 4096;
const TO: Address = Address { ours: US, target: THEM, shelf: 5, slot: 2 };
/// 2 sectors a frame and a window of 8: chunks of 16 sectors.
const PER_FRAME: usize = 2;
const WINDOW: usize = 8;
/// Longer than any read here takes (a round is one time unit; a real read takes far less than the kernel's 10 ms tick).
const STALE: u64 = 20;
/// Above the slow target's queueing delay (a full queue of 8 served 2 a round), as the kernel's 20 ms is above a real
/// target's: frames are resent only when lost.
const TIMEOUT: u64 = 6;

struct Ram {
    data: Vec<u8>,
    reads: u64,
}

impl Disk for Ram {
    fn sectors(&self) -> u64 {
        SECTORS
    }
    fn read(&mut self, lba: u64, buffer: &mut [u8]) -> bool {
        self.reads += 1;
        let at = lba as usize * SECTOR;
        buffer.copy_from_slice(&self.data[at..at + buffer.len()]);
        true
    }
    fn write(&mut self, lba: u64, data: &[u8]) -> bool {
        let at = lba as usize * SECTOR;
        self.data[at..at + data.len()].copy_from_slice(data);
        true
    }
    fn flush(&mut self) -> bool {
        true
    }
}

/// What `World::pump` runs: a read with its read-ahead, a drain, or a plain transfer.
trait Work {
    fn next(&mut self, now: u64, out: &mut [u8]) -> usize;
    fn receive(&mut self, frame: &[u8]);
    fn finished(&self) -> Option<Result<(), Failure>>;
}

struct Ahead<'a> {
    ahead: &'a mut ReadAhead,
    buffer: &'a mut [u8],
    store: &'a mut [u8],
}

impl Work for Ahead<'_> {
    fn next(&mut self, now: u64, out: &mut [u8]) -> usize {
        self.ahead.next(now, &TO, out)
    }
    fn receive(&mut self, frame: &[u8]) {
        self.ahead.receive(&TO, &parse(frame).unwrap(), self.buffer, self.store);
    }
    fn finished(&self) -> Option<Result<(), Failure>> {
        self.ahead.finished()
    }
}

struct Single<'a> {
    transfer: Transfer,
    data: &'a mut [u8],
}

impl Work for Single<'_> {
    fn next(&mut self, now: u64, out: &mut [u8]) -> usize {
        self.transfer.next(now, &TO, self.data, out)
    }
    fn receive(&mut self, frame: &[u8]) {
        self.transfer.receive(&TO, &parse(frame).unwrap(), self.data);
    }
    fn finished(&self) -> Option<Result<(), Failure>> {
        self.transfer.finished()
    }
}

/// The target, its disk, its queue, the replies on their way back, and the clock.
struct World {
    target: Target,
    disk: Ram,
    /// Requests the target has not served yet: it holds `WINDOW` (its `bufcount`) and drops what does not fit, as the
    /// kernel's target does. It serves `serve_per_round` of them a round.
    queue: Vec<Vec<u8>>,
    serve_per_round: usize,
    overflowed: usize,
    inbox: Vec<Vec<u8>>,
    now: u64,
    tag: u32,
    /// Lose every `drop_reply`-th reply (0: never) and hand each round's replies back in reverse order.
    drop_reply: usize,
    reverse: bool,
    replies: usize,
}

impl World {
    fn new() -> World {
        let data = (0..SECTORS as usize * SECTOR).map(|at| (at % 251) as u8 ^ (at / 4096) as u8).collect();
        let mut target = Target::new(5, 2, THEM, 1500);
        target.read_only = false;
        World {
            target,
            disk: Ram { data, reads: 0 },
            queue: Vec::new(),
            serve_per_round: usize::MAX,
            overflowed: 0,
            inbox: Vec::new(),
            now: 0,
            tag: 1,
            drop_reply: 0,
            reverse: false,
            replies: 0,
        }
    }

    fn tags(&mut self, frames: u64) -> u32 {
        let base = self.tag;
        self.tag += frames as u32 + 1;
        base
    }

    /// One read through `ahead`, run to its end one time unit a round. Returns the rounds it took.
    fn read(&mut self, ahead: &mut ReadAhead, store: &mut [u8], lba: u64, buffer: &mut [u8]) -> Result<u64, Failure> {
        let tag = &mut self.tag;
        ahead.begin(lba, (buffer.len() / SECTOR) as u64, self.now, &mut |frames| {
            let base = *tag;
            *tag += frames as u32 + 1;
            base
        });
        let result = self.pump(&mut Ahead { ahead: &mut *ahead, buffer: &mut *buffer, store: &mut *store });
        match result {
            Ok(_) => ahead.finish(buffer, store),
            Err(_) => ahead.clear(),
        }
        result
    }

    /// Waits for the frames of dropped read-ahead, as the kernel does before a write.
    fn drain(&mut self, ahead: &mut ReadAhead, store: &mut [u8]) -> u64 {
        ahead.drain();
        let rounds = self.pump(&mut Ahead { ahead: &mut *ahead, buffer: &mut [], store: &mut *store }).unwrap();
        ahead.finish(&mut [], store);
        rounds
    }

    fn write(&mut self, lba: u64, data: &mut [u8]) -> Result<u64, Failure> {
        let sectors = (data.len() / SECTOR) as u64;
        let base = self.tags(frames_for(Op::Write, sectors, PER_FRAME));
        let transfer = Transfer::new(Op::Write, lba, sectors, PER_FRAME, WINDOW, base, TIMEOUT, 8);
        self.pump(&mut Single { transfer, data })
    }

    /// Runs `work` to its end: each round hands over the replies that came back, sends what it gives (into the target's
    /// queue), lets the target serve, and moves the clock one unit. Returns the rounds it took.
    fn pump(&mut self, work: &mut dyn Work) -> Result<u64, Failure> {
        let mut out = [0u8; 2000];
        for round in 0..10_000u64 {
            let mut arrived = core::mem::take(&mut self.inbox);
            if self.reverse {
                arrived.reverse();
            }
            for frame in arrived {
                self.replies += 1;
                if self.drop_reply != 0 && self.replies % self.drop_reply == 0 {
                    continue;
                }
                work.receive(&frame);
            }
            if let Some(result) = work.finished() {
                return result.map(|()| round);
            }
            loop {
                let length = work.next(self.now, &mut out);
                if length == 0 {
                    break;
                }
                if self.queue.len() < WINDOW {
                    self.queue.push(out[..length].to_vec());
                } else {
                    self.overflowed += 1;
                }
            }
            self.serve();
            self.now += 1;
        }
        panic!("the read never finished");
    }

    /// One round with no read in progress: what a thread driving the read-ahead between reads does.
    fn pump_between(&mut self, ahead: &mut ReadAhead, store: &mut [u8]) {
        let mut out = [0u8; 2000];
        for frame in core::mem::take(&mut self.inbox) {
            ahead.receive(&TO, &parse(&frame).unwrap(), &mut [], store);
        }
        loop {
            let length = ahead.next(self.now, &TO, &mut out);
            if length == 0 {
                break;
            }
            if self.queue.len() < WINDOW {
                self.queue.push(out[..length].to_vec());
            } else {
                self.overflowed += 1;
            }
        }
        self.serve();
        self.now += 1;
    }

    /// The target serves what it can this round; the replies travel back.
    fn serve(&mut self) {
        let mut reply = [0u8; 2000];
        let count = self.queue.len().min(self.serve_per_round);
        for request in self.queue.drain(..count).collect::<Vec<_>>() {
            let answer = self.target.handle(&request, &mut self.disk, &mut reply);
            if answer != 0 {
                self.inbox.push(reply[..answer].to_vec());
            }
        }
    }

    fn expect(&self, lba: u64, buffer: &[u8]) {
        let at = lba as usize * SECTOR;
        assert!(buffer == &self.disk.data[at..at + buffer.len()], "sectors from {lba} differ from the disk");
    }
}

fn read_ahead() -> (ReadAhead, Vec<u8>) {
    let ahead = ReadAhead::new(PER_FRAME, WINDOW, TIMEOUT, 8, SECTORS, STALE);
    let store = std::vec![0u8; ahead.store_bytes()];
    (ahead, store)
}

#[test]
fn sequential_reads_match_the_disk_and_come_from_read_ahead() {
    let mut world = World::new();
    let (mut ahead, mut store) = read_ahead();
    assert_eq!(ahead.store_bytes(), CHUNKS * 16 * SECTOR, "a chunk is one window of frames");
    let mut rounds = 0;
    for request in 0..40u64 {
        // 24 sectors: one and a half chunks, so reads start and end inside chunks.
        let mut buffer = std::vec![0u8; 24 * SECTOR];
        rounds += world.read(&mut ahead, &mut store, 100 + request * 24, &mut buffer).unwrap();
        world.expect(100 + request * 24, &buffer);
    }
    assert!(ahead.used > 35 * 24, "most sectors came from read-ahead: {}", ahead.used);
    assert!(ahead.fetched - ahead.used <= (CHUNKS as u64 + 1) * 16, "nothing fetched twice: {} of {}", ahead.used, ahead.fetched);
    // A plain transfer of 12 frames at a window of 8 takes 3 rounds (two of sending, one to deliver): 120 for 40 reads.
    // A full window every round is 12 / 8 rounds a read, 60 in all; read-ahead comes within the first read's start.
    assert!(rounds <= 40 * 12 / 8 + 2, "read-ahead keeps the window full between reads: {rounds} rounds");
}

#[test]
fn lost_and_reordered_replies_still_give_the_right_data() {
    let mut world = World::new();
    world.drop_reply = 7;
    world.reverse = true;
    let (mut ahead, mut store) = read_ahead();
    for request in 0..30u64 {
        let mut buffer = std::vec![0u8; 10 * SECTOR];
        world.read(&mut ahead, &mut store, request * 10, &mut buffer).unwrap();
        world.expect(request * 10, &buffer);
        // The caller works between reads: time passes with replies arriving and nothing sent.
        world.now += 1;
    }
    assert!(ahead.resent() > 0, "some replies were lost, so some frames were sent again");
    assert!(ahead.used > 0);
}

#[test]
fn random_reads_fetch_nothing_extra() {
    let mut world = World::new();
    let (mut ahead, mut store) = read_ahead();
    for lba in [900u64, 30, 2000, 31, 4000, 7] {
        let mut buffer = std::vec![0u8; 4 * SECTOR];
        world.read(&mut ahead, &mut store, lba, &mut buffer).unwrap();
        world.expect(lba, &buffer);
    }
    assert_eq!(ahead.fetched, 0);
}

#[test]
fn a_read_elsewhere_leaves_the_stream_alone() {
    let mut world = World::new();
    let (mut ahead, mut store) = read_ahead();
    let mut buffer = std::vec![0u8; 8 * SECTOR];
    world.read(&mut ahead, &mut store, 0, &mut buffer).unwrap();
    world.read(&mut ahead, &mut store, 8, &mut buffer).unwrap();
    let fetched = ahead.fetched;
    assert!(fetched > 0, "the second read continued the first");
    // An inode or extent block elsewhere, then the file again.
    let mut side = std::vec![0u8; 2 * SECTOR];
    world.read(&mut ahead, &mut store, 3000, &mut side).unwrap();
    world.expect(3000, &side);
    let used = ahead.used;
    world.read(&mut ahead, &mut store, 16, &mut buffer).unwrap();
    world.expect(16, &buffer);
    assert_eq!(ahead.used, used + 8, "the stream's next read came from read-ahead");
}

#[test]
fn read_ahead_stops_at_the_end_of_the_disk() {
    let mut world = World::new();
    let (mut ahead, mut store) = read_ahead();
    let mut lba = SECTORS - 100;
    while lba < SECTORS {
        let mut buffer = std::vec![0u8; 10 * SECTOR];
        world.read(&mut ahead, &mut store, lba, &mut buffer).unwrap();
        world.expect(lba, &buffer);
        lba += 10;
    }
    assert!(ahead.fetched <= 90, "nothing asked for past the end: {}", ahead.fetched);
}

#[test]
fn a_stream_left_idle_reads_the_disk_again() {
    let mut world = World::new();
    let (mut ahead, mut store) = read_ahead();
    let mut buffer = std::vec![0u8; 8 * SECTOR];
    world.read(&mut ahead, &mut store, 0, &mut buffer).unwrap();
    world.read(&mut ahead, &mut store, 8, &mut buffer).unwrap();
    // Let the read-ahead arrive, then another host changes the next sectors.
    world.read(&mut ahead, &mut store, 3000, &mut buffer).unwrap();
    world.disk.data[16 * SECTOR..24 * SECTOR].fill(0xee);
    world.now += STALE + 1;
    world.read(&mut ahead, &mut store, 16, &mut buffer).unwrap();
    assert!(buffer.iter().all(|&byte| byte == 0xee), "idle chunks were dropped, the new sectors read");
}

#[test]
fn clear_drops_what_was_read_ahead() {
    let mut world = World::new();
    let (mut ahead, mut store) = read_ahead();
    let mut buffer = std::vec![0u8; 8 * SECTOR];
    world.read(&mut ahead, &mut store, 0, &mut buffer).unwrap();
    world.read(&mut ahead, &mut store, 8, &mut buffer).unwrap();
    world.read(&mut ahead, &mut store, 3000, &mut buffer).unwrap();
    // Our own write, as the kernel does it: the read-ahead goes first.
    ahead.clear();
    world.disk.data[16 * SECTOR..24 * SECTOR].fill(0x5a);
    world.read(&mut ahead, &mut store, 16, &mut buffer).unwrap();
    assert!(buffer.iter().all(|&byte| byte == 0x5a));
}

#[test]
fn a_long_read_in_a_stream_is_read_straight_into_the_buffer() {
    let mut world = World::new();
    let (mut ahead, mut store) = read_ahead();
    let mut small = std::vec![0u8; 4 * SECTOR];
    world.read(&mut ahead, &mut store, 0, &mut small).unwrap();
    world.read(&mut ahead, &mut store, 4, &mut small).unwrap();
    // Far longer than the chunks: part from them, the rest from a transfer of its own.
    let mut long = std::vec![0u8; 500 * SECTOR];
    world.read(&mut ahead, &mut store, 8, &mut long).unwrap();
    world.expect(8, &long);
    world.read(&mut ahead, &mut store, 508, &mut small).unwrap();
    world.expect(508, &small);
}

#[test]
fn a_short_sequential_run_reads_little_ahead() {
    let mut world = World::new();
    let (mut ahead, mut store) = read_ahead();
    let mut sector = std::vec![0u8; SECTOR];
    world.read(&mut ahead, &mut store, 50, &mut sector).unwrap();
    world.read(&mut ahead, &mut store, 51, &mut sector).unwrap();
    assert_eq!(ahead.fetched, 2, "the second read of a stream asks for twice its size");
    world.read(&mut ahead, &mut store, 52, &mut sector).unwrap();
    // Now 4 sectors past its end, 53 to 57, of which 53 is already asked for.
    assert_eq!(ahead.fetched, 2 + 3, "then twice as much again");
    world.expect(52, &sector);
}

/// Streams on a target that serves 2 frames a round, so read-ahead frames wait in its queue, then writes a full window.
fn stream_then_write(drain: bool) -> World {
    let mut world = World::new();
    world.serve_per_round = 2;
    let (mut ahead, mut store) = read_ahead();
    let mut buffer = std::vec![0u8; 16 * SECTOR];
    for request in 0..4u64 {
        world.read(&mut ahead, &mut store, request * 16, &mut buffer).unwrap();
        world.expect(request * 16, &buffer);
    }
    assert!(ahead.orphaned() == 0 && world.queue.len() > 2, "read-ahead frames are waiting at the target");
    if drain {
        world.drain(&mut ahead, &mut store);
        assert_eq!(ahead.orphaned(), 0);
    } else {
        ahead.clear();
    }
    let mut data = std::vec![0x77u8; WINDOW * PER_FRAME * SECTOR];
    world.write(1000, &mut data).unwrap();
    assert!(world.disk.data[1000 * SECTOR..][..data.len()].iter().all(|&byte| byte == 0x77));
    world
}

#[test]
fn a_write_after_read_ahead_waits_for_its_frames_and_none_is_dropped() {
    assert_eq!(stream_then_write(true).overflowed, 0);
    // What the drain prevents: the write's burst lands on a queue still holding read-ahead and frames are dropped there.
    assert!(stream_then_write(false).overflowed > 0);
}

#[test]
fn frames_of_dropped_read_ahead_still_count_against_the_window() {
    let mut world = World::new();
    world.serve_per_round = 2;
    let (mut ahead, mut store) = read_ahead();
    let mut buffer = std::vec![0u8; 16 * SECTOR];
    for request in 0..4u64 {
        world.read(&mut ahead, &mut store, request * 16, &mut buffer).unwrap();
    }
    // Dropped with frames still queued at the target (as after a failed read); the next read must not overflow it.
    ahead.clear();
    assert!(ahead.orphaned() > 0);
    world.read(&mut ahead, &mut store, 2000, &mut buffer).unwrap();
    world.expect(2000, &buffer);
    assert_eq!(world.overflowed, 0);
}

#[test]
fn driven_between_reads_the_next_read_finds_everything_in() {
    let mut world = World::new();
    let (mut ahead, mut store) = read_ahead();
    let mut buffer = std::vec![0u8; 24 * SECTOR];
    for request in 0..3u64 {
        world.read(&mut ahead, &mut store, request * 24, &mut buffer).unwrap();
    }
    // The reader works; something else keeps the read-ahead going until it is all in.
    while ahead.active() {
        world.pump_between(&mut ahead, &mut store);
    }
    let rounds = world.read(&mut ahead, &mut store, 3 * 24, &mut buffer).unwrap();
    world.expect(3 * 24, &buffer);
    assert_eq!(rounds, 0, "every sector was already there");
}

#[test]
fn read_ahead_that_failed_between_reads_does_not_fail_the_next_read() {
    let mut world = World::new();
    let (mut ahead, mut store) = read_ahead();
    let mut buffer = std::vec![0u8; 8 * SECTOR];
    world.read(&mut ahead, &mut store, 0, &mut buffer).unwrap();
    world.read(&mut ahead, &mut store, 8, &mut buffer).unwrap();
    // The target is away long enough for the read-ahead's frames to run out of tries.
    world.queue.clear();
    world.inbox.clear();
    world.serve_per_round = 0;
    while ahead.active() {
        world.pump_between(&mut ahead, &mut store);
    }
    world.serve_per_round = usize::MAX;
    world.read(&mut ahead, &mut store, 16, &mut buffer).unwrap();
    world.expect(16, &buffer);
}
