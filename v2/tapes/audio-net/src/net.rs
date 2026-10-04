//! The IPv4 side of `audio-net`: addressing, ARP, UDP/RTP framing and a DHCP client. Built on the shared
//! `tape-net` protocol library (the same one `router`, `ip-chat` and `telnet` use).

use tape_net::{
    ArpCache, ArpOperation, ArpPacket, DHCP_CLIENT_PORT, DHCP_SERVER_PORT, DhcpClient, DhcpClientState, DhcpPacket, ETHERTYPE_ARP, ETHERTYPE_IPV4,
    EthernetFrame, Ipv4Address, Ipv4Header, Ipv4Packet, MacAddress, RtpPacket, UdpDatagram,
};
use tape_rt::TapeApi;

pub const MAX_FRAME: usize = 1514;

#[derive(Clone, Copy)]
pub struct NetConfig {
    pub mac: MacAddress,
    pub ip: Ipv4Address,
    pub netmask: Ipv4Address,
    pub gateway: Ipv4Address,
}

/// `192.168.1.10/24` as (address, netmask).
pub fn parse_cidr(text: &str) -> Option<(Ipv4Address, Ipv4Address)> {
    let (address, prefix) = text.split_once('/')?;
    let address = parse_ip(address)?;
    let prefix: u32 = prefix.parse().ok().filter(|&p| p <= 32)?;
    let mask = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix) };
    Some((address, Ipv4Address(mask.to_be_bytes())))
}

pub fn parse_ip(text: &str) -> Option<Ipv4Address> {
    let mut octets = [0u8; 4];
    let mut parts = text.split('.');
    for octet in octets.iter_mut() {
        *octet = parts.next()?.parse().ok()?;
    }
    parts.next().is_none().then_some(Ipv4Address(octets))
}

fn same_subnet(config: &NetConfig, other: Ipv4Address) -> bool {
    (0..4).all(|index| config.ip.0[index] & config.netmask.0[index] == other.0[index] & config.netmask.0[index])
}

pub fn send_frame(api: &TapeApi, nic: usize, frame: &[u8]) -> bool {
    (api.nic_send)(nic, frame.as_ptr(), frame.len()) != 0
}

/// Builds Ethernet + IPv4 + UDP + RTP around `payload` into `out`; returns the frame's length. The UDP
/// checksum is computed (a zero checksum is legal over IPv4 but some receivers insist).
#[allow(clippy::too_many_arguments)]
pub fn build_rtp_frame(out: &mut [u8], config: &NetConfig, destination_mac: MacAddress, destination_ip: Ipv4Address, port: u16, ttl: u8, rtp: &RtpPacket) -> Option<usize> {
    let mut rtp_bytes = [0u8; MAX_FRAME];
    let rtp_len = rtp.encode(&mut rtp_bytes).ok()?;
    let mut udp_bytes = [0u8; MAX_FRAME];
    let udp_len = UdpDatagram::new(port, port, &rtp_bytes[..rtp_len]).encode(&mut udp_bytes, config.ip, destination_ip).ok()?;
    let mut header = Ipv4Header::new(config.ip, destination_ip, 17, udp_len);
    header.ttl = ttl;
    let mut ip_bytes = [0u8; MAX_FRAME];
    let ip_len = Ipv4Packet { header, payload: &udp_bytes[..udp_len] }.encode(&mut ip_bytes).ok()?;
    EthernetFrame::new(destination_mac, config.mac, ETHERTYPE_IPV4, &ip_bytes[..ip_len]).encode(out).ok()
}

/// Builds Ethernet + IPv4 + UDP around `payload` into `out` (a datagram that is not RTP, such as a SAP announcement).
#[allow(clippy::too_many_arguments)]
pub fn build_udp_frame(out: &mut [u8], config: &NetConfig, destination_mac: MacAddress, destination_ip: Ipv4Address, source_port: u16, destination_port: u16, ttl: u8, payload: &[u8]) -> Option<usize> {
    let mut udp_bytes = [0u8; MAX_FRAME];
    let udp_len = UdpDatagram::new(source_port, destination_port, payload).encode(&mut udp_bytes, config.ip, destination_ip).ok()?;
    let mut header = Ipv4Header::new(config.ip, destination_ip, 17, udp_len);
    header.ttl = ttl;
    let mut ip_bytes = [0u8; MAX_FRAME];
    let ip_len = Ipv4Packet { header, payload: &udp_bytes[..udp_len] }.encode(&mut ip_bytes).ok()?;
    EthernetFrame::new(destination_mac, config.mac, ETHERTYPE_IPV4, &ip_bytes[..ip_len]).encode(out).ok()
}

