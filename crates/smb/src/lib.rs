//! A small read-only SMB 2.1 client: negotiate, anonymous (guest) session, tree connect, directory listing, file reads.
//! Written for `fosplay`, which streams music from a Samba or NAS share, and meant to stay that small: no signing, no
//! encryption, no Kerberos, no writes. A server that insists on signing or on a user name refuses the session and the error
//! says so.
//!
//! The crate does no I/O of its own: it builds requests and parses answers in one buffer and moves bytes through a
//! `Transport` (a TCP socket in a tape, a `TcpStream` in the host tests). One request is in flight at a time.
//!
//! Wire facts (MS-SMB2): every message is a 4-byte NetBIOS-style frame header (0, then a 24-bit length) and a 64-byte SMB2
//! header; all integers are little-endian; names are UTF-16LE with backslashes. We offer only dialects 2.0.2 and 2.1, so
//! there are no multi-credit requests (a read is at most 64 KiB) and no preauthentication hash.
//!
//! Authentication: NTLMSSP *anonymous*: a Negotiate message, then an Authenticate message with an empty user name and no
//! challenge responses (flag `NEGOTIATE_ANONYMOUS`). Samba with `map to guest = bad user`, a NAS with guest access and
//! Windows with null sessions allowed accept this for a share that allows guests; it needs no hashing at all. The blob is
//! sent raw, not wrapped in SPNEGO, which Samba accepts.
#![no_std]

/// Moves bytes to and from the server.
pub trait Transport {
    fn send(&mut self, data: &[u8]) -> Result<(), Error>;
    /// Fills `out` completely or fails.
    fn recv_exact(&mut self, out: &mut [u8]) -> Result<(), Error>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The transport failed (closed, timed out).
    Transport,
    /// The server's answer did not look like SMB2 (what was wrong).
    Protocol(&'static str),
    /// The server answered with this NT status.
    Status(u32),
    /// A message does not fit the client's buffer.
    TooBig,
}

impl Error {
    pub fn describe(&self) -> &'static str {
        match self {
            Error::Transport => "the connection failed",
            Error::Protocol(why) => why,
            Error::Status(status) => status_name(*status),
            Error::TooBig => "a message did not fit the buffer",
        }
    }
}

pub const STATUS_PENDING: u32 = 0x0000_0103;
pub const STATUS_MORE_PROCESSING: u32 = 0xC000_0016;
pub const STATUS_NO_MORE_FILES: u32 = 0x8000_0006;
pub const STATUS_END_OF_FILE: u32 = 0xC000_0011;

pub fn status_name(status: u32) -> &'static str {
    match status {
        0 => "success",
        0x8000_0006 => "no more files",
        0xC000_0011 => "end of file",
        0xC000_0022 => "access denied",
        0xC000_0034 | 0xC000_003A => "no such file or directory",
        0xC000_006D => "logon failure (the server wants a user name)",
        0xC000_006E => "account restriction",
        0xC000_0072 => "account disabled",
        0xC000_00BB => "not supported",
        0xC000_00CC => "no such share",
        0xC000_00D0 => "request not accepted",
        0xC000_0103 => "not a directory",
        0xC000_0016 => "more processing required",
        _ => "the server refused the request",
    }
}

const CMD_NEGOTIATE: u16 = 0;
const CMD_SESSION_SETUP: u16 = 1;
const CMD_TREE_CONNECT: u16 = 3;
const CMD_CREATE: u16 = 5;
const CMD_CLOSE: u16 = 6;
const CMD_READ: u16 = 8;
const CMD_QUERY_DIRECTORY: u16 = 14;

const HEADER: usize = 64;
/// Request layout in the buffer: 4 bytes of frame header, then the SMB2 header, then the body.
const BODY: usize = 4 + HEADER;
const NTLMSSP: &[u8; 8] = b"NTLMSSP\0";
/// UNICODE | REQUEST_TARGET | NTLM | ALWAYS_SIGN | EXTENDED_SESSIONSECURITY | 128 | 56
const NTLM_FLAGS: u32 = 0xA008_8205;
const NTLM_ANONYMOUS: u32 = 0x0000_0800;

pub const ATTRIBUTE_DIRECTORY: u32 = 0x10;

/// An open file or directory.
#[derive(Clone, Copy)]
pub struct Handle {
    id: [u8; 16],
    pub size: u64,
}

