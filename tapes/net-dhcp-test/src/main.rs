#![no_std]
#![no_main]

//! A persistent physical-NIC DHCP connectivity test.
//!
//! The tape deliberately stays simple and noisy: it enumerates every
//! kernel-owned NIC, runs the shared fixed-buffer DHCP client on one
//! interface at a time, and does not leave that interface until it reaches
//! `Bound`. It prints bounded progress counters so a physical test can
//! distinguish "no DHCP server answered" from "the NIC is not receiving",
//! "packet parsing rejected the traffic", or "transmit failed". A short PC
//! speaker beep marks every successfully acquired address.
//!
//! Run it with:
//!
//! ```text
//! tape load disk NET-DHCP.BIN
//! tape run
//! tape run 192.168.0.1 5
//! ```
//!
//! The optional arguments are an IPv4 host and a positive ping interval in
//! seconds. When supplied, each leased NIC resolves the host with ARP and
//! sends ICMP echo requests at that interval; without arguments, the tape
//! only reports DHCP status. A host outside the leased subnet is reached via
//! the DHCP-provided gateway.
//!
//! F12/F4/F6/F7/F8/F10 (or the corresponding remote Escape event) remains
//! the explicit operator escape. There is intentionally no DHCP timeout:
//! after a failed retry budget, `DhcpClient` keeps retrying forever until the
//! interface gets an address or the operator aborts the tape.

use core::arch::asm;

use tape_net::{
    ArpOperation, ArpPacket, DHCP_CLIENT_PORT, DHCP_MAX_DNS_SERVERS, DHCP_SERVER_PORT, DhcpClient,
    DhcpClientState, DhcpPacket, DhcpTransmit, ETHERTYPE_ARP, ETHERTYPE_IPV4, EthernetFrame,
    IcmpEcho, Ipv4Address, Ipv4Header, Ipv4Packet, MacAddress, UdpDatagram,
};
use tape_rt::{print_bytes, print_u32};

const MAX_FRAME_LEN: usize = 1514;
const DHCP_PAYLOAD_LEN: usize = 600;
const DHCP_RETRY_TICKS: u32 = 1;
const DHCP_MAX_RETRIES: u8 = 5;
const PROGRESS_INTERVAL_SECONDS: u32 = 5;
const PIT_COMMAND: u16 = 0x43;
const PIT_CHANNEL2_DATA: u16 = 0x42;
const PIT_CHANNEL0_DATA: u16 = 0x40;
const SPEAKER_PORT: u16 = 0x61;
const PIT_INPUT_HZ: u32 = 1_193_182;
const TONE_HZ: u32 = 440;
const BEEP_TICKS: u32 = 10;
const TAPE_EVENT_ABORT: u32 = 1;
const MAX_STATUS_NICS: usize = 16;
const PING_TIMEOUT_SECONDS: u32 = 5;

type Putc = extern "C" fn(u32);
type ShouldAbort = extern "C" fn() -> u32;
type NicCount = extern "C" fn() -> u32;
type NicMacAt = extern "C" fn(u32, u32) -> u32;
type NicSendAt = extern "C" fn(u32, u32, u32) -> u32;
type NicReceiveAt = extern "C" fn(u32, u32, u32) -> u32;
type NicMaxFrameLen = extern "C" fn() -> u32;
type Ticks = extern "C" fn() -> u64;

#[derive(Clone, Copy)]
struct Counters {
    tx_attempts: u32,
    tx_ok: u32,
    tx_errors: u32,
    rx_frames: u32,
    rx_parse_errors: u32,
    rx_unexpected: u32,
    dhcp_messages: u32,
    dhcp_errors: u32,
}

impl Counters {
    const fn new() -> Self {
        Self {
            tx_attempts: 0,
            tx_ok: 0,
            tx_errors: 0,
            rx_frames: 0,
            rx_parse_errors: 0,
            rx_unexpected: 0,
            dhcp_messages: 0,
            dhcp_errors: 0,
        }
    }
}

#[derive(Clone, Copy)]
struct AcquiredNic {
    mac: MacAddress,
    address: Ipv4Address,
    subnet_mask: Ipv4Address,
    gateway: Ipv4Address,
    dns_servers: [Ipv4Address; DHCP_MAX_DNS_SERVERS],
    dns_server_count: usize,
    counters: Counters,
    ping_sent: u32,
    ping_received: u32,
}

