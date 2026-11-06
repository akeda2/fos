//! sshmount: mounts a folder of another machine, over SFTP, as the volume `//NAME`, as a service
//! (`run sshmount USER@HOST as=NAME &N`).
//!
//! Like `smbmount` this tape owns one NIC and one connection and answers the file calls the kernel queues for `//NAME`
//! (tape ABI 35, `remote_poll` / `remote_reply`). The login is an ed25519 key: the seed is `settings/ssh_key` (`sshmount keygen`
//! makes it and prints the line for the server's `authorized_keys`), and the server's key is checked against
//! `settings/known_hosts`, trust on first use.
#![no_std]
#![no_main]

use ssh::authorized::{fingerprint_public, parse_seed, public_line, seed_text};
use ssh::client::{
    Attrs, CLIENT_ARENA, Client, Error, Handle, Link, OPEN_CREATE, OPEN_EXCLUSIVE, OPEN_READ, OPEN_WRITE, STATUS_NO_SUCH_FILE,
    STATUS_PERMISSION_DENIED,
};
use tape_ip::net;
use tape_ip::tcp::Socket;
use tape_net::{ArpCache, MacAddress};
use tape_rt::config::{read_named, write_named};
use tape_rt::*;

tape_rt::tape_help!(
    br#"run sshmount USER@HOST[:PORT] [as=NAME] [path=/DIR] [key=NAME] &N
sshmount keygen [key=NAME]
Mounts a folder of another machine over SFTP as the volume //NAME (default: the
host's first name label) until the service is stopped (kill). Then
  ls //NAME/folder    cat //NAME/file    copy //NAME/file disk1/file
and every tape's vfs_* calls work on it, writes too where the server allows
them. path= is the folder to show (default: the login folder). The service
owns NIC N and takes its address from DHCP.
Login is by key only: `sshmount keygen` makes settings/ssh_key (key=NAME: another
file) and prints its line; put that line in ~/.ssh/authorized_keys on the server.
The server's key is remembered in settings/known_hosts the first time and
checked afterwards: a changed key is refused (edit the file to accept it).
Folders list their first 128 entries. OpenSSH, fos's sshd, any SFTP v3 server.
"#
);

const TIMEOUT_MICROS: u64 = 20_000_000;
/// What one request carries: `remote::MAX_DATA` in the kernel plus room for the two paths.
const BUFFER: usize = 128 * 1024 + 4096;
const LISTED: usize = 128;
const PATH: usize = 600;
/// A cached open file is closed after this long without a request.
const IDLE_CLOSE_MICROS: u64 = 300_000;
const MODE_FILE: u32 = 0o644;
const MODE_DIR: u32 = 0o755;
const STATUS_NO_CONNECTION: u32 = 6;
const STATUS_CONNECTION_LOST: u32 = 7;

struct Wire {
    socket: Socket<16384>,
    api: &'static TapeApi,
}

impl Link for Wire {
    fn send(&mut self, data: &[u8]) -> Result<(), Error> {
        self.socket.send(self.api, data, TIMEOUT_MICROS).map_err(|_| Error::Link)
    }

    fn recv(&mut self, out: &mut [u8]) -> Result<usize, Error> {
        self.socket.recv(self.api, out, TIMEOUT_MICROS).map_err(|_| Error::Link)
    }

    fn random(&mut self, out: &mut [u8]) {
        if tape_rt::random(out) != Some(true) {
            for chunk in out.chunks_mut(8) {
                let value = tape_rt::micros().to_le_bytes();
                chunk.copy_from_slice(&value[..chunk.len()]);
            }
        }
    }
}

type Session = Client<'static, Wire>;

static mut ARENA: [u8; CLIENT_ARENA] = [0; CLIENT_ARENA];
static mut BUF: [u8; BUFFER] = [0; BUFFER];
static mut LIST: Listing = Listing::new();
static mut OPEN: Option<Open> = None;
static mut SOCKET: Socket<16384> = Socket::new();

