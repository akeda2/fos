//! smbd: a read-only SMB 2/3 file server (guest access, no signing, no encryption) for one folder of any volume the kernel
//! can read, ext2/3/4 included (`vfs_*`, tape ABI 32). The protocol is `crates/smb` (`server`); this tape is its TCP
//! front end, a service like `httpd` (`run smbd @SMBD.CFG &2`). No authentication yet: use it on a network you trust.
#![no_std]
#![no_main]

use smb::server::{MAX_IO, Server, Share, Stat};
use tape_ip::net::{self, NetConfig};
use tape_ip::tcp::Server as Tcp;
use tape_net::MacAddress;
use tape_rt::config::Config;
use tape_rt::{TapeApi, arg_value as value, println};

tape_rt::tape_help!(
    br#"smbd [@NAME] [root=DIR] [share=NAME] [port=445] [nic=N] [ip=A.B.C.D/N]
      [gateway=A.B.C.D] [log]
A read-only SMB file server (SMB 2.0.2 to 3.0, guest, no signing): Linux
mount.cifs/smbclient, Windows and macOS can browse and copy from it. It runs as
a service: run smbd @SMBD.CFG &2 (the service owns NIC 2; tape run smbd takes
the foreground and Escape stops it). Settings are key=value words on the
command line or in the file (one per line, # for comments).
root=   the folder shared: disk2 (a whole ext4 disk), disk2/music, @LABEL/site,
        a FOSFS or FAT folder. Default: the boot volume's root.
share=  the share name clients use, //HOST/NAME (default: share).
port=   TCP port (default 445).
ip=     a fixed address like 192.168.1.20/24 (gateway= for the router);
        without it DHCP. nic= is the NIC number from 1 (default: nic use).
log     print one line per connection.
No password is asked and nothing can be written. Needs tape ABI 32.
"#
);

const SLOTS: usize = 4;
const OPENS: usize = 16;
const RX: usize = 8192;
const ROOT_MAX: usize = 200;
const SEND_TIMEOUT: u64 = 5_000_000;

struct Files {
    root: [u8; ROOT_MAX],
    root_length: usize,
    name: [u8; 32],
    name_length: usize,
}

impl Files {
    const fn new() -> Self {
        Files { root: [0; ROOT_MAX], root_length: 0, name: [0; 32], name_length: 0 }
    }

    /// `root` and `path` joined with a slash, into `out`.
    fn join<'a>(&self, path: &str, out: &'a mut [u8; 512]) -> Option<&'a str> {
        let root = &self.root[..self.root_length];
        if root.len() + 1 + path.len() > out.len() {
            return None;
        }
        out[..root.len()].copy_from_slice(root);
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
}

fn stat_of(stat: &tape_rt::VfsStat) -> Stat {
    Stat { dir: stat.kind == tape_rt::VFS_DIR, size: stat.size, mtime: stat.mtime }
}

impl Share for Files {
    fn name(&self) -> &str {
        core::str::from_utf8(&self.name[..self.name_length]).unwrap_or("share")
    }

    fn stat(&mut self, path: &str) -> Option<Stat> {
        let mut buffer = [0u8; 512];
        let reference = self.join(path, &mut buffer)?;
        tape_rt::vfs_stat(reference).ok().map(|stat| stat_of(&stat))
    }

    fn read_dir(&mut self, path: &str, index: usize, name: &mut [u8]) -> Option<(usize, Stat)> {
        let mut buffer = [0u8; 512];
        let reference = self.join(path, &mut buffer)?;
        let entry = tape_rt::vfs_read_dir(reference, index).ok()??;
        let length = entry.name().len().min(name.len());
        name[..length].copy_from_slice(&entry.name()[..length]);
        Some((length, stat_of(&entry.stat)))
    }

    fn read(&mut self, path: &str, offset: u64, out: &mut [u8]) -> Result<usize, ()> {
        let mut buffer = [0u8; 512];
        let reference = self.join(path, &mut buffer).ok_or(())?;
        tape_rt::vfs_read(reference, offset, out).map_err(|_| ())
    }
}

