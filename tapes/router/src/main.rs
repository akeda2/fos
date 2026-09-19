#![no_std]
#![no_main]

//! A small fixed-buffer IPv4 router.
//!
//! The bridge tape forwards Ethernet frames unchanged.  This tape reuses its
//! selected-interface polling structure but operates one layer higher:
//! Ethernet/ARP terminates on each configured interface, directly connected
//! IPv4 subnets are selected with longest-prefix matching, an optional
//! default gateway handles otherwise-unmatched destinations, IPv4 TTL and
//! checksum are updated, and the packet is emitted with the egress
//! interface's MAC address after ARP resolution.
//!
//! Run with one or more triples, for example:
//!
//! ```text
//! tape run router 1 10.0.1.1/24 2 10.0.2.1/24
//! tape run router dhcp 2 10.0.2.1/24
//! tape run router dhcp-server 1 10.0.1.1/24 2 10.0.2.1/24
//! tape run router 1 10.0.1.1/24 2 10.0.2.1/24 default 1 10.0.1.254
//! ```
//!
//! `dhcp` configures NIC 1 (callback index zero), and a trailing `default NIC
//! GATEWAY` selects the egress NIC and next hop for unmatched destinations.
//! Without an explicit default, a DHCP router option becomes NIC 1's default.
//! `dhcp-server` serves every configured interface except NIC 1, deriving a
//! pool from each subnet and allocating one in-memory lease slot per usable
//! address for this run.
//! Static interfaces also accept the legacy `NIC IP NETMASK` form and the
//! separate `NIC IP /PREFIX` form.
//! The router remains a fixed-buffer teaching example: there is no NAT,
//! dynamic routing protocol, ICMP error generation, fragmentation, or IPv6.

use core::cmp::min;

use router_tape::{
    ConfigError, InterfaceConfig, MAX_INTERFACES, RouterConfig, parse_config, route_next_hop,
};
use tape_net::{
    ArpCache, ArpOperation, ArpPacket, DHCP_CLIENT_PORT, DHCP_MAX_DNS_SERVERS,
    DHCP_OPTION_BROADCAST_ADDRESS, DHCP_OPTION_DNS, DHCP_OPTION_LEASE_TIME,
    DHCP_OPTION_MESSAGE_TYPE, DHCP_OPTION_REBINDING_TIME, DHCP_OPTION_RENEWAL_TIME,
    DHCP_OPTION_REQUESTED_IP, DHCP_OPTION_ROUTER, DHCP_OPTION_SERVER_IDENTIFIER,
    DHCP_OPTION_SUBNET_MASK, DHCP_SERVER_PORT, DhcpClient, DhcpClientState, DhcpLease,
    DhcpMessageType, DhcpOp, DhcpOptionWriter, DhcpOptions, DhcpPacket, ETHERTYPE_ARP,
    ETHERTYPE_IPV4, EthernetFrame, Ipv4Address, Ipv4Header, Ipv4Packet, MacAddress, UdpDatagram,
};
use tape_rt::{print_bytes, print_u32};

const MAX_FRAME_LEN: usize = 1514;
const ARP_TIMEOUT_TICKS: u64 = 300;
const ARP_CACHE_SIZE: usize = 8;
const BROADCAST_IP: Ipv4Address = Ipv4Address([255, 255, 255, 255]);
const DHCP_TIMEOUT_SECONDS: u32 = 8;
const DHCP_RETRY_SECONDS: u32 = 1;
const DHCP_MAX_RETRIES: u8 = 5;
const DHCP_LEASE_SECONDS: u32 = 7 * 24 * 60 * 60;
const DHCP_OFFER_SECONDS: u32 = 30;
const DHCP_RENEWAL_SECONDS: u32 = DHCP_LEASE_SECONDS / 2;
const DHCP_REBINDING_SECONDS: u32 = DHCP_LEASE_SECONDS * 7 / 8;

const TAPE_EVENT_ABORT: u32 = 1;

type Putc = extern "C" fn(u32);
type ShouldAbort = extern "C" fn() -> u32;
type Alloc = extern "C" fn(u32) -> u32;
type Free = extern "C" fn(u32, u32);
type NicCount = extern "C" fn() -> u32;
type NicMacAt = extern "C" fn(u32, u32) -> u32;
type NicSendAt = extern "C" fn(u32, u32, u32) -> u32;
type NicReceiveAt = extern "C" fn(u32, u32, u32) -> u32;
type NicSetPromiscuousAt = extern "C" fn(u32, u32) -> u32;
type Ticks = extern "C" fn() -> u64;

struct RouterInterface {
    nic: u32,
    address: Ipv4Address,
    netmask: Ipv4Address,
    mac: MacAddress,
    arp: ArpCache<ARP_CACHE_SIZE>,
}

#[repr(u8)]
#[derive(Clone, Copy, Eq, PartialEq)]
enum LeaseState {
    Empty,
    Offered,
    Bound,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct DhcpLeaseSlot {
    state: LeaseState,
    mac: MacAddress,
    expires_at: u32,
}

impl DhcpLeaseSlot {
    const fn empty() -> Self {
        Self {
            state: LeaseState::Empty,
            mac: MacAddress::ZERO,
            expires_at: 0,
        }
    }
}

const DHCP_STATIC_LEASE_SLOTS: usize = MAX_INTERFACES * 65_534;
static mut DHCP_STATIC_LEASES: [DhcpLeaseSlot; DHCP_STATIC_LEASE_SLOTS] =
    [DhcpLeaseSlot::empty(); DHCP_STATIC_LEASE_SLOTS];

struct DhcpServer<'a> {
    leases: &'a mut [DhcpLeaseSlot],
    pool_start: u32,
    pool_end: u32,
}

