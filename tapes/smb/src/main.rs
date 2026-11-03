//! smb: the SMB client as a shell command: list, show and copy files of a share (SMB 2/3) over
//! `crates/smb` and the shared TCP client, on the interface chosen with `nic use N` and an address from DHCP.
#![no_std]
#![no_main]

use smb::{Client, Error, Transport};
use tape_ip::net;
use tape_ip::tcp::Socket;
use tape_net::{ArpCache, MacAddress};
use tape_rt::{TapeApi, println};

tape_rt::tape_help!(
    br#"smb ls   //HOST/SHARE[/DIR]
smb cat  //HOST/SHARE/FILE
smb get  //HOST/SHARE/FILE [NAME]
      add user=NAME password=SECRET to log in (default: guest)
List a folder, print a file, or copy a file to this machine (NAME is a file
reference: NAME, diskN/NAME or @LABEL/NAME; default the file's own name). HOST
is an IPv4 address or a name. Guest login unless user= is given (NTLMv2,
requests signed): a Linux/Samba/Windows share or fos's own smbd (smbusers).
SMB 2.0.2 to 2.1. The address
comes from DHCP on the NIC of `nic use`. Escape stops a transfer.
"#
);

const PORT: u16 = 445;
const TIMEOUT_MICROS: u64 = 5_000_000;
const CHUNK: usize = 64 * 1024;

struct Link {
    socket: Socket<16384>,
    api: Option<&'static TapeApi>,
}

impl Transport for Link {
    fn send(&mut self, data: &[u8]) -> Result<(), Error> {
        let api = self.api.ok_or(Error::Transport)?;
        self.socket.send(api, data, TIMEOUT_MICROS).map_err(|_| Error::Transport)
    }

    fn recv_exact(&mut self, out: &mut [u8]) -> Result<(), Error> {
        let api = self.api.ok_or(Error::Transport)?;
        self.socket.recv_exact(api, out, TIMEOUT_MICROS).map_err(|_| Error::Transport)
    }
}

type Session = Client<Link, { CHUNK + 1024 }>;

static mut CLIENT: Session = Client::new(Link { socket: Socket::new(), api: None });
static mut BLOCK: [u8; CHUNK] = [0; CHUNK];

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(api: &'static TapeApi) {
    tape_rt::init(api);
    let arguments = core::str::from_utf8(tape_rt::args()).unwrap_or("");
    let (mut user, mut password) = (None, "");
    // Every word is looked at first: a lazy filter would not reach user= behind the local name of `get`.
    for word in arguments.split_whitespace() {
        if let Some(name) = word.strip_prefix("user=") {
            user = Some(name);
        } else if let Some(secret) = word.strip_prefix("password=") {
            password = secret;
        }
    }
    let mut words = arguments.split_whitespace().filter(|word| !word.starts_with("user=") && !word.starts_with("password="));
    let (Some(command), Some(url)) = (words.next(), words.next()) else { return println!("usage: smb ls|cat|get //HOST/SHARE[/PATH] [NAME]  (smb help)") };
    let Some(rest) = url.strip_prefix("//") else { return println!("smb: {url}: write //HOST/SHARE/PATH.") };
    let (host, rest) = rest.split_once('/').unwrap_or((rest, ""));
    let (share, path) = rest.split_once('/').unwrap_or((rest, ""));
    let path = path.trim_end_matches('/');
    if host.is_empty() || share.is_empty() || !matches!(command, "ls" | "cat" | "get") || (command != "ls" && path.is_empty()) {
        return println!("usage: smb ls|cat|get //HOST/SHARE[/PATH] [NAME]  (smb help)");
    }
    let local = words.next().unwrap_or_else(|| path.rsplit('/').next().unwrap_or(path));
    #[allow(static_mut_refs)]
    let (client, block) = unsafe { (&mut *core::ptr::addr_of_mut!(CLIENT), &mut *core::ptr::addr_of_mut!(BLOCK)) };
    if connect(api, client, host, share, user.map(|name| (name, password))).is_none() {
        return;
    }
    match command {
        "ls" => list(client, path),
        "cat" => cat(api, client, path, block),
        _ => get(api, client, path, local, block),
    }
    client.transport().socket.close(api);
}

fn connect(api: &'static TapeApi, client: &mut Session, host: &str, share: &str, login: Option<(&str, &str)>) -> Option<()> {
    let nic = tape_rt::active_nic();
    if nic >= (api.nic_count)() as usize {
        println!("smb: no network interface.");
        return None;
    }
    let mut mac = [0u8; 6];
    (api.nic_mac)(nic, mac.as_mut_ptr());
    let Some((config, _, dns_server)) = net::dhcp_with_servers(api, nic, MacAddress(mac)) else {
        println!("smb: no DHCP lease on NIC {}.", nic + 1);
        return None;
    };
    let address = match tape_ip::dns::lookup(api, nic, &config, &mut ArpCache::<8>::new(), dns_server.unwrap_or(config.gateway), tape_net::dns::PORT, host) {
        Ok(address) => address,
        Err(why) => {
            println!("smb: {host}: {why}.");
            return None;
        }
    };
    let link = client.transport();
    link.api = Some(api);
    if let Err(why) = link.socket.connect(api, nic, &config, address, PORT, 3_000_000) {
        println!("smb: {host}: {why}.");
        return None;
    }
    if let Some((user, password)) = login {
        // The client challenge: random where the kernel has the call, else the clock (a weaker nonce, still never repeated).
        let mut nonce = [0u8; 8];
        if tape_rt::random(&mut nonce).is_none() {
            nonce = tape_rt::micros().to_le_bytes();
        }
        if client.login(user, password, nonce).is_err() {
            println!("smb: the user name is too long.");
            return None;
        }
    }
    if let Err(error) = client.connect(host, share) {
        println!("smb: share {share}: {}.", error.describe());
        return None;
    }
    Some(())
}

fn list(client: &mut Session, path: &str) {
    let mut entries = 0u32;
    let result = client.list(path, |entry| {
        let mut name = [0u8; 120];
        let length = entry.name_utf8(&mut name);
        let name = core::str::from_utf8(&name[..length]).unwrap_or("?");
        if name == "." || name == ".." {
            return;
        }
        if entry.is_dir() {
            println!("  <dir>        {name}");
        } else {
            println!("  {:>12} {name}", entry.size);
        }
        entries += 1;
    });
    match result {
        Ok(()) => println!("smb: {entries} entries."),
        Err(error) => println!("smb: {path}: {}.", error.describe()),
    }
}

/// Prints the file as text (anything that is not UTF-8 shows as `?`).
fn cat(api: &TapeApi, client: &mut Session, path: &str, block: &mut [u8; CHUNK]) {
    let file = match client.open(path, false) {
        Ok(file) => file,
        Err(error) => return println!("smb: {path}: {}.", error.describe()),
    };
    let mut offset = 0u64;
    while offset < file.size {
        if (api.should_abort)() != 0 {
            println!("\nsmb: stopped.");
            break;
        }
        let count = match client.read(&file, offset, &mut block[..]) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) => return println!("\nsmb: read at {offset}: {}.", error.describe()),
        };
        for piece in block[..count].utf8_chunks() {
            tape_rt::print!("{}", piece.valid());
            if !piece.invalid().is_empty() {
                tape_rt::print!("?");
            }
        }
        offset += count as u64;
    }
    let _ = client.close(&file);
    println!();
}