/// One entry of a directory listing; `name` is UTF-16LE as the server sent it.
pub struct DirEntry<'a> {
    pub name: &'a [u8],
    pub size: u64,
    pub attributes: u32,
}

impl DirEntry<'_> {
    pub fn is_dir(&self) -> bool {
        self.attributes & ATTRIBUTE_DIRECTORY != 0
    }

    /// Writes the name as UTF-8 to `out` (cut off where it no longer fits) and returns the length.
    pub fn name_utf8(&self, out: &mut [u8]) -> usize {
        let units = self.name.chunks_exact(2).map(|pair| u16::from_le_bytes([pair[0], pair[1]]));
        let mut length = 0;
        for character in char::decode_utf16(units) {
            let character = character.unwrap_or('\u{fffd}');
            if length + character.len_utf8() > out.len() {
                break;
            }
            length += character.encode_utf8(&mut out[length..]).len();
        }
        length
    }
}

/// The client. `N` is the buffer size: at least 512 plus the largest read or directory chunk wanted; 66 KiB allows the
/// protocol's 64 KiB reads.
pub struct Client<T: Transport, const N: usize> {
    transport: T,
    buffer: [u8; N],
    message_id: u64,
    session: u64,
    tree: u32,
    max_read: u32,
}

fn put16(buffer: &mut [u8], at: usize, value: u16) {
    buffer[at..at + 2].copy_from_slice(&value.to_le_bytes());
}
fn put32(buffer: &mut [u8], at: usize, value: u32) {
    buffer[at..at + 4].copy_from_slice(&value.to_le_bytes());
}
fn put64(buffer: &mut [u8], at: usize, value: u64) {
    buffer[at..at + 8].copy_from_slice(&value.to_le_bytes());
}
fn get16(buffer: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([buffer[at], buffer[at + 1]])
}
fn get32(buffer: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([buffer[at], buffer[at + 1], buffer[at + 2], buffer[at + 3]])
}
fn get64(buffer: &[u8], at: usize) -> u64 {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&buffer[at..at + 8]);
    u64::from_le_bytes(bytes)
}

/// Writes `text` as UTF-16LE at `at`, '/' becoming '\'; returns the byte count.
fn put_name(buffer: &mut [u8], at: usize, text: &str) -> Result<usize, Error> {
    let mut length = 0;
    for unit in text.encode_utf16() {
        if at + length + 2 > buffer.len() {
            return Err(Error::TooBig);
        }
        let unit = if unit == b'/' as u16 { b'\\' as u16 } else { unit };
        put16(buffer, at + length, unit);
        length += 2;
    }
    Ok(length)
}

impl<T: Transport, const N: usize> Client<T, N> {
    pub const fn new(transport: T) -> Self {
        Client { transport, buffer: [0; N], message_id: 0, session: 0, tree: 0, max_read: 0 }
    }

    pub fn transport(&mut self) -> &mut T {
        &mut self.transport
    }

    /// Dialect negotiation, anonymous session and the share: `\\host\share`.
    pub fn connect(&mut self, host: &str, share: &str) -> Result<(), Error> {
        self.message_id = 0;
        self.session = 0;
        self.tree = 0;
        self.negotiate()?;
        self.session_setup()?;
        self.tree_connect(host, share)
    }

    /// Starts a request: clears the header and the start of the body area and fills the header. The body is then written at
    /// `BODY`; `finish` sends it.
    fn begin(&mut self, command: u16) {
        let buffer = &mut self.buffer;
        buffer[..BODY + 128].fill(0);
        buffer[4..8].copy_from_slice(&[0xFE, b'S', b'M', b'B']);
        put16(buffer, 8, HEADER as u16);
        put16(buffer, 10, if command == CMD_NEGOTIATE { 0 } else { 1 }); // credit charge
        put16(buffer, 16, command);
        put16(buffer, 18, 16); // credits requested
        put64(buffer, 28, self.message_id);
        put32(buffer, 40, self.tree);
        put64(buffer, 44, self.session);
        self.message_id += 1;
    }

