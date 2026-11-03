//! NETFS on the server kernel: `tools/foscopy.py` reads (GET, 0x30-0x34) and writes (PUT, 0x35-0x39) files of a FOSFS volume
//! over raw Ethernet, in the wire format of the embedded kernel's `netfs.rs`.
//!
//! The receive path runs under the NIC lock, where a disk transfer (it sleeps on the disk lock) must not happen. So
//! `handle` only copies a request into a queue and the service thread's `serve` works through it with the NIC lock released:
//! a GET streams START, 1 KiB DATA frames one millisecond apart, STOP; a PUT collects its chunks in a heap buffer and
//! writes the file when STOP arrives and the length and RFC 1071 checksum match. Unlike the embedded kernel there is no
//! shared staging buffer, so a running tape does not stop NETFS; the disk lock (`tapefs::lock`) orders it with the tapes'
//! own file calls.

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use kernel_core::{fosfs, tape_fs, volumes};

use crate::apic;
use crate::net;
use crate::sync::SpinLock;
use crate::tapefs;
use crate::tapesync;

pub const TAG_GET_REQUEST: u8 = 0x30;
pub const TAG_GET_START: u8 = 0x31;
pub const TAG_GET_DATA: u8 = 0x32;
pub const TAG_GET_STOP: u8 = 0x33;
pub const TAG_GET_ERROR: u8 = 0x34;
pub const TAG_PUT_REQUEST: u8 = 0x35;
pub const TAG_PUT_DATA: u8 = 0x36;
pub const TAG_PUT_STOP: u8 = 0x37;
pub const TAG_PUT_ACK: u8 = 0x38;
pub const TAG_PUT_ERROR: u8 = 0x39;
const NOT_FOUND: u8 = 1;
const NO_DISK: u8 = 2;
const TOO_LARGE: u8 = 3;
const BAD_NAME: u8 = 4;
const DIRECTORY_FULL: u8 = 5;
const DATA_FULL: u8 = 6;
const DISK_ERROR: u8 = 7;
const CHECKSUM: u8 = 8;
const BUSY: u8 = 9;
/// The tape-sync request (`tapesync.rs`): a nonce in, a signed manifest out as a GET stream.
pub const TAG_SYNC_REQUEST: u8 = 0x40;
pub use kernel_core::gpio_client::{TAG_GPIO_REPLY, TAG_GPIO_REQUEST};
/// The shared secret of tape sync: NETFS never reads or writes it.
pub const KEY_FILE: &str = "synckey";
const CHUNK: usize = 1024;
/// The largest file a transfer carries (the embedded kernel's staging buffer is 4 MiB; a tape's own write limit is 8 MiB).
const LIMIT: usize = 8 << 20;
/// Requests queued at once: a PUT of `LIMIT` is 8192 DATA frames, the service loop empties the queue every pass.
const QUEUE_LIMIT: usize = 16384;
/// A transfer another machine started is left alone for this many ticks of silence.
const STALE_TICKS: u64 = 500;

struct Request {
    index: usize,
    source: [u8; 6],
    tag: u8,
    payload: Vec<u8>,
}

static QUEUE: SpinLock<VecDeque<Request>> = SpinLock::new(VecDeque::new());

struct Put {
    source: [u8; 6],
    disk: usize,
    name: Vec<u8>,
    expected: usize,
    checksum: u16,
    next_sequence: u32,
    last_activity: u64,
    data: Vec<u8>,
}

/// The PUT in progress; touched only by `serve` on the service thread.
static mut PUT: Option<Put> = None;

fn put() -> &'static mut Option<Put> {
    unsafe { &mut *core::ptr::addr_of_mut!(PUT) }
}

/// True for the tags this module serves.
pub fn owns(tag: u8) -> bool {
    matches!(tag, TAG_GET_REQUEST | TAG_PUT_REQUEST | TAG_PUT_DATA | TAG_PUT_STOP | TAG_SYNC_REQUEST | TAG_GPIO_REQUEST)
}

/// Called from the frame handler (NIC lock held): queues the request for `serve`.
pub fn handle(index: usize, source: [u8; 6], tag: u8, payload: &[u8]) {
    let request = Request { index, source, tag, payload: payload.to_vec() };
    let mut queue = QUEUE.lock();
    if queue.len() < QUEUE_LIMIT {
        queue.push_back(request);
    }
}