/// What the network layer made of one received frame.
pub enum Inbound<'a> {
    /// Not for us, or handled here (an ARP exchange).
    Nothing,
    /// Why an IPv4/UDP frame for this tape was refused (for `verbose`).
    Rejected(&'static str),
    /// A UDP datagram addressed to `port`: its source address and the UDP payload.
    Udp { source: Ipv4Address, source_port: u16, destination_port: u16, payload: &'a [u8] },
}

/// Handles ARP (answers requests for our address, learns replies) and returns UDP datagrams for `port`.
pub fn inbound<'a>(api: &TapeApi, nic: usize, config: &NetConfig, arp: &mut ArpCache<8>, frame: &'a [u8], ports: core::ops::RangeInclusive<u16>, now: u32) -> Inbound<'a> {
    let Ok(ethernet) = EthernetFrame::parse(frame) else { return Inbound::Nothing };
    match ethernet.ethertype {
        ETHERTYPE_ARP => {
            let Ok(packet) = ArpPacket::parse(ethernet.payload) else { return Inbound::Nothing };
            arp.insert(packet.sender_protocol, packet.sender_hardware, now);
            if packet.operation == ArpOperation::Request && packet.target_protocol == config.ip && packet.sender_hardware != config.mac {
                let reply = ArpPacket::reply(config.mac, config.ip, packet.sender_hardware, packet.sender_protocol);
                let mut arp_bytes = [0u8; ArpPacket::LEN];
                if let Ok(length) = reply.encode(&mut arp_bytes) {
                    let mut out = [0u8; 64];
                    if let Ok(total) = EthernetFrame::new(packet.sender_hardware, config.mac, ETHERTYPE_ARP, &arp_bytes[..length]).encode(&mut out) {
                        send_frame(api, nic, &out[..total]);
                    }
                }
            }
            Inbound::Nothing
        }
        ETHERTYPE_IPV4 => {
            let packet = match Ipv4Packet::parse(ethernet.payload, true) {
                Ok(packet) => packet,
                Err(_) => return Inbound::Rejected("bad IPv4 header or checksum"),
            };
            if packet.header.protocol != 17 {
                return Inbound::Nothing;
            }
            let datagram = match UdpDatagram::parse(packet.payload, true, packet.header.source, packet.header.destination) {
                Ok(datagram) => datagram,
                Err(_) => return Inbound::Rejected("bad UDP length or checksum"),
            };
            if !ports.contains(&datagram.destination_port) {
                return Inbound::Nothing;
            }
            Inbound::Udp { source: packet.header.source, source_port: datagram.source_port, destination_port: datagram.destination_port, payload: datagram.payload }
        }
        _ => Inbound::Nothing,
    }
}

