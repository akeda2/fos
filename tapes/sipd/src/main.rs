//! sipd: a SIP registrar and stateless proxy (`crates/tape-sip` `registrar`) as a service, so that machines and phones
//! register under a name, call each other by it, and keep their signalling path through one fixed address. Digest
//! authentication against a users file; media is not relayed. Meant to run as a service (`run sipd @SIPD.CFG &2`).
#![no_std]
#![no_main]

use tape_ip::net::{self, Inbound, NetConfig};
use tape_net::{ArpCache, Ipv4Address, MacAddress};
use tape_rt::config::Config;
use tape_rt::{TapeApi, arg_value as value, println};
use tape_sip::registrar::{self, Server};

tape_rt::tape_help!(
    br#"sipd [@NAME] [ip=A.B.C.D/N] [gateway=A.B.C.D] [nic=N] [port=5060]
     [realm=fos] [users=sipusers] [log]
A SIP registrar and proxy (UDP). Phones and machines REGISTER under a user name
(digest authentication), then call each other as sip:USER@THIS-ADDRESS; the
proxy stays in the signalling path (Record-Route), so NAT bindings stay open.
Audio is not relayed: media flows between the two ends.
users=  file of lines user:password (# comments), in settings/ or the root; it
        is read again every 30 s. A password with a colon is fine.
ip=     a fixed address (recommended: clients are told this one); without it
        DHCP. nic= is the NIC number from 1 (default: nic use).
log     print registrations and refusals.
Run as a service: run sipd @SIPD.CFG &2. No TLS, no TCP, IPv4 addresses only.
"#
);

const USERS_BYTES: usize = 4096;
const MACS: usize = 32;

/// MAC addresses seen on frames from this subnet, so a reply needs no ARP exchange.
struct Macs {
    entries: [(Ipv4Address, MacAddress); MACS],
    next: usize,
}

impl Macs {
    fn learn(&mut self, ip: Ipv4Address, mac: MacAddress) {
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.0 == ip) {
            entry.1 = mac;
            return;
        }
        self.entries[self.next] = (ip, mac);
        self.next = (self.next + 1) % MACS;
    }

    fn get(&self, ip: Ipv4Address) -> Option<MacAddress> {
        self.entries.iter().find(|entry| entry.0 == ip && ip.0 != [0; 4]).map(|entry| entry.1)
    }
}

fn same_subnet(network: &NetConfig, other: Ipv4Address) -> bool {
    (0..4).all(|i| network.ip.0[i] & network.netmask.0[i] == other.0[i] & network.netmask.0[i])
}

