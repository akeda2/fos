//! Telnet as a link: the telnet tape's session turned into a state machine that `poll` advances a step at a time, so the
//! terminal loop (keys, Ctrl-A commands) keeps running while DHCP, ARP and the TCP handshake are in flight.
//!
//! Dhcp -> Arp -> Connect -> Up -> Closed. The protocol pieces are the telnet tape's (`telnet_tape`: option parser, IAC
//! doubling, TCP frame builder) and `tape_net`'s fixed-buffer ARP/DHCP/TCP; nothing here allocates. What the telnet tape does
//! on top and this link does not: it does not answer the remote's cursor-position queries (ESC [ 6 n).

use crate::link::{Link, MAX_FRAME, Nic};
use core::cmp::min;
use fterm::TelnetTarget;
use tape_net::frames;
use tape_net::{
    ArpCache, ArpOperation, ArpPacket, DHCP_BOOTSTRAP_DESTINATION, DhcpClient, DhcpClientState, ETHERTYPE_ARP, ETHERTYPE_IPV4,
    EthernetFrame, Ipv4Address, Ipv4Packet, MacAddress, TcpConnection, TcpSegment, TcpState,
};
use tape_rt::println;
use telnet_tape::{
    TCP_SEGMENT_CAPACITY, TELNET_BINARY, TELNET_DO, TELNET_IAC, TELNET_NAWS, TELNET_SB, TELNET_SE, TELNET_SUPPRESS_GO_AHEAD,
    TELNET_TERMINAL_NAME, TELNET_TERMINAL_TYPE, TELNET_TTYPE_IS, TELNET_TTYPE_SEND, TELNET_WILL, TELNET_WINDOW_HEIGHT,
    TELNET_WINDOW_WIDTH, TelnetEvent, TelnetParser, build_tcp_frame, encode_application, same_subnet,
};

const TCP_RX_CAPACITY: usize = 1024;
const TX_CAPACITY: usize = 1024;
const TCP_RETRANSMIT_TICKS: u32 = 100;
const ARP_RETRY_TICKS: u64 = 100;
const ARP_TIMEOUT_TICKS: u64 = 400;
const CONNECT_TIMEOUT_TICKS: u64 = 1500;
const DHCP_TIMEOUT_SECONDS: u32 = 8;
const LOCAL_PORT_BASE: u16 = 40000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Dhcp,
    Arp,
    Connect,
    Up,
    Closed,
}

pub struct TelnetLink {
    nic: Nic,
    phase: Phase,
    server: Ipv4Address,
    server_port: u16,
    local: Ipv4Address,
    netmask: Ipv4Address,
    gateway: Ipv4Address,
    next_hop: Ipv4Address,
    server_mac: MacAddress,
    dhcp: DhcpClient<128>,
    dhcp_started: u32,
    arp: ArpCache<8>,
    arp_sent: u64,
    phase_started: u64,
    connection: Option<TcpConnection<TCP_RX_CAPACITY>>,
    parser: TelnetParser,
    /// Application bytes for the screen, filled from one TCP read.
    screen: [u8; TCP_RX_CAPACITY],
    screen_len: usize,
    screen_pos: usize,
    /// Bytes for the server: typed (IAC doubled) and negotiation replies.
    tx: [u8; TX_CAPACITY],
    tx_len: usize,
    frame: [u8; MAX_FRAME],
    segment: [u8; TCP_SEGMENT_CAPACITY],
    sent: u32,
    received: u32,
}

fn prefix_mask(prefix: u8) -> Ipv4Address {
    let mask = if prefix == 0 { 0 } else { u32::MAX << (32 - u32::from(prefix)) };
    Ipv4Address::new(mask.to_be_bytes())
}

