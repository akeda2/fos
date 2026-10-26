//! netcheck: the smallest end-to-end test of a NIC through the tape ABI. Gets an address by DHCP, resolves the gateway (or
//! the host given) with ARP, and pings it four times, printing each round trip. Written for the Raspberry Pi 4's Genet,
//! where it is the first thing to run after the link comes up: a lease proves transmit and receive, the ping proves
//! ARP, IPv4 and ICMP checksums in both directions.
#![no_std]
#![no_main]

use tape_ip::net::{self, NetConfig};
use tape_ip::tcp::Socket;
use tape_net::{ArpCache, EthernetFrame, IcmpEcho, Ipv4Address, Ipv4Packet, ETHERTYPE_IPV4, MacAddress};
use tape_rt::{TapeApi, println};

const COUNT: u16 = 4;
/// The receive window of the `tcp=` test: also what a file-reading tape would use.
static mut SOCKET: Socket<16384> = Socket::new();
const IDENTIFIER: u16 = 0xf05b;

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(api: &'static TapeApi) {
    tape_rt::init(api);
    if (api.nic_count)() == 0 {
        println!("netcheck: no network interface.");
        return;
    }
    let mut mac = [0u8; 6];
    (api.nic_mac)(0, mac.as_mut_ptr());
    let mac = MacAddress(mac);
    println!("netcheck: NIC 0, MAC {mac:?}; asking DHCP (8 s)...");
    let Some(config) = net::dhcp(api, 0, mac) else {
        println!("netcheck: no DHCP lease: nothing answered (link? cable? server?).");
        return;
    };
    println!("netcheck: lease: address {:?}, mask {:?}, gateway {:?}.", config.ip, config.netmask, config.gateway);
    let arguments = core::str::from_utf8(tape_rt::args()).unwrap_or("").trim();
    let mut words = arguments.split_whitespace();
    let host = words.next().unwrap_or("");
    let tcp_port = words.next().and_then(|word| word.strip_prefix("tcp=")).and_then(|port| port.parse::<u16>().ok());
    let tcp_bytes = words.next().and_then(|word| word.parse::<u32>().ok()).unwrap_or(1 << 20);
    let target = if host.is_empty() { config.gateway } else { net::parse_ip(host).unwrap_or(config.gateway) };
    if target == Ipv4Address([0; 4]) {
        println!("netcheck: no gateway and no host given (tape run netcheck A.B.C.D).");
        return;
    }
    let mut arp = ArpCache::<8>::new();
    let Some(target_mac) = net::resolve(api, 0, &config, &mut arp, target) else {
        println!("netcheck: ARP for {target:?} got no answer.");
        return;
    };
    if let Some(port) = tcp_port {
        tcp_test(api, &config, target, port, tcp_bytes);
        return;
    }
    println!("netcheck: {target:?} is at {target_mac:?}; pinging.");
    let mut replies = 0;
    for sequence in 1..=COUNT {
        if (api.should_abort)() != 0 {
            return;
        }
        let started = tape_rt::micros();
        if !send_echo(api, &config, target_mac, target, sequence) {
            println!("netcheck: seq {sequence}: the NIC would not send.");
            continue;
        }
        match wait_reply(api, &config, &mut arp, target, sequence, started) {
            Some(microseconds) => {
                replies += 1;
                println!("netcheck: seq {sequence}: reply from {target:?} in {microseconds} us.");
            }
            None => println!("netcheck: seq {sequence}: no reply."),
        }
        let next = started + 1_000_000;
        while tape_rt::micros() < next {
            tape_rt::idle();
        }
    }
    println!("netcheck: {replies}/{COUNT} replies.");
}

fn send_echo(api: &TapeApi, config: &NetConfig, target_mac: MacAddress, target: Ipv4Address, sequence: u16) -> bool {
    let mut icmp = [0u8; 64];
    let request = IcmpEcho::request(IDENTIFIER, sequence, b"fosfor netcheck 0123456789abcdef");
    let Ok(length) = request.encode(&mut icmp) else { return false };
    let mut frame = [0u8; net::MAX_FRAME];
    let Some(total) = net::build_ip_frame(&mut frame, config, target_mac, target, 1, &icmp[..length]) else { return false };
    net::send_frame(api, 0, &frame[..total])
}

