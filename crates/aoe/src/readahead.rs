//! Read-ahead for the initiator: keeps the window full between a caller's reads (docs/block-sharing.md, "Read-ahead").
//!
//! Measured on hardware: raw reads in 128 KiB requests reached 90 MB/s against 113 in 1 MiB ones, and ext4 asks for
//! about 122 KB at a time. A lone transfer starts with an empty window (a round trip before the first reply), ends by
//! draining it, and between two requests nothing is on the wire at all.
//!
//! A read that continues the previous one, or starts in what was read ahead, is a stream: besides the sectors asked for,
//! chunks past its end are requested, each a `Transfer` of at most `chunk_sectors` into its own part of a store the
//! caller owns. All transfers share one window: the frames the read needs go first, read-ahead takes what room is left.
//! The read returns as soon as its own sectors are in. Between reads the caller may go on driving the chunks (`active`,
//! then `next` and `receive` with an empty buffer), as the server kernel's `aoe-ahead` thread does while the reader
//! works on what it got; the next read in the stream then finds them done.
//!
//! **How much.** The second read of a stream asks for twice its own size ahead, and each further one twice as much as
//! the one before, up to `CHUNKS` chunks (as Linux's read-ahead grows its window). A short run of sequential reads, as a
//! file system's metadata gives, then costs a few frames, not 256 KiB.
//!
//! A read elsewhere (ext4 looking up an inode or an extent block) is a plain transfer and leaves the chunks alone, so
//! the stream picks up again after it; random reads fetch nothing extra. A read elsewhere that is itself sequential
//! starts a new stream and drops the old chunks.
//!
//! **Dropped transfers with frames in flight** are kept as orphans until those frames are answered or due: the target
//! still holds them in its queue, which is the window (`bufcount`), so they count against the window and new frames do
//! not overflow that queue and get dropped there. Before a write the caller waits for them (`drain`). Seen under QEMU
//! before this was done: a write burst sent straight after dropping read-ahead lost frames to the target's full queue
//! and waited for resends.
//!
//! **Staleness.** A chunk can hold sectors the target's disk has changed since (another host writing it). Chunks are
//! dropped when the stream has not been used for more than `stale_after` time units, the bound the target's own cache
//! has too (`cache.rs`), and the caller drops them (`clear`) on each write or flush of its own and after a failure.
//!
//! `no_std`, no allocation: the caller passes the store (`store_bytes()` long) to the calls that touch data; time is
//! in the caller's unit, as for `Transfer`.

use crate::initiator::{frames_for, Address, Failure, Op, Response, Transfer};
use crate::SECTOR;

/// Chunks read ahead at most; with a 64-frame window of 2-sector frames each is 64 KiB, so 256 KiB in all.
pub const CHUNKS: usize = 4;
/// Dropped transfers waiting for their frames in flight: every chunk and the read itself.
const ORPHANS: usize = CHUNKS + 1;
/// The largest chunk, whatever the window and frame size (jumbo frames would otherwise make the store large).
const MAX_CHUNK_SECTORS: u64 = 256;

struct Chunk {
    lba: u64,
    sectors: u64,
    /// Which `chunk_bytes` part of the store it fills.
    slot: usize,
    transfer: Transfer,
}

impl Chunk {
    fn end(&self) -> u64 {
        self.lba + self.sectors
    }
}

/// The read-ahead state of one remote disk. See the module comment.
pub struct ReadAhead {
    per_frame: usize,
    window: usize,
    timeout: u64,
    max_tries: u8,
    chunk_sectors: u64,
    disk_sectors: u64,
    stale_after: u64,
    /// In LBA order from index 0, the `Some`s first; contiguous between reads.
    chunks: [Option<Chunk>; CHUNKS],
    orphans: [Option<Transfer>; ORPHANS],
    /// The part of the current read no chunk covers, read straight into the caller's buffer from `main_offset` on.
    main: Option<Transfer>,
    main_lba: u64,
    main_offset: usize,
    /// The read being served, and whether it starts in the chunks (only then does it take sectors from them).
    lba: u64,
    end: u64,
    hit: bool,
    /// Waiting for the orphans only (`drain`).
    draining: bool,
    /// Where the previous read ended, to recognise the next one continuing it.
    last_end: Option<u64>,
    /// Reads in the current stream after its first: how far ahead to ask.
    run: u32,
    /// When the stream was last used (a hit or new chunks).
    touched: u64,
    /// Resends of transfers already gone (`resent` adds the live ones).
    retired_resent: u64,
    /// Sectors requested ahead, and sectors a read took from them.
    pub fetched: u64,
    pub used: u64,
}

