//! smbd: an SMB 2/3 file server (guest access, no signing, no encryption) for one folder of any volume the kernel
//! can read, ext2/3/4 included (`vfs_*`, tape ABI 32). The protocol is `crates/smb` (`server`); this tape is its TCP
//! front end, a service like `httpd` (`run smbd @SMBD.CFG &2`). Logins are checked against the `smbusers` file (NTLMv2);
//! without that file everyone is a guest, so use it on a network you trust.
#![no_std]
#![no_main]

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use smb::ntlm::{md5, nt_hash};
use smb::server::{Account, DEFAULT_IO, Fail, Identity, MAX_IO, Server, Share, Stat};
use tape_ip::net::{self, NetConfig};
use tape_ip::tcp::Server as Tcp;
use tape_net::MacAddress;
use tape_rt::config::Config;
use tape_rt::{TapeApi, arg_value as value, println};

tape_rt::tape_help!(
    br#"smbd [@NAME] [root=DIR] [share=NAME] [port=445] [nic=N] [ip=A.B.C.D/N]
      [gateway=A.B.C.D] [maxio=KIB] [log] [stats] [ro] [ipc]
An SMB file server (SMB 2.0.2 to 3.0, no signing): Linux mount.cifs/smbclient,
Windows and macOS can browse, copy from it and, on ext volumes, write to it. It runs as
a service: run smbd @SMBD.CFG &2 (the service owns NIC 2; tape run smbd takes
the foreground and Escape stops it). Settings are key=value words on the
command line or in the file (one per line, # for comments).
root=   the folder shared: disk2 (a whole ext4 disk), ext1 (the ext4 partition of a disk that also has FOSFS), disk2/music, @LABEL/site,
        a FOSFS or FAT folder, or a //NAME mount (smbmount): FOS as a
        gateway. Default: the boot volume's root.
share=  the share name clients use, //HOST/NAME (default: share). Give it
        more than once for more shares (up to 8): share=NAME uses root=,
        share=NAME=ROOT shares ROOT. Add ,ro (this share refuses changes)
        and/or ,hidden (left out of the share list; connect by name),
        ,guest (guests may use this share) or ,noguest (they may not),
        otherwise a guest line in smbusers decides.
ipc     answer IPC$ with the share list, so Explorer, Finder and smbclient -L
        show the shares (otherwise clients must name the share).
port=   TCP port (default 445).
ip=     a fixed address like 192.168.1.20/24 (gateway= for the router);
        without it DHCP. nic= is the NIC number from 1 (default: nic use).
maxio=  largest read or write in KiB, 64 to 1024 (default 1024; SMB 2.1+
        clients use it, 2.0.2 stays at 64). 64 is the low-memory mode.
log     print one line per request (slows a transfer: use stats to time one).
stats   print only the totals after a transfer: KB and ms in file reads, writes
        and sends, and the ms the transfer lasted.
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

/// Open files per connection. Connections at once: 128, or 16 or 4 when the kernel has less memory; when all are taken a new
/// connection evicts the quietest idle one.
const OPENS: usize = 16;
/// Largest read or write offered with `maxio=` unset (SMB 2.1+ clients; the multi-credit requests of `smb::server::MAX_IO`),
/// and each connection's TCP receive buffer for it: two requests, so the client keeps sending while one is on the disk.
/// With `maxio=64` (or when the kernel has too little memory) the 64 KiB tier runs: three requests in flight, not much
/// more than the NIC's receive ring (128 frames), or a stall drops frames.
const BIG_RX: usize = 2 * MAX_IO;
const SMALL_RX: usize = 3 * DEFAULT_IO;
/// Room around a message's payload: the SMB and WRITE headers.
const SLACK: usize = 4096;
const ROOT_MAX: usize = 200;
const SEND_TIMEOUT: u64 = 5_000_000;
const IDLE_US: u64 = 600_000_000;

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

/// One share: the name clients use, the folder behind it and whether it refuses changes.
#[derive(Clone, Copy)]
struct Entry {
    root: [u8; ROOT_MAX],
    root_length: usize,
    name: [u8; 32],
    name_length: usize,
    read_only: bool,
    /// Not in the share list of `IPC$`; clients can still connect to it by name.
    hidden: bool,
    /// `,guest` (Some(true)) or `,noguest` (Some(false)); None follows the guest line of smbusers.
    guest: Option<bool>,
}

/// Shares one smbd serves (the tree ids of a connection; `smb::server` allows 16).
const SHARES: usize = 8;
static mut SHARE_TABLE: ([Entry; SHARES], usize) = ([Entry { root: [0; ROOT_MAX], root_length: 0, name: [0; 32], name_length: 0, read_only: false, hidden: false, guest: None }; SHARES], 0);
/// `ipc` was given: `IPC$` answers with the share list.
static IPC_ON: AtomicBool = AtomicBool::new(false);

fn shares() -> &'static mut ([Entry; SHARES], usize) {
    unsafe { &mut *core::ptr::addr_of_mut!(SHARE_TABLE) }
}

struct Files {
    /// The share the current request is for (`Share::select`).
    current: usize,
    /// Where this session's last read ended (hash of the file's path, offset): the next read there is sequential.
    next_hash: u64,
    next_offset: u64,
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
        Files { current: 0, next_hash: 0, next_offset: 0 }
    }

    /// `root` and `path` joined with a slash, into `out`.
    fn join<'a>(&self, path: &str, out: &'a mut [u8; 512]) -> Option<&'a str> {
        let entry = &shares().0[self.current];
        let root = &entry.root[..entry.root_length];
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
        let entry = &shares().0[self.current];
        core::str::from_utf8(&entry.name[..entry.name_length]).unwrap_or("share")
    }

    fn find(&mut self, name: &str) -> Option<usize> {
        let table = shares();
        table.0[..table.1].iter().position(|entry| entry.name[..entry.name_length].eq_ignore_ascii_case(name.as_bytes()))
    }

    fn listed(&self, index: usize) -> Option<(&str, bool)> {
        let table = shares();
        let entry = table.0[..table.1].get(index)?;
        Some((core::str::from_utf8(&entry.name[..entry.name_length]).unwrap_or(""), entry.hidden))
    }

    fn ipc(&self) -> bool {
        IPC_ON.load(Ordering::Relaxed)
    }

    fn select(&mut self, index: usize) {
        self.current = index.min(shares().1.saturating_sub(1));
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
        if !table.open {
            return Some(Identity { uid: 65534, gid: 65534 });
        }
        // a share marked ,guest lets guests in even without a guest line (then as nobody)
        let marked = shares().0[..shares().1].iter().any(|entry| entry.guest == Some(true));
        table.guest.or(marked.then_some(Identity { uid: 65534, gid: 65534 }))
    }

    fn guest_allowed(&self, index: usize) -> bool {
        let table = users();
        !table.open || shares().0[..shares().1].get(index).is_some_and(|entry| entry.guest.unwrap_or(table.guest.is_some()))
    }

    fn read(&mut self, path: &str, offset: u64, out: &mut [u8]) -> Result<usize, ()> {
        let mut buffer = [0u8; 512];
        let reference = self.join(path, &mut buffer).ok_or(())?;
        let hash = fnv(reference.as_bytes());
        let sequential = self.next_hash == hash && self.next_offset == offset;
        let result = if ahead_take(reference, offset, out) {
            Ok(out.len())
        } else {
            let start = tape_rt::micros();
            let result = tape_rt::vfs_read(reference, offset, out).map_err(|_| ());
            READ_US.fetch_add(tape_rt::micros() - start, Ordering::Relaxed);
            result
        };
        if let Ok(count) = result {
            READ_BYTES.fetch_add(count as u64, Ordering::Relaxed);
            self.next_hash = hash;
            self.next_offset = offset + count as u64;
            if sequential && count == out.len() {
                ahead_hint(reference, offset + count as u64, out.len());
            }
        }
        result
    }

    fn writable(&self) -> bool {
        !shares().0[self.current].read_only && tape_rt::vfs_can_change()
    }

    fn create(&mut self, path: &str, dir: bool, mode: u32, owner: Identity) -> Result<(), Fail> {
        ahead_invalidate();
        let mut buffer = [0u8; 512];
        let reference = self.join(path, &mut buffer).ok_or(Fail::Io)?;
        let kind = if dir { tape_rt::VFS_DIR } else { tape_rt::VFS_FILE };
        tape_rt::vfs_create(reference, kind, mode, owner.uid, owner.gid).map_err(fail)
    }

    fn write(&mut self, path: &str, offset: u64, data: &[u8]) -> Result<(), Fail> {
        ahead_invalidate();
        let mut buffer = [0u8; 512];
        let reference = self.join(path, &mut buffer).ok_or(Fail::Io)?;
        let mut done = 0;
        let start = tape_rt::micros();
        let _timing = Timed(start);
        WRITE_BYTES.fetch_add(data.len() as u64, Ordering::Relaxed);
        WRITE_COUNT.fetch_add(1, Ordering::Relaxed);
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
        ahead_invalidate();
        let mut buffer = [0u8; 512];
        let reference = self.join(path, &mut buffer).ok_or(Fail::Io)?;
        tape_rt::vfs_truncate(reference, size).map_err(fail)
    }

    fn remove(&mut self, path: &str) -> Result<(), Fail> {
        ahead_invalidate();
        let mut buffer = [0u8; 512];
        let reference = self.join(path, &mut buffer).ok_or(Fail::Io)?;
        tape_rt::vfs_remove(reference).map_err(fail)
    }

    fn rename(&mut self, from: &str, to: &str) -> Result<(), Fail> {
        ahead_invalidate();
        let (mut first, mut second) = ([0u8; 512], [0u8; 512]);
        let from = self.join(from, &mut first).ok_or(Fail::Io)?;
        let to = self.join(to, &mut second).ok_or(Fail::Io)?;
        tape_rt::vfs_rename(from, to).map_err(fail)
    }
}

