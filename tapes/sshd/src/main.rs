//! sshd: an SSH-2 server for file access: SFTP (so `sftp`, `scp -s` and `sshfs` work) on folders of any volume the
//! kernel can read (`vfs_*`, tape ABI 32; writes need an ext volume or FOSFS). The protocol is `crates/ssh`; this tape is its
//! TCP front end, a service like `smbd` (`run sshd @SSHD.CFG &2`). Public-key logins only (ssh-ed25519).
#![no_std]
#![no_main]

use ssh::authorized::{AuthorizedKeys, fingerprint, parse_seed, seed_text};
use ssh::{ARENA, Account, Exports, Host, Session};
use tape_ip::net::{self, NetConfig};
use tape_ip::tcp::Server as Tcp;
use tape_net::MacAddress;
use tape_rt::config::{Config, read_named, write_named};
use tape_rt::{TapeApi, arg_value as value, println};
use vfs_export::{Files, Vfs};

tape_rt::tape_help!(
    br#"sshd [@NAME] [root=DIR] [export=NAME[=ROOT][,ro]] [port=22] [nic=N]
      [ip=A.B.C.D/N] [gateway=A.B.C.D] [ro] [log]
An SSH server for file access over SFTP: sftp, sshfs and scp -s from Linux,
macOS and Windows. Public-key logins only (ssh-ed25519): settings/authorized_keys
has one key per line, user=NAME[,uid=N][,gid=N][,ro] ssh-ed25519 KEY comment.
The host key is made at the first start (settings/ssh_host_key); its fingerprint
is printed at every start. Runs as a service: run sshd @SSHD.CFG &2.
root=   the folder served: disk2, disk2/music, @LABEL/site, //NAME. Default: the
        boot volume. One export shows as /, several as folders under /.
export= one more folder under its own name (up to 8); ,ro refuses changes.
ro      every export refuses changes.   log  show logins and sessions.
Without ip= the NIC asks DHCP."#
);

const PORT: u16 = 22;
const SLOTS: usize = 4;
const RX: usize = 262_144;
const LOGIN_US: u64 = 20_000_000;
const IDLE_US: u64 = 600_000_000;
const SYNC_QUIET_US: u64 = 20_000;
const SPIN_US: u64 = 2_000;

struct Node {
    exports: Exports,
    keys: AuthorizedKeys,
    files: Vfs,
    seed: [u8; 32],
    log: bool,
}

impl Host for Node {
    fn random(&mut self, out: &mut [u8]) {
        let _ = tape_rt::random(out);
    }
    fn host_seed(&self) -> [u8; 32] {
        self.seed
    }
    fn authorize(&mut self, user: &str, key: &[u8; 32]) -> Option<Account> {
        self.keys.lookup(user, key)
    }
    fn split(&mut self) -> (&Exports, &mut dyn Files) {
        (&self.exports, &mut self.files)
    }
    fn log(&mut self, text: core::fmt::Arguments) {
        if self.log {
            println!("sshd: {text}");
        }
    }
}