static mut TCP: Tcp<RX, SLOTS> = Tcp::new();
static mut REQUEST: [u8; RX] = [0; RX];
static mut ANSWER: [u8; MAX_IO + 256] = [0; MAX_IO + 256];
static mut SESSIONS: [Option<Server<Files, OPENS>>; SLOTS] = [const { None }; SLOTS];

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(api: &'static TapeApi) {
    tape_rt::init(api);
    let config = match Config::load(api, "SMBD", core::str::from_utf8(tape_rt::args()).unwrap_or("")) {
        Ok(config) => config,
        Err(reason) => return println!("smbd: {reason}."),
    };
    let text = config.text();
    let root = value(text, "root").unwrap_or("").trim_end_matches('/');
    let share = value(text, "share").unwrap_or("share");
    if root.len() > ROOT_MAX || share.len() > 32 {
        return println!("smbd: root= or share= is too long.");
    }
    if tape_rt::vfs_stat(root).map_or(true, |stat| stat.kind != tape_rt::VFS_DIR) {
        return println!("smbd: {} is not a folder this kernel can read (needs tape ABI 32).", if root.is_empty() { "the boot volume" } else { root });
    }
    let log = text.split_ascii_whitespace().any(|word| word == "log" || word == "log=on");
    let Some(port) = value(text, "port").map_or(Some(445), |word| word.parse::<u16>().ok().filter(|&port| port != 0)) else {
        return println!("smbd: port= takes a number from 1 to 65535.");
    };
    let nic = value(text, "nic").and_then(|word| word.parse::<usize>().ok()).map_or(tape_rt::active_nic(), |n| n.saturating_sub(1));
    if (api.nic_count)() <= nic {
        return println!("smbd: this machine has no NIC {}.", nic + 1);
    }
    let mut mac = [0u8; 6];
    (api.nic_mac)(nic, mac.as_mut_ptr());
    let mac = MacAddress(mac);
    let network: NetConfig = match value(text, "ip") {
        Some(ip) => match net::static_config(mac, ip, value(text, "gateway").unwrap_or("")) {
            Ok(network) => network,
            Err(reason) => return println!("smbd: {reason}."),
        },
        None => match net::dhcp(api, nic, mac) {
            Some(network) => network,
            None => return println!("smbd: no DHCP lease (give ip=A.B.C.D/N for a fixed address)."),
        },
    };
    #[allow(static_mut_refs)]
    let (tcp, request, answer, sessions) = unsafe {
        (&mut *core::ptr::addr_of_mut!(TCP), &mut *core::ptr::addr_of_mut!(REQUEST), &mut *core::ptr::addr_of_mut!(ANSWER), &mut *core::ptr::addr_of_mut!(SESSIONS))
    };
    tcp.start(nic, &network, port);
    println!("smbd: NIC {} at {}.{}.{}.{}:{port}, sharing {} as //HOST/{share}.", nic + 1, network.ip.0[0], network.ip.0[1], network.ip.0[2], network.ip.0[3], if root.is_empty() { "the boot volume" } else { root });
    loop {
        if (api.should_abort)() != 0 {
            break;
        }
        let busy = tcp.poll(api, 16);
        for slot in 0..SLOTS {
            if !tcp.is_open(slot) {
                sessions[slot] = None;
                continue;
            }
            if sessions[slot].is_none() {
                let mut files = Files::new();
                files.root[..root.len()].copy_from_slice(root.as_bytes());
                files.root_length = root.len();
                files.name[..share.len()].copy_from_slice(share.as_bytes());
                files.name_length = share.len();
                let mut session = Server::new(files);
                let seed = tape_rt::micros();
                session.challenge = (seed ^ (slot as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15)).wrapping_mul(0xD6E8_FEB8_6659_FD93).to_le_bytes();
                session.guid[8..].copy_from_slice(&seed.to_le_bytes());
                sessions[slot] = Some(session);
                if log {
                    let (ip, _) = tcp.peer(slot);
                    println!("smbd: connection from {}.{}.{}.{}", ip.0[0], ip.0[1], ip.0[2], ip.0[3]);
                }
            }
            serve(api, tcp, slot, request, answer, sessions[slot].as_mut().unwrap());
        }
        if !busy {
            tape_rt::idle();
        }
    }
}

/// Answers every complete message waiting on `slot` (a 4-byte frame header, then the SMB message).
fn serve(api: &TapeApi, tcp: &mut Tcp<RX, SLOTS>, slot: usize, request: &mut [u8; RX], answer: &mut [u8], session: &mut Server<Files, OPENS>) {
    loop {
        let waiting = tcp.received(slot);
        if waiting.len() < 4 {
            return;
        }
        let length = (usize::from(waiting[1]) << 16) | (usize::from(waiting[2]) << 8) | usize::from(waiting[3]);
        if waiting[0] != 0 || length + 4 > RX {
            // A message this server never takes (a write, or not SMB at all): hang up.
            tcp.close(api, slot);
            return;
        }
        if waiting.len() < length + 4 {
            return;
        }
        let mut taken = 0;
        while taken < length + 4 {
            let got = tcp.take(slot, &mut request[taken..length + 4]);
            if got == 0 {
                break;
            }
            taken += got;
        }
        let sent = session.handle(&request[4..length + 4], answer);
        if sent > 0 && tcp.send(api, slot, &answer[..sent], SEND_TIMEOUT).is_err() {
            return;
        }
    }
}
