//! A host directory behind `Files`, and the `Host` around it: shared by `serve` and the loopback test.
#![allow(dead_code)]
use ssh::authorized::AuthorizedKeys;
use ssh::{Account, Exports, Host};
use std::io::Read;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;
use vfs_export::{Fail, Files, Stat};

pub struct StdFiles;

fn host_path(root: &str, path: &str) -> PathBuf {
    PathBuf::from(root).join(path)
}

fn stat_of(meta: &std::fs::Metadata) -> Stat {
    Stat { dir: meta.is_dir(), size: meta.len(), mtime: meta.mtime() as u64, mode: meta.mode() & 0o777, uid: meta.uid(), gid: meta.gid() }
}

fn fail(error: std::io::Error) -> Fail {
    use std::io::ErrorKind::*;
    match error.kind() {
        NotFound => Fail::NotFound,
        PermissionDenied => Fail::Denied,
        AlreadyExists => Fail::Exists,
        _ => Fail::Io,
    }
}

impl Files for StdFiles {
    fn stat(&mut self, root: &str, path: &str) -> Option<Stat> {
        std::fs::metadata(host_path(root, path)).ok().map(|meta| stat_of(&meta))
    }
    fn read_dir(&mut self, root: &str, path: &str, index: usize, name: &mut [u8]) -> Option<(usize, Stat)> {
        let mut entries: Vec<_> = std::fs::read_dir(host_path(root, path)).ok()?.filter_map(Result::ok).collect();
        entries.sort_by_key(|entry| entry.file_name());
        let entry = entries.get(index)?;
        let text = entry.file_name();
        let bytes = text.as_encoded_bytes();
        name[..bytes.len()].copy_from_slice(bytes);
        Some((bytes.len(), stat_of(&entry.metadata().ok()?)))
    }
    fn can_change(&mut self) -> bool {
        true
    }
    fn create(&mut self, root: &str, path: &str, dir: bool, mode: u32, _uid: u32, _gid: u32) -> Result<(), Fail> {
        let target = host_path(root, path);
        if dir {
            std::fs::create_dir(&target).map_err(fail)?;
        } else {
            std::fs::OpenOptions::new().write(true).create_new(true).open(&target).map_err(fail)?;
        }
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode)).map_err(fail)
    }
    fn write(&mut self, root: &str, path: &str, offset: u64, data: &[u8]) -> Result<(), Fail> {
        use std::os::unix::fs::FileExt;
        std::fs::OpenOptions::new().write(true).open(host_path(root, path)).map_err(fail)?.write_all_at(data, offset).map_err(fail)
    }
    fn read(&mut self, root: &str, path: &str, offset: u64, out: &mut [u8]) -> Result<usize, Fail> {
        use std::os::unix::fs::FileExt;
        std::fs::File::open(host_path(root, path)).map_err(fail)?.read_at(out, offset).map_err(fail)
    }
    fn truncate(&mut self, root: &str, path: &str, size: u64) -> Result<(), Fail> {
        std::fs::OpenOptions::new().write(true).open(host_path(root, path)).map_err(fail)?.set_len(size).map_err(fail)
    }
    fn remove(&mut self, root: &str, path: &str) -> Result<(), Fail> {
        let target = host_path(root, path);
        if target.is_dir() { std::fs::remove_dir(target) } else { std::fs::remove_file(target) }.map_err(fail)
    }
    fn rename(&mut self, root: &str, from: &str, to: &str) -> Result<(), Fail> {
        std::fs::rename(host_path(root, from), host_path(root, to)).map_err(fail)
    }
    fn set_attributes(&mut self, root: &str, path: &str, _owner: Option<(u32, u32)>, mode: Option<u32>) -> Result<(), Fail> {
        match mode {
            Some(mode) => std::fs::set_permissions(host_path(root, path), std::fs::Permissions::from_mode(mode)).map_err(fail),
            None => Ok(()),
        }
    }
    fn sync(&mut self) -> Result<(), Fail> {
        Ok(())
    }
}

pub struct Server {
    pub exports: Exports,
    pub keys: AuthorizedKeys,
    pub files: StdFiles,
    pub random: std::fs::File,
}

impl Host for Server {
    fn random(&mut self, out: &mut [u8]) {
        self.random.read_exact(out).unwrap();
    }
    fn host_seed(&self) -> [u8; 32] {
        [42; 32]
    }
    fn authorize(&mut self, user: &str, key: &[u8; 32]) -> Option<Account> {
        self.keys.lookup(user, key)
    }
    fn split(&mut self) -> (&Exports, &mut dyn Files) {
        (&self.exports, &mut self.files)
    }
    fn log(&mut self, text: core::fmt::Arguments) {
        eprintln!("serve: {text}");
    }
}

impl Server {
    pub fn new(dir: &str, keys: &str, read_only: bool) -> Server {
        let mut server = Server { exports: Exports::new(), keys: AuthorizedKeys::new(), files: StdFiles, random: std::fs::File::open("/dev/urandom").unwrap() };
        server.exports.add("data", dir, read_only).unwrap();
        server.keys.parse(keys).unwrap();
        server
    }
}
