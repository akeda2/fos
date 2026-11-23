//! The initiator side: the frames a host sends to use a remote disk, the replies it reads back, and the bookkeeping of one
//! transfer (`Transfer`). No I/O and no clock of its own: the kernel sends what `Transfer::next` builds, feeds it what
//! arrives (`parse`, `Transfer::receive`) and passes its own idea of "now" in whatever unit it counts (ticks in kernel-mf),
//! so the tests here can run it against `Target` over a link that loses and reorders frames.
//!
//! **One transfer** is one block-layer request: `sectors` from `lba`, split into frames of `per_frame` sectors (what the
//! target's Query Config and our MTU allow). Up to `window` frames are in flight (the target's `bufcount`, capped); each
//! carries the tag `tag_base + frame index`, which is how a reply finds its frame. A frame that is not answered within the
//! timeout is sent again with the same tag (doubling the timeout each time), so a late reply to the first copy is as good
//! as one to the second: reads and writes of the same sectors are idempotent. After `max_tries` sends it fails the whole
//! transfer. An ATA error status or an AoE error flag fails it at once.
//!
//! Standard AoE only: no MAC masks, no reserve/release, nothing a Linux `vblade` would not answer.

use crate::{AFLAG_LBA48, AOE, ATA, ATA_FLUSH_EXT, ATA_IDENTIFY, ATA_READ_EXT, ATA_WRITE_EXT, COMMAND_ATA, COMMAND_CONFIG, DATA_AT, ETHERNET, ETHERTYPE};
use crate::{FLAG_ERROR, FLAG_RESPONSE, SECTOR, VERSION};

/// The most frames one transfer keeps in flight, whatever the target's `bufcount` says.
pub const MAX_WINDOW: usize = 64;
const AFLAG_WRITE: u8 = 0x01;
const STATUS_ERR: u8 = 0x01;
const STATUS_BUSY: u8 = 0x80;

/// Who talks to whom: our MAC, the target's, and the target's shelf and slot.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Address {
    pub ours: [u8; 6],
    pub target: [u8; 6],
    pub shelf: u16,
    pub slot: u8,
}

fn header(out: &mut [u8], ours: [u8; 6], target: [u8; 6], shelf: u16, slot: u8, command: u8, tag: u32) {
    out[0..6].copy_from_slice(&target);
    out[6..12].copy_from_slice(&ours);
    out[12..14].copy_from_slice(&ETHERTYPE.to_be_bytes());
    out[14] = VERSION << 4;
    out[15] = 0;
    out[16..18].copy_from_slice(&shelf.to_be_bytes());
    out[18] = slot;
    out[19] = command;
    out[20..24].copy_from_slice(&tag.to_be_bytes());
}

/// A Query Config "read" to `shelf.slot` (0xffff / 0xff for every target) from `ours`, sent to the broadcast address:
/// every target that matches answers with its `bufcount`, sectors per frame and config string. Returns the length.
pub fn query_config(out: &mut [u8], ours: [u8; 6], shelf: u16, slot: u8, tag: u32) -> usize {
    header(out, ours, [0xff; 6], shelf, slot, COMMAND_CONFIG, tag);
    // bufcount, firmware, sectors/frame (all ignored in a request), ver/ccmd 0 = read, config length 0.
    out[ETHERNET + AOE..ETHERNET + AOE + 8].fill(0);
    ETHERNET + AOE + 8
}

/// One ATA command frame: LBA48, `count` sectors, with `data` after the registers for a write. Returns the length.
fn ata_frame(out: &mut [u8], to: &Address, tag: u32, command: u8, aflags: u8, lba: u64, count: u8, data: &[u8]) -> usize {
    header(out, to.ours, to.target, to.shelf, to.slot, COMMAND_ATA, tag);
    let at = ETHERNET + AOE;
    out[at] = aflags;
    out[at + 1] = 0;
    out[at + 2] = count;
    out[at + 3] = command;
    for byte in 0..6 {
        out[at + 4 + byte] = (lba >> (8 * byte)) as u8;
    }
    out[at + 10..at + 12].fill(0);
    out[DATA_AT..DATA_AT + data.len()].copy_from_slice(data);
    DATA_AT + data.len()
}

