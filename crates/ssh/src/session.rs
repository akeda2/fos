//! An SSH-2 server connection as a state machine over bytes: the caller feeds what arrived (`feed`), sends what is queued
//! (`output`/`sent`) and calls `poll` to let queued work (SFTP requests, replies waiting for window) move.
//!
//! What it speaks: `curve25519-sha256`, host key `ssh-ed25519`, cipher `chacha20-poly1305@openssh.com` (its own MAC), no
//! compression; public-key login with ssh-ed25519 keys only; one `session` channel with the `sftp` subsystem. Strict key
//! exchange (`kex-strict-*-v00@openssh.com`, the Terrapin fix) is on when the client offers it. The client may re-key at any time;
//! this side never starts one (a re-key is due after gigabytes, and every OpenSSH client starts it itself).
//!
//! Buffers: one arena the caller provides (`ARENA` bytes) holds the packet being decrypted, the packets waiting to be sent, and
//! the SFTP request and reply. The channel window granted to the client is the free room of the request buffer, so a client
//! can never send more than fits.
//!
//! Padding is zeros: before the keys it hides nothing, after them it is encrypted, and it saves a random call per packet.

use crate::sftp::{self, Exports, Sftp, Step};
use crate::wire::{Reader, Writer};
use chacha20::ChaCha20;
use chacha20::cipher::{KeyIvInit, StreamCipher, StreamCipherSeek};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use poly1305::Poly1305;
use poly1305::universal_hash::KeyInit;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use vfs_export::Files;
use x25519_dalek::{PublicKey, StaticSecret};

const VERSION: &[u8] = b"SSH-2.0-fos_sshd";
/// The most shell output taken per pass.
const SHELL_CHUNK: usize = 4096;
pub(crate) const PACKET: usize = 36 * 1024;
pub(crate) const OUT: usize = 40 * 1024;
const SFTP_IN: usize = 256 * 1024;
/// The memory one session needs, given by the caller.
pub const ARENA: usize = PACKET + OUT + SFTP_IN + sftp::MAX_PACKET;
/// The largest data field of a CHANNEL_DATA packet either way.
pub(crate) const MAX_DATA: usize = 32 * 1024;
pub(crate) const KEXINIT_MAX: usize = 4096;
pub(crate) const TAG: usize = 16;
const AUTH_REQUESTS: u32 = 20;
const AUTH_FAILURES: u32 = 3;

pub(crate) const MSG_DISCONNECT: u8 = 1;
pub(crate) const MSG_IGNORE: u8 = 2;
pub(crate) const MSG_UNIMPLEMENTED: u8 = 3;
pub(crate) const MSG_DEBUG: u8 = 4;
pub(crate) const MSG_SERVICE_REQUEST: u8 = 5;
pub(crate) const MSG_SERVICE_ACCEPT: u8 = 6;
pub(crate) const MSG_KEXINIT: u8 = 20;
pub(crate) const MSG_NEWKEYS: u8 = 21;
pub(crate) const MSG_KEX_ECDH_INIT: u8 = 30;
pub(crate) const MSG_KEX_ECDH_REPLY: u8 = 31;
pub(crate) const MSG_USERAUTH_REQUEST: u8 = 50;
pub(crate) const MSG_USERAUTH_FAILURE: u8 = 51;
pub(crate) const MSG_USERAUTH_SUCCESS: u8 = 52;
pub(crate) const MSG_USERAUTH_PK_OK: u8 = 60;
pub(crate) const MSG_GLOBAL_REQUEST: u8 = 80;
pub(crate) const MSG_REQUEST_FAILURE: u8 = 82;
pub(crate) const MSG_CHANNEL_OPEN: u8 = 90;
pub(crate) const MSG_CHANNEL_OPEN_CONFIRMATION: u8 = 91;
pub(crate) const MSG_CHANNEL_OPEN_FAILURE: u8 = 92;
pub(crate) const MSG_CHANNEL_WINDOW_ADJUST: u8 = 93;
pub(crate) const MSG_CHANNEL_DATA: u8 = 94;
pub(crate) const MSG_CHANNEL_EXTENDED_DATA: u8 = 95;
pub(crate) const MSG_CHANNEL_EOF: u8 = 96;
pub(crate) const MSG_CHANNEL_CLOSE: u8 = 97;
pub(crate) const MSG_CHANNEL_REQUEST: u8 = 98;
pub(crate) const MSG_CHANNEL_SUCCESS: u8 = 99;
pub(crate) const MSG_CHANNEL_FAILURE: u8 = 100;

pub(crate) const REASON_PROTOCOL_ERROR: u32 = 2;
pub(crate) const REASON_KEY_EXCHANGE_FAILED: u32 = 3;
pub(crate) const REASON_MAC_ERROR: u32 = 5;
pub(crate) const REASON_SERVICE_NOT_AVAILABLE: u32 = 7;
pub(crate) const REASON_NO_MORE_AUTH_METHODS: u32 = 14;

/// Who a key logs in as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Account {
    pub uid: u32,
    pub gid: u32,
    /// Refuses every change, whatever the files allow.
    pub read_only: bool,
    /// May open a shell channel (the `shell` option): that is a login on the machine, not just files.
    pub shell: bool,
}

/// What a shell session answers a read with.
pub enum ShellRead {
    /// This many bytes of output were copied.
    Data(usize),
    /// The shell ended the session (its output is drained).
    Closed,
}