/// Where the time goes (shown with `log` when a connection ends): microseconds and bytes spent in file reads and in sending answers.
static WRITE_US: AtomicU64 = AtomicU64::new(0);
static WRITE_BYTES: AtomicU64 = AtomicU64::new(0);
static WRITE_COUNT: AtomicU64 = AtomicU64::new(0);
/// When the last write ended, and 0 once the kernel's held-open writes were committed (`vfs_sync`).
static UNSYNCED_AT: AtomicU64 = AtomicU64::new(0);
/// How long the writes must pause before they are committed: a stream shares one journal commit.
const SYNC_QUIET_US: u64 = 20_000;
/// How long the main loop keeps polling after the last frame before it sleeps.
const SPIN_US: u64 = 2_000;

/// Adds the time since its start to `WRITE_US` when dropped, whichever way `write` returns.
struct Timed(u64);

impl Drop for Timed {
    fn drop(&mut self) {
        let now = tape_rt::micros();
        WRITE_US.fetch_add(now - self.0, Ordering::Relaxed);
        UNSYNCED_AT.store(now, Ordering::Relaxed);
    }
}
static READ_US: AtomicU64 = AtomicU64::new(0);
static READ_BYTES: AtomicU64 = AtomicU64::new(0);
static SEND_US: AtomicU64 = AtomicU64::new(0);
static SEND_BYTES: AtomicU64 = AtomicU64::new(0);
static CONNECTED_US: AtomicU64 = AtomicU64::new(0);


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
    let read_only = text.split_ascii_whitespace().any(|word| word == "ro");
    // share=NAME uses root=; share=NAME=ROOT is one more folder under its own name; either takes ,ro and ,hidden after it.
    let table = shares();
    for word in text.split_ascii_whitespace().filter_map(|word| word.strip_prefix("share=")) {
        let (spec, options) = word.split_once(',').unwrap_or((word, ""));
        let (name, folder) = spec.split_once('=').unwrap_or((spec, root));
        let folder = folder.trim_end_matches('/');
        let (mut entry_only, mut hidden, mut guest) = (false, false, None);
        for option in options.split(',').filter(|option| !option.is_empty()) {
            match option {
                "ro" => entry_only = true,
                "hidden" => hidden = true,
                "guest" => guest = Some(true),
                "noguest" => guest = Some(false),
                _ => return println!("smbd: share={word}: ,{option} is not ,ro, ,hidden, ,guest or ,noguest."),
            }
        }
        if name.is_empty() || name.len() > 32 || name.contains(['/', '\\']) || folder.len() > ROOT_MAX {
            return println!("smbd: share={word}: a name of 1 to 32 characters without slashes and a root up to {ROOT_MAX}.");
        }
        if table.1 == SHARES {
            return println!("smbd: at most {SHARES} shares.");
        }
        if table.0[..table.1].iter().any(|entry| entry.name[..entry.name_length].eq_ignore_ascii_case(name.as_bytes())) {
            return println!("smbd: share {name} is given twice.");
        }
        let entry = &mut table.0[table.1];
        entry.name[..name.len()].copy_from_slice(name.as_bytes());
        entry.name_length = name.len();
        entry.root[..folder.len()].copy_from_slice(folder.as_bytes());
        entry.root_length = folder.len();
        entry.read_only = read_only || entry_only;
        entry.hidden = hidden;
        entry.guest = guest;
        table.1 += 1;
    }
    if table.1 == 0 {
        if root.len() > ROOT_MAX {
            return println!("smbd: root= is too long.");
        }
        let entry = &mut table.0[0];
        entry.name[..5].copy_from_slice(b"share");
        entry.name_length = 5;
        entry.root[..root.len()].copy_from_slice(root.as_bytes());
        entry.root_length = root.len();
        entry.read_only = read_only;
        table.1 = 1;
    }
    for entry in &table.0[..table.1] {
        let folder = core::str::from_utf8(&entry.root[..entry.root_length]).unwrap_or("");
        if tape_rt::vfs_stat(folder).map_or(true, |stat| stat.kind != tape_rt::VFS_DIR) {
            return println!("smbd: {} is not a folder this kernel can read (needs tape ABI 32).", if folder.is_empty() { "the boot volume" } else { folder });
        }
    }
    if users().open {
        println!("smbd: {} user(s) from smbusers{}.", users().count, if users().guest.is_some() { ", guests allowed" } else { ", no guests" });
    } else {
        println!("smbd: no smbusers file: everyone is a guest.");
    }
    IPC_ON.store(text.split_ascii_whitespace().any(|word| word == "ipc"), Ordering::Relaxed);
    let log = text.split_ascii_whitespace().any(|word| word == "log" || word == "log=on");
    let stats = log || text.split_ascii_whitespace().any(|word| word == "stats");
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
    let io_kib = match value(text, "maxio") {
        None => MAX_IO / 1024,
        Some(word) => match word.parse::<usize>() {
            Ok(kib) if (DEFAULT_IO / 1024..=MAX_IO / 1024).contains(&kib) => kib,
            _ => return println!("smbd: maxio= takes KiB from {} to {}.", DEFAULT_IO / 1024, MAX_IO / 1024),
        },
    };
    let big = io_kib > DEFAULT_IO / 1024;
    // Each connection holds two receive buffers, so a kernel short of memory gets fewer connections, then the small tier.
    let started = if big {
        let setup = Setup { nic, network, port, log, stats, io: io_kib * 1024 };
        start::<BIG_RX, 32>(api, &setup) || start::<BIG_RX, 8>(api, &setup)
    } else {
        false
    };
    let setup = Setup { nic, network, port, log, stats, io: DEFAULT_IO };
    if !(started || start::<SMALL_RX, 128>(api, &setup) || start::<SMALL_RX, 16>(api, &setup) || start::<SMALL_RX, 4>(api, &setup)) {
        println!("smbd: no memory for the connections.");
    }
}