    /// Sends the request whose body is `body` bytes long (the frame header gets its length here).
    fn finish(&mut self, body: usize) -> Result<(), Error> {
        let length = HEADER + body;
        self.buffer[0] = 0;
        self.buffer[1] = (length >> 16) as u8;
        self.buffer[2] = (length >> 8) as u8;
        self.buffer[3] = length as u8;
        self.transport.send(&self.buffer[..4 + length])
    }

    /// Reads the next answer (skipping interim "pending" ones) into the buffer's start: the SMB2 header at 0, the body at
    /// 64. Returns the status and the message length.
    fn receive(&mut self, command: u16) -> Result<(u32, usize), Error> {
        loop {
            let mut frame = [0u8; 4];
            self.transport.recv_exact(&mut frame)?;
            if frame[0] != 0 {
                return Err(Error::Protocol("not an SMB2 frame"));
            }
            let length = ((frame[1] as usize) << 16) | ((frame[2] as usize) << 8) | frame[3] as usize;
            if length < HEADER {
                return Err(Error::Protocol("a frame shorter than an SMB2 header"));
            }
            if length > N {
                return Err(Error::TooBig);
            }
            self.transport.recv_exact(&mut self.buffer[..length])?;
            let buffer = &self.buffer;
            if buffer[..4] != [0xFE, b'S', b'M', b'B'] {
                return Err(Error::Protocol("the answer is not SMB2 (the server speaks only SMB1?)"));
            }
            let status = get32(buffer, 8);
            if status == STATUS_PENDING {
                continue;
            }
            if get16(buffer, 12) != command {
                return Err(Error::Protocol("the answer is for another command"));
            }
            return Ok((status, length));
        }
    }

    fn negotiate(&mut self) -> Result<(), Error> {
        self.begin(CMD_NEGOTIATE);
        let buffer = &mut self.buffer;
        put16(buffer, BODY, 36);
        put16(buffer, BODY + 2, 2); // two dialects
        put16(buffer, BODY + 4, 1); // signing enabled (not required)
        buffer[BODY + 12..BODY + 28].copy_from_slice(b"fosfor-smb-guid!");
        put16(buffer, BODY + 36, 0x0202);
        put16(buffer, BODY + 38, 0x0210);
        self.finish(36 + 4)?;
        let (status, length) = self.receive(CMD_NEGOTIATE)?;
        if status != 0 {
            return Err(Error::Status(status));
        }
        if length < HEADER + 64 {
            return Err(Error::Protocol("a short negotiate answer"));
        }
        let dialect = get16(&self.buffer, HEADER + 4);
        if dialect != 0x0202 && dialect != 0x0210 {
            return Err(Error::Protocol("the server chose a dialect we did not offer"));
        }
        // Without the large-MTU capability a read is at most 64 KiB whatever the server would allow.
        self.max_read = get32(&self.buffer, HEADER + 32).min(65536);
        Ok(())
    }

    fn session_setup(&mut self) -> Result<(), Error> {
        // Message 1: NTLM Negotiate (32 bytes, no domain or workstation).
        self.begin(CMD_SESSION_SETUP);
        let buffer = &mut self.buffer;
        put16(buffer, BODY, 25);
        buffer[BODY + 3] = 1; // signing enabled
        put16(buffer, BODY + 12, 88);
        put16(buffer, BODY + 14, 32);
        let blob = BODY + 24;
        buffer[blob..blob + 8].copy_from_slice(NTLMSSP);
        put32(buffer, blob + 8, 1);
        put32(buffer, blob + 12, NTLM_FLAGS);
        self.finish(24 + 32)?;
        let (status, _) = self.receive(CMD_SESSION_SETUP)?;
        if status != STATUS_MORE_PROCESSING {
            return Err(Error::Status(status));
        }
        self.session = get64(&self.buffer, 40);

        // Message 2: NTLM Authenticate with an empty user name and no responses (the anonymous form; the LM response is
        // one zero byte). Field order: LM, NT, domain, user, workstation, session key, each (length, max, offset).
        self.begin(CMD_SESSION_SETUP);
        let buffer = &mut self.buffer;
        put16(buffer, BODY, 25);
        buffer[BODY + 3] = 1;
        put16(buffer, BODY + 12, 88);
        put16(buffer, BODY + 14, 65);
        let blob = BODY + 24;
        buffer[blob..blob + 8].copy_from_slice(NTLMSSP);
        put32(buffer, blob + 8, 3);
        put16(buffer, blob + 12, 1);
        put16(buffer, blob + 14, 1);
        put32(buffer, blob + 16, 64);
        for at in [20, 28, 36, 44, 52] {
            put32(buffer, blob + at + 4, 65);
        }
        put32(buffer, blob + 60, NTLM_FLAGS | NTLM_ANONYMOUS);
        buffer[blob + 64] = 0;
        self.finish(24 + 65)?;
        let (status, _) = self.receive(CMD_SESSION_SETUP)?;
        if status != 0 {
            return Err(Error::Status(status));
        }
        Ok(())
    }

