//! A host-side SMB server for tests: `cargo run --example serve -- DIR PORT [SHARE]` shares DIR (read-only, guest) as
//! //HOST/SHARE (default `share`) on 127.0.0.1:PORT, one connection at a time per thread. `tools/test_smb_client.py` points the
//! `smb` tape at it through QEMU's guestfwd. With `SMB_LOGIN=user:password` in the environment only that user may log in.
use smb::ntlm::nt_hash;
use smb::server::{Account, Identity, Server, Share, Stat};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::time::UNIX_EPOCH;

struct Folder {
    root: PathBuf,
    name: String,
    login: Option<(String, String)>,
}

fn stat_of(meta: &fs::Metadata) -> Stat {
    let mtime = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
    Stat { dir: meta.is_dir(), size: if meta.is_dir() { 0 } else { meta.len() }, mtime, ..Stat::default() }
}

impl Share for Folder {
    fn name(&self) -> &str {
        &self.name
    }
    fn account(&mut self, user: &str) -> Option<Account> {
        let (name, password) = self.login.as_ref()?;
        name.eq_ignore_ascii_case(user).then(|| Account { nt_hash: nt_hash(password), identity: Identity { uid: 1000, gid: 1000 } })
    }
    fn guest(&mut self) -> Option<Identity> {
        self.login.is_none().then_some(Identity { uid: 65534, gid: 65534 })
    }
    fn stat(&mut self, path: &str) -> Option<Stat> {
        fs::metadata(self.root.join(path)).ok().map(|meta| stat_of(&meta))
    }
    fn read_dir(&mut self, path: &str, index: usize, name: &mut [u8]) -> Option<(usize, Stat)> {
        let mut entries: Vec<_> = fs::read_dir(self.root.join(path)).ok()?.filter_map(Result::ok).collect();
        entries.sort_by_key(|entry| entry.file_name());
        let entry = entries.get(index)?;
        let text = entry.file_name().to_string_lossy().into_owned();
        let length = text.len().min(name.len());
        name[..length].copy_from_slice(&text.as_bytes()[..length]);
        Some((length, stat_of(&entry.metadata().ok()?)))
    }
    fn read(&mut self, path: &str, offset: u64, out: &mut [u8]) -> Result<usize, ()> {
        let mut file = File::open(self.root.join(path)).map_err(|_| ())?;
        file.seek(SeekFrom::Start(offset)).map_err(|_| ())?;
        file.read(out).map_err(|_| ())
    }
}

fn connection(mut stream: TcpStream, root: PathBuf, name: String) {
    let login = std::env::var("SMB_LOGIN").ok().and_then(|text| text.split_once(':').map(|(user, password)| (user.to_string(), password.to_string())));
    let mut server = Box::new(Server::<Folder, 16>::new(Folder { root, name, login }));
    let nanos = std::time::SystemTime::now().duration_since(UNIX_EPOCH).map_or(1, |d| d.as_nanos() as u64);
    server.challenge = nanos.wrapping_mul(0x9E37_79B9_7F4A_7C15).to_le_bytes();
    let mut out = vec![0u8; 70_000 + 200];
    loop {
        let mut header = [0u8; 4];
        if stream.read_exact(&mut header).is_err() {
            return;
        }
        let Some(length) = smb::wire::frame_length(&header) else { return };
        let mut request = vec![0u8; length];
        if stream.read_exact(&mut request).is_err() {
            return;
        }
        let answer = server.handle(&request, &mut out);
        if answer > 0 && stream.write_all(&out[..answer]).is_err() {
            return;
        }
    }
}

fn main() {
    let mut arguments = std::env::args().skip(1);
    let root = PathBuf::from(arguments.next().expect("DIR"));
    let port: u16 = arguments.next().expect("PORT").parse().expect("PORT is a number");
    let name = arguments.next().unwrap_or_else(|| "share".into());
    let listener = TcpListener::bind(("127.0.0.1", port)).expect("bind");
    println!("serving {} on 127.0.0.1:{port}", root.display());
    for stream in listener.incoming().flatten() {
        let (root, name) = (root.clone(), name.clone());
        std::thread::spawn(move || connection(stream, root, name));
    }
}