/// Runs the server with room for `N` connections if the kernel can give the memory; false if it cannot.
fn start<const RX: usize, const N: usize>(api: &TapeApi, setup: &Setup) -> bool {
    let Some(tcp) = alloc_in_kernel::<Tcp<RX, N>>(api) else { return false };
    let Some(sessions) = many_sessions::<N>(api) else { return false };
    let Some(request) = buffer(api, setup.io + SLACK) else { return false };
    let Some(answer) = buffer(api, setup.io + 256) else { return false };
    println!("smbd: room for {N} connections, reads and writes up to {} KiB.", setup.io / 1024);
    run(api, setup, tcp, request, answer, sessions);
    true
}

struct Setup {
    nic: usize,
    network: NetConfig,
    port: u16,
    log: bool,
    stats: bool,
    /// Largest read or write offered to the client.
    io: usize,
}

/// `bytes` of zeroed kernel memory for a message.
fn buffer(api: &TapeApi, bytes: usize) -> Option<&'static mut [u8]> {
    let address = (api.alloc)(bytes);
    if address == 0 {
        return None;
    }
    unsafe {
        core::ptr::write_bytes(address as *mut u8, 0, bytes);
        Some(core::slice::from_raw_parts_mut(address as *mut u8, bytes))
    }
}

/// A zeroed `T` from the kernel; all-zero is a valid `Tcp` (every slot is made a listener by `start`).
fn alloc_in_kernel<T>(api: &TapeApi) -> Option<&'static mut T> {
    tape_rt::alloc_zeroed::<T>(api)
}

