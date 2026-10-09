#![no_std]
#![no_main]

//! A fixed-buffer 32-bit Telnet client tape.
//!
//! Startup syntax:
//!
//! ```text
//! tape run static LOCAL_IP/PREFIX GATEWAY SERVER_IP SERVER_PORT
//! tape run static LOCAL_IP /PREFIX GATEWAY SERVER_IP SERVER_PORT
//! tape run static LOCAL_IP NETMASK GATEWAY SERVER_IP SERVER_PORT
//! tape run dhcp SERVER_IP SERVER_PORT
//! ```
//!
//! Example for a Telnet server at 192.0.2.20:23:
//!
//! ```text
//! tape run static 192.0.2.10/24 0.0.0.0 192.0.2.20 23
//! tape run dhcp 192.0.2.20 23
//! ```
//!
//! The tape uses the append-only tape ABI. The kernel still owns PCI, DMA,
//! VGA state, and the active NIC; this tape owns only packet assembly, ARP
//! policy, DHCP startup, TCP connection state, Telnet parsing, CPR responses,
//! and the terminal loop.
//! `tape-net` supplies the fixed-buffer Ethernet/IPv4/TCP codecs and the
//! bounded single-connection TCP state machine.
//!
//! Terminal input is deliberately byte-oriented. Every byte returned by
//! `read_key` is forwarded (including bytes >= 0x80 and UTF-8 continuation
//! bytes) without decoding it as text; the kernel maps navigation keys to
//! ordinary ANSI escape sequences before this callback sees them. A literal
//! Telnet IAC (0xff) is doubled on the wire. Server data is passed to `putc`
//! byte by byte, so a UTF-8 sequence remains a sequence of original bytes.

use core::cmp::min;

use tape_net::{
    ArpCache, ArpOperation, ArpPacket, DHCP_BOOTSTRAP_DESTINATION, DHCP_CLIENT_PORT,
    DHCP_SERVER_PORT, DhcpClient, DhcpClientState, DhcpPacket, ETHERTYPE_ARP, ETHERTYPE_IPV4,
    EthernetFrame, Ipv4Address, Ipv4Header, Ipv4Packet, MacAddress, TcpConnection, TcpSegment,
    TcpState, UdpDatagram,
};
use tape_rt::print_bytes;
use telnet_tape::{
    MARKER_ABORTED, MARKER_CONNECTED, MARKER_CONNECTING, MARKER_DISCONNECTED, MAX_FRAME_LEN,
    StartupError, StartupMode, TCP_SEGMENT_CAPACITY, TELNET_BINARY, TELNET_DO, TELNET_IAC,
    TELNET_NAWS, TELNET_SB, TELNET_SE, TELNET_SUPPRESS_GO_AHEAD, TELNET_TERMINAL_NAME,
    TELNET_TERMINAL_TYPE, TELNET_TTYPE_IS, TELNET_TTYPE_SEND, TELNET_WILL, TELNET_WINDOW_HEIGHT,
    TELNET_WINDOW_WIDTH, TelnetConfig, TelnetEvent, TelnetParser, build_tcp_frame,
    encode_application, parse_startup, same_subnet,
};

const MAX_INPUT_QUEUE: usize = 2048;
const TCP_RX_CAPACITY: usize = 1024;
const TCP_RETRANSMIT_TICKS: u32 = 100;
const ARP_TIMEOUT_TICKS: u64 = 300;
const CONNECT_TIMEOUT_TICKS: u64 = 1500;
const DHCP_TIMEOUT_SECONDS: u32 = 8;
const DHCP_RETRY_SECONDS: u32 = 1;
const DHCP_MAX_RETRIES: u8 = 5;
const TELNET_LOCAL_PORT: u16 = 40000;

const TAPE_EVENT_NONE: u32 = 0;
const TAPE_EVENT_ABORT: u32 = 1;
const TAPE_EVENT_CHAR_BASE: u32 = 0x100;

type Putc = extern "C" fn(u32);
type ReadKey = fn() -> u32;
type NicMac = fn(usize) -> u32;
type NicSend = fn(usize, u32) -> u32;
type NicReceive = fn(usize, u32) -> u32;
type Ticks = fn() -> u64;
type TerminalPosition = fn() -> u32;

#[derive(Clone, Copy, Eq, PartialEq)]
enum ConnectResult {
    Connected,
    Aborted,
    Failed,
}

enum DhcpResult {
    Configured(TelnetConfig),
    Aborted,
    Failed,
}

#[derive(Clone, Copy)]
struct ByteQueue<const N: usize> {
    bytes: [u8; N],
    head: usize,
    len: usize,
}

impl<const N: usize> ByteQueue<N> {
    const fn new() -> Self {
        Self {
            bytes: [0; N],
            head: 0,
            len: 0,
        }
    }
}

#[derive(Clone, Copy)]
enum TerminalOutputState {
    Normal,
    Escape,
    Csi {
        private: bool,
        params: [u16; 4],
        count: usize,
        value: u16,
        has_value: bool,
    },
}

#[derive(Clone, Copy)]
enum TerminalQuery {
    CursorPosition,
    Status,
}

struct TerminalOutputParser {
    state: TerminalOutputState,
}