#[inline]
unsafe fn inb(port: u16) -> u8 {
    let value: u8;
    unsafe {
        asm!(
            "in al, dx",
            in("dx") port,
            out("al") value,
            options(nomem, nostack, preserves_flags)
        );
    }
    value
}

#[inline]
unsafe fn outb(port: u16, value: u8) {
    unsafe {
        asm!(
            "out dx, al",
            in("dx") port,
            in("al") value,
            options(nomem, nostack, preserves_flags)
        );
    }
}

fn pit_channel0_count() -> u16 {
    unsafe {
        outb(PIT_COMMAND, 0);
        let low = inb(PIT_CHANNEL0_DATA);
        let high = inb(PIT_CHANNEL0_DATA);
        u16::from_le_bytes([low, high])
    }
}

fn beep() {
    let divisor = (PIT_INPUT_HZ / TONE_HZ) as u16;
    unsafe {
        outb(PIT_COMMAND, 0xb6);
        outb(PIT_CHANNEL2_DATA, divisor as u8);
        outb(PIT_CHANNEL2_DATA, (divisor >> 8) as u8);
        let control = inb(SPEAKER_PORT);
        outb(SPEAKER_PORT, control | 0x03);
    }
    let mut previous = pit_channel0_count();
    let mut elapsed = 0;
    while elapsed < BEEP_TICKS {
        let current = pit_channel0_count();
        if current > previous {
            elapsed += 1;
        }
        previous = current;
        core::hint::spin_loop();
    }
    unsafe {
        let control = inb(SPEAKER_PORT);
        outb(SPEAKER_PORT, control & !0x03);
    }
}

fn print_ip(putc: Putc, address: Ipv4Address) {
    for (index, octet) in address.0.iter().copied().enumerate() {
        if index != 0 {
            putc(u32::from(b'.'));
        }
        print_u32(putc, u32::from(octet));
    }
}

fn print_hex_byte(putc: Putc, value: u8) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    putc(u32::from(HEX[(value >> 4) as usize]));
    putc(u32::from(HEX[(value & 0x0f) as usize]));
}

fn print_counter_line(putc: Putc, nic: usize, counters: Counters) {
    print_bytes(putc, b"NIC ");
    print_u32(putc, (nic + 1) as u32);
    print_bytes(putc, b" progress: tx ");
    print_u32(putc, counters.tx_ok);
    print_bytes(putc, b"/");
    print_u32(putc, counters.tx_attempts);
    print_bytes(putc, b" (");
    print_u32(putc, counters.tx_errors);
    print_bytes(putc, b" errors), rx ");
    print_u32(putc, counters.rx_frames);
    print_bytes(putc, b", parse ");
    print_u32(putc, counters.rx_parse_errors);
    print_bytes(putc, b", unexpected ");
    print_u32(putc, counters.rx_unexpected);
    print_bytes(putc, b", DHCP ");
    print_u32(putc, counters.dhcp_messages);
    print_bytes(putc, b" (");
    print_u32(putc, counters.dhcp_errors);
    print_bytes(putc, b" errors).\n");
}

fn build_frame(
    mac: MacAddress,
    payload: &[u8],
    transmit: DhcpTransmit,
    frame: &mut [u8],
    udp: &mut [u8],
    ip: &mut [u8],
) -> Option<usize> {
    let udp_len = UdpDatagram::new(transmit.source_port, transmit.destination_port, payload)
        .encode_with_checksum(udp, transmit.source, transmit.destination, false)
        .ok()?;
    let ip_len = Ipv4Packet {
        header: Ipv4Header::new(transmit.source, transmit.destination, 17, udp_len),
        payload: &udp[..udp_len],
    }
    .encode(ip)
    .ok()?;
    EthernetFrame::new(MacAddress::BROADCAST, mac, ETHERTYPE_IPV4, &ip[..ip_len])
        .encode(frame)
        .ok()
}