impl<'a> DhcpServer<'a> {
    fn new(leases: &'a mut [DhcpLeaseSlot], pool_start: u32, pool_end: u32) -> Self {
        for lease in leases.iter_mut() {
            *lease = DhcpLeaseSlot::empty();
        }
        Self {
            leases,
            pool_start,
            pool_end,
        }
    }

    fn address(&self, index: usize) -> Ipv4Address {
        address_from_u32(self.pool_start + index as u32)
    }

    fn expire(&mut self, now: u32) {
        for lease in self.leases.iter_mut() {
            if lease.state != LeaseState::Empty && now.wrapping_sub(lease.expires_at) < 0x8000_0000
            {
                *lease = DhcpLeaseSlot::empty();
            }
        }
    }

    fn find_mac(&self, mac: MacAddress) -> Option<usize> {
        self.leases
            .iter()
            .position(|lease| lease.state != LeaseState::Empty && lease.mac == mac)
    }

    fn address_index(&self, address: Ipv4Address) -> Option<usize> {
        let value = address_to_u32(address);
        if value < self.pool_start || value > self.pool_end {
            return None;
        }
        Some((value - self.pool_start) as usize)
    }

    fn find_address(&self, address: Ipv4Address) -> Option<usize> {
        let index = self.address_index(address)?;
        (self.leases[index].state != LeaseState::Empty).then_some(index)
    }

    fn allocate(&mut self, mac: MacAddress, now: u32) -> Option<usize> {
        self.expire(now);
        if let Some(index) = self.find_mac(mac) {
            return Some(index);
        }
        let slot = self
            .leases
            .iter()
            .position(|lease| lease.state == LeaseState::Empty)?;
        self.leases[slot] = DhcpLeaseSlot {
            state: LeaseState::Offered,
            mac,
            expires_at: now.wrapping_add(DHCP_OFFER_SECONDS),
        };
        Some(slot)
    }

    fn requested(
        &mut self,
        mac: MacAddress,
        requested: Option<Ipv4Address>,
        client_address: Ipv4Address,
        start: u32,
        end: u32,
        now: u32,
    ) -> Option<usize> {
        self.expire(now);
        let existing = self.find_mac(mac);
        let desired = requested
            .filter(|address| *address != Ipv4Address::UNSPECIFIED)
            .or_else(|| (client_address != Ipv4Address::UNSPECIFIED).then_some(client_address));
        if let Some(address) = desired {
            let value = address_to_u32(address);
            if value < start || value > end {
                return None;
            }
            if let Some(owner) = self.find_address(address) {
                if Some(owner) != existing {
                    return None;
                }
            }
        }
        let index = existing.or_else(|| {
            let address = desired?;
            let slot = self.address_index(address)?;
            if self.leases[slot].state != LeaseState::Empty {
                return None;
            }
            self.leases[slot] = DhcpLeaseSlot {
                state: LeaseState::Offered,
                mac,
                expires_at: now.wrapping_add(DHCP_OFFER_SECONDS),
            };
            Some(slot)
        })?;
        self.leases[index].state = LeaseState::Bound;
        self.leases[index].expires_at = now.wrapping_add(DHCP_LEASE_SECONDS);
        Some(index)
    }

    fn release(&mut self, mac: MacAddress, address: Ipv4Address) {
        if let Some(index) = self.find_mac(mac) {
            if address == Ipv4Address::UNSPECIFIED || self.address(index) == address {
                self.leases[index] = DhcpLeaseSlot::empty();
            }
        }
    }
}

fn free_dhcp_allocations(
    allocations: &mut [Option<(u32, u32)>; MAX_INTERFACES],
    free: Free,
) {
    for allocation in allocations.iter_mut() {
        if let Some((address, length)) = allocation.take() {
            free(address, length);
        }
    }
}

fn address_from_u32(value: u32) -> Ipv4Address {
    Ipv4Address(value.to_be_bytes())
}

fn address_to_u32(address: Ipv4Address) -> u32 {
    u32::from_be_bytes(address.0)
}