impl TelnetLink {
    pub fn open(nic: Nic, target: &TelnetTarget) -> TelnetLink {
        let now = tape_rt::ticks();
        let own = nic.mac.0;
        let transaction = (now as u32) ^ u32::from_be_bytes([own[0], own[1], own[2], own[3]]) ^ u32::from_be_bytes([own[2], own[3], own[4], own[5]]);
        let mut link = TelnetLink {
            nic,
            phase: Phase::Dhcp,
            server: Ipv4Address::new(target.server),
            server_port: target.port,
            local: Ipv4Address::UNSPECIFIED,
            netmask: Ipv4Address::UNSPECIFIED,
            gateway: Ipv4Address::UNSPECIFIED,
            next_hop: Ipv4Address::UNSPECIFIED,
            server_mac: MacAddress([0; 6]),
            dhcp: DhcpClient::<128>::new(nic.mac, transaction, 1, 5),
            dhcp_started: (now / 100) as u32,
            arp: ArpCache::<8>::new(),
            arp_sent: 0,
            phase_started: now,
            connection: None,
            parser: TelnetParser::new(),
            screen: [0; TCP_RX_CAPACITY],
            screen_len: 0,
            screen_pos: 0,
            tx: [0; TX_CAPACITY],
            tx_len: 0,
            frame: [0; MAX_FRAME],
            segment: [0; TCP_SEGMENT_CAPACITY],
            sent: 0,
            received: 0,
        };
        match target.fixed {
            Some(fixed) => {
                link.local = Ipv4Address::new(fixed.local);
                link.netmask = prefix_mask(fixed.prefix);
                link.gateway = Ipv4Address::new(fixed.gateway);
                link.start_arp();
            }
            None => println!("[DHCP: asking for an address...]"),
        }
        link
    }

    fn fail(&mut self, message: &str) {
        println!("\n[telnet: {message}]");
        self.phase = Phase::Closed;
    }

    fn start_arp(&mut self) {
        self.next_hop = if same_subnet(self.local, self.netmask, self.server) { self.server } else { self.gateway };
        if self.next_hop == Ipv4Address::UNSPECIFIED {
            self.fail("the server is off-subnet and there is no gateway");
            return;
        }
        println!("[telnet: {}.{}.{}.{} port {}, local {}.{}.{}.{}]", self.server.0[0], self.server.0[1], self.server.0[2], self.server.0[3], self.server_port,
            self.local.0[0], self.local.0[1], self.local.0[2], self.local.0[3]);
        self.phase = Phase::Arp;
        self.phase_started = tape_rt::ticks();
        self.arp_sent = 0;
    }

    fn send_tcp(&mut self, length: usize) -> bool {
        let mut out = [0u8; MAX_FRAME];
        let Ok(parsed) = TcpSegment::parse(&self.segment[..length], self.local, self.server, true) else { return false };
        let Ok(total) = build_tcp_frame(&mut out, self.nic.mac, self.server_mac, self.local, self.server, &parsed) else { return false };
        self.nic.send(&out[..total])
    }

    fn handle_arp(&mut self, length: usize, now: u32) {
        let Ok(ethernet) = EthernetFrame::parse(&self.frame[..length]) else { return };
        let Ok(packet) = ArpPacket::parse(ethernet.payload) else { return };
        self.arp.insert(packet.sender_protocol, packet.sender_hardware, now);
        if packet.operation == ArpOperation::Request && packet.target_protocol == self.local && packet.sender_hardware != self.nic.mac {
            let mut out = [0u8; 64];
            if let Some(total) = frames::arp_reply_frame(&mut out, self.nic.mac, self.local, packet.sender_hardware, packet.sender_protocol) {
                self.nic.send(&out[..total]);
            }
        }
    }

