//! Remote volumes (tape ABI 35): a tape serves the volume `//NAME`, and the file calls that name it are answered by that tape.
//!
//! The kernel has no TCP, so a volume that lives across the network (an SMB share today, the native layer-2 file protocol later)
//! is served by a provider tape, normally a service. The provider registers a name (`remote_register`) and loops on
//! `remote_poll` / `remote_reply`. A caller (`vfs_*`, `fs_*`, the shell) posts one request into a single slot and waits,
//! handing the CPU on (`tasks::yield_now`) so the provider can run. Nothing here knows the protocol: a request is an operation,
//! one or two paths, an offset and some bytes, and the answer is a code, an entry and some bytes.
//!
//! One request is in flight at a time (the file lock already serialises callers on the server kernel; a second caller waits for
//! the slot). The slot's pointers refer to the waiting caller's buffers, so the caller must stay until the state is `DONE`, or
//! until it can take the request back (state `POSTED`, nobody has it yet). The state byte is the hand-over: whoever changes it
//! has written everything it hands over first (release) and the reader sees it with acquire.

use core::ptr::{addr_of, addr_of_mut};
use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use fosfor_abi::tape::*;

use crate::{tasks, timer};

const NAMES: usize = 4;
const NAME_MAX: usize = 16;
/// The most data one request carries (a read's length, a write's bytes): bigger calls are cut short, callers loop.
pub const MAX_DATA: usize = 128 * 1024;
const POST_WAIT_US: u64 = 10_000_000;
const TAKEN_WAIT_US: u64 = 60_000_000;

const IDLE: u8 = 0;
const SETUP: u8 = 1;
const POSTED: u8 = 2;
const TAKEN: u8 = 3;
const DONE: u8 = 4;

struct Slot {
    request: RemoteRequest,
    owner: usize,
    path: *const u8,
    path2: *const u8,
    data: *const u8,
    out: *mut u8,
    out_capacity: usize,
    entry: VfsEntry,
    result: isize,
}

static STATE: AtomicU8 = AtomicU8::new(IDLE);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static mut SLOT: Slot = Slot {
    request: RemoteRequest { id: 0, op: 0, kind: 0, mode: 0, path_len: 0, path2_len: 0, data_len: 0, offset: 0, length: 0, uid: 0, gid: 0 },
    owner: 0,
    path: core::ptr::null(),
    path2: core::ptr::null(),
    data: core::ptr::null(),
    out: core::ptr::null_mut(),
    out_capacity: 0,
    entry: VfsEntry { stat: VfsStat { kind: 0, mode: 0, size: 0, mtime: 0, ctime: 0, atime: 0, uid: 0, gid: 0, links: 0, reserved: 0, inode: 0 }, name_length: 0, reserved: 0, name: [0; 256] },
    result: 0,
};
static mut REGISTRY: [([u8; NAME_MAX], usize); NAMES] = [([0; NAME_MAX], 0); NAMES];

fn slot() -> &'static mut Slot {
    unsafe { &mut *addr_of_mut!(SLOT) }
}

fn registry() -> &'static mut [([u8; NAME_MAX], usize); NAMES] {
    unsafe { &mut *addr_of_mut!(REGISTRY) }
}

fn valid_name(name: &[u8]) -> bool {
    (1..=NAME_MAX).contains(&name.len()) && name.iter().all(|&byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'_')
}

fn find(name: &[u8]) -> Option<usize> {
    registry().iter().position(|(stored, length)| *length != 0 && &stored[..*length] == name)
}

/// Whether a reference names a remote volume (`//NAME` or `//NAME/path`), registered or not.
pub fn is_remote(reference: &[u8]) -> bool {
    reference.starts_with(b"//")
}

/// `remote_register`.
pub fn register(name: &[u8]) -> isize {
    if !valid_name(name) {
        return FS_BAD_NAME;
    }
    let index = match find(name).or_else(|| registry().iter().position(|(_, length)| *length == 0)) {
        Some(index) => index,
        None => return FS_NO_SPACE,
    };
    abandon(index);
    let entry = &mut registry()[index];
    entry.0[..name.len()].copy_from_slice(name);
    entry.1 = name.len();
    0
}