fn parse_ipv4(text: &[u8]) -> Option<Ipv4Address> {
    let mut octets = [0u8; 4];
    let mut octet = 0u32;
    let mut index = 0usize;
    let mut have_digit = false;
    for &byte in text.iter().chain(core::iter::once(&b'.')) {
        if byte.is_ascii_digit() {
            octet = octet.checked_mul(10)?.checked_add(u32::from(byte - b'0'))?;
            if octet > 255 {
                return None;
            }
            have_digit = true;
        } else if byte == b'.' && have_digit && index < 4 {
            octets[index] = octet as u8;
            index += 1;
            octet = 0;
            have_digit = false;
        } else {
            return None;
        }
    }
    (index == 4).then_some(Ipv4Address::new(octets))
}

fn same_subnet(left: Ipv4Address, right: Ipv4Address, mask: Ipv4Address) -> bool {
    left.0
        .iter()
        .zip(right.0.iter())
        .zip(mask.0.iter())
        .all(|((&a, &b), &m)| (a & m) == (b & m))
}

fn send_arp_request(
    mac: MacAddress,
    address: Ipv4Address,
    target: Ipv4Address,
    nic_send_at: NicSendAt,
    nic: usize,
    frame: &mut [u8],
    arp: &mut [u8],
) -> bool {
    let Ok(arp_len) = ArpPacket::request(mac, address, target).encode(arp) else {
        return false;
    };
    let Ok(frame_len) =
        EthernetFrame::new(MacAddress::BROADCAST, mac, ETHERTYPE_ARP, &arp[..arp_len])
            .encode(frame)
    else {
        return false;
    };
    nic_send_at(nic as u32, frame.as_ptr() as u32, frame_len as u32) != 0
}

fn resolve_mac(
    nic: usize,
    mac: MacAddress,
    address: Ipv4Address,
    target: Ipv4Address,
    nic_send_at: NicSendAt,
    nic_receive_at: NicReceiveAt,
    max_frame_len: usize,
    ticks: Ticks,
    frame: &mut [u8],
    arp: &mut [u8],
) -> Option<MacAddress> {
    if !send_arp_request(mac, address, target, nic_send_at, nic, frame, arp) {
        return None;
    }
    let deadline = (ticks() / 100).saturating_add(PING_TIMEOUT_SECONDS as u64 * 100);
    while (ticks() / 100) < deadline {
        let length =
            nic_receive_at(nic as u32, frame.as_mut_ptr() as u32, max_frame_len as u32) as usize;
        if length == 0 {
            core::hint::spin_loop();
            continue;
        }
        let Ok(ethernet) = EthernetFrame::parse(&frame[..length.min(MAX_FRAME_LEN)]) else {
            continue;
        };
        if ethernet.ethertype != ETHERTYPE_ARP {
            continue;
        }
        let Ok(packet) = ArpPacket::parse(ethernet.payload) else {
            continue;
        };
        if packet.operation == ArpOperation::Reply
            && packet.sender_protocol == target
            && packet.target_protocol == address
        {
            return Some(packet.sender_hardware);
        }
    }
    None
}

