//! smbd: an SMB 2/3 file server (guest access, no signing, no encryption) for one folder of any volume the kernel
//! can read, ext2/3/4 included (`vfs_*`, tape ABI 32). The protocol is `crates/smb` (`server`); this tape is its TCP
//! front end, a service like `httpd` (`run smbd @SMBD.CFG &2`). Logins are checked against the `smbusers` file (NTLMv2);
//! without that file everyone is a guest, so use it on a network you trust.
#![no_std]
#![no_main]

use core::sync::atomic::{AtomicU64, Ordering};
use smb::ntlm::{md5, nt_hash};
use smb::server::{Account, Fail, Identity, MAX_IO, Server, Share, Stat};
use tape_ip::net::{self, NetConfig};
use tape_ip::tcp::Server as Tcp;
use tape_net::MacAddress;
use tape_rt::config::Config;
use tape_rt::{TapeApi, arg_value as value, println};

tape_rt::tape_help!(
    br#"smbd [@NAME] [root=DIR] [share=NAME] [port=445] [nic=N] [ip=A.B.C.D/N]
      [gateway=A.B.C.D] [log] [ro]
An SMB file server (SMB 2.0.2 to 3.0, no signing): Linux mount.cifs/smbclient,
Windows and macOS can browse, copy from it and, on ext volumes, write to it. It runs as
a service: run smbd @SMBD.CFG &2 (the service owns NIC 2; tape run smbd takes
the foreground and Escape stops it). Settings are key=value words on the
command line or in the file (one per line, # for comments).
root=   the folder shared: disk2 (a whole ext4 disk), ext1 (the ext4 partition of a disk that also has FOSFS), disk2/music, @LABEL/site,
        a FOSFS or FAT folder, or a //NAME mount (smbmount): FOS as a
        gateway. Default: the boot volume's root.
share=  the share name clients use, //HOST/NAME (default: share).
port=   TCP port (default 445).
ip=     a fixed address like 192.168.1.20/24 (gateway= for the router);
        without it DHCP. nic= is the NIC number from 1 (default: nic use).
log     print one line per connection.
ro      refuse all changes (otherwise ext volumes can be written).
Users: the file smbusers, one line per user, name:uid:gid:NTHASH. The line
guest:uid:gid: lets anonymous and unknown logins in (as that uid/gid). With
the file present, other logins need the password; without it everyone is a
guest. A file's owner/group/other bits are checked against the uid and gid
(uid 0 sees everything). Make a line with: smbd user NAME UID GID PASSWORD
Changes (put, mkdir, rename, delete) need tape ABI 34 and an ext2/3/4 volume;
a new file belongs to the logged-in uid/gid, and the folder's mode bits decide
who may add or remove entries. FOSFS and FAT shares stay read-only.
Needs tape ABI 32.
"#
);

const SLOTS: usize = 4;
const OPENS: usize = 16;
/// Room for the largest message: a WRITE of MAX_IO bytes plus the headers.
const RX: usize = MAX_IO + 4096;
const ROOT_MAX: usize = 200;
const SEND_TIMEOUT: u64 = 5_000_000;

const USERS: usize = 8;

/// The accounts of the `smbusers` file. `open` is false when there is no file: everyone is a guest then.
struct Users {
    open: bool,
    guest: Option<Identity>,
    names: [([u8; 32], usize, Account); USERS],
    count: usize,
}

static mut USER_TABLE: Users = Users { open: false, guest: None, names: [([0; 32], 0, Account { nt_hash: [0; 16], identity: Identity { uid: 0, gid: 0 } }); USERS], count: 0 };

fn users() -> &'static mut Users {
    #[allow(static_mut_refs)]
    unsafe {
        &mut *core::ptr::addr_of_mut!(USER_TABLE)
    }
}