impl TerminalOutputParser {
    const fn new() -> Self {
        Self {
            state: TerminalOutputState::Normal,
        }
    }

    fn feed(&mut self, byte: u8) -> Option<TerminalQuery> {
        let state = core::mem::replace(&mut self.state, TerminalOutputState::Normal);
        match state {
            TerminalOutputState::Normal => {
                if byte == 0x1b {
                    self.state = TerminalOutputState::Escape;
                }
            }
            TerminalOutputState::Escape => {
                if byte == b'[' {
                    self.state = TerminalOutputState::Csi {
                        private: false,
                        params: [0; 4],
                        count: 0,
                        value: 0,
                        has_value: false,
                    };
                } else if byte == 0x1b {
                    self.state = TerminalOutputState::Escape;
                }
            }
            TerminalOutputState::Csi {
                private,
                mut params,
                mut count,
                mut value,
                mut has_value,
            } => {
                if byte == b'?' && count == 0 && !has_value {
                    self.state = TerminalOutputState::Csi {
                        private: true,
                        params,
                        count,
                        value,
                        has_value,
                    };
                } else if byte.is_ascii_digit() {
                    has_value = true;
                    value = value
                        .saturating_mul(10)
                        .saturating_add(u16::from(byte - b'0'));
                    self.state = TerminalOutputState::Csi {
                        private,
                        params,
                        count,
                        value,
                        has_value,
                    };
                } else if byte == b';' {
                    if count < params.len() {
                        params[count] = if has_value { value } else { 0 };
                        count += 1;
                    }
                    self.state = TerminalOutputState::Csi {
                        private,
                        params,
                        count,
                        value: 0,
                        has_value: false,
                    };
                } else if (0x40..=0x7e).contains(&byte) {
                    if has_value || count != 0 {
                        if count < params.len() {
                            params[count] = if has_value { value } else { 0 };
                            count += 1;
                        }
                    }
                    let query = if !private && byte == b'n' && count == 1 {
                        match params[0] {
                            5 => Some(TerminalQuery::Status),
                            6 => Some(TerminalQuery::CursorPosition),
                            _ => None,
                        }
                    } else {
                        None
                    };
                    self.state = TerminalOutputState::Normal;
                    return query;
                } else if byte == 0x1b {
                    self.state = TerminalOutputState::Escape;
                } else {
                    self.state = TerminalOutputState::Normal;
                }
            }
        }
        None
    }
}

fn push_decimal(queue: &mut ByteQueue<MAX_INPUT_QUEUE>, mut value: u32) -> bool {
    let mut digits = [0u8; 10];
    let mut length = 0;
    if value == 0 {
        return queue.push(b'0');
    }
    while value != 0 {
        digits[length] = b'0' + (value % 10) as u8;
        length += 1;
        value /= 10;
    }
    while length != 0 {
        length -= 1;
        if !queue.push(digits[length]) {
            return false;
        }
    }
    true
}

fn queue_terminal_query_response(
    queue: &mut ByteQueue<MAX_INPUT_QUEUE>,
    query: TerminalQuery,
    terminal_position: TerminalPosition,
) -> bool {
    if !queue.push_slice(b"\x1b[") {
        return false;
    }
    match query {
        TerminalQuery::Status => queue.push_slice(b"0n"),
        TerminalQuery::CursorPosition => {
            let packed = terminal_position();
            let row = packed >> 16;
            let column = packed & 0xffff;
            if !push_decimal(queue, row) || !queue.push(b';') || !push_decimal(queue, column) {
                return false;
            }
            queue.push(b'R')
        }
    }
}

impl<const N: usize> ByteQueue<N> {
    fn push(&mut self, byte: u8) -> bool {
        if self.len == N {
            return false;
        }
        self.bytes[(self.head + self.len) % N] = byte;
        self.len += 1;
        true
    }

    fn push_slice(&mut self, bytes: &[u8]) -> bool {
        if bytes.len() > N.saturating_sub(self.len) {
            return false;
        }
        for &byte in bytes {
            let _ = self.push(byte);
        }
        true
    }

    fn peek(&self, output: &mut [u8]) -> usize {
        let count = min(output.len(), self.len);
        for (index, slot) in output[..count].iter_mut().enumerate() {
            *slot = self.bytes[(self.head + index) % N];
        }
        count
    }

    fn discard(&mut self, count: usize) {
        let count = min(count, self.len);
        self.head = (self.head + count) % N;
        self.len -= count;
    }
}

fn print_u16(putc: Putc, mut value: u16) {
    let mut digits = [0u8; 5];
    let mut count = 0;
    if value == 0 {
        putc(u32::from(b'0'));
        return;
    }
    while value != 0 {
        digits[count] = b'0' + (value % 10) as u8;
        value /= 10;
        count += 1;
    }
    for &digit in digits[..count].iter().rev() {
        putc(u32::from(digit));
    }
}

fn print_ip(putc: Putc, address: Ipv4Address) {
    for (index, octet) in address.0.iter().copied().enumerate() {
        if index != 0 {
            putc(u32::from(b'.'));
        }
        print_u16(putc, u16::from(octet));
    }
}