impl ReadAhead {
    /// For a disk of `disk_sectors` reached with frames of `per_frame` sectors and a window of `window` frames. `timeout`
    /// and `max_tries` are passed to each `Transfer`; `stale_after` is in the same unit.
    pub fn new(per_frame: usize, window: usize, timeout: u64, max_tries: u8, disk_sectors: u64, stale_after: u64) -> ReadAhead {
        let per_frame = per_frame.clamp(1, 255);
        let window = window.clamp(1, crate::initiator::MAX_WINDOW);
        ReadAhead {
            per_frame,
            window,
            timeout,
            max_tries,
            // One window of frames: a chunk is what can be in flight at once.
            chunk_sectors: ((window * per_frame) as u64).clamp(1, MAX_CHUNK_SECTORS),
            disk_sectors,
            stale_after,
            chunks: [const { None }; CHUNKS],
            orphans: [const { None }; ORPHANS],
            main: None,
            main_lba: 0,
            main_offset: 0,
            lba: 0,
            end: 0,
            hit: false,
            draining: false,
            last_end: None,
            run: 0,
            touched: 0,
            retired_resent: 0,
            fetched: 0,
            used: 0,
        }
    }

    /// How long the store passed to `receive` and `finish` must be.
    pub fn store_bytes(&self) -> usize {
        CHUNKS * self.chunk_bytes()
    }

    fn chunk_bytes(&self) -> usize {
        self.chunk_sectors as usize * SECTOR
    }

    fn len(&self) -> usize {
        self.chunks.iter().take_while(|chunk| chunk.is_some()).count()
    }

    fn chunks_end(&self) -> Option<u64> {
        self.chunks.iter().flatten().last().map(Chunk::end)
    }

    /// A transfer that is no longer wanted: an orphan while it has frames in flight, else gone.
    fn retire(&mut self, transfer: Transfer) {
        if transfer.in_flight() == 0 || transfer.finished().is_some() {
            self.retired_resent += u64::from(transfer.resent);
            return;
        }
        if self.orphans.iter().all(Option::is_some) {
            // Full (not expected: there are as many places as transfers): the oldest goes.
            let oldest = self.orphans[0].take().expect("full");
            self.retired_resent += u64::from(oldest.resent);
            self.orphans.rotate_left(1);
        }
        let free = self.orphans.iter().position(Option::is_none).expect("one is free");
        self.orphans[free] = Some(transfer);
    }

    /// Lets orphans go once their frames are answered or overdue (then the target has served or lost them).
    fn prune(&mut self, now: Option<u64>) {
        for index in 0..ORPHANS {
            let gone = self.orphans[index].as_ref().is_some_and(|orphan| {
                orphan.in_flight() == 0 || orphan.finished().is_some() || now.is_some_and(|now| orphan.deadline().is_none_or(|due| due <= now))
            });
            if gone {
                let orphan = self.orphans[index].take().expect("checked");
                self.retired_resent += u64::from(orphan.resent);
            }
        }
    }

    /// Drops chunk `at` and every one after it.
    fn drop_from(&mut self, at: usize) {
        for index in at..CHUNKS {
            if let Some(chunk) = self.chunks[index].take() {
                self.retire(chunk.transfer);
            }
        }
    }

    fn drop_front(&mut self) {
        if let Some(chunk) = self.chunks[0].take() {
            self.retire(chunk.transfer);
        }
        self.chunks.rotate_left(1);
    }

    /// Forgets everything read ahead and the stream: after a write, a flush or a failed read. Frames in flight are still
    /// counted until answered; `drain` waits for them.
    pub fn clear(&mut self) {
        self.drop_from(0);
        if let Some(main) = self.main.take() {
            self.retire(main);
        }
        self.last_end = None;
        self.run = 0;
    }

    /// Frames of dropped transfers still in flight.
    pub fn orphaned(&self) -> usize {
        self.orphans.iter().flatten().map(Transfer::in_flight).sum()
    }

    /// After `clear`: `next`/`receive` until `finished` then wait only for the orphans' frames (nothing is sent), so the
    /// next transfer has the target's whole queue.
    pub fn drain(&mut self) {
        self.clear();
        self.draining = true;
        self.hit = false;
    }