/// Waits up to a second for the echo reply to `sequence`; other frames are ARP-answered or ignored.
fn wait_reply(api: &TapeApi, config: &NetConfig, arp: &mut ArpCache<8>, target: Ipv4Address, sequence: u16, started: u64) -> Option<u64> {
    let mut frame = [0u8; net::MAX_FRAME];
    while tape_rt::micros() < started + 1_000_000 {
        let length = (api.nic_receive)(0, frame.as_mut_ptr(), frame.len());
        if length == 0 {
            tape_rt::idle();
            continue;
        }
        let now = tape_rt::micros();
        if let Ok(ethernet) = EthernetFrame::parse(&frame[..length]) {
            if ethernet.ethertype == ETHERTYPE_IPV4 {
                if let Ok(packet) = Ipv4Packet::parse(ethernet.payload, true) {
                    if packet.header.protocol == 1 && packet.header.source == target {
                        if let Ok(echo) = IcmpEcho::parse(packet.payload, true) {
                            if echo.message_type == IcmpEcho::REPLY && echo.identifier == IDENTIFIER && echo.sequence == sequence {
                                return Some(now - started);
                            }
                        }
                    }
                }
                continue;
            }
        }
        let _ = net::inbound(api, 0, config, arp, &frame[..length], 0..=0, 0);
    }
    None
}

tape_rt::tape_help!(br#"netcheck: DHCP, ARP and ping through the first NIC.

  tape run netcheck [A.B.C.D]

  Gets a lease, resolves the gateway (or the given host) and sends four
  ICMP echo requests, one a second. Escape stops.
"#);

/// `netcheck HOST tcp=PORT [BYTES]`: sends "GET BYTES\n" to a TCP server, which answers with BYTES bytes where byte i is
/// `i % 251`; checks every one and prints the throughput. Proves the shared TCP client (`tape_ip::tcp`) end to end.
fn tcp_test(api: &TapeApi, config: &NetConfig, target: Ipv4Address, port: u16, total: u32) {
    #[allow(static_mut_refs)]
    let socket = unsafe { &mut *core::ptr::addr_of_mut!(SOCKET) };
    println!("netcheck: TCP to {target:?}:{port}, asking for {total} bytes.");
    if let Err(why) = socket.connect(api, 0, config, target, port, 3_000_000) {
        println!("netcheck: tcp: {why}.");
        return;
    }
    let mut request = [0u8; 24];
    let mut length = 0;
    for byte in b"GET " {
        request[length] = *byte;
        length += 1;
    }
    let mut digits = [0u8; 10];
    let mut count = 0;
    let mut value = total;
    loop {
        digits[count] = b'0' + (value % 10) as u8;
        count += 1;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    while count > 0 {
        count -= 1;
        request[length] = digits[count];
        length += 1;
    }
    request[length] = b'\n';
    length += 1;
    let started = tape_rt::micros();
    if let Err(why) = socket.send(api, &request[..length], 2_000_000) {
        println!("netcheck: tcp: {why}.");
        return;
    }
    let mut buffer = [0u8; 4096];
    let mut received: u32 = 0;
    let mut bad: u32 = 0;
    while received < total {
        if (api.should_abort)() != 0 {
            break;
        }
        let want = buffer.len().min((total - received) as usize);
        match socket.recv(api, &mut buffer[..want], 3_000_000) {
            Ok(0) => {
                println!("netcheck: tcp: closed after {received} bytes.");
                break;
            }
            Ok(count) => {
                for (offset, byte) in buffer[..count].iter().enumerate() {
                    if *byte != ((received as usize + offset) % 251) as u8 {
                        bad += 1;
                    }
                }
                received += count as u32;
            }
            Err(why) => {
                println!("netcheck: tcp: {why} after {received} bytes.");
                break;
            }
        }
    }
    let micros = (tape_rt::micros() - started).max(1);
    socket.close(api);
    let kbit = received as u64 * 8 * 1000 / micros;
    println!("netcheck: tcp: {received} bytes in {} ms, {kbit} kbit/s, {bad} wrong.", micros / 1000);
    if received == total && bad == 0 {
        println!("netcheck: tcp: OK.");
    }
}
