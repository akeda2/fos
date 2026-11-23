//! ATA over Ethernet, revision 1.0 (ethertype 0x88a2): the frame format, a *target*, the side that serves a disk, and
//! the *initiator*'s frames and transfer bookkeeping (`initiator`), the side that uses one, with its read-ahead
//! (`readahead`).
//!
//! `no_std`, no allocation, no I/O: `Target::handle` takes one received frame, asks a [`Disk`] for sectors and writes
//! the reply frame into a buffer. The kernel wires `Disk` to its block layer and the frame hook to the NIC; the tests
//! here wire it to RAM. docs/block-sharing.md has the design.
//!
//! A frame is the 14-byte Ethernet header, a 10-byte AoE header, then the command:
//!
//! ```text
//! AoE:   ver(4) flags(4) | error | major (be16) | minor | command | tag (be32)
//!        flags: R = response (0x8), E = error (0x4)
//! ATA:   aflags | err/feature | count | cmd/status | lba0..lba5 (lba0 is the low byte) | 2 reserved | data
//!        aflags: E = 48-bit LBA (0x40), D = device bit (0x10), A = asynchronous (0x02), W = write (0x01)
//! Query Config (command 1): bufcount (be16) | firmware (be16) | sectors/frame | ver(4) ccmd(4) | cfg length (be16) | cfg
//! ```
//!
//! Everything is 512-byte sectors: the protocol has no other unit (Linux's `aoe` initiator assumes it). What fits in one
//! frame follows the MTU: 2 sectors at 1500, 17 at 9000 (`max_sectors`).

#![no_std]

pub mod cache;
pub mod initiator;
pub mod readahead;

pub const ETHERTYPE: u16 = 0x88a2;
pub const SECTOR: usize = 512;
const ETHERNET: usize = 14;
const AOE: usize = 10;
const ATA: usize = 12;
/// Where the sector data starts in an ATA frame.
pub const DATA_AT: usize = ETHERNET + AOE + ATA;
/// The most the Query Config string can hold here (the protocol allows 1024).
pub const CONFIG_MAX: usize = 64;

const VERSION: u8 = 1;
const FLAG_RESPONSE: u8 = 0x08;
const FLAG_ERROR: u8 = 0x04;
const BROADCAST_SHELF: u16 = 0xffff;
const BROADCAST_SLOT: u8 = 0xff;

pub const COMMAND_ATA: u8 = 0;
pub const COMMAND_CONFIG: u8 = 1;

/// AoE error codes (the `error` byte when the E flag is set).
pub const ERROR_UNKNOWN_COMMAND: u8 = 1;
pub const ERROR_BAD_ARGUMENT: u8 = 2;
pub const ERROR_UNAVAILABLE: u8 = 3;
pub const ERROR_CONFIG_PRESENT: u8 = 4;
pub const ERROR_BAD_VERSION: u8 = 5;

const AFLAG_LBA48: u8 = 0x40;

/// ATA commands a target answers.
const ATA_READ: u8 = 0x20;
const ATA_READ_EXT: u8 = 0x24;
const ATA_WRITE: u8 = 0x30;
const ATA_WRITE_EXT: u8 = 0x34;
const ATA_FLUSH: u8 = 0xe7;
const ATA_FLUSH_EXT: u8 = 0xea;
const ATA_IDENTIFY: u8 = 0xec;

/// ATA status: device ready, seek complete; and the same with ERR set (error register ABRT: command aborted).
const STATUS_OK: u8 = 0x50;
const STATUS_ERROR: u8 = 0x51;
const ERR_ABORT: u8 = 0x04;

/// The disk a target serves, in 512-byte sectors.
pub trait Disk {
    fn sectors(&self) -> u64;
    /// Reads `buffer.len() / 512` sectors from `lba`. False when the disk fails.
    fn read(&mut self, lba: u64, buffer: &mut [u8]) -> bool;
    /// Writes `data.len() / 512` sectors at `lba`. May stay in a cache until `flush`. False when the disk fails.
    fn write(&mut self, lba: u64, data: &[u8]) -> bool;
    fn flush(&mut self) -> bool;
}