/// What a connection needs from its surroundings.
pub trait Host {
    fn random(&mut self, out: &mut [u8]);
    /// The seed of the server's ssh-ed25519 host key.
    fn host_seed(&self) -> [u8; 32];
    /// The account `user` may log in as with this public key, if any (the `authorized_keys` lookup).
    fn authorize(&mut self, user: &str, key: &[u8; 32]) -> Option<Account>;
    /// The exports and the files behind them.
    fn split(&mut self) -> (&Exports, &mut dyn Files);
    fn log(&mut self, _text: core::fmt::Arguments) {}
    /// Opens a shell for a `shell` channel request (a terminal without a pty: typed bytes in, console output back). None
    /// refuses it. The returned number names the shell in the calls below.
    fn shell_open(&mut self) -> Option<u32> {
        None
    }
    /// Typed bytes for the shell; how many it took (it may take fewer: the rest is offered again).
    fn shell_write(&mut self, _shell: u32, _data: &[u8]) -> usize {
        0
    }
    fn shell_read(&mut self, _shell: u32, _out: &mut [u8]) -> ShellRead {
        ShellRead::Closed
    }
    fn shell_close(&mut self, _shell: u32) {}
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Waiting for the client's identification line.
    Version,
    /// Waiting for the client's KEXINIT.
    KexInit,
    /// Waiting for KEX_ECDH_INIT.
    KexDh,
    /// Waiting for the client's NEWKEYS.
    NewKeys,
    Running,
    /// Disconnected: the queued output goes out, nothing more is read.
    Closed,
}

#[derive(Clone, Copy)]
pub(crate) struct Keys {
    /// K_2 of the OpenSSH chacha20-poly1305: the packet body and the Poly1305 key.
    pub(crate) main: [u8; 32],
    /// K_1: the packet length.
    pub(crate) header: [u8; 32],
}

fn nonce(sequence: u32) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    nonce[8..].copy_from_slice(&sequence.to_be_bytes());
    nonce
}

/// The body cipher positioned after the block that gives the Poly1305 key, and that key.
fn body_cipher(keys: &Keys, sequence: u32) -> (ChaCha20, [u8; 32]) {
    let nonce = nonce(sequence);
    let mut cipher = ChaCha20::new((&keys.main).into(), (&nonce).into());
    let mut key = [0u8; 32];
    cipher.apply_keystream(&mut key);
    cipher.seek(64u64);
    (cipher, key)
}

fn tag_of(key: &[u8; 32], data: &[u8]) -> [u8; TAG] {
    Poly1305::new(key.into()).compute_unpadded(data).into()
}

pub(crate) struct Tx<'a> {
    pub(crate) buffer: &'a mut [u8],
    pub(crate) length: usize,
    pub(crate) sent: usize,
    pub(crate) keys: Option<Keys>,
    pub(crate) sequence: u32,
}

impl Tx<'_> {
    pub(crate) fn pending(&self) -> usize {
        self.length - self.sent
    }

    /// Queues one packet whose payload `build` writes; false when it does not fit (nothing is queued).
    pub(crate) fn packet(&mut self, build: impl FnOnce(&mut Writer)) -> bool {
        let at = self.length;
        let overhead = 5 + 8 + TAG;
        if self.buffer.len() < at + overhead {
            return false;
        }
        let room = self.buffer.len() - at - overhead;
        let mut writer = Writer::new(&mut self.buffer[at + 5..at + 5 + room]);
        build(&mut writer);
        if !writer.ok() {
            return false;
        }
        let payload = writer.len();
        let mut pad = 8 - (payload + if self.keys.is_some() { 1 } else { 5 }) % 8;
        if pad < 4 {
            pad += 8;
        }
        let length = 1 + payload + pad;
        let block = &mut self.buffer[at..];
        block[..4].copy_from_slice(&(length as u32).to_be_bytes());
        block[4] = pad as u8;
        block[5 + payload..5 + payload + pad].fill(0);
        if let Some(keys) = &self.keys {
            let (mut cipher, key) = body_cipher(keys, self.sequence);
            cipher.apply_keystream(&mut block[4..4 + length]);
            let nonce = nonce(self.sequence);
            ChaCha20::new((&keys.header).into(), (&nonce).into()).apply_keystream(&mut block[..4]);
            let tag = tag_of(&key, &block[..4 + length]);
            block[4 + length..4 + length + TAG].copy_from_slice(&tag);
            self.length += 4 + length + TAG;
        } else {
            self.length += 4 + length;
        }
        self.sequence = self.sequence.wrapping_add(1);
        true
    }

    pub(crate) fn raw(&mut self, data: &[u8]) {
        if self.buffer.len() >= self.length + data.len() {
            self.buffer[self.length..self.length + data.len()].copy_from_slice(data);
            self.length += data.len();
        }
    }
}

pub(crate) struct Rx<'a> {
    pub(crate) buffer: &'a mut [u8],
    pub(crate) keys: Option<Keys>,
    pub(crate) sequence: u32,
}

