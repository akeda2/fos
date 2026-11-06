//! SFTP version 3 (draft-ietf-secsh-filexfer-02, what OpenSSH's `sftp`, `scp -s` and `sshfs` speak) over `vfs_export::Files`,
//! with the OpenSSH extensions `posix-rename@openssh.com`, `fsync@openssh.com` and `limits@openssh.com`. No links.
//!
//! One request is answered completely before the next is looked at (`Sftp::step`), into a buffer sized for the largest reply
//! (a read of `MAX_IO` bytes). Paths are the client's, rooted at `/`, cleaned so no `..` leaves an export; with one export
//! it is `/`, with several each is a folder under `/` named after it. Permission bits are checked against the login's
//! uid and gid like smbd does (`Stat::allows`); volumes that keep none (FOSFS, FAT) check nothing and are read-only anyway.
//!
//! What it does not do, and says so: symlinks and hard links (OP_UNSUPPORTED), file times (`setstat` accepts and ignores them,
//! so `cp -p` and sshfs do not fail), and free-space numbers (no `statvfs@openssh.com`).

use crate::Account;
use crate::wire::{Reader, Writer};
use core::fmt::Write as _;
use vfs_export::{Fail, Files, PATH_MAX, Stat, clean};

pub const ROOT_MAX: usize = 200;
pub const EXPORTS: usize = 8;
/// The largest read or write one request carries (announced through `limits@openssh.com`).
pub const MAX_IO: usize = 65_536;
/// The largest request or reply, the packet length included.
pub const MAX_PACKET: usize = 69_632;
const HANDLES: usize = 16;
const HANDLE_PATH: usize = 256;

const INIT: u8 = 1;
const VERSION: u8 = 2;
const OPEN: u8 = 3;
const CLOSE: u8 = 4;
const READ: u8 = 5;
const WRITE: u8 = 6;
const LSTAT: u8 = 7;
const FSTAT: u8 = 8;
const SETSTAT: u8 = 9;
const FSETSTAT: u8 = 10;
const OPENDIR: u8 = 11;
const READDIR: u8 = 12;
const REMOVE: u8 = 13;
const MKDIR: u8 = 14;
const RMDIR: u8 = 15;
const REALPATH: u8 = 16;
const STAT: u8 = 17;
const RENAME: u8 = 18;
const STATUS: u8 = 101;
const HANDLE: u8 = 102;
const DATA: u8 = 103;
const NAME: u8 = 104;
const ATTRS: u8 = 105;
const EXTENDED: u8 = 200;
const EXTENDED_REPLY: u8 = 201;

const OK: u32 = 0;
const EOF: u32 = 1;
const NO_SUCH_FILE: u32 = 2;
const PERMISSION_DENIED: u32 = 3;
const FAILURE: u32 = 4;
const BAD_MESSAGE: u32 = 5;
const OP_UNSUPPORTED: u32 = 8;

const OPEN_READ: u32 = 1;
const OPEN_WRITE: u32 = 2;
const OPEN_APPEND: u32 = 4;
const OPEN_CREATE: u32 = 8;
const OPEN_TRUNCATE: u32 = 0x10;
const OPEN_EXCLUSIVE: u32 = 0x20;

const ATTR_SIZE: u32 = 1;
const ATTR_OWNER: u32 = 2;
const ATTR_MODE: u32 = 4;
const ATTR_TIMES: u32 = 8;
const ATTR_EXTENDED: u32 = 0x8000_0000;

#[derive(Clone, Copy)]
pub struct Export {
    name: [u8; 32],
    name_length: usize,
    root: [u8; ROOT_MAX],
    root_length: usize,
    pub read_only: bool,
}

/// The folders offered, each under a name.
pub struct Exports {
    list: [Export; EXPORTS],
    count: usize,
}

impl Default for Exports {
    fn default() -> Self {
        Self::new()
    }
}

impl Exports {
    pub const fn new() -> Exports {
        Exports { list: [Export { name: [0; 32], name_length: 0, root: [0; ROOT_MAX], root_length: 0, read_only: false }; EXPORTS], count: 0 }
    }