struct Item {
    name: [u8; 255],
    length: usize,
    attrs: Attrs,
}

/// The entries of the folder last listed, so a walk by index costs one directory read, not one per entry.
struct Listing {
    path: [u8; PATH],
    path_length: usize,
    valid: bool,
    count: usize,
    items: [Item; LISTED],
}

impl Listing {
    const fn new() -> Self {
        const EMPTY: Item = Item { name: [0; 255], length: 0, attrs: Attrs { size: None, permissions: None, uid: None, gid: None, mtime: None } };
        Listing { path: [0; PATH], path_length: 0, valid: false, count: 0, items: [EMPTY; LISTED] }
    }
}

/// The one file kept open between requests (a copy reads or writes the same file in many pieces).
struct Open {
    path: [u8; PATH],
    path_length: usize,
    handle: Handle,
    writable: bool,
}

struct Mount {
    dead: bool,
    /// The remote folder shown, without a trailing slash ("" is the root).
    base: [u8; PATH],
    base_length: usize,
}

impl Mount {
    /// The remote path of `path` (relative to the mount) into `out`.
    fn remote<'a>(&self, path: &str, out: &'a mut [u8; PATH]) -> Option<&'a str> {
        let base = &self.base[..self.base_length];
        let path = path.trim_matches('/');
        let length = base.len() + if path.is_empty() { 0 } else { 1 + path.len() };
        if length > PATH {
            return None;
        }
        out[..base.len()].copy_from_slice(base);
        let mut at = base.len();
        if !path.is_empty() {
            out[at] = b'/';
            out[at + 1..length].copy_from_slice(path.as_bytes());
            at = length;
        }
        if at == 0 {
            out[0] = b'/';
            at = 1;
        }
        core::str::from_utf8(&out[..at]).ok()
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(api: &'static TapeApi) {
    tape_rt::init(api);
    let arguments = core::str::from_utf8(tape_rt::args()).unwrap_or("");
    let (mut target, mut name, mut folder, mut key_file, mut keygen) = (None, None, ".", "ssh_key", false);
    for word in arguments.split_whitespace() {
        if let Some(value) = word.strip_prefix("as=") {
            name = Some(value);
        } else if let Some(value) = word.strip_prefix("path=") {
            folder = value;
        } else if let Some(value) = word.strip_prefix("key=") {
            key_file = value;
        } else if word == "keygen" {
            keygen = true;
        } else if target.is_none() {
            target = Some(word);
        }
    }
    let Some(seed) = load_seed(api, key_file, keygen) else { return };
    if keygen {
        let mut line = [0u8; 80];
        let length = public_line(&seed, &mut line);
        println!("sshmount: add this line to ~/.ssh/authorized_keys on the server:");
        return println!("{}", core::str::from_utf8(&line[..length]).unwrap_or(""));
    }
    let Some((user, rest)) = target.and_then(|target| target.split_once('@')) else {
        return println!("usage: run sshmount USER@HOST[:PORT] [as=NAME] [path=/DIR] [key=NAME] &N  (help sshmount)");
    };
    let (host, port) = match rest.split_once(':') {
        Some((host, port)) => match port.parse::<u16>() {
            Ok(port) if port != 0 => (host, port),
            _ => return println!("sshmount: the port is a number from 1 to 65535."),
        },
        None => (rest, 22),
    };
    if user.is_empty() || host.is_empty() {
        return println!("sshmount: write USER@HOST.");
    }
    let mut default = [0u8; 16];
    let label = host.split('.').next().unwrap_or(host);
    let shown = label.len().min(16);
    for (slot, byte) in default.iter_mut().zip(label.bytes()) {
        *slot = if byte.is_ascii_alphanumeric() { byte.to_ascii_lowercase() } else { b'-' };
    }
    let name = name.unwrap_or(core::str::from_utf8(&default[..shown]).unwrap_or("remote"));

    let mut mount = Mount { dead: false, base: [0; PATH], base_length: 0 };
    let base = folder.trim_end_matches('/');
    if base.len() > PATH - 100 {
        return println!("sshmount: path= is too long.");
    }
    mount.base[..base.len()].copy_from_slice(base.as_bytes());
    mount.base_length = base.len();

    #[allow(static_mut_refs)]
    let (arena, socket) = unsafe { (&mut *core::ptr::addr_of_mut!(ARENA), core::ptr::replace(core::ptr::addr_of_mut!(SOCKET), Socket::new())) };
    let Some(mut client) = connect(api, socket, arena, user, host, port, &seed) else { return };
    if let Err(code) = tape_rt::remote_register(name) {
        client.link().socket.close(api);
        return println!("sshmount: cannot serve //{name} ({code}): a kernel older than ABI 35, a bad name or no free slot.");
    }
    println!("sshmount: {user}@{host}:{folder} is //{name}.");
    serve(api, &mut client, &mut mount, name);
    close_open(&mut client);
    tape_rt::remote_unregister(name);
    client.link().socket.close(api);
    println!("sshmount: //{name} unmounted.");
}