fn hex_hash(text: &str) -> Option<[u8; 16]> {
    if text.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(text.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

impl Users {
    fn load(&mut self) -> Result<(), &'static str> {
        let mut buffer = [0u8; 1024];
        let Ok(length) = tape_rt::read_file_at("smbusers", 0, &mut buffer) else { return Ok(()) };
        let text = core::str::from_utf8(&buffer[..length]).map_err(|_| "smbusers is not text")?;
        self.open = true;
        for line in text.lines().map(str::trim).filter(|line| !line.is_empty() && !line.starts_with('#')) {
            let mut fields = line.split(':');
            let (Some(name), Some(uid), Some(gid)) = (fields.next(), fields.next(), fields.next()) else { return Err("smbusers: a line is not name:uid:gid:hash") };
            let (Ok(uid), Ok(gid)) = (uid.trim().parse(), gid.trim().parse()) else { return Err("smbusers: uid and gid are numbers") };
            let identity = Identity { uid, gid };
            if name == "guest" {
                self.guest = Some(identity);
            } else {
                let hash = hex_hash(fields.next().unwrap_or("").trim()).ok_or("smbusers: the hash is 32 hex digits")?;
                if self.count == USERS || name.len() > 32 {
                    return Err("smbusers: too many users or a name too long");
                }
                self.names[self.count].0[..name.len()].copy_from_slice(name.as_bytes());
                self.names[self.count].1 = name.len();
                self.names[self.count].2 = Account { nt_hash: hash, identity };
                self.count += 1;
            }
        }
        Ok(())
    }
}

struct Files {
    root: [u8; ROOT_MAX],
    root_length: usize,
    name: [u8; 32],
    name_length: usize,
    read_only: bool,
}

/// The `Fail` an ABI 34 error code stands for.
fn fail(code: isize) -> Fail {
    match code {
        tape_rt::FS_NOT_FOUND => Fail::NotFound,
        tape_rt::FS_EXISTS => Fail::Exists,
        tape_rt::FS_NO_SPACE => Fail::NoSpace,
        tape_rt::FS_NOT_EMPTY => Fail::NotEmpty,
        tape_rt::FS_IS_DIR => Fail::IsDir,
        tape_rt::FS_READ_ONLY | tape_rt::FS_BAD_NAME => Fail::Denied,
        _ => Fail::Io,
    }
}

impl Files {
    const fn new() -> Self {
        Files { root: [0; ROOT_MAX], root_length: 0, name: [0; 32], name_length: 0, read_only: false }
    }

    /// `root` and `path` joined with a slash, into `out`.
    fn join<'a>(&self, path: &str, out: &'a mut [u8; 512]) -> Option<&'a str> {
        let root = &self.root[..self.root_length];
        if root.len() + 1 + path.len() > out.len() {
            return None;
        }
        out[..root.len()].copy_from_slice(root);
        let mut at = root.len();
        if !path.is_empty() {
            if at > 0 {
                out[at] = b'/';
                at += 1;
            }
            out[at..at + path.len()].copy_from_slice(path.as_bytes());
            at += path.len();
        }
        core::str::from_utf8(&out[..at]).ok()
    }
}

fn stat_of(stat: &tape_rt::VfsStat) -> Stat {
    Stat { dir: stat.kind == tape_rt::VFS_DIR, size: stat.size, mtime: stat.mtime, mode: stat.mode & 0o777, uid: stat.uid, gid: stat.gid }
}

impl Share for Files {
    fn name(&self) -> &str {
        core::str::from_utf8(&self.name[..self.name_length]).unwrap_or("share")
    }

    fn stat(&mut self, path: &str) -> Option<Stat> {
        let mut buffer = [0u8; 512];
        let reference = self.join(path, &mut buffer)?;
        tape_rt::vfs_stat(reference).ok().map(|stat| stat_of(&stat))
    }

    fn read_dir(&mut self, path: &str, index: usize, name: &mut [u8]) -> Option<(usize, Stat)> {
        let mut buffer = [0u8; 512];
        let reference = self.join(path, &mut buffer)?;
        let entry = tape_rt::vfs_read_dir(reference, index).ok()??;
        let length = entry.name().len().min(name.len());
        name[..length].copy_from_slice(&entry.name()[..length]);
        Some((length, stat_of(&entry.stat)))
    }

    fn account(&mut self, user: &str) -> Option<Account> {
        let table = users();
        table.names[..table.count].iter().find(|(name, length, _)| name[..*length].eq_ignore_ascii_case(user.as_bytes())).map(|entry| entry.2)
    }

    fn guest(&mut self) -> Option<Identity> {
        let table = users();
        if table.open { table.guest } else { Some(Identity { uid: 65534, gid: 65534 }) }
    }