fn bytes(api: &TapeApi, count: usize) -> Option<&'static mut [u8]> {
    let address = (api.alloc)(count);
    if address == 0 {
        return None;
    }
    unsafe {
        core::ptr::write_bytes(address as *mut u8, 0, count);
        Some(core::slice::from_raw_parts_mut(address as *mut u8, count))
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(api: &'static TapeApi) {
    tape_rt::init(api);
    let config = match Config::load(api, "SSHD", core::str::from_utf8(tape_rt::args()).unwrap_or("")) {
        Ok(config) => config,
        Err(reason) => return println!("sshd: {reason}."),
    };
    let text = config.text();
    let root = value(text, "root").unwrap_or("").trim_end_matches('/');
    let read_only = text.split_ascii_whitespace().any(|word| word == "ro");
    let log = text.split_ascii_whitespace().any(|word| word == "log");
    let Some(node) = tape_rt::alloc_zeroed::<core::mem::MaybeUninit<Node>>(api) else { return println!("sshd: out of memory.") };
    let node = node.write(Node { exports: Exports::new(), keys: AuthorizedKeys::new(), files: Vfs, seed: [0; 32], log });
    for word in text.split_ascii_whitespace().filter_map(|word| word.strip_prefix("export=")) {
        let (spec, option) = word.split_once(',').unwrap_or((word, ""));
        if !matches!(option, "" | "ro") {
            return println!("sshd: export={word}: the only option is ,ro.");
        }
        let (name, folder) = spec.split_once('=').unwrap_or((spec, root));
        if let Err(reason) = node.exports.add(name, folder.trim_end_matches('/'), read_only || option == "ro") {
            return println!("sshd: export={word}: {reason}.");
        }
    }
    if node.exports.is_empty() {
        if let Err(reason) = node.exports.add("data", root, read_only) {
            return println!("sshd: root={root}: {reason}.");
        }
    }
    for index in 0..node.exports.len() {
        let folder = node.exports.root(index);
        if tape_rt::vfs_stat(folder).map_or(true, |stat| stat.kind != tape_rt::VFS_DIR) {
            return println!("sshd: {} is not a folder this kernel can read (needs tape ABI 32).", if folder.is_empty() { "the boot volume" } else { folder });
        }
    }

    let Some(buffer) = bytes(api, 8192) else { return println!("sshd: out of memory.") };
    let length = read_named(api, "authorized_keys", buffer);
    if length < 0 {
        return println!("sshd: no authorized_keys file (settings/authorized_keys): nobody could log in.");
    }
    if let Err(reason) = node.keys.parse(core::str::from_utf8(&buffer[..length as usize]).unwrap_or("")) {
        return println!("sshd: {reason}.");
    }
    println!("sshd: {} authorized key(s).", node.keys.len());

    // Two machines must not share a host key, so a kernel without `random` (ABI 33) cannot serve.
    let length = read_named(api, "ssh_host_key", buffer);
    let stored = if length > 0 { core::str::from_utf8(&buffer[..length as usize]).ok().and_then(parse_seed) } else { None };
    node.seed = match stored {
        Some(seed) => seed,
        None => {
            let mut seed = [0u8; 32];
            if tape_rt::random(&mut seed) != Some(true) {
                return println!("sshd: this kernel has no random numbers for a host key (needs tape ABI 33).");
            }
            let mut file = [0u8; 65];
            seed_text(&seed, &mut file);
            if write_named(api, "ssh_host_key", &file) != 0 {
                return println!("sshd: could not write settings/ssh_host_key.");
            }
            println!("sshd: made a new host key.");
            seed
        }
    };
    let mut print = [0u8; 50];
    fingerprint(&node.seed, &mut print);
    println!("sshd: host key ssh-ed25519 {}", core::str::from_utf8(&print).unwrap_or(""));

    let Some(port) = value(text, "port").map_or(Some(PORT), |word| word.parse::<u16>().ok().filter(|&port| port != 0)) else {
        return println!("sshd: port= takes a number from 1 to 65535.");
    };
    let nic = value(text, "nic").and_then(|word| word.parse::<usize>().ok()).map_or(tape_rt::active_nic(), |n| n.saturating_sub(1));
    if (api.nic_count)() <= nic {
        return println!("sshd: this machine has no NIC {}.", nic + 1);
    }
    let mut mac = [0u8; 6];
    (api.nic_mac)(nic, mac.as_mut_ptr());
    let mac = MacAddress(mac);
    let network: NetConfig = match value(text, "ip") {
        Some(ip) => match net::static_config(mac, ip, value(text, "gateway").unwrap_or("")) {
            Ok(network) => network,
            Err(reason) => return println!("sshd: {reason}."),
        },
        None => match net::dhcp(api, nic, mac) {
            Some(network) => network,
            None => return println!("sshd: no DHCP lease (give ip=A.B.C.D/N for a fixed address)."),
        },
    };
    let Some(tcp) = tape_rt::alloc_zeroed::<Tcp<RX, SLOTS>>(api) else { return println!("sshd: no memory for the connections.") };
    let mut arenas = [0usize; SLOTS];
    for arena in arenas.iter_mut() {
        match (api.alloc)(ARENA) {
            0 => return println!("sshd: no memory for the connections."),
            address => *arena = address,
        }
    }
    println!("sshd: NIC {} at {}.{}.{}.{}:{port}, room for {SLOTS} connections.", nic + 1, network.ip.0[0], network.ip.0[1], network.ip.0[2], network.ip.0[3]);
    for index in 0..node.exports.len() {
        let folder = node.exports.root(index);
        println!("sshd: serving {} as {}{}.", if folder.is_empty() { "the boot volume" } else { folder }, node.exports.name(index), if node.exports.read_only(index) { " (read-only)" } else { "" });
    }
    run(api, node, tcp, &arenas, nic, &network, port);
}

fn run(api: &TapeApi, node: &mut Node, tcp: &mut Tcp<RX, SLOTS>, arenas: &[usize; SLOTS], nic: usize, network: &NetConfig, port: u16) {
    tcp.start(nic, network, port);
    tcp.set_idle(IDLE_US);
    let mut sessions: [Option<Session<'static>>; SLOTS] = [const { None }; SLOTS];
    let mut since = [0u64; SLOTS];
    let (mut dirty_at, mut last_frame) = (0u64, 0u64);
    let mut scratch = [0u8; 2048];
    loop {
        if (api.should_abort)() != 0 {
            break;
        }
        let mut busy = tcp.poll(api, 16);
        for slot in 0..SLOTS {
            if !tcp.is_open(slot) {
                if sessions[slot].take().is_some() {
                    node.log(format_args!("connection closed"));
                }
                continue;
            }
            if sessions[slot].is_none() {
                // The previous session of this slot is gone, so its arena is free again.
                let arena = unsafe { core::slice::from_raw_parts_mut(arenas[slot] as *mut u8, ARENA) };
                sessions[slot] = Some(Session::new(node, arena));
                since[slot] = tape_rt::micros();
                let (ip, _) = tcp.peer(slot);
                node.log(format_args!("connection from {}.{}.{}.{}", ip.0[0], ip.0[1], ip.0[2], ip.0[3]));
            }
            let session = sessions[slot].as_mut().unwrap();
            loop {
                let used = session.feed(node, tcp.received(slot));
                if used == 0 {
                    break;
                }
                let mut left = used;
                while left > 0 {
                    let chunk = left.min(scratch.len());
                    let taken = tcp.take(api, slot, &mut scratch[..chunk]);
                    if taken == 0 {
                        break;
                    }
                    left -= taken;
                }
                busy = true;
                dirty_at = tape_rt::micros();
            }
            session.poll(node);
            while !session.output().is_empty() {
                match tcp.send_some(api, slot, session.output()) {
                    Ok(0) => break,
                    Ok(taken) => {
                        session.sent(taken);
                        busy = true;
                    }
                    Err(_) => {
                        tcp.close(api, slot);
                        break;
                    }
                }
            }
            let now = tape_rt::micros();
            if session.finished() && session.output().is_empty() {
                tcp.finish(api, slot);
            } else if !session.is_authenticated() && now - since[slot] > LOGIN_US {
                node.log(format_args!("login took too long"));
                tcp.close(api, slot);
            } else if tcp.peer_closed(slot) && tcp.received(slot).is_empty() {
                tcp.finish(api, slot);
            }
        }
        let now = tape_rt::micros();
        if dirty_at != 0 && now - dirty_at > SYNC_QUIET_US {
            dirty_at = 0;
            let _ = tape_rt::vfs_sync();
        }
        if busy {
            last_frame = now;
        } else if now - last_frame > SPIN_US {
            tape_rt::idle();
        }
    }
}
