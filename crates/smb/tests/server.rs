//! The server half against real clients: our own `Client` in-process, and `smbclient` (when installed) over a loopback
//! TCP connection. The share is a few files in memory.
use smb::ntlm::nt_hash;
use smb::server::{Account, Fail, Identity, Server, Share, Stat};
use smb::{Client, Error, Transport};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;

/// The files, and whether the share has accounts: alice (uid 1000, secret) and bob (uid 2000, hunter2), no guests, and
/// private.txt readable by its owner alice only.
struct Memory(Vec<(&'static str, Option<Vec<u8>>)>, bool);

fn pattern(size: usize) -> Vec<u8> {
    (0..size).map(|i| (i * 7 + i / 251) as u8).collect()
}

fn memory() -> Memory {
    Memory(vec![
        ("readme.txt", Some(b"hello from fos\n".to_vec())),
        ("big.bin", Some(pattern(300_000))),
        ("sub", None),
        ("sub/inner.txt", Some(b"inside".to_vec())),
        ("sub/Åäö.txt", Some(b"unicode".to_vec())),
        ("private.txt", Some(b"for alice".to_vec())),
    ], false)
}

impl Share for Memory {
    fn name(&self) -> &str {
        "fos"
    }
    fn stat(&mut self, path: &str) -> Option<Stat> {
        if path.is_empty() {
            let mode = if self.1 { 0o755 } else { 0 };
            return Some(Stat { dir: true, mode, uid: 1000, gid: 1000, ..Stat::default() });
        }
        let (_, data) = self.0.iter().find(|(name, _)| *name == path)?;
        let mode = match (self.1, path) {
            (false, _) => 0,
            (true, "private.txt") => 0o600,
            (true, _) => 0o755,
        };
        Some(Stat { dir: data.is_none(), size: data.as_ref().map_or(0, |d| d.len() as u64), mtime: 1_700_000_000, mode, uid: 1000, gid: 1000 })
    }
    fn read_dir(&mut self, path: &str, index: usize, name: &mut [u8]) -> Option<(usize, Stat)> {
        let prefix = if path.is_empty() { String::new() } else { format!("{path}/") };
        let (entry, _) = self.0.iter().filter(|(n, _)| n.starts_with(&prefix) && !n[prefix.len()..].contains('/') && n.len() > prefix.len()).nth(index)?;
        let short = &entry[prefix.len()..];
        name[..short.len()].copy_from_slice(short.as_bytes());
        Some((short.len(), self.stat(entry)?))
    }
    fn read(&mut self, path: &str, offset: u64, out: &mut [u8]) -> Result<usize, ()> {
        let (_, data) = self.0.iter().find(|(name, _)| *name == path).ok_or(())?;
        let data = data.as_ref().ok_or(())?;
        let start = (offset as usize).min(data.len());
        let count = out.len().min(data.len() - start);
        out[..count].copy_from_slice(&data[start..start + count]);
        Ok(count)
    }
    fn account(&mut self, user: &str) -> Option<Account> {
        let (password, uid) = match user {
            "alice" => ("secret", 1000),
            "bob" => ("hunter2", 2000),
            _ => return None,
        };
        self.1.then(|| Account { nt_hash: nt_hash(password), identity: Identity { uid, gid: uid } })
    }
    fn guest(&mut self) -> Option<Identity> {
        (!self.1).then_some(Identity { uid: 65534, gid: 65534 })
    }
    fn space(&mut self) -> (u64, u64) {
        (1 << 30, 1 << 29)
    }
    fn writable(&self) -> bool {
        true
    }
    fn create(&mut self, path: &str, dir: bool, _mode: u32, _owner: Identity) -> Result<(), Fail> {
        if self.0.iter().any(|(name, _)| *name == path) {
            return Err(Fail::Exists);
        }
        self.0.push((Box::leak(path.to_string().into_boxed_str()), if dir { None } else { Some(Vec::new()) }));
        Ok(())
    }
    fn write(&mut self, path: &str, offset: u64, data: &[u8]) -> Result<(), Fail> {
        let (_, file) = self.0.iter_mut().find(|(name, _)| *name == path).ok_or(Fail::NotFound)?;
        let file = file.as_mut().ok_or(Fail::IsDir)?;
        let end = offset as usize + data.len();
        if file.len() < end {
            file.resize(end, 0);
        }
        file[offset as usize..end].copy_from_slice(data);
        Ok(())
    }
    fn truncate(&mut self, path: &str, size: u64) -> Result<(), Fail> {
        let (_, file) = self.0.iter_mut().find(|(name, _)| *name == path).ok_or(Fail::NotFound)?;
        file.as_mut().ok_or(Fail::IsDir)?.resize(size as usize, 0);
        Ok(())
    }
    fn remove(&mut self, path: &str) -> Result<(), Fail> {
        let at = self.0.iter().position(|(name, _)| *name == path).ok_or(Fail::NotFound)?;
        let prefix = format!("{path}/");
        if self.0.iter().any(|(name, _)| name.starts_with(&prefix)) {
            return Err(Fail::NotEmpty);
        }
        self.0.remove(at);
        Ok(())
    }
    fn rename(&mut self, from: &str, to: &str) -> Result<(), Fail> {
        let at = self.0.iter().position(|(name, _)| *name == from).ok_or(Fail::NotFound)?;
        self.0.retain(|(name, _)| *name != to);
        let at = self.0.iter().position(|(name, _)| *name == from).unwrap_or(at);
        self.0[at].0 = Box::leak(to.to_string().into_boxed_str());
        Ok(())
    }
}

type Fos = Server<Memory, 8>;

fn serve(server: &mut Fos, stream: &mut TcpStream) {
    let mut out = vec![0u8; 70_000 + 200];
    loop {
        let mut header = [0u8; 4];
        if stream.read_exact(&mut header).is_err() {
            return;
        }
        let length = smb::wire::frame_length(&header).unwrap();
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

struct Loopback {
    server: Box<Fos>,
    out: Vec<u8>,
    incoming: Vec<u8>,
}

impl Transport for Loopback {
    fn send(&mut self, data: &[u8]) -> Result<(), Error> {
        let answer = self.server.handle(&data[4..], &mut self.out);
        self.incoming.extend_from_slice(&self.out[..answer]);
        Ok(())
    }
    fn recv_exact(&mut self, out: &mut [u8]) -> Result<(), Error> {
        if self.incoming.len() < out.len() {
            return Err(Error::Transport);
        }
        out.copy_from_slice(&self.incoming[..out.len()]);
        self.incoming.drain(..out.len());
        Ok(())
    }
}

#[test]
fn our_client_lists_and_reads() {
    let transport = Loopback { server: Box::new(Server::new(memory())), out: vec![0; 70_000 + 200], incoming: Vec::new() };
    let mut client = Box::new(Client::<Loopback, 70_000>::new(transport));
    client.connect("fos", "fos").unwrap();
    let mut names = Vec::new();
    client
        .list("", |entry| {
            let mut name = [0u8; 255];
            let length = entry.name_utf8(&mut name);
            names.push((String::from_utf8_lossy(&name[..length]).into_owned(), entry.size, entry.is_dir()));
        })
        .unwrap();
    names.sort();
    assert!(names.contains(&("readme.txt".into(), 15, false)));
    assert!(names.contains(&("sub".into(), 0, true)));
    assert!(names.contains(&(".".into(), 0, true)));
    let file = client.open("big.bin", false).unwrap();
    let mut got = Vec::new();
    let mut buffer = vec![0u8; 65_536];
    loop {
        let n = client.read(&file, got.len() as u64, &mut buffer).unwrap();
        if n == 0 {
            break;
        }
        got.extend_from_slice(&buffer[..n]);
    }
    assert_eq!(got, pattern(300_000));
    client.close(&file).unwrap();
    assert!(matches!(client.open("nothing", false), Err(Error::Status(0xC000_0034))));
    let sub = client.open("sub", true).unwrap();
    client.close(&sub).unwrap();
}

#[test]
fn smbclient_lists_and_downloads() {
    if Command::new("smbclient").arg("--version").output().is_err() {
        eprintln!("smbclient not installed: skipped");
        return;
    }
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        while let Ok((mut stream, _)) = listener.accept() {
            std::thread::spawn(move || {
                let mut server = Box::new(Server::new(memory()));
                serve(&mut server, &mut stream);
            });
        }
    });
    let dir = std::env::temp_dir().join(format!("smbtest-{port}"));
    std::fs::create_dir_all(&dir).unwrap();
    let script = format!("ls; ls sub\\*; allinfo readme.txt; get big.bin {d}/big.bin; get readme.txt {d}/readme.txt; cd sub; get Åäö.txt {d}/uni.txt; du", d = dir.display());
    for protocol in ["SMB2_02", "NT1", "SMB3_00"] {
        let output = Command::new("smbclient")
            .args(["-N", "-p", &port.to_string(), "//127.0.0.1/fos", "-m", protocol, "-c", &script])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&output.stdout).into_owned() + &String::from_utf8_lossy(&output.stderr);
        println!("--- {protocol}\n{text}");
        if protocol == "NT1" {
            continue; // SMB1 only: the server must refuse, not hang
        }
        assert!(output.status.success(), "{protocol}: {text}");
        assert!(text.contains("readme.txt") && text.contains("inner.txt"), "{protocol}: {text}");
        assert_eq!(std::fs::read(dir.join("big.bin")).unwrap(), pattern(300_000), "{protocol}");
        assert_eq!(std::fs::read(dir.join("readme.txt")).unwrap(), b"hello from fos\n");
        assert_eq!(std::fs::read(dir.join("uni.txt")).unwrap(), b"unicode");
        std::fs::remove_file(dir.join("big.bin")).unwrap();
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn logins_are_checked() {
    if Command::new("smbclient").arg("--version").output().is_err() {
        eprintln!("smbclient not installed: skipped");
        return;
    }
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        while let Ok((mut stream, _)) = listener.accept() {
            std::thread::spawn(move || {
                let mut share = memory();
                share.1 = true;
                let mut server = Box::new(Server::new(share));
                serve(&mut server, &mut stream);
            });
        }
    });
    let run = |login: &[&str], script: &str| {
        let output = Command::new("smbclient")
            .args(login)
            .args(["-p", &port.to_string(), "//127.0.0.1/fos", "-m", "SMB3_00", "-c", script])
            .output()
            .unwrap();
        (output.status.success(), String::from_utf8_lossy(&output.stdout).into_owned() + &String::from_utf8_lossy(&output.stderr))
    };
    let dir = std::env::temp_dir().join(format!("smbauth-{port}"));
    std::fs::create_dir_all(&dir).unwrap();
    let get = format!("get private.txt {}/p.txt", dir.display());

    let (ok, text) = run(&["-U", "alice%secret"], &get);
    assert!(ok, "alice: {text}");
    assert_eq!(std::fs::read(dir.join("p.txt")).unwrap(), b"for alice");
    std::fs::remove_file(dir.join("p.txt")).unwrap();

    // SMB 2.0.2 signs with HMAC-SHA-256 instead of AES-CMAC.
    let output = Command::new("smbclient").args(["-U", "alice%secret", "-p", &port.to_string(), "//127.0.0.1/fos", "-m", "SMB2_02", "-c", "ls"]).output().unwrap();
    assert!(output.status.success(), "SMB2_02: {}", String::from_utf8_lossy(&output.stderr));

    let (ok, text) = run(&["-U", "bob%hunter2"], "ls");
    assert!(ok && text.contains("readme.txt"), "bob lists: {text}");
    let (_, text) = run(&["-U", "bob%hunter2"], &get);
    assert!(text.contains("ACCESS_DENIED") && !dir.join("p.txt").exists(), "bob must not read private.txt: {text}");

    let (ok, text) = run(&["-U", "alice%wrong"], "ls");
    assert!(!ok && text.contains("LOGON_FAILURE"), "wrong password: {text}");
    let (ok, text) = run(&["-U", "mallory%secret"], "ls");
    assert!(!ok, "unknown user without guests: {text}");
    let (ok, text) = run(&["-N"], "ls");
    assert!(!ok, "anonymous without guests: {text}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn our_client_logs_in() {
    let accounts = |password: &str, user: &str| {
        let mut share = memory();
        share.1 = true;
        let mut server = Box::new(Server::new(share));
        server.challenge = *b"\x01\x23\x45\x67\x89\xab\xcd\xef";
        let mut client = Box::new(Client::<Loopback, 70_000>::new(Loopback { server, out: vec![0; 70_000 + 200], incoming: Vec::new() }));
        client.login(user, password, *b"clientnc").unwrap();
        (client.connect("fos", "fos"), client)
    };
    let (result, mut client) = accounts("secret", "alice");
    result.unwrap();
    let file = client.open("private.txt", false).unwrap();
    let mut data = [0u8; 64];
    let count = client.read(&file, 0, &mut data).unwrap();
    assert_eq!(&data[..count], b"for alice");
    assert_eq!(accounts("wrong", "alice").0, Err(Error::Status(0xC000_006D)));
    assert!(accounts("hunter2", "bob").0.is_ok());
}

struct Tcp(TcpStream);

impl Transport for Tcp {
    fn send(&mut self, data: &[u8]) -> Result<(), Error> {
        self.0.write_all(data).map_err(|_| Error::Transport)
    }
    fn recv_exact(&mut self, out: &mut [u8]) -> Result<(), Error> {
        self.0.read_exact(out).map_err(|_| Error::Transport)
    }
}

#[test]
fn our_client_logs_in_over_tcp_with_reads() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut share = memory();
        share.1 = true;
        let mut server = Box::new(Server::new(share));
        serve(&mut server, &mut stream);
    });
    let mut client = Box::new(Client::<Tcp, { 64 * 1024 + 1024 }>::new(Tcp(TcpStream::connect(("127.0.0.1", port)).unwrap())));
    client.login("alice", "secret", *b"12345678").unwrap();
    client.connect("127.0.0.1", "fos").unwrap();
    let file = client.open("big.bin", false).unwrap();
    let mut data = vec![0u8; 65536];
    let mut offset = 0;
    while offset < file.size {
        let count = client.read(&file, offset, &mut data).unwrap();
        assert_eq!(&data[..count], &pattern(300_000)[offset as usize..offset as usize + count]);
        offset += count as u64;
    }
}

