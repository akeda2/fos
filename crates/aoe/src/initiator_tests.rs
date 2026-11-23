//! The initiator against this crate's own `Target`, over a simulated link that can lose requests and replies and deliver
//! replies out of order.

extern crate std;

use super::initiator::*;
use super::*;
use std::vec::Vec;

const US: [u8; 6] = [2, 0, 0, 0, 0, 1];
const THEM: [u8; 6] = [2, 0, 0, 0, 0, 9];
const SECTORS: usize = 96;
const TO: Address = Address { ours: US, target: THEM, shelf: 5, slot: 2 };

struct Ram {
    data: Vec<u8>,
}

impl Ram {
    fn new() -> Ram {
        Ram { data: (0..SECTORS * SECTOR).map(|at| (at % 251) as u8).collect() }
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
        let at = lba as usize * SECTOR;
        self.data[at..at + data.len()].copy_from_slice(data);
        true
    }
    fn flush(&mut self) -> bool {
        true
    }
}

/// How the link misbehaves: drop every `drop_request`-th request and every `drop_reply`-th reply (0: never), and deliver
/// each round's replies in reverse order.
#[derive(Default, Clone, Copy)]
struct Link {
    drop_request: usize,
    drop_reply: usize,
    reverse: bool,
}

struct Run {
    result: Result<(), Failure>,
    sent: usize,
    resent: u32,
    rounds: u64,
}

/// Drives one transfer to the end: each round (one time unit) sends everything `next` gives, lets the target answer, and
/// hands the replies back. `buffer` is the read buffer or the write data.
fn run(transfer: &mut Transfer, target: &mut Target, disk: &mut Ram, link: Link, buffer: &mut [u8]) -> Run {
    let (mut sent, mut replies_seen) = (0, 0);
    let mut out = [0u8; 2000];
    let mut reply = [0u8; 2000];
    for now in 0..10_000u64 {
        if let Some(result) = transfer.finished() {
            return Run { result, sent, resent: transfer.resent, rounds: now };
        }
        let mut replies: Vec<Vec<u8>> = Vec::new();
        loop {
            let length = transfer.next(now, &TO, buffer, &mut out);
            if length == 0 {
                break;
            }
            sent += 1;
            if link.drop_request != 0 && sent % link.drop_request == 0 {
                continue;
            }
            let answer = target.handle(&out[..length], disk, &mut reply);
            if answer != 0 {
                replies.push(reply[..answer].to_vec());
            }
        }
        if link.reverse {
            replies.reverse();
        }
        for frame in replies {
            replies_seen += 1;
            if link.drop_reply != 0 && replies_seen % link.drop_reply == 0 {
                continue;
            }
            let response = parse(&frame).expect("the target's reply parses");
            transfer.receive(&TO, &response, buffer);
        }
    }
    panic!("the transfer never finished");
}

fn target() -> Target {
    let mut target = Target::new(5, 2, THEM, 1500);
    target.read_only = false;
    target
}

#[test]
fn query_config_finds_the_target_and_its_limits() {
    let mut out = [0u8; 64];
    let length = query_config(&mut out, US, 0xffff, 0xff, 7);
    assert_eq!(&out[0..6], &[0xff; 6], "a discovery goes to the broadcast address");
    let mut reply = [0u8; 2000];
    let answer = target().handle(&out[..length], &mut Ram::new(), &mut reply);
    let response = parse(&reply[..answer]).unwrap();
    assert_eq!((response.source, response.shelf, response.slot, response.tag), (THEM, 5, 2, 7));
    match response.kind {
        Kind::Config(config) => {
            assert_eq!(config.bufcount, 16);
            assert_eq!(config.sectors, 2);
            assert_eq!(config.config, b"");
        }
        other => panic!("not a config reply: {other:?}"),
    }
}

#[test]
fn identify_gives_the_size_and_model() {
    let mut transfer = Transfer::new(Op::Identify, 0, 0, 2, 16, 100, 3, 4);
    let mut sector = [0u8; SECTOR];
    let done = run(&mut transfer, &mut target(), &mut Ram::new(), Link::default(), &mut sector);
    assert_eq!(done.result, Ok(()));
    let geometry = parse_identify(&sector).unwrap();
    assert_eq!(geometry.sectors, SECTORS as u64);
    assert!(geometry.model.starts_with(b"fosfor-os aoe "), "{:?}", core::str::from_utf8(&geometry.model));
}

#[test]
fn a_read_over_a_clean_link_matches_the_disk() {
    let mut disk = Ram::new();
    let mut buffer = std::vec![0u8; 37 * SECTOR];
    let mut transfer = Transfer::new(Op::Read, 11, 37, 2, 4, 1000, 3, 4);
    assert_eq!(transfer.frames(), 19, "37 sectors at 2 a frame");
    let done = run(&mut transfer, &mut target(), &mut disk, Link::default(), &mut buffer);
    assert_eq!(done.result, Ok(()));
    assert_eq!(done.sent, 19);
    assert_eq!(done.resent, 0);
    assert_eq!(buffer[..], disk.data[11 * SECTOR..48 * SECTOR]);
    assert_eq!(done.rounds, 5, "a window of 4: 19 frames in 5 rounds (replies come back at the end of each)");
}