fn ping_once(
    putc: Putc,
    nic: usize,
    mac: MacAddress,
    status: &AcquiredNic,
    target: Ipv4Address,
    identifier: u16,
    nic_send_at: NicSendAt,
    nic_receive_at: NicReceiveAt,
    max_frame_len: usize,
    ticks: Ticks,
    frame: &mut [u8],
    arp: &mut [u8],
    icmp: &mut [u8],
    ip: &mut [u8],
) -> bool {
    let next_hop = if same_subnet(status.address, target, status.subnet_mask) {
        target
    } else {
        status.gateway
    };
    let Some(destination_mac) = resolve_mac(
        nic,
        mac,
        status.address,
        next_hop,
        nic_send_at,
        nic_receive_at,
        max_frame_len,
        ticks,
        frame,
        arp,
    ) else {
        print_bytes(putc, b"NIC ");
        print_u32(putc, (nic + 1) as u32);
        print_bytes(putc, b" ping ");
        print_ip(putc, target);
        print_bytes(putc, b": ARP timeout.\n");
        return false;
    };
    let echo = IcmpEcho::request(identifier, 1, b"fos DHCP test");
    let Ok(icmp_len) = echo.encode(icmp) else {
        return false;
    };
    let Ok(ip_len) = (Ipv4Packet {
        header: Ipv4Header::new(status.address, target, 1, icmp_len),
        payload: &icmp[..icmp_len],
    })
    .encode(ip) else {
        return false;
    };
    let Ok(frame_len) =
        EthernetFrame::new(destination_mac, mac, ETHERTYPE_IPV4, &ip[..ip_len]).encode(frame)
    else {
        return false;
    };
    if nic_send_at(nic as u32, frame.as_ptr() as u32, frame_len as u32) == 0 {
        print_bytes(putc, b"NIC ");
        print_u32(putc, (nic + 1) as u32);
        print_bytes(putc, b" ping ");
        print_ip(putc, target);
        print_bytes(putc, b": transmit error.\n");
        return false;
    }
    let deadline = (ticks() / 100).saturating_add(PING_TIMEOUT_SECONDS as u64 * 100);
    while (ticks() / 100) < deadline {
        let length =
            nic_receive_at(nic as u32, frame.as_mut_ptr() as u32, max_frame_len as u32) as usize;
        if length == 0 {
            core::hint::spin_loop();
            continue;
        }
        let Ok(ethernet) = EthernetFrame::parse(&frame[..length.min(MAX_FRAME_LEN)]) else {
            continue;
        };
        if ethernet.ethertype != ETHERTYPE_IPV4 {
            continue;
        }
        let Ok(packet) = Ipv4Packet::parse(ethernet.payload, true) else {
            continue;
        };
        if packet.header.protocol != 1
            || packet.header.source != target
            || packet.header.destination != status.address
        {
            continue;
        }
        let Ok(reply) = IcmpEcho::parse(packet.payload, true) else {
            continue;
        };
        if reply.message_type == IcmpEcho::REPLY && reply.identifier == identifier {
            print_bytes(putc, b"NIC ");
            print_u32(putc, (nic + 1) as u32);
            print_bytes(putc, b" ping ");
            print_ip(putc, target);
            print_bytes(putc, b": reply received.\n");
            return true;
        }
    }
    print_bytes(putc, b"NIC ");
    print_u32(putc, (nic + 1) as u32);
    print_bytes(putc, b" ping ");
    print_ip(putc, target);
    print_bytes(putc, b": timeout.\n");
    false
}

fn receive_dhcp(client: &mut DhcpClient<64>, frame: &[u8], now: u32, counters: &mut Counters) {
    let Ok(ethernet) = EthernetFrame::parse(frame) else {
        counters.rx_parse_errors += 1;
        return;
    };
    if ethernet.ethertype != ETHERTYPE_IPV4 {
        counters.rx_unexpected += 1;
        return;
    }
    let Ok(packet) = Ipv4Packet::parse(ethernet.payload, true) else {
        counters.rx_parse_errors += 1;
        return;
    };
    if packet.header.protocol != 17 {
        counters.rx_unexpected += 1;
        return;
    }
    let Ok(datagram) = UdpDatagram::parse(
        packet.payload,
        false,
        packet.header.source,
        packet.header.destination,
    ) else {
        counters.rx_parse_errors += 1;
        return;
    };
    if datagram.source_port != DHCP_SERVER_PORT || datagram.destination_port != DHCP_CLIENT_PORT {
        counters.rx_unexpected += 1;
        return;
    }
    let Ok(dhcp) = DhcpPacket::parse(datagram.payload) else {
        counters.rx_parse_errors += 1;
        return;
    };
    counters.dhcp_messages += 1;
    if client.receive(now, &dhcp).is_err() {
        counters.dhcp_errors += 1;
    }
}

