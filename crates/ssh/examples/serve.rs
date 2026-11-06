//! The `sshd` tape's protocol code on a host directory, to try the real `ssh`, `sftp` and `sshfs` against it:
//!
//!   cargo run --example serve -- PORT DIR AUTHORIZED_KEYS [ro]
//!
//! DIR is exported as `/`; the host key is derived from a fixed seed (this is a test tool, not a server to leave running).
mod common;

use common::Server;
use ssh::Session;
use std::io::{Read, Write};
use std::net::TcpListener;

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
            let mut host = Server::new(&dir, &keys, read_only);
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