fn get(api: &TapeApi, client: &mut Session, path: &str, local: &str, block: &mut [u8; CHUNK]) {
    let file = match client.open(path, false) {
        Ok(file) => file,
        Err(error) => return println!("smb: {path}: {}.", error.describe()),
    };
    let mut capacity = 0u64;
    let handle = (api.fs_stream_open2)(local.as_ptr(), local.len(), file.size.max(1), &mut capacity);
    if handle < 0 {
        let _ = client.close(&file);
        return println!("smb: cannot create {local} ({handle}); the name or {} bytes of free space?", file.size);
    }
    let (mut offset, mut filled) = (0u64, 0usize);
    let started = tape_rt::micros();
    let mut failed = false;
    while offset + (filled as u64) < file.size {
        if (api.should_abort)() != 0 {
            println!("smb: stopped.");
            failed = true;
            break;
        }
        match client.read(&file, offset + filled as u64, &mut block[filled..]) {
            Ok(0) => break,
            Ok(count) => filled += count,
            Err(error) => {
                println!("smb: read at {}: {}.", offset + filled as u64, error.describe());
                failed = true;
                break;
            }
        }
        // Stream writes start on a 512-byte boundary: write whole blocks, the rest at the end.
        if filled == CHUNK {
            if (api.fs_stream_write2)(handle, offset, block.as_ptr(), filled, 0) != 0 {
                println!("smb: writing {local} failed.");
                failed = true;
                break;
            }
            offset += filled as u64;
            filled = 0;
        }
    }
    let total = offset + filled as u64;
    if !failed && filled > 0 && (api.fs_stream_write2)(handle, offset, block.as_ptr(), filled, 0) != 0 {
        println!("smb: writing {local} failed.");
        failed = true;
    }
    let closed = (api.fs_stream_close2)(handle, if failed { 0 } else { total });
    let _ = client.close(&file);
    if failed || closed != 0 || total != file.size {
        return println!("smb: {local}: copied {total} of {} bytes{}.", file.size, if closed != 0 { " (could not finish the file)" } else { "" });
    }
    let micros = (tape_rt::micros() - started).max(1);
    println!("smb: {path} -> {local}: {total} bytes in {} ms, {} kbit/s.", micros / 1000, total * 8 * 1000 / micros);
}