/// `remote_unregister`.
pub fn unregister(name: &[u8]) -> isize {
    let Some(index) = find(name) else { return FS_NOT_FOUND };
    registry()[index].1 = 0;
    abandon(index);
    0
}

/// A caller waiting on a provider that went away is told so.
fn abandon(index: usize) {
    if slot().owner == index {
        for from in [POSTED, TAKEN] {
            if STATE.compare_exchange(from, DONE, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                slot().result = FS_ERROR;
                break;
            }
        }
    }
}

/// `remote_poll`: 1 and the request, 0 for nothing queued.
pub fn poll(name: &[u8], request: &mut RemoteRequest, buffer: &mut [u8]) -> isize {
    let Some(index) = find(name) else { return FS_NOT_FOUND };
    if STATE.load(Ordering::Acquire) != POSTED || slot().owner != index {
        return 0;
    }
    let s = slot();
    let (path, path2, data) = (s.request.path_len as usize, s.request.path2_len as usize, s.request.data_len as usize);
    if path + path2 + data > buffer.len() {
        s.result = FS_TOO_BIG;
        STATE.store(DONE, Ordering::Release);
        return 0;
    }
    if STATE.compare_exchange(POSTED, TAKEN, Ordering::AcqRel, Ordering::Acquire).is_err() {
        return 0;
    }
    unsafe {
        core::ptr::copy_nonoverlapping(s.path, buffer.as_mut_ptr(), path);
        core::ptr::copy_nonoverlapping(s.path2, buffer.as_mut_ptr().add(path), path2);
        core::ptr::copy_nonoverlapping(s.data, buffer.as_mut_ptr().add(path + path2), data);
    }
    *request = s.request;
    1
}

/// `remote_reply`.
pub fn reply(id: u64, code: isize, entry: Option<&VfsEntry>, data: &[u8]) -> isize {
    let s = slot();
    if STATE.load(Ordering::Acquire) != TAKEN || s.request.id != id {
        return FS_ERROR;
    }
    if let Some(entry) = entry {
        s.entry = *entry;
    }
    s.result = code;
    if s.request.op == REMOTE_READ && code >= 0 {
        let count = data.len().min(s.out_capacity);
        unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), s.out, count) };
        s.result = count as isize;
    }
    STATE.store(DONE, Ordering::Release);
    0
}

struct Call<'a> {
    request: RemoteRequest,
    path: &'a [u8],
    path2: &'a [u8],
    data: &'a [u8],
}

fn call(index: usize, what: Call, out: &mut [u8], entry: Option<&mut VfsEntry>) -> isize {
    let begin = timer::micros();
    while STATE.compare_exchange(IDLE, SETUP, Ordering::AcqRel, Ordering::Acquire).is_err() {
        if timer::micros().wrapping_sub(begin) > POST_WAIT_US {
            return FS_ERROR;
        }
        tasks::yield_now();
    }
    let s = slot();
    s.request = what.request;
    s.request.id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    s.request.path_len = what.path.len() as u32;
    s.request.path2_len = what.path2.len() as u32;
    s.request.data_len = what.data.len() as u32;
    s.owner = index;
    (s.path, s.path2, s.data) = (what.path.as_ptr(), what.path2.as_ptr(), what.data.as_ptr());
    (s.out, s.out_capacity) = (out.as_mut_ptr(), out.len());
    s.result = FS_ERROR;
    STATE.store(POSTED, Ordering::Release);
    let posted = timer::micros();
    let result = loop {
        match STATE.load(Ordering::Acquire) {
            DONE => break s.result,
            POSTED if timer::micros().wrapping_sub(posted) > POST_WAIT_US => {
                if STATE.compare_exchange(POSTED, IDLE, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                    return FS_ERROR;
                }
            }
            TAKEN if timer::micros().wrapping_sub(posted) > POST_WAIT_US + TAKEN_WAIT_US => break FS_ERROR,
            _ => {}
        }
        tasks::yield_now();
    };
    if let Some(entry) = entry {
        *entry = unsafe { (*addr_of!(SLOT)).entry };
    }
    STATE.store(IDLE, Ordering::Release);
    result
}

