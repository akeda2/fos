//! An SSH-2 client with an SFTP v3 client on top, over a blocking byte stream (`Link`): what the `sshmount` tape uses to show
//! a remote machine's folder as a volume.
//!
//! The same algorithms as the server (`session`): `curve25519-sha256` with strict key exchange, host key `ssh-ed25519`,
//! `chacha20-poly1305@openssh.com`, public-key login with one ed25519 key. The caller decides whether the host key is trusted
//! (`connect` takes a function; known_hosts is the tape's business). One SFTP request is in flight at a time: simple, and
//! correct against any server; throughput on a long link is bounded by the round trip, not by the cipher.
//!
//! `Client::connect_shell` opens a terminal (or runs one command) instead of SFTP: `write_input` sends typed bytes,
//! `read_output` returns what the server printed. The link's `recv` may then answer `Error::Timeout` after a short wait.
//!
//! Packets go through the server's `Tx`/`Rx` (framing, cipher, MAC), so both ends are tested against each other.

use crate::session::*;
use crate::sftp::MAX_PACKET;
use crate::wire::{Reader, Writer};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};
use x25519_dalek::{PublicKey, StaticSecret};

const VERSION_LINE: &[u8] = b"SSH-2.0-fos_ssh";
const INBUF: usize = 40 * 1024;
/// The memory a connection needs, given by the caller.
pub const CLIENT_ARENA: usize = PACKET + OUT + INBUF + 2 * MAX_PACKET;
/// Bytes of file data in one SFTP read or write (every server takes this much).
pub const IO: usize = 32 * 1024;
const WINDOW: u32 = 128 * 1024;
/// The largest piece of terminal output the server sends in one packet, and the smallest buffer `read_output` takes.
pub const SHELL_CHUNK: usize = 4096;

/// What the session channel is for.
#[derive(Clone, Copy)]
pub enum Start<'s> {
    /// The `sftp` subsystem.
    Sftp,
    /// A login shell on a terminal of this size.
    Shell { cols: u32, rows: u32 },
    /// One command, no terminal.
    Exec(&'s str),
}
const MSG_USERAUTH_BANNER: u8 = 53;
const MSG_EXT_INFO: u8 = 7;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The byte stream failed or timed out.
    Link,
    /// Nothing arrived within the link's (short) wait; an interactive session asks again. Never ends a connection.
    Timeout,
    /// The server hung up.
    Closed,
    Protocol(&'static str),
    /// `connect`'s function did not trust the server's host key.
    HostKey,
    /// The server refused the key.
    Auth,
    /// An SFTP status other than OK (`NO_SUCH_FILE` and friends, see the `STATUS_*` constants).
    Status(u32),
}

pub const STATUS_EOF: u32 = 1;
pub const STATUS_NO_SUCH_FILE: u32 = 2;
pub const STATUS_PERMISSION_DENIED: u32 = 3;
pub const STATUS_FAILURE: u32 = 4;

impl Error {
    pub fn describe(&self) -> &'static str {
        match self {
            Error::Link => "the connection failed",
            Error::Timeout => "timed out",
            Error::Closed => "the server closed the connection",
            Error::Protocol(why) => why,
            Error::HostKey => "the host key is not the one on record",
            Error::Auth => "the server did not accept the key",
            Error::Status(STATUS_NO_SUCH_FILE) => "no such file",
            Error::Status(STATUS_PERMISSION_DENIED) => "permission denied",
            Error::Status(_) => "the server refused",
        }
    }
}

/// A blocking byte stream to the server.
pub trait Link {
    /// Sends all of `data`.
    fn send(&mut self, data: &[u8]) -> Result<(), Error>;
    /// Waits for at least one byte (the link has its own timeout); 0 means the server closed. A session that polls
    /// (`read_output`) has a short wait and answers `Error::Timeout` when nothing came: bytes already read are kept.
    fn recv(&mut self, out: &mut [u8]) -> Result<usize, Error>;
    fn random(&mut self, out: &mut [u8]);
}

/// An SFTP handle (OpenSSH's are 4 to 8 bytes).
#[derive(Clone, Copy)]
pub struct Handle {
    bytes: [u8; 64],
    length: usize,
}

