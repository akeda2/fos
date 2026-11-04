//! smbmount: mounts an SMB share as the volume `//NAME`, as a service (`run smbmount //HOST/SHARE as=NAME &N`).
//!
//! The kernel has no TCP, so this tape owns one NIC and one SMB connection and answers the file calls the kernel queues
//! for `//NAME` (tape ABI 35, `remote_poll` / `remote_reply`, `crates/kernel-core/src/remote.rs`). The kernel side knows
//! nothing about SMB: the same calls will serve a mount over the native layer-2 file protocol.
#![no_std]
#![no_main]

use tape_rt::*;
use smb::{Client, Error, Handle, Transport};
use tape_ip::net;
use tape_ip::tcp::Socket;
use tape_net::{ArpCache, MacAddress};


tape_rt::tape_help!(
    br#"run smbmount //HOST/SHARE [as=NAME] [user=NAME PASSWORD] &N
Mounts an SMB share as the volume //NAME (default: the share's name, lower case)
until the service is stopped (kill). Then
  ls //NAME/folder    cat //NAME/file    copy //NAME/file disk1/file
and every tape's vfs_* calls work on it, writes too where the share allows
them. The service owns NIC N and takes its address from DHCP. Guest login
unless user= is given. SMB 2.0.2 to 2.1: Linux/Samba/Windows, or fos's smbd.
Folders list their first 128 entries.
"#
);

const PORT: u16 = 445;
const TIMEOUT_MICROS: u64 = 5_000_000;
const CHUNK: usize = 64 * 1024;
/// What one request carries: `remote::MAX_DATA` in the kernel plus room for the two paths.
const BUFFER: usize = 128 * 1024 + 4096;
const LISTED: usize = 128;
/// A cached open file is closed after this long without a request.
const IDLE_CLOSE_MICROS: u64 = 300_000;

struct Link {
    socket: Socket<16384>,
    api: Option<&'static TapeApi>,
}

impl Transport for Link {
    fn send(&mut self, data: &[u8]) -> Result<(), Error> {
        let api = self.api.ok_or(Error::Transport)?;
        self.socket.send(api, data, TIMEOUT_MICROS).map_err(|_| Error::Transport)
    }

    fn recv_exact(&mut self, out: &mut [u8]) -> Result<(), Error> {
        let api = self.api.ok_or(Error::Transport)?;
        self.socket.recv_exact(api, out, TIMEOUT_MICROS).map_err(|_| Error::Transport)
    }
}

type Session = Client<Link, { CHUNK + 1024 }>;

static mut CLIENT: Session = Client::new(Link { socket: Socket::new(), api: None });
static mut BUF: [u8; BUFFER] = [0; BUFFER];
static mut LIST: Listing = Listing::new();
static mut OPEN: Option<Open> = None;

struct Item {
    name: [u8; 255],
    length: usize,
    size: u64,
    directory: bool,
}

/// The entries of the folder last listed, so a walk by index costs one directory read, not one per entry.
struct Listing {
    path: [u8; 256],
    path_length: usize,
    valid: bool,
    count: usize,
    items: [Item; LISTED],
}

impl Listing {
    const fn new() -> Self {
        const EMPTY: Item = Item { name: [0; 255], length: 0, size: 0, directory: false };
        Listing { path: [0; 256], path_length: 0, valid: false, count: 0, items: [EMPTY; LISTED] }
    }
}

/// The one file kept open between requests (a copy reads or writes the same file in many pieces).
struct Open {
    path: [u8; 256],
    path_length: usize,
    handle: Handle,
    writable: bool,
}

