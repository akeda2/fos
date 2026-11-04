//! The server half: one SMB2 request (or a compound chain) in, the answer out. No I/O and no allocation: the caller reads
//! a frame from its TCP connection, calls `Server::handle`, and sends what comes back. Files come from a `Share`.
//!
//! Scope (docs/smb.md): dialects 2.0.2, 2.1 and 3.0 (no 3.1.1, so no preauthentication hash), guest sessions, one share,
//! read-only, no signing or encryption. NEGOTIATE, SESSION_SETUP (raw NTLMSSP or SPNEGO-wrapped), LOGOFF, TREE_CONNECT,
//! TREE_DISCONNECT, CREATE, CLOSE, READ, QUERY_DIRECTORY, QUERY_INFO, ECHO, FLUSH and the one IOCTL a 3.0 client insists on
//! (VALIDATE_NEGOTIATE_INFO). Writes answer ACCESS_DENIED. Compound requests are handled: a client such as Windows or
//! Linux cifs sends CREATE+QUERY_INFO+CLOSE as one chain, with `RELATED` operations taking the previous file id.
//!
//! One connection carries one session and one tree; a `Server` is therefore one connection's state. Reads and directory
//! answers are limited to `MAX_IO` bytes, so the output buffer should hold `MAX_IO + 4 + 64 + 16` bytes.

use crate::ntlm::{hmac_md5, ntlmv2_key, ntlmv2_proof};
use crate::sign;
use crate::wire::*;

pub const MAX_IO: usize = 65536;
pub const MAX_PATH: usize = 256;

const OK: u32 = 0;
const MORE_PROCESSING: u32 = 0xC000_0016;
const INVALID_PARAMETER: u32 = 0xC000_000D;
const NO_SUCH_FILE: u32 = 0xC000_000F;
const END_OF_FILE: u32 = 0xC000_0011;
const NO_MORE_FILES: u32 = 0x8000_0006;
const NOT_SUPPORTED: u32 = 0xC000_00BB;
const ACCESS_DENIED: u32 = 0xC000_0022;
const NAME_NOT_FOUND: u32 = 0xC000_0034;
const NAME_INVALID: u32 = 0xC000_0033;
const IS_A_DIRECTORY: u32 = 0xC000_00BA;
const NOT_A_DIRECTORY: u32 = 0xC000_0103;
const LOGON_FAILURE: u32 = 0xC000_006D;
const SESSION_DELETED: u32 = 0xC000_0203;
const NETWORK_NAME_DELETED: u32 = 0xC000_00C9;
const BAD_NETWORK_NAME: u32 = 0xC000_00CC;
const FILE_CLOSED: u32 = 0xC000_0128;
const NO_RESOURCES: u32 = 0xC000_009A;
const IO_ERROR: u32 = 0xC000_00E9;
const LENGTH_MISMATCH: u32 = 0xC000_0004;
const DIALECT_MISMATCH: u32 = 0xC000_00BD;
const NAME_COLLISION: u32 = 0xC000_0035;
const DIRECTORY_NOT_EMPTY: u32 = 0xC000_0101;
const DISK_FULL: u32 = 0xC000_007F;

const SESSION_ID: u64 = 0x0000_0001_0000_0001;
const TREE_ID: u32 = 1;
const FLAG_SIGNED: u32 = 8;
const NTLMSSP: &[u8; 8] = b"NTLMSSP\0";
/// OID 1.3.6.1.4.1.311.2.2.10 (NTLMSSP) as a DER object identifier.
const NTLM_OID: [u8; 12] = [0x06, 0x0a, 0x2b, 0x06, 0x01, 0x04, 0x01, 0x82, 0x37, 0x02, 0x02, 0x0a];
/// The SPNEGO NegTokenInit a server offers in NEGOTIATE: mechanism list with NTLMSSP only.
const NEG_TOKEN_INIT: [u8; 30] = [
    0x60, 0x1c, 0x06, 0x06, 0x2b, 0x06, 0x01, 0x05, 0x05, 0x02, 0xa0, 0x12, 0x30, 0x10, 0xa0, 0x0e, 0x30, 0x0c, 0x06, 0x0a, 0x2b, 0x06,
    0x01, 0x04, 0x01, 0x82, 0x37, 0x02, 0x02, 0x0a,
];

/// What the server needs to know about a file or folder.
#[derive(Clone, Copy, Default)]
pub struct Stat {
    pub dir: bool,
    pub size: u64,
    /// Modification time, seconds since 1970 (0: unknown).
    pub mtime: u64,
    /// Permission bits (`0o644`); 0 means the volume keeps none and nothing is checked.
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
}

/// Who a session acts as: its uid and gid are checked against a file's mode bits (uid 0 passes everything).
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Identity {
    pub uid: u32,
    pub gid: u32,
}

/// A user the share knows: the NT hash (MD4 of the password as UTF-16LE) is all that is stored.
#[derive(Clone, Copy)]
pub struct Account {
    pub nt_hash: [u8; 16],
    pub identity: Identity,
}

/// Why a change to the share failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Fail {
    Denied,
    Exists,
    NotFound,
    NotEmpty,
    NoSpace,
    IsDir,
    Io,
}

impl Fail {
    fn status(self) -> u32 {
        match self {
            Fail::Denied => ACCESS_DENIED,
            Fail::Exists => NAME_COLLISION,
            Fail::NotFound => NAME_NOT_FOUND,
            Fail::NotEmpty => DIRECTORY_NOT_EMPTY,
            Fail::NoSpace => DISK_FULL,
            Fail::IsDir => IS_A_DIRECTORY,
            Fail::Io => IO_ERROR,
        }
    }
}