/// Room for `N` sessions from the kernel, each set to `None`.
fn many_sessions<const N: usize>(api: &TapeApi) -> Option<&'static mut [Option<Server<Files, OPENS>>; N]> {
    let bytes = N * core::mem::size_of::<Option<Server<Files, OPENS>>>();
    let address = (api.alloc)(bytes);
    if address == 0 {
        return None;
    }
    let first = address as *mut Option<Server<Files, OPENS>>;
    unsafe {
        for index in 0..N {
            first.add(index).write(None);
        }
        Some(&mut *(address as *mut [Option<Server<Files, OPENS>>; N]))
    }
}

fn run<const RX: usize, const N: usize>(api: &TapeApi, setup: &Setup, tcp: &mut Tcp<RX, N>, request: &mut [u8], answer: &mut [u8], sessions: &mut [Option<Server<Files, OPENS>>; N]) {
    let Setup { nic, network, port, log, stats, io } = *setup;
    let (mut pool, mut state) = (0u64, [0u8; 16]);
    let (pool, state) = (&mut pool, &mut state);
    tcp.start(nic, &network, port);
    // A mount idles between requests (the Linux client's echo is 60 s apart); do not drop it behind its back.
    tcp.set_idle(IDLE_US);
    CONNECTED_US.store(tape_rt::micros(), Ordering::Relaxed);
    let (mut last_busy, mut first_busy) = (0u64, 0u64);
    let mut last_frame = 0u64;
    let mut workers = [0usize; WORKERS];
    let worker_count = start_workers(api, io, &mut workers);
    // The worker (if any) holding the request of each slot.
    let mut assigned = [usize::MAX; N];
    if worker_count > 0 {
        println!("smbd: {worker_count} worker threads.");
    }
    println!("smbd: NIC {} at {}.{}.{}.{}:{port}.", nic + 1, network.ip.0[0], network.ip.0[1], network.ip.0[2], network.ip.0[3]);
    for entry in &shares().0[..shares().1] {
        let (name, folder) = (core::str::from_utf8(&entry.name[..entry.name_length]).unwrap_or(""), core::str::from_utf8(&entry.root[..entry.root_length]).unwrap_or(""));
        println!("smbd: sharing {} as //HOST/{name}{}.", if folder.is_empty() { "the boot volume" } else { folder }, if entry.read_only { " (read-only)" } else { "" });
    }
    if IPC_ON.load(Ordering::Relaxed) {
        println!("smbd: IPC$ lists the shares to clients (hidden ones left out).");
    }
    loop {
        if (api.should_abort)() != 0 {
            break;
        }
        let mut busy = tcp.poll(api, 16);
        for slot in 0..N {
            if assigned[slot] != usize::MAX {
                let worker = unsafe { &*(workers[assigned[slot]] as *const Worker) };
                if worker.state.load(Ordering::Acquire) != DONE {
                    // The worker has the session; the network thread must not poll sleep until it answers.
                    busy = true;
                    continue;
                }
                let worker = unsafe { &mut *(workers[assigned[slot]] as *mut Worker) };
                let total = worker.length.load(Ordering::Relaxed);
                let mut finished = !tcp.is_open(slot);
                if !finished {
                    // The answer goes out as the window opens; the loop keeps serving the other connections meanwhile.
                    let answer = unsafe { core::slice::from_raw_parts(worker.answer as *const u8, total) };
                    let (start, now) = (tape_rt::micros(), tape_rt::micros());
                    if worker.progress == 0 {
                        worker.progress = now;
                    }
                    match tcp.send_some(api, slot, &answer[worker.sent..]) {
                        Ok(taken) => {
                            SEND_BYTES.fetch_add(taken as u64, Ordering::Relaxed);
                            worker.sent += taken;
                            if taken > 0 {
                                worker.progress = now;
                            }
                            finished = worker.sent == total;
                            if !finished && now - worker.progress > SEND_TIMEOUT {
                                println!("smbd: sending failed: the peer did not acknowledge");
                                finished = true;
                            }
                        }
                        Err(reason) => {
                            println!("smbd: sending failed: {reason}");
                            finished = true;
                        }
                    }
                    SEND_US.fetch_add(tape_rt::micros() - start, Ordering::Relaxed);
                }
                if !finished {
                    busy = true;
                    continue;
                }
                worker.state.store(IDLE, Ordering::Release);
                assigned[slot] = usize::MAX;
                busy = true;
            }
            if !tcp.is_open(slot) {
                sessions[slot] = None;
                continue;
            }
            if sessions[slot].is_none() {
                let files = Files::new();
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
                if log {
                    session.trace = Some(trace_request);
                }
                session.guid.copy_from_slice(&fresh[8..24]);
                session.max_io = io;
                sessions[slot] = Some(session);
                if log {
                    let (ip, _) = tcp.peer(slot);
                    println!("smbd: connection from {}.{}.{}.{}", ip.0[0], ip.0[1], ip.0[2], ip.0[3]);
                }
            }
            if worker_count == 0 {
                serve(api, tcp, slot, request, answer, sessions[slot].as_mut().unwrap());
            } else if let Some(length) = peek_message(api, tcp, slot, io + SLACK) {
                let free = (0..worker_count).find(|&index| unsafe { &*(workers[index] as *const Worker) }.state.load(Ordering::Acquire) == IDLE);
                if let Some(index) = free {
                    let worker = unsafe { &mut *(workers[index] as *mut Worker) };
                    let into = unsafe { core::slice::from_raw_parts_mut(worker.request as *mut u8, io + SLACK) };
                    take_message(api, tcp, slot, length, into);
                    worker.length.store(length, Ordering::Relaxed);
                    worker.sent = 0;
                    worker.progress = 0;
                    worker.session.store(sessions[slot].as_mut().unwrap() as *mut Server<Files, OPENS> as usize, Ordering::Relaxed);
                    worker.state.store(JOB, Ordering::Release);
                    tape_rt::futex_wake(&worker.state, 1);
                    assigned[slot] = index;
                    busy = true;
                }
            }
            // A client that hung up leaves the slot in CloseWait; without our FIN it stays taken for the idle timeout,
            // and a few quick connections in a row would fill all of them.
            if assigned[slot] == usize::MAX && tcp.peer_closed(slot) && tcp.received(slot).is_empty() {
                tcp.finish(api, slot);
            }
        }
        if stats && SEND_BYTES.load(Ordering::Relaxed) > 0 {
            if busy {
                last_busy = tape_rt::micros();
                if first_busy == 0 {
                    first_busy = last_busy;
                }
            } else if tape_rt::micros() - last_busy > 500_000 {
                report(tcp.tx_dropped(), last_busy - first_busy);
                first_busy = 0;
            }
        }
        let unsynced = UNSYNCED_AT.load(Ordering::Relaxed);
        if unsynced != 0 && tape_rt::micros() - unsynced > SYNC_QUIET_US {
            let start = tape_rt::micros();
            UNSYNCED_AT.store(0, Ordering::Relaxed);
            let _ = tape_rt::vfs_sync();
            WRITE_US.fetch_add(tape_rt::micros() - start, Ordering::Relaxed);
        }
        // `idle` sleeps a whole timer tick on mf, and a transfer's frames arrive a few hundred microseconds apart once the disk keeps
        // up: sleeping at every gap makes each 64 KiB request cost milliseconds. Spin while traffic is recent, sleep when it stops.
        let now = tape_rt::micros();
        if busy {
            last_frame = now;
        } else if now - last_frame > SPIN_US {
            tape_rt::idle();
        }
    }
}