/// What a target said about itself in a Query Config reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config<'a> {
    pub bufcount: u16,
    pub firmware: u16,
    /// Sectors per frame the target takes (from its MTU); 0 from some old targets, which means 2.
    pub sectors: u8,
    pub config: &'a [u8],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind<'a> {
    Config(Config<'a>),
    /// An ATA reply: the status and error registers and the data that followed (a read's sectors, IDENTIFY's 512 bytes).
    Ata { status: u8, error: u8, data: &'a [u8] },
    /// The E flag: the target refused the command (`ERROR_*`).
    Error(u8),
}

/// A reply a target sent: who sent it, which of its devices, the tag it echoes and what it says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Response<'a> {
    pub source: [u8; 6],
    pub shelf: u16,
    pub slot: u8,
    pub tag: u32,
    pub kind: Kind<'a>,
}

/// Reads one received frame as an AoE reply; `None` for anything else (a request, another ethertype, too short).
pub fn parse(frame: &[u8]) -> Option<Response<'_>> {
    if frame.len() < ETHERNET + AOE || frame[12..14] != ETHERTYPE.to_be_bytes() {
        return None;
    }
    let flags = frame[14];
    if flags & FLAG_RESPONSE == 0 || flags >> 4 != VERSION {
        return None;
    }
    let source = frame[6..12].try_into().unwrap();
    let shelf = u16::from_be_bytes([frame[16], frame[17]]);
    let (slot, command) = (frame[18], frame[19]);
    let tag = u32::from_be_bytes(frame[20..24].try_into().unwrap());
    let body = &frame[ETHERNET + AOE..];
    let kind = if flags & FLAG_ERROR != 0 {
        Kind::Error(frame[15])
    } else if command == COMMAND_CONFIG {
        let length = usize::from(u16::from_be_bytes([*body.get(6)?, *body.get(7)?]));
        Kind::Config(Config {
            bufcount: u16::from_be_bytes([body[0], body[1]]),
            firmware: u16::from_be_bytes([body[2], body[3]]),
            sectors: body[4],
            config: body.get(8..8 + length)?,
        })
    } else if command == COMMAND_ATA {
        if body.len() < ATA {
            return None;
        }
        Kind::Ata { status: body[3], error: body[1], data: &body[ATA..] }
    } else {
        return None;
    };
    Some(Response { source, shelf, slot, tag, kind })
}

/// What IDENTIFY DEVICE says about the remote disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    pub sectors: u64,
    /// The model string, as text (ATA's byte-swapped words undone), space padded.
    pub model: [u8; 40],
}

/// Reads the capacity and model from an IDENTIFY sector: the 48-bit count (words 100-103) when word 83 says LBA48 is
/// supported, else the 28-bit one (words 60-61).
pub fn parse_identify(sector: &[u8]) -> Option<Geometry> {
    if sector.len() < SECTOR {
        return None;
    }
    let word = |index: usize| u64::from(u16::from_le_bytes([sector[index * 2], sector[index * 2 + 1]]));
    let sectors = if word(83) & (1 << 10) != 0 {
        (0..4).fold(0, |total, index| total | word(100 + index) << (16 * index))
    } else {
        word(60) | word(61) << 16
    };
    let mut model = [b' '; 40];
    for pair in 0..20 {
        model[pair * 2] = sector[(27 + pair) * 2 + 1];
        model[pair * 2 + 1] = sector[(27 + pair) * 2];
    }
    Some(Geometry { sectors, model })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Read,
    Write,
    /// FLUSH CACHE EXT: what makes the writes so far durable on the target.
    Flush,
    /// IDENTIFY DEVICE: one frame, 512 bytes back (`parse_identify`).
    Identify,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    /// A frame went unanswered `max_tries` times.
    Timeout,
    /// The target's ATA status had ERR set (a write to a read-only export, a range past the end, a disk error).
    Device,
    /// The target refused the AoE command (`ERROR_*`).
    Refused(u8),
    /// A read reply carried fewer bytes than the frame asked for.
    Short,
}

