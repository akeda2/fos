//! What smbd and sshd (SFTP) both do with a served folder: the export's `root` (any reference `vfs_*` takes: `@LABEL/dir`,
//! `diskN`, a folder of the boot volume) plus a client path become one reference, and the stat, listing and change calls
//! answer with one `Stat` and one `Fail`, whatever the protocol. The protocols keep their own wire formats, access rules and
//! caching; this crate has no state.
#![cfg_attr(not(test), no_std)]

use tape_rt::{VFS_DIR, VFS_FILE};

/// The longest reference (root, slash, client path) these calls build.
pub const PATH_MAX: usize = 512;

/// Why a call failed, in protocol-neutral terms.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Fail {
    Denied,
    Exists,
    NotFound,
    NotEmpty,
    NoSpace,
    IsDir,
    Io,
}

impl Fail {
    /// The `Fail` a tape ABI `FS_*` code stands for.
    pub fn from_code(code: isize) -> Fail {
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
}

/// What the volume knows about a file or folder.
#[derive(Clone, Copy)]
pub struct Stat {
    pub dir: bool,
    pub size: u64,
    /// Modification time, seconds since 1970 (0: unknown).
    pub mtime: u64,
    /// Permission bits (`0o644`); 0 means the volume keeps none and nothing is checked.
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
}

impl Stat {
    pub fn of(stat: &tape_rt::VfsStat) -> Stat {
        Stat { dir: stat.kind == VFS_DIR, size: stat.size, mtime: stat.mtime, mode: stat.mode & 0o777, uid: stat.uid, gid: stat.gid }
    }
}

impl Stat {
    /// May `uid`/`gid` do `want` (4 read, 2 write, 1 search) with this entry? A volume without permission bits (`mode` 0)
    /// and uid 0 are never refused.
    pub fn allows(&self, uid: u32, gid: u32, want: u32) -> bool {
        if self.mode == 0 || uid == 0 {
            return true;
        }
        let bits = if uid == self.uid {
            self.mode >> 6
        } else if gid == self.gid {
            self.mode >> 3
        } else {
            self.mode
        };
        bits & want == want
    }
}

/// `root` and `path` joined with a slash into `out`; `path` must be clean (no `.` or `..`, see `clean`).
pub fn join<'a>(root: &str, path: &str, out: &'a mut [u8; PATH_MAX]) -> Option<&'a str> {
    if root.len() + 1 + path.len() > out.len() {
        return None;
    }
    out[..root.len()].copy_from_slice(root.as_bytes());
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

/// A client path (`/a/./b/../c`) as a clean relative path (`a/c`): `.` and empty parts vanish, `..` removes the part before it
/// and stops at the root, so no path leaves the export. `None` when it does not fit `out`.
pub fn clean<'a>(path: &str, out: &'a mut [u8; PATH_MAX]) -> Option<&'a str> {
    let mut length = 0usize;
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => length = out[..length].iter().rposition(|&byte| byte == b'/').unwrap_or(0),
            name => {
                let slash = usize::from(length > 0);
                if length + slash + name.len() > out.len() {
                    return None;
                }
                if slash == 1 {
                    out[length] = b'/';
                }
                out[length + slash..length + slash + name.len()].copy_from_slice(name.as_bytes());
                length += slash + name.len();
            }
        }
    }
    core::str::from_utf8(&out[..length]).ok()
}

pub fn stat(root: &str, path: &str) -> Option<Stat> {
    let mut buffer = [0u8; PATH_MAX];
    tape_rt::vfs_stat(join(root, path, &mut buffer)?).ok().map(|stat| Stat::of(&stat))
}