/// `//NAME/path` -> (the provider's index, `path`), `Err` when nobody serves `NAME`.
fn target(reference: &[u8]) -> Result<(usize, &[u8]), isize> {
    let rest = &reference[2..];
    let end = rest.iter().position(|&byte| byte == b'/').unwrap_or(rest.len());
    let index = find(&rest[..end]).ok_or(FS_NO_VOLUME)?;
    let path = &rest[end..];
    Ok((index, &path[path.iter().take_while(|&&byte| byte == b'/').count()..]))
}

fn plain(op: u32, path: &[u8]) -> Call<'_> {
    Call { request: RemoteRequest { op, ..RemoteRequest::default() }, path, path2: &[], data: &[] }
}

pub fn stat(reference: &[u8], out: &mut VfsStat) -> isize {
    let (index, path) = match target(reference) {
        Ok(found) => found,
        Err(code) => return code,
    };
    let mut entry = VfsEntry::default();
    let code = call(index, plain(REMOTE_STAT, path), &mut [], Some(&mut entry));
    if code >= 0 {
        *out = entry.stat;
        return 0;
    }
    code
}

pub fn read_dir(reference: &[u8], index: usize, out: &mut VfsEntry) -> isize {
    let (provider, path) = match target(reference) {
        Ok(found) => found,
        Err(code) => return code,
    };
    let mut call_ = plain(REMOTE_READ_DIR, path);
    call_.request.offset = index as u64;
    let mut entry = VfsEntry::default();
    let code = call(provider, call_, &mut [], Some(&mut entry));
    if code == 1 {
        *out = entry;
    }
    code
}

pub fn read(reference: &[u8], offset: u64, buffer: &mut [u8]) -> isize {
    let (index, path) = match target(reference) {
        Ok(found) => found,
        Err(code) => return code,
    };
    let length = buffer.len().min(MAX_DATA);
    let mut call_ = plain(REMOTE_READ, path);
    call_.request.offset = offset;
    call_.request.length = length as u64;
    call(index, call_, &mut buffer[..length], None)
}

pub fn create(reference: &[u8], make: &VfsMake) -> isize {
    let (index, path) = match target(reference) {
        Ok(found) => found,
        Err(code) => return code,
    };
    let mut call_ = plain(REMOTE_CREATE, path);
    call_.request.kind = make.kind;
    call_.request.mode = make.mode;
    (call_.request.uid, call_.request.gid) = (make.uid, make.gid);
    call(index, call_, &mut [], None)
}

pub fn write(reference: &[u8], offset: u64, data: &[u8]) -> isize {
    let (index, path) = match target(reference) {
        Ok(found) => found,
        Err(code) => return code,
    };
    let mut call_ = plain(REMOTE_WRITE, path);
    call_.request.offset = offset;
    call_.data = &data[..data.len().min(MAX_DATA)];
    call(index, call_, &mut [], None)
}

pub fn truncate(reference: &[u8], size: u64) -> isize {
    let (index, path) = match target(reference) {
        Ok(found) => found,
        Err(code) => return code,
    };
    let mut call_ = plain(REMOTE_TRUNCATE, path);
    call_.request.offset = size;
    call(index, call_, &mut [], None)
}

pub fn remove(reference: &[u8]) -> isize {
    match target(reference) {
        Ok((index, path)) => call(index, plain(REMOTE_REMOVE, path), &mut [], None),
        Err(code) => code,
    }
}

/// Both names must be on the same remote volume.
pub fn rename(from: &[u8], to: &[u8]) -> isize {
    let ((index, path), other) = match (target(from), is_remote(to).then(|| target(to))) {
        (Ok(found), Some(Ok(other))) => (found, other),
        (Err(code), _) | (_, Some(Err(code))) => return code,
        (_, None) => return FS_ERROR,
    };
    if other.0 != index {
        return FS_ERROR;
    }
    let mut call_ = plain(REMOTE_RENAME, path);
    call_.path2 = other.1;
    call(index, call_, &mut [], None)
}

pub fn set_owner(reference: &[u8], uid: u32, gid: u32, mode: u32) -> isize {
    let (index, path) = match target(reference) {
        Ok(found) => found,
        Err(code) => return code,
    };
    let mut call_ = plain(REMOTE_SET_OWNER, path);
    (call_.request.uid, call_.request.gid, call_.request.mode) = (uid, gid, mode);
    call(index, call_, &mut [], None)
}