    fn tree_connect(&mut self, host: &str, share: &str) -> Result<(), Error> {
        self.begin(CMD_TREE_CONNECT);
        let buffer = &mut self.buffer;
        put16(buffer, BODY, 9);
        put16(buffer, BODY + 4, 72);
        let mut at = BODY + 8;
        at += put_name(buffer, at, "\\\\")?;
        at += put_name(buffer, at, host)?;
        at += put_name(buffer, at, "\\")?;
        at += put_name(buffer, at, share)?;
        let path = at - (BODY + 8);
        put16(buffer, BODY + 6, path as u16);
        self.finish(8 + path)?;
        let (status, _) = self.receive(CMD_TREE_CONNECT)?;
        if status != 0 {
            return Err(Error::Status(status));
        }
        self.tree = get32(&self.buffer, 36);
        Ok(())
    }

    /// Opens a file (`directory` false) or directory by its path below the share ("" is the share's root; '/' or '\'
    /// separate).
    pub fn open(&mut self, path: &str, directory: bool) -> Result<Handle, Error> {
        self.begin(CMD_CREATE);
        let buffer = &mut self.buffer;
        put16(buffer, BODY, 57);
        put32(buffer, BODY + 4, 2); // impersonation
        put32(buffer, BODY + 24, 0x0012_0089); // read data, attributes, read control, synchronize
        put32(buffer, BODY + 32, 7); // share read, write, delete
        put32(buffer, BODY + 36, 1); // open existing
        put32(buffer, BODY + 40, if directory { 0x01 } else { 0x40 });
        put16(buffer, BODY + 44, 120);
        let name = put_name(buffer, BODY + 56, path.trim_start_matches(['/', '\\']))?;
        put16(buffer, BODY + 46, name as u16);
        self.finish(56 + name.max(1))?;
        let (status, length) = self.receive(CMD_CREATE)?;
        if status != 0 {
            return Err(Error::Status(status));
        }
        if length < HEADER + 80 {
            return Err(Error::Protocol("a short create answer"));
        }
        let mut id = [0u8; 16];
        id.copy_from_slice(&self.buffer[HEADER + 64..HEADER + 80]);
        Ok(Handle { id, size: get64(&self.buffer, HEADER + 48) })
    }

    pub fn close(&mut self, handle: &Handle) -> Result<(), Error> {
        self.begin(CMD_CLOSE);
        put16(&mut self.buffer, BODY, 24);
        self.buffer[BODY + 8..BODY + 24].copy_from_slice(&handle.id);
        self.finish(24)?;
        let (status, _) = self.receive(CMD_CLOSE)?;
        if status != 0 { Err(Error::Status(status)) } else { Ok(()) }
    }

    /// The most one `read` returns.
    pub fn max_read(&self) -> usize {
        (self.max_read as usize).min(N.saturating_sub(HEADER + 16)).max(1)
    }

    /// Reads up to `out.len()` bytes (at most `max_read`) at `offset`; 0 at the end of the file.
    pub fn read(&mut self, handle: &Handle, offset: u64, out: &mut [u8]) -> Result<usize, Error> {
        let wanted = out.len().min(self.max_read());
        self.begin(CMD_READ);
        let buffer = &mut self.buffer;
        put16(buffer, BODY, 49);
        buffer[BODY + 2] = 0x50; // padding: the data offset we would like (informational)
        put32(buffer, BODY + 4, wanted as u32);
        put64(buffer, BODY + 8, offset);
        buffer[BODY + 16..BODY + 32].copy_from_slice(&handle.id);
        self.finish(49)?;
        let (status, length) = self.receive(CMD_READ)?;
        if status == STATUS_END_OF_FILE {
            return Ok(0);
        }
        if status != 0 {
            return Err(Error::Status(status));
        }
        let buffer = &self.buffer;
        let start = buffer[HEADER + 2] as usize;
        let count = get32(buffer, HEADER + 4) as usize;
        if start < HEADER + 16 || start + count > length || count > out.len() {
            return Err(Error::Protocol("a read answer with a bad data range"));
        }
        out[..count].copy_from_slice(&buffer[start..start + count]);
        Ok(count)
    }

