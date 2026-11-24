use super::*;

const US: [u8; 6] = [2, 0, 0, 0, 0, 1];
const THEM: [u8; 6] = [2, 0, 0, 0, 0, 9];

struct Ram {
    data: [u8; SECTOR * 64],
    flushes: u32,
    fail: bool,
}

impl Ram {
    fn new() -> Ram {
        let mut data = [0u8; SECTOR * 64];
        for (at, byte) in data.iter_mut().enumerate() {
            *byte = (at % 251) as u8;
        }
        Ram { data, flushes: 0, fail: false }
    }
}

impl Disk for Ram {
    fn sectors(&self) -> u64 {
        64
    }
    fn read(&mut self, lba: u64, buffer: &mut [u8]) -> bool {
        let at = lba as usize * SECTOR;
        buffer.copy_from_slice(&self.data[at..at + buffer.len()]);
        !self.fail
    }
    fn write(&mut self, lba: u64, data: &[u8]) -> bool {
        let at = lba as usize * SECTOR;
        self.data[at..at + data.len()].copy_from_slice(data);
        !self.fail
    }
    fn flush(&mut self) -> bool {
        self.flushes += 1;
        !self.fail
    }
}

fn header(shelf: u16, slot: u8, command: u8, tag: u32) -> [u8; 24] {
    let mut frame = [0u8; 24];
    frame[0..6].copy_from_slice(&US);
    frame[6..12].copy_from_slice(&THEM);
    frame[12..14].copy_from_slice(&ETHERTYPE.to_be_bytes());
    frame[14] = VERSION << 4;
    frame[16..18].copy_from_slice(&shelf.to_be_bytes());
    frame[18] = slot;
    frame[19] = command;
    frame[20..24].copy_from_slice(&tag.to_be_bytes());
    frame
}

/// An ATA request; `data` follows the header.
fn ata(shelf: u16, slot: u8, aflags: u8, count: u8, command: u8, lba: u64, data: &[u8]) -> ([u8; 2000], usize) {
    let mut frame = [0u8; 2000];
    frame[..24].copy_from_slice(&header(shelf, slot, COMMAND_ATA, 0xcafe));
    frame[24] = aflags;
    frame[26] = count;
    frame[27] = command;
    frame[28..34].copy_from_slice(&lba.to_le_bytes()[..6]);
    frame[DATA_AT..DATA_AT + data.len()].copy_from_slice(data);
    (frame, DATA_AT + data.len())
}

fn target() -> Target {
    let mut target = Target::new(7, 3, US, 1500);
    target.read_only = false;
    target
}

fn run(target: &mut Target, disk: &mut Ram, request: ([u8; 2000], usize)) -> ([u8; 2000], usize) {
    let mut out = [0u8; 2000];
    let length = target.handle(&request.0[..request.1], disk, &mut out);
    (out, length)
}

fn status(reply: &([u8; 2000], usize)) -> u8 {
    reply.0[27]
}

#[test]
fn sectors_per_frame_follow_the_mtu() {
    assert_eq!(max_sectors(1500), 2);
    assert_eq!(max_sectors(9000), 17);
    assert_eq!(max_sectors(100), 0);
    assert_eq!(max_sectors(200_000), 255);
}

#[test]
fn read_returns_the_sectors_and_echoes_the_request() {
    let (mut target, mut disk) = (target(), Ram::new());
    let reply = run(&mut target, &mut disk, ata(7, 3, 0x40, 2, ATA_READ_EXT, 5, &[]));
    let (frame, length) = reply;
    assert_eq!(length, DATA_AT + 2 * SECTOR);
    assert_eq!(&frame[0..6], &THEM);
    assert_eq!(&frame[6..12], &US);
    assert_eq!(frame[14], 0x18, "version 1, response");
    assert_eq!(&frame[20..24], &0xcafe_u32.to_be_bytes(), "the tag is echoed");
    assert_eq!(status(&(frame, length)), STATUS_OK);
    assert_eq!(&frame[DATA_AT..length], &disk.data[5 * SECTOR..7 * SECTOR]);
    assert_eq!(target.reads, 1);
}