fn run_interface(
    putc: Putc,
    should_abort: ShouldAbort,
    nic: usize,
    mac: MacAddress,
    nic_send_at: NicSendAt,
    nic_receive_at: NicReceiveAt,
    max_frame_len: usize,
    ticks: Ticks,
) -> Option<AcquiredNic> {
    let mut client = DhcpClient::<64>::new(
        mac,
        0x4e445443 ^ nic as u32,
        DHCP_RETRY_TICKS,
        DHCP_MAX_RETRIES,
    );
    let mut counters = Counters::new();
    let mut dhcp_payload = [0u8; DHCP_PAYLOAD_LEN];
    let mut frame = [0u8; MAX_FRAME_LEN];
    let mut udp = [0u8; MAX_FRAME_LEN];
    let mut ip = [0u8; MAX_FRAME_LEN];
    let mut next_progress = 0u32;

    loop {
        if should_abort() == TAPE_EVENT_ABORT {
            return None;
        }
        let now = (ticks() / 100) as u32;
        match client.poll(now, &mut dhcp_payload) {
            Ok(Some(length)) => {
                counters.tx_attempts += 1;
                let sent = build_frame(
                    mac,
                    &dhcp_payload[..length],
                    client.transmit(),
                    &mut frame,
                    &mut udp,
                    &mut ip,
                )
                .is_some_and(|length| {
                    length <= max_frame_len
                        && nic_send_at(nic as u32, frame.as_ptr() as u32, length as u32) != 0
                });
                if sent {
                    counters.tx_ok += 1;
                } else {
                    counters.tx_errors += 1;
                }
            }
            Ok(None) => {}
            Err(_) => {
                counters.dhcp_errors += 1;
            }
        }
        if client.state() == DhcpClientState::Failed {
            // The library bounds one retry cycle.  The diagnostic tape does
            // not: restart that cycle and keep trying until this NIC binds.
            client = DhcpClient::<64>::new(
                mac,
                0x4e445443 ^ nic as u32 ^ counters.tx_attempts,
                DHCP_RETRY_TICKS,
                DHCP_MAX_RETRIES,
            );
        }

        for _ in 0..8 {
            let length = nic_receive_at(nic as u32, frame.as_mut_ptr() as u32, max_frame_len as u32)
                as usize;
            if length == 0 {
                break;
            }
            counters.rx_frames += 1;
            receive_dhcp(
                &mut client,
                &frame[..length.min(MAX_FRAME_LEN)],
                now,
                &mut counters,
            );
        }

        if client.state() == DhcpClientState::Bound {
            if let Some(lease) = client.lease() {
                print_bytes(putc, b"NIC ");
                print_u32(putc, (nic + 1) as u32);
                print_bytes(putc, b" acquired ");
                print_ip(putc, lease.address);
                print_bytes(putc, b"/");
                print_ip(putc, lease.subnet_mask);
                print_bytes(putc, b", gateway ");
                print_ip(putc, lease.router);
                print_bytes(putc, b".\n");
                print_bytes(putc, b"NIC ");
                print_u32(putc, (nic + 1) as u32);
                print_bytes(putc, b" DHCP DNS: ");
                if lease.dns_server_count == 0 {
                    print_bytes(putc, b"none");
                } else {
                    for (index, server) in lease
                        .dns_servers
                        .iter()
                        .copied()
                        .take(lease.dns_server_count)
                        .enumerate()
                    {
                        if index != 0 {
                            print_bytes(putc, b",");
                        }
                        print_ip(putc, server);
                    }
                }
                print_bytes(putc, b".\n");
                print_counter_line(putc, nic, counters);
                beep();
                print_bytes(putc, b"NIC ");
                print_u32(putc, (nic + 1) as u32);
                print_bytes(putc, b" beep complete.\n");
                return Some(AcquiredNic {
                    mac,
                    address: lease.address,
                    subnet_mask: lease.subnet_mask,
                    gateway: lease.router,
                    dns_servers: lease.dns_servers,
                    dns_server_count: lease.dns_server_count,
                    counters,
                    ping_sent: 0,
                    ping_received: 0,
                });
            }
        }
        if now >= next_progress {
            print_counter_line(putc, nic, counters);
            next_progress = now.saturating_add(PROGRESS_INTERVAL_SECONDS);
        }
        core::hint::spin_loop();
    }
}