/// How many 512-byte sectors one frame carries at `mtu` (the Ethernet payload size): what a target advertises, and the
/// most an initiator may ask for in one command. 2 at 1500, 17 at 9000; never more than the 8-bit count allows.
pub fn max_sectors(mtu: usize) -> usize {
    (mtu.saturating_sub(AOE + ATA) / SECTOR).min(255)
}

/// One exported device: its address (shelf and slot), what it says about itself and the settings of the export.
pub struct Target {
    pub shelf: u16,
    pub slot: u8,
    pub mac: [u8; 6],
    /// The Ethernet payload size in force on the NIC (`max_sectors` follows from it).
    pub mtu: usize,
    /// Writes are refused (ATA abort) while this is set. The kernel's export command defaults to it.
    pub read_only: bool,
    /// Commands the initiator may have outstanding (its window), as advertised in Query Config.
    pub bufcount: u16,
    pub firmware: u16,
    pub model: [u8; 40],
    pub serial: [u8; 20],
    pub revision: [u8; 8],
    config: [u8; CONFIG_MAX],
    config_len: usize,
    /// Frames answered, by kind: handy for a shell `aoe` listing and the tests.
    pub reads: u64,
    pub writes: u64,
    pub errors: u64,
}

impl Target {
    pub fn new(shelf: u16, slot: u8, mac: [u8; 6], mtu: usize) -> Target {
        let mut target = Target {
            shelf,
            slot,
            mac,
            mtu,
            read_only: true,
            bufcount: 16,
            firmware: 1,
            model: [b' '; 40],
            serial: [b' '; 20],
            revision: [b' '; 8],
            config: [0; CONFIG_MAX],
            config_len: 0,
            reads: 0,
            writes: 0,
            errors: 0,
        };
        set_text(&mut target.model, b"fosfor-os aoe");
        set_text(&mut target.revision, b"1");
        target
    }

    pub fn config(&self) -> &[u8] {
        &self.config[..self.config_len]
    }

    /// Answers one received frame into `out`; returns the reply's length, 0 for no reply (not AoE, not for this device,
    /// a response, a command a broadcast must not carry). `out` must hold a whole frame (`DATA_AT + 255 * 512` at most).
    pub fn handle(&mut self, frame: &[u8], disk: &mut dyn Disk, out: &mut [u8]) -> usize {
        if frame.len() < ETHERNET + AOE || frame[12..14] != ETHERTYPE.to_be_bytes() || out.len() < DATA_AT {
            return 0;
        }
        let flags = frame[14];
        if flags & FLAG_RESPONSE != 0 || frame[6] & 1 != 0 {
            return 0;
        }
        let shelf = u16::from_be_bytes([frame[16], frame[17]]);
        let (slot, command) = (frame[18], frame[19]);
        if !(shelf == self.shelf || shelf == BROADCAST_SHELF) || !(slot == self.slot || slot == BROADCAST_SLOT) {
            return 0;
        }
        let broadcast = shelf == BROADCAST_SHELF || slot == BROADCAST_SLOT;
        if flags >> 4 != VERSION {
            return self.error_reply(frame, out, ERROR_BAD_VERSION);
        }
        match command {
            COMMAND_CONFIG => self.config_command(frame, out),
            // An ATA command to "everyone" would be answered by every device: not allowed.
            COMMAND_ATA if broadcast => 0,
            COMMAND_ATA => self.ata_command(frame, disk, out),
            _ => self.error_reply(frame, out, ERROR_UNKNOWN_COMMAND),
        }
    }