/// The files behind the share. Paths are relative to the share root with '/' between components, no leading slash, ""
/// for the root; names are UTF-8.
pub trait Share {
    /// The share name clients connect to (compared without regard to case).
    fn name(&self) -> &str;
    fn stat(&mut self, path: &str) -> Option<Stat>;
    /// The `index`th entry of folder `path` (without "." and ".."), its name written to `name`; `None` past the end.
    fn read_dir(&mut self, path: &str, index: usize, name: &mut [u8]) -> Option<(usize, Stat)>;
    /// Reads at `offset`; returns the bytes read (0 at or past the end).
    fn read(&mut self, path: &str, offset: u64, out: &mut [u8]) -> Result<usize, ()>;
    /// The account named `user` (UTF-8, compare without regard to case); `None` for a name the share does not know.
    fn account(&mut self, _user: &str) -> Option<Account> {
        None
    }
    /// Who a guest (an anonymous login, or a name the share does not know) acts as; `None` refuses guests.
    fn guest(&mut self) -> Option<Identity> {
        Some(Identity { uid: 65534, gid: 65534 })
    }
    /// Total and free bytes of the volume.
    fn space(&mut self) -> (u64, u64) {
        (0, 0)
    }
    /// Whether the share accepts changes at all; a read-only share never sees the calls below.
    fn writable(&self) -> bool {
        false
    }
    /// Makes an empty file or folder owned by `owner` with permission bits `mode`; the parent exists.
    fn create(&mut self, _path: &str, _dir: bool, _mode: u32, _owner: Identity) -> Result<(), Fail> {
        Err(Fail::Denied)
    }
    /// Writes `data` at `offset`, growing the file (a gap reads as zeros).
    fn write(&mut self, _path: &str, _offset: u64, _data: &[u8]) -> Result<(), Fail> {
        Err(Fail::Denied)
    }
    /// Sets the file's length, cutting it or extending it with zeros.
    fn truncate(&mut self, _path: &str, _size: u64) -> Result<(), Fail> {
        Err(Fail::Denied)
    }
    /// Removes a file or an empty folder.
    fn remove(&mut self, _path: &str) -> Result<(), Fail> {
        Err(Fail::Denied)
    }
    /// Renames; an existing target of the same kind is replaced.
    fn rename(&mut self, _from: &str, _to: &str) -> Result<(), Fail> {
        Err(Fail::Denied)
    }
}

#[derive(Clone, Copy)]
struct Open {
    used: bool,
    dir: bool,
    length: usize,
    path: [u8; MAX_PATH],
    /// Opened with an access that changes data (write, append or delete).
    write: bool,
    /// Removed when closed.
    delete: bool,
    /// Folder enumeration position: 0 is ".", 1 is "..", then the share's entries.
    next: usize,
}

impl Open {
    const EMPTY: Open = Open { used: false, dir: false, length: 0, path: [0; MAX_PATH], write: false, delete: false, next: 0 };

    fn path(&self) -> &str {
        core::str::from_utf8(&self.path[..self.length]).unwrap_or("")
    }
}

/// What a compound chain carries from one request to the next.
struct Chain {
    tree: u32,
    session: u64,
    file: Option<usize>,
    error: Option<u32>,
    status: u32,
}

pub struct Server<S: Share, const OPENS: usize> {
    pub share: S,
    /// The 8-byte NTLM server challenge; set it to something unpredictable per connection.
    pub challenge: [u8; 8],
    pub guid: [u8; 16],
    /// The time now, seconds since 1970.
    pub now: u64,
    dialect: u16,
    /// 0: nothing yet, 1: challenge sent, 2: logged in.
    auth: u8,
    identity: Identity,
    /// The signing key once a password login succeeded; messages are signed with it (`sign`).
    signing: Option<[u8; 16]>,
    sign_answer: bool,
    tree: bool,
    opens: [Open; OPENS],
}

fn filetime(unix: u64) -> u64 {
    (unix + 11_644_473_600) * 10_000_000
}

fn need(msg: &[u8], length: usize) -> Result<(), u32> {
    if msg.len() >= HEADER + length { Ok(()) } else { Err(INVALID_PARAMETER) }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

/// DER: tag, length, then `content` (already in `out` at `at + header`); writes the header in front by shifting nothing:
/// the caller reserves `der_header_len(content)` bytes. Returns the header length.
fn der_header_len(content: usize) -> usize {
    if content < 0x80 {
        2
    } else if content < 0x100 {
        3
    } else {
        4
    }
}

fn put_der_header(out: &mut [u8], at: usize, tag: u8, content: usize) -> usize {
    out[at] = tag;
    match der_header_len(content) {
        2 => out[at + 1] = content as u8,
        3 => {
            out[at + 1] = 0x81;
            out[at + 2] = content as u8;
        }
        _ => {
            out[at + 1] = 0x82;
            out[at + 2] = (content >> 8) as u8;
            out[at + 3] = content as u8;
        }
    }
    der_header_len(content)
}

/// A SPNEGO NegTokenResp: negotiation state (0 accept-completed, 1 accept-incomplete), the NTLMSSP mechanism when
/// `token` is given, and the token. Returns the length written.
fn neg_token_resp(out: &mut [u8], state: u8, token: &[u8]) -> usize {
    let wrapped = if token.is_empty() { 0 } else { der_header_len(token.len()) + token.len() };
    let octet = if token.is_empty() { 0 } else { der_header_len(wrapped) + wrapped };
    let mech = if token.is_empty() { 0 } else { 2 + NTLM_OID.len() };
    let sequence = 5 + mech + octet;
    let total_sequence = der_header_len(sequence) + sequence;
    let mut at = put_der_header(out, 0, 0xa1, total_sequence);
    at += put_der_header(out, at, 0x30, sequence);
    out[at..at + 5].copy_from_slice(&[0xa0, 0x03, 0x0a, 0x01, state]);
    at += 5;
    if !token.is_empty() {
        at += put_der_header(out, at, 0xa1, NTLM_OID.len());
        out[at..at + NTLM_OID.len()].copy_from_slice(&NTLM_OID);
        at += NTLM_OID.len();
        at += put_der_header(out, at, 0xa2, wrapped);
        at += put_der_header(out, at, 0x04, token.len());
        out[at..at + token.len()].copy_from_slice(token);
        at += token.len();
    }
    at
}

/// `*`, `?` and the DOS wildcards `<`, `>`, `"` (as `*`, `?`, `.`), case-insensitive.
fn wildcard(pattern: &[u8], name: &[u8]) -> bool {
    let (mut p, mut n) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        let matched = p < pattern.len()
            && match pattern[p] {
                b'*' | b'<' => {
                    star = Some((p, n));
                    p += 1;
                    continue;
                }
                b'?' | b'>' => true,
                b'"' => name[n] == b'.',
                c => c.eq_ignore_ascii_case(&name[n]),
            };
        if matched {
            p += 1;
            n += 1;
        } else if let Some((sp, sn)) = star {
            p = sp + 1;
            n = sn + 1;
            star = Some((sp, sn + 1));
        } else {
            return false;
        }
    }
    while p < pattern.len() && matches!(pattern[p], b'*' | b'<') {
        p += 1;
    }
    p == pattern.len()
}

fn put_times(buffer: &mut [u8], at: usize, unix: u64) {
    for i in 0..4 {
        put64(buffer, at + i * 8, filetime(unix));
    }
}

fn attributes(stat: &Stat) -> u32 {
    if stat.dir { 0x10 } else { 0x80 }
}

fn allocation(stat: &Stat) -> u64 {
    (stat.size + 4095) & !4095
}