/// Works through the queued requests. Service thread only.
pub fn serve() {
    loop {
        let Some(request) = QUEUE.lock().pop_front() else { return };
        let Request { index, source, tag, payload } = request;
        match tag {
            TAG_GET_REQUEST => get_request(index, source, &payload),
            TAG_PUT_REQUEST => put_request(index, source, &payload),
            TAG_PUT_DATA => put_data(source, &payload),
            TAG_PUT_STOP => put_stop(index, source),
            TAG_SYNC_REQUEST => tapesync::serve(index, source, &payload),
            // No pins here: say so to the challenge request, answer nothing else.
            TAG_GPIO_REQUEST if payload.first() == Some(&0) => reply(index, &source, TAG_GPIO_REPLY, &[kernel_core::gpio_client::STATUS_NO_GPIO]),
            _ => {}
        }
    }
}

pub fn reply(index: usize, destination: &[u8; 6], tag: u8, payload: &[u8]) {
    net::send_frame(index, destination, tag, payload);
}

/// The name in a request: up to the first zero byte (frames are padded to the Ethernet minimum).
fn name_of(bytes: &[u8]) -> &[u8] {
    &bytes[..bytes.iter().position(|&byte| byte == 0).unwrap_or(bytes.len())]
}

pub fn busy(source: &[u8; 6]) -> bool {
    let other_put = put().as_ref().is_some_and(|put| put.source != *source && apic::ticks().wrapping_sub(put.last_activity) < STALE_TICKS);
    other_put || net::kup_busy()
}

fn get_request(index: usize, source: [u8; 6], payload: &[u8]) {
    if payload.len() < 2 {
        return;
    }
    let fail = |code: u8| reply(index, &source, TAG_GET_ERROR, &[code]);
    if busy(&source) {
        return fail(BUSY);
    }
    let name = name_of(&payload[1..]);
    if !fosfs::valid_name(name) {
        return fail(BAD_NAME);
    }
    if name == KEY_FILE.as_bytes() {
        return fail(NOT_FOUND);
    }
    let data = {
        let _disk = tapefs::lock();
        let Ok(volume) = volumes::volume_on(usize::from(payload[0])) else { return fail(NO_DISK) };
        let path = match volume.file_length(name) {
            Ok(Some(_)) => name,
            Ok(None) => match tape_fs::bin_path(name) {
                Some(path) if matches!(volume.file_length(path), Ok(Some(_))) => path,
                _ => return fail(NOT_FOUND),
            },
            Err(_) => return fail(DISK_ERROR),
        };
        let length = match volume.file_length(path) {
            Ok(Some(length)) if length > 0 && length as usize <= LIMIT => length as usize,
            Ok(Some(_)) => return fail(TOO_LARGE),
            _ => return fail(DISK_ERROR),
        };
        let mut data = alloc::vec![0u8; length];
        match volume.read(path, &mut data) {
            Ok(Some(read)) => data.truncate(read),
            Ok(None) => return fail(NOT_FOUND),
            Err(fosfs::Error::TooBig) => return fail(TOO_LARGE),
            Err(_) => return fail(DISK_ERROR),
        }
        data
    };
    stream(index, &source, &data, core::str::from_utf8(name).unwrap_or("?"));
}

/// Sends `data` as a GET answer: START with the length and checksum, numbered DATA chunks, STOP.
pub fn stream(index: usize, source: &[u8; 6], data: &[u8], what: &str) {
    let mut start = [0u8; 6];
    start[0..4].copy_from_slice(&(data.len() as u32).to_le_bytes());
    start[4..6].copy_from_slice(&fosfor_abi::internet_checksum(data).to_le_bytes());
    reply(index, source, TAG_GET_START, &start);
    println!("NETFS: sending {what} ({} bytes) to {}.", data.len(), kernel_core::netdump::Mac(source));
    // The receiver reads from a finite ring: one chunk a millisecond is a pace real hardware keeps up with.
    let mut next = apic::micros();
    for (sequence, chunk) in data.chunks(CHUNK).enumerate() {
        let mut frame = [0u8; 4 + CHUNK];
        frame[0..4].copy_from_slice(&(sequence as u32).to_le_bytes());
        frame[4..4 + chunk.len()].copy_from_slice(chunk);
        while apic::micros() < next {
            core::hint::spin_loop();
        }
        next = apic::micros() + 1000;
        reply(index, source, TAG_GET_DATA, &frame[..4 + chunk.len()]);
        crate::debug::pet();
    }
    reply(index, source, TAG_GET_STOP, &[]);
}