/// The Ethernet destination for `target`: a multicast group maps to its multicast MAC, an address on our
/// subnet is resolved itself, anything else goes through the gateway. Resolution sends an ARP request and
/// listens (answering other ARP traffic) for up to two seconds; frames of other kinds are dropped meanwhile,
/// so call it before streaming starts.
pub fn resolve(api: &TapeApi, nic: usize, config: &NetConfig, arp: &mut ArpCache<8>, target: Ipv4Address) -> Option<MacAddress> {
    if audio_wire::is_multicast_address(target.0) {
        return Some(MacAddress(audio_wire::multicast_mac(target.0)));
    }
    if target.0 == [255, 255, 255, 255] {
        return Some(MacAddress::BROADCAST);
    }
    let next_hop = if same_subnet(config, target) { target } else if config.gateway != Ipv4Address([0; 4]) { config.gateway } else { return None };
    let start = tape_rt::micros();
    if let Some(mac) = arp.lookup(next_hop, 0) {
        return Some(mac);
    }
    let mut frame = [0u8; MAX_FRAME];
    let mut last_request = 0u64;
    while tape_rt::micros() < start + 2_000_000 {
        if tape_rt::micros() >= last_request + 500_000 {
            last_request = tape_rt::micros();
            let request = ArpPacket::request(config.mac, config.ip, next_hop);
            let mut arp_bytes = [0u8; ArpPacket::LEN];
            if let Ok(length) = request.encode(&mut arp_bytes) {
                let mut out = [0u8; 64];
                if let Ok(total) = EthernetFrame::new(MacAddress::BROADCAST, config.mac, ETHERTYPE_ARP, &arp_bytes[..length]).encode(&mut out) {
                    send_frame(api, nic, &out[..total]);
                }
            }
        }
        let length = (api.nic_receive)(nic, frame.as_mut_ptr(), frame.len());
        if length == 0 {
            tape_rt::idle();
            continue;
        }
        let _ = inbound(api, nic, config, arp, &frame[..length], 0..=0, 0);
        if let Some(mac) = arp.lookup(next_hop, 0) {
            return Some(mac);
        }
    }
    None
}

/// DHCP (the same client `ip-chat` and `router` use): broadcasts DISCOVER, waits up to 8 s for a lease.
pub fn dhcp(api: &TapeApi, nic: usize, mac: MacAddress) -> Option<NetConfig> {
    let seconds = || (api.ticks)() as u32 / 100;
    let transaction = (tape_rt::micros() as u32) ^ u32::from_be_bytes([mac.0[2], mac.0[3], mac.0[4], mac.0[5]]);
    let mut client = DhcpClient::<128>::new(mac, transaction, 1, 5);
    let started = seconds();
    let mut payload = [0u8; 600];
    let mut frame = [0u8; MAX_FRAME];
    loop {
        let now = seconds();
        if now.wrapping_sub(started) > 8 || client.state() == DhcpClientState::Failed {
            return None;
        }
        if let Ok(Some(length)) = client.poll(now, &mut payload) {
            let transmit = client.transmit();
            let mut udp = [0u8; MAX_FRAME];
            let udp_len = UdpDatagram::new(DHCP_CLIENT_PORT, DHCP_SERVER_PORT, &payload[..length]).encode_with_checksum(&mut udp, transmit.source, transmit.destination, false).ok()?;
            let mut ip = [0u8; MAX_FRAME];
            let ip_len = Ipv4Packet { header: Ipv4Header::new(transmit.source, transmit.destination, 17, udp_len), payload: &udp[..udp_len] }.encode(&mut ip).ok()?;
            let total = EthernetFrame::new(MacAddress::BROADCAST, mac, ETHERTYPE_IPV4, &ip[..ip_len]).encode(&mut frame).ok()?;
            send_frame(api, nic, &frame[..total]);
        }
        if client.state() == DhcpClientState::Bound {
            let lease = client.lease()?;
            if lease.subnet_mask == Ipv4Address([0; 4]) {
                return None;
            }
            return Some(NetConfig { mac, ip: lease.address, netmask: lease.subnet_mask, gateway: lease.router });
        }
        let length = (api.nic_receive)(nic, frame.as_mut_ptr(), frame.len());
        if length == 0 {
            tape_rt::idle();
            continue;
        }
        if let Ok(ethernet) = EthernetFrame::parse(&frame[..length]) {
            if ethernet.ethertype != ETHERTYPE_IPV4 {
                continue;
            }
            let Ok(packet) = Ipv4Packet::parse(ethernet.payload, true) else { continue };
            if packet.header.protocol != 17 {
                continue;
            }
            let Ok(datagram) = UdpDatagram::parse(packet.payload, false, packet.header.source, packet.header.destination) else { continue };
            if datagram.source_port == DHCP_SERVER_PORT && datagram.destination_port == DHCP_CLIENT_PORT {
                if let Ok(dhcp) = DhcpPacket::parse(datagram.payload) {
                    let _ = client.receive(now, &dhcp);
                }
            }
        }
    }
}