    /// The Ethernet and AoE headers of a reply to `request` (response flag set), 24 bytes.
    fn reply_header(&self, request: &[u8], out: &mut [u8], flags: u8, error: u8) -> usize {
        out[0..6].copy_from_slice(&request[6..12]);
        out[6..12].copy_from_slice(&self.mac);
        out[12..14].copy_from_slice(&ETHERTYPE.to_be_bytes());
        out[14] = (VERSION << 4) | FLAG_RESPONSE | flags;
        out[15] = error;
        // Always our own address, also when the request was a broadcast: that is how an initiator learns it.
        out[16..18].copy_from_slice(&self.shelf.to_be_bytes());
        out[18] = self.slot;
        out[19] = request[19];
        out[20..24].copy_from_slice(&request[20..24]);
        ETHERNET + AOE
    }

    fn error_reply(&mut self, request: &[u8], out: &mut [u8], code: u8) -> usize {
        self.errors += 1;
        self.reply_header(request, out, FLAG_ERROR, code)
    }

    fn config_command(&mut self, frame: &[u8], out: &mut [u8]) -> usize {
        if frame.len() < ETHERNET + AOE + 8 {
            return self.error_reply(frame, out, ERROR_BAD_ARGUMENT);
        }
        let body = &frame[ETHERNET + AOE..];
        let ccmd = body[5] & 0x0f;
        let length = usize::from(u16::from_be_bytes([body[6], body[7]]));
        let Some(wanted) = body.get(8..8 + length) else { return self.error_reply(frame, out, ERROR_BAD_ARGUMENT) };
        match ccmd {
            0 => {}
            // Test for an exact match / a prefix: only the devices that match answer.
            1 if self.config() != wanted => return 0,
            2 if !self.config().starts_with(wanted) => return 0,
            1 | 2 => {}
            3 | 4 => {
                if ccmd == 3 && self.config_len != 0 {
                    return self.error_reply(frame, out, ERROR_CONFIG_PRESENT);
                }
                if length > CONFIG_MAX {
                    return self.error_reply(frame, out, ERROR_BAD_ARGUMENT);
                }
                self.config[..length].copy_from_slice(wanted);
                self.config_len = length;
            }
            _ => return self.error_reply(frame, out, ERROR_BAD_ARGUMENT),
        }
        let end = self.reply_header(frame, out, 0, 0);
        out[end..end + 2].copy_from_slice(&self.bufcount.to_be_bytes());
        out[end + 2..end + 4].copy_from_slice(&self.firmware.to_be_bytes());
        out[end + 4] = max_sectors(self.mtu) as u8;
        out[end + 5] = (VERSION << 4) | ccmd;
        out[end + 6..end + 8].copy_from_slice(&(self.config_len as u16).to_be_bytes());
        out[end + 8..end + 8 + self.config_len].copy_from_slice(&self.config[..self.config_len]);
        end + 8 + self.config_len
    }