fn print_startup_error(putc: Putc, error: StartupError) {
    let detail: &[u8] = match error {
        StartupError::MissingArguments => b"missing startup arguments",
        StartupError::UnknownMode => b"mode must be static or dhcp",
        StartupError::WrongArgumentCount => b"wrong number of startup words",
        StartupError::BadAddress => b"invalid dotted IPv4 address",
        StartupError::BadNetmask => b"invalid IPv4 prefix or netmask",
        StartupError::BadPort => b"invalid TCP server port",
    };
    print_bytes(putc, b"TELNET ERROR: ");
    print_bytes(putc, detail);
    print_bytes(
        putc,
        b". Usage: tape run static LOCAL_IP/PREFIX GATEWAY SERVER_IP SERVER_PORT | \
tape run static LOCAL_IP /PREFIX GATEWAY SERVER_IP SERVER_PORT | \
tape run static LOCAL_IP NETMASK GATEWAY SERVER_IP SERVER_PORT | \
tape run dhcp SERVER_IP SERVER_PORT\n",
    );
}

fn print_config(putc: Putc, config: TelnetConfig) {
    print_bytes(putc, b"TELNET CONFIG: local ");
    print_ip(putc, config.local_ip);
    print_bytes(putc, b", server ");
    print_ip(putc, config.server_ip);
    putc(u32::from(b':'));
    print_u16(putc, config.server_port);
    print_bytes(putc, b"\n");
}

fn tick_reached(now: u64, deadline: u64) -> bool {
    now.wrapping_sub(deadline) < (1u64 << 63)
}

/// Set whenever `queue_input` got a real event from `read_key`, so a session
/// loop only sleeps (`tape_rt::idle`) after a pass with no key *and* no frame.
/// `read_key` hands back one event per call, so a pasted remote line is drained
/// one byte per loop pass -- sleeping between those would add a tick per byte.
static mut KEY_SEEN: bool = false;

/// Reports (and clears) `KEY_SEEN`.
fn take_key_seen() -> bool {
    unsafe {
        let seen = KEY_SEEN;
        KEY_SEEN = false;
        seen
    }
}

fn queue_input(read_key: ReadKey, queue: &mut ByteQueue<MAX_INPUT_QUEUE>) -> Result<bool, ()> {
    let event = read_key();
    if event != TAPE_EVENT_NONE {
        unsafe { KEY_SEEN = true };
    }
    match event {
        TAPE_EVENT_NONE => Ok(false),
        TAPE_EVENT_ABORT => Ok(true),
        event if event >= TAPE_EVENT_CHAR_BASE => {
            let byte = (event - TAPE_EVENT_CHAR_BASE) as u8;
            let mut encoded = [0u8; 2];
            let length = encode_application(&[byte], &mut encoded).ok_or(())?;
            if !queue.push_slice(&encoded[..length]) {
                return Err(());
            }
            Ok(false)
        }
        _ => Ok(false),
    }
}

fn send_frame(nic_send: NicSend, frame: &mut [u8], length: usize, max_frame_len: usize) -> bool {
    // The kernel's active NIC wrapper pads short Ethernet frames to the
    // Ethernet minimum.  The tape therefore validates only the actual
    // headers it built and leaves padding/FCS ownership below the ABI.
    if length < EthernetFrame::HEADER_LEN || length > max_frame_len {
        return false;
    }
    nic_send(frame.as_ptr() as usize, length as u32) != 0
}

fn send_tcp_bytes(
    nic_send: NicSend,
    config: TelnetConfig,
    own_mac: MacAddress,
    destination_mac: MacAddress,
    segment: &[u8],
    max_frame_len: usize,
) -> bool {
    let mut frame = [0u8; MAX_FRAME_LEN];
    let Ok(parsed) = TcpSegment::parse(segment, config.local_ip, config.server_ip, true) else {
        return false;
    };
    let Ok(length) = build_tcp_frame(
        &mut frame,
        own_mac,
        destination_mac,
        config.local_ip,
        config.server_ip,
        &parsed,
    ) else {
        return false;
    };
    send_frame(nic_send, &mut frame, length, max_frame_len)
}