/// The `index`th entry of folder `path` (never `.` or `..`): its name is copied to `name` (cut to fit) and its length and
/// `Stat` returned; `None` past the end or when the folder cannot be read.
pub fn read_dir(root: &str, path: &str, index: usize, name: &mut [u8]) -> Option<(usize, Stat)> {
    let mut buffer = [0u8; PATH_MAX];
    let entry = tape_rt::vfs_read_dir(join(root, path, &mut buffer)?, index).ok()??;
    let length = entry.name().len().min(name.len());
    name[..length].copy_from_slice(&entry.name()[..length]);
    Some((length, Stat::of(&entry.stat)))
}

/// True when the volume behind the exports can be changed at all (the kernel has the ABI 34 calls).
pub fn can_change() -> bool {
    tape_rt::vfs_can_change()
}

pub fn create(root: &str, path: &str, dir: bool, mode: u32, uid: u32, gid: u32) -> Result<(), Fail> {
    let mut buffer = [0u8; PATH_MAX];
    let reference = join(root, path, &mut buffer).ok_or(Fail::Io)?;
    tape_rt::vfs_create(reference, if dir { VFS_DIR } else { VFS_FILE }, mode, uid, gid).map_err(Fail::from_code)
}

/// Writes all of `data` at `offset` (the kernel may take it in pieces).
pub fn write(root: &str, path: &str, offset: u64, data: &[u8]) -> Result<(), Fail> {
    let mut buffer = [0u8; PATH_MAX];
    let reference = join(root, path, &mut buffer).ok_or(Fail::Io)?;
    let mut done = 0;
    while done < data.len() {
        let count = tape_rt::vfs_write(reference, offset + done as u64, &data[done..]).map_err(Fail::from_code)?;
        if count == 0 {
            return Err(Fail::Io);
        }
        done += count;
    }
    Ok(())
}

pub fn read(root: &str, path: &str, offset: u64, out: &mut [u8]) -> Result<usize, Fail> {
    let mut buffer = [0u8; PATH_MAX];
    tape_rt::vfs_read(join(root, path, &mut buffer).ok_or(Fail::Io)?, offset, out).map_err(Fail::from_code)
}

pub fn truncate(root: &str, path: &str, size: u64) -> Result<(), Fail> {
    let mut buffer = [0u8; PATH_MAX];
    tape_rt::vfs_truncate(join(root, path, &mut buffer).ok_or(Fail::Io)?, size).map_err(Fail::from_code)
}

pub fn remove(root: &str, path: &str) -> Result<(), Fail> {
    let mut buffer = [0u8; PATH_MAX];
    tape_rt::vfs_remove(join(root, path, &mut buffer).ok_or(Fail::Io)?).map_err(Fail::from_code)
}

/// Sets the permission bits and/or the owner; `None` keeps what is there.
pub fn set_attributes(root: &str, path: &str, owner: Option<(u32, u32)>, mode: Option<u32>) -> Result<(), Fail> {
    let mut buffer = [0u8; PATH_MAX];
    let reference = join(root, path, &mut buffer).ok_or(Fail::Io)?;
    let (uid, gid) = match owner {
        Some(owner) => owner,
        None => {
            let stat = tape_rt::vfs_stat(reference).map_err(Fail::from_code)?;
            (stat.uid, stat.gid)
        }
    };
    tape_rt::vfs_set_owner(reference, uid, gid, mode.unwrap_or(tape_rt::VFS_KEEP_MODE)).map_err(Fail::from_code)
}

/// Commits writes the kernel still holds back (see `tape_rt::vfs_sync`).
pub fn sync() -> Result<(), Fail> {
    tape_rt::vfs_sync().map_err(Fail::from_code)
}

pub fn rename(root: &str, from: &str, to: &str) -> Result<(), Fail> {
    let (mut first, mut second) = ([0u8; PATH_MAX], [0u8; PATH_MAX]);
    let from = join(root, from, &mut first).ok_or(Fail::Io)?;
    let to = join(root, to, &mut second).ok_or(Fail::Io)?;
    tape_rt::vfs_rename(from, to).map_err(Fail::from_code)
}

