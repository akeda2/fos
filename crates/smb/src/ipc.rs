//! The `IPC$` pipe `srvsvc`, just enough for clients to list the shares of a server: DCE/RPC (MS-RPCE) over an SMB named
//! pipe, with the bind and the one call `NetShareEnumAll` (MS-SRVS opnum 15, levels 0 and 1). Everything else is refused
//! (a fault for a call, a rejected context for a bind). No I/O and no allocation: `call` turns one PDU into the reply that
//! the client then reads from the pipe.
//!
//! Wire facts: little-endian NDR (data representation 10 00 00 00), PDUs of one fragment (the reply of 16 shares fits, so
//! it is never split), no authentication trailer. The stub of NetShareEnumAll is: the server name (a unique pointer to a
//! conformant varying UTF-16 string), the level, the union tag (the level again) and a unique pointer to a container
//! (entries read, a null array), the preferred length, and a unique pointer to the resume handle. The answer repeats the
//! container with the array, then the total, the resume handle (when one came in) and the Windows error code.
use crate::wire::{get16, get32, put16, put32, put_utf16};

/// Bytes of one reply; `NAMES` shares of at most `NAME_MAX` characters (about 110 bytes each) fit.
pub const REPLY_MAX: usize = 2048;
/// Longest share name that is listed.
pub const NAME_MAX: usize = 32;
/// Most shares that are listed.
pub const NAMES: usize = 16;

const SRVSVC: [u8; 16] = [0xc8, 0x4f, 0x32, 0x4b, 0x70, 0x16, 0xd3, 0x01, 0x12, 0x78, 0x5a, 0x47, 0xbf, 0x6e, 0xe1, 0x88];
const NDR: [u8; 16] = [0x04, 0x5d, 0x88, 0x8a, 0xeb, 0x1c, 0xc9, 0x11, 0x9f, 0xe8, 0x08, 0x00, 0x2b, 0x10, 0x48, 0x60];
const PTYPE_REQUEST: u8 = 0;
const PTYPE_RESPONSE: u8 = 2;
const PTYPE_FAULT: u8 = 3;
const PTYPE_BIND: u8 = 11;
const PTYPE_BIND_ACK: u8 = 12;
const OPNUM_ENUM_ALL: u16 = 15;
const FAULT_OP_RANGE: u32 = 0x1c01_0002;
const WERR_UNKNOWN_LEVEL: u32 = 124;
/// The fragment size offered in the bind answer.
const FRAGMENT: u16 = 4280;

/// The reply waiting to be read, or being read in pieces.
pub struct Pipe {
    pub reply: [u8; REPLY_MAX],
    pub length: usize,
    pub position: usize,
}

impl Pipe {
    pub const fn new() -> Self {
        Pipe { reply: [0; REPLY_MAX], length: 0, position: 0 }
    }

    /// The reply bytes not yet read, at most `room` of them; they count as read.
    pub fn take(&mut self, room: usize) -> &[u8] {
        let end = (self.position + room).min(self.length);
        let start = self.position;
        self.position = end;
        &self.reply[start..end]
    }

    pub fn pending(&self) -> usize {
        self.length - self.position
    }
}

/// Whether `name` (UTF-8, '/' between parts) opens the share-list pipe.
pub fn is_srvsvc(name: &str) -> bool {
    name.rsplit('/').next().is_some_and(|last| last.eq_ignore_ascii_case("srvsvc"))
}

fn header(out: &mut [u8], ptype: u8, length: usize, call: u32) {
    out[..4].copy_from_slice(&[5, 0, ptype, 3]);
    out[4..8].copy_from_slice(&[0x10, 0, 0, 0]);
    put16(out, 8, length as u16);
    put16(out, 10, 0);
    put32(out, 12, call);
}

/// Handles one PDU written to the pipe: the reply is left in `pipe` (empty for a PDU that wants none). `names` are the
/// shares to list.
pub fn call(pipe: &mut Pipe, request: &[u8], names: &[&str]) {
    pipe.length = 0;
    pipe.position = 0;
    if request.len() < 16 || request[0] != 5 {
        return;
    }
    let call = get32(request, 12);
    match request[2] {
        PTYPE_BIND => bind(pipe, request, call),
        PTYPE_REQUEST => enum_all(pipe, request, call, names),
        _ => {}
    }
}