/// The key seed from settings (64 hex digits); `make` creates it when there is none (`keygen`).
fn load_seed(api: &TapeApi, file: &str, make: bool) -> Option<[u8; 32]> {
    let mut text = [0u8; 128];
    let length = read_named(api, file, &mut text);
    if length > 0 {
        let seed = core::str::from_utf8(&text[..length as usize]).ok().and_then(parse_seed);
        if seed.is_none() {
            println!("sshmount: settings/{file} is not 64 hex digits.");
        }
        return seed;
    }
    if !make {
        println!("sshmount: no settings/{file}: run `sshmount keygen` first and put its line on the server.");
        return None;
    }
    let mut seed = [0u8; 32];
    if tape_rt::random(&mut seed) != Some(true) {
        println!("sshmount: this kernel has no random numbers for a key (needs tape ABI 33).");
        return None;
    }
    let mut body = [0u8; 65];
    seed_text(&seed, &mut body);
    if write_named(api, file, &body) != 0 {
        println!("sshmount: could not write settings/{file}.");
        return None;
    }
    println!("sshmount: made settings/{file}.");
    Some(seed)
}

/// `settings/known_hosts`: lines `HOST SHA256:fingerprint`. Unknown hosts are added, a different key is refused.
fn check_host(api: &TapeApi, host: &str, port: u16, public: &[u8; 32]) -> bool {
    let mut print = [0u8; 50];
    fingerprint_public(public, &mut print);
    let print = core::str::from_utf8(&print).unwrap_or("");
    let mut label = [0u8; 64];
    let mut written = host.len().min(40);
    label[..written].copy_from_slice(&host.as_bytes()[..written]);
    if port != 22 {
        label[written] = b':';
        let mut digits = [0u8; 5];
        let mut at = 5;
        let mut rest = port;
        while rest > 0 {
            at -= 1;
            digits[at] = b'0' + (rest % 10) as u8;
            rest /= 10;
        }
        label[written + 1..written + 1 + 5 - at].copy_from_slice(&digits[at..]);
        written += 1 + 5 - at;
    }
    let label = core::str::from_utf8(&label[..written]).unwrap_or(host);
    let mut file = [0u8; 4096];
    let length = read_named(api, "known_hosts", &mut file[..4000]).max(0) as usize;
    let known = core::str::from_utf8(&file[..length]).unwrap_or("");
    for line in known.lines() {
        let mut words = line.split_ascii_whitespace();
        if words.next() == Some(label) {
            if words.next() == Some(print) {
                return true;
            }
            println!("sshmount: THE KEY OF {label} CHANGED. Refusing to log in.");
            println!("sshmount: it was meant to be {print}; if that is right, remove its line from settings/known_hosts.");
            return false;
        }
    }
    let mut at = length;
    if at > 0 && file[at - 1] != b'\n' {
        file[at] = b'\n';
        at += 1;
    }
    for part in [label.as_bytes(), b" ", print.as_bytes(), b"\n"] {
        file[at..at + part.len()].copy_from_slice(part);
        at += part.len();
    }
    if write_named(api, "known_hosts", &file[..at]) != 0 {
        println!("sshmount: could not write settings/known_hosts; not trusting {label}.");
        return false;
    }
    println!("sshmount: {label} is new: remembered its key {print}.");
    true
}