impl<S: Share, const OPENS: usize> Server<S, OPENS> {
    pub const fn new(share: S) -> Self {
        Server {
            share,
            challenge: [0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88],
            guid: *b"fosfor-smbd-0001",
            now: 0,
            dialect: 0,
            auth: 0,
            identity: Identity { uid: 65534, gid: 65534 },
            signing: None,
            sign_answer: false,
            tree: false,
            opens: [Open::EMPTY; OPENS],
        }
    }

    /// Answers `request` (a message without its 4-byte frame header, possibly a compound chain). The answer, frame
    /// header included, goes into `out`; returns its length, 0 when there is nothing to send.
    pub fn handle(&mut self, request: &[u8], out: &mut [u8]) -> usize {
        if request.len() >= 4 && request[..4] == [0xFF, b'S', b'M', b'B'] {
            return self.smb1_negotiate(out);
        }
        let mut chain = Chain { tree: 0, session: 0, file: None, error: None, status: OK };
        let (mut off, mut at) = (0, 4);
        while request.len() >= off + HEADER && request[off..off + 4] == PROTOCOL_ID {
            let next = get32(request, off + 20) as usize;
            let end = if next == 0 { request.len() } else { (off + next).min(request.len()) };
            if at + HEADER + 9 > out.len() {
                break;
            }
            if let Some(length) = self.one(&mut chain, &request[off..end], &mut out[at..]) {
                let padded = if next != 0 { (length + 7) & !7 } else { length };
                if padded > out.len() - at {
                    break;
                }
                for byte in &mut out[at + length..at + padded] {
                    *byte = 0;
                }
                if next != 0 {
                    put32(out, at + 20, padded as u32);
                }
                if self.sign_answer {
                    if let Some(key) = self.signing {
                        let value = sign::signature(self.dialect, &key, &out[at..at + padded]);
                        out[at + 48..at + 64].copy_from_slice(&value);
                    }
                }
                at += padded;
            }
            if next == 0 {
                break;
            }
            off += next;
        }
        if at == 4 {
            return 0;
        }
        out[..4].copy_from_slice(&frame_header(at - 4));
        at
    }

    /// A first contact in the old protocol (`SMB 2.002` and `SMB 2.???` in an SMB1 NEGOTIATE): answer with the SMB2
    /// wildcard dialect and the client repeats the negotiation in SMB2.
    fn smb1_negotiate(&mut self, out: &mut [u8]) -> usize {
        if out.len() < 4 + HEADER + 65 + NEG_TOKEN_INIT.len() {
            return 0;
        }
        let body = self.negotiate_body(0x02FF, &mut out[4 + HEADER..]);
        Self::write_header(&mut out[4..], CMD_NEGOTIATE, 0, 0, 1, OK, 0, 0);
        let length = HEADER + body;
        out[..4].copy_from_slice(&frame_header(length));
        4 + length
    }

    #[allow(clippy::too_many_arguments)]
    fn write_header(out: &mut [u8], command: u16, flags: u32, message: u64, credits: u32, status: u32, tree: u32, session: u64) {
        out[..HEADER].fill(0);
        out[..4].copy_from_slice(&PROTOCOL_ID);
        put16(out, 4, 64);
        put16(out, 6, (credits >> 16) as u16);
        put32(out, 8, status);
        put16(out, 12, command);
        put16(out, 14, credits as u16);
        put32(out, 16, FLAG_RESPONSE | (flags & (FLAG_RELATED | FLAG_SIGNED)));
        put64(out, 24, message);
        put32(out, 36, tree);
        put64(out, 40, session);
    }

    fn one(&mut self, chain: &mut Chain, msg: &[u8], out: &mut [u8]) -> Option<usize> {
        let command = get16(msg, 12);
        let charge = get16(msg, 6);
        let credits = get16(msg, 14);
        let flags = get32(msg, 16);
        let message = get64(msg, 24);
        let related = flags & FLAG_RELATED != 0;
        let mut tree = get32(msg, 36);
        let mut session = get64(msg, 40);
        if related {
            if tree == 0xFFFF_FFFF {
                tree = chain.tree;
            }
            if session == u64::MAX {
                session = chain.session;
            }
        } else {
            chain.error = None;
        }
        chain.tree = tree;
        chain.session = session;
        chain.status = OK;
        if command == CMD_CANCEL {
            return None;
        }
        // A signed request must carry the right signature; an answer is signed when the request was, and the answer
        // that completes the login always.
        self.sign_answer = false;
        if let Some(key) = self.signing.filter(|_| self.auth == 2 || command == CMD_SESSION_SETUP) {
            if flags & FLAG_SIGNED != 0 {
                if !self.signature_ok(&key, msg) {
                    let (head, body) = out.split_at_mut(HEADER);
                    body[..9].fill(0);
                    put16(body, 0, 9);
                    Self::write_header(head, command, flags, message, 1, ACCESS_DENIED, tree, session);
                    return Some(HEADER + 9);
                }
                self.sign_answer = true;
            }
        }

        let (head, body) = out.split_at_mut(HEADER);
        let outcome = if let Some(status) = chain.error.filter(|_| related) {
            Err(status)
        } else {
            self.dispatch(chain, command, related, msg, body)
        };
        let (status, length) = match outcome {
            Ok(length) => (chain.status, length),
            Err(status) => {
                if command == CMD_CREATE {
                    chain.error = Some(status);
                }
                body[..9].fill(0);
                put16(body, 0, 9);
                (status, 9)
            }
        };
        let granted = u32::from(credits.max(1)) | (u32::from(charge) << 16);
        let signed = self.sign_answer || (command == CMD_SESSION_SETUP && self.signing.is_some() && self.auth == 2);
        Self::write_header(head, command, flags | if signed { FLAG_SIGNED } else { 0 }, message, granted, status, chain.tree, chain.session);
        self.sign_answer = signed;
        Some(HEADER + length)
    }

    /// Whether the signature in `msg` (header bytes 48..64) is the one `key` gives for the message with that field zeroed.
    fn signature_ok(&self, key: &[u8; 16], msg: &[u8]) -> bool {
        if msg.len() < HEADER {
            return false;
        }
        let expected = sign::signature(self.dialect, key, msg);
        expected.iter().zip(&msg[48..64]).fold(0u8, |sum, (a, b)| sum | (a ^ b)) == 0
    }

