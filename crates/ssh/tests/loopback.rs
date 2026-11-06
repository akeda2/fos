//! The client against the real server code over a loopback TCP socket, exporting a temporary host directory.
#[path = "../examples/common/mod.rs"]
mod common;

use common::Server;
use ssh::Session;
use ssh::authorized::public_line;
use ssh::client::{CLIENT_ARENA, Client, Error, Link, OPEN_CREATE, OPEN_READ, OPEN_TRUNCATE, OPEN_WRITE, STATUS_NO_SUCH_FILE};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

struct Tcp(TcpStream);

impl Link for Tcp {
    fn send(&mut self, data: &[u8]) -> Result<(), Error> {
        self.0.write_all(data).map_err(|_| Error::Link)
    }
    fn recv(&mut self, out: &mut [u8]) -> Result<usize, Error> {
        self.0.read(out).map_err(|_| Error::Link)
    }
    fn random(&mut self, out: &mut [u8]) {
        std::fs::File::open("/dev/urandom").unwrap().read_exact(out).unwrap();
    }
}

fn serve(dir: String, keys: String) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut host = Server::new(&dir, &keys, false);
        let mut arena = vec![0u8; ssh::ARENA].into_boxed_slice();
        let mut session = Session::new(&mut host, &mut arena);
        let (mut input, mut chunk) = (Vec::new(), vec![0u8; 65536]);
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
    port
}

fn authorized(seed: &[u8; 32]) -> String {
    let mut line = [0u8; 80];
    let length = public_line(seed, &mut line);
    format!("user=tester {}\n", std::str::from_utf8(&line[..length]).unwrap())
}

fn temp(name: &str) -> String {
    let dir = std::env::temp_dir().join(format!("ssh-loopback-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.to_str().unwrap().to_string()
}

#[test]
fn files_and_folders_round_trip() {
    let dir = temp("ops");
    let client_seed = [7u8; 32];
    let port = serve(dir.clone(), authorized(&client_seed));
    let mut arena = vec![0u8; CLIENT_ARENA].into_boxed_slice();
    let mut seen = None;
    let mut client = Client::connect(Tcp(TcpStream::connect(("127.0.0.1", port)).unwrap()), &mut arena, "tester", &client_seed, &mut |key| {
        seen = Some(*key);
        true
    })
    .unwrap();
    // The example server's fixed host seed.
    assert_eq!(seen, Some(*ed25519_dalek::SigningKey::from_bytes(&[42; 32]).verifying_key().as_bytes()));

    client.make_dir("/sub", 0o755).unwrap();
    assert!(client.stat("/sub").unwrap().is_dir());

    // Larger than one SFTP request and than the channel window, with a pattern that shows misplaced pieces.
    let data: Vec<u8> = (0..700_000u32).map(|n| (n ^ n >> 8) as u8).collect();
    let handle = client.open("/sub/big", OPEN_WRITE | OPEN_CREATE | OPEN_TRUNCATE, 0o644).unwrap();
    client.write(&handle, 0, &data).unwrap();
    client.close(&handle).unwrap();
    assert_eq!(std::fs::read(format!("{dir}/sub/big")).unwrap(), data);

    let handle = client.open("/sub/big", OPEN_READ, 0).unwrap();
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

    client.set_size("/sub/big", 10).unwrap();
    assert_eq!(client.stat("/sub/big").unwrap().size, Some(10));
    client.rename("/sub/big", "/sub/small").unwrap();

    let mut names = Vec::new();
    let handle = client.open_dir("/sub").unwrap();
    while client.read_dir(&handle, &mut |name, _| names.push(String::from_utf8_lossy(name).to_string())).unwrap() {}
    client.close(&handle).unwrap();
    names.retain(|name| name != "." && name != "..");
    assert_eq!(names, ["small"]);

    client.remove("/sub/small").unwrap();
    assert_eq!(client.stat("/sub/small").unwrap_err(), Error::Status(STATUS_NO_SUCH_FILE));
    client.remove_dir("/sub").unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_unknown_key_is_refused() {
    let dir = temp("auth");
    let port = serve(dir.clone(), authorized(&[7u8; 32]));
    let mut arena = vec![0u8; CLIENT_ARENA].into_boxed_slice();
    let result = Client::connect(Tcp(TcpStream::connect(("127.0.0.1", port)).unwrap()), &mut arena, "tester", &[8u8; 32], &mut |_| true);
    assert!(matches!(result, Err(Error::Auth)));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_host_key_that_is_not_trusted_stops_the_login() {
    let dir = temp("hostkey");
    let port = serve(dir.clone(), authorized(&[7u8; 32]));
    let mut arena = vec![0u8; CLIENT_ARENA].into_boxed_slice();
    let result = Client::connect(Tcp(TcpStream::connect(("127.0.0.1", port)).unwrap()), &mut arena, "tester", &[7u8; 32], &mut |_| false);
    assert!(matches!(result, Err(Error::HostKey)));
    let _ = std::fs::remove_dir_all(&dir);
}