impl RouterInterface {
    const fn empty() -> Self {
        Self {
            nic: 0,
            address: Ipv4Address::UNSPECIFIED,
            netmask: Ipv4Address::UNSPECIFIED,
            mac: MacAddress::ZERO,
            arp: ArpCache::new(),
        }
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

fn print_mac(putc: Putc, address: MacAddress) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for (index, byte) in address.0.iter().copied().enumerate() {
        if index != 0 {
            putc(u32::from(b':'));
        }
        putc(u32::from(HEX[usize::from(byte >> 4)]));
        putc(u32::from(HEX[usize::from(byte & 0x0f)]));
    }
}

fn print_config_error(putc: Putc, error: ConfigError) {
    let message = match error {
        ConfigError::MissingInterface => &b"no interface configuration supplied"[..],
        ConfigError::TooManyInterfaces => &b"too many interfaces"[..],
        ConfigError::WrongArgumentCount => {
            &b"arguments must be NIC IP/PREFIX or NIC IP NETMASK entries"[..]
        }
        ConfigError::BadNicNumber => &b"NIC numbers must be positive decimal values"[..],
        ConfigError::DuplicateNic => &b"the same NIC was selected more than once"[..],
        ConfigError::BadAddress => &b"invalid dotted IPv4 address"[..],
        ConfigError::BadNetmask => &b"invalid contiguous IPv4 netmask"[..],
        ConfigError::MissingDefaultRouteValue => {
            &b"default must be followed by NIC and gateway"[..]
        }
        ConfigError::BadDefaultNic => &b"default-route NIC must be a positive decimal value"[..],
        ConfigError::BadGateway => &b"invalid or unreachable default gateway"[..],
        ConfigError::DefaultNicNotConfigured => {
            &b"default-route NIC is not among the configured interfaces"[..]
        }
        ConfigError::MissingDnsValue => &b"dns must be followed by at least one IPv4 address"[..],
        ConfigError::BadDnsServer => &b"invalid DNS server IPv4 address"[..],
        ConfigError::TooManyDnsServers => &b"at most two DNS servers may be configured"[..],
    };
    print_bytes(putc, b"Router configuration error: ");
    print_bytes(putc, message);
    print_bytes(
        putc,
        b". Usage: tape run router [dhcp] [dhcp-server] [dns SERVER [SERVER]] \
NIC IP NETMASK ... [default NIC GATEWAY] (or NIC IP/PREFIX)\n",
    );
}

fn print_startup(
    putc: Putc,
    interfaces: &[RouterInterface; MAX_INTERFACES],
    count: usize,
    default_route: Option<(u32, Ipv4Address)>,
) {
    print_bytes(putc, b"fos router: ");
    print_u32(putc, count as u32);
    print_bytes(putc, b" interface(s), directly connected IPv4 routing:\n");
    for interface in interfaces.iter().take(count) {
        print_bytes(putc, b"  NIC ");
        print_u32(putc, interface.nic + 1);
        print_bytes(putc, b" ");
        print_ip(putc, interface.address);
        print_bytes(putc, b"/");
        print_ip(putc, interface.netmask);
        print_bytes(putc, b" MAC ");
        print_mac(putc, interface.mac);
        print_bytes(putc, b"\n");
    }
    if let Some((nic, gateway)) = default_route {
        print_bytes(putc, b"  default route: NIC ");
        print_u32(putc, nic + 1);
        print_bytes(putc, b" via ");
        print_ip(putc, gateway);
        print_bytes(putc, b"\n");
    } else {
        print_bytes(
            putc,
            b"  default route: none (unknown networks are dropped)\n",
        );
    }
    print_bytes(
        putc,
        b"Forwarding IPv4 frames with TTL/checksum updates; F12 (or F4/F6/F7/F8/F10) stops.\n",
    );
}

fn send_arp_request(
    interface: &RouterInterface,
    target: Ipv4Address,
    nic_send_at: NicSendAt,
    max_frame_len: usize,
) -> bool {
    let packet = ArpPacket::request(interface.mac, interface.address, target);
    let mut arp = [0u8; ArpPacket::LEN];
    let Ok(arp_len) = packet.encode(&mut arp) else {
        return false;
    };
    let ethernet = EthernetFrame::new(
        MacAddress::BROADCAST,
        interface.mac,
        ETHERTYPE_ARP,
        &arp[..arp_len],
    );
    let mut frame = [0u8; MAX_FRAME_LEN];
    let Ok(length) = ethernet.encode(&mut frame) else {
        return false;
    };
    length <= max_frame_len && nic_send_at(interface.nic, frame.as_ptr() as u32, length as u32) != 0
}

fn send_arp_reply(
    interface: &RouterInterface,
    target_mac: MacAddress,
    target_ip: Ipv4Address,
    nic_send_at: NicSendAt,
    max_frame_len: usize,
) -> bool {
    let packet = ArpPacket::reply(interface.mac, interface.address, target_mac, target_ip);
    let mut arp = [0u8; ArpPacket::LEN];
    let Ok(arp_len) = packet.encode(&mut arp) else {
        return false;
    };
    let ethernet = EthernetFrame::new(target_mac, interface.mac, ETHERTYPE_ARP, &arp[..arp_len]);
    let mut frame = [0u8; MAX_FRAME_LEN];
    let Ok(length) = ethernet.encode(&mut frame) else {
        return false;
    };
    length <= max_frame_len && nic_send_at(interface.nic, frame.as_ptr() as u32, length as u32) != 0
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
    nic_send_at: NicSendAt,
    nic: u32,
    own_mac: MacAddress,
    payload: &[u8],
    target_mac: MacAddress,
    source: Ipv4Address,
    destination: Ipv4Address,
    source_port: u16,
    destination_port: u16,
    max_frame_len: usize,
) -> bool {
    let mut frame = [0u8; MAX_FRAME_LEN];
    let mut udp_bytes = [0u8; MAX_FRAME_LEN];
    let udp = UdpDatagram::new(source_port, destination_port, payload);
    let Ok(udp_len) = udp.encode_with_checksum(&mut udp_bytes, source, destination, false) else {
        return false;
    };
    let packet = Ipv4Packet {
        header: Ipv4Header::new(source, destination, 17, udp_len),
        payload: &udp_bytes[..udp_len],
    };
    let mut ip_bytes = [0u8; MAX_FRAME_LEN];
    let Ok(ip_len) = packet.encode(&mut ip_bytes) else {
        return false;
    };
    let ethernet = EthernetFrame::new(target_mac, own_mac, ETHERTYPE_IPV4, &ip_bytes[..ip_len]);
    let Ok(length) = ethernet.encode(&mut frame) else {
        return false;
    };
    length <= max_frame_len && nic_send_at(nic, frame.as_ptr() as u32, length as u32) != 0
}

fn dhcp_pool(interface: &RouterInterface) -> Option<(u32, u32)> {
    let address = address_to_u32(interface.address);
    let mask = address_to_u32(interface.netmask);
    let network = address & mask;
    let broadcast = network | !mask;
    if broadcast <= network.saturating_add(1) {
        return None;
    }
    let mut start = network + 1;
    let mut end = broadcast - 1;
    if start == address {
        start += 1;
    }
    if end == address {
        end -= 1;
    }
    (start <= end).then_some((start, end))
}

fn dhcp_broadcast(interface: &RouterInterface) -> Ipv4Address {
    let address = address_to_u32(interface.address);
    let mask = address_to_u32(interface.netmask);
    address_from_u32((address & mask) | !mask)
}

fn dhcp_reply(
    interface: &RouterInterface,
    server: &mut DhcpServer,
    dns_servers: &[Ipv4Address; DHCP_MAX_DNS_SERVERS],
    dns_server_count: usize,
    frame: &[u8],
    now: u32,
    nic_send_at: NicSendAt,
    max_frame_len: usize,
) -> bool {
    let Ok(ethernet) = EthernetFrame::parse(frame) else {
        return false;
    };
    if ethernet.ethertype != ETHERTYPE_IPV4 {
        return false;
    }
    let Ok(packet) = Ipv4Packet::parse(ethernet.payload, true) else {
        return false;
    };
    if packet.header.protocol != 17 {
        return false;
    }
    let Ok(datagram) = UdpDatagram::parse(
        packet.payload,
        false,
        packet.header.source,
        packet.header.destination,
    ) else {
        return false;
    };
    if datagram.source_port != DHCP_CLIENT_PORT || datagram.destination_port != DHCP_SERVER_PORT {
        return false;
    }
    let Ok(request) = DhcpPacket::parse(datagram.payload) else {
        return false;
    };
    if request.header.op != DhcpOp::Request
        || request.header.hardware_type != 1
        || request.header.hardware_length != 6
    {
        return true;
    }
    let Ok(message_type) = request.message_type() else {
        return true;
    };
    let client_mac = request.header.client_mac();
    let Some((pool_start, pool_end)) = dhcp_pool(interface) else {
        return true;
    };
    let options = DhcpOptions::new(request.options);
    let requested = options.ipv4(DHCP_OPTION_REQUESTED_IP).ok().flatten();
    let server_identifier = options.ipv4(DHCP_OPTION_SERVER_IDENTIFIER).ok().flatten();

    if message_type == DhcpMessageType::Release {
        server.release(client_mac, request.header.client_address);
        return true;
    }
    if message_type == DhcpMessageType::Decline {
        server.release(
            client_mac,
            requested.unwrap_or(request.header.client_address),
        );
        return true;
    }
    if server_identifier.is_some_and(|value| value != interface.address) {
        return true;
    }

    let (reply_type, address) = match message_type {
        DhcpMessageType::Discover => {
            let Some(index) = server.allocate(client_mac, now) else {
                return true;
            };
            (DhcpMessageType::Offer, server.address(index))
        }
        DhcpMessageType::Request => {
            match server.requested(
                client_mac,
                requested,
                request.header.client_address,
                pool_start,
                pool_end,
                now,
            ) {
                Some(index) => (DhcpMessageType::Ack, server.address(index)),
                None => (DhcpMessageType::Nak, Ipv4Address::UNSPECIFIED),
            }
        }
        _ => return true,
    };

    let mut header = request.header;
    header.op = DhcpOp::Reply;
    header.server_address = interface.address;
    header.your_address = address;
    header.client_address = if address == Ipv4Address::UNSPECIFIED {
        Ipv4Address::UNSPECIFIED
    } else {
        request.header.client_address
    };

    let broadcast = address == Ipv4Address::UNSPECIFIED
        || request.header.flags & 0x8000 != 0
        || request.header.client_address == Ipv4Address::UNSPECIFIED;
    let destination = if broadcast {
        BROADCAST_IP
    } else {
        request.header.client_address
    };
    let target_mac = if broadcast {
        MacAddress::BROADCAST
    } else {
        client_mac
    };
    let mut option_bytes = [0u8; 128];
    let mut writer = DhcpOptionWriter::new(&mut option_bytes);
    if writer
        .push_u8(DHCP_OPTION_MESSAGE_TYPE, reply_type.byte())
        .is_err()
        || writer
            .push_ipv4(DHCP_OPTION_SERVER_IDENTIFIER, interface.address)
            .is_err()
    {
        return true;
    }
    if reply_type != DhcpMessageType::Nak
        && (writer
            .push_ipv4(DHCP_OPTION_SUBNET_MASK, interface.netmask)
            .is_err()
            || writer
                .push_ipv4(DHCP_OPTION_ROUTER, interface.address)
                .is_err()
            || writer
                .push_ipv4(DHCP_OPTION_BROADCAST_ADDRESS, dhcp_broadcast(interface))
                .is_err()
            || writer
                .push_u32(DHCP_OPTION_LEASE_TIME, DHCP_LEASE_SECONDS)
                .is_err()
            || writer
                .push_u32(DHCP_OPTION_RENEWAL_TIME, DHCP_RENEWAL_SECONDS)
                .is_err()
            || writer
                .push_u32(DHCP_OPTION_REBINDING_TIME, DHCP_REBINDING_SECONDS)
                .is_err())
    {
        return true;
    }
    if reply_type != DhcpMessageType::Nak && dns_server_count != 0 {
        let mut dns_bytes = [0u8; DHCP_MAX_DNS_SERVERS * 4];
        for (index, server) in dns_servers.iter().enumerate().take(dns_server_count) {
            dns_bytes[index * 4..index * 4 + 4].copy_from_slice(&server.0);
        }
        if writer
            .push(DHCP_OPTION_DNS, &dns_bytes[..dns_server_count * 4])
            .is_err()
        {
            return true;
        }
    }
    let Ok(option_length) = writer.finish() else {
        return true;
    };
    let dhcp = DhcpPacket::new(header, &option_bytes[..option_length]);
    let mut payload = [0u8; 600];
    let Ok(payload_length) = dhcp.encode(&mut payload) else {
        return true;
    };
    let _ = send_dhcp_payload(
        nic_send_at,
        interface.nic,
        interface.mac,
        &payload[..payload_length],
        target_mac,
        interface.address,
        destination,
        DHCP_SERVER_PORT,
        DHCP_CLIENT_PORT,
        max_frame_len,
    );
    true
}

#[allow(clippy::too_many_arguments)]
fn configure_dhcp(
    putc: Putc,
    should_abort: ShouldAbort,
    nic_send_at: NicSendAt,
    nic_receive_at: NicReceiveAt,
    ticks: Ticks,
    own_mac: MacAddress,
    receive_buffer: &mut [u8; MAX_FRAME_LEN],
    max_frame_len: usize,
) -> Option<DhcpLease> {
    print_bytes(
        putc,
        b"DHCP: configuring NIC 1; broadcasting DISCOVER and waiting for a lease...\n",
    );
    let transaction_id = (ticks() as u32)
        ^ u32::from_be_bytes([own_mac.0[0], own_mac.0[1], own_mac.0[2], own_mac.0[3]])
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
        if should_abort() == TAPE_EVENT_ABORT {
            print_bytes(
                putc,
                b"DHCP stopped by operator before a lease was acquired.\n",
            );
            return None;
        }
        let now = (ticks() / 100) as u32;
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
                    nic_send_at,
                    0,
                    own_mac,
                    &payload[..length],
                    MacAddress::BROADCAST,
                    transmit.source,
                    transmit.destination,
                    DHCP_CLIENT_PORT,
                    DHCP_SERVER_PORT,
                    max_frame_len,
                ) {
                    print_bytes(putc, b"DHCP failed: NIC 1 rejected the request frame.\n");
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
            let Some(lease) = client.lease() else {
                print_bytes(putc, b"DHCP failed: client reported a missing lease.\n");
                return None;
            };
            if lease.subnet_mask == Ipv4Address::UNSPECIFIED {
                print_bytes(putc, b"DHCP failed: server supplied no subnet mask.\n");
                return None;
            }
            print_bytes(putc, b"DHCP lease acquired on NIC 1: ");
            print_ip(putc, lease.address);
            print_bytes(putc, b"/");
            print_ip(putc, lease.subnet_mask);
            if lease.router != Ipv4Address::UNSPECIFIED {
                print_bytes(putc, b", router ");
                print_ip(putc, lease.router);
            } else {
                print_bytes(putc, b", no router option");
            }
            print_bytes(putc, b".\n");
            return Some(lease);
        }
        if client.state() == DhcpClientState::Failed {
            print_bytes(
                putc,
                b"DHCP failed: no usable offer/acknowledgement arrived.\n",
            );
            return None;
        }