fn bind(pipe: &mut Pipe, request: &[u8], call: u32) {
    if request.len() < 28 {
        return;
    }
    let out = &mut pipe.reply;
    out[..64].fill(0);
    put16(out, 16, FRAGMENT);
    put16(out, 18, FRAGMENT);
    let group = get32(request, 20);
    put32(out, 20, if group == 0 { 0x0000_5a5a } else { group });
    // the secondary address "\PIPE\srvsvc" and its NUL, padded to 4
    put16(out, 24, 13);
    out[26..39].copy_from_slice(b"\\PIPE\\srvsvc\0");
    let count = (request[24] as usize).min(8);
    out[40] = count as u8;
    let mut at = 28;
    for index in 0..count {
        let Some(head) = request.get(at..at + 24) else { break };
        let transfers = head[2] as usize;
        let supported = head[4..20] == SRVSVC
            && request.get(at + 24..at + 24 + transfers * 20).is_some_and(|list| list.chunks_exact(20).any(|syntax| syntax[..16] == NDR));
        let result = 44 + index * 24;
        out[result + 24..result + 48].fill(0);
        if supported {
            out[result + 4..result + 20].copy_from_slice(&NDR);
            put32(out, result + 20, 2);
        } else {
            // provider rejection, transfer syntaxes not supported
            put16(out, result, 2);
            put16(out, result + 2, 2);
        }
        at += 24 + transfers * 20;
    }
    let length = 44 + count * 24;
    header(out, PTYPE_BIND_ACK, length, call);
    pipe.length = length;
}

fn fault(pipe: &mut Pipe, request: &[u8], call: u32, status: u32) {
    let out = &mut pipe.reply;
    out[..32].fill(0);
    put16(out, 20, if request.len() >= 22 { get16(request, 20) } else { 0 });
    put32(out, 24, status);
    header(out, PTYPE_FAULT, 32, call);
    pipe.length = 32;
}

/// A conformant varying string (maximum, offset 0, actual, the UTF-16 text and a NUL, padded to 4) at `at`; the end.
fn put_string(out: &mut [u8], mut at: usize, text: &str) -> usize {
    let units = text.encode_utf16().count() + 1;
    put32(out, at, units as u32);
    put32(out, at + 4, 0);
    put32(out, at + 8, units as u32);
    at += 12;
    let bytes = put_utf16(out, at, text.as_bytes()).unwrap_or(0);
    let padded = (bytes + 2 + 3) & !3;
    out[at + bytes..at + padded].fill(0);
    at + padded
}