#[test]
fn write_lands_on_the_disk_and_read_only_refuses_it() {
    let (mut target, mut disk) = (target(), Ram::new());
    let payload = [0xa5u8; 2 * SECTOR];
    let reply = run(&mut target, &mut disk, ata(7, 3, 0x41, 2, ATA_WRITE_EXT, 10, &payload));
    assert_eq!(status(&reply), STATUS_OK);
    assert_eq!(reply.1, DATA_AT, "a write is answered with the header alone");
    assert_eq!(&disk.data[10 * SECTOR..12 * SECTOR], &payload);

    target.read_only = true;
    let before = disk.data;
    let reply = run(&mut target, &mut disk, ata(7, 3, 0x41, 2, ATA_WRITE_EXT, 20, &payload));
    assert_eq!((status(&reply), reply.0[25]), (STATUS_ERROR, ERR_ABORT));
    assert_eq!(before[..], disk.data[..], "nothing was written");
}

#[test]
fn only_the_named_writer_writes() {
    let (mut target, mut disk) = (target(), Ram::new());
    let payload = [0x5au8; SECTOR];
    target.writer = Some([2, 0, 0, 0, 0, 7]);
    let before = disk.data;
    let reply = run(&mut target, &mut disk, ata(7, 3, 0x41, 1, ATA_WRITE_EXT, 4, &payload));
    assert_eq!((status(&reply), reply.0[25]), (STATUS_ERROR, ERR_ABORT), "THEM is not the writer");
    assert_eq!(before[..], disk.data[..]);
    assert_eq!(status(&run(&mut target, &mut disk, ata(7, 3, 0x40, 0, ATA_FLUSH_EXT, 0, &[]))), STATUS_OK);
    assert_eq!(disk.flushes, 0, "a flush from another machine does not reach the disk");
    let reply = run(&mut target, &mut disk, ata(7, 3, 0x40, 1, ATA_READ_EXT, 4, &[]));
    assert_eq!(status(&reply), STATUS_OK, "it may still read");
    assert_eq!((target.foreign_writes, target.writes), (1, 0));

    target.writer = Some(THEM);
    let reply = run(&mut target, &mut disk, ata(7, 3, 0x41, 1, ATA_WRITE_EXT, 4, &payload));
    assert_eq!(status(&reply), STATUS_OK, "the writer writes");
    assert_eq!(&disk.data[4 * SECTOR..5 * SECTOR], &payload);
    assert_eq!(target.foreign_writes, 1);

    target.read_only = true;
    run(&mut target, &mut disk, ata(7, 3, 0x41, 1, ATA_WRITE_EXT, 4, &payload));
    assert_eq!(target.foreign_writes, 1, "read-only refuses everyone, and that is not counted as foreign");
}

#[test]
fn a_short_write_and_a_disk_failure_abort() {
    let (mut target, mut disk) = (target(), Ram::new());
    let reply = run(&mut target, &mut disk, ata(7, 3, 0x41, 2, ATA_WRITE_EXT, 0, &[0u8; SECTOR]));
    assert_eq!(status(&reply), STATUS_ERROR, "the frame carries one sector of the two it announces");
    disk.fail = true;
    let reply = run(&mut target, &mut disk, ata(7, 3, 0x40, 1, ATA_READ_EXT, 0, &[]));
    assert_eq!(status(&reply), STATUS_ERROR);
    assert_eq!(target.errors, 2);
}

#[test]
fn out_of_range_and_oversize_requests_abort() {
    let (mut target, mut disk) = (target(), Ram::new());
    for (count, lba) in [(1, 64), (2, 63), (0, 0), (3, 0)] {
        let reply = run(&mut target, &mut disk, ata(7, 3, 0x40, count, ATA_READ_EXT, lba, &[]));
        assert_eq!(status(&reply), STATUS_ERROR, "count {count} at {lba}");
    }
    // The last sector is fine, and a huge LBA does not wrap around.
    assert_eq!(status(&run(&mut target, &mut disk, ata(7, 3, 0x40, 1, ATA_READ_EXT, 63, &[]))), STATUS_OK);
    assert_eq!(status(&run(&mut target, &mut disk, ata(7, 3, 0x40, 1, ATA_READ_EXT, u64::MAX >> 16, &[]))), STATUS_ERROR);
}