    /// Whether anything is still to be sent or answered: read-ahead not yet in, or orphans. Between reads the caller can
    /// keep calling `next` and `receive` (with an empty buffer) while this holds, so read-ahead goes on while it works.
    pub fn active(&self) -> bool {
        self.chunks.iter().flatten().any(|chunk| chunk.transfer.finished().is_none()) || self.orphans.iter().any(Option::is_some)
    }

    /// Frames resent so far by every transfer this has run, for the caller's counters.
    pub fn resent(&self) -> u64 {
        let chunks = self.chunks.iter().flatten().map(|chunk| u64::from(chunk.transfer.resent)).sum::<u64>();
        let orphans = self.orphans.iter().flatten().map(|orphan| u64::from(orphan.resent)).sum::<u64>();
        self.retired_resent + chunks + orphans + self.main.as_ref().map_or(0, |main| u64::from(main.resent))
    }

    /// Starts a read of `sectors` from `lba` at time `now`. `tags(frames)` reserves that many tags and returns the first.
    /// Then `next`/`receive` until `finished`, then `finish` (or `clear` on a failure).
    pub fn begin(&mut self, lba: u64, sectors: u64, now: u64, tags: &mut dyn FnMut(u64) -> u32) {
        self.draining = false;
        if let Some(main) = self.main.take() {
            self.retire(main);
        }
        self.prune(Some(now));
        if self.len() > 0 && now.saturating_sub(self.touched) > self.stale_after {
            self.drop_from(0);
        }
        // Read-ahead that failed between reads (the target was away for a while) is not this read's failure: drop it and
        // let the read ask again.
        if let Some(failed) = self.chunks.iter().position(|chunk| chunk.as_ref().is_some_and(|chunk| matches!(chunk.transfer.finished(), Some(Err(_))))) {
            self.drop_from(failed);
        }
        let end = lba + sectors;
        let hit = self.chunks[0].as_ref().is_some_and(|front| lba >= front.lba) && self.chunks_end().is_some_and(|last| lba < last);
        let sequential = self.last_end == Some(lba);
        if hit {
            while self.chunks[0].as_ref().is_some_and(|front| front.end() <= lba) {
                self.drop_front();
            }
            self.run += 1;
        } else if sequential {
            // A new stream: the old one's chunks will not be wanted.
            self.drop_from(0);
            self.run = 1;
        }
        let covered = if hit { self.chunks_end().map_or(lba, |last| last.min(end)) } else { lba };
        if covered < end {
            let rest = end - covered;
            let base = tags(frames_for(Op::Read, rest, self.per_frame));
            self.main = Some(Transfer::new(Op::Read, covered, rest, self.per_frame, self.window, base, self.timeout, self.max_tries));
            self.main_lba = covered;
            self.main_offset = (covered - lba) as usize * SECTOR;
        }
        if hit || sequential {
            self.touched = now;
            // Twice the read, doubling with each read of the stream, at most the store.
            let ahead = (sectors.max(1) * 2).saturating_mul(1 << (self.run - 1).min(16)).min(CHUNKS as u64 * self.chunk_sectors);
            let limit = (end + ahead).min(self.disk_sectors);
            let mut at = self.chunks_end().map_or(end, |last| last.max(end));
            while self.len() < CHUNKS && at < limit {
                let sectors = self.chunk_sectors.min(limit - at);
                let Some(slot) = (0..CHUNKS).find(|&slot| !self.chunks.iter().flatten().any(|chunk| chunk.slot == slot)) else { break };
                let base = tags(frames_for(Op::Read, sectors, self.per_frame));
                let transfer = Transfer::new(Op::Read, at, sectors, self.per_frame, self.window, base, self.timeout, self.max_tries);
                let free = self.len();
                self.chunks[free] = Some(Chunk { lba: at, sectors, slot, transfer });
                self.fetched += sectors;
                at += sectors;
            }
        }
        self.lba = lba;
        self.end = end;
        self.hit = hit;
        self.last_end = Some(end);
    }

    fn needed(&self, chunk: &Chunk) -> bool {
        self.hit && chunk.lba < self.end && chunk.end() > self.lba
    }