        let length =
            nic_receive_at(0, receive_buffer.as_mut_ptr() as u32, max_frame_len as u32) as usize;
        if length != 0 {
            receive_dhcp(
                &receive_buffer[..min(length, MAX_FRAME_LEN)],
                &mut client,
                now,
            );
        }
    }
}

fn process_arp(
    interface: &mut RouterInterface,
    frame: &[u8],
    now: u64,
    nic_send_at: NicSendAt,
    max_frame_len: usize,
) {
    let Ok(ethernet) = EthernetFrame::parse(frame) else {
        return;
    };
    if ethernet.ethertype != ETHERTYPE_ARP {
        return;
    }
    let Ok(packet) = ArpPacket::parse(ethernet.payload) else {
        return;
    };
    interface
        .arp
        .insert(packet.sender_protocol, packet.sender_hardware, now as u32);
    if packet.operation == ArpOperation::Request
        && packet.target_protocol == interface.address
        && packet.sender_hardware != interface.mac
    {
        let _ = send_arp_reply(
            interface,
            packet.sender_hardware,
            packet.sender_protocol,
            nic_send_at,
            max_frame_len,
        );
    }
}

fn resolve_mac(
    egress: usize,
    target: Ipv4Address,
    interfaces: &mut [RouterInterface; MAX_INTERFACES],
    count: usize,
    nic_send_at: NicSendAt,
    nic_receive_at: NicReceiveAt,
    should_abort: ShouldAbort,
    ticks: Ticks,
    receive_buffer: &mut [u8; MAX_FRAME_LEN],
    max_frame_len: usize,
) -> Option<MacAddress> {
    let now = ticks();
    if let Some(mac) = interfaces[egress].arp.lookup(target, now as u32) {
        return Some(mac);
    }
    if !send_arp_request(&interfaces[egress], target, nic_send_at, max_frame_len) {
        return None;
    }
    let deadline = now.wrapping_add(ARP_TIMEOUT_TICKS);
    loop {
        if should_abort() == TAPE_EVENT_ABORT {
            return None;
        }
        for index in 0..count {
            let length = nic_receive_at(
                interfaces[index].nic,
                receive_buffer.as_mut_ptr() as u32,
                max_frame_len as u32,
            ) as usize;
            if length == 0 {
                continue;
            }
            let length = min(length, MAX_FRAME_LEN);
            process_arp(
                &mut interfaces[index],
                &receive_buffer[..length],
                ticks(),
                nic_send_at,
                max_frame_len,
            );
        }
        if let Some(mac) = interfaces[egress].arp.lookup(target, ticks() as u32) {
            return Some(mac);
        }
        if ticks().wrapping_sub(deadline) < (1u64 << 63) {
            return None;
        }
    }
}