#[test]
fn lba28_uses_the_low_nibble_of_the_fourth_byte() {
    let (mut target, mut disk) = (target(), Ram::new());
    // The device register's high nibble (0xe0) is set the way an old initiator sends it and must not count as an address.
    let (mut frame, length) = ata(7, 3, 0x00, 1, ATA_READ, 9, &[]);
    frame[31] = 0xe0;
    let reply = run(&mut target, &mut disk, (frame, length));
    assert_eq!(status(&reply), STATUS_OK);
    assert_eq!(&reply.0[DATA_AT..DATA_AT + SECTOR], &disk.data[9 * SECTOR..10 * SECTOR]);
}

#[test]
fn flush_reaches_the_disk() {
    let (mut target, mut disk) = (target(), Ram::new());
    assert_eq!(status(&run(&mut target, &mut disk, ata(7, 3, 0x40, 0, ATA_FLUSH_EXT, 0, &[]))), STATUS_OK);
    assert_eq!(disk.flushes, 1);
}

#[test]
fn identify_reports_the_capacity_and_the_name() {
    let (mut target, mut disk) = (target(), Ram::new());
    let reply = run(&mut target, &mut disk, ata(7, 3, 0x40, 0, ATA_IDENTIFY, 0, &[]));
    assert_eq!((status(&reply), reply.1), (STATUS_OK, DATA_AT + SECTOR));
    let word = |n: usize| u16::from_le_bytes([reply.0[DATA_AT + n * 2], reply.0[DATA_AT + n * 2 + 1]]);
    assert_eq!(word(83) & (1 << 10), 1 << 10, "LBA48");
    assert_eq!(word(100), 64);
    assert_eq!((word(60), word(61)), (64, 0));
    assert_eq!(word(106), 0x4000, "logical and physical sectors are 512 bytes");
    // "fosfor-os aoe": the first character sits in the high byte of word 27.
    assert_eq!(&reply.0[DATA_AT + 54..DATA_AT + 58], b"offs");
}

#[test]
fn a_big_disk_is_reported_in_four_words_and_capped_for_lba28() {
    let target = target();
    let mut sector = [0u8; SECTOR];
    identify(&target, 1 << 40, &mut sector);
    let word = |n: usize| u16::from_le_bytes([sector[n * 2], sector[n * 2 + 1]]);
    assert_eq!((word(100), word(101), word(102), word(103)), (0, 0, 0x100, 0));
    assert_eq!((word(60), word(61)), (0xffff, 0x0fff));
}

fn config_request(shelf: u16, slot: u8, ccmd: u8, config: &[u8]) -> ([u8; 2000], usize) {
    let mut frame = [0u8; 2000];
    frame[..24].copy_from_slice(&header(shelf, slot, COMMAND_CONFIG, 77));
    // Body at 24: bufcount, firmware (ignored in a request), sectors/frame at 28, version and ccmd at 29, length, string.
    frame[29] = (VERSION << 4) | ccmd;
    frame[30..32].copy_from_slice(&(config.len() as u16).to_be_bytes());
    frame[32..32 + config.len()].copy_from_slice(config);
    (frame, 24 + 8 + config.len())
}

#[test]
fn a_broadcast_query_is_answered_with_our_address() {
    let (mut target, mut disk) = (target(), Ram::new());
    let (frame, length) = run(&mut target, &mut disk, config_request(0xffff, 0xff, 0, &[]));
    assert_eq!(length, 24 + 8);
    assert_eq!(u16::from_be_bytes([frame[16], frame[17]]), 7, "shelf");
    assert_eq!(frame[18], 3, "slot");
    assert_eq!(u16::from_be_bytes([frame[24], frame[25]]), 16, "bufcount");
    assert_eq!(frame[28], 2, "two sectors per frame at MTU 1500");
    assert_eq!(frame[14], 0x18);
}