    fn poll_dhcp(&mut self) -> bool {
        let now = (tape_rt::ticks() / 100) as u32;
        if now.wrapping_sub(self.dhcp_started) > DHCP_TIMEOUT_SECONDS {
            self.fail("no DHCP server answered");
            return true;
        }
        let mut payload = [0u8; 600];
        match self.dhcp.poll(now, &mut payload) {
            Ok(Some(length)) => {
                let mut out = [0u8; MAX_FRAME];
                match frames::dhcp_client_frame(&mut out, self.nic.mac, Ipv4Address::UNSPECIFIED, DHCP_BOOTSTRAP_DESTINATION, &payload[..length]) {
                    Some(total) if self.nic.send(&out[..total]) => {}
                    _ => {
                        self.fail("the NIC refused the DHCP request");
                        return true;
                    }
                }
            }
            Ok(None) => {}
            Err(_) => {
                self.fail("DHCP failed");
                return true;
            }
        }
        match self.dhcp.state() {
            DhcpClientState::Bound => match self.dhcp.lease() {
                Some(lease) if lease.subnet_mask != Ipv4Address::UNSPECIFIED => {
                    self.local = lease.address;
                    self.netmask = lease.subnet_mask;
                    self.gateway = lease.router;
                    println!("[DHCP: leased {}.{}.{}.{}]", lease.address.0[0], lease.address.0[1], lease.address.0[2], lease.address.0[3]);
                    self.start_arp();
                    return true;
                }
                _ => {
                    self.fail("DHCP gave no usable lease");
                    return true;
                }
            },
            DhcpClientState::Failed => {
                self.fail("DHCP failed");
                return true;
            }
            _ => {}
        }
        let length = self.nic.receive(&mut self.frame);
        if length != 0 {
            frames::feed_dhcp(&self.frame[..length], &mut self.dhcp, now);
        }
        length != 0
    }

    fn poll_arp(&mut self) -> bool {
        let ticks = tape_rt::ticks();
        if let Some(mac) = self.arp.lookup(self.next_hop, ticks as u32) {
            self.server_mac = mac;
            return self.start_connect();
        }
        if ticks.wrapping_sub(self.phase_started) > ARP_TIMEOUT_TICKS {
            self.fail("ARP found no answer for the server");
            return true;
        }
        if self.arp_sent == 0 || ticks.wrapping_sub(self.arp_sent) >= ARP_RETRY_TICKS {
            let mut out = [0u8; 64];
            if let Some(total) = frames::arp_request_frame(&mut out, self.nic.mac, self.local, self.next_hop) {
                self.nic.send(&out[..total]);
            }
            self.arp_sent = ticks.max(1);
        }
        let length = self.nic.receive(&mut self.frame);
        if length != 0 {
            if EthernetFrame::parse(&self.frame[..length]).is_ok_and(|e| e.ethertype == ETHERTYPE_ARP) {
                self.handle_arp(length, ticks as u32);
            }
        }
        length != 0
    }

    fn start_connect(&mut self) -> bool {
        let ticks = tape_rt::ticks();
        // A fresh local port per run: a server still holding the last connection in TIME_WAIT ignores a SYN on the same port.
        let port = LOCAL_PORT_BASE + ((ticks as u32).wrapping_mul(2_654_435_761) >> 16) as u16 % 20_000;
        let own = self.nic.mac.0;
        let isn = (ticks as u32) ^ u32::from_be_bytes([own[0], own[1], own[2], own[3]]);
        let mut connection = TcpConnection::<TCP_RX_CAPACITY>::active(self.local, port, self.server, self.server_port, isn, TCP_RETRANSMIT_TICKS);
        let Ok(length) = connection.start_connect(ticks as u32, &mut self.segment) else {
            self.fail("could not build the TCP SYN");
            return true;
        };
        self.connection = Some(connection);
        if !self.send_tcp(length) {
            self.fail("the NIC refused the TCP SYN");
            return true;
        }
        self.phase = Phase::Connect;
        self.phase_started = ticks;
        true
    }

    fn queue(&mut self, bytes: &[u8]) -> bool {
        if bytes.len() > TX_CAPACITY - self.tx_len {
            return false;
        }
        self.tx[self.tx_len..self.tx_len + bytes.len()].copy_from_slice(bytes);
        self.tx_len += bytes.len();
        true
    }