    fn dispatch(&mut self, chain: &mut Chain, command: u16, related: bool, msg: &[u8], body: &mut [u8]) -> Result<usize, u32> {
        match command {
            CMD_NEGOTIATE => return self.negotiate(msg, body),
            CMD_SESSION_SETUP => return self.session_setup(chain, msg, body),
            CMD_ECHO => return Self::empty(body),
            _ => {}
        }
        if self.auth != 2 || chain.session != SESSION_ID {
            return Err(SESSION_DELETED);
        }
        match command {
            CMD_LOGOFF => {
                self.auth = 0;
                self.tree = false;
                self.opens = [Open::EMPTY; OPENS];
                return Self::empty(body);
            }
            CMD_TREE_CONNECT => return self.tree_connect(chain, msg, body),
            _ => {}
        }
        if !self.tree || chain.tree != TREE_ID {
            return Err(NETWORK_NAME_DELETED);
        }
        match command {
            CMD_TREE_DISCONNECT => {
                self.tree = false;
                self.opens = [Open::EMPTY; OPENS];
                Self::empty(body)
            }
            CMD_CREATE => self.create(chain, msg, body),
            CMD_CLOSE => self.close(chain, related, msg, body),
            CMD_READ => self.read(chain, related, msg, body),
            CMD_QUERY_DIRECTORY => self.query_directory(chain, related, msg, body),
            CMD_QUERY_INFO => self.query_info(chain, related, msg, body),
            CMD_IOCTL => self.ioctl(msg, body),
            CMD_FLUSH => Self::empty(body),
            CMD_WRITE => self.write(chain, related, msg, body),
            CMD_SET_INFO => self.set_info(chain, related, msg, body),
            CMD_LOCK => Err(ACCESS_DENIED),
            _ => Err(NOT_SUPPORTED),
        }
    }

    /// The common answer: structure size 4 and a reserved word.
    fn empty(body: &mut [u8]) -> Result<usize, u32> {
        body[..4].fill(0);
        put16(body, 0, 4);
        Ok(4)
    }

    fn negotiate_body(&self, dialect: u16, body: &mut [u8]) -> usize {
        body[..64].fill(0);
        put16(body, 0, 65);
        put16(body, 2, 1); // signing enabled, not required (guests are not signed)
        put16(body, 4, dialect);
        body[8..24].copy_from_slice(&self.guid);
        put32(body, 28, MAX_IO as u32);
        put32(body, 32, MAX_IO as u32);
        put32(body, 36, MAX_IO as u32);
        put64(body, 40, filetime(self.now));
        put64(body, 48, filetime(self.now));
        put16(body, 56, (HEADER + 64) as u16);
        put16(body, 58, NEG_TOKEN_INIT.len() as u16);
        body[64..64 + NEG_TOKEN_INIT.len()].copy_from_slice(&NEG_TOKEN_INIT);
        64 + NEG_TOKEN_INIT.len()
    }

    fn negotiate(&mut self, msg: &[u8], body: &mut [u8]) -> Result<usize, u32> {
        need(msg, 36)?;
        let count = get16(msg, HEADER + 2) as usize;
        let list = msg.get(HEADER + 36..HEADER + 36 + count * 2).ok_or(INVALID_PARAMETER)?;
        let mut best = 0;
        for pair in list.chunks_exact(2) {
            let dialect = u16::from_le_bytes([pair[0], pair[1]]);
            if matches!(dialect, 0x0202 | 0x0210 | 0x0300) && dialect > best {
                best = dialect;
            }
        }
        if best == 0 {
            return Err(DIALECT_MISMATCH);
        }
        if body.len() < 64 + NEG_TOKEN_INIT.len() {
            return Err(NO_RESOURCES);
        }
        self.dialect = best;
        Ok(self.negotiate_body(best, body))
    }

    fn session_setup(&mut self, chain: &mut Chain, msg: &[u8], body: &mut [u8]) -> Result<usize, u32> {
        need(msg, 25)?;
        if self.dialect == 0 {
            return Err(INVALID_PARAMETER);
        }
        let offset = get16(msg, HEADER + 12) as usize;
        let length = get16(msg, HEADER + 14) as usize;
        let blob = msg.get(offset..offset + length).ok_or(INVALID_PARAMETER)?;
        let wrapped = matches!(blob.first(), Some(0x60 | 0xa1));
        let at = find(blob, NTLMSSP).ok_or(LOGON_FAILURE)?;
        if blob.len() < at + 16 {
            return Err(INVALID_PARAMETER);
        }
        body[..8].fill(0);
        put16(body, 0, 9);
        put16(body, 4, (HEADER + 8) as u16);
        chain.session = SESSION_ID;
        match get32(blob, at + 8) {
            1 => {
                self.signing = None;
                let mut token = [0u8; 128];
                let token_length = self.challenge_message(get32(blob, at + 12), &mut token);
                let written = if wrapped {
                    neg_token_resp(&mut body[8..], 1, &token[..token_length])
                } else {
                    body[8..8 + token_length].copy_from_slice(&token[..token_length]);
                    token_length
                };
                put16(body, 6, written as u16);
                self.auth = 1;
                chain.status = MORE_PROCESSING;
                Ok(8 + written)
            }
            3 if self.auth == 1 => {
                let (identity, guest, key) = self.authenticate(&blob[at..])?;
                self.identity = identity;
                self.signing = key.map(|session_key| if self.dialect >= 0x0300 { sign::signing_key_v3(&session_key) } else { session_key });
                self.sign_answer = self.signing.is_some();
                let written = if wrapped { neg_token_resp(&mut body[8..], 0, &[]) } else { 0 };
                put16(body, 2, u16::from(guest)); // IS_GUEST
                put16(body, 6, written as u16);
                self.auth = 2;
                Ok(8 + written)
            }
            _ => Err(LOGON_FAILURE),
        }
    }

    /// Checks an NTLMSSP AUTHENTICATE message (`msg` starts at "NTLMSSP"). A known user must prove the password with
    /// NTLMv2 (the NTLMv1 and LM answers are refused); an anonymous login or an unknown name is a guest when the share
    /// has guests. Returns the identity and whether it is a guest.
    fn authenticate(&mut self, msg: &[u8]) -> Result<(Identity, bool, Option<[u8; 16]>), u32> {
        let field = |at: usize| -> Option<&[u8]> {
            if msg.len() < at + 8 {
                return None;
            }
            let (length, offset) = (get16(msg, at) as usize, get32(msg, at + 4) as usize);
            msg.get(offset..offset.checked_add(length)?)
        };
        let response = field(20).ok_or(LOGON_FAILURE)?;
        let domain = field(28).ok_or(LOGON_FAILURE)?;
        let user = field(36).ok_or(LOGON_FAILURE)?;
        let mut name = [0u8; 128];
        let used = name_to_utf8(user, &mut name);
        let name = core::str::from_utf8(&name[..used]).map_err(|_| LOGON_FAILURE)?;
        let guest = |server: &mut Self| server.share.guest().map(|identity| (identity, true, None)).ok_or(LOGON_FAILURE);
        if name.is_empty() {
            return guest(self);
        }
        let Some(account) = self.share.account(name) else { return guest(self) };
        if response.len() < 16 + 28 {
            return Err(LOGON_FAILURE);
        }
        let key = ntlmv2_key(&account.nt_hash, user, domain);
        let proof = ntlmv2_proof(&key, &self.challenge, &response[16..]);
        let difference = proof.iter().zip(&response[..16]).fold(0u8, |sum, (a, b)| sum | (a ^ b));
        if difference != 0 {
            return Err(LOGON_FAILURE);
        }
        Ok((account.identity, false, Some(hmac_md5(&key, &[&proof]))))
    }

