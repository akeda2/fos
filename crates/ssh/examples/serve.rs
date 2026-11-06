//! The `sshd` tape's protocol code on a host directory, to try the real `ssh`, `sftp` and `sshfs` against it:
//!
//!   cargo run --example serve -- PORT DIR AUTHORIZED_KEYS [ro]
//!
//! DIR is exported as `/`; the host key is derived from a fixed seed (this is a test tool, not a server to leave running).
use ssh::authorized::AuthorizedKeys;
use ssh::{Account, Exports, Host, Session};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;
use vfs_export::{Fail, Files, Stat};

struct StdFiles;

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

struct Server {
    exports: Exports,
    keys: AuthorizedKeys,
    files: StdFiles,
    random: std::fs::File,
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

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (port, dir, keys) = (&args[1], &args[2], &args[3]);
    let read_only = args.get(4).is_some_and(|word| word == "ro");
    let listener = TcpListener::bind(format!("127.0.0.1:{port}")).unwrap();
    eprintln!("serve: listening on 127.0.0.1:{port}, exporting {dir}");
    for stream in listener.incoming() {
        let mut stream = stream.unwrap();
        let (dir, keys) = (dir.clone(), std::fs::read_to_string(keys).unwrap());
        std::thread::spawn(move || {
            let mut host = Server { exports: Exports::new(), keys: AuthorizedKeys::new(), files: StdFiles, random: std::fs::File::open("/dev/urandom").unwrap() };
            host.exports.add("data", &dir, read_only).unwrap();
            host.keys.parse(&keys).unwrap();
            let mut arena = vec![0u8; ssh::ARENA].into_boxed_slice();
            let mut session = Session::new(&mut host, &mut arena);
            let mut input: Vec<u8> = Vec::new();
            let mut chunk = vec![0u8; 65536];
            loop {
                while !session.output().is_empty() {
                    match stream.write(session.output()) {
                        Ok(count) => session.sent(count),
                        Err(_) => return,
                    }
                }
                if session.finished() {
                    return;
                }
                let used = session.feed(&mut host, &input);
                input.drain(..used);
                session.poll(&mut host);
                if used > 0 || !session.output().is_empty() {
                    continue;
                }
                match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => return,
                    Ok(count) => input.extend_from_slice(&chunk[..count]),
                }
            }
        });
    }
}