fn print_acquired_status(putc: Putc, nic: usize, status: AcquiredNic) {
    print_bytes(putc, b"NIC ");
    print_u32(putc, (nic + 1) as u32);
    print_bytes(putc, b" status: IP ");
    print_ip(putc, status.address);
    print_bytes(putc, b"/");
    print_ip(putc, status.subnet_mask);
    print_bytes(putc, b", gateway ");
    print_ip(putc, status.gateway);
    print_bytes(putc, b"; ");
    print_bytes(putc, b"DNS ");
    if status.dns_server_count == 0 {
        print_bytes(putc, b"none");
    } else {
        for (index, server) in status
            .dns_servers
            .iter()
            .copied()
            .take(status.dns_server_count)
            .enumerate()
        {
            if index != 0 {
                print_bytes(putc, b",");
            }
            print_ip(putc, server);
        }
    }
    print_bytes(putc, b"; ");
    print_counter_line(putc, nic, status.counters);
    if status.ping_sent != 0 {
        print_bytes(putc, b"NIC ");
        print_u32(putc, (nic + 1) as u32);
        print_bytes(putc, b" ping: ");
        print_u32(putc, status.ping_received);
        print_bytes(putc, b"/");
        print_u32(putc, status.ping_sent);
        print_bytes(putc, b" replies.\n");
    }
}

