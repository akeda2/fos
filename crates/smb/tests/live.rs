//! Talks to a real server: `FOS_SMB_HOST=192.168.0.94 FOS_SMB_SHARE=media FOS_SMB_FILE=musik/x.flac cargo test --test live -- --nocapture`.
//! Skipped (passes) when FOS_SMB_HOST is not set.
use smb::{Client, Error, Transport};
use std::io::{Read, Write};
use std::net::TcpStream;

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
fn guest_share_listing_and_read() {
    let Ok(host) = std::env::var("FOS_SMB_HOST") else { return };
    let share = std::env::var("FOS_SMB_SHARE").unwrap_or("media".into());
    let stream = TcpStream::connect((host.as_str(), 445)).unwrap();
    stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
    let mut client = Box::new(Client::<Tcp, 70000>::new(Tcp(stream)));
    client.connect(&host, &share).map_err(|e| format!("{e:?}: {}", e.describe())).unwrap();
    println!("connected, max read {}", client.max_read());
    let mut count = 0;
    client
        .list("", |entry| {
            let mut name = [0u8; 255];
            let length = entry.name_utf8(&mut name);
            if count < 20 {
                println!("{} {:>12} {}", if entry.is_dir() { "d" } else { "-" }, entry.size, String::from_utf8_lossy(&name[..length]));
            }
            count += 1;
        })
        .map_err(|e| format!("{e:?}: {}", e.describe()))
        .unwrap();
    println!("{count} entries in the root");
    if let Ok(file) = std::env::var("FOS_SMB_FILE") {
        let handle = client.open(&file, false).map_err(|e| format!("{e:?}: {}", e.describe())).unwrap();
        println!("{file}: {} bytes", handle.size);
        let mut data = vec![0u8; 65536];
        let n = client.read(&handle, 0, &mut data).unwrap();
        println!("first read {n} bytes: {:?}", String::from_utf8_lossy(&data[..4]));
        assert_eq!(&data[..4], b"fLaC");
        let mut total = 0u64;
        loop {
            let n = client.read(&handle, total, &mut data).unwrap();
            if n == 0 {
                break;
            }
            total += n as u64;
        }
        assert_eq!(total, handle.size);
        client.close(&handle).unwrap();
    }
}