fn connect(api: &'static TapeApi, socket: Socket<16384>, arena: &'static mut [u8], user: &str, host: &str, port: u16, seed: &[u8; 32]) -> Option<Session> {
    let nic = tape_rt::active_nic();
    if nic >= (api.nic_count)() as usize {
        println!("sshmount: no network interface.");
        return None;
    }
    let mut mac = [0u8; 6];
    (api.nic_mac)(nic, mac.as_mut_ptr());
    let Some((config, _, dns_server)) = net::dhcp_with_servers(api, nic, MacAddress(mac)) else {
        println!("sshmount: no DHCP lease on NIC {}.", nic + 1);
        return None;
    };
    let address = match tape_ip::dns::lookup(api, nic, &config, &mut ArpCache::<8>::new(), dns_server.unwrap_or(config.gateway), tape_net::dns::PORT, host) {
        Ok(address) => address,
        Err(why) => {
            println!("sshmount: {host}: {why}.");
            return None;
        }
    };
    let mut wire = Wire { socket, api };
    if let Err(why) = wire.socket.connect(api, nic, &config, address, port, 3_000_000) {
        println!("sshmount: {host}: {why}.");
        return None;
    }
    let mut refused = false;
    let result = Client::connect(wire, arena, user, seed, &mut |public| {
        let trusted = check_host(api, host, port, public);
        refused = !trusted;
        trusted
    });
    match result {
        Ok(client) => Some(client),
        Err(error) => {
            if !refused {
                println!("sshmount: {user}@{host}: {}.", error.describe());
            }
            None
        }
    }
}

fn serve(api: &'static TapeApi, client: &mut Session, mount: &mut Mount, name: &str) {
    #[allow(static_mut_refs)]
    let buffer = unsafe { &mut *core::ptr::addr_of_mut!(BUF) };
    let mut last = tape_rt::micros();
    let mut request = RemoteRequest::default();
    while (api.should_abort)() == 0 && !mount.dead {
        match tape_rt::remote_poll(name, &mut request, &mut buffer[..]) {
            Ok(true) => {
                answer(client, mount, &request, buffer);
                last = tape_rt::micros();
            }
            Ok(false) => {
                if tape_rt::micros().wrapping_sub(last) > IDLE_CLOSE_MICROS {
                    close_open(client);
                }
                (api.wait_for_event)();
            }
            Err(_) => break,
        }
    }
    if mount.dead {
        println!("sshmount: the connection to the server failed.");
    }
}

fn text(bytes: &[u8]) -> &str {
    core::str::from_utf8(bytes).unwrap_or("")
}

/// Maps an SFTP failure to the `FS_*` code the caller sees; a dead connection also ends the mount.
fn fail(mount: &mut Mount, error: Error) -> isize {
    match error {
        Error::Status(STATUS_NO_SUCH_FILE) => FS_NOT_FOUND,
        Error::Status(STATUS_PERMISSION_DENIED) => FS_READ_ONLY,
        Error::Status(STATUS_NO_CONNECTION | STATUS_CONNECTION_LOST) | Error::Link | Error::Closed | Error::Protocol(_) => {
            mount.dead = true;
            FS_ERROR
        }
        _ => FS_ERROR,
    }
}