fn parse_options(putc: Putc, args_ptr: u32, args_len: u32) -> Option<(Ipv4Address, u32)> {
    if args_len == 0 {
        return None;
    }
    let args = unsafe { core::slice::from_raw_parts(args_ptr as *const u8, args_len as usize) };
    let mut words = args
        .split(|byte| byte.is_ascii_whitespace())
        .filter(|word| !word.is_empty());
    let Some(host) = words.next().and_then(parse_ipv4) else {
        print_bytes(
            putc,
            b"Network DHCP test: invalid ping host; use A.B.C.D [interval].\n",
        );
        return None;
    };
    let interval = match words.next() {
        Some(word) => match core::str::from_utf8(word)
            .ok()
            .and_then(|text| text.parse::<u32>().ok())
        {
            Some(value) if value != 0 => value,
            _ => {
                print_bytes(
                    putc,
                    b"Network DHCP test: ping interval must be a positive number of seconds.\n",
                );
                return None;
            }
        },
        None => 1,
    };
    if words.next().is_some() {
        print_bytes(
            putc,
            b"Network DHCP test: usage is `tape run [host] [interval]`.\n",
        );
        return None;
    }
    Some((host, interval))
}

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(
    putc: Putc,
    should_abort: ShouldAbort,
    _alloc: extern "C" fn(u32) -> u32,
    _free: extern "C" fn(u32, u32),
    _read_key: extern "C" fn() -> u32,
    _args_ptr: u32,
    _args_len: u32,
    _nic_mac: extern "C" fn(u32) -> u32,
    _nic_send: extern "C" fn(u32, u32) -> u32,
    _nic_receive: extern "C" fn(u32, u32) -> u32,
    nic_max_frame_len: NicMaxFrameLen,
    ticks: Ticks,
    nic_count: NicCount,
    nic_mac_at: NicMacAt,
    nic_send_at: NicSendAt,
    nic_receive_at: NicReceiveAt,
    _nic_set_promiscuous_at: extern "C" fn(u32, u32) -> u32,
    _terminal_position: extern "C" fn() -> u32,
    _fs_file_length: extern "C" fn(u32, u32, u32) -> u32,
    _fs_read: extern "C" fn(u32, u32, u32, u32, u32) -> u32,
    _fs_write: extern "C" fn(u32, u32, u32, u32) -> u32,
    _fs_remove: extern "C" fn(u32, u32) -> u32,
) {
    let ping_options = parse_options(putc, _args_ptr, _args_len);
    print_bytes(
        putc,
        if ping_options.is_some() {
            b"Network DHCP test: every NIC will retry until it acquires an IP; ping monitoring enabled.\n"
        } else {
            b"Network DHCP test: every NIC will retry until it acquires an IP; \
F12 (or F4/F6/F7/F8/F10) aborts.\n"
        },
    );
    let count = nic_count() as usize;
    let max_frame_len = (nic_max_frame_len() as usize).min(MAX_FRAME_LEN);
    if count == 0 || max_frame_len < EthernetFrame::HEADER_LEN {
        print_bytes(putc, b"Network DHCP test cannot run: no usable NICs.\n");
        return;
    }
    let status_count = count.min(MAX_STATUS_NICS);
    let mut statuses = [AcquiredNic {
        mac: MacAddress::ZERO,
        address: Ipv4Address::UNSPECIFIED,
        subnet_mask: Ipv4Address::UNSPECIFIED,
        gateway: Ipv4Address::UNSPECIFIED,
        dns_servers: [Ipv4Address::UNSPECIFIED; DHCP_MAX_DNS_SERVERS],
        dns_server_count: 0,
        counters: Counters::new(),
        ping_sent: 0,
        ping_received: 0,
    }; MAX_STATUS_NICS];
    for nic in 0..status_count {
        let mut bytes = [0u8; 6];
        if nic_mac_at(nic as u32, bytes.as_mut_ptr() as u32) == 0 {
            print_bytes(putc, b"NIC ");
            print_u32(putc, (nic + 1) as u32);
            print_bytes(putc, b" MAC read failed; stopping.\n");
            return;
        }
        let mac = MacAddress::new(bytes);
        print_bytes(putc, b"Testing NIC ");
        print_u32(putc, (nic + 1) as u32);
        print_bytes(putc, b" MAC ");
        for (index, byte) in bytes.iter().copied().enumerate() {
            if index != 0 {
                putc(u32::from(b':'));
            }
            print_hex_byte(putc, byte);
        }
        print_bytes(putc, b"; DHCP will retry until bound.\n");
        let Some(status) = run_interface(
            putc,
            should_abort,
            nic,
            mac,
            nic_send_at,
            nic_receive_at,
            max_frame_len,
            ticks,
        ) else {
            print_bytes(putc, b"Network DHCP test aborted.\n");
            return;
        };
        let mut status = status;
        if let Some((target, _interval)) = ping_options {
            let mut ping_frame = [0u8; MAX_FRAME_LEN];
            let mut ping_arp = [0u8; ArpPacket::LEN];
            let mut ping_icmp = [0u8; MAX_FRAME_LEN];
            let mut ping_ip = [0u8; MAX_FRAME_LEN];
            status.ping_sent = 1;
            if ping_once(
                putc,
                nic,
                status.mac,
                &status,
                target,
                nic as u16 + 1,
                nic_send_at,
                nic_receive_at,
                max_frame_len,
                ticks,
                &mut ping_frame,
                &mut ping_arp,
                &mut ping_icmp,
                &mut ping_ip,
            ) {
                status.ping_received = 1;
            }
        }
        statuses[nic] = status;
    }
    print_bytes(
        putc,
        b"Network DHCP test: every NIC acquired an IP; monitoring until manual exit.\n",
    );
    let mut ping_frame = [0u8; MAX_FRAME_LEN];
    let mut ping_arp = [0u8; ArpPacket::LEN];
    let mut ping_icmp = [0u8; MAX_FRAME_LEN];
    let mut ping_ip = [0u8; MAX_FRAME_LEN];
    let mut next_status = 0u32;
    let mut next_ping = if let Some((_target, interval)) = ping_options {
        (ticks() / 100) as u32 + interval
    } else {
        0
    };
    loop {
        if should_abort() == TAPE_EVENT_ABORT {
            print_bytes(putc, b"Network DHCP test stopped.\n");
            return;
        }
        let now = (ticks() / 100) as u32;
        if let Some((target, interval)) = ping_options {
            if now >= next_ping {
                for nic in 0..status_count {
                    let status = statuses[nic];
                    statuses[nic].ping_sent += 1;
                    if ping_once(
                        putc,
                        nic,
                        status.mac,
                        &status,
                        target,
                        (nic as u16).wrapping_add(status.ping_sent as u16),
                        nic_send_at,
                        nic_receive_at,
                        max_frame_len,
                        ticks,
                        &mut ping_frame,
                        &mut ping_arp,
                        &mut ping_icmp,
                        &mut ping_ip,
                    ) {
                        statuses[nic].ping_received += 1;
                    }
                }
                next_ping = now.saturating_add(interval);
            }
        }
        if now >= next_status {
            for nic in 0..status_count {
                print_acquired_status(putc, nic, statuses[nic]);
            }
            next_status = now.saturating_add(PROGRESS_INTERVAL_SECONDS);
        }
        core::hint::spin_loop();
    }
}