#[derive(Clone, Copy, Default)]
struct Slot {
    busy: bool,
    frame: u64,
    tries: u8,
    deadline: u64,
}

/// One block-layer request on the wire. See the module comment.
pub struct Transfer {
    op: Op,
    lba: u64,
    sectors: u64,
    per_frame: u64,
    window: usize,
    tag_base: u32,
    frames: u64,
    next: u64,
    done: u64,
    slots: [Slot; MAX_WINDOW],
    timeout: u64,
    max_tries: u8,
    failed: Option<Failure>,
    /// Frames sent again after a timeout, for the counters.
    pub resent: u32,
}

/// How many frames a transfer of `op` puts on the wire (without resends): what the caller reserves tags for.
pub fn frames_for(op: Op, sectors: u64, per_frame: usize) -> u64 {
    match op {
        Op::Read | Op::Write => sectors.div_ceil(per_frame.clamp(1, 255) as u64),
        Op::Flush | Op::Identify => 1,
    }
}

impl Transfer {
    /// `sectors` from `lba` (Read and Write; Flush and Identify ignore both). `per_frame` and `window` are clamped to
    /// at least 1 and at most 255 and `MAX_WINDOW`. `timeout` is in the caller's time unit, the first wait before a resend.
    pub fn new(op: Op, lba: u64, sectors: u64, per_frame: usize, window: usize, tag_base: u32, timeout: u64, max_tries: u8) -> Transfer {
        let frames = frames_for(op, sectors, per_frame);
        let per_frame = per_frame.clamp(1, 255) as u64;
        Transfer {
            op,
            lba,
            sectors,
            per_frame,
            window: window.clamp(1, MAX_WINDOW),
            tag_base,
            frames,
            next: 0,
            done: 0,
            slots: [Slot::default(); MAX_WINDOW],
            timeout: timeout.max(1),
            max_tries: max_tries.max(1),
            failed: None,
            resent: 0,
        }
    }

    /// Frames this transfer puts on the wire (without resends): the caller advances its tag counter by this much.
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// `Some` once every frame is answered (`Ok`) or one has failed.
    pub fn finished(&self) -> Option<Result<(), Failure>> {
        match self.failed {
            Some(failure) => Some(Err(failure)),
            None if self.done == self.frames => Some(Ok(())),
            None => None,
        }
    }

    /// The earliest time a frame in flight is due for a resend, if any is in flight.
    pub fn deadline(&self) -> Option<u64> {
        self.slots[..self.window].iter().filter(|slot| slot.busy).map(|slot| slot.deadline).min()
    }

    fn sectors_of(&self, frame: u64) -> u64 {
        match self.op {
            Op::Read | Op::Write => self.per_frame.min(self.sectors - frame * self.per_frame),
            Op::Flush => 0,
            Op::Identify => 1,
        }
    }

    fn build(&self, frame: u64, to: &Address, data: &[u8], out: &mut [u8]) -> usize {
        let tag = self.tag_base.wrapping_add(frame as u32);
        let count = self.sectors_of(frame);
        let lba = self.lba + frame * self.per_frame;
        match self.op {
            Op::Read => ata_frame(out, to, tag, ATA_READ_EXT, AFLAG_LBA48, lba, count as u8, &[]),
            Op::Write => {
                let start = (frame * self.per_frame) as usize * SECTOR;
                ata_frame(out, to, tag, ATA_WRITE_EXT, AFLAG_LBA48 | AFLAG_WRITE, lba, count as u8, &data[start..start + count as usize * SECTOR])
            }
            Op::Flush => ata_frame(out, to, tag, ATA_FLUSH_EXT, AFLAG_LBA48, 0, 0, &[]),
            Op::Identify => ata_frame(out, to, tag, ATA_IDENTIFY, 0, 0, 1, &[]),
        }
    }