struct Mount {
    dead: bool,
}

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(api: &'static TapeApi) {
    tape_rt::init(api);
    let arguments = core::str::from_utf8(tape_rt::args()).unwrap_or("");
    let (mut url, mut name, mut user, mut password) = (None, None, None, "");
    let secret_prefix = concat!("pass", "word=");
    for word in arguments.split_whitespace() {
        if let Some(value) = word.strip_prefix("as=") {
            name = Some(value);
        } else if let Some(value) = word.strip_prefix("user=") {
            user = Some(value);
        } else if let Some(value) = word.strip_prefix(secret_prefix) {
            password = value;
        } else if url.is_none() {
            url = Some(word);
        } else if user.is_some() && password.is_empty() {
            password = word;
        }
    }
    let Some(rest) = url.and_then(|url| url.strip_prefix("//")) else {
        return println!("usage: run smbmount //HOST/SHARE [as=NAME] [user=NAME PASSWORD] &N  (help smbmount)");
    };
    let Some((host, share)) = rest.split_once('/') else { return println!("smbmount: write //HOST/SHARE.") };
    let share = share.trim_end_matches('/');
    if host.is_empty() || share.is_empty() || share.contains('/') {
        return println!("smbmount: write //HOST/SHARE.");
    }
    let mut default = [0u8; 16];
    let shown = share.len().min(16);
    for (slot, byte) in default.iter_mut().zip(share.bytes()) {
        *slot = if byte.is_ascii_alphanumeric() { byte.to_ascii_lowercase() } else { b'-' };
    }
    let name = name.unwrap_or(core::str::from_utf8(&default[..shown]).unwrap_or("share"));
    #[allow(static_mut_refs)]
    let client = unsafe { &mut *core::ptr::addr_of_mut!(CLIENT) };
    if connect(api, client, host, share, user.map(|user| (user, password))).is_none() {
        return;
    }
    if let Err(code) = tape_rt::remote_register(name) {
        client.transport().socket.close(api);
        return println!("smbmount: cannot serve //{name} ({code}): a kernel older than ABI 35, a bad name or no free slot.");
    }
    println!("smbmount: //{host}/{share} is //{name}.");
    serve(api, client, name);
    close_open(client);
    tape_rt::remote_unregister(name);
    client.transport().socket.close(api);
    println!("smbmount: //{name} unmounted.");
}

fn connect(api: &'static TapeApi, client: &mut Session, host: &str, share: &str, login: Option<(&str, &str)>) -> Option<()> {
    let nic = tape_rt::active_nic();
    if nic >= (api.nic_count)() as usize {
        println!("smbmount: no network interface.");
        return None;
    }
    let mut mac = [0u8; 6];
    (api.nic_mac)(nic, mac.as_mut_ptr());
    let Some((config, _, dns_server)) = net::dhcp_with_servers(api, nic, MacAddress(mac)) else {
        println!("smbmount: no DHCP lease on NIC {}.", nic + 1);
        return None;
    };
    let address = match tape_ip::dns::lookup(api, nic, &config, &mut ArpCache::<8>::new(), dns_server.unwrap_or(config.gateway), tape_net::dns::PORT, host) {
        Ok(address) => address,
        Err(why) => {
            println!("smbmount: {host}: {why}.");
            return None;
        }
    };
    let link = client.transport();
    link.api = Some(api);
    if let Err(why) = link.socket.connect(api, nic, &config, address, PORT, 3_000_000) {
        println!("smbmount: {host}: {why}.");
        return None;
    }
    if let Some((user, password)) = login {
        let mut nonce = [0u8; 8];
        if tape_rt::random(&mut nonce).is_none() {
            nonce = tape_rt::micros().to_le_bytes();
        }
        if client.login(user, password, nonce).is_err() {
            println!("smbmount: the user name is too long.");
            return None;
        }
    }
    if let Err(error) = client.connect(host, share) {
        println!("smbmount: share {share}: {}.", error.describe());
        return None;
    }
    Some(())
}

