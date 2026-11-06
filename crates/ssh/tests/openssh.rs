//! The client against a real OpenSSH `sshd` run as the current user on a loopback port (skipped when there is none).
use ssh::authorized::public_line;
use ssh::client::{CLIENT_ARENA, Client, Error, Link, OPEN_CREATE, OPEN_READ, OPEN_TRUNCATE, OPEN_WRITE, SHELL_CHUNK, Start};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

struct Tcp(TcpStream);

impl Link for Tcp {
    fn send(&mut self, data: &[u8]) -> Result<(), Error> {
        self.0.write_all(data).map_err(|_| Error::Link)
    }
    fn recv(&mut self, out: &mut [u8]) -> Result<usize, Error> {
        self.0.read(out).map_err(|error| if matches!(error.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) { Error::Timeout } else { Error::Link })
    }
    fn random(&mut self, out: &mut [u8]) {
        std::fs::File::open("/dev/urandom").unwrap().read_exact(out).unwrap();
    }
}

struct Sshd(Child);

impl Drop for Sshd {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Running {
    _server: Sshd,
    stream: TcpStream,
    dir: std::path::PathBuf,
    user: String,
    seed: [u8; 32],
}

/// An OpenSSH `sshd` on a loopback port that accepts the test key, and a connection to it; None when there is no sshd.
fn start() -> Option<Running> {
    let (sshd, sftp) = ("/usr/sbin/sshd", "/usr/lib/openssh/sftp-server");
    if !std::path::Path::new(sshd).exists() || !std::path::Path::new(sftp).exists() {
        eprintln!("no OpenSSH sshd here: skipped");
        return None;
    }
    let user = std::env::var("USER").unwrap_or_else(|_| "root".into());
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    // The tests run in parallel: every server gets its own folder and port.
    let number = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("ssh-openssh-{}-{number}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let seed = [9u8; 32];
    let mut line = [0u8; 80];
    let length = public_line(&seed, &mut line);
    let keys = dir.join("authorized_keys");
    std::fs::write(&keys, &line[..length]).unwrap();
    let host_key = dir.join("host_key");
    assert!(Command::new("ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-f"]).arg(&host_key).status().unwrap().success());
    let port = (20000 + (std::process::id() % 20000 + number * 7) % 20000) as u16;
    let config = format!(
        "Port {port}\nListenAddress 127.0.0.1\nHostKey {}\nAuthorizedKeysFile {}\nPidFile {}/pid\nUsePAM no\nStrictModes no\nPasswordAuthentication no\nSubsystem sftp {sftp}\n",
        host_key.display(),
        keys.display(),
        dir.display()
    );
    std::fs::write(dir.join("sshd_config"), config).unwrap();
    let _server = Sshd(Command::new(sshd).arg("-D").arg("-f").arg(dir.join("sshd_config")).stderr(Stdio::null()).spawn().unwrap());
    let stream = (0..50)
        .find_map(|_| {
            TcpStream::connect(("127.0.0.1", port)).ok().or_else(|| {
                std::thread::sleep(Duration::from_millis(100));
                None
            })
        })
        .expect("sshd did not start");
    stream.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    Some(Running { _server, stream, dir, user, seed })
}

#[test]
fn talks_to_openssh() {
    let Some(Running { stream, dir, user, seed, _server }) = start() else { return };
    let mut arena = vec![0u8; CLIENT_ARENA].into_boxed_slice();
    let mut client = Client::connect(Tcp(stream), &mut arena, &user, &seed, &mut |_| true).unwrap();

    let root = dir.to_str().unwrap();
    client.make_dir(&format!("{root}/sub"), 0o755).unwrap();
    let data: Vec<u8> = (0..500_000u32).map(|n| (n ^ n >> 8) as u8).collect();
    let path = format!("{root}/sub/file");
    let handle = client.open(&path, OPEN_WRITE | OPEN_CREATE | OPEN_TRUNCATE, 0o644).unwrap();
    client.write(&handle, 0, &data).unwrap();
    client.close(&handle).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), data);
    let handle = client.open(&path, OPEN_READ, 0).unwrap();
    let (mut back, mut piece) = (Vec::new(), vec![0u8; 32768]);
    loop {
        let count = client.read(&handle, back.len() as u64, &mut piece).unwrap();
        if count == 0 {
            break;
        }
        back.extend_from_slice(&piece[..count]);
    }
    client.close(&handle).unwrap();
    assert_eq!(back, data);
    client.rename(&path, &format!("{root}/sub/other")).unwrap();
    let mut names = Vec::new();
    let handle = client.open_dir(&format!("{root}/sub")).unwrap();
    while client.read_dir(&handle, &mut |name, _| names.push(String::from_utf8_lossy(name).to_string())).unwrap() {}
    client.close(&handle).unwrap();
    names.retain(|name| name != "." && name != "..");
    assert_eq!(names, ["other"]);
    client.remove(&format!("{root}/sub/other")).unwrap();
    client.remove_dir(&format!("{root}/sub")).unwrap();
    drop(client);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Runs a session until the server ends it; the output and the exit status.
fn collect(client: &mut Client<Tcp>, mut input: &[u8]) -> (String, Option<u32>) {
    let (mut text, mut chunk) = (Vec::new(), vec![0u8; SHELL_CHUNK]);
    for _ in 0..200 {
        if !input.is_empty() {
            let taken = client.write_input(input).unwrap();
            input = &input[taken..];
        }
        match client.read_output(&mut chunk) {
            Ok(Some(count)) => text.extend_from_slice(&chunk[..count]),
            Ok(None) => {}
            Err(Error::Closed) => break,
            Err(error) => panic!("{error:?}"),
        }
    }
    (String::from_utf8_lossy(&text).into_owned(), client.exit_status())
}

#[test]
fn runs_a_command_and_a_shell_on_openssh() {
    for exec in [true, false] {
        let Some(Running { stream, dir, user, seed, _server }) = start() else { return };
        stream.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
        let mut arena = vec![0u8; CLIENT_ARENA].into_boxed_slice();
        let start = if exec { Start::Exec("echo out$((20+3)); echo err >&2; exit 3") } else { Start::Shell { cols: 80, rows: 25 } };
        let mut client = Client::connect_shell(Tcp(stream), &mut arena, &user, &seed, &mut |_| true, start).unwrap();
        let (text, status) = collect(&mut client, if exec { b"" } else { b"echo shell$((20+3))\nexit 4\n" });
        if exec {
            assert!(text.contains("out23") && text.contains("err"), "{text:?}");
            assert_eq!(status, Some(3));
        } else {
            assert!(text.contains("shell23"), "{text:?}");
            assert_eq!(status, Some(4));
        }
        client.hang_up();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