    fn read(&mut self, path: &str, offset: u64, out: &mut [u8]) -> Result<usize, ()> {
        let mut buffer = [0u8; 512];
        let reference = self.join(path, &mut buffer).ok_or(())?;
        let start = tape_rt::micros();
        let result = tape_rt::vfs_read(reference, offset, out).map_err(|_| ());
        READ_US.fetch_add(tape_rt::micros() - start, Ordering::Relaxed);
        READ_BYTES.fetch_add(*result.as_ref().unwrap_or(&0) as u64, Ordering::Relaxed);
        result
    }

    fn writable(&self) -> bool {
        !self.read_only && tape_rt::vfs_can_change()
    }

    fn create(&mut self, path: &str, dir: bool, mode: u32, owner: Identity) -> Result<(), Fail> {
        let mut buffer = [0u8; 512];
        let reference = self.join(path, &mut buffer).ok_or(Fail::Io)?;
        let kind = if dir { tape_rt::VFS_DIR } else { tape_rt::VFS_FILE };
        tape_rt::vfs_create(reference, kind, mode, owner.uid, owner.gid).map_err(fail)
    }

    fn write(&mut self, path: &str, offset: u64, data: &[u8]) -> Result<(), Fail> {
        let mut buffer = [0u8; 512];
        let reference = self.join(path, &mut buffer).ok_or(Fail::Io)?;
        let mut done = 0;
        let start = tape_rt::micros();
        let _timing = Timed(start);
        WRITE_BYTES.fetch_add(data.len() as u64, Ordering::Relaxed);
        while done < data.len() {
            let count = tape_rt::vfs_write(reference, offset + done as u64, &data[done..]).map_err(fail)?;
            if count == 0 {
                return Err(Fail::Io);
            }
            done += count;
        }
        Ok(())
    }

    fn truncate(&mut self, path: &str, size: u64) -> Result<(), Fail> {
        let mut buffer = [0u8; 512];
        let reference = self.join(path, &mut buffer).ok_or(Fail::Io)?;
        tape_rt::vfs_truncate(reference, size).map_err(fail)
    }

    fn remove(&mut self, path: &str) -> Result<(), Fail> {
        let mut buffer = [0u8; 512];
        let reference = self.join(path, &mut buffer).ok_or(Fail::Io)?;
        tape_rt::vfs_remove(reference).map_err(fail)
    }

    fn rename(&mut self, from: &str, to: &str) -> Result<(), Fail> {
        let (mut first, mut second) = ([0u8; 512], [0u8; 512]);
        let from = self.join(from, &mut first).ok_or(Fail::Io)?;
        let to = self.join(to, &mut second).ok_or(Fail::Io)?;
        tape_rt::vfs_rename(from, to).map_err(fail)
    }
}

/// Where the time goes (shown with `log` when a connection ends): microseconds and bytes spent in file reads and in sending answers.
static WRITE_US: AtomicU64 = AtomicU64::new(0);
static WRITE_BYTES: AtomicU64 = AtomicU64::new(0);

/// Adds the time since its start to `WRITE_US` when dropped, whichever way `write` returns.
struct Timed(u64);

impl Drop for Timed {
    fn drop(&mut self) {
        WRITE_US.fetch_add(tape_rt::micros() - self.0, Ordering::Relaxed);
    }
}
static READ_US: AtomicU64 = AtomicU64::new(0);
static READ_BYTES: AtomicU64 = AtomicU64::new(0);
static SEND_US: AtomicU64 = AtomicU64::new(0);
static SEND_BYTES: AtomicU64 = AtomicU64::new(0);
static CONNECTED_US: AtomicU64 = AtomicU64::new(0);