    fn ata_command(&mut self, frame: &[u8], disk: &mut dyn Disk, out: &mut [u8]) -> usize {
        if frame.len() < DATA_AT {
            return self.error_reply(frame, out, ERROR_BAD_ARGUMENT);
        }
        let aflags = frame[24];
        let (count, command) = (usize::from(frame[26]), frame[27]);
        let lba_bytes = &frame[28..34];
        let lba = if aflags & AFLAG_LBA48 != 0 {
            lba_bytes.iter().rev().fold(0u64, |lba, &byte| lba << 8 | u64::from(byte))
        } else {
            // 28 bits: the fourth byte's low nibble is bits 24..27 (its high nibble is the device register).
            u64::from(lba_bytes[0]) | u64::from(lba_bytes[1]) << 8 | u64::from(lba_bytes[2]) << 16 | u64::from(lba_bytes[3] & 0x0f) << 24
        };
        let data = &frame[DATA_AT..];
        // The reply starts as the request's ATA header with a status; a command fills `status`, `error`, the data.
        let mut status = STATUS_OK;
        let mut error = 0;
        let mut length = 0;
        let in_range = |sectors: usize| count != 0 && lba.checked_add(sectors as u64).is_some_and(|end| end <= disk.sectors());
        let abort = |status: &mut u8, error: &mut u8| {
            *status = STATUS_ERROR;
            *error = ERR_ABORT;
        };
        match command {
            ATA_IDENTIFY => {
                if out.len() < DATA_AT + SECTOR {
                    abort(&mut status, &mut error);
                } else {
                    identify(self, disk.sectors(), &mut out[DATA_AT..DATA_AT + SECTOR]);
                    length = SECTOR;
                }
            }
            ATA_READ | ATA_READ_EXT => {
                if count > max_sectors(self.mtu) || !in_range(count) || out.len() < DATA_AT + count * SECTOR || !disk.read(lba, &mut out[DATA_AT..DATA_AT + count * SECTOR]) {
                    abort(&mut status, &mut error);
                } else {
                    length = count * SECTOR;
                    self.reads += 1;
                }
            }
            ATA_WRITE | ATA_WRITE_EXT => {
                if self.read_only || !in_range(count) || data.len() < count * SECTOR || !disk.write(lba, &data[..count * SECTOR]) {
                    abort(&mut status, &mut error);
                } else {
                    self.writes += 1;
                }
            }
            ATA_FLUSH | ATA_FLUSH_EXT => {
                if !self.read_only && !disk.flush() {
                    abort(&mut status, &mut error);
                }
            }
            _ => abort(&mut status, &mut error),
        }
        if status != STATUS_OK {
            self.errors += 1;
        }
        let end = self.reply_header(frame, out, 0, 0);
        out[end] = aflags;
        out[end + 1] = error;
        out[end + 2] = frame[26];
        out[end + 3] = status;
        out[end + 4..end + 10].copy_from_slice(lba_bytes);
        out[end + 10..end + 12].fill(0);
        end + ATA + length
    }
}

/// Copies `text` into an ATA identify string field, space padded.
pub fn set_text(field: &mut [u8], text: &[u8]) {
    field.fill(b' ');
    let length = text.len().min(field.len());
    field[..length].copy_from_slice(&text[..length]);
}

/// ATA strings are stored two characters to a word with the first character in the high byte.
fn put_string(sector: &mut [u8], first_word: usize, text: &[u8]) {
    for (pair, chunk) in text.chunks(2).enumerate() {
        let at = (first_word + pair) * 2;
        sector[at + 1] = chunk[0];
        sector[at] = chunk.get(1).copied().unwrap_or(b' ');
    }
}

fn put_word(sector: &mut [u8], word: usize, value: u16) {
    sector[word * 2..word * 2 + 2].copy_from_slice(&value.to_le_bytes());
}

/// IDENTIFY DEVICE: what an initiator reads for the capacity and the name. 48-bit LBA, a write cache that FLUSH CACHE
/// EXT empties, 512-byte logical and physical sectors (word 106 says so explicitly rather than leaving it to guess).
fn identify(target: &Target, sectors: u64, sector: &mut [u8]) {
    sector.fill(0);
    put_word(sector, 0, 0x0040); // fixed disk
    put_string(sector, 10, &target.serial);
    put_string(sector, 23, &target.revision);
    put_string(sector, 27, &target.model);
    put_word(sector, 49, 1 << 9); // LBA supported
    let lba28 = sectors.min(0x0fff_ffff) as u32;
    put_word(sector, 60, lba28 as u16);
    put_word(sector, 61, (lba28 >> 16) as u16);
    put_word(sector, 82, 1 << 5); // write cache supported
    put_word(sector, 83, (1 << 14) | (1 << 13) | (1 << 12) | (1 << 10)); // valid, FLUSH CACHE EXT, FLUSH CACHE, LBA48
    put_word(sector, 84, 1 << 14);
    put_word(sector, 85, 1 << 5);
    put_word(sector, 86, (1 << 13) | (1 << 12) | (1 << 10));
    put_word(sector, 87, 1 << 14);
    for word in 0..4 {
        put_word(sector, 100 + word, (sectors >> (16 * word)) as u16);
    }
    put_word(sector, 106, 1 << 14); // word valid, logical = physical = 512 bytes
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod cache_tests;
#[cfg(test)]
mod initiator_tests;
#[cfg(test)]
mod readahead_tests;