pub(crate) enum Read {
    /// Not all of the packet is here.
    More,
    /// A packet of `used` bytes arrived; its payload is the first `payload` bytes of the buffer.
    Packet { used: usize, payload: usize },
    Bad(u32, &'static str),
}

impl Rx<'_> {
    pub(crate) fn read(&mut self, input: &[u8]) -> Read {
        let Some(head) = input.get(..4) else { return Read::More };
        let mut length_bytes = [head[0], head[1], head[2], head[3]];
        if let Some(keys) = &self.keys {
            let nonce = nonce(self.sequence);
            ChaCha20::new((&keys.header).into(), (&nonce).into()).apply_keystream(&mut length_bytes);
        }
        let length = u32::from_be_bytes(length_bytes) as usize;
        let encrypted = self.keys.is_some();
        if length < 5 || length > self.buffer.len() || (encrypted && length % 8 != 0) {
            return Read::Bad(REASON_PROTOCOL_ERROR, "bad packet length");
        }
        let used = 4 + length + if encrypted { TAG } else { 0 };
        let Some(packet) = input.get(..used) else { return Read::More };
        let body = &mut self.buffer[..length];
        if let Some(keys) = &self.keys {
            let (mut cipher, key) = body_cipher(keys, self.sequence);
            let expected = tag_of(&key, &packet[..4 + length]);
            if !bool::from(expected.ct_eq(&packet[4 + length..])) {
                return Read::Bad(REASON_MAC_ERROR, "bad packet MAC");
            }
            body.copy_from_slice(&packet[4..4 + length]);
            cipher.apply_keystream(body);
        } else {
            body.copy_from_slice(&packet[4..4 + length]);
        }
        self.sequence = self.sequence.wrapping_add(1);
        let pad = body[0] as usize;
        if pad < 4 || pad + 1 >= length {
            return Read::Bad(REASON_PROTOCOL_ERROR, "bad padding");
        }
        let payload = length - 1 - pad;
        body.copy_within(1..1 + payload, 0);
        Read::Packet { used, payload }
    }
}

struct Channel {
    peer: u32,
    peer_window: u64,
    peer_max: u32,
    /// What the client may still send us.
    granted: usize,
    /// Consumed request bytes not yet given back as window.
    to_return: usize,
    sftp: bool,
    /// The shell serving this channel (`Host::shell_open`).
    shell: Option<u32>,
    peer_eof: bool,
    closing: bool,
}

pub struct Session<'a> {
    tx: Tx<'a>,
    rx: Rx<'a>,
    sftp_in: &'a mut [u8],
    sftp_out: &'a mut [u8],
    sftp_in_length: usize,
    sftp_out_length: usize,
    sftp_out_sent: usize,
    phase: Phase,
    established: bool,
    strict: bool,
    kex_sent: bool,
    skip_next: bool,
    version_client: [u8; 256],
    version_client_length: usize,
    kexinit_client: [u8; KEXINIT_MAX],
    kexinit_client_length: usize,
    kexinit_server: [u8; 512],
    kexinit_server_length: usize,
    next_receive: Option<Keys>,
    session_id: Option<[u8; 32]>,
    service_ok: bool,
    user: [u8; 64],
    user_length: usize,
    account: Option<Account>,
    auth_requests: u32,
    auth_failures: u32,
    channel: Option<Channel>,
    sftp: Sftp,
}

pub(crate) fn has(list: &[u8], name: &str) -> bool {
    list.split(|&byte| byte == b',').any(|item| item == name.as_bytes())
}

pub(crate) fn first(list: &[u8]) -> &[u8] {
    list.split(|&byte| byte == b',').next().unwrap_or(&[])
}

pub(crate) fn hash_string(hasher: &mut Sha256, data: &[u8]) {
    hasher.update((data.len() as u32).to_be_bytes());
    hasher.update(data);
}

/// The 64 key bytes of RFC 4253 section 7.2 for `letter` (C: client to server, D: server to client).
pub(crate) fn derive(secret: &[u8], exchange_hash: &[u8; 32], letter: u8, session_id: &[u8; 32]) -> Keys {
    let mut hasher = Sha256::new();
    hasher.update(secret);
    hasher.update(exchange_hash);
    hasher.update([letter]);
    hasher.update(session_id);
    let first: [u8; 32] = hasher.finalize().into();
    let mut hasher = Sha256::new();
    hasher.update(secret);
    hasher.update(exchange_hash);
    hasher.update(first);
    let second: [u8; 32] = hasher.finalize().into();
    Keys { main: first, header: second }
}