fn put_request(index: usize, source: [u8; 6], payload: &[u8]) {
    if payload.len() < 8 {
        return;
    }
    let fail = |code: u8| reply(index, &source, TAG_PUT_ERROR, &[code]);
    if busy(&source) {
        return fail(BUSY);
    }
    let expected = u32::from_le_bytes(payload[1..5].try_into().unwrap()) as usize;
    let checksum = u16::from_le_bytes(payload[5..7].try_into().unwrap());
    let name = name_of(&payload[7..]);
    if !fosfs::valid_name(name) || name == KEY_FILE.as_bytes() {
        return fail(BAD_NAME);
    }
    {
        let _disk = tapefs::lock();
        if volumes::volume_on(usize::from(payload[0])).is_err() {
            return fail(NO_DISK);
        }
    }
    if expected == 0 || expected > LIMIT {
        return fail(TOO_LARGE);
    }
    let mut data = Vec::new();
    if data.try_reserve_exact(expected).is_err() {
        return fail(TOO_LARGE);
    }
    println!("NETFS: receiving {} ({expected} bytes) from {}.", core::str::from_utf8(name).unwrap_or("?"), kernel_core::netdump::Mac(&source));
    *put() = Some(Put { source, disk: usize::from(payload[0]), name: name.to_vec(), expected, checksum, next_sequence: 0, last_activity: apic::ticks(), data });
}

fn put_data(source: [u8; 6], payload: &[u8]) {
    let Some(state) = put().as_mut().filter(|put| put.source == source) else { return };
    if payload.len() < 4 {
        return;
    }
    let sequence = u32::from_le_bytes(payload[0..4].try_into().unwrap());
    if sequence != state.next_sequence {
        println!("NETFS: chunk {sequence} arrived where {} was expected; transfer abandoned.", state.next_sequence);
        *put() = None;
        return;
    }
    // The chunk's size comes from the declared length, never from the (padded) frame.
    let take = (payload.len() - 4).min(CHUNK).min(state.expected - state.data.len());
    state.data.extend_from_slice(&payload[4..4 + take]);
    state.next_sequence += 1;
    state.last_activity = apic::ticks();
}

fn put_stop(index: usize, source: [u8; 6]) {
    if !put().as_ref().is_some_and(|put| put.source == source) {
        return;
    }
    let state = put().take().unwrap();
    let fail = |code: u8| reply(index, &source, TAG_PUT_ERROR, &[code]);
    if state.data.len() != state.expected {
        println!("NETFS: transfer ended after {} of {} bytes; abandoned.", state.data.len(), state.expected);
        return fail(CHECKSUM);
    }
    if fosfor_abi::internet_checksum(&state.data) != state.checksum {
        println!("NETFS: checksum mismatch; file rejected.");
        return fail(CHECKSUM);
    }
    let result = {
        let _disk = tapefs::lock();
        let Ok(volume) = volumes::volume_on(state.disk) else { return fail(NO_DISK) };
        volume.write(tape_fs::replace_target(&volume, &state.name), &state.data)
    };
    let shown = core::str::from_utf8(&state.name).unwrap_or("?");
    match result {
        Ok(()) => {
            println!("NETFS: stored {shown} ({} bytes).", state.data.len());
            reply(index, &source, TAG_PUT_ACK, &[]);
        }
        Err(fosfs::Error::DirectoryFull) => fail(DIRECTORY_FULL),
        Err(fosfs::Error::DataFull | fosfs::Error::NoRoomToReplace) => fail(DATA_FULL),
        Err(fosfs::Error::InvalidName) => fail(BAD_NAME),
        Err(_) => fail(DISK_ERROR),
    }
}