    pub fn add(&mut self, name: &str, root: &str, read_only: bool) -> Result<(), &'static str> {
        if name.is_empty() || name.len() > 32 || name.contains('/') || root.len() > ROOT_MAX {
            return Err("an export name is 1 to 32 characters without slashes and its folder at most 200");
        }
        if self.count == EXPORTS {
            return Err("at most 8 exports");
        }
        if (0..self.count).any(|index| self.name(index) == name) {
            return Err("an export name is given twice");
        }
        let export = &mut self.list[self.count];
        export.name[..name.len()].copy_from_slice(name.as_bytes());
        export.name_length = name.len();
        export.root[..root.len()].copy_from_slice(root.as_bytes());
        export.root_length = root.len();
        export.read_only = read_only;
        self.count += 1;
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn name(&self, index: usize) -> &str {
        core::str::from_utf8(&self.list[index].name[..self.list[index].name_length]).unwrap_or("")
    }

    pub fn root(&self, index: usize) -> &str {
        core::str::from_utf8(&self.list[index].root[..self.list[index].root_length]).unwrap_or("")
    }

    pub fn read_only(&self, index: usize) -> bool {
        self.list[index].read_only
    }

    fn find(&self, name: &str) -> Option<usize> {
        (0..self.count).find(|&index| self.name(index) == name)
    }
}

/// Where a client path points: the list of exports (with more than one) or a path inside one.
enum Spot<'a> {
    Top,
    In(usize, &'a str),
}

fn resolve<'a>(exports: &Exports, path: &str, buffer: &'a mut [u8; PATH_MAX]) -> Option<Spot<'a>> {
    let cleaned = clean(path, buffer)?;
    if exports.count == 1 {
        return Some(Spot::In(0, cleaned));
    }
    if cleaned.is_empty() {
        return Some(Spot::Top);
    }
    let (first, rest) = cleaned.split_once('/').unwrap_or((cleaned, ""));
    Some(Spot::In(exports.find(first)?, rest))
}

const TOP: Stat = Stat { dir: true, size: 0, mtime: 0, mode: 0o555, uid: 0, gid: 0 };

/// The folder part and the last name of a clean relative path ("" has no name).
fn split_name(path: &str) -> Option<(&str, &str)> {
    if path.is_empty() {
        return None;
    }
    Some(path.rsplit_once('/').unwrap_or(("", path)))
}

/// A status code, and the text the client shows.
#[derive(Clone, Copy)]
struct Refusal(u32, &'static str);

impl From<Fail> for Refusal {
    fn from(fail: Fail) -> Refusal {
        match fail {
            Fail::NotFound => Refusal(NO_SUCH_FILE, "no such file"),
            Fail::Denied => Refusal(PERMISSION_DENIED, "permission denied"),
            Fail::Exists => Refusal(FAILURE, "file exists"),
            Fail::NotEmpty => Refusal(FAILURE, "directory not empty"),
            Fail::NoSpace => Refusal(FAILURE, "no space left on device"),
            Fail::IsDir => Refusal(FAILURE, "is a directory"),
            Fail::Io => Refusal(FAILURE, "input/output error"),
        }
    }
}

const DENIED: Refusal = Refusal(PERMISSION_DENIED, "permission denied");
const MISSING: Refusal = Refusal(NO_SUCH_FILE, "no such file");

#[derive(Default)]
struct Attributes {
    size: Option<u64>,
    owner: Option<(u32, u32)>,
    mode: Option<u32>,
}

fn read_attributes(reader: &mut Reader) -> Option<Attributes> {
    let flags = reader.u32()?;
    let mut attributes = Attributes::default();
    if flags & ATTR_SIZE != 0 {
        attributes.size = Some(reader.u64()?);
    }
    if flags & ATTR_OWNER != 0 {
        attributes.owner = Some((reader.u32()?, reader.u32()?));
    }
    if flags & ATTR_MODE != 0 {
        attributes.mode = Some(reader.u32()?);
    }
    if flags & ATTR_TIMES != 0 {
        reader.u32()?;
        reader.u32()?;
    }
    if flags & ATTR_EXTENDED != 0 {
        for _ in 0..reader.u32()? {
            reader.string()?;
            reader.string()?;
        }
    }
    Some(attributes)
}

/// Permission bits as shown to the client: those of the volume, or 755/644 where it keeps none, minus write on a read-only export.
fn permissions(stat: &Stat, read_only: bool) -> u32 {
    let mut bits = if stat.mode == 0 { if stat.dir { 0o755 } else { 0o644 } } else { stat.mode & 0o777 };
    if read_only {
        bits &= !0o222;
    }
    bits | if stat.dir { 0o040_000 } else { 0o100_000 }
}

fn put_attributes(out: &mut Writer, stat: &Stat, read_only: bool) {
    out.u32(ATTR_SIZE | ATTR_OWNER | ATTR_MODE | ATTR_TIMES).u64(stat.size).u32(stat.uid).u32(stat.gid).u32(permissions(stat, read_only));
    out.u32(stat.mtime as u32).u32(stat.mtime as u32);
}

/// Days since 1970-01-01 as year, month (1-12) and day (Howard Hinnant's civil-from-days).
fn civil(days: u64) -> (u64, usize, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let day_of_era = z % 146_097;
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted + 2) / 5 + 1;
    let month = if shifted < 10 { shifted + 3 } else { shifted - 9 };
    (year_of_era + era * 400 + (month <= 2) as u64, month as usize, day)
}