impl<'a> Session<'a> {
    /// A connection over `arena` (at least `ARENA` bytes): queues the identification line and the KEXINIT.
    pub fn new<H: Host>(host: &mut H, arena: &'a mut [u8]) -> Session<'a> {
        assert!(arena.len() >= ARENA);
        let (packet, rest) = arena.split_at_mut(PACKET);
        let (out, rest) = rest.split_at_mut(OUT);
        let (sftp_in, rest) = rest.split_at_mut(SFTP_IN);
        let sftp_out = &mut rest[..sftp::MAX_PACKET];
        let mut session = Session {
            tx: Tx { buffer: out, length: 0, sent: 0, keys: None, sequence: 0 },
            rx: Rx { buffer: packet, keys: None, sequence: 0 },
            sftp_in,
            sftp_out,
            sftp_in_length: 0,
            sftp_out_length: 0,
            sftp_out_sent: 0,
            phase: Phase::Version,
            established: false,
            strict: false,
            kex_sent: false,
            skip_next: false,
            version_client: [0; 256],
            version_client_length: 0,
            kexinit_client: [0; KEXINIT_MAX],
            kexinit_client_length: 0,
            kexinit_server: [0; 512],
            kexinit_server_length: 0,
            next_receive: None,
            session_id: None,
            service_ok: false,
            user: [0; 64],
            user_length: 0,
            account: None,
            auth_requests: 0,
            auth_failures: 0,
            channel: None,
            sftp: Sftp::new(),
        };
        session.tx.raw(VERSION);
        session.tx.raw(b"\r\n");
        session.send_kexinit(host);
        session
    }

    /// The bytes waiting to go to the client.
    pub fn output(&self) -> &[u8] {
        &self.tx.buffer[self.tx.sent..self.tx.length]
    }

    /// `n` bytes of `output` went out.
    pub fn sent(&mut self, n: usize) {
        self.tx.sent = (self.tx.sent + n).min(self.tx.length);
        if self.tx.sent == self.tx.length {
            self.tx.sent = 0;
            self.tx.length = 0;
        }
    }

    /// Disconnected and everything queued has gone out: close the TCP connection.
    pub fn finished(&self) -> bool {
        self.phase == Phase::Closed && self.tx.pending() == 0
    }

    pub fn is_authenticated(&self) -> bool {
        self.account.is_some()
    }

    /// Takes at most one packet from `input`; the number of bytes used (0: not all of a packet is there yet, or the output
    /// has to be sent first).
    pub fn feed<H: Host>(&mut self, host: &mut H, input: &[u8]) -> usize {
        if self.phase == Phase::Closed || self.tx.pending() > 0 {
            return 0;
        }
        if self.phase == Phase::Version {
            return self.read_version(host, input);
        }
        match self.rx.read(input) {
            Read::More => 0,
            Read::Bad(code, text) => {
                self.disconnect(code, text);
                input.len()
            }
            Read::Packet { used, payload } => {
                let packet = core::mem::take(&mut self.rx.buffer);
                if let Err((code, text)) = self.dispatch(host, &packet[..payload]) {
                    self.disconnect(code, text);
                }
                self.rx.buffer = packet;
                used
            }
        }
    }

    /// Moves queued work along: SFTP requests, reply data into the channel window, window adjustments, closing.
    pub fn poll<H: Host>(&mut self, host: &mut H) {
        while self.phase == Phase::Running && self.tx.pending() == 0 && self.channel.is_some() {
            if self.sftp_out_length > self.sftp_out_sent {
                self.pump();
                break;
            }
            self.sftp_out_length = 0;
            self.sftp_out_sent = 0;
            if let Some(shell) = self.channel.as_ref().and_then(|channel| channel.shell) {
                self.poll_shell(host, shell);
                break;
            }
            let Some(channel) = self.channel.as_mut().filter(|channel| channel.sftp) else { break };
            let Some(account) = self.account else { break };
            if self.sftp_in_length == 0 {
                break;
            }
            let (exports, files) = host.split();
            let mut writer = Writer::new(self.sftp_out);
            match self.sftp.step(exports, files, &account, &self.sftp_in[..self.sftp_in_length], &mut writer) {
                Step::More => break,
                Step::Bad => {
                    self.disconnect(REASON_PROTOCOL_ERROR, "bad SFTP request");
                    return;
                }
                Step::Done(used) => {
                    self.sftp_out_length = writer.len();
                    self.sftp_in.copy_within(used..self.sftp_in_length, 0);
                    self.sftp_in_length -= used;
                    channel.to_return += used;
                }
            }
        }
        self.finish_channel(host);
        self.give_window();
    }

    fn read_version<H: Host>(&mut self, _host: &mut H, input: &[u8]) -> usize {
        let Some(end) = input.iter().position(|&byte| byte == b'\n') else {
            if input.len() > 255 {
                self.disconnect(REASON_PROTOCOL_ERROR, "no identification line");
            }
            return 0;
        };
        let line = input[..end].strip_suffix(b"\r").unwrap_or(&input[..end]);
        if !(line.starts_with(b"SSH-2.0-") || line.starts_with(b"SSH-1.99-")) || line.len() > 255 {
            self.disconnect(REASON_PROTOCOL_ERROR, "this server speaks SSH 2.0 only");
            return end + 1;
        }
        self.version_client[..line.len()].copy_from_slice(line);
        self.version_client_length = line.len();
        self.phase = Phase::KexInit;
        end + 1
    }

    fn disconnect(&mut self, code: u32, text: &str) {
        if self.phase == Phase::Closed {
            return;
        }
        self.tx.packet(|writer| {
            writer.u8(MSG_DISCONNECT).u32(code).string(text.as_bytes()).string(b"");
        });
        self.phase = Phase::Closed;
    }

    fn send_kexinit<H: Host>(&mut self, host: &mut H) {
        let mut payload = [0u8; 512];
        let mut writer = Writer::new(&mut payload);
        let mut cookie = [0u8; 16];
        host.random(&mut cookie);
        writer.u8(MSG_KEXINIT).raw(&cookie);
        for list in [
            "curve25519-sha256,curve25519-sha256@libssh.org,kex-strict-s-v00@openssh.com",
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
        let length = writer.len();
        self.kexinit_server[..length].copy_from_slice(&payload[..length]);
        self.kexinit_server_length = length;
        self.tx.packet(|out| {
            out.raw(&payload[..length]);
        });
        self.kex_sent = true;
    }

    fn dispatch<H: Host>(&mut self, host: &mut H, payload: &[u8]) -> Result<(), (u32, &'static str)> {
        let Some(&kind) = payload.first() else { return Err((REASON_PROTOCOL_ERROR, "empty packet")) };
        let strict_start = self.strict && !self.established;
        if self.skip_next {
            self.skip_next = false;
            return Ok(());
        }
        match kind {
            MSG_DISCONNECT => {
                self.phase = Phase::Closed;
                self.tx.length = 0;
                self.tx.sent = 0;
                return Ok(());
            }
            MSG_IGNORE | MSG_DEBUG | MSG_UNIMPLEMENTED if !strict_start => return Ok(()),
            _ => {}
        }
        match (self.phase, kind) {
            (Phase::KexInit, MSG_KEXINIT) | (Phase::Running, MSG_KEXINIT) => self.on_kexinit(host, payload),
            (Phase::KexDh, MSG_KEX_ECDH_INIT) => self.on_dh_init(host, payload),
            (Phase::NewKeys, MSG_NEWKEYS) => {
                self.rx.keys = self.next_receive.take();
                if self.strict {
                    self.rx.sequence = 0;
                }
                self.phase = Phase::Running;
                self.established = true;
                self.kex_sent = false;
                Ok(())
            }
            (Phase::Running, MSG_SERVICE_REQUEST) => {
                let service = Reader::new(&payload[1..]).string().unwrap_or(&[]);
                if service != b"ssh-userauth" || self.service_ok {
                    return Err((REASON_SERVICE_NOT_AVAILABLE, "service not available"));
                }
                self.service_ok = true;
                self.tx.packet(|writer| {
                    writer.u8(MSG_SERVICE_ACCEPT).string(b"ssh-userauth");
                });
                Ok(())
            }
            (Phase::Running, MSG_USERAUTH_REQUEST) if self.service_ok && self.account.is_none() => self.on_auth(host, payload),
            (Phase::Running, MSG_GLOBAL_REQUEST..=MSG_CHANNEL_FAILURE) if self.account.is_some() => self.on_connection(host, payload),
            (Phase::Running, _) => {
                let rejected = self.rx.sequence.wrapping_sub(1);
                self.tx.packet(|writer| {
                    writer.u8(MSG_UNIMPLEMENTED).u32(rejected);
                });
                Ok(())
            }
            _ => Err((REASON_PROTOCOL_ERROR, "unexpected message")),
        }
    }

    fn on_kexinit<H: Host>(&mut self, host: &mut H, payload: &[u8]) -> Result<(), (u32, &'static str)> {
        let bad = (REASON_PROTOCOL_ERROR, "bad KEXINIT");
        let mut reader = Reader::new(&payload[1..]);
        reader.bytes(16).ok_or(bad)?;
        let mut lists = [&[][..]; 10];
        for list in &mut lists {
            *list = reader.string().ok_or(bad)?;
        }
        let guess = reader.bool().ok_or(bad)?;
        let [kex, hostkey, enc_c2s, enc_s2c, _, _, comp_c2s, comp_s2c, _, _] = lists;
        let fail = (REASON_KEY_EXCHANGE_FAILED, "no common algorithms: need curve25519-sha256, ssh-ed25519, chacha20-poly1305@openssh.com");
        let kex_ok = has(kex, "curve25519-sha256") || has(kex, "curve25519-sha256@libssh.org");
        if !(kex_ok && has(hostkey, "ssh-ed25519") && has(enc_c2s, "chacha20-poly1305@openssh.com") && has(enc_s2c, "chacha20-poly1305@openssh.com") && has(comp_c2s, "none") && has(comp_s2c, "none")) {
            return Err(fail);
        }
        if !self.established {
            self.strict = has(kex, "kex-strict-c-v00@openssh.com");
            if self.strict && self.rx.sequence != 1 {
                return Err((REASON_PROTOCOL_ERROR, "strict key exchange: KEXINIT must come first"));
            }
        }
        if payload.len() > KEXINIT_MAX {
            return Err(bad);
        }
        self.kexinit_client[..payload.len()].copy_from_slice(payload);
        self.kexinit_client_length = payload.len();
        // A guessed first packet of the wrong kind is to be dropped (RFC 4253 section 7).
        let right = matches!(first(kex), b"curve25519-sha256" | b"curve25519-sha256@libssh.org") && first(hostkey) == b"ssh-ed25519";
        self.skip_next = guess && !right;
        if !self.kex_sent {
            self.send_kexinit(host);
        }
        self.phase = Phase::KexDh;
        Ok(())
    }

    fn on_dh_init<H: Host>(&mut self, host: &mut H, payload: &[u8]) -> Result<(), (u32, &'static str)> {
        let bad = (REASON_KEY_EXCHANGE_FAILED, "bad key exchange");
        let client_public: [u8; 32] = Reader::new(&payload[1..]).string().and_then(|key| key.try_into().ok()).ok_or(bad)?;
        let mut seed = [0u8; 32];
        host.random(&mut seed);
        let secret = StaticSecret::from(seed);
        let public = PublicKey::from(&secret);
        let shared = secret.diffie_hellman(&PublicKey::from(client_public));
        if !shared.was_contributory() {
            return Err(bad);
        }
        let signing = SigningKey::from_bytes(&host.host_seed());
        let mut blob = [0u8; 64];
        let mut writer = Writer::new(&mut blob);
        writer.string(b"ssh-ed25519").string(signing.verifying_key().as_bytes());
        let blob_length = writer.len();
        let mut secret_bytes = [0u8; 40];
        let mut writer = Writer::new(&mut secret_bytes);
        writer.mpint(shared.as_bytes());
        let secret_length = writer.len();
        let secret_mpint = &secret_bytes[..secret_length];

        let mut hasher = Sha256::new();
        hash_string(&mut hasher, &self.version_client[..self.version_client_length]);
        hash_string(&mut hasher, VERSION);
        hash_string(&mut hasher, &self.kexinit_client[..self.kexinit_client_length]);
        hash_string(&mut hasher, &self.kexinit_server[..self.kexinit_server_length]);
        hash_string(&mut hasher, &blob[..blob_length]);
        hash_string(&mut hasher, &client_public);
        hash_string(&mut hasher, public.as_bytes());
        hasher.update(secret_mpint);
        let exchange_hash: [u8; 32] = hasher.finalize().into();
        let session_id = *self.session_id.get_or_insert(exchange_hash);
        let signature = signing.sign(&exchange_hash).to_bytes();

        let sent = self.tx.packet(|out| {
            let mut signature_blob = [0u8; 96];
            let mut inner = Writer::new(&mut signature_blob);
            inner.string(b"ssh-ed25519").string(&signature);
            out.u8(MSG_KEX_ECDH_REPLY).string(&blob[..blob_length]).string(public.as_bytes()).string(inner.written());
        });
        let sent = sent && self.tx.packet(|out| {
            out.u8(MSG_NEWKEYS);
        });
        if !sent {
            return Err((REASON_KEY_EXCHANGE_FAILED, "no room for the key exchange reply"));
        }
        self.tx.keys = Some(derive(secret_mpint, &exchange_hash, b'D', &session_id));
        if self.strict {
            self.tx.sequence = 0;
        }
        self.next_receive = Some(derive(secret_mpint, &exchange_hash, b'C', &session_id));
        self.phase = Phase::NewKeys;
        Ok(())
    }

    fn on_auth<H: Host>(&mut self, host: &mut H, payload: &[u8]) -> Result<(), (u32, &'static str)> {
        let bad = (REASON_PROTOCOL_ERROR, "bad authentication request");
        let mut reader = Reader::new(&payload[1..]);
        let user = reader.text().ok_or(bad)?;
        let service = reader.string().ok_or(bad)?;
        let method = reader.string().ok_or(bad)?;
        self.auth_requests += 1;
        if self.auth_requests > AUTH_REQUESTS {
            return Err((REASON_NO_MORE_AUTH_METHODS, "too many authentication attempts"));
        }
        if service != b"ssh-connection" {
            return Err((REASON_SERVICE_NOT_AVAILABLE, "service not available"));
        }
        if method == b"publickey" {
            let has_signature = reader.bool().ok_or(bad)?;
            let algorithm = reader.string().ok_or(bad)?;
            let blob = reader.string().ok_or(bad)?;
            let key = Self::ed25519_key(blob);
            let account = match (algorithm, key) {
                (b"ssh-ed25519", Some(key)) => host.authorize(user, &key).map(|account| (account, key)),
                _ => None,
            };
            if let Some((account, key)) = account {
                if !has_signature {
                    self.tx.packet(|out| {
                        out.u8(MSG_USERAUTH_PK_OK).string(algorithm).string(blob);
                    });
                    return Ok(());
                }
                let signed = self.signed_data(user, algorithm, blob);
                let signature = reader.string().ok_or(bad)?;
                if signed.is_some_and(|signed| Self::verify(&key, signed.written(), signature)) {
                    self.user[..user.len().min(64)].copy_from_slice(&user.as_bytes()[..user.len().min(64)]);
                    self.user_length = user.len().min(64);
                    self.account = Some(account);
                    host.log(format_args!("{user} logged in with {}", self.version_text()));
                    self.tx.packet(|out| {
                        out.u8(MSG_USERAUTH_SUCCESS);
                    });
                    return Ok(());
                }
                self.auth_failures += 1;
                host.log(format_args!("{user}: bad signature"));
            } else {
                host.log(format_args!("{user}: key not accepted"));
            }
        }
        if self.auth_failures >= AUTH_FAILURES {
            return Err((REASON_NO_MORE_AUTH_METHODS, "too many authentication failures"));
        }
        self.tx.packet(|out| {
            out.u8(MSG_USERAUTH_FAILURE).string(b"publickey").bool(false);
        });
        Ok(())
    }

    fn version_text(&self) -> &str {
        core::str::from_utf8(&self.version_client[..self.version_client_length]).unwrap_or("?")
    }

    fn ed25519_key(blob: &[u8]) -> Option<[u8; 32]> {
        let mut reader = Reader::new(blob);
        (reader.string()? == b"ssh-ed25519").then_some(())?;
        reader.string()?.try_into().ok()
    }

    /// What the client signs to log in (RFC 4252 section 7).
    fn signed_data(&self, user: &str, algorithm: &[u8], blob: &[u8]) -> Option<SignedData> {
        let mut data = SignedData { bytes: [0; 512], length: 0 };
        let mut writer = Writer::new(&mut data.bytes);
        writer.string(self.session_id.as_ref()?).u8(MSG_USERAUTH_REQUEST).string(user.as_bytes()).string(b"ssh-connection").string(b"publickey");
        writer.bool(true).string(algorithm).string(blob);
        if !writer.ok() {
            return None;
        }
        data.length = writer.len();
        Some(data)
    }

    fn verify(key: &[u8; 32], message: &[u8], signature_blob: &[u8]) -> bool {
        let mut reader = Reader::new(signature_blob);
        let (Some(name), Some(signature)) = (reader.string(), reader.string()) else { return false };
        let (Ok(signature), Ok(key)) = (<[u8; 64]>::try_from(signature), VerifyingKey::from_bytes(key)) else { return false };
        name == b"ssh-ed25519" && key.verify(message, &Signature::from_bytes(&signature)).is_ok()
    }

    /// One pass for a shell channel: typed bytes to the shell, its output into the reply buffer, the end of the session.
    fn poll_shell<H: Host>(&mut self, host: &mut H, shell: u32) {
        let Some(channel) = self.channel.as_mut() else { return };
        if channel.closing {
            return;
        }
        if self.sftp_in_length > 0 {
            let taken = host.shell_write(shell, &self.sftp_in[..self.sftp_in_length]).min(self.sftp_in_length);
            self.sftp_in.copy_within(taken..self.sftp_in_length, 0);
            self.sftp_in_length -= taken;
            channel.to_return += taken;
        }
        let room = self.sftp_out.len().min(SHELL_CHUNK);
        match host.shell_read(shell, &mut self.sftp_out[..room]) {
            ShellRead::Data(count) => self.sftp_out_length = count.min(room),
            ShellRead::Closed => {
                let peer = channel.peer;
                channel.shell = None;
                channel.closing = true;
                host.shell_close(shell);
                self.tx.packet(|out| {
                    out.u8(MSG_CHANNEL_REQUEST).u32(peer).string(b"exit-status").bool(false).u32(0);
                });
                self.tx.packet(|out| {
                    out.u8(MSG_CHANNEL_EOF).u32(peer);
                });
                self.tx.packet(|out| {
                    out.u8(MSG_CHANNEL_CLOSE).u32(peer);
                });
            }
        }
    }

    /// The connection is going away: end the shell it holds, if any.
    pub fn shutdown<H: Host>(&mut self, host: &mut H) {
        if let Some(shell) = self.channel.as_mut().and_then(|channel| channel.shell.take()) {
            host.shell_close(shell);
        }
    }

    fn on_connection<H: Host>(&mut self, host: &mut H, payload: &[u8]) -> Result<(), (u32, &'static str)> {
        let bad = (REASON_PROTOCOL_ERROR, "bad connection message");
        let mut reader = Reader::new(&payload[1..]);
        match payload[0] {
            MSG_GLOBAL_REQUEST => {
                reader.string().ok_or(bad)?;
                if reader.bool().ok_or(bad)? {
                    self.tx.packet(|out| {
                        out.u8(MSG_REQUEST_FAILURE);
                    });
                }
            }
            MSG_CHANNEL_OPEN => {
                let kind = reader.string().ok_or(bad)?;
                let (peer, window, max) = (reader.u32().ok_or(bad)?, reader.u32().ok_or(bad)?, reader.u32().ok_or(bad)?);
                if kind != b"session" || self.channel.is_some() {
                    self.tx.packet(|out| {
                        out.u8(MSG_CHANNEL_OPEN_FAILURE).u32(peer).u32(if kind == b"session" { 4 } else { 3 }).string(b"only one session at a time").string(b"");
                    });
                    return Ok(());
                }
                self.sftp_in_length = 0;
                self.sftp_out_length = 0;
                self.sftp_out_sent = 0;
                self.sftp.reset();
                let granted = self.sftp_in.len();
                self.channel = Some(Channel { peer, peer_window: window as u64, peer_max: max.max(1), granted, to_return: 0, sftp: false, shell: None, peer_eof: false, closing: false });
                self.tx.packet(|out| {
                    out.u8(MSG_CHANNEL_OPEN_CONFIRMATION).u32(peer).u32(0).u32(granted as u32).u32(MAX_DATA as u32);
                });
            }
            MSG_CHANNEL_WINDOW_ADJUST => {
                reader.u32().ok_or(bad)?;
                let add = reader.u32().ok_or(bad)? as u64;
                if let Some(channel) = self.channel.as_mut() {
                    channel.peer_window = (channel.peer_window + add).min(u32::MAX as u64);
                }
            }
            MSG_CHANNEL_DATA => {
                reader.u32().ok_or(bad)?;
                let data = reader.string().ok_or(bad)?;
                let channel = self.channel.as_mut().ok_or(bad)?;
                if data.len() > channel.granted || self.sftp_in_length + data.len() > self.sftp_in.len() {
                    return Err((REASON_PROTOCOL_ERROR, "more data than the window allows"));
                }
                channel.granted -= data.len();
                if channel.sftp || channel.shell.is_some() {
                    self.sftp_in[self.sftp_in_length..self.sftp_in_length + data.len()].copy_from_slice(data);
                    self.sftp_in_length += data.len();
                } else {
                    channel.to_return += data.len();
                }
            }
            MSG_CHANNEL_EXTENDED_DATA => {
                reader.u32().ok_or(bad)?;
                reader.u32().ok_or(bad)?;
                let data = reader.string().ok_or(bad)?;
                let channel = self.channel.as_mut().ok_or(bad)?;
                if data.len() > channel.granted {
                    return Err((REASON_PROTOCOL_ERROR, "more data than the window allows"));
                }
                channel.granted -= data.len();
                channel.to_return += data.len();
            }
            MSG_CHANNEL_REQUEST => {
                reader.u32().ok_or(bad)?;
                let kind = reader.string().ok_or(bad)?;
                let want_reply = reader.bool().ok_or(bad)?;
                let channel = self.channel.as_mut().ok_or(bad)?;
                let peer = channel.peer;
                let allowed = self.account.is_some_and(|account| account.shell);
                let ok = match kind {
                    b"subsystem" => {
                        let ok = reader.string() == Some(b"sftp") && !channel.sftp && channel.shell.is_none();
                        if ok {
                            channel.sftp = true;
                            host.log(format_args!("sftp session"));
                        }
                        ok
                    }
                    // There is no pty: the shell echoes and edits the line itself, so the terminal's size and modes are
                    // accepted and ignored.
                    b"pty-req" | b"env" | b"window-change" => allowed && !channel.sftp,
                    b"shell" if allowed && !channel.sftp && channel.shell.is_none() => match host.shell_open() {
                        Some(number) => {
                            channel.shell = Some(number);
                            host.log(format_args!("shell session"));
                            true
                        }
                        None => false,
                    },
                    _ => false,
                };
                if want_reply {
                    self.tx.packet(|out| {
                        out.u8(if ok { MSG_CHANNEL_SUCCESS } else { MSG_CHANNEL_FAILURE }).u32(peer);
                    });
                }
            }
            MSG_CHANNEL_EOF => {
                reader.u32().ok_or(bad)?;
                if let Some(channel) = self.channel.as_mut() {
                    channel.peer_eof = true;
                }
            }
            MSG_CHANNEL_CLOSE => {
                reader.u32().ok_or(bad)?;
                if let Some(channel) = self.channel.take() {
                    if let Some(shell) = channel.shell {
                        host.shell_close(shell);
                    }
                    if !channel.closing {
                        self.tx.packet(|out| {
                            out.u8(MSG_CHANNEL_CLOSE).u32(channel.peer);
                        });
                    }
                    let (_, files) = host.split();
                    let _ = files.sync();
                    self.sftp.reset();
                    self.sftp_in_length = 0;
                    self.sftp_out_length = 0;
                    self.sftp_out_sent = 0;
                }
            }
            MSG_CHANNEL_SUCCESS | MSG_CHANNEL_FAILURE | MSG_REQUEST_FAILURE => {}
            _ => {
                let rejected = self.rx.sequence.wrapping_sub(1);
                self.tx.packet(|out| {
                    out.u8(MSG_UNIMPLEMENTED).u32(rejected);
                });
            }
        }
        Ok(())
    }

    /// Sends one CHANNEL_DATA from the reply buffer, as much as the client's window and packet size take.
    fn pump(&mut self) {
        let Some(channel) = self.channel.as_mut() else { return };
        let pending = self.sftp_out_length - self.sftp_out_sent;
        let count = pending.min(channel.peer_window as usize).min(channel.peer_max as usize).min(MAX_DATA);
        if count == 0 {
            return;
        }
        let chunk = &self.sftp_out[self.sftp_out_sent..self.sftp_out_sent + count];
        let peer = channel.peer;
        if self.tx.packet(|out| {
            out.u8(MSG_CHANNEL_DATA).u32(peer).string(chunk);
        }) {
            channel.peer_window -= count as u64;
            self.sftp_out_sent += count;
        }
    }

    /// Gives consumed request room back to the client as window.
    fn give_window(&mut self) {
        if self.phase != Phase::Running {
            return;
        }
        let Some(channel) = self.channel.as_mut() else { return };
        let idle = self.sftp_in_length == 0 && channel.to_return > 0;
        if channel.closing || !(channel.to_return >= 32 * 1024 || idle) {
            return;
        }
        let (peer, add) = (channel.peer, channel.to_return);
        if self.tx.packet(|out| {
            out.u8(MSG_CHANNEL_WINDOW_ADJUST).u32(peer).u32(add as u32);
        }) {
            channel.granted += add;
            channel.to_return = 0;
        }
    }

    /// The client is done sending: close the channel once every reply has gone out.
    fn finish_channel<H: Host>(&mut self, host: &mut H) {
        let Some(channel) = self.channel.as_mut() else { return };
        if channel.peer_eof && !channel.closing && channel.shell.is_none() && self.sftp_in_length == 0 && self.sftp_out_length == self.sftp_out_sent {
            let peer = channel.peer;
            if self.tx.packet(|out| {
                out.u8(MSG_CHANNEL_CLOSE).u32(peer);
            }) {
                channel.closing = true;
                let (_, files) = host.split();
                let _ = files.sync();
            }
        }
    }
}

struct SignedData {
    bytes: [u8; 512],
    length: usize,
}

impl SignedData {
    fn written(&self) -> &[u8] {
        &self.bytes[..self.length]
    }
}