/// `log` shows every request: the command number (2 = session setup, 3 = tree connect, 5 = create, 16 = query info), its
/// status and a detail (QUERY_INFO: info type and class; IOCTL: the control code).
fn trace_request(command: u16, status: u32, detail: u32, flags: u32) {
    println!("smbd: cmd {command} -> {status:#x} ({detail:#x}) flags {flags:#x}");
}

/// Prints and resets the counters: reading, sending, and everything else since the last report.
fn report(dropped: u32, busy_us: u64) {
    let (read_us, read_bytes) = (READ_US.swap(0, Ordering::Relaxed), READ_BYTES.swap(0, Ordering::Relaxed));
    let (send_us, send_bytes) = (SEND_US.swap(0, Ordering::Relaxed), SEND_BYTES.swap(0, Ordering::Relaxed));
    let total = tape_rt::micros() - CONNECTED_US.swap(tape_rt::micros(), Ordering::Relaxed);
    let (write_us, write_bytes) = (WRITE_US.swap(0, Ordering::Relaxed), WRITE_BYTES.swap(0, Ordering::Relaxed));
    if write_bytes > 0 {
        let requests = WRITE_COUNT.swap(0, Ordering::Relaxed).max(1);
        println!("smbd: wrote {} KB in {} ms ({} requests, {} KiB each on average)", write_bytes / 1024, write_us / 1000, requests, write_bytes / requests / 1024);
    }
    if dropped > 0 {
        println!("smbd: the card refused {dropped} frames");
    }
    println!("smbd: read {} KB in {} ms, sent {} KB in {} ms, active for {} ms ({} ms since the last report).", read_bytes / 1024, read_us / 1000, send_bytes / 1024, send_us / 1000, busy_us / 1000, total / 1000);
}

