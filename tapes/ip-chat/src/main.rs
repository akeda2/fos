#![no_std]
#![no_main]

//! A line-oriented IPv4/UDP chat tape.
//!
//! This is deliberately an application rather than a kernel service.  The
//! kernel supplies only raw Ethernet callbacks; `tape-net` supplies packet
//! codecs and the DHCP state machine; this file drives ARP, routing, DHCP,
//! line editing, and the receive loop in one readable place.
//!
//! Startup forms:
//!
//! ```text
//! tape run static LOCAL_IP/PREFIX GATEWAY [PEER_IP]
//! tape run static LOCAL_IP /PREFIX GATEWAY [PEER_IP]
//! tape run static LOCAL_IP NETMASK GATEWAY [PEER_IP]
//! tape run dhcp [PEER_IP]
//! ```
//!
//! `static` accepts a combined `IP/PREFIX` CIDR word, a separate
//! `/PREFIX` word, or the legacy dotted `NETMASK` -- matching
//! `telnet`/`audio-rtp`/`router`'s identical convention (see
//! `parse_cidr`/`parse_prefix_mask` in `lib.rs`).
//!
//! A static configuration is used exactly as supplied.  DHCP sends the
//! standard DISCOVER/OFFER/REQUEST/ACK exchange and reports a bounded,
//! explicit failure if no server answers.  After configuration, ordinary
//! lines are UDP datagrams to the selected peer, while `/to IP`,
//! `/broadcast`, `/peers`, and `/quit` control the small fixed-buffer chat
//! client.  F12/F4/F6/F7/F8/F10 and the same events delivered by `term net`
//! abort the tape.
//!
//! The tape uses no heap and does not own the NIC.  `nic_receive` returns
//! complete Ethernet frames from the kernel's polling RTL8139 driver;
//! `nic_send` accepts complete frames.  Ethernet padding and FCS handling
//! remain the driver's responsibility.  IPv4 options, fragmentation, ARP
//! proxying, DNS, IPv6, and retransmission/acknowledgement above UDP are
//! intentionally outside this teaching example.

use core::cmp::min;

use ip_chat_tape::{
    BROADCAST_IP, CHAT_PORT, MAX_FRAME_LEN, NetworkConfig, StartupError, StartupMode,
    build_udp_frame, is_subnet_broadcast, parse_ipv4, parse_startup, same_subnet,
};
use tape_net::frames;
use tape_net::{
    ArpCache, ArpOperation, ArpPacket, DhcpClient,
    DhcpClientState, ETHERTYPE_ARP, ETHERTYPE_IPV4, EthernetFrame, Ipv4Address,
    Ipv4Packet, MacAddress, UdpDatagram,
};
use tape_rt::{print_bytes, print_u32};

const MAX_LINE: usize = 240;
const MAX_PEERS: usize = 8;
const ARP_TIMEOUT_TICKS: u64 = 300;
const DHCP_TIMEOUT_SECONDS: u32 = 8;
const DHCP_RETRY_SECONDS: u32 = 1;
const DHCP_MAX_RETRIES: u8 = 5;
const CHAT_SOURCE_PORT: u16 = 40000;

const TAPE_EVENT_NONE: u32 = 0;
const TAPE_EVENT_ABORT: u32 = 1;
const TAPE_EVENT_CHAR_BASE: u32 = 0x100;

type Putc = extern "C" fn(u32);
type ReadKey = fn() -> u32;
type NicSend = fn(usize, u32) -> u32;
type NicReceive = fn(usize, u32) -> u32;
type Ticks = fn() -> u64;

#[derive(Clone, Copy)]
enum ResolveResult {
    Found(MacAddress),
    Failed,
    Aborted,
}

#[derive(Clone, Copy)]
struct PeerTable {
    entries: [Ipv4Address; MAX_PEERS],
    count: usize,
}

impl PeerTable {
    const fn new() -> Self {
        Self {
            entries: [Ipv4Address::UNSPECIFIED; MAX_PEERS],
            count: 0,
        }
    }

