//! Whole Ethernet frames for ARP and DHCP, built and read without any I/O. The tapes that do their own ARP and DHCP
//! (`ip-chat`, `telnet`, `router`, and `tape-ip` for `audio-net`) differ only in how a frame leaves the machine
//! (a send callback, a per-NIC send, the tape API), so the frame itself lives here once.

use crate::{
    ArpPacket, DHCP_CLIENT_PORT, DHCP_SERVER_PORT, DhcpClient, DhcpPacket, ETHERTYPE_ARP, ETHERTYPE_IPV4, EthernetFrame, Ipv4Address, Ipv4Header,
    Ipv4Packet, MacAddress, UdpDatagram,
};

/// The largest frame these builders handle (a standard Ethernet frame without its checksum).
pub const MAX_FRAME_LEN: usize = 1514;

fn arp_frame(destination: MacAddress, own_mac: MacAddress, packet: &ArpPacket, out: &mut [u8]) -> Option<usize> {
    let mut arp = [0u8; ArpPacket::LEN];
    let length = packet.encode(&mut arp).ok()?;
    EthernetFrame::new(destination, own_mac, ETHERTYPE_ARP, &arp[..length]).encode(out).ok()
}

/// A broadcast "who has `target_ip`?" from `local_ip`. Returns the frame's length.
pub fn arp_request_frame(out: &mut [u8], own_mac: MacAddress, local_ip: Ipv4Address, target_ip: Ipv4Address) -> Option<usize> {
    arp_frame(MacAddress::BROADCAST, own_mac, &ArpPacket::request(own_mac, local_ip, target_ip), out)
}

/// The answer "`local_ip` is at `own_mac`", sent straight to the asker.
pub fn arp_reply_frame(out: &mut [u8], own_mac: MacAddress, local_ip: Ipv4Address, target_mac: MacAddress, target_ip: Ipv4Address) -> Option<usize> {
    arp_frame(target_mac, own_mac, &ArpPacket::reply(own_mac, local_ip, target_mac, target_ip), out)
}

/// A DHCP message (`payload`) as Ethernet + IPv4 + UDP from `source_port` to `destination_port`. The UDP checksum is
/// left at zero, which is what a client without an address of its own may do and what every DHCP server accepts.
pub fn dhcp_frame(
    out: &mut [u8],
    own_mac: MacAddress,
    target_mac: MacAddress,
    source: Ipv4Address,
    destination: Ipv4Address,
    source_port: u16,
    destination_port: u16,
    payload: &[u8],
) -> Option<usize> {
    let mut udp = [0u8; MAX_FRAME_LEN];
    let udp_len = UdpDatagram::new(source_port, destination_port, payload).encode_with_checksum(&mut udp, source, destination, false).ok()?;
    let mut ip = [0u8; MAX_FRAME_LEN];
    let ip_len = Ipv4Packet { header: Ipv4Header::new(source, destination, 17, udp_len), payload: &udp[..udp_len] }.encode(&mut ip).ok()?;
    EthernetFrame::new(target_mac, own_mac, ETHERTYPE_IPV4, &ip[..ip_len]).encode(out).ok()
}

/// A client's DISCOVER/REQUEST: broadcast from port 68 to port 67.
pub fn dhcp_client_frame(out: &mut [u8], own_mac: MacAddress, source: Ipv4Address, destination: Ipv4Address, payload: &[u8]) -> Option<usize> {
    dhcp_frame(out, own_mac, MacAddress::BROADCAST, source, destination, DHCP_CLIENT_PORT, DHCP_SERVER_PORT, payload)
}

/// Hands `frame` to `client` if it is a server's DHCP reply (UDP 67 -> 68); anything else is ignored.
pub fn feed_dhcp<const N: usize>(frame: &[u8], client: &mut DhcpClient<N>, now: u32) {
    let Ok(ethernet) = EthernetFrame::parse(frame) else { return };
    if ethernet.ethertype != ETHERTYPE_IPV4 {
        return;
    }
    let Ok(packet) = Ipv4Packet::parse(ethernet.payload, true) else { return };
    if packet.header.protocol != 17 {
        return;
    }
    let Ok(datagram) = UdpDatagram::parse(packet.payload, false, packet.header.source, packet.header.destination) else { return };
    if datagram.source_port != DHCP_SERVER_PORT || datagram.destination_port != DHCP_CLIENT_PORT {
        return;
    }
    if let Ok(dhcp) = DhcpPacket::parse(datagram.payload) {
        let _ = client.receive(now, &dhcp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAC: MacAddress = MacAddress([2, 0, 0, 0, 0, 1]);
    const PEER: MacAddress = MacAddress([2, 0, 0, 0, 0, 2]);

    #[test]
    fn an_arp_request_is_a_broadcast_and_parses_back() {
        let mut frame = [0u8; MAX_FRAME_LEN];
        let length = arp_request_frame(&mut frame, MAC, Ipv4Address([10, 0, 0, 1]), Ipv4Address([10, 0, 0, 2])).unwrap();
        let ethernet = EthernetFrame::parse(&frame[..length]).unwrap();
        assert_eq!(ethernet.destination, MacAddress::BROADCAST);
        assert_eq!(ethernet.ethertype, ETHERTYPE_ARP);
        let arp = ArpPacket::parse(ethernet.payload).unwrap();
        assert_eq!(arp.sender_protocol, Ipv4Address([10, 0, 0, 1]));
        assert_eq!(arp.target_protocol, Ipv4Address([10, 0, 0, 2]));
    }

    #[test]
    fn an_arp_reply_goes_to_the_asker() {
        let mut frame = [0u8; MAX_FRAME_LEN];
        let length = arp_reply_frame(&mut frame, MAC, Ipv4Address([10, 0, 0, 1]), PEER, Ipv4Address([10, 0, 0, 2])).unwrap();
        let ethernet = EthernetFrame::parse(&frame[..length]).unwrap();
        assert_eq!(ethernet.destination, PEER);
        assert_eq!(ethernet.source, MAC);
    }

    #[test]
    fn a_dhcp_client_frame_is_broadcast_udp_68_to_67_with_a_zero_checksum() {
        let mut frame = [0u8; MAX_FRAME_LEN];
        let length = dhcp_client_frame(&mut frame, MAC, Ipv4Address([0; 4]), Ipv4Address([255; 4]), &[1, 2, 3, 4]).unwrap();
        let ethernet = EthernetFrame::parse(&frame[..length]).unwrap();
        assert_eq!(ethernet.destination, MacAddress::BROADCAST);
        let packet = Ipv4Packet::parse(ethernet.payload, true).unwrap();
        assert_eq!(packet.header.protocol, 17);
        let datagram = UdpDatagram::parse(packet.payload, false, packet.header.source, packet.header.destination).unwrap();
        assert_eq!((datagram.source_port, datagram.destination_port), (DHCP_CLIENT_PORT, DHCP_SERVER_PORT));
        assert_eq!(datagram.payload, &[1, 2, 3, 4]);
        assert_eq!(&packet.payload[6..8], &[0, 0], "the UDP checksum is left at zero");
    }
}