/// The length of the SMB message at the front of `slot`'s stream (without its 4-byte frame header), once all of it has arrived.
/// Hangs up on a frame that is not SMB or is larger than the server negotiated.
fn peek_message<const RX: usize, const N: usize>(api: &TapeApi, tcp: &mut Tcp<RX, N>, slot: usize, limit: usize) -> Option<usize> {
    let waiting = tcp.received(slot);
    if waiting.len() < 4 {
        return None;
    }
    let length = (usize::from(waiting[1]) << 16) | (usize::from(waiting[2]) << 8) | usize::from(waiting[3]);
    if waiting[0] != 0 || length + 4 > limit {
        tcp.close(api, slot);
        return None;
    }
    (waiting.len() >= length + 4).then_some(length)
}

/// Moves the `length + 4` bytes of the message peeked at into `into`.
fn take_message<const RX: usize, const N: usize>(api: &TapeApi, tcp: &mut Tcp<RX, N>, slot: usize, length: usize, into: &mut [u8]) {
    let mut taken = 0;
    while taken < length + 4 {
        let got = tcp.take(api, slot, &mut into[taken..length + 4]);
        if got == 0 {
            break;
        }
        taken += got;
    }
}

/// Sends an answer of `sent` bytes (none if 0) and counts the time; false if the send failed.
fn reply<const RX: usize, const N: usize>(api: &TapeApi, tcp: &mut Tcp<RX, N>, slot: usize, answer: &[u8], sent: usize) -> bool {
    let start = tape_rt::micros();
    let result = if sent > 0 { tcp.send_buffered(api, slot, &answer[..sent], SEND_TIMEOUT) } else { Ok(()) };
    SEND_US.fetch_add(tape_rt::micros() - start, Ordering::Relaxed);
    SEND_BYTES.fetch_add(sent as u64, Ordering::Relaxed);
    if let Err(reason) = result {
        println!("smbd: sending failed: {reason}");
        return false;
    }
    true
}