    /// The next frame to send, as `Transfer::next`. The read's own frames come first (chunks before `main`, `main`, then
    /// read-ahead); a new frame goes out only while the shared window, orphans included, has room, a resend whenever it is
    /// due. Orphans are never sent again.
    pub fn next(&mut self, now: u64, to: &Address, out: &mut [u8]) -> usize {
        self.prune(Some(now));
        let in_flight = self.chunks.iter().flatten().map(|chunk| chunk.transfer.in_flight()).sum::<usize>()
            + self.main.as_ref().map_or(0, Transfer::in_flight)
            + self.orphaned();
        let room = in_flight < self.window;
        let mut main_tried = self.main.is_none();
        for index in 0..CHUNKS {
            let Some(chunk) = self.chunks[index].as_mut() else { break };
            if !main_tried && chunk.lba >= self.main_lba {
                main_tried = true;
                let length = self.main.as_mut().map_or(0, |main| main.next_limited(now, to, &[], out, room));
                if length != 0 {
                    return length;
                }
            }
            let chunk = self.chunks[index].as_mut().expect("checked above");
            let length = chunk.transfer.next_limited(now, to, &[], out, room);
            if length != 0 {
                return length;
            }
        }
        if !main_tried {
            return self.main.as_mut().map_or(0, |main| main.next_limited(now, to, &[], out, room));
        }
        0
    }

    /// Takes a reply, as `Transfer::receive`: into `buffer` (the caller's read buffer) or a chunk's part of `store`, or
    /// counted for an orphan.
    pub fn receive(&mut self, to: &Address, response: &Response, buffer: &mut [u8], store: &mut [u8]) -> bool {
        if let Some(main) = self.main.as_mut() {
            if main.receive(to, response, &mut buffer[self.main_offset..]) {
                return true;
            }
        }
        let bytes = self.chunk_bytes();
        for chunk in self.chunks.iter_mut().flatten() {
            let at = chunk.slot * bytes;
            if chunk.transfer.receive(to, response, &mut store[at..at + chunk.sectors as usize * SECTOR]) {
                return true;
            }
        }
        if self.orphans.iter_mut().flatten().any(|orphan| orphan.discard(to, response)) {
            self.prune(None);
            return true;
        }
        false
    }

    /// `Some` once every sector of the read is in (`Ok`) or a transfer it needs has failed. Read-ahead past it may still
    /// be in flight. While draining: once no orphan is left.
    pub fn finished(&self) -> Option<Result<(), Failure>> {
        if self.draining {
            return self.orphans.iter().all(Option::is_none).then_some(Ok(()));
        }
        let mut pending = false;
        let needed = self.chunks.iter().flatten().filter(|chunk| self.needed(chunk)).map(|chunk| &chunk.transfer);
        for transfer in needed.chain(self.main.as_ref()) {
            match transfer.finished() {
                Some(Err(failure)) => return Some(Err(failure)),
                Some(Ok(())) => {}
                None => pending = true,
            }
        }
        if pending { None } else { Some(Ok(())) }
    }

    /// The earliest time a frame in flight is due for a resend (or an orphan's frame for letting it go).
    pub fn deadline(&self) -> Option<u64> {
        let chunks = self.chunks.iter().flatten().map(|chunk| &chunk.transfer);
        chunks.chain(self.main.as_ref()).chain(self.orphans.iter().flatten()).filter_map(Transfer::deadline).min()
    }

    /// After `finished` gave `Ok`: copies the read's sectors that chunks hold into `buffer` and drops the chunks it used
    /// up, and any read-ahead that failed (with the chunks after it).
    pub fn finish(&mut self, buffer: &mut [u8], store: &[u8]) {
        if self.draining {
            self.draining = false;
            return;
        }
        let bytes = self.chunk_bytes();
        let mut used = 0;
        for chunk in self.chunks.iter().flatten().filter(|chunk| self.needed(chunk)) {
            let from = chunk.lba.max(self.lba);
            let to = chunk.end().min(self.end);
            let source = chunk.slot * bytes + (from - chunk.lba) as usize * SECTOR;
            let target = (from - self.lba) as usize * SECTOR;
            let length = (to - from) as usize * SECTOR;
            buffer[target..target + length].copy_from_slice(&store[source..source + length]);
            used += to - from;
        }
        self.used += used;
        if let Some(main) = self.main.take() {
            self.retire(main);
        }
        while self.hit && self.chunks[0].as_ref().is_some_and(|front| front.end() <= self.end) {
            self.drop_front();
        }
        if let Some(failed) = self.chunks.iter().position(|chunk| chunk.as_ref().is_some_and(|chunk| matches!(chunk.transfer.finished(), Some(Err(_))))) {
            self.drop_from(failed);
        }
    }
}