fn is_broadcast_or_multicast(address: Ipv4Address) -> bool {
    address == BROADCAST_IP || address.0[0] >= 224
}

fn is_local(
    interfaces: &[RouterInterface; MAX_INTERFACES],
    count: usize,
    address: Ipv4Address,
) -> bool {
    interfaces[..count]
        .iter()
        .any(|interface| interface.address == address)
}

fn forward_ipv4(
    ingress: usize,
    frame: &[u8],
    interfaces: &mut [RouterInterface; MAX_INTERFACES],
    configs: &[Option<InterfaceConfig>; MAX_INTERFACES],
    count: usize,
    default_route: Option<router_tape::DefaultRoute>,
    nic_send_at: NicSendAt,
    nic_receive_at: NicReceiveAt,
    should_abort: ShouldAbort,
    ticks: Ticks,
    receive_buffer: &mut [u8; MAX_FRAME_LEN],
    ip_buffer: &mut [u8; MAX_FRAME_LEN],
    output_buffer: &mut [u8; MAX_FRAME_LEN],
    max_frame_len: usize,
) -> ForwardResult {
    let Ok(ethernet) = EthernetFrame::parse(frame) else {
        return ForwardResult::Dropped;
    };
    if ethernet.ethertype != ETHERTYPE_IPV4 {
        return ForwardResult::Dropped;
    }
    let Ok(packet) = Ipv4Packet::parse(ethernet.payload, true) else {
        return ForwardResult::Dropped;
    };
    interfaces[ingress]
        .arp
        .insert(packet.header.source, ethernet.source, ticks() as u32);
    if is_local(interfaces, count, packet.header.destination)
        || is_broadcast_or_multicast(packet.header.destination)
    {
        return ForwardResult::Dropped;
    }
    let Some((egress, next_hop)) =
        route_next_hop(configs, count, packet.header.destination, default_route)
    else {
        return ForwardResult::Dropped;
    };
    if egress == ingress || packet.header.ttl <= 1 {
        return ForwardResult::Dropped;
    }
    let Some(destination_mac) = resolve_mac(
        egress,
        next_hop,
        interfaces,
        count,
        nic_send_at,
        nic_receive_at,
        should_abort,
        ticks,
        receive_buffer,
        max_frame_len,
    ) else {
        return ForwardResult::Dropped;
    };
    let mut header = packet.header;
    header.ttl -= 1;
    let forwarded = Ipv4Packet {
        header,
        payload: packet.payload,
    };
    let Ok(ip_length) = forwarded.encode(ip_buffer) else {
        return ForwardResult::Dropped;
    };
    let ethernet = EthernetFrame::new(
        destination_mac,
        interfaces[egress].mac,
        ETHERTYPE_IPV4,
        &ip_buffer[..ip_length],
    );
    let Ok(length) = ethernet.encode(output_buffer) else {
        return ForwardResult::Dropped;
    };
    if length > max_frame_len {
        return ForwardResult::Dropped;
    }
    if nic_send_at(
        interfaces[egress].nic,
        output_buffer.as_ptr() as u32,
        length as u32,
    ) == 0
    {
        ForwardResult::Dropped
    } else {
        ForwardResult::Forwarded
    }
}