    /// The next frame to put on the wire at `now`, built into `out` (which must hold `DATA_AT + per_frame * 512`): a frame
    /// whose reply is overdue first (or the failure, when it has had its tries), else a new frame while the window has room.
    /// `data` is the whole write buffer (ignored by the other operations). 0 when there is nothing to send now.
    pub fn next(&mut self, now: u64, to: &Address, data: &[u8], out: &mut [u8]) -> usize {
        self.next_limited(now, to, data, out, true)
    }

    /// Frames sent and not yet answered.
    pub fn in_flight(&self) -> usize {
        self.slots[..self.window].iter().filter(|slot| slot.busy).count()
    }

    /// `next`, but a new frame only when `may_start`; an overdue frame is resent either way. Lets several transfers share
    /// one window (`readahead::ReadAhead`): the caller says whether the shared window has room.
    pub fn next_limited(&mut self, now: u64, to: &Address, data: &[u8], out: &mut [u8], may_start: bool) -> usize {
        if self.failed.is_some() {
            return 0;
        }
        for index in 0..self.window {
            let slot = self.slots[index];
            if slot.busy && slot.deadline <= now {
                if slot.tries >= self.max_tries {
                    self.failed = Some(Failure::Timeout);
                    return 0;
                }
                let wait = self.timeout << slot.tries.min(16);
                self.slots[index].tries += 1;
                self.slots[index].deadline = now + wait;
                self.resent += 1;
                return self.build(slot.frame, to, data, out);
            }
        }
        if may_start && self.next < self.frames {
            if let Some(index) = (0..self.window).find(|&index| !self.slots[index].busy) {
                let frame = self.next;
                self.next += 1;
                self.slots[index] = Slot { busy: true, frame, tries: 1, deadline: now + self.timeout };
                return self.build(frame, to, data, out);
            }
        }
        0
    }

    /// Takes a reply: true when it belonged to a frame of this transfer still in flight (a duplicate, a late reply to a
    /// resent frame or one from another device is ignored and gives false). A read's sectors go into `buffer`, which is
    /// the whole read buffer (512 bytes for Identify).
    pub fn receive(&mut self, to: &Address, response: &Response, buffer: &mut [u8]) -> bool {
        self.take(to, response, Some(buffer))
    }

    /// Takes a reply as `receive` does but keeps none of its data: for a transfer the caller has given up on whose frames
    /// are still in flight (`readahead`), so they are counted as answered without a buffer to put them in.
    pub fn discard(&mut self, to: &Address, response: &Response) -> bool {
        self.take(to, response, None)
    }

    fn take(&mut self, to: &Address, response: &Response, buffer: Option<&mut [u8]>) -> bool {
        if response.source != to.target || response.shelf != to.shelf || response.slot != to.slot || self.failed.is_some() {
            return false;
        }
        let frame = u64::from(response.tag.wrapping_sub(self.tag_base));
        if frame >= self.frames {
            return false;
        }
        let Some(index) = (0..self.window).find(|&index| self.slots[index].busy && self.slots[index].frame == frame) else { return false };
        self.slots[index].busy = false;
        self.done += 1;
        match response.kind {
            Kind::Error(code) => self.failed = Some(Failure::Refused(code)),
            Kind::Config(_) => {
                // Not an answer to an ATA command: give the slot back so the frame is resent.
                self.slots[index].busy = true;
                self.done -= 1;
                return false;
            }
            Kind::Ata { status, .. } if status & (STATUS_ERR | STATUS_BUSY) != 0 => self.failed = Some(Failure::Device),
            Kind::Ata { data, .. } => {
                let bytes = self.sectors_of(frame) as usize * SECTOR;
                if matches!(self.op, Op::Read | Op::Identify) {
                    if data.len() < bytes {
                        self.failed = Some(Failure::Short);
                    } else if let Some(buffer) = buffer {
                        let start = if self.op == Op::Read { (frame * self.per_frame) as usize * SECTOR } else { 0 };
                        buffer[start..start + bytes].copy_from_slice(&data[..bytes]);
                    }
                }
            }
        }
        true
    }
}