/// Answers every complete message waiting on `slot` (a 4-byte frame header, then the SMB message); the single-threaded path.
fn serve<const RX: usize, const N: usize>(api: &TapeApi, tcp: &mut Tcp<RX, N>, slot: usize, request: &mut [u8], answer: &mut [u8], session: &mut Server<Files, OPENS>) {
    while let Some(length) = peek_message(api, tcp, slot, request.len()) {
        take_message(api, tcp, slot, length, request);
        let sent = session.handle(&request[4..length + 4], answer);
        if !reply(api, tcp, slot, answer, sent) {
            return;
        }
    }
}

fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |hash, &byte| (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3))
}

/// Read-ahead for a sequential read (server kernel, with the workers): a client that reads a file in 1 MiB requests waits for
/// each answer before asking for the next, so the disk read of one request used to sit between two stretches of wire time. After
/// the second read in a row a worker reads the next range into `AHEAD` while the network thread is still sending the answer,
/// and the next request is a memory copy. One cache for all sessions, taken only for a request that it covers entirely; any
/// change to a file (`ahead_invalidate`, counted in `generation`) throws it away, also a read that was in progress.
struct Ahead {
    data: usize,
    room: usize,
    valid: bool,
    key: [u8; 512],
    key_length: usize,
    offset: u64,
    length: usize,
    wanted: bool,
    want_key: [u8; 512],
    want_key_length: usize,
    want_offset: u64,
    want_length: usize,
    generation: u64,
}

static AHEAD_LOCK: AtomicBool = AtomicBool::new(false);
static mut AHEAD: Ahead = Ahead { data: 0, room: 0, valid: false, key: [0; 512], key_length: 0, offset: 0, length: 0, wanted: false, want_key: [0; 512], want_key_length: 0, want_offset: 0, want_length: 0, generation: 0 };

/// Runs `body` on the cache with its spin lock held (it is held for a memory copy at most).
fn with_ahead<T>(body: impl FnOnce(&mut Ahead) -> T) -> T {
    while AHEAD_LOCK.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
        core::hint::spin_loop();
    }
    let result = body(unsafe { &mut *core::ptr::addr_of_mut!(AHEAD) });
    AHEAD_LOCK.store(false, Ordering::Release);
    result
}

/// Fills `out` from the cache if it covers `offset..offset + out.len()` of `key`.
fn ahead_take(key: &str, offset: u64, out: &mut [u8]) -> bool {
    with_ahead(|ahead| {
        let covers = ahead.valid && &ahead.key[..ahead.key_length] == key.as_bytes() && offset >= ahead.offset && offset + out.len() as u64 <= ahead.offset + ahead.length as u64;
        if covers {
            let from = (offset - ahead.offset) as usize;
            unsafe { core::ptr::copy_nonoverlapping((ahead.data + from) as *const u8, out.as_mut_ptr(), out.len()) };
        }
        covers
    })
}

/// Asks for `length` bytes of `key` from `offset` to be read ahead by the next worker that has a moment.
fn ahead_hint(key: &str, offset: u64, length: usize) {
    with_ahead(|ahead| {
        if ahead.data != 0 && key.len() <= ahead.want_key.len() && length <= ahead.room {
            ahead.want_key[..key.len()].copy_from_slice(key.as_bytes());
            ahead.want_key_length = key.len();
            ahead.want_offset = offset;
            ahead.want_length = length;
            ahead.wanted = true;
        }
    });
}

fn ahead_invalidate() {
    with_ahead(|ahead| {
        ahead.valid = false;
        ahead.wanted = false;
        ahead.generation += 1;
    });
}