// ---- what the `fs_*` calls and the shell need, built from the calls above ----

/// The whole file (`fs_read`): `FS_TOO_BIG` when it does not fit.
pub fn read_whole(reference: &[u8], buffer: &mut [u8]) -> isize {
    let mut stat = VfsStat::default();
    let code = self::stat(reference, &mut stat);
    if code < 0 {
        return code;
    }
    if stat.kind == VFS_DIR {
        return FS_NOT_FOUND;
    }
    if stat.size > buffer.len() as u64 {
        return FS_TOO_BIG;
    }
    let mut done = 0;
    while done < stat.size as usize {
        let count = read(reference, done as u64, &mut buffer[done..stat.size as usize]);
        if count < 0 {
            return count;
        }
        if count == 0 {
            break;
        }
        done += count as usize;
    }
    done as isize
}

/// The length of a file, or an `FS_*` code (a folder is `FS_NOT_FOUND`, as `tape_fs::length` has it).
pub fn length(reference: &[u8]) -> isize {
    let mut stat = VfsStat::default();
    let code = self::stat(reference, &mut stat);
    if code < 0 {
        return code;
    }
    if stat.kind == VFS_DIR { FS_NOT_FOUND } else { stat.size as isize }
}

/// Every entry of a folder, as `tape_fs::each_entry` reports them.
pub fn each_entry(reference: &[u8], each: &mut dyn FnMut(&[u8], u64, bool, Option<fat::Time>)) -> Result<&'static str, isize> {
    let mut entry = VfsEntry::default();
    for index in 0.. {
        match read_dir(reference, index, &mut entry) {
            1 => each(&entry.name[..(entry.name_length as usize).min(entry.name.len())], entry.stat.size, entry.stat.kind == VFS_DIR, None),
            0 => break,
            code => return Err(code),
        }
    }
    Ok("remote")
}

/// Replaces a file with `data` (`fs_write`): created if missing, emptied, then written in pieces; 0 or an `FS_*` code.
pub fn write_whole(reference: &[u8], data: &[u8]) -> isize {
    let make = VfsMake { kind: VFS_FILE, mode: 0o644, uid: 0, gid: 0 };
    let code = create(reference, &make);
    if code < 0 && code != FS_EXISTS {
        return code;
    }
    let code = truncate(reference, 0);
    if code < 0 {
        return code;
    }
    let mut done = 0;
    while done < data.len() {
        let count = write(reference, done as u64, &data[done..]);
        if count <= 0 {
            return if count < 0 { count } else { FS_ERROR };
        }
        done += count as usize;
    }
    0
}

/// A folder (`mkdir`).
pub fn make_folder(reference: &[u8]) -> isize {
    create(reference, &VfsMake { kind: VFS_DIR, mode: 0o755, uid: 0, gid: 0 })
}

fn bytes<'a>(at: *const u8, length: usize) -> &'a [u8] {
    unsafe { core::slice::from_raw_parts(at, length) }
}

/// Tape ABI 35 `remote_register(name, name_len)`; shared by the x86 and aarch64 tape tables.
pub extern "C" fn api_remote_register(name: *const u8, length: usize) -> isize {
    register(bytes(name, length))
}

/// Tape ABI 35 `remote_unregister(name, name_len)`.
pub extern "C" fn api_remote_unregister(name: *const u8, length: usize) -> isize {
    unregister(bytes(name, length))
}

/// Tape ABI 35 `remote_poll(name, name_len, request, buffer, capacity)`.
pub extern "C" fn api_remote_poll(name: *const u8, length: usize, request: *mut RemoteRequest, buffer: *mut u8, capacity: usize) -> isize {
    poll(bytes(name, length), unsafe { &mut *request }, unsafe { core::slice::from_raw_parts_mut(buffer, capacity) })
}

/// Tape ABI 35 `remote_reply(id, code, entry, data, len)`.
pub extern "C" fn api_remote_reply(id: u64, code: isize, entry: *const VfsEntry, data: *const u8, length: usize) -> isize {
    reply(id, code, unsafe { entry.as_ref() }, bytes(data, length))
}