#[test]
fn the_config_string_is_set_tested_and_protected() {
    let (mut target, mut disk) = (target(), Ram::new());
    let reply = run(&mut target, &mut disk, config_request(7, 3, 3, b"san"));
    assert_eq!(reply.1, 24 + 8 + 3, "set if empty: answered with the new string");
    assert_eq!(target.config(), b"san");
    let reply = run(&mut target, &mut disk, config_request(7, 3, 3, b"other"));
    assert_eq!((reply.0[14] & FLAG_ERROR, reply.0[15]), (FLAG_ERROR, ERROR_CONFIG_PRESENT));
    assert_eq!(run(&mut target, &mut disk, config_request(0xffff, 0xff, 1, b"nas")).1, 0, "exact test: no match, no answer");
    assert_ne!(run(&mut target, &mut disk, config_request(0xffff, 0xff, 1, b"san")).1, 0);
    assert_ne!(run(&mut target, &mut disk, config_request(0xffff, 0xff, 2, b"sa")).1, 0, "prefix test");
    run(&mut target, &mut disk, config_request(7, 3, 4, b"nas"));
    assert_eq!(target.config(), b"nas", "force set replaces it");
}

#[test]
fn frames_for_others_and_responses_are_ignored() {
    let (mut target, mut disk) = (target(), Ram::new());
    assert_eq!(run(&mut target, &mut disk, ata(7, 4, 0x40, 1, ATA_READ_EXT, 0, &[])).1, 0, "another slot");
    assert_eq!(run(&mut target, &mut disk, ata(8, 3, 0x40, 1, ATA_READ_EXT, 0, &[])).1, 0, "another shelf");
    assert_eq!(run(&mut target, &mut disk, ata(0xffff, 3, 0x40, 1, ATA_READ_EXT, 0, &[])).1, 0, "ATA to a broadcast address");
    let (mut response, length) = ata(7, 3, 0x40, 1, ATA_READ_EXT, 0, &[]);
    response[14] |= FLAG_RESPONSE;
    assert_eq!(run(&mut target, &mut disk, (response, length)).1, 0);
    let (mut other, length) = ata(7, 3, 0x40, 1, ATA_READ_EXT, 0, &[]);
    other[12..14].copy_from_slice(&[0x08, 0x00]);
    assert_eq!(run(&mut target, &mut disk, (other, length)).1, 0, "not AoE");
    let (mut multicast, length) = ata(7, 3, 0x40, 1, ATA_READ_EXT, 0, &[]);
    multicast[6] = 0x01;
    assert_eq!(run(&mut target, &mut disk, (multicast, length)).1, 0, "a multicast source gets no answer");
}

#[test]
fn unknown_commands_and_versions_are_errors() {
    let (mut target, mut disk) = (target(), Ram::new());
    let mut frame = [0u8; 2000];
    frame[..24].copy_from_slice(&header(7, 3, 2, 1));
    let reply = run(&mut target, &mut disk, (frame, 24));
    assert_eq!((reply.0[14], reply.0[15]), (0x1c, ERROR_UNKNOWN_COMMAND));
    frame[14] = 2 << 4;
    let reply = run(&mut target, &mut disk, (frame, 24));
    assert_eq!(reply.0[15], ERROR_BAD_VERSION);
    let reply = run(&mut target, &mut disk, ata(7, 3, 0x40, 1, 0x99, 0, &[]));
    assert_eq!(status(&reply), STATUS_ERROR, "an ATA command we do not know is aborted");
}

#[test]
fn a_truncated_frame_is_not_a_crash() {
    let (mut target, mut disk) = (target(), Ram::new());
    let mut out = [0u8; 2000];
    for length in 0..40 {
        let (frame, _) = ata(7, 3, 0x40, 1, ATA_READ_EXT, 0, &[]);
        target.handle(&frame[..length], &mut disk, &mut out);
    }
}