    fn queue_subnegotiation(&mut self, option: u8, data: &[u8]) {
        let mut out = [0u8; 32];
        let mut length = 0;
        for &byte in [TELNET_IAC, TELNET_SB, option].iter().chain(data) {
            out[length] = byte;
            length += 1;
            if byte == TELNET_IAC && length > 3 {
                out[length] = byte;
                length += 1;
            }
        }
        out[length] = TELNET_IAC;
        out[length + 1] = TELNET_SE;
        self.queue(&out[..length + 2]);
    }

    fn queue_naws(&mut self) {
        let data = [(TELNET_WINDOW_WIDTH >> 8) as u8, TELNET_WINDOW_WIDTH as u8, (TELNET_WINDOW_HEIGHT >> 8) as u8, TELNET_WINDOW_HEIGHT as u8];
        self.queue_subnegotiation(TELNET_NAWS, &data);
    }

    fn feed_application(&mut self, count: usize, application: &[u8; TCP_RX_CAPACITY]) {
        for &byte in &application[..count] {
            match self.parser.feed(byte) {
                TelnetEvent::Data(byte) => {
                    if self.screen_len < TCP_RX_CAPACITY {
                        self.screen[self.screen_len] = byte;
                        self.screen_len += 1;
                    }
                }
                TelnetEvent::Reply { verb, option } => {
                    self.queue(&[TELNET_IAC, verb, option]);
                    if verb == TELNET_WILL && option == TELNET_NAWS {
                        self.queue_naws();
                    }
                }
                TelnetEvent::Subnegotiation { option, data, length } if option == TELNET_TERMINAL_TYPE && length == 1 && data[0] == TELNET_TTYPE_SEND => {
                    let mut name = [0u8; 1 + TELNET_TERMINAL_NAME.len()];
                    name[0] = TELNET_TTYPE_IS;
                    name[1..].copy_from_slice(TELNET_TERMINAL_NAME);
                    self.queue_subnegotiation(TELNET_TERMINAL_TYPE, &name);
                }
                _ => {}
            }
        }
    }

    /// One received frame in the Connect/Up phases. True if the frame was ours.
    fn process_frame(&mut self, length: usize) -> bool {
        let ticks = tape_rt::ticks() as u32;
        let Ok(ethernet) = EthernetFrame::parse(&self.frame[..length]) else { return true };
        if ethernet.ethertype == ETHERTYPE_ARP {
            self.handle_arp(length, ticks);
            return true;
        }
        if ethernet.ethertype != ETHERTYPE_IPV4 {
            return true;
        }
        let Ok(packet) = Ipv4Packet::parse(ethernet.payload, true) else { return true };
        if packet.header.protocol != 6 || packet.header.source != self.server || packet.header.destination != self.local {
            return true;
        }
        let Ok(segment) = TcpSegment::parse(packet.payload, self.server, self.local, true) else { return true };
        let Some(connection) = self.connection.as_mut() else { return true };
        let reply = connection.on_segment(&segment, ticks, &mut self.segment);
        let mut application = [0u8; TCP_RX_CAPACITY];
        let count = connection.take_received(&mut application);
        let state = connection.state();
        if let Ok(Some(length)) = reply {
            self.send_tcp(length);
        }
        self.received = self.received.wrapping_add(count as u32);
        self.feed_application(count, &application);
        if state == TcpState::CloseWait {
            if let Some(connection) = self.connection.as_mut()
                && let Ok(length) = connection.close(ticks, &mut self.segment)
            {
                self.send_tcp(length);
            }
            self.phase = Phase::Closed;
        }
        true
    }