fn answer(client: &mut Session, mount: &mut Mount, request: &RemoteRequest, buffer: &mut [u8]) {
    let (path_length, path2_length, data_length) = (request.path_len as usize, request.path2_len as usize, request.data_len as usize);
    let (paths, rest) = buffer.split_at_mut(path_length + path2_length);
    let (mut first, mut second) = ([0u8; PATH], [0u8; PATH]);
    let (path, path2) = (mount.remote(text(&paths[..path_length]), &mut first), mount.remote(text(&paths[path_length..]), &mut second));
    let (Some(path), Some(path2)) = (path, path2) else {
        tape_rt::remote_reply(request.id, FS_BAD_NAME, None, &[]);
        return;
    };
    let mut entry = VfsEntry::default();
    let (mut code, mut with_entry, mut produced) = (FS_ERROR, false, 0usize);
    // A change invalidates the folder listing and the kept handle (which could name the file being replaced).
    if !matches!(request.op, REMOTE_STAT | REMOTE_READ_DIR | REMOTE_READ | REMOTE_WRITE) {
        unsafe { (*core::ptr::addr_of_mut!(LIST)).valid = false };
        close_open(client);
    } else if request.op == REMOTE_WRITE {
        unsafe { (*core::ptr::addr_of_mut!(LIST)).valid = false };
    }
    match request.op {
        REMOTE_STAT => match client.stat(path) {
            Ok(attrs) => {
                entry.stat = shape(&attrs);
                (code, with_entry) = (0, true);
            }
            Err(error) => code = fail(mount, error),
        },
        REMOTE_READ_DIR => match read_dir(client, path, request.offset as usize) {
            Ok(Some(found)) => {
                let item = unsafe { &(*core::ptr::addr_of!(LIST)).items[found] };
                entry.stat = shape(&item.attrs);
                entry.name_length = item.length as u32;
                entry.name[..item.length].copy_from_slice(&item.name[..item.length]);
                (code, with_entry) = (1, true);
            }
            Ok(None) => code = 0,
            Err(error) => code = fail(mount, error),
        },
        REMOTE_READ => {
            let wanted = (request.length as usize).min(rest.len());
            match read(client, path, request.offset, &mut rest[..wanted]) {
                Ok(count) => (code, produced) = (count as isize, count),
                Err(error) => code = fail(mount, error),
            }
        }
        REMOTE_CREATE => {
            let mode = if request.mode & 0o7777 != 0 { request.mode & 0o7777 } else if request.kind == VFS_DIR { MODE_DIR } else { MODE_FILE };
            let made = if request.kind == VFS_DIR {
                client.make_dir(path, mode)
            } else {
                client.open(path, OPEN_WRITE | OPEN_CREATE | OPEN_EXCLUSIVE, mode).and_then(|handle| client.close(&handle))
            };
            code = match made {
                Ok(()) => 0,
                // SFTP v3 only says "failure" for both a name that is taken and any other trouble.
                Err(Error::Status(_)) if client.stat(path).is_ok() => FS_EXISTS,
                Err(error) => fail(mount, error),
            };
        }
        REMOTE_WRITE => match write(client, path, request.offset, &rest[..data_length]) {
            Ok(count) => code = count as isize,
            Err(error) => code = fail(mount, error),
        },
        REMOTE_TRUNCATE => code = client.set_size(path, request.offset).map_or_else(|error| fail(mount, error), |()| 0),
        REMOTE_REMOVE => {
            let removed = match client.stat(path) {
                Ok(attrs) if attrs.is_dir() => client.remove_dir(path).map_err(|error| match error {
                    Error::Status(_) => Error::Status(u32::MAX),
                    other => other,
                }),
                Ok(_) => client.remove(path),
                Err(error) => Err(error),
            };
            code = match removed {
                Ok(()) => 0,
                // A folder that would not go is, in practice, not empty.
                Err(Error::Status(u32::MAX)) => FS_NOT_EMPTY,
                Err(error) => fail(mount, error),
            };
        }
        REMOTE_RENAME => code = client.rename(path, path2).map_or_else(|error| fail(mount, error), |()| 0),
        REMOTE_SET_OWNER => {
            let mut done = client.set_owner(path, request.uid, request.gid);
            if done.is_ok() && request.mode != VFS_KEEP_MODE {
                done = client.set_mode(path, request.mode & 0o7777);
            }
            code = done.map_or_else(|error| fail(mount, error), |()| 0);
        }
        _ => {}
    }
    tape_rt::remote_reply(request.id, code, with_entry.then_some(&entry), &rest[..produced]);
}