#[test]
fn a_write_then_a_read_round_trips() {
    let mut disk = Ram::new();
    let mut target = target();
    let data: Vec<u8> = (0..20 * SECTOR).map(|at| (at * 7 % 253) as u8).collect();
    let mut write = Transfer::new(Op::Write, 40, 20, 2, 16, 5000, 3, 4);
    let mut data_copy = data.clone();
    assert_eq!(run(&mut write, &mut target, &mut disk, Link::default(), &mut data_copy).result, Ok(()));
    assert_eq!(disk.data[40 * SECTOR..60 * SECTOR], data[..]);
    let mut flush = Transfer::new(Op::Flush, 0, 0, 2, 16, 6000, 3, 4);
    assert_eq!(run(&mut flush, &mut target, &mut disk, Link::default(), &mut []).result, Ok(()));
    let mut back = std::vec![0u8; 20 * SECTOR];
    let mut read = Transfer::new(Op::Read, 40, 20, 2, 16, 7000, 3, 4);
    assert_eq!(run(&mut read, &mut target, &mut disk, Link::default(), &mut back).result, Ok(()));
    assert_eq!(back, data);
}

#[test]
fn lost_and_reordered_frames_are_resent_and_the_data_is_still_right() {
    let mut disk = Ram::new();
    let mut target = target();
    let link = Link { drop_request: 5, drop_reply: 7, reverse: true };
    let data: Vec<u8> = (0..64 * SECTOR).map(|at| (at * 13 % 241) as u8).collect();
    let mut copy = data.clone();
    let mut write = Transfer::new(Op::Write, 8, 64, 2, 16, 8000, 3, 8);
    let done = run(&mut write, &mut target, &mut disk, link, &mut copy);
    assert_eq!(done.result, Ok(()));
    assert!(done.resent > 0, "some frames were lost, so some were sent again");
    assert_eq!(disk.data[8 * SECTOR..72 * SECTOR], data[..]);
    let mut back = std::vec![0u8; 64 * SECTOR];
    let mut read = Transfer::new(Op::Read, 8, 64, 2, 16, 9000, 3, 8);
    let done = run(&mut read, &mut target, &mut disk, link, &mut back);
    assert_eq!(done.result, Ok(()));
    assert!(done.resent > 0);
    assert_eq!(back, data);
}

#[test]
fn a_write_to_a_read_only_export_fails_as_a_device_error() {
    let mut disk = Ram::new();
    let before = disk.data.clone();
    let mut target = target();
    target.read_only = true;
    let mut data = std::vec![0xaa; 4 * SECTOR];
    let mut write = Transfer::new(Op::Write, 0, 4, 2, 16, 1, 3, 4);
    assert_eq!(run(&mut write, &mut target, &mut disk, Link::default(), &mut data).result, Err(Failure::Device));
    assert_eq!(disk.data, before);
}

#[test]
fn a_read_past_the_end_fails_as_a_device_error() {
    let mut buffer = std::vec![0u8; 4 * SECTOR];
    let mut read = Transfer::new(Op::Read, SECTORS as u64 - 2, 4, 2, 16, 1, 3, 4);
    assert_eq!(run(&mut read, &mut target(), &mut Ram::new(), Link::default(), &mut buffer).result, Err(Failure::Device));
}

#[test]
fn a_target_that_never_answers_times_out_after_max_tries() {
    let mut buffer = std::vec![0u8; 2 * SECTOR];
    let mut read = Transfer::new(Op::Read, 0, 2, 2, 16, 1, 3, 4);
    let link = Link { drop_request: 1, ..Link::default() };
    let done = run(&mut read, &mut target(), &mut Ram::new(), link, &mut buffer);
    assert_eq!(done.result, Err(Failure::Timeout));
    assert_eq!(done.sent, 4, "the first send and three resends");
    // 3, then 6, then 12 units between sends, then 24 before giving up (at time 45; the harness sees it a round later).
    assert_eq!(done.rounds, 3 + 6 + 12 + 24 + 1);
}

#[test]
fn replies_that_are_not_ours_are_ignored() {
    let mut disk = Ram::new();
    let mut target = target();
    let mut out = [0u8; 2000];
    let mut reply = [0u8; 2000];
    let mut buffer = std::vec![0u8; 2 * SECTOR];
    let mut read = Transfer::new(Op::Read, 0, 2, 2, 16, 50, 3, 4);
    let length = read.next(0, &TO, &[], &mut out);
    let answer = target.handle(&out[..length], &mut disk, &mut reply);
    let mut frame = reply[..answer].to_vec();
    // Another tag (a reply to an earlier transfer), another slot, another MAC: none counts.
    for (at, value) in [(23, 49u8), (18, 3), (11, 0x55)] {
        let saved = frame[at];
        frame[at] = value;
        assert!(!read.receive(&TO, &parse(&frame).unwrap(), &mut buffer), "byte {at} changed");
        frame[at] = saved;
    }
    assert!(read.finished().is_none());
    assert!(read.receive(&TO, &parse(&frame).unwrap(), &mut buffer));
    assert!(!read.receive(&TO, &parse(&frame).unwrap(), &mut buffer), "a duplicate is ignored");
    assert_eq!(read.finished(), Some(Ok(())));
    assert_eq!(buffer[..], disk.data[..2 * SECTOR]);
}

#[test]
fn a_request_is_not_mistaken_for_a_reply() {
    let mut out = [0u8; 64];
    let length = query_config(&mut out, US, 5, 2, 1);
    assert!(parse(&out[..length]).is_none());
}