enum ForwardResult {
    Forwarded,
    Dropped,
}

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(
    putc: Putc,
    should_abort: ShouldAbort,
    alloc: Alloc,
    free: Free,
    _read_key: extern "C" fn() -> u32,
    args_ptr: u32,
    args_len: u32,
    _nic_mac: extern "C" fn(u32) -> u32,
    _nic_send: extern "C" fn(u32, u32) -> u32,
    _nic_receive: extern "C" fn(u32, u32) -> u32,
    nic_max_frame_len: extern "C" fn() -> u32,
    ticks: Ticks,
    nic_count: NicCount,
    nic_mac_at: NicMacAt,
    nic_send_at: NicSendAt,
    nic_receive_at: NicReceiveAt,
    nic_set_promiscuous_at: NicSetPromiscuousAt,
) {
    let nic_total = nic_count() as usize;
    let arguments = if args_len == 0 {
        &[][..]
    } else {
        unsafe { core::slice::from_raw_parts(args_ptr as *const u8, args_len as usize) }
    };
    let parsed: RouterConfig = match parse_config(arguments) {
        Ok(value) => value,
        Err(error) => {
            print_config_error(putc, error);
            return;
        }
    };
    if parsed.interfaces[..parsed.interface_count]
        .iter()
        .flatten()
        .any(|config| config.nic as usize >= nic_total)
    {
        print_bytes(
            putc,
            b"Router configuration error: NIC number is not present.\n",
        );
        return;
    }
    if parsed.dhcp && nic_total == 0 {
        print_bytes(
            putc,
            b"Router configuration error: DHCP NIC 1 is not present.\n",
        );
        return;
    }
    let max_frame_len = min(nic_max_frame_len() as usize, MAX_FRAME_LEN);
    if max_frame_len < 60 {
        print_bytes(
            putc,
            b"Router cannot start: NIC frame limit is too small.\n",
        );
        return;
    }

    let mut interfaces = [const { RouterInterface::empty() }; MAX_INTERFACES];
    let mut dhcp_servers: [Option<DhcpServer<'_>>; MAX_INTERFACES] =
        core::array::from_fn(|_| None);
    let mut dhcp_allocations = [None; MAX_INTERFACES];
    let mut configs: [Option<InterfaceConfig>; MAX_INTERFACES] = [None; MAX_INTERFACES];
    let mut interface_count = 0;
    let mut default_route = parsed.default_route;
    let mut dns_servers = [Ipv4Address::UNSPECIFIED; DHCP_MAX_DNS_SERVERS];
    let mut dns_server_count = parsed.dns_server_count;
    for (index, server) in parsed.dns_servers.iter().flatten().enumerate() {
        dns_servers[index] = *server;
    }
    let mut receive_buffer = [0u8; MAX_FRAME_LEN];

    if parsed.dhcp {
        let mut mac = [0u8; 6];
        if nic_mac_at(0, mac.as_mut_ptr() as u32) == 0 {
            print_bytes(
                putc,
                b"Router cannot start: failed to read DHCP NIC 1 MAC.\n",
            );
            return;
        }
        let own_mac = MacAddress::new(mac);
        let Some(lease) = configure_dhcp(
            putc,
            should_abort,
            nic_send_at,
            nic_receive_at,
            ticks,
            own_mac,
            &mut receive_buffer,
            max_frame_len,
        ) else {
            return;
        };
        let dhcp_config = InterfaceConfig {
            nic: 0,
            address: lease.address,
            netmask: lease.subnet_mask,
        };
        interfaces[interface_count] = RouterInterface {
            nic: 0,
            address: lease.address,
            netmask: lease.subnet_mask,
            mac: own_mac,
            arp: ArpCache::new(),
        };
        configs[interface_count] = Some(dhcp_config);
        interface_count += 1;
        if default_route.is_none() && lease.router != Ipv4Address::UNSPECIFIED {
            default_route = Some(router_tape::DefaultRoute {
                nic: 0,
                gateway: lease.router,
            });
            print_bytes(putc, b"DHCP supplied the default router for NIC 1.\n");
        }
        if let Some(route) = default_route {
            if route.nic == 0
                && !router_tape::same_subnet(lease.address, lease.subnet_mask, route.gateway)
            {
                print_bytes(
                    putc,
                    b"Router configuration error: default gateway is outside the DHCP subnet.\n",
                );
                return;
            }
        }
        if dns_server_count == 0 {
            dns_server_count = lease.dns_server_count;
            dns_servers[..dns_server_count].copy_from_slice(&lease.dns_servers[..dns_server_count]);
            if dns_server_count != 0 {
                print_bytes(putc, b"DHCP supplied DNS server(s) for downstream clients.\n");
            }
        }
    }

    for config in parsed.interfaces[..parsed.interface_count].iter().flatten() {
        let mut mac = [0u8; 6];
        if nic_mac_at(config.nic, mac.as_mut_ptr() as u32) == 0 {
            print_bytes(
                putc,
                b"Router cannot start: failed to read a selected NIC MAC.\n",
            );
            return;
        }
        interfaces[interface_count] = RouterInterface {
            nic: config.nic,
            address: config.address,
            netmask: config.netmask,
            mac: MacAddress::new(mac),
            arp: ArpCache::new(),
        };
        configs[interface_count] = Some(*config);
        interface_count += 1;
    }
    if parsed.dhcp_server {
        let mut enabled = 0;
        let mut static_lease_offset = 0usize;
        for (index, interface) in interfaces.iter().take(interface_count).enumerate() {
            if interface.nic == 0 {
                continue;
            }
            if let Some((start, end)) = dhcp_pool(interface) {
                let slot_count_u64 = u64::from(end) - u64::from(start) + 1;
                let Ok(slot_count) = usize::try_from(slot_count_u64) else {
                    free_dhcp_allocations(&mut dhcp_allocations, free);
                    print_bytes(
                        putc,
                        b"Router cannot start: DHCP subnet is too large for this CPU.\n",
                    );
                    return;
                };
                let lease_slots = if static_lease_offset
                    .checked_add(slot_count)
                    .is_some_and(|end| end <= DHCP_STATIC_LEASE_SLOTS)
                {
                    let slots = unsafe {
                        core::slice::from_raw_parts_mut(
                            (core::ptr::addr_of_mut!(DHCP_STATIC_LEASES) as *mut DhcpLeaseSlot)
                                .add(static_lease_offset),
                            slot_count,
                        )
                    };
                    static_lease_offset += slot_count;
                    slots
                } else {
                    let Some(byte_count) =
                        slot_count.checked_mul(core::mem::size_of::<DhcpLeaseSlot>())
                    else {
                        free_dhcp_allocations(&mut dhcp_allocations, free);
                        print_bytes(
                            putc,
                            b"Router cannot start: DHCP lease table size overflowed.\n",
                        );
                        return;
                    };
                    let Ok(byte_count_u32) = u32::try_from(byte_count) else {
                        free_dhcp_allocations(&mut dhcp_allocations, free);
                        print_bytes(
                            putc,
                            b"Router cannot start: DHCP lease table needs too much memory.\n",
                        );
                        return;
                    };
                    let lease_address = alloc(byte_count_u32);
                    if lease_address == 0 {
                        free_dhcp_allocations(&mut dhcp_allocations, free);
                        print_bytes(
                            putc,
                            b"Router cannot start: insufficient memory for the full DHCP subnet.\n",
                        );
                        return;
                    }
                    dhcp_allocations[index] = Some((lease_address, byte_count_u32));
                    unsafe {
                        core::slice::from_raw_parts_mut(
                            lease_address as *mut DhcpLeaseSlot,
                            slot_count,
                        )
                    }
                };
                dhcp_servers[index] = Some(DhcpServer::new(lease_slots, start, end));
                enabled += 1;
                print_bytes(putc, b"DHCP server on NIC ");
                print_u32(putc, interface.nic + 1);
                print_bytes(putc, b": pool ");
                print_ip(putc, address_from_u32(start));
                print_bytes(putc, b"-");
                print_ip(putc, address_from_u32(end));
                print_bytes(putc, b", leases ");
                print_u32(putc, DHCP_LEASE_SECONDS);
                print_bytes(putc, b" seconds.\n");
            } else {
                print_bytes(putc, b"DHCP server skipped NIC ");
                print_u32(putc, interface.nic + 1);
                print_bytes(putc, b": subnet has no usable host addresses.\n");
            }
        }
        if enabled == 0 {
            print_bytes(
                putc,
                b"Router cannot start: dhcp-server has no usable non-primary interface.\n",
            );
            return;
        }
    }
    let _ = nic_set_promiscuous_at;
    print_startup(
        putc,
        &interfaces,
        interface_count,
        default_route.map(|route| (route.nic, route.gateway)),
    );
    let mut arp_receive_buffer = [0u8; MAX_FRAME_LEN];
    let mut ip_buffer = [0u8; MAX_FRAME_LEN];
    let mut output_buffer = [0u8; MAX_FRAME_LEN];
    let mut forwarded = 0u32;
    let mut dropped = 0u32;

    'router: loop {
        for ingress in 0..interface_count {
            if should_abort() == TAPE_EVENT_ABORT {
                break 'router;
            }
            let length = nic_receive_at(
                interfaces[ingress].nic,
                receive_buffer.as_mut_ptr() as u32,
                max_frame_len as u32,
            ) as usize;
            if length == 0 {
                continue;
            }
            let length = min(length, MAX_FRAME_LEN);
            if parsed.dhcp_server && interfaces[ingress].nic != 0 {
                if let Some(server) = dhcp_servers[ingress].as_mut() {
                    let _ = dhcp_reply(
                        &interfaces[ingress],
                        server,
                        &dns_servers,
                        dns_server_count,
                        &receive_buffer[..length],
                        (ticks() / 100) as u32,
                        nic_send_at,
                        max_frame_len,
                    );
                }
            }
            process_arp(
                &mut interfaces[ingress],
                &receive_buffer[..length],
                ticks(),
                nic_send_at,
                max_frame_len,
            );
            match forward_ipv4(
                ingress,
                &receive_buffer[..length],
                &mut interfaces,
                &configs,
                interface_count,
                default_route,
                nic_send_at,
                nic_receive_at,
                should_abort,
                ticks,
                &mut arp_receive_buffer,
                &mut ip_buffer,
                &mut output_buffer,
                max_frame_len,
            ) {
                ForwardResult::Forwarded => forwarded += 1,
                ForwardResult::Dropped => dropped += 1,
            }
        }
    }

    for server in &mut dhcp_servers {
        *server = None;
    }
    free_dhcp_allocations(&mut dhcp_allocations, free);
    print_bytes(putc, b"Router stopped; forwarded ");
    print_u32(putc, forwarded);
    print_bytes(putc, b" IPv4 frame(s), dropped ");
    print_u32(putc, dropped);
    print_bytes(putc, b".\n");
}