struct Line {
    bytes: [u8; 400],
    length: usize,
}

impl core::fmt::Write for Line {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        let room = self.bytes.len() - self.length;
        let take = text.len().min(room);
        self.bytes[self.length..self.length + take].copy_from_slice(&text.as_bytes()[..take]);
        self.length += take;
        Ok(())
    }
}

/// The `ls -l` line of a directory entry that SFTP v3 puts in `longname`.
fn long_name(name: &[u8], stat: &Stat, read_only: bool, line: &mut Line) {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let bits = permissions(stat, read_only);
    line.length = 0;
    let _ = line.write_char(if stat.dir { 'd' } else { '-' });
    for shift in [6, 3, 0] {
        let triple = bits >> shift;
        let _ = write!(line, "{}{}{}", if triple & 4 != 0 { 'r' } else { '-' }, if triple & 2 != 0 { 'w' } else { '-' }, if triple & 1 != 0 { 'x' } else { '-' });
    }
    let (year, month, day) = civil(stat.mtime / 86_400);
    let _ = write!(line, " {:>3} {:<8} {:<8} {:>10} {} {:>2} {:>5} ", 1, stat.uid, stat.gid, stat.size, MONTHS[month - 1], day, year);
    let text = core::str::from_utf8(name).unwrap_or("?");
    let _ = line.write_str(text);
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Free,
    File,
    Folder,
}

#[derive(Clone, Copy)]
struct Handle {
    id: u32,
    kind: Kind,
    read: bool,
    write: bool,
    append: bool,
    /// `EXPORTS` for the list of exports.
    export: usize,
    path: [u8; HANDLE_PATH],
    path_length: usize,
    /// The next entry a READDIR returns.
    cursor: usize,
}

const FREE: Handle = Handle { id: 0, kind: Kind::Free, read: false, write: false, append: false, export: 0, path: [0; HANDLE_PATH], path_length: 0, cursor: 0 };

pub enum Step {
    /// The request is not all here yet.
    More,
    /// A request of this many bytes (length field included) was answered.
    Done(usize),
    /// Not SFTP; end the session.
    Bad,
}

pub struct Sftp {
    handles: [Handle; HANDLES],
    serial: u32,
    started: bool,
}

impl Default for Sftp {
    fn default() -> Self {
        Self::new()
    }
}

impl Sftp {
    pub const fn new() -> Sftp {
        Sftp { handles: [FREE; HANDLES], serial: 0, started: false }
    }

    /// Forgets every open handle (the channel closed); the caller syncs the volume.
    pub fn reset(&mut self) {
        *self = Sftp::new();
    }