fn load_users(api: &TapeApi, name: &str, buffer: &mut [u8]) -> usize {
    let located = tape_rt::config::locate(api, name);
    if !located.exists {
        return 0;
    }
    let path = located.path();
    match (api.fs_read)(path.as_ptr(), path.len(), buffer.as_mut_ptr(), buffer.len()) {
        length if length >= 0 => length as usize,
        _ => 0,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(api: &'static TapeApi) {
    tape_rt::init(api);
    let config = match Config::load(api, "SIPD", core::str::from_utf8(tape_rt::args()).unwrap_or("")) {
        Ok(config) => config,
        Err(reason) => return println!("sipd: {reason}."),
    };
    let text = config.text();
    let Some(port) = value(text, "port").map_or(Some(5060), |word| word.parse::<u16>().ok().filter(|&port| port != 0)) else {
        return println!("sipd: port= takes a number from 1 to 65535.");
    };
    let realm = value(text, "realm").unwrap_or("fos");
    let users_name = value(text, "users").unwrap_or("sipusers");
    let log = text.split_ascii_whitespace().any(|word| word == "log" || word == "log=on");
    let nic = value(text, "nic").and_then(|word| word.parse::<usize>().ok()).map_or(tape_rt::active_nic(), |n| n.saturating_sub(1));
    if (api.nic_count)() <= nic {
        return println!("sipd: this machine has no NIC {}.", nic + 1);
    }
    let mut mac = [0u8; 6];
    (api.nic_mac)(nic, mac.as_mut_ptr());
    let mac = MacAddress(mac);
    let mut lease = None;
    let mut network: NetConfig = match value(text, "ip") {
        Some(ip) => match net::static_config(mac, ip, value(text, "gateway").unwrap_or("")) {
            Ok(network) => network,
            Err(reason) => return println!("sipd: {reason}."),
        },
        None => match net::DhcpTask::bind(api, nic, mac) {
            Some((task, network)) => {
                lease = Some(task);
                network
            }
            None => return println!("sipd: no DHCP lease (give ip=A.B.C.D/N for a fixed address)."),
        },
    };
    let Some(users) = tape_rt::alloc_bytes(api, USERS_BYTES) else { return println!("sipd: out of memory.") };
    let mut users_length = load_users(api, users_name, users);
    let mut secret_bytes = [0u8; 4];
    if tape_rt::random(&mut secret_bytes).is_none() {
        secret_bytes = (tape_rt::micros() as u32).to_le_bytes();
    }
    let secret = u32::from_le_bytes(secret_bytes);
    let mut server = Server::new();
    let mut macs = Macs { entries: [(Ipv4Address([0; 4]), MacAddress([0; 6])); MACS], next: 0 };
    let mut arp = ArpCache::<8>::new();
    let ip = network.ip.0;
    println!("sipd: NIC {} at {}.{}.{}.{}:{port}, realm {realm}, {} bytes of users from {users_name}.", nic + 1, ip[0], ip[1], ip[2], ip[3], users_length);
    if users_length == 0 {
        println!("sipd: no users file {users_name}: nobody can register (lines of user:password).");
    }
    let mut frame = [0u8; 1514];
    let mut out = [0u8; tape_sip::ua::MAX_DATAGRAM];
    let mut counters = (0, 0, 0);
    let mut last_reload = tape_rt::micros();
    loop {
        if (api.should_abort)() != 0 {
            break;
        }
        let now = tape_rt::micros();
        if now - last_reload > 30_000_000 {
            last_reload = now;
            users_length = load_users(api, users_name, users);
        }
        let length = (api.nic_receive)(nic, frame.as_mut_ptr(), frame.len());
        if length == 0 {
            if let Some(task) = lease.as_mut() {
                if let net::Lease::Replaced(Some(new)) = task.maintain(api, None) {
                    network = new;
                    println!("sipd: DHCP gave another address; clients must register again at it.");
                }
            }
            tape_rt::idle();
            continue;
        }
        let frame = &frame[..length];
        let Inbound::Udp { source, source_port, destination_port, payload } = net::inbound(api, nic, &network, &mut arp, frame, 68..=port, 0) else { continue };
        if destination_port == 68 {
            if let Some(task) = lease.as_mut() {
                task.maintain(api, Some(frame));
            }
            continue;
        }
        if destination_port != port {
            continue;
        }
        if same_subnet(&network, source) {
            macs.learn(source, MacAddress(frame[6..12].try_into().unwrap()));
        }
        let users_text = core::str::from_utf8(&users[..users_length]).unwrap_or("");
        let settings = registrar::Config { ip: network.ip.0, port, realm, users: users_text, secret };
        let Some(reply) = server.handle(now / 1000, (source.0, source_port), payload, &settings, &mut out) else { continue };
        let destination = Ipv4Address(reply.to.0);
        let Some(destination_mac) = macs.get(destination).or_else(|| net::resolve(api, nic, &network, &mut arp, destination)) else { continue };
        let mut built = [0u8; 1514];
        if let Some(total) = net::build_udp_frame(&mut built, &network, destination_mac, destination, port, reply.to.1, 64, &out[..reply.length]) {
            net::send_frame(api, nic, &built[..total]);
        }
        let now_counters = (server.registered, server.refused, server.challenged);
        if log && now_counters != counters {
            let (a, b, c) = (now_counters.0 - counters.0, now_counters.1 - counters.1, now_counters.2 - counters.2);
            println!("sipd: {} bindings; +{a} registered, +{b} refused, +{c} challenged (from {}.{}.{}.{}).", server.count(now / 1000), source.0[0], source.0[1], source.0[2], source.0[3]);
            counters = now_counters;
        }
    }
}