    fn remember(&mut self, address: Ipv4Address) {
        if address == Ipv4Address::UNSPECIFIED
            || self.entries[..self.count].contains(&address)
            || self.count == MAX_PEERS
        {
            return;
        }
        self.entries[self.count] = address;
        self.count += 1;
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

fn print_config(putc: Putc, config: NetworkConfig) {
    print_bytes(putc, b"IPv4 configured: ");
    print_ip(putc, config.local);
    print_bytes(putc, b"/");
    print_ip(putc, config.netmask);
    print_bytes(putc, b", gateway ");
    print_ip(putc, config.gateway);
    print_bytes(putc, b".\n");
}

fn print_startup_error(putc: Putc, error: StartupError) {
    let detail = match error {
        StartupError::MissingMode => &b"missing startup mode"[..],
        StartupError::UnknownMode => &b"mode must be static or dhcp"[..],
        StartupError::WrongArgumentCount => &b"wrong number of arguments"[..],
        StartupError::BadAddress => &b"invalid dotted IPv4 address"[..],
        StartupError::BadNetmask => &b"invalid netmask or /PREFIX"[..],
    };
    print_bytes(putc, b"IPv4 chat startup error: ");
    print_bytes(putc, detail);
    print_bytes(
        putc,
        b". Usage: tape run static LOCAL_IP[/PREFIX] [/PREFIX|NETMASK] \
GATEWAY [PEER_IP] | tape run dhcp [PEER_IP]\n",
    );
}

fn raw_ticks(ticks: Ticks) -> u64 {
    ticks()
}

fn dhcp_seconds(ticks: Ticks) -> u32 {
    (raw_ticks(ticks) / 100) as u32
}

fn tick_reached(now: u64, deadline: u64) -> bool {
    now.wrapping_sub(deadline) < (1u64 << 63)
}

fn send_frame(nic_send: NicSend, frame: &mut [u8], length: usize, max_frame_len: usize) -> bool {
    if length < EthernetFrame::HEADER_LEN || length > max_frame_len {
        return false;
    }
    nic_send(frame.as_ptr() as usize, length as u32) != 0
}

fn send_arp_request(nic_send: NicSend, own_mac: MacAddress, local_ip: Ipv4Address, target_ip: Ipv4Address, max_frame_len: usize) -> bool {
    let mut frame = [0u8; MAX_FRAME_LEN];
    let Some(length) = frames::arp_request_frame(&mut frame, own_mac, local_ip, target_ip) else {
        return false;
    };
    send_frame(nic_send, &mut frame, length, max_frame_len)
}

fn send_arp_reply(
    nic_send: NicSend,
    own_mac: MacAddress,
    local_ip: Ipv4Address,
    target_mac: MacAddress,
    target_ip: Ipv4Address,
    max_frame_len: usize,
) -> bool {
    let mut frame = [0u8; MAX_FRAME_LEN];
    let Some(length) = frames::arp_reply_frame(&mut frame, own_mac, local_ip, target_mac, target_ip) else {
        return false;
    };
    send_frame(nic_send, &mut frame, length, max_frame_len)
}

fn print_incoming_chat(putc: Putc, source: Ipv4Address, payload: &[u8]) {
    print_bytes(putc, b"\n");
    print_ip(putc, source);
    print_bytes(putc, b": ");
    for &byte in payload {
        if (0x20..=0x7e).contains(&byte) || byte == b'\t' {
            putc(u32::from(byte));
        } else {
            putc(u32::from(b'.'));
        }
    }
    putc(u32::from(b'\n'));
}

/// Consume ARP and addressed UDP frames.  ARP requests for our configured
/// address receive a direct reply, which is necessary for another host to
/// learn our MAC before it can deliver a unicast chat datagram.
fn process_frame(
    putc: Putc,
    nic_send: NicSend,
    frame: &[u8],
    own_mac: MacAddress,
    config: NetworkConfig,
    arp: &mut ArpCache<8>,
    peers: &mut PeerTable,
    max_frame_len: usize,
) {
    let Ok(ethernet) = EthernetFrame::parse(frame) else {
        return;
    };
    if ethernet.ethertype == ETHERTYPE_ARP {
        let Ok(packet) = ArpPacket::parse(ethernet.payload) else {
            return;
        };
        arp.insert(packet.sender_protocol, packet.sender_hardware, 0);
        if packet.operation == ArpOperation::Request
            && packet.target_protocol == config.local
            && packet.sender_hardware != own_mac
        {
            let _ = send_arp_reply(
                nic_send,
                own_mac,
                config.local,
                packet.sender_hardware,
                packet.sender_protocol,
                max_frame_len,
            );
        }
        return;
    }
    if ethernet.ethertype != ETHERTYPE_IPV4 {
        return;
    }
    let Ok(packet) = Ipv4Packet::parse(ethernet.payload, true) else {
        return;
    };
    if packet.header.protocol != 17
        || (packet.header.destination != config.local
            && !is_subnet_broadcast(config.local, config.netmask, packet.header.destination))
    {
        return;
    }
    let Ok(datagram) = UdpDatagram::parse(
        packet.payload,
        true,
        packet.header.source,
        packet.header.destination,
    ) else {
        return;
    };
    if datagram.destination_port == CHAT_PORT && packet.header.source != config.local {
        peers.remember(packet.header.source);
        print_incoming_chat(putc, packet.header.source, datagram.payload);
    }
}

/// Resolve the next-hop MAC while preserving one ordinary input event.  The
/// pending event matters for `term net`: remote typing can arrive while ARP
/// is waiting, and dropping it would make the line editor appear unreliable.
fn resolve_mac(
    putc: Putc,
    read_key: ReadKey,
    nic_receive: NicReceive,
    nic_send: NicSend,
    ticks: Ticks,
    target_ip: Ipv4Address,
    own_mac: MacAddress,
    config: NetworkConfig,
    arp: &mut ArpCache<8>,
    peers: &mut PeerTable,
    pending_event: &mut Option<u32>,
    receive_buffer: &mut [u8; MAX_FRAME_LEN],
    max_frame_len: usize,
) -> ResolveResult {
    let initial_tick = raw_ticks(ticks);
    if let Some(mac) = arp.lookup(target_ip, initial_tick as u32) {
        return ResolveResult::Found(mac);
    }
    if !send_arp_request(nic_send, own_mac, config.local, target_ip, max_frame_len) {
        return ResolveResult::Failed;
    }

    let deadline = initial_tick.wrapping_add(ARP_TIMEOUT_TICKS);
    loop {
        if pending_event.is_none() {
            let event = read_key();
            if event == TAPE_EVENT_ABORT {
                return ResolveResult::Aborted;
            }
            if event >= TAPE_EVENT_CHAR_BASE {
                *pending_event = Some(event);
            }
        }

        let length = nic_receive(receive_buffer.as_mut_ptr() as usize, max_frame_len as u32) as usize;
        if length != 0 {
            process_frame(
                putc,
                nic_send,
                &receive_buffer[..min(length, MAX_FRAME_LEN)],
                own_mac,
                config,
                arp,
                peers,
                max_frame_len,
            );
            if let Some(mac) = arp.lookup(target_ip, raw_ticks(ticks) as u32) {
                return ResolveResult::Found(mac);
            }
        } else {
            tape_rt::idle();
        }
        if tick_reached(raw_ticks(ticks), deadline) {
            return ResolveResult::Failed;
        }
    }
}

fn send_chat(
    putc: Putc,
    read_key: ReadKey,
    nic_send: NicSend,
    nic_receive: NicReceive,
    ticks: Ticks,
    config: NetworkConfig,
    target: Option<Ipv4Address>,
    line: &[u8],
    own_mac: MacAddress,
    arp: &mut ArpCache<8>,
    peers: &mut PeerTable,
    pending_event: &mut Option<u32>,
    receive_buffer: &mut [u8; MAX_FRAME_LEN],
    max_frame_len: usize,
) -> ResolveResult {
    let destination_ip = target.unwrap_or(BROADCAST_IP);
    let destination_mac = if is_subnet_broadcast(config.local, config.netmask, destination_ip) {
        MacAddress::BROADCAST
    } else {
        let next_hop = if same_subnet(config.local, config.netmask, destination_ip) {
            destination_ip
        } else if config.gateway != Ipv4Address::UNSPECIFIED {
            config.gateway
        } else {
            print_bytes(
                putc,
                b"Cannot send: peer is off-subnet and no gateway is configured.\n",
            );
            return ResolveResult::Failed;
        };
        match resolve_mac(
            putc,
            read_key,
            nic_receive,
            nic_send,
            ticks,
            next_hop,
            own_mac,
            config,
            arp,
            peers,
            pending_event,
            receive_buffer,
            max_frame_len,
        ) {
            ResolveResult::Found(mac) => mac,
            other => return other,
        }
    };

    let mut frame = [0u8; MAX_FRAME_LEN];
    let Ok(length) = build_udp_frame(
        &mut frame,
        own_mac,
        destination_mac,
        config.local,
        destination_ip,
        CHAT_SOURCE_PORT,
        CHAT_PORT,
        line,
        true,
    ) else {
        print_bytes(
            putc,
            b"Cannot send: chat line is too large for one IPv4 frame.\n",
        );
        return ResolveResult::Failed;
    };
    if !send_frame(nic_send, &mut frame, length, max_frame_len) {
        print_bytes(putc, b"Cannot send: the active NIC rejected the Ethernet frame.\n");
    }
    ResolveResult::Found(destination_mac)
}

fn print_peers(putc: Putc, peers: PeerTable, target: Option<Ipv4Address>) {
    print_bytes(putc, b"Known/configured IPv4 peers:\n");
    if peers.count == 0 {
        print_bytes(putc, b"  (none heard yet)\n");
    } else {
        for (index, address) in peers.entries[..peers.count].iter().enumerate() {
            putc(u32::from(b' '));
            putc(u32::from(b'1' + index as u8));
            print_bytes(putc, b": ");
            print_ip(putc, *address);
            putc(u32::from(b'\n'));
        }
    }
    print_bytes(putc, b"Outgoing target: ");
    match target {
        Some(address) => print_ip(putc, address),
        None => print_bytes(putc, b"255.255.255.255 (broadcast)"),
    }
    putc(u32::from(b'\n'));
}

fn handle_command(
    putc: Putc,
    read_key: ReadKey,
    nic_send: NicSend,
    nic_receive: NicReceive,
    ticks: Ticks,
    config: NetworkConfig,
    line: &[u8],
    target: &mut Option<Ipv4Address>,
    own_mac: MacAddress,
    arp: &mut ArpCache<8>,
    peers: &mut PeerTable,
    pending_event: &mut Option<u32>,
    receive_buffer: &mut [u8; MAX_FRAME_LEN],
    max_frame_len: usize,
) -> bool {
    if line == b"/quit" {
        return true;
    }
    if line == b"/broadcast" {
        *target = None;
        print_bytes(putc, b"Outgoing target is now IPv4 broadcast.\n");
        return false;
    }
    if line == b"/peers" {
        print_peers(putc, *peers, *target);
        return false;
    }
    if let Some(address) = line.strip_prefix(b"/to ") {
        match parse_ipv4(address) {
            Some(parsed) => {
                *target = Some(parsed);
                peers.remember(parsed);
                print_bytes(putc, b"Outgoing target is now ");
                print_ip(putc, parsed);
                print_bytes(putc, b".\n");
            }
            None => print_bytes(putc, b"Usage: /to A.B.C.D\n"),
        }
        return false;
    }
    if line.len() > MAX_LINE {
        print_bytes(putc, b"Chat lines are limited to 240 bytes.\n");
        return false;
    }
    match send_chat(
        putc,
        read_key,
        nic_send,
        nic_receive,
        ticks,
        config,
        *target,
        line,
        own_mac,
        arp,
        peers,
        pending_event,
        receive_buffer,
        max_frame_len,
    ) {
        ResolveResult::Aborted => return true,
        ResolveResult::Failed | ResolveResult::Found(_) => {}
    }
    false
}

fn receive_dhcp(frame: &[u8], client: &mut DhcpClient<128>, now: u32) {
    frames::feed_dhcp(frame, client, now);
}

fn send_dhcp_payload(
    nic_send: NicSend,
    own_mac: MacAddress,
    payload: &[u8],
    source: Ipv4Address,
    destination: Ipv4Address,
    max_frame_len: usize,
) -> bool {
    let mut frame = [0u8; MAX_FRAME_LEN];
    let Some(length) = frames::dhcp_client_frame(&mut frame, own_mac, source, destination, payload) else {
        return false;
    };
    send_frame(nic_send, &mut frame, length, max_frame_len)
}

fn configure_dhcp(
    putc: Putc,
    nic_send: NicSend,
    nic_receive: NicReceive,
    ticks: Ticks,
    own_mac: MacAddress,
    receive_buffer: &mut [u8; MAX_FRAME_LEN],
    max_frame_len: usize,
) -> Option<NetworkConfig> {
    print_bytes(
        putc,
        b"DHCP: broadcasting DISCOVER and waiting for a server...\n",
    );
    let transaction_id = (raw_ticks(ticks) as u32)
        ^ u32::from_be_bytes(own_mac.0[..4].try_into().unwrap())
        ^ u32::from_be_bytes([own_mac.0[2], own_mac.0[3], own_mac.0[4], own_mac.0[5]]);
    let mut client = DhcpClient::<128>::new(
        own_mac,
        transaction_id,
        DHCP_RETRY_SECONDS,
        DHCP_MAX_RETRIES,
    );
    let started = dhcp_seconds(ticks);
    let mut payload = [0u8; 600];

    loop {
        let now = dhcp_seconds(ticks);
        if now.wrapping_sub(started) > DHCP_TIMEOUT_SECONDS {
            print_bytes(
                putc,
                b"DHCP failed: no server responded before the timeout.\n",
            );
            return None;
        }
        match client.poll(now, &mut payload) {
            Ok(Some(length)) => {
                let transmit = client.transmit();
                if !send_dhcp_payload(
                    nic_send,
                    own_mac,
                    &payload[..length],
                    transmit.source,
                    transmit.destination,
                    max_frame_len,
                ) {
                    print_bytes(putc, b"DHCP failed: NIC rejected the request frame.\n");
                    return None;
                }
            }
            Ok(None) => {}
            Err(_) => {
                print_bytes(putc, b"DHCP failed: packet construction error.\n");
                return None;
            }
        }
        if client.state() == DhcpClientState::Bound {
            let lease = client.lease().unwrap();
            if lease.subnet_mask == Ipv4Address::UNSPECIFIED {
                print_bytes(putc, b"DHCP failed: server supplied no subnet mask.\n");
                return None;
            }
            let config = NetworkConfig {
                local: lease.address,
                netmask: lease.subnet_mask,
                gateway: lease.router,
                peer: None,
            };
            print_bytes(putc, b"DHCP lease acquired for ");
            print_ip(putc, lease.address);
            print_bytes(putc, b".\n");
            return Some(config);
        }
        if client.state() == DhcpClientState::Failed {
            print_bytes(
                putc,
                b"DHCP failed: no usable offer/acknowledgement arrived.\n",
            );
            return None;
        }

        let length = nic_receive(receive_buffer.as_mut_ptr() as usize, max_frame_len as u32) as usize;
        if length != 0 {
            receive_dhcp(
                &receive_buffer[..min(length, MAX_FRAME_LEN)],
                &mut client,
                now,
            );
        } else {
            tape_rt::idle();
        }
    }
}

/// Declares the complete current twelve-argument tape ABI.  The two callback
/// groups are intentionally independent: `read_key` consumes local or
/// term-net input, while the three raw NIC callbacks expose only Ethernet
/// frames.  `ticks` is the kernel's 100 Hz PIT count; DHCP converts it to
/// seconds because DHCP lease fields are specified in seconds.
#[unsafe(no_mangle)]
pub extern "C" fn tape_main(api: &'static tape_rt::TapeApi) {
    tape_rt::init(api);
    use tape_rt::compat::*;
    let putc = api.putc;
    let args = tape_rt::args();
    let (args_ptr, args_len) = (args.as_ptr() as usize, args.len() as u32);

    let mut mac_bytes = [0u8; 6];
    if nic_mac(mac_bytes.as_mut_ptr() as usize) == 0 {
        print_bytes(putc, b"IPv4 chat cannot start: no supported NIC is available.\n");
        return;
    }
    let own_mac = MacAddress::new(mac_bytes);
    let max_frame_len = min(nic_max_frame_len() as usize, MAX_FRAME_LEN);
    if max_frame_len < 60 {
        print_bytes(
            putc,
            b"IPv4 chat cannot start: NIC frame limit is too small.\n",
        );
        return;
    }

    let arguments = if args_len == 0 {
        &[][..]
    } else {
        unsafe { core::slice::from_raw_parts(args_ptr as *const u8, args_len as usize) }
    };
    let startup = match parse_startup(arguments) {
        Ok(mode) => mode,
        Err(error) => {
            print_startup_error(putc, error);
            return;
        }
    };

    let mut receive_buffer = [0u8; MAX_FRAME_LEN];
    let config = match startup {
        StartupMode::Static {
            local,
            netmask,
            gateway,
            peer,
        } => NetworkConfig {
            local,
            netmask,
            gateway,
            peer,
        },
        StartupMode::Dhcp { peer } => match configure_dhcp(
            putc,
            nic_send,
            nic_receive,
            ticks,
            own_mac,
            &mut receive_buffer,
            max_frame_len,
        ) {
            Some(mut config) => {
                config.peer = peer;
                config
            }
            None => return,
        },
    };

    print_config(putc, config);
    let mut target = config.peer;
    let mut peers = PeerTable::new();
    if let Some(peer) = target {
        peers.remember(peer);
    }
    print_bytes(
        putc,
        b"IPv4 chat ready. Type a line; /to A.B.C.D, /broadcast, /peers, /quit.\n",
    );

    let mut arp = ArpCache::<8>::new();
    let mut line = [0u8; MAX_LINE];
    let mut line_len = 0usize;
    let mut saw_carriage_return = false;
    let mut pending_event = None;

    loop {
        let event = pending_event.take().unwrap_or_else(|| read_key());
        // No key this pass: if no frame arrives either, sleep (below).
        let key_idle = event == TAPE_EVENT_NONE;
        match event {
            TAPE_EVENT_NONE => {}
            TAPE_EVENT_ABORT => break,
            value if value >= TAPE_EVENT_CHAR_BASE => {
                let byte = (value & 0xff) as u8;
                match byte {
                    b'\n' if saw_carriage_return => saw_carriage_return = false,
                    b'\r' | b'\n' => {
                        putc(u32::from(b'\n'));
                        if line_len != 0
                            && handle_command(
                                putc,
                                read_key,
                                nic_send,
                                nic_receive,
                                ticks,
                                config,
                                &line[..line_len],
                                &mut target,
                                own_mac,
                                &mut arp,
                                &mut peers,
                                &mut pending_event,
                                &mut receive_buffer,
                                max_frame_len,
                            )
                        {
                            break;
                        }
                        line_len = 0;
                        saw_carriage_return = byte == b'\r';
                    }
                    8 | 127 if line_len != 0 => {
                        line_len -= 1;
                        saw_carriage_return = false;
                    }
                    printable if (0x20..=0x7e).contains(&printable) => {
                        if line_len < MAX_LINE {
                            line[line_len] = printable;
                            line_len += 1;
                            putc(u32::from(printable));
                        }
                        saw_carriage_return = false;
                    }
                    _ => {}
                }
            }
            _ => {}
        }

        let length = nic_receive(receive_buffer.as_mut_ptr() as usize, max_frame_len as u32) as usize;
        if length != 0 {
            process_frame(
                putc,
                nic_send,
                &receive_buffer[..min(length, MAX_FRAME_LEN)],
                own_mac,
                config,
                &mut arp,
                &mut peers,
                max_frame_len,
            );
        } else if key_idle {
            tape_rt::idle();
        }
    }

    print_bytes(putc, b"Left IPv4 chat; back at the fos shell.\n");
}

tape_rt::tape_help!(br#"ip-chat: line-oriented IPv4/UDP chat.

  tape run static LOCAL_IP/PREFIX GATEWAY [PEER_IP]
  tape run static LOCAL_IP NETMASK GATEWAY [PEER_IP]
  tape run dhcp [PEER_IP]

  Type a line and press Enter to send it. Escape stops.
"#);