/// A worker's spare moment: does the read-ahead somebody asked for, into its own `scratch` and then into the cache.
fn ahead_fill(scratch: &mut [u8]) {
    let wanted = with_ahead(|ahead| {
        if !ahead.wanted {
            return None;
        }
        ahead.wanted = false;
        Some((ahead.want_key, ahead.want_key_length, ahead.want_offset, ahead.want_length, ahead.generation))
    });
    let Some((key, key_length, offset, length, generation)) = wanted else { return };
    let Ok(name) = core::str::from_utf8(&key[..key_length]) else { return };
    let room = length.min(scratch.len());
    let Ok(count) = tape_rt::vfs_read(name, offset, &mut scratch[..room]) else { return };
    with_ahead(|ahead| {
        if ahead.generation == generation && count <= ahead.room {
            unsafe { core::ptr::copy_nonoverlapping(scratch.as_ptr(), ahead.data as *mut u8, count) };
            ahead.key = key;
            ahead.key_length = key_length;
            ahead.offset = offset;
            ahead.length = count;
            ahead.valid = true;
        }
    });
}

/// Worker threads (tape ABI 37, server kernel): the network thread owns the NIC and every `Tcp` slot; a worker only runs
/// `Server::handle` (and with it the file I/O) for one request at a time, so a slow disk does not stop the frames coming in
/// and sessions do not wait for each other. A slot has one request in flight, so a session's requests stay in order and
/// only one thread ever touches its `Server`.
const WORKERS: usize = 4;
const IDLE: u32 = 0;
const JOB: u32 = 1;
const DONE: u32 = 2;

/// Shared between the network thread and one worker; all-zero is valid. `state` hands the buffers over: IDLE -> JOB
/// (network thread, request and session filled in) -> DONE (worker, answer and `length` filled in) -> IDLE (network
/// thread, answer sent). The Release store and Acquire load on `state` order the plain fields around it.
struct Worker {
    state: AtomicU32,
    /// The request's SMB length going in, the answer's length coming out.
    length: AtomicUsize,
    session: AtomicUsize,
    request: usize,
    answer: usize,
    answer_room: usize,
    /// The worker's own buffer for read-ahead.
    scratch: usize,
    scratch_room: usize,
    /// Network thread only: how much of the answer is out, and when the peer last took some (0: not started).
    sent: usize,
    progress: u64,
}

extern "C" fn worker_main(address: usize) {
    let worker = unsafe { &*(address as *const Worker) };
    loop {
        let state = worker.state.load(Ordering::Acquire);
        if state != JOB {
            tape_rt::futex_wait(&worker.state, state);
            continue;
        }
        let length = worker.length.load(Ordering::Relaxed);
        let session = unsafe { &mut *(worker.session.load(Ordering::Relaxed) as *mut Server<Files, OPENS>) };
        let request = unsafe { core::slice::from_raw_parts((worker.request + 4) as *const u8, length) };
        let answer = unsafe { core::slice::from_raw_parts_mut(worker.answer as *mut u8, worker.answer_room) };
        let sent = session.handle(request, answer);
        worker.length.store(sent, Ordering::Relaxed);
        worker.state.store(DONE, Ordering::Release);
        // The network thread is sending the answer now; this is the moment for the disk to read what comes next.
        ahead_fill(unsafe { core::slice::from_raw_parts_mut(worker.scratch as *mut u8, worker.scratch_room) });
    }
}

/// Starts up to `WORKERS` workers, each with its own request and answer buffer; returns how many started.
fn start_workers(api: &TapeApi, io: usize, workers: &mut [usize; WORKERS]) -> usize {
    if !tape_rt::threads_supported() {
        return 0;
    }
    if let Some(cache) = buffer(api, io) {
        with_ahead(|ahead| {
            ahead.data = cache.as_ptr() as usize;
            ahead.room = cache.len();
        });
    }
    let mut count = 0;
    while count < WORKERS {
        let Some(worker) = tape_rt::alloc_zeroed::<Worker>(api) else { break };
        let (Some(request), Some(answer)) = (buffer(api, io + SLACK), buffer(api, io + 256)) else { break };
        worker.request = request.as_ptr() as usize;
        worker.answer = answer.as_ptr() as usize;
        worker.answer_room = answer.len();
        let Some(scratch) = buffer(api, io) else { break };
        worker.scratch = scratch.as_ptr() as usize;
        worker.scratch_room = scratch.len();
        let address = worker as *mut Worker as usize;
        match tape_rt::spawn_thread(worker_main, address, 64 * 1024) {
            Ok(thread) => core::mem::forget(thread),
            Err(_) => break,
        }
        workers[count] = address;
        count += 1;
    }
    count
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