static mut TCP: Tcp<RX, SLOTS> = Tcp::new();
static mut REQUEST: [u8; RX] = [0; RX];
static mut ANSWER: [u8; MAX_IO + 256] = [0; MAX_IO + 256];
static mut SESSIONS: [Option<Server<Files, OPENS>>; SLOTS] = [const { None }; SLOTS];

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(api: &'static TapeApi) {
    tape_rt::init(api);
    let arguments = core::str::from_utf8(tape_rt::args()).unwrap_or("");
    if let Some(rest) = arguments.strip_prefix("user ") {
        return make_user_line(rest);
    }
    if let Err(reason) = users().load() {
        return println!("smbd: {reason}.");
    }
    let config = match Config::load(api, "SMBD", core::str::from_utf8(tape_rt::args()).unwrap_or("")) {
        Ok(config) => config,
        Err(reason) => return println!("smbd: {reason}."),
    };
    let text = config.text();
    let root = value(text, "root").unwrap_or("").trim_end_matches('/');
    let share = value(text, "share").unwrap_or("share");
    if root.len() > ROOT_MAX || share.len() > 32 {
        return println!("smbd: root= or share= is too long.");
    }
    if tape_rt::vfs_stat(root).map_or(true, |stat| stat.kind != tape_rt::VFS_DIR) {
        return println!("smbd: {} is not a folder this kernel can read (needs tape ABI 32).", if root.is_empty() { "the boot volume" } else { root });
    }
    if users().open {
        println!("smbd: {} user(s) from smbusers{}.", users().count, if users().guest.is_some() { ", guests allowed" } else { ", no guests" });
    } else {
        println!("smbd: no smbusers file: everyone is a guest.");
    }
    let read_only = text.split_ascii_whitespace().any(|word| word == "ro");
    let log = text.split_ascii_whitespace().any(|word| word == "log" || word == "log=on");
    let Some(port) = value(text, "port").map_or(Some(445), |word| word.parse::<u16>().ok().filter(|&port| port != 0)) else {
        return println!("smbd: port= takes a number from 1 to 65535.");
    };
    let nic = value(text, "nic").and_then(|word| word.parse::<usize>().ok()).map_or(tape_rt::active_nic(), |n| n.saturating_sub(1));
    if (api.nic_count)() <= nic {
        return println!("smbd: this machine has no NIC {}.", nic + 1);
    }
    let mut mac = [0u8; 6];
    (api.nic_mac)(nic, mac.as_mut_ptr());
    let mac = MacAddress(mac);
    let network: NetConfig = match value(text, "ip") {
        Some(ip) => match net::static_config(mac, ip, value(text, "gateway").unwrap_or("")) {
            Ok(network) => network,
            Err(reason) => return println!("smbd: {reason}."),
        },
        None => match net::dhcp(api, nic, mac) {
            Some(network) => network,
            None => return println!("smbd: no DHCP lease (give ip=A.B.C.D/N for a fixed address)."),
        },
    };
    #[allow(static_mut_refs)]
    let (tcp, request, answer, sessions) = unsafe {
        (&mut *core::ptr::addr_of_mut!(TCP), &mut *core::ptr::addr_of_mut!(REQUEST), &mut *core::ptr::addr_of_mut!(ANSWER), &mut *core::ptr::addr_of_mut!(SESSIONS))
    };
    let (mut pool, mut state) = (0u64, [0u8; 16]);
    let (pool, state) = (&mut pool, &mut state);
    tcp.start(nic, &network, port);
    CONNECTED_US.store(tape_rt::micros(), Ordering::Relaxed);
    let mut last_busy = 0u64;
    println!("smbd: NIC {} at {}.{}.{}.{}:{port}, sharing {} as //HOST/{share}.", nic + 1, network.ip.0[0], network.ip.0[1], network.ip.0[2], network.ip.0[3], if root.is_empty() { "the boot volume" } else { root });
    loop {
        if (api.should_abort)() != 0 {
            break;
        }
        let busy = tcp.poll(api, 16);
        for slot in 0..SLOTS {
            if !tcp.is_open(slot) {
                sessions[slot] = None;
                continue;
            }
            if sessions[slot].is_none() {
                let mut files = Files::new();
                files.root[..root.len()].copy_from_slice(root.as_bytes());
                files.root_length = root.len();
                files.name[..share.len()].copy_from_slice(share.as_bytes());
                files.name_length = share.len();
                files.read_only = read_only;
                let mut session = Server::new(files);
                // ABI 33 `random`; a kernel without it falls back to a hash of the clock, a counter and the peer
                // (unpredictable only to a peer that cannot guess the microsecond; docs/smb.md).
                let mut fresh = [0u8; 24];
                if tape_rt::random(&mut fresh).is_none() {
                    let (ip, port) = tcp.peer(slot);
                    *pool = pool.wrapping_add(1);
                    let mixed = md5(&[&tape_rt::micros().to_le_bytes(), &pool.to_le_bytes(), &ip.0, &port.to_le_bytes(), &state[..]]);
                    state.copy_from_slice(&mixed);
                    fresh[..16].copy_from_slice(&mixed);
                    fresh[16..].copy_from_slice(&mixed[..8]);
                }
                session.challenge.copy_from_slice(&fresh[..8]);
                session.guid.copy_from_slice(&fresh[8..24]);
                sessions[slot] = Some(session);
                if log {
                    let (ip, _) = tcp.peer(slot);
                    println!("smbd: connection from {}.{}.{}.{}", ip.0[0], ip.0[1], ip.0[2], ip.0[3]);
                }
            }
            serve(api, tcp, slot, request, answer, sessions[slot].as_mut().unwrap());
        }
        if log && SEND_BYTES.load(Ordering::Relaxed) > 0 {
            if busy {
                last_busy = tape_rt::micros();
            } else if tape_rt::micros() - last_busy > 500_000 {
                report();
            }
        }
        if !busy {
            tape_rt::idle();
        }
    }
}

