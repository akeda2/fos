//! sshmount: mounts a folder of another machine, over SFTP, as the volume `//NAME`, as a service
//! (`run sshmount USER@HOST as=NAME &N`).
//!
//! Like `smbmount` this tape owns one NIC and one connection and answers the file calls the kernel queues for `//NAME`
//! (tape ABI 35, `remote_poll` / `remote_reply`). The login is an ed25519 key: the seed is `settings/ssh_key` (`sshmount keygen`
//! makes it and prints the line for the server's `authorized_keys`), and the server's key is checked against
//! `settings/known_hosts`, trust on first use.
#![no_std]
#![no_main]

use ssh::authorized::public_line;
use ssh::client::{
    Attrs, CLIENT_ARENA, Client, Error, Handle, OPEN_CREATE, OPEN_EXCLUSIVE, OPEN_READ, OPEN_WRITE, STATUS_NO_SUCH_FILE,
    STATUS_PERMISSION_DENIED,
};
use ssh_tape::{Trust, Wire, load_seed, parse_target};
use tape_ip::tcp::Socket;
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
    let Some(seed) = load_seed("sshmount", api, key_file, keygen) else { return };
    if keygen {
        let mut line = [0u8; 80];
        let length = public_line(&seed, &mut line);
        println!("sshmount: add this line to ~/.ssh/authorized_keys on the server:");
        return println!("{}", core::str::from_utf8(&line[..length]).unwrap_or(""));
    }
    let Some((user, host, port)) = target.and_then(|target| parse_target("sshmount", target)) else {
        return println!("usage: run sshmount USER@HOST[:PORT] [as=NAME] [path=/DIR] [key=NAME] &N  (help sshmount)");
    };
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

/// The mount's connection: SFTP over the shared TCP link.
fn connect(api: &'static TapeApi, socket: Socket<16384>, arena: &'static mut [u8], user: &str, host: &str, port: u16, seed: &[u8; 32]) -> Option<Session> {
    let wire = ssh_tape::dial("sshmount", api, socket, host, port, TIMEOUT_MICROS)?;
    let mut trust = Trust { tool: "sshmount", api, host, port, refused: false };
    match Client::connect(wire, arena, user, seed, &mut |public| trust.check(public)) {
        Ok(client) => Some(client),
        Err(error) => {
            if !trust.refused {
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