impl Handle {
    fn text(&self) -> &[u8] {
        &self.bytes[..self.length]
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Attrs {
    pub size: Option<u64>,
    pub permissions: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub mtime: Option<u32>,
}

impl Attrs {
    pub fn is_dir(&self) -> bool {
        self.permissions.is_some_and(|mode| mode & 0o170000 == 0o040000)
    }

    fn parse(reader: &mut Reader) -> Option<Attrs> {
        let flags = reader.u32()?;
        let mut attrs = Attrs::default();
        if flags & 1 != 0 {
            attrs.size = Some(reader.u64()?);
        }
        if flags & 2 != 0 {
            attrs.uid = Some(reader.u32()?);
            attrs.gid = Some(reader.u32()?);
        }
        if flags & 4 != 0 {
            attrs.permissions = Some(reader.u32()?);
        }
        if flags & 8 != 0 {
            reader.u32()?;
            attrs.mtime = Some(reader.u32()?);
        }
        if flags & 0x8000_0000 != 0 {
            for _ in 0..reader.u32()? {
                reader.string()?;
                reader.string()?;
            }
        }
        Some(attrs)
    }
}

pub const OPEN_READ: u32 = 1;
pub const OPEN_WRITE: u32 = 2;
pub const OPEN_CREATE: u32 = 8;
pub const OPEN_TRUNCATE: u32 = 16;
/// With `OPEN_CREATE`: fail when the file exists.
pub const OPEN_EXCLUSIVE: u32 = 32;

const FXP_INIT: u8 = 1;
const FXP_VERSION: u8 = 2;
const FXP_OPEN: u8 = 3;
const FXP_CLOSE: u8 = 4;
const FXP_READ: u8 = 5;
const FXP_WRITE: u8 = 6;
const FXP_SETSTAT: u8 = 9;
const FXP_OPENDIR: u8 = 11;
const FXP_READDIR: u8 = 12;
const FXP_REMOVE: u8 = 13;
const FXP_MKDIR: u8 = 14;
const FXP_RMDIR: u8 = 15;
const FXP_STAT: u8 = 17;
const FXP_RENAME: u8 = 18;
const FXP_STATUS: u8 = 101;
const FXP_HANDLE: u8 = 102;
const FXP_DATA: u8 = 103;
const FXP_NAME: u8 = 104;
const FXP_ATTRS: u8 = 105;
const FXP_EXTENDED: u8 = 200;

pub struct Client<'a, L: Link> {
    link: L,
    tx: Tx<'a>,
    rx: Rx<'a>,
    inbuf: &'a mut [u8],
    have: usize,
    response: &'a mut [u8],
    response_length: usize,
    request: &'a mut [u8],
    peer: u32,
    peer_window: u64,
    peer_max: usize,
    /// Channel data taken since the last WINDOW_ADJUST.
    consumed: u32,
    next_id: u32,
    posix_rename: bool,
    eof: bool,
    exit: Option<u32>,
}

fn fail<T>(why: &'static str) -> Result<T, Error> {
    Err(Error::Protocol(why))
}

impl<'a, L: Link> Client<'a, L> {
    /// Connects, checks the host key with `trust` (its 32 public key bytes), logs `user` in with `seed`'s ed25519 key, opens
    /// the `sftp` subsystem. `arena` is at least `CLIENT_ARENA` bytes.
    pub fn connect(link: L, arena: &'a mut [u8], user: &str, seed: &[u8; 32], trust: &mut dyn FnMut(&[u8; 32]) -> bool) -> Result<Client<'a, L>, Error> {
        let mut client = Self::new(link, arena);
        client.handshake(user, seed, trust, Start::Sftp)?;
        Ok(client)
    }

    /// Like `connect`, but opens a shell on a terminal (`Start::Shell`) or runs one command (`Start::Exec`) instead of
    /// SFTP; the file calls do not work on such a connection.
    pub fn connect_shell(link: L, arena: &'a mut [u8], user: &str, seed: &[u8; 32], trust: &mut dyn FnMut(&[u8; 32]) -> bool, start: Start) -> Result<Client<'a, L>, Error> {
        let mut client = Self::new(link, arena);
        client.handshake(user, seed, trust, start)?;
        Ok(client)
    }

    fn new(link: L, arena: &'a mut [u8]) -> Client<'a, L> {
        assert!(arena.len() >= CLIENT_ARENA);
        let (packet, rest) = arena.split_at_mut(PACKET);
        let (out, rest) = rest.split_at_mut(OUT);
        let (inbuf, rest) = rest.split_at_mut(INBUF);
        let (response, rest) = rest.split_at_mut(MAX_PACKET);
        let request = &mut rest[..MAX_PACKET];
        Client {
            link,
            tx: Tx { buffer: out, length: 0, sent: 0, keys: None, sequence: 0 },
            rx: Rx { buffer: packet, keys: None, sequence: 0 },
            inbuf,
            have: 0,
            response,
            response_length: 0,
            request,
            peer: 0,
            peer_window: 0,
            peer_max: 0,
            consumed: 0,
            next_id: 1,
            posix_rename: false,
            eof: false,
            exit: None,
        }
    }

    fn flush(&mut self) -> Result<(), Error> {
        if self.tx.length > 0 {
            self.link.send(&self.tx.buffer[..self.tx.length])?;
            self.tx.length = 0;
            self.tx.sent = 0;
        }
        Ok(())
    }

    fn send(&mut self, build: impl FnOnce(&mut Writer)) -> Result<(), Error> {
        if !self.tx.packet(build) {
            return fail("a packet did not fit");
        }
        self.flush()
    }

    fn fill(&mut self) -> Result<(), Error> {
        if self.have == self.inbuf.len() {
            return fail("a packet is too big");
        }
        match self.link.recv(&mut self.inbuf[self.have..])? {
            0 => Err(Error::Closed),
            count => {
                self.have += count;
                Ok(())
            }
        }
    }

    /// The next packet's payload length (the payload is `rx.buffer[..n]`); the connection layer's chatter is skipped.
    fn read_packet(&mut self, strict_start: bool) -> Result<usize, Error> {
        loop {
            match self.rx.read(&self.inbuf[..self.have]) {
                Read::More => self.fill()?,
                Read::Bad(_, why) => return fail(why),
                Read::Packet { used, payload } => {
                    self.inbuf.copy_within(used..self.have, 0);
                    self.have -= used;
                    match self.rx.buffer.first() {
                        Some(&MSG_DISCONNECT) => return Err(Error::Closed),
                        Some(&(MSG_IGNORE | MSG_DEBUG | MSG_UNIMPLEMENTED)) if !strict_start => continue,
                        Some(&MSG_USERAUTH_BANNER) => continue,
                        _ => return Ok(payload),
                    }
                }
            }
        }
    }

    /// Reads until a packet of `kind` arrives; any other kind is a protocol error.
    fn expect(&mut self, kind: u8, strict_start: bool) -> Result<usize, Error> {
        let length = self.read_packet(strict_start)?;
        if self.rx.buffer[0] != kind {
            return fail("an unexpected message");
        }
        Ok(length)
    }

    fn handshake(&mut self, user: &str, seed: &[u8; 32], trust: &mut dyn FnMut(&[u8; 32]) -> bool, start: Start) -> Result<(), Error> {
        // Identification lines.
        let mut line = [0u8; 40];
        line[..VERSION_LINE.len()].copy_from_slice(VERSION_LINE);
        line[VERSION_LINE.len()..VERSION_LINE.len() + 2].copy_from_slice(b"\r\n");
        self.link.send(&line[..VERSION_LINE.len() + 2])?;
        let mut server_version = [0u8; 256];
        let server_version_length = loop {
            let Some(end) = self.inbuf[..self.have].iter().position(|&byte| byte == b'\n') else {
                self.fill()?;
                continue;
            };
            let text = self.inbuf[..end].strip_suffix(b"\r").unwrap_or(&self.inbuf[..end]);
            let is_version = text.starts_with(b"SSH-2.0-") || text.starts_with(b"SSH-1.99-");
            let length = text.len();
            if is_version && length <= 255 {
                server_version[..length].copy_from_slice(text);
            } else if is_version || text.starts_with(b"SSH-") {
                return fail("this client speaks SSH 2.0 only");
            }
            self.inbuf.copy_within(end + 1..self.have, 0);
            self.have -= end + 1;
            if is_version {
                break length;
            }
        };

        // KEXINIT both ways.
        let mut kexinit_client = [0u8; 512];
        let mut writer = Writer::new(&mut kexinit_client);
        let mut cookie = [0u8; 16];
        self.link.random(&mut cookie);
        writer.u8(MSG_KEXINIT).raw(&cookie);
        for list in [
            "curve25519-sha256,kex-strict-c-v00@openssh.com",
            "ssh-ed25519",
            "chacha20-poly1305@openssh.com",
            "chacha20-poly1305@openssh.com",
            "hmac-sha2-256",
            "hmac-sha2-256",
            "none",
            "none",
            "",
            "",
        ] {
            writer.string(list.as_bytes());
        }
        writer.bool(false).u32(0);
        let kexinit_client_length = writer.len();
        self.send(|out| {
            out.raw(&kexinit_client[..kexinit_client_length]);
        })?;
        let length = self.expect(MSG_KEXINIT, false)?;
        if length > KEXINIT_MAX {
            return fail("the server's KEXINIT is too long");
        }
        let mut kexinit_server = [0u8; KEXINIT_MAX];
        let kexinit_server_length = length;
        kexinit_server[..length].copy_from_slice(&self.rx.buffer[..length]);
        let mut reader = Reader::new(&kexinit_server[1..length]);
        let bad = Error::Protocol("a bad KEXINIT");
        reader.bytes(16).ok_or(bad)?;
        let mut lists = [&[][..]; 10];
        for list in &mut lists {
            *list = reader.string().ok_or(bad)?;
        }
        let [kex, hostkey, enc_c2s, enc_s2c, _, _, comp_c2s, comp_s2c, _, _] = lists;
        if !(has(kex, "curve25519-sha256") && has(hostkey, "ssh-ed25519") && has(enc_c2s, "chacha20-poly1305@openssh.com") && has(enc_s2c, "chacha20-poly1305@openssh.com") && has(comp_c2s, "none") && has(comp_s2c, "none")) {
            return fail("no common algorithms: need curve25519-sha256, ssh-ed25519, chacha20-poly1305@openssh.com");
        }
        let strict = has(kex, "kex-strict-s-v00@openssh.com");
        if strict && self.rx.sequence != 1 {
            return fail("strict key exchange: KEXINIT must come first");
        }

        // Key exchange.
        let mut ephemeral = [0u8; 32];
        self.link.random(&mut ephemeral);
        let secret = StaticSecret::from(ephemeral);
        let public = PublicKey::from(&secret);
        self.send(|out| {
            out.u8(MSG_KEX_ECDH_INIT).string(public.as_bytes());
        })?;
        let length = self.expect(MSG_KEX_ECDH_REPLY, strict)?;
        let mut reader = Reader::new(&self.rx.buffer[1..length]);
        let key_blob = reader.string().ok_or(bad)?;
        let server_public: [u8; 32] = reader.string().and_then(|key| key.try_into().ok()).ok_or(bad)?;
        let signature_blob = reader.string().ok_or(bad)?;
        let mut blob_reader = Reader::new(key_blob);
        if blob_reader.string() != Some(b"ssh-ed25519") {
            return fail("the host key is not ssh-ed25519");
        }
        let host_key: [u8; 32] = blob_reader.string().and_then(|key| key.try_into().ok()).ok_or(bad)?;
        let shared = secret.diffie_hellman(&PublicKey::from(server_public));
        if !shared.was_contributory() {
            return fail("a bad key exchange");
        }
        let mut secret_bytes = [0u8; 40];
        let mut writer = Writer::new(&mut secret_bytes);
        writer.mpint(shared.as_bytes());
        let secret_length = writer.len();
        let secret_mpint = &secret_bytes[..secret_length];
        let mut hasher = Sha256::new();
        hash_string(&mut hasher, VERSION_LINE);
        hash_string(&mut hasher, &server_version[..server_version_length]);
        hash_string(&mut hasher, &kexinit_client[..kexinit_client_length]);
        hash_string(&mut hasher, &kexinit_server[..kexinit_server_length]);
        hash_string(&mut hasher, key_blob);
        hash_string(&mut hasher, public.as_bytes());
        hash_string(&mut hasher, &server_public);
        hasher.update(secret_mpint);
        let exchange_hash: [u8; 32] = hasher.finalize().into();
        let mut signature_reader = Reader::new(signature_blob);
        let (Some(name), Some(signature)) = (signature_reader.string(), signature_reader.string()) else { return Err(bad) };
        let (Ok(signature), Ok(verifying)) = (<[u8; 64]>::try_from(signature), VerifyingKey::from_bytes(&host_key)) else { return Err(bad) };
        if name != b"ssh-ed25519" || verifying.verify(&exchange_hash, &Signature::from_bytes(&signature)).is_err() {
            return fail("the server's signature does not verify");
        }
        if !trust(&host_key) {
            return Err(Error::HostKey);
        }

        self.send(|out| {
            out.u8(MSG_NEWKEYS);
        })?;
        self.tx.keys = Some(derive(secret_mpint, &exchange_hash, b'C', &exchange_hash));
        if strict {
            self.tx.sequence = 0;
        }
        self.expect(MSG_NEWKEYS, strict)?;
        self.rx.keys = Some(derive(secret_mpint, &exchange_hash, b'D', &exchange_hash));
        if strict {
            self.rx.sequence = 0;
        }

        // Log in.
        self.send(|out| {
            out.u8(MSG_SERVICE_REQUEST).string(b"ssh-userauth");
        })?;
        self.expect(MSG_SERVICE_ACCEPT, false)?;
        let signing = SigningKey::from_bytes(seed);
        let mut blob = [0u8; 64];
        let mut writer = Writer::new(&mut blob);
        writer.string(b"ssh-ed25519").string(signing.verifying_key().as_bytes());
        let blob_length = writer.len();
        let mut signed = [0u8; 512];
        let mut writer = Writer::new(&mut signed);
        writer.string(&exchange_hash).u8(MSG_USERAUTH_REQUEST).string(user.as_bytes()).string(b"ssh-connection").string(b"publickey");
        writer.bool(true).string(b"ssh-ed25519").string(&blob[..blob_length]);
        if !writer.ok() {
            return fail("the user name is too long");
        }
        let signature = signing.sign(writer.written()).to_bytes();
        self.send(|out| {
            let mut inner = [0u8; 96];
            let mut inner_writer = Writer::new(&mut inner);
            inner_writer.string(b"ssh-ed25519").string(&signature);
            out.u8(MSG_USERAUTH_REQUEST).string(user.as_bytes()).string(b"ssh-connection").string(b"publickey");
            out.bool(true).string(b"ssh-ed25519").string(&blob[..blob_length]).string(inner_writer.written());
        })?;
        loop {
            self.read_packet(false)?;
            match self.rx.buffer[0] {
                MSG_USERAUTH_SUCCESS => break,
                MSG_USERAUTH_FAILURE => return Err(Error::Auth),
                // A banner, or the extension list OpenSSH sends before the answer.
                MSG_USERAUTH_BANNER | MSG_EXT_INFO => {}
                _ => return fail("an unexpected message while logging in"),
            }
        }

        // A session channel for sftp, a shell or one command.
        let max_packet = if matches!(start, Start::Sftp) { MAX_DATA } else { SHELL_CHUNK };
        self.send(|out| {
            out.u8(MSG_CHANNEL_OPEN).string(b"session").u32(0).u32(WINDOW).u32(max_packet as u32);
        })?;
        // OpenSSH announces its host keys with a global request first.
        let length = loop {
            let length = self.read_packet(false)?;
            match self.rx.buffer[0] {
                MSG_CHANNEL_OPEN_FAILURE | MSG_CHANNEL_OPEN_CONFIRMATION => break length,
                MSG_GLOBAL_REQUEST | MSG_EXT_INFO | MSG_DEBUG | MSG_IGNORE => self.connection_message(true)?,
                _ => return fail("an unexpected message opening the channel"),
            }
        };
        if self.rx.buffer[0] == MSG_CHANNEL_OPEN_FAILURE {
            return fail("the server refused a session channel");
        }
        let mut reader = Reader::new(&self.rx.buffer[1..length]);
        reader.u32().ok_or(bad)?;
        self.peer = reader.u32().ok_or(bad)?;
        self.peer_window = u64::from(reader.u32().ok_or(bad)?);
        self.peer_max = (reader.u32().ok_or(bad)? as usize).min(MAX_DATA);
        if self.peer_max == 0 {
            return fail("the server allows no channel data");
        }
        let peer = self.peer;
        match start {
            Start::Sftp => {
                if !self.channel_request(|out| {
                    out.u8(MSG_CHANNEL_REQUEST).u32(peer).string(b"subsystem").bool(true).string(b"sftp");
                })? {
                    return fail("the server has no sftp subsystem");
                }
                self.sftp_start()
            }
            Start::Shell { cols, rows } => {
                // A refused terminal is not fatal: the shell then runs without one (our own sshd has none).
                let _ = self.channel_request(|out| {
                    out.u8(MSG_CHANNEL_REQUEST).u32(peer).string(b"pty-req").bool(true).string(b"vt100").u32(cols).u32(rows).u32(0).u32(0).string(&[0]);
                })?;
                if !self.channel_request(|out| {
                    out.u8(MSG_CHANNEL_REQUEST).u32(peer).string(b"shell").bool(true);
                })? {
                    return fail("the server refused to start a shell");
                }
                Ok(())
            }
            Start::Exec(command) => {
                if !self.channel_request(|out| {
                    out.u8(MSG_CHANNEL_REQUEST).u32(peer).string(b"exec").bool(true).string(command.as_bytes());
                })? {
                    return fail("the server refused to run the command");
                }
                Ok(())
            }
        }
    }

    /// Sends a channel request that wants an answer (`build` writes it) and waits for it: true for success.
    fn channel_request(&mut self, build: impl FnOnce(&mut Writer)) -> Result<bool, Error> {
        self.send(build)?;
        loop {
            self.read_packet(false)?;
            match self.rx.buffer[0] {
                MSG_CHANNEL_SUCCESS => return Ok(true),
                MSG_CHANNEL_FAILURE => return Ok(false),
                MSG_CHANNEL_WINDOW_ADJUST | MSG_GLOBAL_REQUEST => self.connection_message(true)?,
                _ => return fail("an unexpected message answering a channel request"),
            }
        }
    }

    /// Sends as much of `data` as the server's window and packet size allow, as terminal input; how many bytes went. 0
    /// means the window is full: `read_output` takes the window changes in, then ask again.
    pub fn write_input(&mut self, data: &[u8]) -> Result<usize, Error> {
        let count = data.len().min(self.peer_max).min(self.peer_window.min(u64::from(u32::MAX)) as usize);
        if count == 0 {
            return Ok(0);
        }
        let peer = self.peer;
        self.send(|out| {
            out.u8(MSG_CHANNEL_DATA).u32(peer).string(&data[..count]);
        })?;
        self.peer_window -= count as u64;
        Ok(count)
    }

    /// Waits one link timeout for what the shell or command printed (stdout and stderr alike, the terminal merges them):
    /// `Some(count)` bytes in `out` (at least `SHELL_CHUNK` long), `None` when nothing came. The end of the session is
    /// `Err(Error::Closed)`; `exit_status` then says how the command ended.
    pub fn read_output(&mut self, out: &mut [u8]) -> Result<Option<usize>, Error> {
        assert!(out.len() >= SHELL_CHUNK);
        loop {
            let length = match self.read_packet(false) {
                Ok(length) => length,
                Err(Error::Timeout) if self.eof => return Err(Error::Closed),
                Err(Error::Timeout) => return Ok(None),
                Err(error) => return Err(error),
            };
            let bad = Error::Protocol("a bad channel message");
            let mut reader = Reader::new(&self.rx.buffer[1..length]);
            match self.rx.buffer[0] {
                MSG_CHANNEL_WINDOW_ADJUST => {
                    reader.u32();
                    self.peer_window += u64::from(reader.u32().ok_or(bad)?);
                }
                kind @ (MSG_CHANNEL_DATA | MSG_CHANNEL_EXTENDED_DATA) => {
                    reader.u32().ok_or(bad)?;
                    if kind == MSG_CHANNEL_EXTENDED_DATA {
                        reader.u32().ok_or(bad)?;
                    }
                    let data = reader.string().ok_or(bad)?;
                    if data.len() > out.len() {
                        return fail("the server sent more than one packet allows");
                    }
                    out[..data.len()].copy_from_slice(data);
                    let count = data.len();
                    self.consumed += count as u32;
                    if self.consumed >= WINDOW / 4 {
                        let (peer, consumed) = (self.peer, self.consumed);
                        self.consumed = 0;
                        self.send(|out| {
                            out.u8(MSG_CHANNEL_WINDOW_ADJUST).u32(peer).u32(consumed);
                        })?;
                    }
                    if count > 0 {
                        return Ok(Some(count));
                    }
                }
                MSG_CHANNEL_REQUEST => {
                    reader.u32().ok_or(bad)?;
                    let kind = reader.string().ok_or(bad)?;
                    let wants_reply = reader.bool().ok_or(bad)?;
                    if kind == b"exit-status" {
                        self.exit = reader.u32();
                    } else if wants_reply {
                        let peer = self.peer;
                        self.send(|out| {
                            out.u8(MSG_CHANNEL_FAILURE).u32(peer);
                        })?;
                    }
                }
                MSG_CHANNEL_EOF => self.eof = true,
                MSG_CHANNEL_CLOSE => {
                    let peer = self.peer;
                    let _ = self.send(|out| {
                        out.u8(MSG_CHANNEL_CLOSE).u32(peer);
                    });
                    return Err(Error::Closed);
                }
                MSG_GLOBAL_REQUEST => self.connection_message(true)?,
                _ => {}
            }
        }
    }

    /// The command's exit status, once the server has sent it.
    pub fn exit_status(&self) -> Option<u32> {
        self.exit
    }

    /// Says goodbye: end of input, close the channel, disconnect. Best effort.
    pub fn hang_up(&mut self) {
        let peer = self.peer;
        let _ = self.send(|out| {
            out.u8(MSG_CHANNEL_EOF).u32(peer);
        });
        let _ = self.send(|out| {
            out.u8(MSG_CHANNEL_CLOSE).u32(peer);
        });
        let _ = self.send(|out| {
            out.u8(MSG_DISCONNECT).u32(11).string(b"bye").string(b"");
        });
    }

    /// A connection-layer message that is not the answer being waited for: window changes, data, end of channel.
    fn connection_message(&mut self, ignore_data: bool) -> Result<(), Error> {
        let length = {
            let payload = &self.rx.buffer;
            match payload[0] {
                MSG_CHANNEL_WINDOW_ADJUST => {
                    let mut reader = Reader::new(&payload[1..]);
                    reader.u32();
                    self.peer_window += u64::from(reader.u32().unwrap_or(0));
                    return Ok(());
                }
                MSG_CHANNEL_DATA if !ignore_data => 0,
                MSG_CHANNEL_DATA => return Ok(()),
                MSG_CHANNEL_EOF | MSG_CHANNEL_CLOSE => return Err(Error::Closed),
                MSG_GLOBAL_REQUEST => 1,
                _ => return Ok(()),
            }
        };
        if length == 1 {
            // A global request that wants an answer (keepalive@openssh.com does): refuse it, which is what a server may.
            let mut reader = Reader::new(&self.rx.buffer[1..]);
            reader.string();
            if reader.bool() == Some(true) {
                self.send(|out| {
                    out.u8(MSG_REQUEST_FAILURE);
                })?;
            }
            return Ok(());
        }
        let mut reader = Reader::new(&self.rx.buffer[1..]);
        reader.u32();
        let data = reader.string().ok_or(Error::Protocol("bad channel data"))?;
        if self.response_length + data.len() > self.response.len() {
            return fail("the server sent more than was asked");
        }
        self.response[self.response_length..self.response_length + data.len()].copy_from_slice(data);
        self.response_length += data.len();
        self.consumed += data.len() as u32;
        Ok(())
    }

    /// The byte stream, for closing it when the session is over.
    pub fn link(&mut self) -> &mut L {
        &mut self.link
    }

    /// Sends the SFTP request in `self.request[..length]` as channel data, as the server's window allows.
    fn send_request(&mut self, length: usize) -> Result<(), Error> {
        let mut at = 0;
        while at < length {
            while self.peer_window == 0 {
                self.read_packet(false)?;
                self.connection_message(true)?;
            }
            let chunk = (length - at).min(self.peer_max).min(self.peer_window as usize);
            let peer = self.peer;
            let request = &self.request[at..at + chunk];
            if !self.tx.packet(|out| {
                out.u8(MSG_CHANNEL_DATA).u32(peer).string(request);
            }) {
                return fail("a packet did not fit");
            }
            self.peer_window -= chunk as u64;
            at += chunk;
            self.flush()?;
        }
        Ok(())
    }

    /// Sends one SFTP request (`kind`, a fresh id, what `build` writes) and waits for the answer: `(kind, length)`, the
    /// answer in `self.response[9..4 + length]` after its type and id.
    fn call(&mut self, kind: u8, build: impl FnOnce(&mut Writer)) -> Result<(u8, usize), Error> {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        let mut writer = Writer::new(&mut *self.request);
        writer.u32(0).u8(kind).u32(id);
        build(&mut writer);
        if !writer.ok() {
            return fail("a request is too long");
        }
        let length = writer.len();
        self.request[..4].copy_from_slice(&((length - 4) as u32).to_be_bytes());
        self.send_request(length)?;
        self.await_response(id)
    }

    fn await_response(&mut self, id: u32) -> Result<(u8, usize), Error> {
        loop {
            if self.response_length >= 4 {
                let length = u32::from_be_bytes([self.response[0], self.response[1], self.response[2], self.response[3]]) as usize;
                if length < 5 || length + 4 > self.response.len() {
                    return fail("a bad SFTP packet");
                }
                if self.response_length >= length + 4 {
                    self.response_length = 0;
                    if self.consumed > 0 {
                        let (peer, consumed) = (self.peer, self.consumed);
                        self.consumed = 0;
                        self.send(|out| {
                            out.u8(MSG_CHANNEL_WINDOW_ADJUST).u32(peer).u32(consumed);
                        })?;
                    }
                    let answer = u32::from_be_bytes([self.response[5], self.response[6], self.response[7], self.response[8]]);
                    if self.response[4] != FXP_VERSION && answer != id {
                        return fail("an answer to another request");
                    }
                    return Ok((self.response[4], length));
                }
            }
            self.read_packet(false)?;
            self.connection_message(false)?;
        }
    }

    fn sftp_start(&mut self) -> Result<(), Error> {
        let mut writer = Writer::new(&mut *self.request);
        writer.u32(5).u8(FXP_INIT).u32(3);
        self.send_request(9)?;
        // VERSION has no request id: its four bytes after the type are the version, which `await_response` does not check.
        let (kind, length) = self.await_response(0)?;
        if kind != FXP_VERSION {
            return fail("the server did not answer sftp");
        }
        let mut reader = Reader::new(&self.response[9..4 + length]);
        let mut posix_rename = false;
        while let (Some(name), Some(data)) = (reader.string(), reader.string()) {
            posix_rename |= name == b"posix-rename@openssh.com" && data == b"1";
        }
        self.posix_rename = posix_rename;
        Ok(())
    }

    fn status(&mut self, kind: u8, build: impl FnOnce(&mut Writer)) -> Result<(), Error> {
        let (answer, length) = self.call(kind, build)?;
        if answer != FXP_STATUS {
            return fail("an unexpected SFTP answer");
        }
        match Reader::new(&self.response[9..4 + length]).u32() {
            Some(0) => Ok(()),
            Some(code) => Err(Error::Status(code)),
            None => fail("a bad SFTP status"),
        }
    }

    /// An answer of kind `want` (`self.response[9..4 + length]` is its body after type and id), or the server's status as an error.
    fn answer(&mut self, kind: u8, want: u8, build: impl FnOnce(&mut Writer)) -> Result<usize, Error> {
        let (answer, length) = self.call(kind, build)?;
        if answer == want {
            return Ok(length);
        }
        if answer == FXP_STATUS {
            return match Reader::new(&self.response[9..4 + length]).u32() {
                Some(code) => Err(Error::Status(code)),
                None => fail("a bad SFTP status"),
            };
        }
        fail("an unexpected SFTP answer")
    }

    fn handle_of(&mut self, kind: u8, build: impl FnOnce(&mut Writer)) -> Result<Handle, Error> {
        let length = self.answer(kind, FXP_HANDLE, build)?;
        let text = Reader::new(&self.response[9..4 + length]).string().filter(|text| text.len() <= 64).ok_or(Error::Protocol("a bad handle"))?;
        let mut handle = Handle { bytes: [0; 64], length: text.len() };
        handle.bytes[..text.len()].copy_from_slice(text);
        Ok(handle)
    }

    pub fn stat(&mut self, path: &str) -> Result<Attrs, Error> {
        let length = self.answer(FXP_STAT, FXP_ATTRS, |out| {
            out.string(path.as_bytes());
        })?;
        Attrs::parse(&mut Reader::new(&self.response[9..4 + length])).ok_or(Error::Protocol("bad attributes"))
    }

    /// `flags` are `OPEN_*`; `mode` is the permission bits a new file gets.
    pub fn open(&mut self, path: &str, flags: u32, mode: u32) -> Result<Handle, Error> {
        self.handle_of(FXP_OPEN, |out| {
            out.string(path.as_bytes()).u32(flags).u32(4).u32(mode);
        })
    }

    pub fn close(&mut self, handle: &Handle) -> Result<(), Error> {
        self.status(FXP_CLOSE, |out| {
            out.string(handle.text());
        })
    }

    /// Up to `IO` bytes from `offset`; 0 at the end of the file.
    pub fn read(&mut self, handle: &Handle, offset: u64, out: &mut [u8]) -> Result<usize, Error> {
        let wanted = out.len().min(IO);
        match self.answer(FXP_READ, FXP_DATA, |request| {
            request.string(handle.text()).u64(offset).u32(wanted as u32);
        }) {
            Ok(length) => {
                let data = Reader::new(&self.response[9..4 + length]).string().filter(|data| data.len() <= wanted).ok_or(Error::Protocol("bad data"))?;
                out[..data.len()].copy_from_slice(data);
                Ok(data.len())
            }
            Err(Error::Status(STATUS_EOF)) => Ok(0),
            Err(error) => Err(error),
        }
    }

    /// All of `data`, in pieces of `IO`.
    pub fn write(&mut self, handle: &Handle, offset: u64, data: &[u8]) -> Result<(), Error> {
        let mut at = 0;
        while at < data.len() {
            let piece = &data[at..(at + IO).min(data.len())];
            self.status(FXP_WRITE, |out| {
                out.string(handle.text()).u64(offset + at as u64).string(piece);
            })?;
            at += piece.len();
        }
        Ok(())
    }

    pub fn open_dir(&mut self, path: &str) -> Result<Handle, Error> {
        self.handle_of(FXP_OPENDIR, |out| {
            out.string(path.as_bytes());
        })
    }

    /// The next batch of entries of an open folder (`.` and `..` included) to `entry`; false at the end.
    pub fn read_dir(&mut self, handle: &Handle, entry: &mut dyn FnMut(&[u8], &Attrs)) -> Result<bool, Error> {
        let length = match self.answer(FXP_READDIR, FXP_NAME, |out| {
            out.string(handle.text());
        }) {
            Ok(length) => length,
            Err(Error::Status(STATUS_EOF)) => return Ok(false),
            Err(error) => return Err(error),
        };
        let mut reader = Reader::new(&self.response[9..4 + length]);
        let bad = Error::Protocol("a bad folder entry");
        for _ in 0..reader.u32().ok_or(bad)? {
            let name = reader.string().ok_or(bad)?;
            reader.string().ok_or(bad)?;
            let attrs = Attrs::parse(&mut reader).ok_or(bad)?;
            entry(name, &attrs);
        }
        Ok(true)
    }

    pub fn make_dir(&mut self, path: &str, mode: u32) -> Result<(), Error> {
        self.status(FXP_MKDIR, |out| {
            out.string(path.as_bytes()).u32(4).u32(mode);
        })
    }

    pub fn remove_dir(&mut self, path: &str) -> Result<(), Error> {
        self.status(FXP_RMDIR, |out| {
            out.string(path.as_bytes());
        })
    }

    pub fn remove(&mut self, path: &str) -> Result<(), Error> {
        self.status(FXP_REMOVE, |out| {
            out.string(path.as_bytes());
        })
    }

    /// Renames, replacing an existing target where the server has `posix-rename@openssh.com`.
    pub fn rename(&mut self, from: &str, to: &str) -> Result<(), Error> {
        if self.posix_rename {
            return self.status(FXP_EXTENDED, |out| {
                out.string(b"posix-rename@openssh.com").string(from.as_bytes()).string(to.as_bytes());
            });
        }
        self.status(FXP_RENAME, |out| {
            out.string(from.as_bytes()).string(to.as_bytes());
        })
    }

    pub fn set_size(&mut self, path: &str, size: u64) -> Result<(), Error> {
        self.status(FXP_SETSTAT, |out| {
            out.string(path.as_bytes()).u32(1).u64(size);
        })
    }

    pub fn set_mode(&mut self, path: &str, mode: u32) -> Result<(), Error> {
        self.status(FXP_SETSTAT, |out| {
            out.string(path.as_bytes()).u32(4).u32(mode);
        })
    }

    pub fn set_owner(&mut self, path: &str, uid: u32, gid: u32) -> Result<(), Error> {
        self.status(FXP_SETSTAT, |out| {
            out.string(path.as_bytes()).u32(2).u32(uid).u32(gid);
        })
    }
}