/// Prints and resets the counters: reading, sending, and everything else since the last report.
fn report() {
    let (read_us, read_bytes) = (READ_US.swap(0, Ordering::Relaxed), READ_BYTES.swap(0, Ordering::Relaxed));
    let (send_us, send_bytes) = (SEND_US.swap(0, Ordering::Relaxed), SEND_BYTES.swap(0, Ordering::Relaxed));
    let total = tape_rt::micros() - CONNECTED_US.swap(tape_rt::micros(), Ordering::Relaxed);
    let (write_us, write_bytes) = (WRITE_US.swap(0, Ordering::Relaxed), WRITE_BYTES.swap(0, Ordering::Relaxed));
    if write_bytes > 0 {
        println!("smbd: wrote {} KB in {} ms", write_bytes / 1024, write_us / 1000);
    }
    println!("smbd: read {} KB in {} ms, sent {} KB in {} ms, {} ms since the last report.", read_bytes / 1024, read_us / 1000, send_bytes / 1024, send_us / 1000, total / 1000);
}

/// Answers every complete message waiting on `slot` (a 4-byte frame header, then the SMB message).
fn serve(api: &TapeApi, tcp: &mut Tcp<RX, SLOTS>, slot: usize, request: &mut [u8; RX], answer: &mut [u8], session: &mut Server<Files, OPENS>) {
    loop {
        let waiting = tcp.received(slot);
        if waiting.len() < 4 {
            return;
        }
        let length = (usize::from(waiting[1]) << 16) | (usize::from(waiting[2]) << 8) | usize::from(waiting[3]);
        if waiting[0] != 0 || length + 4 > RX {
            // Not SMB, or larger than the server negotiated: hang up.
            tcp.close(api, slot);
            return;
        }
        if waiting.len() < length + 4 {
            return;
        }
        let mut taken = 0;
        while taken < length + 4 {
            let got = tcp.take(api, slot, &mut request[taken..length + 4]);
            if got == 0 {
                break;
            }
            taken += got;
        }
        let sent = session.handle(&request[4..length + 4], answer);
        let start = tape_rt::micros();
        let result = if sent > 0 { tcp.send_buffered(api, slot, &answer[..sent], SEND_TIMEOUT) } else { Ok(()) };
        SEND_US.fetch_add(tape_rt::micros() - start, Ordering::Relaxed);
        SEND_BYTES.fetch_add(sent as u64, Ordering::Relaxed);
        if result.is_err() {
            return;
        }
    }
}

/// `smbd user NAME UID GID PASSWORD`: prints the line for the smbusers file.
fn make_user_line(rest: &str) {
    let mut words = rest.split_ascii_whitespace();
    let (Some(name), Some(uid), Some(gid), Some(password)) = (words.next(), words.next(), words.next(), words.next()) else {
        return println!("smbd user NAME UID GID PASSWORD");
    };
    if uid.parse::<u32>().is_err() || gid.parse::<u32>().is_err() || name.contains(':') {
        return println!("smbd: uid and gid are numbers, the name has no colon.");
    }
    let hash = nt_hash(password);
    println!("{name}:{uid}:{gid}:{}", Hex(&hash));
}

struct Hex<'a>(&'a [u8]);

impl core::fmt::Display for Hex<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}