    /// Whether the session may do `want` (4 read, 1 search) with `stat`, by its owner, group and other bits.
    fn allowed(&self, stat: &Stat, want: u32) -> bool {
        if stat.mode == 0 || self.identity.uid == 0 {
            return true;
        }
        let bits = if self.identity.uid == stat.uid {
            stat.mode >> 6
        } else if self.identity.gid == stat.gid {
            stat.mode >> 3
        } else {
            stat.mode
        };
        bits & want == want
    }

    /// An NTLMSSP CHALLENGE message (MS-NLMP 2.2.1.2): target name "FOS" and a target-info list naming the domain and
    /// the computer. Flags echo what the client offered of Unicode, NTLM, extended session security, 128 and 56 bit.
    fn challenge_message(&self, client_flags: u32, out: &mut [u8]) -> usize {
        const UNICODE: u32 = 0x1;
        const REQUEST_TARGET: u32 = 0x4;
        const NTLM: u32 = 0x200;
        const TARGET_TYPE_SERVER: u32 = 0x2_0000;
        const EXTENDED: u32 = 0x8_0000;
        const TARGET_INFO: u32 = 0x80_0000;
        const BITS128: u32 = 0x2000_0000;
        const BITS56: u32 = 0x8000_0000;
        let flags = (client_flags & (UNICODE | REQUEST_TARGET | NTLM | EXTENDED | BITS128 | BITS56)) | TARGET_TYPE_SERVER | TARGET_INFO | UNICODE;
        let name: [u8; 6] = [b'F', 0, b'O', 0, b'S', 0];
        out[..56].fill(0);
        out[..8].copy_from_slice(NTLMSSP);
        put32(out, 8, 2);
        put16(out, 12, name.len() as u16);
        put16(out, 14, name.len() as u16);
        put32(out, 16, 56);
        put32(out, 20, flags);
        out[24..32].copy_from_slice(&self.challenge);
        out[56..56 + name.len()].copy_from_slice(&name);
        let mut at = 56 + name.len();
        let info_start = at;
        for id in [2u16, 1] {
            put16(out, at, id);
            put16(out, at + 2, name.len() as u16);
            out[at + 4..at + 4 + name.len()].copy_from_slice(&name);
            at += 4 + name.len();
        }
        put32(out, at, 0); // MsvAvEOL
        at += 4;
        put16(out, 40, (at - info_start) as u16);
        put16(out, 42, (at - info_start) as u16);
        put32(out, 44, info_start as u32);
        at
    }

    fn tree_connect(&mut self, chain: &mut Chain, msg: &[u8], body: &mut [u8]) -> Result<usize, u32> {
        need(msg, 9)?;
        let offset = get16(msg, HEADER + 4) as usize;
        let length = get16(msg, HEADER + 6) as usize;
        let name = msg.get(offset..offset + length).ok_or(INVALID_PARAMETER)?;
        let mut path = [0u8; MAX_PATH];
        let used = name_to_utf8(name, &mut path);
        let path = &path[..used];
        let share = path.rsplit(|&c| c == b'/').next().unwrap_or(path);
        if !share.eq_ignore_ascii_case(self.share.name().as_bytes()) {
            return Err(BAD_NETWORK_NAME);
        }
        self.tree = true;
        chain.tree = TREE_ID;
        body[..16].fill(0);
        put16(body, 0, 16);
        body[2] = 1; // disk
        // read and execute, or everything but the owner-only rights on a share that takes changes
        put32(body, 12, if self.share.writable() { 0x001F_01FF } else { 0x0012_00A9 });
        Ok(16)
    }

    /// The file id at `at` in the request, or the chain's last one when the request is related and says "all ones".
    fn slot(&self, chain: &Chain, related: bool, msg: &[u8], at: usize) -> Result<usize, u32> {
        let persistent = get64(msg, at);
        let volatile = get64(msg, at + 8);
        let slot = if related && persistent == u64::MAX && volatile == u64::MAX {
            chain.file.ok_or(INVALID_PARAMETER)?
        } else {
            if persistent != volatile || persistent == 0 || persistent > OPENS as u64 {
                return Err(FILE_CLOSED);
            }
            persistent as usize - 1
        };
        if self.opens[slot].used { Ok(slot) } else { Err(FILE_CLOSED) }
    }

    fn stat_open(&mut self, slot: usize) -> Result<Stat, u32> {
        let open = self.opens[slot];
        self.share.stat(open.path()).ok_or(NAME_NOT_FOUND)
    }