    /// Answers the request at the front of `input` into `out`, if all of it is there.
    pub fn step(&mut self, exports: &Exports, files: &mut dyn Files, account: &Account, input: &[u8], out: &mut Writer) -> Step {
        let Some(head) = input.get(..4) else { return Step::More };
        let length = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as usize;
        if length == 0 || length > MAX_PACKET {
            return Step::Bad;
        }
        let Some(body) = input.get(4..4 + length) else { return Step::More };
        let mut reader = Reader::new(body);
        let Some(kind) = reader.u8() else { return Step::Bad };
        if kind == INIT {
            if self.started {
                return Step::Bad;
            }
            self.started = true;
            let mark = begin(out, VERSION);
            out.u32(3);
            for (name, value) in [("posix-rename@openssh.com", "1"), ("fsync@openssh.com", "1"), ("limits@openssh.com", "1")] {
                out.string(name.as_bytes()).string(value.as_bytes());
            }
            end(out, mark);
            return Step::Done(4 + length);
        }
        let Some(id) = reader.u32().filter(|_| self.started) else { return Step::Bad };
        let mark = out.len();
        let mut work = Work { sftp: self, exports, files, account };
        if let Err(Refusal(code, text)) = work.request(kind, id, reader, out) {
            out.truncate(mark);
            status(out, id, code, text);
        }
        if out.ok() { Step::Done(4 + length) } else { Step::Bad }
    }
}

fn begin(out: &mut Writer, kind: u8) -> usize {
    let mark = out.len();
    out.u32(0).u8(kind);
    mark
}

fn end(out: &mut Writer, mark: usize) {
    let length = (out.len() - mark - 4) as u32;
    out.patch_u32(mark, length);
}

fn status(out: &mut Writer, id: u32, code: u32, text: &str) {
    let mark = begin(out, STATUS);
    out.u32(id).u32(code).string(text.as_bytes()).string(b"");
    end(out, mark);
}

struct Work<'a> {
    sftp: &'a mut Sftp,
    exports: &'a Exports,
    files: &'a mut dyn Files,
    account: &'a Account,
}