fn send_arp_request(
    nic_send: NicSend,
    own_mac: MacAddress,
    local_ip: Ipv4Address,
    target_ip: Ipv4Address,
    max_frame_len: usize,
) -> bool {
    let packet = ArpPacket::request(own_mac, local_ip, target_ip);
    let mut arp_bytes = [0u8; ArpPacket::LEN];
    let Ok(arp_len) = packet.encode(&mut arp_bytes) else {
        return false;
    };
    let ethernet = EthernetFrame::new(
        MacAddress::BROADCAST,
        own_mac,
        ETHERTYPE_ARP,
        &arp_bytes[..arp_len],
    );
    let mut frame = [0u8; MAX_FRAME_LEN];
    let Ok(length) = ethernet.encode(&mut frame) else {
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
    let packet = ArpPacket::reply(own_mac, local_ip, target_mac, target_ip);
    let mut arp_bytes = [0u8; ArpPacket::LEN];
    let Ok(arp_len) = packet.encode(&mut arp_bytes) else {
        return false;
    };
    let ethernet = EthernetFrame::new(target_mac, own_mac, ETHERTYPE_ARP, &arp_bytes[..arp_len]);
    let mut frame = [0u8; MAX_FRAME_LEN];
    let Ok(length) = ethernet.encode(&mut frame) else {
        return false;
    };
    send_frame(nic_send, &mut frame, length, max_frame_len)
}

fn handle_arp(
    ethernet: EthernetFrame<'_>,
    config: TelnetConfig,
    own_mac: MacAddress,
    arp: &mut ArpCache<8>,
    nic_send: NicSend,
    max_frame_len: usize,
    now: u32,
) {
    let Ok(packet) = ArpPacket::parse(ethernet.payload) else {
        return;
    };
    arp.insert(packet.sender_protocol, packet.sender_hardware, now);
    if packet.operation == ArpOperation::Request
        && packet.target_protocol == config.local_ip
        && packet.sender_hardware != own_mac
    {
        let _ = send_arp_reply(
            nic_send,
            own_mac,
            config.local_ip,
            packet.sender_hardware,
            packet.sender_protocol,
            max_frame_len,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn poll_arp(
    nic_receive: NicReceive,
    nic_send: NicSend,
    config: TelnetConfig,
    own_mac: MacAddress,
    arp: &mut ArpCache<8>,
    receive_buffer: &mut [u8; MAX_FRAME_LEN],
    max_frame_len: usize,
    now: u32,
) -> bool {
    let length = nic_receive(receive_buffer.as_mut_ptr() as usize, max_frame_len as u32) as usize;
    if length == 0 {
        return false;
    }
    let length = min(length, MAX_FRAME_LEN);
    let Ok(ethernet) = EthernetFrame::parse(&receive_buffer[..length]) else {
        return true;
    };
    if ethernet.ethertype == ETHERTYPE_ARP {
        handle_arp(ethernet, config, own_mac, arp, nic_send, max_frame_len, now);
    }
    true
}

fn receive_dhcp(frame: &[u8], client: &mut DhcpClient<128>, now: u32) {
    let Ok(ethernet) = EthernetFrame::parse(frame) else {
        return;
    };
    if ethernet.ethertype != ETHERTYPE_IPV4 {
        return;
    }
    let Ok(packet) = Ipv4Packet::parse(ethernet.payload, true) else {
        return;
    };
    if packet.header.protocol != 17 {
        return;
    }
    let Ok(datagram) = UdpDatagram::parse(
        packet.payload,
        false,
        packet.header.source,
        packet.header.destination,
    ) else {
        return;
    };
    if datagram.source_port != DHCP_SERVER_PORT || datagram.destination_port != DHCP_CLIENT_PORT {
        return;
    }
    let Ok(dhcp) = DhcpPacket::parse(datagram.payload) else {
        return;
    };
    let _ = client.receive(now, &dhcp);
}

fn send_dhcp_payload(
    nic_send: NicSend,
    own_mac: MacAddress,
    payload: &[u8],
    max_frame_len: usize,
) -> bool {
    let mut frame = [0u8; MAX_FRAME_LEN];
    let mut udp_bytes = [0u8; MAX_FRAME_LEN];
    let udp = UdpDatagram::new(DHCP_CLIENT_PORT, DHCP_SERVER_PORT, payload);
    let Ok(udp_len) = udp.encode_with_checksum(
        &mut udp_bytes,
        Ipv4Address::UNSPECIFIED,
        DHCP_BOOTSTRAP_DESTINATION,
        false,
    ) else {
        return false;
    };
    let packet = Ipv4Packet {
        header: Ipv4Header::new(
            Ipv4Address::UNSPECIFIED,
            DHCP_BOOTSTRAP_DESTINATION,
            17,
            udp_len,
        ),
        payload: &udp_bytes[..udp_len],
    };
    let mut ip_bytes = [0u8; MAX_FRAME_LEN];
    let Ok(ip_len) = packet.encode(&mut ip_bytes) else {
        return false;
    };
    let ethernet = EthernetFrame::new(
        MacAddress::BROADCAST,
        own_mac,
        ETHERTYPE_IPV4,
        &ip_bytes[..ip_len],
    );
    let Ok(length) = ethernet.encode(&mut frame) else {
        return false;
    };
    send_frame(nic_send, &mut frame, length, max_frame_len)
}

#[allow(clippy::too_many_arguments)]
fn configure_dhcp(
    putc: Putc,
    read_key: ReadKey,
    nic_send: NicSend,
    nic_receive: NicReceive,
    ticks: Ticks,
    own_mac: MacAddress,
    server_ip: Ipv4Address,
    server_port: u16,
    receive_buffer: &mut [u8; MAX_FRAME_LEN],
    max_frame_len: usize,
) -> DhcpResult {
    print_bytes(
        putc,
        b"DHCP: broadcasting DISCOVER and waiting for a server...\n",
    );
    let transaction_id = (ticks() as u32)
        ^ u32::from_be_bytes(own_mac.0[..4].try_into().unwrap())
        ^ u32::from_be_bytes([own_mac.0[2], own_mac.0[3], own_mac.0[4], own_mac.0[5]]);
    let mut client = DhcpClient::<128>::new(
        own_mac,
        transaction_id,
        DHCP_RETRY_SECONDS,
        DHCP_MAX_RETRIES,
    );
    let started = (ticks() / 100) as u32;
    let mut payload = [0u8; 600];

    loop {
        if read_key() == TAPE_EVENT_ABORT {
            return DhcpResult::Aborted;
        }
        let now = (ticks() / 100) as u32;
        if now.wrapping_sub(started) > DHCP_TIMEOUT_SECONDS {
            print_bytes(
                putc,
                b"DHCP failed: no server responded before the timeout.\n",
            );
            return DhcpResult::Failed;
        }
        match client.poll(now, &mut payload) {
            Ok(Some(length)) => {
                if !send_dhcp_payload(nic_send, own_mac, &payload[..length], max_frame_len) {
                    print_bytes(putc, b"DHCP failed: NIC rejected the request frame.\n");
                    return DhcpResult::Failed;
                }
            }
            Ok(None) => {}
            Err(_) => {
                print_bytes(putc, b"DHCP failed: packet construction error.\n");
                return DhcpResult::Failed;
            }
        }
        if client.state() == DhcpClientState::Bound {
            let Some(lease) = client.lease() else {
                print_bytes(putc, b"DHCP failed: client reported a missing lease.\n");
                return DhcpResult::Failed;
            };
            if lease.subnet_mask == Ipv4Address::UNSPECIFIED {
                print_bytes(putc, b"DHCP failed: server supplied no subnet mask.\n");
                return DhcpResult::Failed;
            }
            print_bytes(putc, b"DHCP lease acquired for ");
            print_ip(putc, lease.address);
            print_bytes(putc, b".\n");
            return DhcpResult::Configured(TelnetConfig {
                local_ip: lease.address,
                netmask: lease.subnet_mask,
                gateway: lease.router,
                server_ip,
                server_port,
            });
        }
        if client.state() == DhcpClientState::Failed {
            print_bytes(
                putc,
                b"DHCP failed: no usable offer/acknowledgement arrived.\n",
            );
            return DhcpResult::Failed;
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

#[allow(clippy::too_many_arguments)]
fn resolve_server_mac(
    putc: Putc,
    read_key: ReadKey,
    nic_receive: NicReceive,
    nic_send: NicSend,
    ticks: Ticks,
    config: TelnetConfig,
    own_mac: MacAddress,
    arp: &mut ArpCache<8>,
    queue: &mut ByteQueue<MAX_INPUT_QUEUE>,
    receive_buffer: &mut [u8; MAX_FRAME_LEN],
    max_frame_len: usize,
) -> ConnectResult {
    let now = ticks();
    let target = if same_subnet(config.local_ip, config.netmask, config.server_ip) {
        config.server_ip
    } else if config.gateway != Ipv4Address::UNSPECIFIED {
        config.gateway
    } else {
        print_bytes(
            putc,
            b"TELNET ERROR: server is off-subnet and gateway is 0.0.0.0\n",
        );
        return ConnectResult::Failed;
    };
    if arp.lookup(target, now as u32).is_some() {
        return ConnectResult::Connected;
    }
    if !send_arp_request(nic_send, own_mac, config.local_ip, target, max_frame_len) {
        return ConnectResult::Failed;
    }

    let deadline = now.wrapping_add(ARP_TIMEOUT_TICKS);
    while !tick_reached(ticks(), deadline) {
        match queue_input(read_key, queue) {
            Ok(true) => return ConnectResult::Aborted,
            Ok(false) => {}
            Err(()) => {
                print_bytes(putc, b"TELNET ERROR: terminal input queue is full\n");
                return ConnectResult::Failed;
            }
        }
        let received = poll_arp(
            nic_receive,
            nic_send,
            config,
            own_mac,
            arp,
            receive_buffer,
            max_frame_len,
            ticks() as u32,
        );
        if arp.lookup(target, ticks() as u32).is_some() {
            return ConnectResult::Connected;
        }
        if !received && !take_key_seen() {
            tape_rt::idle();
        }
    }
    print_bytes(putc, b"TELNET ERROR: ARP resolution timed out\n");
    ConnectResult::Failed
}

fn queue_negotiation(queue: &mut ByteQueue<MAX_INPUT_QUEUE>, event: TelnetEvent) -> bool {
    let TelnetEvent::Reply { verb, option } = event else {
        return true;
    };
    if !queue.push_slice(&[TELNET_IAC, verb, option]) {
        return false;
    }
    if verb == TELNET_WILL && option == TELNET_NAWS {
        queue_naws(queue)
    } else {
        true
    }
}

fn queue_subnegotiation(queue: &mut ByteQueue<MAX_INPUT_QUEUE>, option: u8, data: &[u8]) -> bool {
    if !queue.push_slice(&[TELNET_IAC, TELNET_SB, option]) {
        return false;
    }
    for &byte in data {
        if !queue.push(byte) {
            return false;
        }
        if byte == TELNET_IAC && !queue.push(byte) {
            return false;
        }
    }
    queue.push_slice(&[TELNET_IAC, TELNET_SE])
}

fn queue_naws(queue: &mut ByteQueue<MAX_INPUT_QUEUE>) -> bool {
    queue_subnegotiation(
        queue,
        TELNET_NAWS,
        &[
            (TELNET_WINDOW_WIDTH >> 8) as u8,
            TELNET_WINDOW_WIDTH as u8,
            (TELNET_WINDOW_HEIGHT >> 8) as u8,
            TELNET_WINDOW_HEIGHT as u8,
        ],
    )
}

fn queue_initial_negotiation(queue: &mut ByteQueue<MAX_INPUT_QUEUE>) -> bool {
    queue.push_slice(&[
        TELNET_IAC,
        TELNET_WILL,
        TELNET_TERMINAL_TYPE,
        TELNET_IAC,
        TELNET_WILL,
        TELNET_NAWS,
        TELNET_IAC,
        TELNET_WILL,
        TELNET_BINARY,
        TELNET_IAC,
        TELNET_DO,
        TELNET_BINARY,
        TELNET_IAC,
        TELNET_DO,
        TELNET_SUPPRESS_GO_AHEAD,
    ])
}

#[allow(clippy::too_many_arguments)]
fn process_tcp_frame(
    putc: Putc,
    terminal_position: TerminalPosition,
    nic_send: NicSend,
    config: TelnetConfig,
    own_mac: MacAddress,
    server_mac: MacAddress,
    connection: &mut TcpConnection<TCP_RX_CAPACITY>,
    parser: &mut TelnetParser,
    terminal_output: &mut TerminalOutputParser,
    queue: &mut ByteQueue<MAX_INPUT_QUEUE>,
    segment_output: &mut [u8; TCP_SEGMENT_CAPACITY],
    app_output: &mut [u8; TCP_RX_CAPACITY],
    frame: &[u8],
    now: u32,
    max_frame_len: usize,
) -> Option<bool> {
    let ethernet = EthernetFrame::parse(frame).ok()?;
    if ethernet.ethertype != ETHERTYPE_IPV4 {
        return None;
    }
    let packet = Ipv4Packet::parse(ethernet.payload, true).ok()?;
    if packet.header.protocol != 6
        || packet.header.source != config.server_ip
        || packet.header.destination != config.local_ip
    {
        return None;
    }
    let segment =
        TcpSegment::parse(packet.payload, config.server_ip, config.local_ip, true).ok()?;
    if let Ok(Some(length)) = connection.on_segment(&segment, now, segment_output) {
        let _ = send_tcp_bytes(
            nic_send,
            config,
            own_mac,
            server_mac,
            &segment_output[..length],
            max_frame_len,
        );
    }

    let count = connection.take_received(app_output);
    for &byte in &app_output[..count] {
        match parser.feed(byte) {
            TelnetEvent::Data(byte) => {
                let query = terminal_output.feed(byte);
                putc(u32::from(byte));
                if let Some(query) = query
                    && !queue_terminal_query_response(queue, query, terminal_position)
                {
                    print_bytes(putc, b"TELNET ERROR: terminal response queue is full\n");
                    return Some(true);
                }
            }
            TelnetEvent::Reply { verb, option } => {
                if !queue_negotiation(queue, TelnetEvent::Reply { verb, option }) {
                    print_bytes(putc, b"TELNET ERROR: negotiation queue is full\n");
                    return Some(true);
                }
            }
            TelnetEvent::Subnegotiation {
                option,
                data,
                length,
            } if option == TELNET_TERMINAL_TYPE && length == 1 && data[0] == TELNET_TTYPE_SEND => {
                let mut terminal_type = [0u8; 1 + TELNET_TERMINAL_NAME.len()];
                terminal_type[0] = TELNET_TTYPE_IS;
                terminal_type[1..].copy_from_slice(TELNET_TERMINAL_NAME);
                if !queue_subnegotiation(queue, TELNET_TERMINAL_TYPE, &terminal_type) {
                    print_bytes(putc, b"TELNET ERROR: negotiation queue is full\n");
                    return Some(true);
                }
            }
            TelnetEvent::Subnegotiation { .. } | TelnetEvent::None => {}
        }
    }

    if connection.state() == TcpState::CloseWait {
        if let Ok(length) = connection.close(now, segment_output) {
            let _ = send_tcp_bytes(
                nic_send,
                config,
                own_mac,
                server_mac,
                &segment_output[..length],
                max_frame_len,
            );
        }
        return Some(true);
    }
    Some(connection.state() == TcpState::Closed)
}

fn close_connection(
    nic_send: NicSend,
    config: TelnetConfig,
    own_mac: MacAddress,
    server_mac: MacAddress,
    connection: &mut TcpConnection<TCP_RX_CAPACITY>,
    segment_output: &mut [u8; TCP_SEGMENT_CAPACITY],
    max_frame_len: usize,
) {
    if matches!(
        connection.state(),
        TcpState::Established | TcpState::CloseWait
    ) && let Ok(length) = connection.close(0, segment_output)
    {
        let _ = send_tcp_bytes(
            nic_send,
            config,
            own_mac,
            server_mac,
            &segment_output[..length],
            max_frame_len,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn run_session(
    putc: Putc,
    terminal_position: TerminalPosition,
    read_key: ReadKey,
    nic_receive: NicReceive,
    nic_send: NicSend,
    ticks: Ticks,
    config: TelnetConfig,
    own_mac: MacAddress,
    server_mac: MacAddress,
    arp: &mut ArpCache<8>,
    queue: &mut ByteQueue<MAX_INPUT_QUEUE>,
    receive_buffer: &mut [u8; MAX_FRAME_LEN],
    max_frame_len: usize,
) {
    // A fresh local port for every run (40000-59999, from the clock). A fixed one made a second connection to a server that had
    // closed the first fail: the server still holds the old connection in TIME_WAIT and ignores a SYN on the same port whose
    // sequence number is lower than the old one's.
    let local_port = TELNET_LOCAL_PORT + ((ticks() as u32).wrapping_mul(2_654_435_761) >> 16) as u16 % 20_000;
    let mut connection = TcpConnection::<TCP_RX_CAPACITY>::active(
        config.local_ip,
        local_port,
        config.server_ip,
        config.server_port,
        (ticks() as u32) ^ u32::from_be_bytes(own_mac.0[..4].try_into().unwrap()),
        TCP_RETRANSMIT_TICKS,
    );
    // Keep the handshake and terminal loop together so the same TCP state
    // machine instance survives the SYN exchange into established mode.
    let mut segment_output = [0u8; TCP_SEGMENT_CAPACITY];
    let mut app_output = [0u8; TCP_RX_CAPACITY];
    let mut parser = TelnetParser::new();
    let mut terminal_output = TerminalOutputParser::new();
    let mut tx = [0u8; TCP_SEGMENT_CAPACITY];

    let Ok(syn_length) = connection.start_connect(ticks() as u32, &mut segment_output) else {
        print_bytes(putc, b"TELNET ERROR: could not create TCP SYN\n");
        return;
    };
    if !send_tcp_bytes(
        nic_send,
        config,
        own_mac,
        server_mac,
        &segment_output[..syn_length],
        max_frame_len,
    ) {
        print_bytes(putc, b"TELNET ERROR: NIC rejected TCP SYN\n");
        return;
    }
    let deadline = ticks().wrapping_add(CONNECT_TIMEOUT_TICKS);
    while connection.state() != TcpState::Established && connection.state() != TcpState::Closed {
        if tick_reached(ticks(), deadline) {
            print_bytes(putc, b"TELNET ERROR: TCP connection timed out\n");
            return;
        }
        match queue_input(read_key, queue) {
            Ok(true) => {
                print_bytes(putc, MARKER_ABORTED);
                return;
            }
            Ok(false) => {}
            Err(()) => {
                print_bytes(putc, b"TELNET ERROR: terminal input queue is full\n");
                return;
            }
        }
        let length = nic_receive(receive_buffer.as_mut_ptr() as usize, max_frame_len as u32) as usize;
        if length != 0 {
            let length = min(length, MAX_FRAME_LEN);
            if let Ok(ethernet) = EthernetFrame::parse(&receive_buffer[..length]) {
                if ethernet.ethertype == ETHERTYPE_ARP {
                    handle_arp(
                        ethernet,
                        config,
                        own_mac,
                        arp,
                        nic_send,
                        max_frame_len,
                        ticks() as u32,
                    );
                } else {
                    let _ = process_tcp_frame(
                        putc,
                        terminal_position,
                        nic_send,
                        config,
                        own_mac,
                        server_mac,
                        &mut connection,
                        &mut parser,
                        &mut terminal_output,
                        queue,
                        &mut segment_output,
                        &mut app_output,
                        &receive_buffer[..length],
                        ticks() as u32,
                        max_frame_len,
                    );
                }
            }
        }
        if let Ok(Some(length)) = connection.retransmit(ticks() as u32, &mut segment_output) {
            let _ = send_tcp_bytes(
                nic_send,
                config,
                own_mac,
                server_mac,
                &segment_output[..length],
                max_frame_len,
            );
        }
        // Nothing arrived and no key was read: sleep until a frame, a key, or
        // the next timer tick (which also drives the retransmit deadline).
        if length == 0 && !take_key_seen() {
            tape_rt::idle();
        }
    }
    if connection.state() == TcpState::Closed {
        print_bytes(putc, MARKER_DISCONNECTED);
        return;
    }
    print_bytes(putc, MARKER_CONNECTED);
    if !queue_initial_negotiation(queue) {
        print_bytes(putc, b"TELNET ERROR: negotiation queue is full\n");
        close_connection(
            nic_send,
            config,
            own_mac,
            server_mac,
            &mut connection,
            &mut segment_output,
            max_frame_len,
        );
        return;
    }

    loop {
        match queue_input(read_key, queue) {
            Ok(true) => {
                close_connection(
                    nic_send,
                    config,
                    own_mac,
                    server_mac,
                    &mut connection,
                    &mut segment_output,
                    max_frame_len,
                );
                print_bytes(putc, MARKER_ABORTED);
                return;
            }
            Ok(false) => {}
            Err(()) => {
                print_bytes(putc, b"TELNET ERROR: terminal output queue is full\n");
                close_connection(
                    nic_send,
                    config,
                    own_mac,
                    server_mac,
                    &mut connection,
                    &mut segment_output,
                    max_frame_len,
                );
                return;
            }
        }

        let length = nic_receive(receive_buffer.as_mut_ptr() as usize, max_frame_len as u32) as usize;
        if length != 0 {
            let length = min(length, MAX_FRAME_LEN);
            if let Ok(ethernet) = EthernetFrame::parse(&receive_buffer[..length]) {
                if ethernet.ethertype == ETHERTYPE_ARP {
                    handle_arp(
                        ethernet,
                        config,
                        own_mac,
                        arp,
                        nic_send,
                        max_frame_len,
                        ticks() as u32,
                    );
                } else if let Some(closed) = process_tcp_frame(
                    putc,
                    terminal_position,
                    nic_send,
                    config,
                    own_mac,
                    server_mac,
                    &mut connection,
                    &mut parser,
                    &mut terminal_output,
                    queue,
                    &mut segment_output,
                    &mut app_output,
                    &receive_buffer[..length],
                    ticks() as u32,
                    max_frame_len,
                ) && closed
                {
                    print_bytes(putc, MARKER_DISCONNECTED);
                    return;
                }
            }
        }

        if let Ok(Some(length)) = connection.retransmit(ticks() as u32, &mut segment_output) {
            let _ = send_tcp_bytes(
                nic_send,
                config,
                own_mac,
                server_mac,
                &segment_output[..length],
                max_frame_len,
            );
        }

        if connection.state() == TcpState::Established
            && connection.retransmission_deadline().is_none()
        {
            let count = queue.peek(&mut tx);
            if count != 0
                && let Ok(length) =
                    connection.send_data(&tx[..count], ticks() as u32, &mut segment_output)
                && send_tcp_bytes(
                    nic_send,
                    config,
                    own_mac,
                    server_mac,
                    &segment_output[..length],
                    max_frame_len,
                )
            {
                queue.discard(count);
            }
        }
        if connection.state() == TcpState::Closed {
            print_bytes(putc, MARKER_DISCONNECTED);
            return;
        }
        // An idle session: no frame and no key this pass. Typed bytes already
        // queued above were sent in this same pass, and an ACK, server data,
        // a keypress, or the next timer tick (retransmit) wakes this sleep.
        if length == 0 && !take_key_seen() {
            tape_rt::idle();
        }
    }
}

/// Tape ABI entry. The networking callbacks after `ticks` are declared even
/// though this single-NIC client does not use them, because the cursor-position
/// callback is append-only after the multi-NIC ABI.
#[unsafe(no_mangle)]
pub extern "C" fn tape_main(api: &'static tape_rt::TapeApi) {
    tape_rt::init(api);
    use tape_rt::compat::*;
    let putc = api.putc;
    let args = tape_rt::args();
    let (args_ptr, args_len) = (args.as_ptr() as usize, args.len() as u32);

    let arguments = if args_len == 0 {
        &[][..]
    } else {
        unsafe { core::slice::from_raw_parts(args_ptr as *const u8, args_len as usize) }
    };
    let startup = match parse_startup(arguments) {
        Ok(startup) => startup,
        Err(error) => {
            print_startup_error(putc, error);
            return;
        }
    };

    let mut mac_bytes = [0u8; 6];
    if nic_mac(mac_bytes.as_mut_ptr() as usize) == 0 {
        print_bytes(putc, b"TELNET ERROR: no supported NIC is available\n");
        return;
    }
    let own_mac = MacAddress::new(mac_bytes);
    let max_frame_len = min(nic_max_frame_len() as usize, MAX_FRAME_LEN);
    if max_frame_len < 60 {
        print_bytes(putc, b"TELNET ERROR: NIC frame limit is too small\n");
        return;
    }

    let mut receive_buffer = [0u8; MAX_FRAME_LEN];
    let config = match startup {
        StartupMode::Static(config) => config,
        StartupMode::Dhcp {
            server_ip,
            server_port,
        } => match configure_dhcp(
            putc,
            read_key,
            nic_send,
            nic_receive,
            ticks,
            own_mac,
            server_ip,
            server_port,
            &mut receive_buffer,
            max_frame_len,
        ) {
            DhcpResult::Configured(config) => config,
            DhcpResult::Aborted => {
                print_bytes(putc, MARKER_ABORTED);
                return;
            }
            DhcpResult::Failed => return,
        },
    };

    print_config(putc, config);
    print_bytes(putc, MARKER_CONNECTING);
    let next_hop = if same_subnet(config.local_ip, config.netmask, config.server_ip) {
        config.server_ip
    } else {
        config.gateway
    };
    if next_hop == Ipv4Address::UNSPECIFIED {
        print_bytes(putc, b"TELNET ERROR: no route to server\n");
        return;
    }

    let mut arp = ArpCache::<8>::new();
    let mut queue = ByteQueue::<MAX_INPUT_QUEUE>::new();
    match resolve_server_mac(
        putc,
        read_key,
        nic_receive,
        nic_send,
        ticks,
        config,
        own_mac,
        &mut arp,
        &mut queue,
        &mut receive_buffer,
        max_frame_len,
    ) {
        ConnectResult::Connected => {}
        ConnectResult::Aborted => {
            print_bytes(putc, MARKER_ABORTED);
            return;
        }
        ConnectResult::Failed => return,
    }
    let Some(server_mac) = arp.lookup(next_hop, ticks() as u32) else {
        print_bytes(putc, b"TELNET ERROR: ARP entry disappeared\n");
        return;
    };

    run_session(
        putc,
        terminal_position,
        read_key,
        nic_receive,
        nic_send,
        ticks,
        config,
        own_mac,
        server_mac,
        &mut arp,
        &mut queue,
        &mut receive_buffer,
        max_frame_len,
    );
}