fn enum_all(pipe: &mut Pipe, request: &[u8], call: u32, names: &[&str]) {
    if request.len() < 24 {
        return fault(pipe, request, call, FAULT_OP_RANGE);
    }
    if get16(request, 22) != OPNUM_ENUM_ALL {
        return fault(pipe, request, call, FAULT_OP_RANGE);
    }
    let flags = request[3];
    let mut at = if flags & 0x80 != 0 { 40 } else { 24 };
    // server name: a unique pointer, then the string
    if request.len() < at + 4 {
        return fault(pipe, request, call, FAULT_OP_RANGE);
    }
    let named = get32(request, at) != 0;
    at += 4;
    if named {
        if request.len() < at + 12 {
            return fault(pipe, request, call, FAULT_OP_RANGE);
        }
        at += 12 + ((get32(request, at + 8) as usize).saturating_mul(2).saturating_add(3) & !3);
    }
    if request.len() < at + 12 {
        return fault(pipe, request, call, FAULT_OP_RANGE);
    }
    let level = get32(request, at);
    let mut after = at + 12;
    if get32(request, at + 8) != 0 {
        after += 8;
    }
    // preferred length, then the resume handle pointer
    let resume = request.len() >= after + 8 && get32(request, after + 4) != 0;

    let out = &mut pipe.reply;
    out[..24].fill(0);
    put16(out, 20, get16(request, 20));
    let mut end = 24;
    put32(out, end, level);
    put32(out, end + 4, level);
    if level > 1 {
        // an unknown level: no container, no entries
        put32(out, end + 8, 0);
        put32(out, end + 12, 0);
        put32(out, end + 16, WERR_UNKNOWN_LEVEL);
        end += 20;
    } else {
        let count = names.len().min(NAMES);
        put32(out, end + 8, 0x0002_0000);
        put32(out, end + 12, count as u32);
        put32(out, end + 16, 0x0002_0004);
        put32(out, end + 20, count as u32);
        end += 24;
        let element = if level == 1 { 12 } else { 4 };
        let mut pointer = 0x0002_0008u32;
        for _ in 0..count {
            put32(out, end, pointer);
            pointer += 4;
            if level == 1 {
                put32(out, end + 4, 0);
                put32(out, end + 8, pointer);
                pointer += 4;
            }
            end += element;
        }
        for name in &names[..count] {
            end = put_string(out, end, name);
            if level == 1 {
                end = put_string(out, end, "");
            }
        }
        put32(out, end, count as u32);
        end += 4;
        if resume {
            put32(out, end, 0x0002_ff00);
            put32(out, end + 4, 0);
            end += 8;
        } else {
            put32(out, end, 0);
            end += 4;
        }
        put32(out, end, 0);
        end += 4;
    }
    put32(out, 16, (end - 24) as u32);
    header(out, PTYPE_RESPONSE, end, call);
    pipe.length = end;
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::string::String;
    use std::vec::Vec;

    fn bind_pdu() -> [u8; 72] {
        let mut pdu = [0u8; 72];
        header(&mut pdu, PTYPE_BIND, 72, 7);
        put16(&mut pdu, 16, 4280);
        put16(&mut pdu, 18, 4280);
        pdu[24] = 1;
        pdu[28 + 2] = 1;
        pdu[32..48].copy_from_slice(&SRVSVC);
        put32(&mut pdu, 48, 3);
        pdu[52..68].copy_from_slice(&NDR);
        put32(&mut pdu, 68, 2);
        pdu
    }

    #[test]
    fn bind_accepts_srvsvc_over_ndr_only() {
        let mut pipe = Pipe::new();
        call(&mut pipe, &bind_pdu(), &[]);
        assert_eq!(pipe.reply[2], PTYPE_BIND_ACK);
        assert_eq!(get16(&pipe.reply, 8) as usize, pipe.length);
        assert_eq!(pipe.reply[40], 1);
        assert_eq!(get16(&pipe.reply, 44), 0);
        assert_eq!(pipe.reply[48..64], NDR);
        let mut other = bind_pdu();
        other[32] ^= 1;
        call(&mut pipe, &other, &[]);
        assert_eq!(get16(&pipe.reply, 44), 2);
    }

    fn request(level: u32, resume: bool) -> ([u8; 128], usize) {
        let mut pdu = [0u8; 128];
        put16(&mut pdu, 22, OPNUM_ENUM_ALL);
        let mut at = 24;
        put32(&mut pdu, at, 0x20000);
        put32(&mut pdu, at + 4, 4);
        put32(&mut pdu, at + 8, 0);
        put32(&mut pdu, at + 12, 4);
        at += 16 + 8;
        put32(&mut pdu, at, level);
        put32(&mut pdu, at + 4, level);
        put32(&mut pdu, at + 8, 0x20004);
        put32(&mut pdu, at + 12, 0);
        put32(&mut pdu, at + 16, 0);
        put32(&mut pdu, at + 20, 0xffff_ffff);
        at += 24;
        if resume {
            put32(&mut pdu, at, 0x20008);
            put32(&mut pdu, at + 4, 0);
            at += 8;
        } else {
            at += 4;
        }
        header(&mut pdu, PTYPE_REQUEST, at, 9);
        (pdu, at)
    }

    #[test]
    fn lists_the_given_shares_at_level_1() {
        let mut pipe = Pipe::new();
        let (pdu, length) = request(1, true);
        call(&mut pipe, &pdu[..length], &["data", "music"]);
        let reply = &pipe.reply[..pipe.length];
        assert_eq!(reply[2], PTYPE_RESPONSE);
        assert_eq!(get16(reply, 8) as usize, reply.len());
        assert_eq!(get32(reply, 24), 1);
        assert_eq!(get32(reply, 36), 2);
        assert_eq!(get32(reply, 44), 2);
        let text: Vec<u16> = reply.chunks_exact(2).map(|pair| u16::from_le_bytes([pair[0], pair[1]])).collect();
        let text = String::from_utf16_lossy(&text);
        assert!(text.contains("data") && text.contains("music"));
        assert_eq!(get32(reply, reply.len() - 4), 0);
        assert_eq!(reply.len() % 4, 0);
    }

    #[test]
    fn level_0_has_names_only_and_an_unknown_level_an_error() {
        let mut pipe = Pipe::new();
        let (pdu, length) = request(0, false);
        call(&mut pipe, &pdu[..length], &["a"]);
        // level, tag, pointer, count, pointer, max count, one name pointer, then the name
        assert_eq!(get32(&pipe.reply, 44), 1);
        assert_eq!(get32(&pipe.reply, 48), 0x0002_0008);
        assert_eq!(get32(&pipe.reply, 52), 1 + 1);
        let (pdu, length) = request(502, true);
        call(&mut pipe, &pdu[..length], &["a"]);
        assert_eq!(get32(&pipe.reply, pipe.length - 4), WERR_UNKNOWN_LEVEL);
    }

    #[test]
    fn other_calls_fault() {
        let mut pipe = Pipe::new();
        let (mut pdu, length) = request(1, true);
        put16(&mut pdu, 22, 99);
        call(&mut pipe, &pdu[..length], &[]);
        assert_eq!(pipe.reply[2], PTYPE_FAULT);
        assert_eq!(get32(&pipe.reply, 24), FAULT_OP_RANGE);
    }

    #[test]
    fn a_reply_is_read_in_pieces() {
        let mut pipe = Pipe::new();
        let (pdu, length) = request(1, true);
        call(&mut pipe, &pdu[..length], &["data"]);
        let total = pipe.length;
        assert_eq!(pipe.take(10).len(), 10);
        assert_eq!(pipe.pending(), total - 10);
        assert_eq!(pipe.take(usize::MAX / 2).len(), total - 10);
        assert_eq!(pipe.pending(), 0);
    }
}