    fn poll_session(&mut self) -> bool {
        let ticks = tape_rt::ticks();
        let mut busy = false;
        // Hold off reading the NIC while the screen has not taken the last data.
        if self.screen_pos == self.screen_len {
            self.screen_len = 0;
            self.screen_pos = 0;
            let length = self.nic.receive(&mut self.frame);
            if length != 0 {
                busy = true;
                self.process_frame(length);
            }
        }
        let Some(connection) = self.connection.as_mut() else { return busy };
        match connection.retransmit(ticks as u32, &mut self.segment) {
            Ok(Some(length)) => {
                self.send_tcp(length);
            }
            _ => {}
        }
        let connection = self.connection.as_mut().unwrap();
        let state = connection.state();
        if self.phase == Phase::Connect {
            if state == TcpState::Established {
                println!("[telnet: connected. Ctrl-A x leaves, Ctrl-A h helps]");
                self.phase = Phase::Up;
                self.queue(&[
                    TELNET_IAC, TELNET_WILL, TELNET_TERMINAL_TYPE, TELNET_IAC, TELNET_WILL, TELNET_NAWS, TELNET_IAC, TELNET_WILL, TELNET_BINARY,
                    TELNET_IAC, TELNET_DO, TELNET_BINARY, TELNET_IAC, TELNET_DO, TELNET_SUPPRESS_GO_AHEAD,
                ]);
                busy = true;
            } else if state == TcpState::Closed {
                self.fail("the server refused the connection");
                return true;
            } else if ticks.wrapping_sub(self.phase_started) > CONNECT_TIMEOUT_TICKS {
                self.fail("the connection timed out");
                return true;
            }
        }
        if self.phase == Phase::Up {
            if state == TcpState::Closed {
                self.phase = Phase::Closed;
                return true;
            }
            if state == TcpState::Established && self.connection.as_ref().unwrap().retransmission_deadline().is_none() && self.tx_len != 0 {
                let count = min(self.tx_len, TCP_SEGMENT_CAPACITY - 64);
                let data = self.tx;
                let connection = self.connection.as_mut().unwrap();
                if let Ok(length) = connection.send_data(&data[..count], ticks as u32, &mut self.segment) && self.send_tcp(length) {
                    self.tx.copy_within(count..self.tx_len, 0);
                    self.tx_len -= count;
                    self.sent = self.sent.wrapping_add(count as u32);
                    busy = true;
                }
            }
        }
        busy
    }
}

impl Link for TelnetLink {
    fn poll(&mut self) -> bool {
        match self.phase {
            Phase::Dhcp => self.poll_dhcp(),
            Phase::Arp => self.poll_arp(),
            Phase::Connect | Phase::Up => self.poll_session(),
            Phase::Closed => false,
        }
    }

    fn read(&mut self, out: &mut [u8]) -> usize {
        let count = min(out.len(), self.screen_len - self.screen_pos);
        out[..count].copy_from_slice(&self.screen[self.screen_pos..self.screen_pos + count]);
        self.screen_pos += count;
        count
    }

    fn write(&mut self, data: &[u8]) -> usize {
        // Typed bytes wait for the session; before that the loop keeps them.
        if self.phase != Phase::Up {
            return 0;
        }
        let mut taken = 0;
        for &byte in data {
            let mut encoded = [0u8; 2];
            let Some(length) = encode_application(&[byte], &mut encoded) else { break };
            if !self.queue(&encoded[..length]) {
                break;
            }
            taken += 1;
        }
        taken
    }

    fn is_open(&self) -> bool {
        self.phase != Phase::Closed || self.screen_pos < self.screen_len
    }

    fn status(&mut self) {
        let name = match self.phase {
            Phase::Dhcp => "asking DHCP",
            Phase::Arp => "resolving the server",
            Phase::Connect => "connecting",
            Phase::Up => "connected",
            Phase::Closed => "closed",
        };
        println!(
            "\n[fterm: telnet {}.{}.{}.{}:{} {}, sent {} received {}]",
            self.server.0[0], self.server.0[1], self.server.0[2], self.server.0[3], self.server_port, name, self.sent, self.received
        );
    }

    fn close(&mut self) {
        if let Some(connection) = self.connection.as_mut()
            && matches!(connection.state(), TcpState::Established | TcpState::CloseWait)
            && let Ok(length) = connection.close(0, &mut self.segment)
        {
            self.send_tcp(length);
        }
        self.phase = Phase::Closed;
    }
}