/// The calls a server makes on its folder tree, so a protocol can be tested against a fake one. `Vfs` is the real thing.
pub trait Files {
    fn stat(&mut self, root: &str, path: &str) -> Option<Stat>;
    fn read_dir(&mut self, root: &str, path: &str, index: usize, name: &mut [u8]) -> Option<(usize, Stat)>;
    fn can_change(&mut self) -> bool;
    fn create(&mut self, root: &str, path: &str, dir: bool, mode: u32, uid: u32, gid: u32) -> Result<(), Fail>;
    fn write(&mut self, root: &str, path: &str, offset: u64, data: &[u8]) -> Result<(), Fail>;
    fn read(&mut self, root: &str, path: &str, offset: u64, out: &mut [u8]) -> Result<usize, Fail>;
    fn truncate(&mut self, root: &str, path: &str, size: u64) -> Result<(), Fail>;
    fn remove(&mut self, root: &str, path: &str) -> Result<(), Fail>;
    fn rename(&mut self, root: &str, from: &str, to: &str) -> Result<(), Fail>;
    fn set_attributes(&mut self, root: &str, path: &str, owner: Option<(u32, u32)>, mode: Option<u32>) -> Result<(), Fail>;
    fn sync(&mut self) -> Result<(), Fail>;
}

/// The tape `vfs_*` calls.
pub struct Vfs;

impl Files for Vfs {
    fn stat(&mut self, root: &str, path: &str) -> Option<Stat> {
        stat(root, path)
    }
    fn read_dir(&mut self, root: &str, path: &str, index: usize, name: &mut [u8]) -> Option<(usize, Stat)> {
        read_dir(root, path, index, name)
    }
    fn can_change(&mut self) -> bool {
        can_change()
    }
    fn create(&mut self, root: &str, path: &str, dir: bool, mode: u32, uid: u32, gid: u32) -> Result<(), Fail> {
        create(root, path, dir, mode, uid, gid)
    }
    fn write(&mut self, root: &str, path: &str, offset: u64, data: &[u8]) -> Result<(), Fail> {
        write(root, path, offset, data)
    }
    fn read(&mut self, root: &str, path: &str, offset: u64, out: &mut [u8]) -> Result<usize, Fail> {
        read(root, path, offset, out)
    }
    fn truncate(&mut self, root: &str, path: &str, size: u64) -> Result<(), Fail> {
        truncate(root, path, size)
    }
    fn remove(&mut self, root: &str, path: &str) -> Result<(), Fail> {
        remove(root, path)
    }
    fn rename(&mut self, root: &str, from: &str, to: &str) -> Result<(), Fail> {
        rename(root, from, to)
    }
    fn set_attributes(&mut self, root: &str, path: &str, owner: Option<(u32, u32)>, mode: Option<u32>) -> Result<(), Fail> {
        set_attributes(root, path, owner, mode)
    }
    fn sync(&mut self) -> Result<(), Fail> {
        sync()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cleaned(path: &str) -> Option<String> {
        let mut out = [0u8; PATH_MAX];
        clean(path, &mut out).map(str::to_owned)
    }

    #[test]
    fn clean_paths_never_leave_the_root() {
        assert_eq!(cleaned("/a/./b//c/").as_deref(), Some("a/b/c"));
        assert_eq!(cleaned("/a/b/../c").as_deref(), Some("a/c"));
        assert_eq!(cleaned("/../../etc").as_deref(), Some("etc"));
        assert_eq!(cleaned("a/..").as_deref(), Some(""));
        assert_eq!(cleaned("/").as_deref(), Some(""));
        assert_eq!(cleaned(&"x/".repeat(400)), None);
    }

    #[test]
    fn join_adds_one_slash() {
        let mut out = [0u8; PATH_MAX];
        assert_eq!(join("@rackdata/media", "a/b", &mut out), Some("@rackdata/media/a/b"));
        assert_eq!(join("@rackdata", "", &mut out), Some("@rackdata"));
        assert_eq!(join("", "a", &mut out), Some("a"));
    }
}