#[test]
fn smbclient_changes_files() {
    if Command::new("smbclient").arg("--version").output().is_err() {
        eprintln!("smbclient not installed: skipped");
        return;
    }
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        while let Ok((mut stream, _)) = listener.accept() {
            std::thread::spawn(move || {
                let mut share = memory();
                share.1 = true;
                let mut server = Box::new(Server::new(share));
                serve(&mut server, &mut stream);
            });
        }
    });
    let run = |user: &str, script: &str| {
        let output = Command::new("smbclient")
            .args(["-U", user, "-p", &port.to_string(), "//127.0.0.1/fos", "-m", "SMB3_00", "-c", script])
            .output()
            .unwrap();
        (output.status.success(), String::from_utf8_lossy(&output.stdout).into_owned() + &String::from_utf8_lossy(&output.stderr))
    };
    let dir = std::env::temp_dir().join(format!("smbwrite-{port}"));
    std::fs::create_dir_all(&dir).unwrap();
    let source = dir.join("up.bin");
    std::fs::write(&source, pattern(200_000)).unwrap();
    let back = dir.join("back.bin");
    let moved = dir.join("moved.txt");
    let script = format!(
        "put {s} up.bin; mkdir newdir; put {s} newdir\\in.bin; rename up.bin renamed.bin; get renamed.bin {b}; \
         put {s} readme.txt; get readme.txt {m}; rm newdir\\in.bin; rmdir newdir; rm sub\\inner.txt; ls",
        s = source.display(), b = back.display(), m = moved.display()
    );
    let (ok, text) = run("alice%secret", &script);
    assert!(ok, "alice: {text}");
    assert_eq!(std::fs::read(&back).unwrap(), pattern(200_000), "uploaded and renamed file");
    assert_eq!(std::fs::read(&moved).unwrap(), pattern(200_000), "overwritten file");
    let listing = text.split("blocks of size").next().unwrap();
    assert!(!listing.contains("newdir") && !listing.contains(" up.bin") && listing.contains("renamed.bin"), "listing: {text}");

    let (_, text) = run("alice%secret", "rmdir sub");
    assert!(text.contains("DIRECTORY_NOT_EMPTY"), "non-empty folder: {text}");

    let (_, text) = run("bob%hunter2", &format!("put {} bob.bin", source.display()));
    assert!(text.contains("ACCESS_DENIED"), "bob may not write: {text}");
    let (_, text) = run("bob%hunter2", "rm readme.txt");
    assert!(text.contains("ACCESS_DENIED"), "bob may not delete: {text}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn our_client_changes_files() {
    let mut client: Client<Loopback, 70_000> = Client::new(Loopback { server: Box::new(Server::new(memory())), out: vec![0; 70_200], incoming: Vec::new() });
    client.connect("h", "fos").unwrap();
    let data = pattern(150_000);
    let file = client.create_file("new.bin", false).unwrap();
    let mut done = 0;
    while done < data.len() {
        done += client.write(&file, done as u64, &data[done..]).unwrap();
    }
    client.close(&file).unwrap();
    assert_eq!(client.create_file("new.bin", false).err(), Some(Error::Status(0xC000_0035)), "an existing file collides");
    let back = client.open("new.bin", false).unwrap();
    assert_eq!(back.size, data.len() as u64);
    let mut buffer = vec![0u8; 65_000];
    let count = client.read(&back, 140_000, &mut buffer).unwrap();
    assert_eq!(&buffer[..count], &data[140_000..]);
    client.close(&back).unwrap();

    let again = client.create_file("new.bin", true).unwrap();
    assert_eq!(again.size, 0, "replace empties the file");
    client.close(&again).unwrap();
    client.make_dir("folder").unwrap();
    client.rename("new.bin", "folder/moved.bin").unwrap();
    assert!(client.open("new.bin", false).is_err());
    assert!(client.open("folder/moved.bin", false).is_ok());
    assert_eq!(client.remove("folder", true).err(), Some(Error::Status(0xC000_0101)), "a folder with a file stays");
    client.remove("folder/moved.bin", false).unwrap();
    client.remove("folder", true).unwrap();
    assert!(client.open("folder", true).is_err());
}