    /// Calls `visit` for every entry of the directory `path` ("" for the share's root), "." and ".." included.
    pub fn list(&mut self, path: &str, mut visit: impl FnMut(&DirEntry)) -> Result<(), Error> {
        let directory = self.open(path, true)?;
        let result = self.list_open(&directory, &mut visit);
        let closed = self.close(&directory);
        result.and(closed)
    }

    fn list_open(&mut self, directory: &Handle, visit: &mut impl FnMut(&DirEntry)) -> Result<(), Error> {
        let mut restart = true;
        loop {
            self.begin(CMD_QUERY_DIRECTORY);
            let output = (N - HEADER - 64).min(16384);
            let buffer = &mut self.buffer;
            put16(buffer, BODY, 33);
            buffer[BODY + 2] = 3; // FileBothDirectoryInformation
            buffer[BODY + 3] = if restart { 1 } else { 0 };
            buffer[BODY + 8..BODY + 24].copy_from_slice(&directory.id);
            put16(buffer, BODY + 24, 96);
            put16(buffer, BODY + 26, 2);
            put32(buffer, BODY + 28, output as u32);
            put16(buffer, BODY + 32, b'*' as u16);
            self.finish(32 + 2)?;
            restart = false;
            let (status, length) = self.receive(CMD_QUERY_DIRECTORY)?;
            if status == STATUS_NO_MORE_FILES {
                return Ok(());
            }
            if status != 0 {
                return Err(Error::Status(status));
            }
            let buffer = &self.buffer;
            let start = get16(buffer, HEADER + 2) as usize;
            let size = get32(buffer, HEADER + 4) as usize;
            if start < HEADER + 8 || start + size > length {
                return Err(Error::Protocol("a directory answer with a bad range"));
            }
            let mut at = start;
            loop {
                if at + 94 > start + size {
                    return Err(Error::Protocol("a truncated directory entry"));
                }
                let next = get32(buffer, at) as usize;
                let name_length = get32(buffer, at + 60) as usize;
                if at + 94 + name_length > start + size {
                    return Err(Error::Protocol("a directory entry name past the end"));
                }
                visit(&DirEntry {
                    name: &buffer[at + 94..at + 94 + name_length],
                    size: get64(buffer, at + 40),
                    attributes: get32(buffer, at + 56),
                });
                if next == 0 {
                    break;
                }
                at += next;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec::Vec;

    /// A scripted server: answers each request by command, remembering what it was sent.
    struct Fake {
        incoming: Vec<u8>,
        requests: Vec<Vec<u8>>,
    }

    fn answer(command: u16, status: u32, session: u64, tree: u32, body: &[u8]) -> Vec<u8> {
        let mut message = std::vec![0u8; 64];
        message[..4].copy_from_slice(&[0xFE, b'S', b'M', b'B']);
        message[8..12].copy_from_slice(&status.to_le_bytes());
        message[12..14].copy_from_slice(&command.to_le_bytes());
        message[36..40].copy_from_slice(&tree.to_le_bytes());
        message[40..48].copy_from_slice(&session.to_le_bytes());
        message.extend_from_slice(body);
        let mut framed = std::vec![0, (message.len() >> 16) as u8, (message.len() >> 8) as u8, message.len() as u8];
        framed.extend(message);
        framed
    }

    impl Transport for Fake {
        fn send(&mut self, data: &[u8]) -> Result<(), Error> {
            self.requests.push(data.to_vec());
            let command = get16(data, 16);
            let reply = match command {
                CMD_NEGOTIATE => {
                    let mut body = std::vec![0u8; 64];
                    body[4..6].copy_from_slice(&0x0210u16.to_le_bytes());
                    body[32..36].copy_from_slice(&(8u32 << 20).to_le_bytes());
                    answer(command, 0, 0, 0, &body)
                }
                CMD_SESSION_SETUP if get64(data, 44) == 0 => answer(command, STATUS_MORE_PROCESSING, 0x77, 0, &[9, 0, 0, 0, 0, 0, 0, 0, 0]),
                CMD_SESSION_SETUP => answer(command, 0, 0x77, 0, &[9, 0, 0, 0, 0, 0, 0, 0, 0]),
                CMD_TREE_CONNECT => answer(command, 0, 0x77, 5, &[16; 16]),
                CMD_CREATE => {
                    let mut body = std::vec![0u8; 88];
                    body[48..56].copy_from_slice(&1000u64.to_le_bytes());
                    body[64..80].copy_from_slice(&[7; 16]);
                    answer(command, 0, 0x77, 5, &body)
                }
                CMD_READ => {
                    let offset = get64(data, BODY + 8);
                    let mut body = std::vec![0u8; 16];
                    if offset >= 1000 {
                        return Ok(self.incoming.extend(answer(command, STATUS_END_OF_FILE, 0x77, 5, &[0; 9])));
                    }
                    body[2] = 80;
                    body[4..8].copy_from_slice(&10u32.to_le_bytes());
                    body.extend_from_slice(b"0123456789");
                    answer(command, 0, 0x77, 5, &body)
                }
                _ => answer(command, 0, 0x77, 5, &[0; 60]),
            };
            self.incoming.extend(reply);
            Ok(())
        }

        fn recv_exact(&mut self, out: &mut [u8]) -> Result<(), Error> {
            if self.incoming.len() < out.len() {
                return Err(Error::Transport);
            }
            out.copy_from_slice(&self.incoming[..out.len()]);
            self.incoming.drain(..out.len());
            Ok(())
        }
    }

    fn client() -> Client<Fake, 4096> {
        Client::new(Fake { incoming: Vec::new(), requests: Vec::new() })
    }

    #[test]
    fn connect_sends_the_expected_requests_and_carries_session_and_tree() {
        let mut client = client();
        client.connect("host", "share").unwrap();
        let requests = &client.transport.requests;
        assert_eq!(requests.len(), 4);
        assert_eq!(&requests[0][4..8], &[0xFE, b'S', b'M', b'B']);
        assert_eq!(get16(&requests[0], BODY + 2), 2);
        // The anonymous Authenticate: carries the session id, an empty user and domain, the anonymous flag, nothing stale.
        let authenticate = &requests[2];
        assert_eq!(get64(authenticate, 44), 0x77);
        let blob = &authenticate[BODY + 24..];
        assert_eq!(&blob[..12], b"NTLMSSP\0\x03\0\0\0");
        for field in [20, 28, 36, 44, 52] {
            assert_eq!(get16(blob, field), 0, "field {field} is empty");
        }
        assert_eq!(get32(blob, 60) & NTLM_ANONYMOUS, NTLM_ANONYMOUS);
        let tree = &requests[3];
        let path: Vec<u16> = tree[BODY + 8..].chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        assert_eq!(std::string::String::from_utf16(&path).unwrap(), "\\\\host\\share");
        assert_eq!(client.max_read(), 4096 - HEADER - 16);
    }

    #[test]
    fn open_read_and_end_of_file() {
        let mut client = client();
        client.connect("h", "s").unwrap();
        let file = client.open("musik/a.flac", false).unwrap();
        assert_eq!(file.size, 1000);
        let create = client.transport.requests.last().unwrap().clone();
        assert_eq!(get32(&create, 40), 5, "the tree id is sent");
        let name: Vec<u16> = create[BODY + 56..].chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        assert_eq!(std::string::String::from_utf16(&name).unwrap(), "musik\\a.flac");
        let mut out = [0u8; 64];
        assert_eq!(client.read(&file, 0, &mut out).unwrap(), 10);
        assert_eq!(&out[..10], b"0123456789");
        assert_eq!(client.read(&file, 1000, &mut out).unwrap(), 0);
    }

    #[test]
    fn names_decode_to_utf8() {
        let name: Vec<u8> = "Sjöfart.flac".encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        let entry = DirEntry { name: &name, size: 0, attributes: 0 };
        let mut out = [0u8; 32];
        let length = entry.name_utf8(&mut out);
        assert_eq!(&out[..length], "Sjöfart.flac".as_bytes());
    }
}