fn serve(api: &'static TapeApi, client: &mut Session, name: &str) {
    #[allow(static_mut_refs)]
    let buffer = unsafe { &mut *core::ptr::addr_of_mut!(BUF) };
    let mut mount = Mount { dead: false };
    let mut last = tape_rt::micros();
    let mut request = RemoteRequest::default();
    while (api.should_abort)() == 0 && !mount.dead {
        match tape_rt::remote_poll(name, &mut request, &mut buffer[..]) {
            Ok(true) => {
                answer(client, &mut mount, &request, buffer);
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
        println!("smbmount: the connection to the server failed.");
    }
}

fn text(bytes: &[u8]) -> &str {
    core::str::from_utf8(bytes).unwrap_or("")
}

/// Maps an SMB failure to the `FS_*` code the caller sees; a dead connection also ends the mount.
fn fail(mount: &mut Mount, error: Error) -> isize {
    match error {
        Error::Status(0xC000_0034 | 0xC000_003A | 0xC000_000F) => FS_NOT_FOUND,
        Error::Status(0xC000_0035) => FS_EXISTS,
        Error::Status(0xC000_0022 | 0xC000_00BB) => FS_READ_ONLY,
        Error::Status(0xC000_007F) => FS_NO_SPACE,
        Error::Status(0xC000_00BA) => FS_IS_DIR,
        Error::Status(0xC000_0101) => FS_NOT_EMPTY,
        Error::Status(0xC000_0033 | 0xC000_0039) => FS_BAD_NAME,
        Error::Transport => {
            mount.dead = true;
            FS_ERROR
        }
        _ => FS_ERROR,
    }
}

fn answer(client: &mut Session, mount: &mut Mount, request: &RemoteRequest, buffer: &mut [u8]) {
    let (path_length, path2_length, data_length) = (request.path_len as usize, request.path2_len as usize, request.data_len as usize);
    let (paths, rest) = buffer.split_at_mut(path_length + path2_length);
    let (path, path2) = (text(&paths[..path_length]), text(&paths[path_length..]));
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
        REMOTE_STAT => match stat(client, path) {
            Ok((directory, size)) => {
                entry.stat = shape(directory, size);
                (code, with_entry) = (0, true);
            }
            Err(error) => code = fail(mount, error),
        },
        REMOTE_READ_DIR => match read_dir(client, path, request.offset as usize) {
            Ok(Some(found)) => {
                let item = unsafe { &(*core::ptr::addr_of!(LIST)).items[found] };
                entry.stat = shape(item.directory, item.size);
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
            let made = if request.kind == VFS_DIR {
                client.make_dir(path)
            } else {
                client.create_file(path, false).and_then(|handle| client.close(&handle))
            };
            code = made.map_or_else(|error| fail(mount, error), |()| 0);
        }
        REMOTE_WRITE => match write(client, path, request.offset, &rest[..data_length]) {
            Ok(count) => code = count as isize,
            Err(error) => code = fail(mount, error),
        },
        REMOTE_TRUNCATE if request.offset == 0 => {
            code = client.create_file(path, true).and_then(|handle| client.close(&handle)).map_or_else(|error| fail(mount, error), |()| 0);
        }
        REMOTE_TRUNCATE => code = FS_READ_ONLY,
        REMOTE_REMOVE => {
            let removed = match client.remove(path, false) {
                Err(Error::Status(0xC000_00BA)) => client.remove(path, true),
                other => other,
            };
            code = removed.map_or_else(|error| fail(mount, error), |()| 0);
        }
        REMOTE_RENAME => code = client.rename(path, path2).map_or_else(|error| fail(mount, error), |()| 0),
        // SMB has no unix owner or mode to set; accepting it keeps a copy that tries going.
        REMOTE_SET_OWNER => code = 0,
        _ => {}
    }
    tape_rt::remote_reply(request.id, code, with_entry.then_some(&entry), &rest[..produced]);
}

fn shape(directory: bool, size: u64) -> VfsStat {
    VfsStat { kind: if directory { VFS_DIR } else { VFS_FILE }, mode: if directory { 0o777 } else { 0o666 }, size, links: 1, ..VfsStat::default() }
}

/// (is a folder, size). A file opens as a file; a folder says so and is then opened as one.
fn stat(client: &mut Session, path: &str) -> Result<(bool, u64), Error> {
    if path.is_empty() {
        return Ok((true, 0));
    }
    match client.open(path, false) {
        Ok(handle) => {
            let size = handle.size;
            client.close(&handle)?;
            Ok((false, size))
        }
        Err(Error::Status(0xC000_00BA)) => {
            let handle = client.open(path, true)?;
            client.close(&handle)?;
            Ok((true, 0))
        }
        Err(error) => Err(error),
    }
}

fn read_dir(client: &mut Session, path: &str, index: usize) -> Result<Option<usize>, Error> {
    #[allow(static_mut_refs)]
    let list = unsafe { &mut *core::ptr::addr_of_mut!(LIST) };
    if !list.valid || index == 0 || &list.path[..list.path_length] != path.as_bytes() {
        list.count = 0;
        let fresh = &mut *list;
        let result = client.list(path, |found| {
            let mut name = [0u8; 255];
            let length = found.name_utf8(&mut name);
            if &name[..length] == b"." || &name[..length] == b".." || fresh.count >= LISTED {
                return;
            }
            fresh.items[fresh.count] = Item { name, length, size: found.size, directory: found.is_dir() };
            fresh.count += 1;
        });
        result?;
        let kept = path.len().min(list.path.len());
        list.path[..kept].copy_from_slice(&path.as_bytes()[..kept]);
        list.path_length = if kept == path.len() { kept } else { 0 };
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
    let handle = if writable { client.open_write(path)? } else { client.open(path, false)? };
    if path.len() <= 256 {
        let mut kept = Open { path: [0; 256], path_length: path.len(), handle, writable };
        kept.path[..path.len()].copy_from_slice(path.as_bytes());
        *open = Some(kept);
    }
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
    let mut done = 0;
    while done < data.len() {
        let count = client.write(&handle, offset + done as u64, &data[done..])?;
        if count == 0 {
            break;
        }
        done += count;
    }
    Ok(done)
}