    /// `name` (UTF-16 from the wire) as a clean share path: no leading or trailing slash, no "." or ".." parts.
    fn clean<'a>(name: &[u8], path: &'a mut [u8; MAX_PATH]) -> Result<&'a str, u32> {
        let mut used = name_to_utf8(name, path);
        if used == MAX_PATH {
            return Err(NAME_INVALID);
        }
        while used > 0 && path[used - 1] == b'/' {
            used -= 1;
        }
        let start = path[..used].iter().position(|&c| c != b'/').unwrap_or(used);
        let text = core::str::from_utf8(&path[start..used]).map_err(|_| NAME_INVALID)?;
        if text.split('/').any(|part| part == ".." || part == ".") && !text.is_empty() {
            return Err(NAME_INVALID);
        }
        Ok(text)
    }

    /// The folder part of `path` ("" for a name in the root).
    fn parent(path: &str) -> &str {
        path.rfind('/').map_or("", |at| &path[..at])
    }

    /// Whether the session may add, remove or rename entries in the folder `path`: write and search.
    fn may_change(&mut self, folder: &str) -> Result<(), u32> {
        let stat = self.share.stat(folder).ok_or(NAME_NOT_FOUND)?;
        if !stat.dir {
            return Err(NOT_A_DIRECTORY);
        }
        if self.allowed(&stat, 3) { Ok(()) } else { Err(ACCESS_DENIED) }
    }

    fn create(&mut self, chain: &mut Chain, msg: &[u8], body: &mut [u8]) -> Result<usize, u32> {
        need(msg, 57)?;
        let access = get32(msg, HEADER + 24);
        let disposition = get32(msg, HEADER + 36);
        let options = get32(msg, HEADER + 40);
        let offset = get16(msg, HEADER + 44) as usize;
        let length = get16(msg, HEADER + 46) as usize;
        let name = if length == 0 { &[][..] } else { msg.get(offset..offset + length).ok_or(INVALID_PARAMETER)? };
        // WRITE_DATA, APPEND_DATA, DELETE, GENERIC_ALL, GENERIC_WRITE
        let write = access & (0x2 | 0x4 | 0x1_0000 | 0x1000_0000 | 0x4000_0000) != 0;
        let data_write = access & (0x2 | 0x4 | 0x1000_0000 | 0x4000_0000) != 0;
        let delete = options & 0x1000 != 0;
        let wants_read = access & (0x1 | 0x1000_0000 | 0x8000_0000) != 0;
        if disposition > 5 {
            return Err(INVALID_PARAMETER);
        }
        // Dispositions: 0 supersede, 1 open, 2 create, 3 open or create, 4 overwrite, 5 overwrite or create.
        let may_create = matches!(disposition, 0 | 2 | 3 | 5);
        let replaces = matches!(disposition, 0 | 4 | 5);
        if (write || delete || disposition >= 2) && !self.share.writable() {
            return Err(ACCESS_DENIED);
        }
        let mut path = [0u8; MAX_PATH];
        let text = Self::clean(name, &mut path)?;
        // Every folder on the way needs the search bit, the target the read bit.
        let folders = core::iter::once(0).chain(text.match_indices('/').map(|(i, _)| i));
        for end in folders {
            let folder = self.share.stat(&text[..end]).ok_or(NAME_NOT_FOUND)?;
            if !self.allowed(&folder, 1) {
                return Err(ACCESS_DENIED);
            }
        }
        let mut action = 1; // FILE_OPENED
        let stat = match self.share.stat(text) {
            Some(stat) => {
                if disposition == 2 {
                    return Err(NAME_COLLISION);
                }
                if (!write || wants_read) && !self.allowed(&stat, 4) {
                    return Err(ACCESS_DENIED);
                }
                if data_write && !stat.dir && !self.allowed(&stat, 2) {
                    return Err(ACCESS_DENIED);
                }
                if options & 1 != 0 && !stat.dir {
                    return Err(NOT_A_DIRECTORY);
                }
                if options & 0x40 != 0 && stat.dir {
                    return Err(IS_A_DIRECTORY);
                }
                if replaces {
                    if stat.dir {
                        return Err(IS_A_DIRECTORY);
                    }
                    if !self.allowed(&stat, 2) {
                        return Err(ACCESS_DENIED);
                    }
                    self.share.truncate(text, 0).map_err(Fail::status)?;
                    action = if disposition == 0 { 0 } else { 3 };
                }
                if delete || access & 0x1_0000 != 0 && stat.dir {
                    self.may_change(Self::parent(text))?;
                }
                if replaces { self.share.stat(text).unwrap_or(stat) } else { stat }
            }
            None => {
                if !may_create {
                    return Err(NAME_NOT_FOUND);
                }
                let dir = options & 1 != 0;
                if text.is_empty() {
                    return Err(ACCESS_DENIED);
                }
                self.may_change(Self::parent(text))?;
                let identity = self.identity;
                self.share.create(text, dir, if dir { 0o755 } else { 0o644 }, identity).map_err(Fail::status)?;
                action = 2; // FILE_CREATED
                self.share.stat(text).ok_or(IO_ERROR)?
            }
        };
        let slot = self.opens.iter().position(|open| !open.used).ok_or(NO_RESOURCES)?;
        let mut open = Open::EMPTY;
        open.used = true;
        open.dir = stat.dir;
        open.write = write || delete;
        open.delete = delete;
        open.length = text.len();
        open.path[..text.len()].copy_from_slice(text.as_bytes());
        self.opens[slot] = open;
        chain.file = Some(slot);

        if body.len() < 96 {
            return Err(NO_RESOURCES);
        }
        body[..89].fill(0);
        put16(body, 0, 89);
        put32(body, 4, action);
        let mtime = if stat.mtime == 0 { self.now } else { stat.mtime };
        put_times(body, 8, mtime);
        put64(body, 40, allocation(&stat));
        put64(body, 48, stat.size);
        put32(body, 56, attributes(&stat));
        put64(body, 64, slot as u64 + 1);
        put64(body, 72, slot as u64 + 1);
        Ok(89)
    }

    fn close(&mut self, chain: &mut Chain, related: bool, msg: &[u8], body: &mut [u8]) -> Result<usize, u32> {
        need(msg, 24)?;
        let slot = self.slot(chain, related, msg, HEADER + 8)?;
        let flags = get16(msg, HEADER + 2);
        let stat = if flags & 1 != 0 { self.stat_open(slot).ok() } else { None };
        let open = self.opens[slot];
        self.opens[slot] = Open::EMPTY;
        if open.delete {
            self.share.remove(open.path()).map_err(Fail::status)?;
        }
        body[..60].fill(0);
        put16(body, 0, 60);
        if let Some(stat) = stat {
            put16(body, 2, 1);
            let mtime = if stat.mtime == 0 { self.now } else { stat.mtime };
            put_times(body, 8, mtime);
            put64(body, 40, allocation(&stat));
            put64(body, 48, stat.size);
            put32(body, 56, attributes(&stat));
        }
        Ok(60)
    }

    fn read(&mut self, chain: &mut Chain, related: bool, msg: &[u8], body: &mut [u8]) -> Result<usize, u32> {
        need(msg, 49)?;
        let length = get32(msg, HEADER + 4) as usize;
        let offset = get64(msg, HEADER + 8);
        let slot = self.slot(chain, related, msg, HEADER + 16)?;
        let open = self.opens[slot];
        if open.dir {
            return Err(IS_A_DIRECTORY);
        }
        let capacity = length.min(MAX_IO).min(body.len().saturating_sub(16));
        let read = self.share.read(open.path(), offset, &mut body[16..16 + capacity]).map_err(|_| IO_ERROR)?;
        if read == 0 && length > 0 {
            return Err(END_OF_FILE);
        }
        body[..16].fill(0);
        put16(body, 0, 17);
        body[2] = (HEADER + 16) as u8;
        put32(body, 4, read as u32);
        Ok(16 + read)
    }

    fn write(&mut self, chain: &mut Chain, related: bool, msg: &[u8], body: &mut [u8]) -> Result<usize, u32> {
        need(msg, 49)?;
        let data_at = get16(msg, HEADER + 2) as usize;
        let length = get32(msg, HEADER + 4) as usize;
        let offset = get64(msg, HEADER + 8);
        let slot = self.slot(chain, related, msg, HEADER + 16)?;
        let open = self.opens[slot];
        if open.dir {
            return Err(IS_A_DIRECTORY);
        }
        if !open.write {
            return Err(ACCESS_DENIED);
        }
        let data = msg.get(data_at..data_at.checked_add(length).ok_or(INVALID_PARAMETER)?).ok_or(INVALID_PARAMETER)?;
        if !data.is_empty() {
            self.share.write(open.path(), offset, data).map_err(Fail::status)?;
        }
        body[..16].fill(0);
        put16(body, 0, 17);
        put32(body, 4, length as u32);
        Ok(16)
    }

    fn set_info(&mut self, chain: &mut Chain, related: bool, msg: &[u8], body: &mut [u8]) -> Result<usize, u32> {
        need(msg, 33)?;
        let kind = msg[HEADER + 2];
        let class = msg[HEADER + 3];
        let length = get32(msg, HEADER + 4) as usize;
        let at = get16(msg, HEADER + 8) as usize;
        let slot = self.slot(chain, related, msg, HEADER + 16)?;
        let buffer = msg.get(at..at.checked_add(length).ok_or(INVALID_PARAMETER)?).ok_or(INVALID_PARAMETER)?;
        if kind != 1 {
            return Err(NOT_SUPPORTED);
        }
        let open = self.opens[slot];
        match class {
            // Basic (times, attributes) and allocation size: nothing to keep.
            4 | 19 => {}
            // FileRenameInformation: replace flag, then a UTF-16 name relative to the share root.
            10 => {
                if !open.write || !self.share.writable() {
                    return Err(ACCESS_DENIED);
                }
                if buffer.len() < 20 {
                    return Err(INVALID_PARAMETER);
                }
                let replace = buffer[0] != 0;
                let name_length = get32(buffer, 16) as usize;
                let name = buffer.get(20..20 + name_length).ok_or(INVALID_PARAMETER)?;
                let mut path = [0u8; MAX_PATH];
                let target = Self::clean(name, &mut path)?;
                if target.is_empty() || open.path().is_empty() {
                    return Err(ACCESS_DENIED);
                }
                if target != open.path() {
                    self.may_change(Self::parent(open.path()))?;
                    self.may_change(Self::parent(target))?;
                    if let Some(existing) = self.share.stat(target) {
                        if !replace {
                            return Err(NAME_COLLISION);
                        }
                        if existing.dir != open.dir {
                            return Err(if existing.dir { IS_A_DIRECTORY } else { NOT_A_DIRECTORY });
                        }
                    }
                    self.share.rename(open.path(), target).map_err(Fail::status)?;
                    let entry = &mut self.opens[slot];
                    entry.length = target.len();
                    entry.path[..target.len()].copy_from_slice(target.as_bytes());
                }
            }
            // FileDispositionInformation: delete when closed.
            13 => {
                let pending = buffer.first().is_some_and(|&flag| flag != 0);
                if pending {
                    if !open.write || !self.share.writable() {
                        return Err(ACCESS_DENIED);
                    }
                    self.may_change(Self::parent(open.path()))?;
                    if open.dir && self.share.read_dir(open.path(), 0, &mut [0u8; MAX_PATH]).is_some() {
                        return Err(DIRECTORY_NOT_EMPTY);
                    }
                }
                self.opens[slot].delete = pending;
            }
            // FileEndOfFileInformation.
            20 => {
                if open.dir {
                    return Err(IS_A_DIRECTORY);
                }
                if !open.write {
                    return Err(ACCESS_DENIED);
                }
                if buffer.len() < 8 {
                    return Err(INVALID_PARAMETER);
                }
                self.share.truncate(open.path(), get64(buffer, 0)).map_err(Fail::status)?;
            }
            _ => return Err(NOT_SUPPORTED),
        }
        put16(body, 0, 2);
        Ok(2)
    }

    /// One directory entry in the layout of information class `class`; returns its length before padding.
    fn dir_entry(buffer: &mut [u8], class: u8, index: u32, name: &[u8], stat: &Stat, now: u64) -> Option<usize> {
        let (name_at, length_at) = match class {
            1 => (64, 60),
            2 => (68, 60),
            3 => (94, 60),
            0x25 => (104, 60),
            0x26 => (80, 60),
            12 => (12, 8),
            _ => return None,
        };
        if buffer.len() < name_at {
            return None;
        }
        buffer[..name_at].fill(0);
        let units = put_utf16(buffer, name_at, name)?;
        put32(buffer, 4, index);
        put32(buffer, length_at, units as u32);
        if class != 12 {
            let mtime = if stat.mtime == 0 { now } else { stat.mtime };
            put_times(buffer, 8, mtime);
            put64(buffer, 40, stat.size);
            put64(buffer, 48, allocation(stat));
            put32(buffer, 56, attributes(stat));
            match class {
                0x25 => put64(buffer, 96, u64::from(index) + 1),
                0x26 => put64(buffer, 72, u64::from(index) + 1),
                _ => {}
            }
        }
        Some(name_at + units)
    }

    fn query_directory(&mut self, chain: &mut Chain, related: bool, msg: &[u8], body: &mut [u8]) -> Result<usize, u32> {
        need(msg, 33)?;
        let class = msg[HEADER + 2];
        let flags = msg[HEADER + 3];
        let slot = self.slot(chain, related, msg, HEADER + 8)?;
        let pattern_at = get16(msg, HEADER + 24) as usize;
        let pattern_length = get16(msg, HEADER + 26) as usize;
        let limit = (get32(msg, HEADER + 28) as usize).min(MAX_IO).min(body.len().saturating_sub(8));
        let mut pattern = [0u8; MAX_PATH];
        let pattern_used = if pattern_length == 0 {
            pattern[0] = b'*';
            1
        } else {
            let raw = msg.get(pattern_at..pattern_at + pattern_length).ok_or(INVALID_PARAMETER)?;
            name_to_utf8(raw, &mut pattern)
        };
        let pattern = &pattern[..pattern_used];
        let open = self.opens[slot];
        if !open.dir {
            return Err(NOT_A_DIRECTORY);
        }
        if !matches!(class, 1 | 2 | 3 | 12 | 0x25 | 0x26) {
            return Err(NOT_SUPPORTED);
        }
        let first = flags & 1 != 0 || open.next == 0;
        if first {
            self.opens[slot].next = 0;
        }
        let single = flags & 2 != 0;
        let folder = self.share.stat(open.path()).unwrap_or(Stat { dir: true, ..Stat::default() });
        let mut next = self.opens[slot].next;
        let mut used = 0;
        let mut last_start = 0;
        let mut count = 0;
        let mut overflow = false;
        loop {
            let mut name = [0u8; MAX_PATH];
            let (length, stat) = match next {
                0 | 1 => {
                    name[..next + 1].fill(b'.');
                    (next + 1, folder)
                }
                n => match self.share.read_dir(open.path(), n - 2, &mut name) {
                    Some(found) => found,
                    None => break,
                },
            };
            if !wildcard(pattern, &name[..length]) {
                next += 1;
                continue;
            }
            let target = &mut body[8 + used..8 + limit];
            let entry = Self::dir_entry(target, class, next as u32, &name[..length], &stat, self.now);
            let Some(entry) = entry.filter(|entry| used + ((entry + 7) & !7) <= limit) else {
                overflow = true;
                break;
            };
            let padded = (entry + 7) & !7;
            for byte in &mut body[8 + used + entry..8 + used + padded] {
                *byte = 0;
            }
            if count > 0 {
                put32(body, 8 + last_start, (used - last_start) as u32);
            }
            last_start = used;
            used += padded;
            count += 1;
            next += 1;
            if single {
                break;
            }
        }
        if count == 0 {
            return Err(if overflow {
                LENGTH_MISMATCH
            } else if first {
                NO_SUCH_FILE
            } else {
                NO_MORE_FILES
            });
        }
        self.opens[slot].next = next;
        // The last entry's next-entry offset is 0 (the buffer is zeroed per entry).
        put16(body, 0, 9);
        put16(body, 2, (HEADER + 8) as u16);
        put32(body, 4, used as u32);
        Ok(8 + used)
    }

    fn query_info(&mut self, chain: &mut Chain, related: bool, msg: &[u8], body: &mut [u8]) -> Result<usize, u32> {
        need(msg, 41)?;
        let kind = msg[HEADER + 2];
        let class = msg[HEADER + 3];
        let limit = (get32(msg, HEADER + 4) as usize).min(MAX_IO).min(body.len().saturating_sub(8));
        let slot = self.slot(chain, related, msg, HEADER + 24)?;
        let mut info = [0u8; 512];
        let length = match kind {
            1 => {
                let stat = self.stat_open(slot)?;
                let open = self.opens[slot];
                let mtime = if stat.mtime == 0 { self.now } else { stat.mtime };
                match class {
                    4 => {
                        put_times(&mut info, 0, mtime);
                        put32(&mut info, 32, attributes(&stat));
                        40
                    }
                    5 => {
                        put64(&mut info, 0, allocation(&stat));
                        put64(&mut info, 8, stat.size);
                        put32(&mut info, 16, 1);
                        info[21] = stat.dir as u8;
                        24
                    }
                    6 => {
                        put64(&mut info, 0, slot as u64 + 1);
                        8
                    }
                    7 => 4,
                    14 => 8,
                    0x23 => {
                        put32(&mut info, 0, attributes(&stat));
                        8
                    }
                    0x22 => {
                        put_times(&mut info, 0, mtime);
                        put64(&mut info, 32, allocation(&stat));
                        put64(&mut info, 40, stat.size);
                        put32(&mut info, 48, attributes(&stat));
                        56
                    }
                    0x12 => {
                        put_times(&mut info, 0, mtime);
                        put32(&mut info, 32, attributes(&stat));
                        put64(&mut info, 40, allocation(&stat));
                        put64(&mut info, 48, stat.size);
                        put32(&mut info, 56, 1);
                        info[61] = stat.dir as u8;
                        put64(&mut info, 64, slot as u64 + 1);
                        put32(&mut info, 88, 0x0012_00A9);
                        let name = open.path().rsplit('/').next().unwrap_or("");
                        let units = put_utf16(&mut info, 104, name.as_bytes()).ok_or(INVALID_PARAMETER)?;
                        put32(&mut info, 100, units as u32);
                        104 + units
                    }
                    9 => {
                        let units = put_utf16(&mut info, 4, open.path().as_bytes()).ok_or(INVALID_PARAMETER)?;
                        put32(&mut info, 0, units as u32);
                        4 + units
                    }
                    _ => return Err(NOT_SUPPORTED),
                }
            }
            2 => {
                let (total, free) = self.share.space();
                match class {
                    1 => {
                        put64(&mut info, 0, filetime(self.now));
                        put32(&mut info, 4, 0x4f53);
                        put32(&mut info, 8, 6);
                        let units = put_utf16(&mut info, 18, b"FOS").ok_or(INVALID_PARAMETER)?;
                        put32(&mut info, 12, units as u32);
                        18 + units
                    }
                    3 => {
                        put64(&mut info, 0, total / 4096);
                        put64(&mut info, 8, free / 4096);
                        put32(&mut info, 16, 8);
                        put32(&mut info, 20, 512);
                        24
                    }
                    4 => {
                        put32(&mut info, 0, 7);
                        put32(&mut info, 4, 0x20); // FILE_DEVICE_IS_MOUNTED
                        8
                    }
                    5 => {
                        put32(&mut info, 0, 0x0000_0003); // case-sensitive search, case-preserved names
                        put32(&mut info, 4, 255);
                        let units = put_utf16(&mut info, 12, b"FOS").ok_or(INVALID_PARAMETER)?;
                        put32(&mut info, 8, units as u32);
                        12 + units
                    }
                    7 => {
                        put64(&mut info, 0, total / 4096);
                        put64(&mut info, 8, free / 4096);
                        put64(&mut info, 16, free / 4096);
                        put32(&mut info, 24, 8);
                        put32(&mut info, 28, 512);
                        32
                    }
                    _ => return Err(NOT_SUPPORTED),
                }
            }
            _ => return Err(NOT_SUPPORTED),
        };
        if length > limit {
            return Err(LENGTH_MISMATCH);
        }
        put16(body, 0, 9);
        put16(body, 2, (HEADER + 8) as u16);
        put32(body, 4, length as u32);
        body[8..8 + length].copy_from_slice(&info[..length]);
        Ok(8 + length)
    }

    /// FSCTL_VALIDATE_NEGOTIATE_INFO: a 3.0 client repeats what it negotiated so a man in the middle cannot have
    /// downgraded it; the answer is what this server would have said.
    fn ioctl(&mut self, msg: &[u8], body: &mut [u8]) -> Result<usize, u32> {
        need(msg, 57)?;
        if get32(msg, HEADER + 4) != 0x0014_0204 {
            return Err(NOT_SUPPORTED);
        }
        if body.len() < 48 + 24 {
            return Err(NO_RESOURCES);
        }
        body[..48].fill(0);
        put16(body, 0, 49);
        put32(body, 4, 0x0014_0204);
        put64(body, 8, u64::MAX);
        put64(body, 16, u64::MAX);
        put32(body, 32, (HEADER + 48) as u32);
        put32(body, 36, 24);
        put32(body, 44, 0);
        body[48..52].fill(0);
        body[52..68].copy_from_slice(&self.guid);
        put16(body, 68, 1);
        put16(body, 70, self.dialect);
        Ok(72)
    }
}