impl Work<'_> {
    fn request(&mut self, kind: u8, id: u32, mut reader: Reader, out: &mut Writer) -> Result<(), Refusal> {
        let bad = Refusal(BAD_MESSAGE, "malformed request");
        match kind {
            OPEN => {
                let name = reader.text().ok_or(bad)?;
                let flags = reader.u32().ok_or(Refusal(BAD_MESSAGE, "malformed request"))?;
                let attributes = read_attributes(&mut reader).ok_or(Refusal(BAD_MESSAGE, "malformed request"))?;
                self.open(id, name, flags, attributes, out)
            }
            CLOSE => {
                let slot = self.slot(reader.string().ok_or(bad)?)?;
                let was_writing = self.sftp.handles[slot].write;
                self.sftp.handles[slot] = FREE;
                if was_writing {
                    self.files.sync()?;
                }
                done(out, id)
            }
            READ => {
                let handle = reader.string().ok_or(Refusal(BAD_MESSAGE, "malformed request"))?;
                let offset = reader.u64().ok_or(Refusal(BAD_MESSAGE, "malformed request"))?;
                let wanted = reader.u32().ok_or(Refusal(BAD_MESSAGE, "malformed request"))? as usize;
                self.read(id, handle, offset, wanted, out)
            }
            WRITE => {
                let handle = reader.string().ok_or(Refusal(BAD_MESSAGE, "malformed request"))?;
                let offset = reader.u64().ok_or(Refusal(BAD_MESSAGE, "malformed request"))?;
                let data = reader.string().ok_or(Refusal(BAD_MESSAGE, "malformed request"))?;
                self.write(handle, offset, data)?;
                done(out, id)
            }
            LSTAT | STAT => {
                let name = reader.text().ok_or(bad)?;
                let mut buffer = [0u8; PATH_MAX];
                let (stat, read_only) = match resolve(self.exports, name, &mut buffer).ok_or(MISSING)? {
                    Spot::Top => (TOP, true),
                    Spot::In(export, path) => (self.files.stat(self.exports.root(export), path).ok_or(MISSING)?, self.exports.read_only(export) || self.account.read_only),
                };
                let mark = begin(out, ATTRS);
                out.u32(id);
                put_attributes(out, &stat, read_only);
                end(out, mark);
                Ok(())
            }
            FSTAT => {
                let slot = self.slot(reader.string().ok_or(bad)?)?;
                let handle = self.sftp.handles[slot];
                let (stat, read_only) = if handle.export == EXPORTS {
                    (TOP, true)
                } else {
                    let path = core::str::from_utf8(&handle.path[..handle.path_length]).unwrap_or("");
                    (self.files.stat(self.exports.root(handle.export), path).ok_or(MISSING)?, self.exports.read_only(handle.export) || self.account.read_only)
                };
                let mark = begin(out, ATTRS);
                out.u32(id);
                put_attributes(out, &stat, read_only);
                end(out, mark);
                Ok(())
            }
            SETSTAT => {
                let name = reader.text().ok_or(Refusal(BAD_MESSAGE, "malformed request"))?;
                let attributes = read_attributes(&mut reader).ok_or(Refusal(BAD_MESSAGE, "malformed request"))?;
                let mut buffer = [0u8; PATH_MAX];
                match resolve(self.exports, name, &mut buffer).ok_or(MISSING)? {
                    Spot::Top => return Err(DENIED),
                    Spot::In(export, path) => self.change(export, path, &attributes)?,
                }
                done(out, id)
            }
            FSETSTAT => {
                let slot = self.slot(reader.string().ok_or(Refusal(BAD_MESSAGE, "malformed request"))?)?;
                let attributes = read_attributes(&mut reader).ok_or(Refusal(BAD_MESSAGE, "malformed request"))?;
                let handle = self.sftp.handles[slot];
                if handle.export == EXPORTS {
                    return Err(DENIED);
                }
                let path = core::str::from_utf8(&handle.path[..handle.path_length]).unwrap_or("");
                self.change(handle.export, path, &attributes)?;
                done(out, id)
            }
            OPENDIR => {
                let name = reader.text().ok_or(bad)?;
                self.open_folder(id, name, out)
            }
            READDIR => {
                let slot = self.slot(reader.string().ok_or(bad)?)?;
                self.read_folder(id, slot, out)
            }
            REMOVE | RMDIR => {
                let name = reader.text().ok_or(bad)?;
                self.remove(name, kind == RMDIR)?;
                done(out, id)
            }
            MKDIR => {
                let name = reader.text().ok_or(Refusal(BAD_MESSAGE, "malformed request"))?;
                let attributes = read_attributes(&mut reader).ok_or(Refusal(BAD_MESSAGE, "malformed request"))?;
                self.make_folder(name, attributes.mode)?;
                done(out, id)
            }
            REALPATH => {
                let name = reader.text().ok_or(bad)?;
                let mut buffer = [0u8; PATH_MAX];
                let cleaned = clean(name, &mut buffer).ok_or(MISSING)?;
                let mut full = [0u8; PATH_MAX + 1];
                full[0] = b'/';
                full[1..=cleaned.len()].copy_from_slice(cleaned.as_bytes());
                let full = &full[..=cleaned.len()];
                let mark = begin(out, NAME);
                out.u32(id).u32(1).string(full).string(full).u32(0);
                end(out, mark);
                Ok(())
            }
            RENAME => {
                let from = reader.text().ok_or(Refusal(BAD_MESSAGE, "malformed request"))?;
                let to = reader.text().ok_or(Refusal(BAD_MESSAGE, "malformed request"))?;
                self.rename(from, to, false)?;
                done(out, id)
            }
            EXTENDED => {
                let name = reader.text().ok_or(bad)?;
                match name {
                    "posix-rename@openssh.com" => {
                        let from = reader.text().ok_or(Refusal(BAD_MESSAGE, "malformed request"))?;
                        let to = reader.text().ok_or(Refusal(BAD_MESSAGE, "malformed request"))?;
                        self.rename(from, to, true)?;
                        done(out, id)
                    }
                    "fsync@openssh.com" => {
                        self.slot(reader.string().ok_or(bad)?)?;
                        self.files.sync()?;
                        done(out, id)
                    }
                    "limits@openssh.com" => {
                        let mark = begin(out, EXTENDED_REPLY);
                        out.u32(id).u64(MAX_PACKET as u64).u64(MAX_IO as u64).u64(MAX_IO as u64).u64(HANDLES as u64);
                        end(out, mark);
                        Ok(())
                    }
                    _ => Err(Refusal(OP_UNSUPPORTED, "operation not supported")),
                }
            }
            _ => Err(Refusal(OP_UNSUPPORTED, "operation not supported")),
        }
    }

    fn slot(&self, handle: &[u8]) -> Result<usize, Refusal> {
        let id = u32::from_be_bytes(handle.try_into().map_err(|_| Refusal(FAILURE, "bad handle"))?);
        let slot = (id & 0xff) as usize;
        match self.sftp.handles.get(slot) {
            Some(entry) if entry.kind != Kind::Free && entry.id == id => Ok(slot),
            _ => Err(Refusal(FAILURE, "bad handle")),
        }
    }

    fn allocate(&mut self, kind: Kind, export: usize, path: &str) -> Result<usize, Refusal> {
        if path.len() > HANDLE_PATH {
            return Err(Refusal(FAILURE, "path too long"));
        }
        let slot = self.sftp.handles.iter().position(|handle| handle.kind == Kind::Free).ok_or(Refusal(FAILURE, "too many open files"))?;
        self.sftp.serial = (self.sftp.serial + 1) & 0x00ff_ffff;
        let handle = &mut self.sftp.handles[slot];
        *handle = FREE;
        handle.id = self.sftp.serial << 8 | slot as u32;
        handle.kind = kind;
        handle.export = export;
        handle.path[..path.len()].copy_from_slice(path.as_bytes());
        handle.path_length = path.len();
        Ok(slot)
    }

    fn handle_reply(&self, id: u32, slot: usize, out: &mut Writer) {
        let mark = begin(out, HANDLE);
        out.u32(id).string(&self.sftp.handles[slot].id.to_be_bytes());
        end(out, mark);
    }

    /// May this login change anything in `export`?
    fn writable(&mut self, export: usize) -> Result<(), Refusal> {
        if self.account.read_only || self.exports.read_only(export) || !self.files.can_change() {
            return Err(Refusal(PERMISSION_DENIED, "read-only"));
        }
        Ok(())
    }

    /// The folder holding `path` must exist and be writable and searchable by this login.
    fn parent_writable(&mut self, export: usize, path: &str) -> Result<(), Refusal> {
        let (parent, _) = split_name(path).ok_or(DENIED)?;
        let stat = self.files.stat(self.exports.root(export), parent).ok_or(MISSING)?;
        if !stat.dir {
            return Err(Refusal(FAILURE, "not a directory"));
        }
        if !stat.allows(self.account.uid, self.account.gid, 3) {
            return Err(DENIED);
        }
        Ok(())
    }

    fn open(&mut self, id: u32, name: &str, flags: u32, attributes: Attributes, out: &mut Writer) -> Result<(), Refusal> {
        let mut buffer = [0u8; PATH_MAX];
        let Spot::In(export, path) = resolve(self.exports, name, &mut buffer).ok_or(MISSING)? else { return Err(Refusal(FAILURE, "is a directory")) };
        let root = self.exports.root(export);
        let (uid, gid) = (self.account.uid, self.account.gid);
        let writing = flags & (OPEN_WRITE | OPEN_APPEND | OPEN_CREATE | OPEN_TRUNCATE) != 0;
        if writing {
            self.writable(export)?;
        }
        match self.files.stat(root, path) {
            Some(stat) => {
                if flags & OPEN_CREATE != 0 && flags & OPEN_EXCLUSIVE != 0 {
                    return Err(Refusal(FAILURE, "file exists"));
                }
                if stat.dir {
                    return Err(Refusal(FAILURE, "is a directory"));
                }
                if (flags & OPEN_READ != 0 || !writing) && !stat.allows(uid, gid, 4) || writing && !stat.allows(uid, gid, 2) {
                    return Err(DENIED);
                }
                if flags & OPEN_TRUNCATE != 0 {
                    self.files.truncate(root, path, 0)?;
                }
            }
            None => {
                if flags & OPEN_CREATE == 0 {
                    return Err(MISSING);
                }
                self.parent_writable(export, path)?;
                self.files.create(root, path, false, attributes.mode.unwrap_or(0o644) & 0o777, uid, gid)?;
            }
        }
        let slot = self.allocate(Kind::File, export, path)?;
        let handle = &mut self.sftp.handles[slot];
        handle.read = flags & OPEN_READ != 0 || !writing;
        handle.write = writing;
        handle.append = flags & OPEN_APPEND != 0;
        self.handle_reply(id, slot, out);
        Ok(())
    }

    fn read(&mut self, id: u32, handle: &[u8], offset: u64, wanted: usize, out: &mut Writer) -> Result<(), Refusal> {
        let slot = self.slot(handle)?;
        let handle = self.sftp.handles[slot];
        if handle.kind != Kind::File || !handle.read {
            return Err(Refusal(FAILURE, "not open for reading"));
        }
        let mark = begin(out, DATA);
        out.u32(id);
        let data_length = out.len();
        out.u32(0);
        let room = out.spare().len().min(wanted).min(MAX_IO);
        let path = core::str::from_utf8(&handle.path[..handle.path_length]).unwrap_or("");
        let count = self.files.read(self.exports.root(handle.export), path, offset, &mut out.spare()[..room])?;
        if count == 0 {
            out.truncate(mark);
            return Err(Refusal(EOF, "end of file"));
        }
        out.advance(count);
        out.patch_u32(data_length, count as u32);
        end(out, mark);
        Ok(())
    }

    fn write(&mut self, handle: &[u8], offset: u64, data: &[u8]) -> Result<(), Refusal> {
        let slot = self.slot(handle)?;
        let handle = self.sftp.handles[slot];
        if handle.kind != Kind::File || !handle.write {
            return Err(Refusal(FAILURE, "not open for writing"));
        }
        let root = self.exports.root(handle.export);
        let path = core::str::from_utf8(&handle.path[..handle.path_length]).unwrap_or("");
        let offset = if handle.append { self.files.stat(root, path).ok_or(MISSING)?.size } else { offset };
        self.files.write(root, path, offset, data)?;
        Ok(())
    }

    fn open_folder(&mut self, id: u32, name: &str, out: &mut Writer) -> Result<(), Refusal> {
        let mut buffer = [0u8; PATH_MAX];
        let slot = match resolve(self.exports, name, &mut buffer).ok_or(MISSING)? {
            Spot::Top => self.allocate(Kind::Folder, EXPORTS, "")?,
            Spot::In(export, path) => {
                let stat = self.files.stat(self.exports.root(export), path).ok_or(MISSING)?;
                if !stat.dir {
                    return Err(Refusal(FAILURE, "not a directory"));
                }
                if !stat.allows(self.account.uid, self.account.gid, 4) {
                    return Err(DENIED);
                }
                self.allocate(Kind::Folder, export, path)?
            }
        };
        self.handle_reply(id, slot, out);
        Ok(())
    }

    fn read_folder(&mut self, id: u32, slot: usize, out: &mut Writer) -> Result<(), Refusal> {
        let handle = self.sftp.handles[slot];
        if handle.kind != Kind::Folder {
            return Err(Refusal(FAILURE, "not a folder"));
        }
        let mark = begin(out, NAME);
        out.u32(id);
        let count_at = out.len();
        out.u32(0);
        let (mut count, mut cursor) = (0u32, handle.cursor);
        let mut line = Line { bytes: [0; 400], length: 0 };
        let mut name = [0u8; 256];
        while out.len() < 24_000 {
            let (length, stat, read_only) = if handle.export == EXPORTS {
                if cursor >= self.exports.len() {
                    break;
                }
                let text = self.exports.name(cursor).as_bytes();
                name[..text.len()].copy_from_slice(text);
                (text.len(), TOP, self.exports.read_only(cursor))
            } else {
                let path = core::str::from_utf8(&handle.path[..handle.path_length]).unwrap_or("");
                let Some((length, stat)) = self.files.read_dir(self.exports.root(handle.export), path, cursor, &mut name) else { break };
                (length, stat, self.exports.read_only(handle.export) || self.account.read_only)
            };
            cursor += 1;
            long_name(&name[..length], &stat, read_only, &mut line);
            out.string(&name[..length]).string(&line.bytes[..line.length]);
            put_attributes(out, &stat, read_only);
            count += 1;
        }
        self.sftp.handles[slot].cursor = cursor;
        if count == 0 {
            out.truncate(mark);
            return Err(Refusal(EOF, "end of directory"));
        }
        out.patch_u32(count_at, count);
        end(out, mark);
        Ok(())
    }

    fn remove(&mut self, name: &str, folder: bool) -> Result<(), Refusal> {
        let mut buffer = [0u8; PATH_MAX];
        let Spot::In(export, path) = resolve(self.exports, name, &mut buffer).ok_or(MISSING)? else { return Err(DENIED) };
        self.writable(export)?;
        let root = self.exports.root(export);
        let stat = self.files.stat(root, path).ok_or(MISSING)?;
        if stat.dir != folder {
            return Err(Refusal(FAILURE, if folder { "not a directory" } else { "is a directory" }));
        }
        self.parent_writable(export, path)?;
        self.files.remove(root, path)?;
        Ok(())
    }

    fn make_folder(&mut self, name: &str, mode: Option<u32>) -> Result<(), Refusal> {
        let mut buffer = [0u8; PATH_MAX];
        let Spot::In(export, path) = resolve(self.exports, name, &mut buffer).ok_or(MISSING)? else { return Err(Refusal(FAILURE, "file exists")) };
        self.writable(export)?;
        let root = self.exports.root(export);
        if self.files.stat(root, path).is_some() {
            return Err(Refusal(FAILURE, "file exists"));
        }
        self.parent_writable(export, path)?;
        self.files.create(root, path, true, mode.unwrap_or(0o755) & 0o777, self.account.uid, self.account.gid)?;
        Ok(())
    }

    fn rename(&mut self, from: &str, to: &str, replace: bool) -> Result<(), Refusal> {
        let (mut first, mut second) = ([0u8; PATH_MAX], [0u8; PATH_MAX]);
        let Spot::In(export, source) = resolve(self.exports, from, &mut first).ok_or(MISSING)? else { return Err(DENIED) };
        let Spot::In(other, target) = resolve(self.exports, to, &mut second).ok_or(MISSING)? else { return Err(DENIED) };
        if export != other {
            return Err(Refusal(FAILURE, "cannot move between exports"));
        }
        self.writable(export)?;
        let root = self.exports.root(export);
        self.files.stat(root, source).ok_or(MISSING)?;
        if !replace && self.files.stat(root, target).is_some() {
            return Err(Refusal(FAILURE, "file exists"));
        }
        self.parent_writable(export, source)?;
        self.parent_writable(export, target)?;
        self.files.rename(root, source, target)?;
        Ok(())
    }

    /// SETSTAT: size (truncate), permission bits and owner are applied; times are accepted and ignored.
    fn change(&mut self, export: usize, path: &str, attributes: &Attributes) -> Result<(), Refusal> {
        if attributes.size.is_none() && attributes.owner.is_none() && attributes.mode.is_none() {
            return Ok(());
        }
        self.writable(export)?;
        let root = self.exports.root(export);
        let stat = self.files.stat(root, path).ok_or(MISSING)?;
        let (uid, gid) = (self.account.uid, self.account.gid);
        if let Some(size) = attributes.size {
            if stat.dir {
                return Err(Refusal(FAILURE, "is a directory"));
            }
            if !stat.allows(uid, gid, 2) {
                return Err(DENIED);
            }
            self.files.truncate(root, path, size)?;
        }
        if attributes.owner.is_some() && uid != 0 {
            return Err(DENIED);
        }
        if attributes.mode.is_some() && uid != 0 && stat.mode != 0 && uid != stat.uid {
            return Err(DENIED);
        }
        if attributes.owner.is_some() || attributes.mode.is_some() {
            self.files.set_attributes(root, path, attributes.owner, attributes.mode.map(|mode| mode & 0o7777))?;
        }
        Ok(())
    }
}

fn done(out: &mut Writer, id: u32) -> Result<(), Refusal> {
    status(out, id, OK, "");
    Ok(())
}