fn shape(attrs: &Attrs) -> VfsStat {
    let directory = attrs.is_dir();
    VfsStat {
        kind: if directory { VFS_DIR } else { VFS_FILE },
        mode: attrs.permissions.map_or(if directory { MODE_DIR } else { MODE_FILE }, |mode| mode & 0o7777),
        size: attrs.size.unwrap_or(0),
        mtime: u64::from(attrs.mtime.unwrap_or(0)),
        uid: attrs.uid.unwrap_or(0),
        gid: attrs.gid.unwrap_or(0),
        links: 1,
        ..VfsStat::default()
    }
}

fn read_dir(client: &mut Session, path: &str, index: usize) -> Result<Option<usize>, Error> {
    #[allow(static_mut_refs)]
    let list = unsafe { &mut *core::ptr::addr_of_mut!(LIST) };
    if !list.valid || index == 0 || &list.path[..list.path_length] != path.as_bytes() {
        list.count = 0;
        let handle = client.open_dir(path)?;
        let walked = loop {
            let fresh = &mut *list;
            match client.read_dir(&handle, &mut |name, attrs| {
                if name == b"." || name == b".." || name.len() > 255 || fresh.count >= LISTED {
                    return;
                }
                let item = &mut fresh.items[fresh.count];
                item.name[..name.len()].copy_from_slice(name);
                (item.length, item.attrs) = (name.len(), *attrs);
                fresh.count += 1;
            }) {
                Ok(true) => {}
                Ok(false) => break Ok(()),
                Err(error) => break Err(error),
            }
        };
        let _ = client.close(&handle);
        walked?;
        let kept = path.len().min(list.path.len());
        list.path[..kept].copy_from_slice(&path.as_bytes()[..kept]);
        list.path_length = kept;
        list.valid = kept == path.len();
    }
    Ok((index < list.count).then_some(index))
}

fn close_open(client: &mut Session) {
    #[allow(static_mut_refs)]
    let open = unsafe { &mut *core::ptr::addr_of_mut!(OPEN) };
    if let Some(kept) = open.take() {
        let _ = client.close(&kept.handle);
    }
}

/// The kept handle for `path`, opened (or reopened for writing) when needed.
fn handle_for(client: &mut Session, path: &str, writable: bool) -> Result<Handle, Error> {
    #[allow(static_mut_refs)]
    let open = unsafe { &mut *core::ptr::addr_of_mut!(OPEN) };
    if let Some(kept) = open.as_ref() {
        if &kept.path[..kept.path_length] == path.as_bytes() && (kept.writable || !writable) {
            return Ok(kept.handle);
        }
    }
    close_open(client);
    let handle = client.open(path, if writable { OPEN_READ | OPEN_WRITE } else { OPEN_READ }, 0)?;
    let mut kept = Open { path: [0; PATH], path_length: path.len(), handle, writable };
    kept.path[..path.len()].copy_from_slice(path.as_bytes());
    *open = Some(kept);
    Ok(handle)
}

fn read(client: &mut Session, path: &str, offset: u64, out: &mut [u8]) -> Result<usize, Error> {
    let handle = handle_for(client, path, false)?;
    let mut done = 0;
    while done < out.len() {
        let count = client.read(&handle, offset + done as u64, &mut out[done..])?;
        if count == 0 {
            break;
        }
        done += count;
    }
    Ok(done)
}

fn write(client: &mut Session, path: &str, offset: u64, data: &[u8]) -> Result<usize, Error> {
    let handle = handle_for(client, path, true)?;
    client.write(&handle, offset, data)?;
    Ok(data.len())
}
