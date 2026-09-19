//! Fixed-buffer network protocol helpers for 32-bit fos tapes.
//!
//! The crate deliberately contains no heap allocation, operating-system
//! calls, or background work.  Every parser borrows its input and every
//! builder writes into a caller-supplied buffer.  This makes the same code
//! usable by a small `no_std` tape and by host-side unit tests.
//!
//! The supported wire formats are Ethernet II, ARP for IPv4/Ethernet,
//! IPv4 without options, ICMP echo, UDP, DHCPv4, and a deliberately bounded
//! TCP connection.  DHCP includes a BOOTP header/options codec and a client
//! state machine for discovery, lease renewal, and rebinding.  The TCP
//! implementation is not a general-purpose stack: it is a teaching state
//! machine for one connection, with one fixed receive buffer and one
//! outstanding segment for retransmission.
#![no_std]

use core::convert::TryInto;

/// Errors returned by fixed-buffer parsers and builders.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// The caller's output buffer is too short.
    BufferTooSmall,
    /// The input ended before the fixed header or declared payload did.
    Truncated,
    /// A field has a value this small implementation does not support.
    Invalid,
    /// A packet checksum did not validate.
    BadChecksum,
    /// A packet is valid in isolation but cannot be accepted by the state
    /// machine in its current state.
    Unexpected,
}

pub type Result<T> = core::result::Result<T, Error>;

/// A six-octet Ethernet hardware address.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MacAddress(pub [u8; 6]);

impl MacAddress {
    pub const BROADCAST: Self = Self([0xff; 6]);
    pub const ZERO: Self = Self([0; 6]);

    pub const fn new(bytes: [u8; 6]) -> Self {
        Self(bytes)
    }

    pub const fn octets(self) -> [u8; 6] {
        self.0
    }

    pub fn is_broadcast(self) -> bool {
        self == Self::BROADCAST
    }

    pub fn is_multicast(self) -> bool {
        self.0[0] & 1 != 0
    }
}

/// A four-octet IPv4 address.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Ipv4Address(pub [u8; 4]);

impl Ipv4Address {
    pub const UNSPECIFIED: Self = Self([0; 4]);

    pub const fn new(bytes: [u8; 4]) -> Self {
        Self(bytes)
    }

    pub const fn octets(self) -> [u8; 4] {
        self.0
    }
}

pub const ETHERTYPE_IPV4: u16 = 0x0800;
pub const ETHERTYPE_ARP: u16 = 0x0806;

/// A borrowed Ethernet II frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EthernetFrame<'a> {
    pub destination: MacAddress,
    pub source: MacAddress,
    pub ethertype: u16,
    pub payload: &'a [u8],
}

impl<'a> EthernetFrame<'a> {
    pub const HEADER_LEN: usize = 14;

    pub const fn new(
        destination: MacAddress,
        source: MacAddress,
        ethertype: u16,
        payload: &'a [u8],
    ) -> Self {
        Self {
            destination,
            source,
            ethertype,
            payload,
        }
    }

    pub fn parse(frame: &'a [u8]) -> Result<Self> {
        if frame.len() < Self::HEADER_LEN {
            return Err(Error::Truncated);
        }
        Ok(Self {
            destination: MacAddress(frame[0..6].try_into().unwrap()),
            source: MacAddress(frame[6..12].try_into().unwrap()),
            ethertype: u16::from_be_bytes([frame[12], frame[13]]),
            payload: &frame[Self::HEADER_LEN..],
        })
    }

    pub fn encode(&self, output: &mut [u8]) -> Result<usize> {
        let length = Self::HEADER_LEN + self.payload.len();
        if output.len() < length {
            return Err(Error::BufferTooSmall);
        }
        output[0..6].copy_from_slice(&self.destination.0);
        output[6..12].copy_from_slice(&self.source.0);
        output[12..14].copy_from_slice(&self.ethertype.to_be_bytes());
        output[14..length].copy_from_slice(self.payload);
        Ok(length)
    }
}

const ARP_HTYPE_ETHERNET: u16 = 1;
const ARP_PTYPE_IPV4: u16 = ETHERTYPE_IPV4;
const ARP_HLEN: u8 = 6;
const ARP_PLEN: u8 = 4;

/// ARP operation code.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArpOperation {
    Request,
    Reply,
}

/// An Ethernet/IPv4 ARP packet.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArpPacket {
    pub operation: ArpOperation,
    pub sender_hardware: MacAddress,
    pub sender_protocol: Ipv4Address,
    pub target_hardware: MacAddress,
    pub target_protocol: Ipv4Address,
}

impl ArpPacket {
    pub const LEN: usize = 28;

    pub const fn request(
        sender_hardware: MacAddress,
        sender_protocol: Ipv4Address,
        target_protocol: Ipv4Address,
    ) -> Self {
        Self {
            operation: ArpOperation::Request,
            sender_hardware,
            sender_protocol,
            target_hardware: MacAddress::ZERO,
            target_protocol,
        }
    }

    pub const fn reply(
        sender_hardware: MacAddress,
        sender_protocol: Ipv4Address,
        target_hardware: MacAddress,
        target_protocol: Ipv4Address,
    ) -> Self {
        Self {
            operation: ArpOperation::Reply,
            sender_hardware,
            sender_protocol,
            target_hardware,
            target_protocol,
        }
    }

    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < Self::LEN {
            return Err(Error::Truncated);
        }
        if u16::from_be_bytes([bytes[0], bytes[1]]) != ARP_HTYPE_ETHERNET
            || u16::from_be_bytes([bytes[2], bytes[3]]) != ARP_PTYPE_IPV4
            || bytes[4] != ARP_HLEN
            || bytes[5] != ARP_PLEN
        {
            return Err(Error::Invalid);
        }
        let operation = match u16::from_be_bytes([bytes[6], bytes[7]]) {
            1 => ArpOperation::Request,
            2 => ArpOperation::Reply,
            _ => return Err(Error::Invalid),
        };
        Ok(Self {
            operation,
            sender_hardware: MacAddress(bytes[8..14].try_into().unwrap()),
            sender_protocol: Ipv4Address(bytes[14..18].try_into().unwrap()),
            target_hardware: MacAddress(bytes[18..24].try_into().unwrap()),
            target_protocol: Ipv4Address(bytes[24..28].try_into().unwrap()),
        })
    }

    pub fn encode(&self, output: &mut [u8]) -> Result<usize> {
        if output.len() < Self::LEN {
            return Err(Error::BufferTooSmall);
        }
        output[0..2].copy_from_slice(&ARP_HTYPE_ETHERNET.to_be_bytes());
        output[2..4].copy_from_slice(&ARP_PTYPE_IPV4.to_be_bytes());
        output[4] = ARP_HLEN;
        output[5] = ARP_PLEN;
        output[6..8].copy_from_slice(
            &(match self.operation {
                ArpOperation::Request => 1u16,
                ArpOperation::Reply => 2u16,
            })
            .to_be_bytes(),
        );
        output[8..14].copy_from_slice(&self.sender_hardware.0);
        output[14..18].copy_from_slice(&self.sender_protocol.0);
        output[18..24].copy_from_slice(&self.target_hardware.0);
        output[24..28].copy_from_slice(&self.target_protocol.0);
        Ok(Self::LEN)
    }
}

#[derive(Clone, Copy)]
struct ArpCacheEntry {
    valid: bool,
    ip: Ipv4Address,
    mac: MacAddress,
    last_used: u32,
}

impl ArpCacheEntry {
    const EMPTY: Self = Self {
        valid: false,
        ip: Ipv4Address::UNSPECIFIED,
        mac: MacAddress::ZERO,
        last_used: 0,
    };
}

/// A tiny fixed-size ARP cache.
///
/// `tick` is supplied by the caller.  Insertion refreshes an existing entry,
/// fills an empty slot, or replaces the least-recently-used slot.
pub struct ArpCache<const N: usize> {
    entries: [ArpCacheEntry; N],
}

impl<const N: usize> ArpCache<N> {
    pub const fn new() -> Self {
        Self {
            entries: [ArpCacheEntry::EMPTY; N],
        }
    }

    pub fn len(&self) -> usize {
        self.entries.iter().filter(|entry| entry.valid).count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn clear(&mut self) {
        self.entries = [ArpCacheEntry::EMPTY; N];
    }

    pub fn lookup(&mut self, ip: Ipv4Address, tick: u32) -> Option<MacAddress> {
        for entry in &mut self.entries {
            if entry.valid && entry.ip == ip {
                entry.last_used = tick;
                return Some(entry.mac);
            }
        }
        None
    }

    pub fn insert(&mut self, ip: Ipv4Address, mac: MacAddress, tick: u32) {
        if N == 0 {
            return;
        }
        let mut slot = 0;
        let mut oldest = u32::MAX;
        for (index, entry) in self.entries.iter().enumerate() {
            if entry.valid && entry.ip == ip {
                slot = index;
                break;
            }
            if !entry.valid {
                slot = index;
                break;
            }
            if entry.last_used < oldest {
                oldest = entry.last_used;
                slot = index;
            }
        }
        self.entries[slot] = ArpCacheEntry {
            valid: true,
            ip,
            mac,
            last_used: tick,
        };
    }
}

impl<const N: usize> Default for ArpCache<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// The fixed IPv4 header supported by this crate.  Options are not emitted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Ipv4Header {
    pub dscp_ecn: u8,
    pub total_length: u16,
    pub identification: u16,
    pub flags_fragment: u16,
    pub ttl: u8,
    pub protocol: u8,
    pub checksum: u16,
    pub source: Ipv4Address,
    pub destination: Ipv4Address,
}

impl Ipv4Header {
    pub const LEN: usize = 20;

    pub const fn new(
        source: Ipv4Address,
        destination: Ipv4Address,
        protocol: u8,
        payload_len: usize,
    ) -> Self {
        Self {
            dscp_ecn: 0,
            total_length: (Self::LEN + payload_len) as u16,
            identification: 0,
            flags_fragment: 0,
            ttl: 64,
            protocol,
            checksum: 0,
            source,
            destination,
        }
    }

    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < Self::LEN {
            return Err(Error::Truncated);
        }
        if bytes[0] >> 4 != 4 || bytes[0] & 0x0f != 5 {
            return Err(Error::Invalid);
        }
        let total_length = u16::from_be_bytes([bytes[2], bytes[3]]);
        if total_length < Self::LEN as u16 {
            return Err(Error::Invalid);
        }
        Ok(Self {
            dscp_ecn: bytes[1],
            total_length,
            identification: u16::from_be_bytes([bytes[4], bytes[5]]),
            flags_fragment: u16::from_be_bytes([bytes[6], bytes[7]]),
            ttl: bytes[8],
            protocol: bytes[9],
            checksum: u16::from_be_bytes([bytes[10], bytes[11]]),
            source: Ipv4Address(bytes[12..16].try_into().unwrap()),
            destination: Ipv4Address(bytes[16..20].try_into().unwrap()),
        })
    }

    pub fn encode(&self, output: &mut [u8]) -> Result<usize> {
        if output.len() < Self::LEN || self.total_length < Self::LEN as u16 {
            return Err(Error::BufferTooSmall);
        }
        output[..Self::LEN].fill(0);
        output[0] = 0x45;
        output[1] = self.dscp_ecn;
        output[2..4].copy_from_slice(&self.total_length.to_be_bytes());
        output[4..6].copy_from_slice(&self.identification.to_be_bytes());
        output[6..8].copy_from_slice(&self.flags_fragment.to_be_bytes());
        output[8] = self.ttl;
        output[9] = self.protocol;
        output[12..16].copy_from_slice(&self.source.0);
        output[16..20].copy_from_slice(&self.destination.0);
        let checksum = internet_checksum(&output[..Self::LEN]);
        output[10..12].copy_from_slice(&checksum.to_be_bytes());
        Ok(Self::LEN)
    }
}

/// A borrowed IPv4 packet with a fixed, twenty-byte header.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Ipv4Packet<'a> {
    pub header: Ipv4Header,
    pub payload: &'a [u8],
}

impl<'a> Ipv4Packet<'a> {
    pub fn parse(bytes: &'a [u8], verify_checksum: bool) -> Result<Self> {
        let header = Ipv4Header::parse(bytes)?;
        let total = header.total_length as usize;
        if bytes.len() < total {
            return Err(Error::Truncated);
        }
        if verify_checksum && internet_checksum(&bytes[..Ipv4Header::LEN]) != 0 {
            return Err(Error::BadChecksum);
        }
        Ok(Self {
            header,
            payload: &bytes[Ipv4Header::LEN..total],
        })
    }

    pub fn encode(&self, output: &mut [u8]) -> Result<usize> {
        let total = Ipv4Header::LEN + self.payload.len();
        if output.len() < total || total > u16::MAX as usize {
            return Err(Error::BufferTooSmall);
        }
        let mut header = self.header;
        header.total_length = total as u16;
        header.encode(output)?;
        output[Ipv4Header::LEN..total].copy_from_slice(self.payload);
        Ok(total)
    }
}

/// Internet checksum in network byte order.
pub fn internet_checksum(bytes: &[u8]) -> u16 {
    let mut sum = 0u32;
    let mut index = 0;
    while index + 1 < bytes.len() {
        sum += u16::from_be_bytes([bytes[index], bytes[index + 1]]) as u32;
        index += 2;
    }
    if index < bytes.len() {
        sum += u32::from(bytes[index]) << 8;
    }
    while (sum >> 16) != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

fn pseudo_checksum(
    source: Ipv4Address,
    destination: Ipv4Address,
    protocol: u8,
    body: &[u8],
) -> u16 {
    let mut sum = 0u32;
    sum += u16::from_be_bytes([source.0[0], source.0[1]]) as u32;
    sum += u16::from_be_bytes([source.0[2], source.0[3]]) as u32;
    sum += u16::from_be_bytes([destination.0[0], destination.0[1]]) as u32;
    sum += u16::from_be_bytes([destination.0[2], destination.0[3]]) as u32;
    sum += u32::from(protocol);
    sum += body.len() as u32;
    let mut index = 0;
    while index + 1 < body.len() {
        sum += u16::from_be_bytes([body[index], body[index + 1]]) as u32;
        index += 2;
    }
    if index < body.len() {
        sum += u32::from(body[index]) << 8;
    }
    while (sum >> 16) != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

/// An ICMP echo packet (request or reply).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IcmpEcho<'a> {
    pub message_type: u8,
    pub code: u8,
    pub identifier: u16,
    pub sequence: u16,
    pub payload: &'a [u8],
}

impl<'a> IcmpEcho<'a> {
    pub const HEADER_LEN: usize = 8;
    pub const REQUEST: u8 = 8;
    pub const REPLY: u8 = 0;

    pub const fn request(identifier: u16, sequence: u16, payload: &'a [u8]) -> Self {
        Self {
            message_type: Self::REQUEST,
            code: 0,
            identifier,
            sequence,
            payload,
        }
    }

    pub const fn reply(identifier: u16, sequence: u16, payload: &'a [u8]) -> Self {
        Self {
            message_type: Self::REPLY,
            code: 0,
            identifier,
            sequence,
            payload,
        }
    }

    pub fn parse(bytes: &'a [u8], verify_checksum: bool) -> Result<Self> {
        if bytes.len() < Self::HEADER_LEN {
            return Err(Error::Truncated);
        }
        if verify_checksum && internet_checksum(bytes) != 0 {
            return Err(Error::BadChecksum);
        }
        if bytes[1] != 0 || (bytes[0] != Self::REQUEST && bytes[0] != Self::REPLY) {
            return Err(Error::Invalid);
        }
        Ok(Self {
            message_type: bytes[0],
            code: bytes[1],
            identifier: u16::from_be_bytes([bytes[4], bytes[5]]),
            sequence: u16::from_be_bytes([bytes[6], bytes[7]]),
            payload: &bytes[Self::HEADER_LEN..],
        })
    }

    pub fn encode(&self, output: &mut [u8]) -> Result<usize> {
        let length = Self::HEADER_LEN + self.payload.len();
        if output.len() < length {
            return Err(Error::BufferTooSmall);
        }
        output[..length].fill(0);
        output[0] = self.message_type;
        output[1] = self.code;
        output[4..6].copy_from_slice(&self.identifier.to_be_bytes());
        output[6..8].copy_from_slice(&self.sequence.to_be_bytes());
        output[8..length].copy_from_slice(self.payload);
        let checksum = internet_checksum(&output[..length]);
        output[2..4].copy_from_slice(&checksum.to_be_bytes());
        Ok(length)
    }
}

/// A borrowed UDP datagram.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UdpDatagram<'a> {
    pub source_port: u16,
    pub destination_port: u16,
    pub checksum: u16,
    pub payload: &'a [u8],
}

impl<'a> UdpDatagram<'a> {
    pub const HEADER_LEN: usize = 8;

    pub const fn new(source_port: u16, destination_port: u16, payload: &'a [u8]) -> Self {
        Self {
            source_port,
            destination_port,
            checksum: 0,
            payload,
        }
    }

    pub fn parse(
        bytes: &'a [u8],
        verify_checksum: bool,
        source: Ipv4Address,
        destination: Ipv4Address,
    ) -> Result<Self> {
        if bytes.len() < Self::HEADER_LEN {
            return Err(Error::Truncated);
        }
        let length = u16::from_be_bytes([bytes[4], bytes[5]]) as usize;
        if length < Self::HEADER_LEN || bytes.len() < length {
            return Err(Error::Truncated);
        }
        let checksum = u16::from_be_bytes([bytes[6], bytes[7]]);
        if verify_checksum
            && checksum != 0
            && pseudo_checksum(source, destination, 17, &bytes[..length]) != 0
        {
            return Err(Error::BadChecksum);
        }
        Ok(Self {
            source_port: u16::from_be_bytes([bytes[0], bytes[1]]),
            destination_port: u16::from_be_bytes([bytes[2], bytes[3]]),
            checksum,
            payload: &bytes[Self::HEADER_LEN..length],
        })
    }

    pub fn encode(
        &self,
        output: &mut [u8],
        source: Ipv4Address,
        destination: Ipv4Address,
    ) -> Result<usize> {
        self.encode_with_checksum(output, source, destination, true)
    }

    /// Encode the datagram, optionally leaving the UDP checksum zero.
    ///
    /// IPv4 permits a zero UDP checksum.  DHCP bootstrap traffic commonly
    /// uses that form while the client has no configured address yet.
    pub fn encode_with_checksum(
        &self,
        output: &mut [u8],
        source: Ipv4Address,
        destination: Ipv4Address,
        checksum_enabled: bool,
    ) -> Result<usize> {
        let length = Self::HEADER_LEN + self.payload.len();
        if length > u16::MAX as usize {
            return Err(Error::BufferTooSmall);
        }
        if output.len() < length {
            return Err(Error::BufferTooSmall);
        }
        output[..length].fill(0);
        output[0..2].copy_from_slice(&self.source_port.to_be_bytes());
        output[2..4].copy_from_slice(&self.destination_port.to_be_bytes());
        output[4..6].copy_from_slice(&(length as u16).to_be_bytes());
        output[8..length].copy_from_slice(self.payload);
        if !checksum_enabled {
            return Ok(length);
        }
        let mut checksum = pseudo_checksum(source, destination, 17, &output[..length]);
        if checksum == 0 {
            checksum = 0xffff;
        }
        output[6..8].copy_from_slice(&checksum.to_be_bytes());
        Ok(length)
    }
}

pub const DHCP_CLIENT_PORT: u16 = 68;
pub const DHCP_SERVER_PORT: u16 = 67;
pub const DHCP_MAGIC_COOKIE: u32 = 0x6382_5363;
pub const DHCP_BOOTSTRAP_SOURCE: Ipv4Address = Ipv4Address([0, 0, 0, 0]);
pub const DHCP_BOOTSTRAP_DESTINATION: Ipv4Address = Ipv4Address([255, 255, 255, 255]);
pub const DHCP_OPTION_PAD: u8 = 0;
pub const DHCP_OPTION_SUBNET_MASK: u8 = 1;
pub const DHCP_OPTION_ROUTER: u8 = 3;
pub const DHCP_OPTION_DNS: u8 = 6;
pub const DHCP_OPTION_BROADCAST_ADDRESS: u8 = 28;
pub const DHCP_OPTION_REQUESTED_IP: u8 = 50;
pub const DHCP_OPTION_LEASE_TIME: u8 = 51;
pub const DHCP_OPTION_MESSAGE_TYPE: u8 = 53;
pub const DHCP_OPTION_SERVER_IDENTIFIER: u8 = 54;
pub const DHCP_OPTION_PARAMETER_REQUEST_LIST: u8 = 55;
pub const DHCP_OPTION_RENEWAL_TIME: u8 = 58;
pub const DHCP_OPTION_REBINDING_TIME: u8 = 59;
pub const DHCP_OPTION_MAX_MESSAGE_SIZE: u8 = 57;
pub const DHCP_OPTION_END: u8 = 255;

/// BOOTP request/reply direction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DhcpOp {
    Request,
    Reply,
}

impl DhcpOp {
    fn from_byte(value: u8) -> Result<Self> {
        match value {
            1 => Ok(Self::Request),
            2 => Ok(Self::Reply),
            _ => Err(Error::Invalid),
        }
    }

    fn byte(self) -> u8 {
        match self {
            Self::Request => 1,
            Self::Reply => 2,
        }
    }
}

/// DHCP message type carried in option 53.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DhcpMessageType {
    Discover,
    Offer,
    Request,
    Decline,
    Ack,
    Nak,
    Release,
    Inform,
}

impl DhcpMessageType {
    pub fn from_byte(value: u8) -> Result<Self> {
        match value {
            1 => Ok(Self::Discover),
            2 => Ok(Self::Offer),
            3 => Ok(Self::Request),
            4 => Ok(Self::Decline),
            5 => Ok(Self::Ack),
            6 => Ok(Self::Nak),
            7 => Ok(Self::Release),
            8 => Ok(Self::Inform),
            _ => Err(Error::Invalid),
        }
    }

    pub const fn byte(self) -> u8 {
        match self {
            Self::Discover => 1,
            Self::Offer => 2,
            Self::Request => 3,
            Self::Decline => 4,
            Self::Ack => 5,
            Self::Nak => 6,
            Self::Release => 7,
            Self::Inform => 8,
        }
    }
}

/// The fixed 236-byte BOOTP header.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DhcpHeader {
    pub op: DhcpOp,
    pub hardware_type: u8,
    pub hardware_length: u8,
    pub hops: u8,
    pub transaction_id: u32,
    pub seconds: u16,
    pub flags: u16,
    pub client_address: Ipv4Address,
    pub your_address: Ipv4Address,
    pub server_address: Ipv4Address,
    pub relay_address: Ipv4Address,
    pub client_hardware: [u8; 16],
    pub server_name: [u8; 64],
    pub boot_file: [u8; 128],
}

impl DhcpHeader {
    pub const LEN: usize = 236;

    /// Construct the Ethernet client form used by a DHCP DISCOVER.
    pub const fn discover(transaction_id: u32, mac: MacAddress) -> Self {
        Self::client_request(transaction_id, mac, Ipv4Address::UNSPECIFIED)
    }

    /// Construct a client request, optionally with `ciaddr` for renewal.
    pub const fn client_request(
        transaction_id: u32,
        mac: MacAddress,
        client_address: Ipv4Address,
    ) -> Self {
        Self::client_request_with_flags(transaction_id, mac, client_address, 0x8000)
    }

    /// Construct a client request with an explicit BOOTP flags field.
    pub const fn client_request_with_flags(
        transaction_id: u32,
        mac: MacAddress,
        client_address: Ipv4Address,
        flags: u16,
    ) -> Self {
        let mut client_hardware = [0u8; 16];
        client_hardware[0] = mac.0[0];
        client_hardware[1] = mac.0[1];
        client_hardware[2] = mac.0[2];
        client_hardware[3] = mac.0[3];
        client_hardware[4] = mac.0[4];
        client_hardware[5] = mac.0[5];
        Self {
            op: DhcpOp::Request,
            hardware_type: 1,
            hardware_length: 6,
            hops: 0,
            transaction_id,
            seconds: 0,
            flags,
            client_address,
            your_address: Ipv4Address::UNSPECIFIED,
            server_address: Ipv4Address::UNSPECIFIED,
            relay_address: Ipv4Address::UNSPECIFIED,
            client_hardware,
            server_name: [0; 64],
            boot_file: [0; 128],
        }
    }

    pub fn client_mac(&self) -> MacAddress {
        MacAddress(self.client_hardware[..6].try_into().unwrap())
    }

    pub fn encode(&self, output: &mut [u8]) -> Result<usize> {
        if output.len() < Self::LEN {
            return Err(Error::BufferTooSmall);
        }
        if self.hardware_length > 16 {
            return Err(Error::Invalid);
        }
        output[..Self::LEN].fill(0);
        output[0] = self.op.byte();
        output[1] = self.hardware_type;
        output[2] = self.hardware_length;
        output[3] = self.hops;
        output[4..8].copy_from_slice(&self.transaction_id.to_be_bytes());
        output[8..10].copy_from_slice(&self.seconds.to_be_bytes());
        output[10..12].copy_from_slice(&self.flags.to_be_bytes());
        output[12..16].copy_from_slice(&self.client_address.0);
        output[16..20].copy_from_slice(&self.your_address.0);
        output[20..24].copy_from_slice(&self.server_address.0);
        output[24..28].copy_from_slice(&self.relay_address.0);
        output[28..44].copy_from_slice(&self.client_hardware);
        output[44..108].copy_from_slice(&self.server_name);
        output[108..236].copy_from_slice(&self.boot_file);
        Ok(Self::LEN)
    }

    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < Self::LEN {
            return Err(Error::Truncated);
        }
        let op = DhcpOp::from_byte(bytes[0])?;
        let hardware_length = bytes[2];
        if hardware_length > 16 {
            return Err(Error::Invalid);
        }
        Ok(Self {
            op,
            hardware_type: bytes[1],
            hardware_length,
            hops: bytes[3],
            transaction_id: u32::from_be_bytes(bytes[4..8].try_into().unwrap()),
            seconds: u16::from_be_bytes(bytes[8..10].try_into().unwrap()),
            flags: u16::from_be_bytes(bytes[10..12].try_into().unwrap()),
            client_address: Ipv4Address(bytes[12..16].try_into().unwrap()),
            your_address: Ipv4Address(bytes[16..20].try_into().unwrap()),
            server_address: Ipv4Address(bytes[20..24].try_into().unwrap()),
            relay_address: Ipv4Address(bytes[24..28].try_into().unwrap()),
            client_hardware: bytes[28..44].try_into().unwrap(),
            server_name: bytes[44..108].try_into().unwrap(),
            boot_file: bytes[108..236].try_into().unwrap(),
        })
    }
}

/// A DHCP packet: fixed BOOTP header followed by DHCP options.
///
/// `options` excludes the four-byte magic cookie.  It normally ends with
/// option 255; `encode` appends that terminator when it is absent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DhcpPacket<'a> {
    pub header: DhcpHeader,
    pub options: &'a [u8],
}

impl<'a> DhcpPacket<'a> {
    pub const FIXED_LEN: usize = DhcpHeader::LEN + 4;

    pub const fn new(header: DhcpHeader, options: &'a [u8]) -> Self {
        Self { header, options }
    }

    pub fn parse(bytes: &'a [u8]) -> Result<Self> {
        if bytes.len() < Self::FIXED_LEN {
            return Err(Error::Truncated);
        }
        if u32::from_be_bytes(bytes[236..240].try_into().unwrap()) != DHCP_MAGIC_COOKIE {
            return Err(Error::Invalid);
        }
        let options = &bytes[Self::FIXED_LEN..];
        let mut parsed_options = DhcpOptions::new(options);
        while parsed_options.next_option()?.is_some() {}
        Ok(Self {
            header: DhcpHeader::parse(bytes)?,
            options,
        })
    }

    pub fn encode(&self, output: &mut [u8]) -> Result<usize> {
        let needs_end = self.options.last().copied() != Some(DHCP_OPTION_END);
        let length = Self::FIXED_LEN + self.options.len() + usize::from(needs_end);
        if output.len() < length {
            return Err(Error::BufferTooSmall);
        }
        self.header.encode(output)?;
        output[236..240].copy_from_slice(&DHCP_MAGIC_COOKIE.to_be_bytes());
        output[240..240 + self.options.len()].copy_from_slice(self.options);
        if needs_end {
            output[240 + self.options.len()] = DHCP_OPTION_END;
        }
        Ok(length)
    }

    pub fn message_type(&self) -> Result<DhcpMessageType> {
        match DhcpOptions::new(self.options).find(DHCP_OPTION_MESSAGE_TYPE)? {
            Some(value) if value.len() == 1 => DhcpMessageType::from_byte(value[0]),
            _ => Err(Error::Invalid),
        }
    }
}

/// One borrowed DHCP option.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DhcpOption<'a> {
    pub code: u8,
    pub data: &'a [u8],
}

/// An allocation-free iterator over DHCP TLV options.
#[derive(Clone, Copy, Debug)]
pub struct DhcpOptions<'a> {
    bytes: &'a [u8],
    offset: usize,
    ended: bool,
}

impl<'a> DhcpOptions<'a> {
    pub const fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            offset: 0,
            ended: false,
        }
    }

    pub fn next_option(&mut self) -> Result<Option<DhcpOption<'a>>> {
        while !self.ended && self.offset < self.bytes.len() {
            let code = self.bytes[self.offset];
            self.offset += 1;
            if code == DHCP_OPTION_PAD {
                continue;
            }
            if code == DHCP_OPTION_END {
                self.ended = true;
                return Ok(None);
            }
            if self.offset >= self.bytes.len() {
                return Err(Error::Truncated);
            }
            let length = self.bytes[self.offset] as usize;
            self.offset += 1;
            if self.bytes.len() - self.offset < length {
                return Err(Error::Truncated);
            }
            let data = &self.bytes[self.offset..self.offset + length];
            self.offset += length;
            return Ok(Some(DhcpOption { code, data }));
        }
        if self.ended {
            Ok(None)
        } else {
            Err(Error::Truncated)
        }
    }

    pub fn find(&self, code: u8) -> Result<Option<&'a [u8]>> {
        let mut options = *self;
        let mut found = None;
        while let Some(option) = options.next_option()? {
            if option.code == code {
                found = Some(option.data);
            }
        }
        Ok(found)
    }

    pub fn u32(&self, code: u8) -> Result<Option<u32>> {
        match self.find(code)? {
            Some(bytes) if bytes.len() == 4 => {
                Ok(Some(u32::from_be_bytes(bytes.try_into().unwrap())))
            }
            Some(_) => Err(Error::Invalid),
            None => Ok(None),
        }
    }

    pub fn ipv4(&self, code: u8) -> Result<Option<Ipv4Address>> {
        match self.find(code)? {
            Some(bytes) if bytes.len() == 4 => Ok(Some(Ipv4Address(bytes.try_into().unwrap()))),
            Some(_) => Err(Error::Invalid),
            None => Ok(None),
        }
    }
}

/// A bounded DHCP option writer.
pub struct DhcpOptionWriter<'a> {
    output: &'a mut [u8],
    offset: usize,
}

impl<'a> DhcpOptionWriter<'a> {
    pub fn new(output: &'a mut [u8]) -> Self {
        Self { output, offset: 0 }
    }

    pub fn push(&mut self, code: u8, data: &[u8]) -> Result<()> {
        if code == DHCP_OPTION_PAD || code == DHCP_OPTION_END || data.len() > u8::MAX as usize {
            return Err(Error::Invalid);
        }
        if self.output.len() - self.offset < data.len() + 2 {
            return Err(Error::BufferTooSmall);
        }
        self.output[self.offset] = code;
        self.output[self.offset + 1] = data.len() as u8;
        self.output[self.offset + 2..self.offset + 2 + data.len()].copy_from_slice(data);
        self.offset += data.len() + 2;
        Ok(())
    }

    pub fn push_u8(&mut self, code: u8, value: u8) -> Result<()> {
        self.push(code, &[value])
    }

    pub fn push_u32(&mut self, code: u8, value: u32) -> Result<()> {
        self.push(code, &value.to_be_bytes())
    }

    pub fn push_ipv4(&mut self, code: u8, value: Ipv4Address) -> Result<()> {
        self.push(code, &value.0)
    }

    pub fn finish(mut self) -> Result<usize> {
        if self.offset == self.output.len() {
            return Err(Error::BufferTooSmall);
        }
        self.output[self.offset] = DHCP_OPTION_END;
        self.offset += 1;
        Ok(self.offset)
    }
}

/// Addresses and ports a caller should use when sending the client's packet.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DhcpTransmit {
    pub source: Ipv4Address,
    pub destination: Ipv4Address,
    pub source_port: u16,
    pub destination_port: u16,
    pub broadcast: bool,
}

/// Client lifecycle, including lease renewal and rebind phases.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DhcpClientState {
    Init,
    Selecting,
    Requesting,
    Bound,
    Renewing,
    Rebinding,
    Expired,
    Failed,
}

/// The currently leased IPv4 configuration and absolute caller-tick times.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DhcpLease {
    pub address: Ipv4Address,
    pub subnet_mask: Ipv4Address,
    pub router: Ipv4Address,
    pub server: Ipv4Address,
    pub lease_seconds: u32,
    pub renewal_seconds: u32,
    pub rebinding_seconds: u32,
    pub acquired_at: u32,
    pub renewal_at: u32,
    pub rebinding_at: u32,
    pub expires_at: u32,
}

const DHCP_OPTION_SCRATCH: usize = 64;

/// A bounded DHCP client implementing DISCOVER/OFFER/REQUEST/ACK.
///
/// `now` and all configured retry/lease values use the caller's tick unit;
/// the usual choice is one tick per second because DHCP lease options are
/// expressed in seconds.  `poll` returns a DHCP payload, not an Ethernet
/// frame.  Wrap it in `UdpDatagram::new(68, 67, payload)` and use
/// `DhcpTransmit` to choose the IPv4 addresses.  During bootstrap, source
/// `0.0.0.0`, destination `255.255.255.255`, and a zero UDP checksum are
/// valid; use `UdpDatagram::encode_with_checksum(..., false)` if desired.
pub struct DhcpClient<const OPTION_CAP: usize = DHCP_OPTION_SCRATCH> {
    mac: MacAddress,
    transaction_id: u32,
    state: DhcpClientState,
    retry_ticks: u32,
    max_retries: u8,
    attempts: u8,
    deadline: Option<u32>,
    offered_address: Ipv4Address,
    offered_server: Ipv4Address,
    lease: Option<DhcpLease>,
    option_scratch: [u8; OPTION_CAP],
}

impl<const OPTION_CAP: usize> DhcpClient<OPTION_CAP> {
    pub const fn new(
        mac: MacAddress,
        transaction_id: u32,
        retry_ticks: u32,
        max_retries: u8,
    ) -> Self {
        Self {
            mac,
            transaction_id,
            state: DhcpClientState::Init,
            retry_ticks,
            max_retries,
            attempts: 0,
            deadline: None,
            offered_address: Ipv4Address::UNSPECIFIED,
            offered_server: Ipv4Address::UNSPECIFIED,
            lease: None,
            option_scratch: [0; OPTION_CAP],
        }
    }

    pub fn state(&self) -> DhcpClientState {
        self.state
    }

    pub fn mac(&self) -> MacAddress {
        self.mac
    }

    pub fn transaction_id(&self) -> u32 {
        self.transaction_id
    }

    pub fn lease(&self) -> Option<DhcpLease> {
        self.lease
    }

    pub fn retransmission_deadline(&self) -> Option<u32> {
        self.deadline
    }

    pub fn retransmission_due(&self, now: u32) -> bool {
        self.deadline
            .map(|deadline| tick_reached(now, deadline))
            .unwrap_or(false)
    }

    pub fn transmit(&self) -> DhcpTransmit {
        match self.state {
            DhcpClientState::Renewing => DhcpTransmit {
                source: self
                    .lease
                    .map_or(DHCP_BOOTSTRAP_SOURCE, |lease| lease.address),
                destination: self.offered_server,
                source_port: DHCP_CLIENT_PORT,
                destination_port: DHCP_SERVER_PORT,
                broadcast: false,
            },
            DhcpClientState::Rebinding => DhcpTransmit {
                source: self
                    .lease
                    .map_or(DHCP_BOOTSTRAP_SOURCE, |lease| lease.address),
                destination: DHCP_BOOTSTRAP_DESTINATION,
                source_port: DHCP_CLIENT_PORT,
                destination_port: DHCP_SERVER_PORT,
                broadcast: true,
            },
            _ => DhcpTransmit {
                source: DHCP_BOOTSTRAP_SOURCE,
                destination: DHCP_BOOTSTRAP_DESTINATION,
                source_port: DHCP_CLIENT_PORT,
                destination_port: DHCP_SERVER_PORT,
                broadcast: true,
            },
        }
    }

    /// Emit the next packet whose deadline has arrived, if any.
    pub fn poll(&mut self, now: u32, output: &mut [u8]) -> Result<Option<usize>> {
        if self.lease_expired(now) {
            self.state = DhcpClientState::Expired;
            self.deadline = None;
            self.lease = None;
            return Ok(None);
        }
        match self.state {
            DhcpClientState::Init => {
                if self.max_retries == 0 {
                    self.state = DhcpClientState::Failed;
                    return Ok(None);
                }
                let length = self.encode_request(output, DhcpMessageType::Discover, false, true)?;
                self.state = DhcpClientState::Selecting;
                self.attempts = 1;
                self.deadline = Some(now.wrapping_add(self.retry_ticks));
                Ok(Some(length))
            }
            DhcpClientState::Selecting => {
                if !self.retransmission_due(now) {
                    return Ok(None);
                }
                if self.attempts >= self.max_retries {
                    self.state = DhcpClientState::Failed;
                    self.deadline = None;
                    return Ok(None);
                }
                let length = self.encode_request(output, DhcpMessageType::Discover, false, true)?;
                self.attempts += 1;
                self.deadline = Some(now.wrapping_add(self.retry_ticks));
                Ok(Some(length))
            }
            DhcpClientState::Requesting => {
                if !self.retransmission_due(now) {
                    return Ok(None);
                }
                if self.attempts >= self.max_retries {
                    self.state = DhcpClientState::Failed;
                    self.deadline = None;
                    return Ok(None);
                }
                let length = self.encode_request(output, DhcpMessageType::Request, false, true)?;
                self.attempts += 1;
                self.deadline = Some(now.wrapping_add(self.retry_ticks));
                Ok(Some(length))
            }
            DhcpClientState::Bound => {
                if let Some(lease) = self.lease
                    && tick_reached(now, lease.renewal_at)
                {
                    self.state = DhcpClientState::Renewing;
                    self.attempts = 0;
                    self.deadline = None;
                    return self.send_renewal(now, output);
                }
                Ok(None)
            }
            DhcpClientState::Renewing => {
                let lease = self.lease.ok_or(Error::Unexpected)?;
                if tick_reached(now, lease.rebinding_at) {
                    self.state = DhcpClientState::Rebinding;
                    self.attempts = 0;
                    self.deadline = None;
                    return self.send_renewal(now, output);
                }
                if !self.retransmission_due(now) {
                    return Ok(None);
                }
                if self.attempts >= self.max_retries {
                    self.deadline = Some(lease.rebinding_at);
                    return Ok(None);
                }
                self.send_renewal(now, output)
            }
            DhcpClientState::Rebinding => {
                let lease = self.lease.ok_or(Error::Unexpected)?;
                if !self.retransmission_due(now) {
                    return Ok(None);
                }
                if self.attempts >= self.max_retries {
                    self.deadline = Some(lease.expires_at);
                    return Ok(None);
                }
                self.send_renewal(now, output)
            }
            DhcpClientState::Expired | DhcpClientState::Failed => Ok(None),
        }
    }

    /// Consume one received DHCP packet after the caller has parsed it.
    pub fn receive(&mut self, now: u32, packet: &DhcpPacket<'_>) -> Result<()> {
        if packet.header.op != DhcpOp::Reply
            || packet.header.hardware_type != 1
            || packet.header.hardware_length != 6
            || packet.header.transaction_id != self.transaction_id
            || packet.header.client_mac() != self.mac
        {
            return Err(Error::Unexpected);
        }
        let message_type = packet.message_type()?;
        match message_type {
            DhcpMessageType::Offer if self.state == DhcpClientState::Selecting => {
                if packet.header.your_address == Ipv4Address::UNSPECIFIED {
                    return Err(Error::Invalid);
                }
                let options = DhcpOptions::new(packet.options);
                self.offered_server = options
                    .ipv4(DHCP_OPTION_SERVER_IDENTIFIER)?
                    .unwrap_or(packet.header.server_address);
                if self.offered_server == Ipv4Address::UNSPECIFIED {
                    return Err(Error::Invalid);
                }
                self.offered_address = packet.header.your_address;
                self.state = DhcpClientState::Requesting;
                self.attempts = 0;
                self.deadline = Some(now);
                Ok(())
            }
            DhcpMessageType::Ack
                if self.state == DhcpClientState::Requesting
                    || self.state == DhcpClientState::Renewing
                    || self.state == DhcpClientState::Rebinding =>
            {
                self.install_lease(now, packet)
            }
            DhcpMessageType::Nak
                if self.state == DhcpClientState::Requesting
                    || self.state == DhcpClientState::Renewing
                    || self.state == DhcpClientState::Rebinding =>
            {
                self.state = DhcpClientState::Init;
                self.deadline = None;
                self.lease = None;
                self.attempts = 0;
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn encode_request(
        &mut self,
        output: &mut [u8],
        message_type: DhcpMessageType,
        renewal: bool,
        broadcast: bool,
    ) -> Result<usize> {
        let lease_address = self
            .lease
            .map_or(Ipv4Address::UNSPECIFIED, |lease| lease.address);
        let client_address = if renewal {
            lease_address
        } else {
            Ipv4Address::UNSPECIFIED
        };
        let header = DhcpHeader::client_request_with_flags(
            self.transaction_id,
            self.mac,
            client_address,
            if broadcast { 0x8000 } else { 0 },
        );
        let option_length = {
            let mut writer = DhcpOptionWriter::new(&mut self.option_scratch);
            writer.push_u8(DHCP_OPTION_MESSAGE_TYPE, message_type.byte())?;
            if message_type == DhcpMessageType::Request && !renewal {
                writer.push_ipv4(DHCP_OPTION_REQUESTED_IP, self.offered_address)?;
                writer.push_ipv4(DHCP_OPTION_SERVER_IDENTIFIER, self.offered_server)?;
            }
            writer.push(
                DHCP_OPTION_PARAMETER_REQUEST_LIST,
                &[
                    DHCP_OPTION_SUBNET_MASK,
                    DHCP_OPTION_ROUTER,
                    DHCP_OPTION_DNS,
                    DHCP_OPTION_SERVER_IDENTIFIER,
                    DHCP_OPTION_LEASE_TIME,
                    DHCP_OPTION_RENEWAL_TIME,
                    DHCP_OPTION_REBINDING_TIME,
                ],
            )?;
            writer.finish()?
        };
        DhcpPacket::new(header, &self.option_scratch[..option_length]).encode(output)
    }

    fn send_renewal(&mut self, now: u32, output: &mut [u8]) -> Result<Option<usize>> {
        let length = self.encode_request(
            output,
            DhcpMessageType::Request,
            true,
            self.state == DhcpClientState::Rebinding,
        )?;
        self.attempts = self.attempts.saturating_add(1);
        self.deadline = Some(now.wrapping_add(self.retry_ticks));
        Ok(Some(length))
    }

    fn install_lease(&mut self, now: u32, packet: &DhcpPacket<'_>) -> Result<()> {
        let options = DhcpOptions::new(packet.options);
        let lease_seconds = options.u32(DHCP_OPTION_LEASE_TIME)?.ok_or(Error::Invalid)?;
        if lease_seconds == 0 {
            return Err(Error::Invalid);
        }
        let renewal_seconds = options
            .u32(DHCP_OPTION_RENEWAL_TIME)?
            .unwrap_or(lease_seconds / 2);
        let rebinding_seconds = options
            .u32(DHCP_OPTION_REBINDING_TIME)?
            .unwrap_or(lease_seconds.saturating_mul(7) / 8);
        if renewal_seconds >= rebinding_seconds || rebinding_seconds >= lease_seconds {
            return Err(Error::Invalid);
        }
        let server = options
            .ipv4(DHCP_OPTION_SERVER_IDENTIFIER)?
            .unwrap_or(self.offered_server);
        if server == Ipv4Address::UNSPECIFIED {
            return Err(Error::Invalid);
        }
        let address = if packet.header.your_address != Ipv4Address::UNSPECIFIED {
            packet.header.your_address
        } else if let Some(previous) = self.lease {
            previous.address
        } else {
            self.offered_address
        };
        if address == Ipv4Address::UNSPECIFIED {
            return Err(Error::Invalid);
        }
        let lease = DhcpLease {
            address,
            subnet_mask: options
                .ipv4(DHCP_OPTION_SUBNET_MASK)?
                .unwrap_or(Ipv4Address::UNSPECIFIED),
            router: options
                .ipv4(DHCP_OPTION_ROUTER)?
                .unwrap_or(Ipv4Address::UNSPECIFIED),
            server,
            lease_seconds,
            renewal_seconds,
            rebinding_seconds,
            acquired_at: now,
            renewal_at: now.wrapping_add(renewal_seconds),
            rebinding_at: now.wrapping_add(rebinding_seconds),
            expires_at: now.wrapping_add(lease_seconds),
        };
        self.offered_server = server;
        self.lease = Some(lease);
        self.state = DhcpClientState::Bound;
        self.deadline = None;
        self.attempts = 0;
        Ok(())
    }

    fn lease_expired(&self, now: u32) -> bool {
        self.lease
            .map(|lease| tick_reached(now, lease.expires_at))
            .unwrap_or(false)
    }
}

fn tick_reached(now: u32, deadline: u32) -> bool {
    now.wrapping_sub(deadline) < 0x8000_0000
}

pub const TCP_FIN: u16 = 0x001;
pub const TCP_SYN: u16 = 0x002;
pub const TCP_RST: u16 = 0x004;
pub const TCP_PSH: u16 = 0x008;
pub const TCP_ACK: u16 = 0x010;

/// A borrowed TCP segment with no parsed options.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TcpSegment<'a> {
    pub source_port: u16,
    pub destination_port: u16,
    pub sequence: u32,
    pub acknowledgment: u32,
    pub flags: u16,
    pub window: u16,
    pub checksum: u16,
    pub urgent: u16,
    pub data: &'a [u8],
}

impl<'a> TcpSegment<'a> {
    pub const MIN_HEADER_LEN: usize = 20;

    pub const fn new(
        source_port: u16,
        destination_port: u16,
        sequence: u32,
        acknowledgment: u32,
        flags: u16,
        window: u16,
        data: &'a [u8],
    ) -> Self {
        Self {
            source_port,
            destination_port,
            sequence,
            acknowledgment,
            flags,
            window,
            checksum: 0,
            urgent: 0,
            data,
        }
    }

    pub fn parse(
        bytes: &'a [u8],
        source: Ipv4Address,
        destination: Ipv4Address,
        verify_checksum: bool,
    ) -> Result<Self> {
        if bytes.len() < Self::MIN_HEADER_LEN {
            return Err(Error::Truncated);
        }
        let data_offset = (bytes[12] >> 4) as usize * 4;
        if data_offset < Self::MIN_HEADER_LEN || data_offset > bytes.len() {
            return Err(Error::Invalid);
        }
        if verify_checksum && pseudo_checksum(source, destination, 6, bytes) != 0 {
            return Err(Error::BadChecksum);
        }
        Ok(Self {
            source_port: u16::from_be_bytes([bytes[0], bytes[1]]),
            destination_port: u16::from_be_bytes([bytes[2], bytes[3]]),
            sequence: u32::from_be_bytes(bytes[4..8].try_into().unwrap()),
            acknowledgment: u32::from_be_bytes(bytes[8..12].try_into().unwrap()),
            flags: u16::from_be_bytes([bytes[12] & 1, bytes[13]]),
            window: u16::from_be_bytes([bytes[14], bytes[15]]),
            checksum: u16::from_be_bytes([bytes[16], bytes[17]]),
            urgent: u16::from_be_bytes([bytes[18], bytes[19]]),
            data: &bytes[data_offset..],
        })
    }

    /// Encode the segment and calculate its IPv4 pseudo-header checksum.
    pub fn encode(
        &self,
        output: &mut [u8],
        source: Ipv4Address,
        destination: Ipv4Address,
    ) -> Result<usize> {
        let length = Self::MIN_HEADER_LEN + self.data.len();
        if length > u16::MAX as usize || output.len() < length {
            return Err(Error::BufferTooSmall);
        }
        output[..length].fill(0);
        output[0..2].copy_from_slice(&self.source_port.to_be_bytes());
        output[2..4].copy_from_slice(&self.destination_port.to_be_bytes());
        output[4..8].copy_from_slice(&self.sequence.to_be_bytes());
        output[8..12].copy_from_slice(&self.acknowledgment.to_be_bytes());
        output[12] = 5 << 4;
        output[13] = self.flags as u8;
        output[14..16].copy_from_slice(&self.window.to_be_bytes());
        output[18..20].copy_from_slice(&self.urgent.to_be_bytes());
        output[20..length].copy_from_slice(self.data);
        let checksum = pseudo_checksum(source, destination, 6, &output[..length]);
        output[16..18].copy_from_slice(&checksum.to_be_bytes());
        Ok(length)
    }
}

/// The states of the bounded single-connection TCP helper.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TcpState {
    Listen,
    SynSent,
    SynReceived,
    Established,
    FinWait1,
    FinWait2,
    CloseWait,
    LastAck,
    Closed,
}

const TCP_RX_CAPACITY: usize = 512;

/// A small one-connection TCP state machine.
///
/// It performs no retransmission itself: callers provide a tick value,
/// inspect [`TcpConnection::retransmission_due`], and call
/// [`TcpConnection::retransmit`] when appropriate.  At most one segment is
/// outstanding, and received application data is copied into a 512-byte
/// fixed buffer.
pub struct TcpConnection<const RX: usize = TCP_RX_CAPACITY> {
    local_ip: Ipv4Address,
    remote_ip: Ipv4Address,
    local_port: u16,
    remote_port: u16,
    state: TcpState,
    send_next: u32,
    receive_next: u32,
    pending_seq: u32,
    pending_end: u32,
    pending_flags: u16,
    pending_payload: [u8; RX],
    pending_payload_len: usize,
    pending: bool,
    deadline: Option<u32>,
    retransmit_ticks: u32,
    receive_buffer: [u8; RX],
    receive_len: usize,
}

impl<const RX: usize> TcpConnection<RX> {
    pub const fn listen(
        local_ip: Ipv4Address,
        local_port: u16,
        remote_ip: Ipv4Address,
        remote_port: u16,
        initial_sequence: u32,
        retransmit_ticks: u32,
    ) -> Self {
        Self {
            local_ip,
            remote_ip,
            local_port,
            remote_port,
            state: TcpState::Listen,
            send_next: initial_sequence,
            receive_next: 0,
            pending_seq: 0,
            pending_end: 0,
            pending_flags: 0,
            pending_payload: [0; RX],
            pending_payload_len: 0,
            pending: false,
            deadline: None,
            retransmit_ticks,
            receive_buffer: [0; RX],
            receive_len: 0,
        }
    }

    pub const fn active(
        local_ip: Ipv4Address,
        local_port: u16,
        remote_ip: Ipv4Address,
        remote_port: u16,
        initial_sequence: u32,
        retransmit_ticks: u32,
    ) -> Self {
        let mut connection = Self::listen(
            local_ip,
            local_port,
            remote_ip,
            remote_port,
            initial_sequence,
            retransmit_ticks,
        );
        connection.state = TcpState::Closed;
        connection
    }

    pub fn state(&self) -> TcpState {
        self.state
    }

    pub fn retransmission_deadline(&self) -> Option<u32> {
        self.deadline
    }

    pub fn retransmission_due(&self, now: u32) -> bool {
        self.deadline
            .map(|deadline| now.wrapping_sub(deadline) < 0x8000_0000)
            .unwrap_or(false)
    }

    pub fn start_connect(&mut self, now: u32, output: &mut [u8]) -> Result<usize> {
        if self.state != TcpState::Closed {
            return Err(Error::Unexpected);
        }
        let sequence = self.send_next;
        let length = self.emit(sequence, 0, TCP_SYN, &[], output)?;
        self.send_next = sequence.wrapping_add(1);
        self.remember_pending(sequence, sequence.wrapping_add(1), TCP_SYN, &[], now);
        self.state = TcpState::SynSent;
        Ok(length)
    }

    /// Process one segment and optionally write the required response.
    pub fn on_segment(
        &mut self,
        segment: &TcpSegment<'_>,
        now: u32,
        output: &mut [u8],
    ) -> Result<Option<usize>> {
        if segment.source_port != self.remote_port || segment.destination_port != self.local_port {
            return Err(Error::Unexpected);
        }
        if segment.flags & TCP_RST != 0 {
            self.state = TcpState::Closed;
            self.pending = false;
            self.deadline = None;
            return Ok(None);
        }
        match self.state {
            TcpState::Listen => {
                if segment.flags & TCP_SYN == 0 {
                    return Ok(None);
                }
                self.receive_next = segment.sequence.wrapping_add(1);
                let sequence = self.send_next;
                let length =
                    self.emit(sequence, self.receive_next, TCP_SYN | TCP_ACK, &[], output)?;
                self.send_next = sequence.wrapping_add(1);
                self.remember_pending(sequence, self.send_next, TCP_SYN | TCP_ACK, &[], now);
                self.state = TcpState::SynReceived;
                Ok(Some(length))
            }
            TcpState::SynSent => {
                if segment.flags & (TCP_SYN | TCP_ACK) == (TCP_SYN | TCP_ACK)
                    && segment.acknowledgment == self.send_next
                {
                    self.receive_next = segment.sequence.wrapping_add(1);
                    self.pending = false;
                    self.deadline = None;
                    let length =
                        self.emit(self.send_next, self.receive_next, TCP_ACK, &[], output)?;
                    self.state = TcpState::Established;
                    Ok(Some(length))
                } else {
                    Ok(None)
                }
            }
            TcpState::SynReceived => {
                if segment.flags & TCP_ACK != 0 && segment.acknowledgment == self.send_next {
                    self.pending = false;
                    self.deadline = None;
                    self.state = TcpState::Established;
                }
                Ok(None)
            }
            TcpState::Established
            | TcpState::FinWait1
            | TcpState::FinWait2
            | TcpState::CloseWait => {
                if segment.sequence == self.receive_next
                    && segment.data.len() > RX.saturating_sub(self.receive_len)
                {
                    return Err(Error::BufferTooSmall);
                }
                self.accept_ack(segment.acknowledgment);
                let mut response = false;
                if !segment.data.is_empty() && segment.sequence == self.receive_next {
                    let available = RX.saturating_sub(self.receive_len);
                    let copied = core::cmp::min(available, segment.data.len());
                    self.receive_buffer[self.receive_len..self.receive_len + copied]
                        .copy_from_slice(&segment.data[..copied]);
                    self.receive_len += copied;
                    self.receive_next = self.receive_next.wrapping_add(segment.data.len() as u32);
                    response = true;
                }
                if segment.flags & TCP_FIN != 0
                    && segment.sequence.wrapping_add(segment.data.len() as u32) == self.receive_next
                {
                    self.receive_next = self.receive_next.wrapping_add(1);
                    response = true;
                    if self.state == TcpState::Established {
                        self.state = TcpState::CloseWait;
                    } else if self.state == TcpState::FinWait2
                        || (self.state == TcpState::FinWait1 && !self.pending)
                    {
                        self.state = TcpState::Closed;
                    }
                }
                if self.state == TcpState::FinWait1 && !self.pending {
                    self.state = TcpState::FinWait2;
                }
                if response {
                    return Ok(Some(self.emit(
                        self.send_next,
                        self.receive_next,
                        TCP_ACK,
                        &[],
                        output,
                    )?));
                }
                Ok(None)
            }
            TcpState::LastAck => {
                self.accept_ack(segment.acknowledgment);
                if !self.pending {
                    self.state = TcpState::Closed;
                }
                Ok(None)
            }
            TcpState::Closed => Ok(None),
        }
    }

    /// Send application data and remember it as the one retransmittable segment.
    pub fn send_data(&mut self, data: &[u8], now: u32, output: &mut [u8]) -> Result<usize> {
        if self.state != TcpState::Established || self.pending || data.len() > RX {
            return Err(Error::Unexpected);
        }
        let sequence = self.send_next;
        let length = self.emit(sequence, self.receive_next, TCP_ACK | TCP_PSH, data, output)?;
        self.send_next = sequence.wrapping_add(data.len() as u32);
        self.remember_pending(sequence, self.send_next, TCP_ACK | TCP_PSH, data, now);
        Ok(length)
    }

    /// Start an orderly close with FIN.  A peer ACK moves the state forward.
    pub fn close(&mut self, now: u32, output: &mut [u8]) -> Result<usize> {
        if (self.state != TcpState::Established && self.state != TcpState::CloseWait)
            || self.pending
        {
            return Err(Error::Unexpected);
        }
        let sequence = self.send_next;
        let length = self.emit(sequence, self.receive_next, TCP_FIN | TCP_ACK, &[], output)?;
        self.send_next = sequence.wrapping_add(1);
        self.remember_pending(sequence, self.send_next, TCP_FIN | TCP_ACK, &[], now);
        self.state = if self.state == TcpState::Established {
            TcpState::FinWait1
        } else {
            TcpState::LastAck
        };
        Ok(length)
    }

    /// Rebuild the outstanding SYN, data, or FIN after its deadline.
    pub fn retransmit(&mut self, now: u32, output: &mut [u8]) -> Result<Option<usize>> {
        if !self.retransmission_due(now) || !self.pending {
            return Ok(None);
        }
        let length = self.emit(
            self.pending_seq,
            self.receive_next,
            self.pending_flags,
            &self.pending_payload[..self.pending_payload_len],
            output,
        )?;
        self.deadline = Some(now.wrapping_add(self.retransmit_ticks));
        Ok(Some(length))
    }

    pub fn received(&self) -> &[u8] {
        &self.receive_buffer[..self.receive_len]
    }

    pub fn take_received(&mut self, output: &mut [u8]) -> usize {
        let count = core::cmp::min(output.len(), self.receive_len);
        output[..count].copy_from_slice(&self.receive_buffer[..count]);
        self.receive_buffer.copy_within(count..self.receive_len, 0);
        self.receive_len -= count;
        count
    }

    fn emit(
        &self,
        sequence: u32,
        acknowledgment: u32,
        flags: u16,
        data: &[u8],
        output: &mut [u8],
    ) -> Result<usize> {
        TcpSegment {
            source_port: self.local_port,
            destination_port: self.remote_port,
            sequence,
            acknowledgment,
            flags,
            window: RX.min(u16::MAX as usize) as u16,
            checksum: 0,
            urgent: 0,
            data,
        }
        .encode(output, self.local_ip, self.remote_ip)
    }

    fn remember_pending(&mut self, sequence: u32, end: u32, flags: u16, payload: &[u8], now: u32) {
        self.pending_seq = sequence;
        self.pending_end = end;
        self.pending_flags = flags;
        self.pending_payload_len = payload.len();
        self.pending_payload[..payload.len()].copy_from_slice(payload);
        self.pending = true;
        self.deadline = Some(now.wrapping_add(self.retransmit_ticks));
    }

    fn accept_ack(&mut self, acknowledgment: u32) {
        if self.pending && seq_at_or_after(acknowledgment, self.pending_end) {
            self.pending = false;
            self.deadline = None;
        }
    }
}

fn seq_at_or_after(left: u32, right: u32) -> bool {
    left.wrapping_sub(right) < 0x8000_0000
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAC_A: MacAddress = MacAddress([0x02, 0, 0, 0, 0, 1]);
    const MAC_B: MacAddress = MacAddress([0x02, 0, 0, 0, 0, 2]);
    const IP_A: Ipv4Address = Ipv4Address([192, 0, 2, 1]);
    const IP_B: Ipv4Address = Ipv4Address([192, 0, 2, 2]);

    #[test]
    fn ethernet_round_trip() {
        let frame = EthernetFrame::new(MAC_B, MAC_A, ETHERTYPE_IPV4, b"hello");
        let mut bytes = [0u8; 32];
        let length = frame.encode(&mut bytes).unwrap();
        assert_eq!(length, 19);
        assert_eq!(EthernetFrame::parse(&bytes[..length]).unwrap(), frame);
        assert_eq!(
            EthernetFrame::parse(&bytes[..EthernetFrame::HEADER_LEN - 1]),
            Err(Error::Truncated)
        );
    }

    #[test]
    fn arp_request_and_cache() {
        let packet = ArpPacket::request(MAC_A, IP_A, IP_B);
        let mut bytes = [0u8; ArpPacket::LEN];
        packet.encode(&mut bytes).unwrap();
        assert_eq!(ArpPacket::parse(&bytes).unwrap(), packet);
        let reply = ArpPacket::reply(MAC_B, IP_B, MAC_A, IP_A);
        reply.encode(&mut bytes).unwrap();
        assert_eq!(ArpPacket::parse(&bytes).unwrap(), reply);

        let mut cache = ArpCache::<2>::new();
        cache.insert(IP_B, MAC_B, 1);
        assert_eq!(cache.lookup(IP_B, 2), Some(MAC_B));
        assert_eq!(cache.len(), 1);
        cache.insert(Ipv4Address([192, 0, 2, 3]), MAC_A, 3);
        cache.insert(Ipv4Address([192, 0, 2, 4]), MAC_A, 4);
        assert_eq!(cache.lookup(IP_B, 5), None);
    }

    #[test]
    fn ipv4_checksum_and_payload() {
        let packet = Ipv4Packet {
            header: Ipv4Header::new(IP_A, IP_B, 17, 3),
            payload: b"abc",
        };
        let mut bytes = [0u8; 64];
        let length = packet.encode(&mut bytes).unwrap();
        assert_eq!(internet_checksum(&bytes[..20]), 0);
        let parsed = Ipv4Packet::parse(&bytes[..length], true).unwrap();
        assert_eq!(parsed.payload, b"abc");
        assert_eq!(parsed.header.source, IP_A);
        bytes[8] = 63;
        assert_eq!(
            Ipv4Packet::parse(&bytes[..length], true),
            Err(Error::BadChecksum)
        );
    }

    #[test]
    fn icmp_echo_round_trip() {
        let packet = IcmpEcho::request(7, 9, b"ping");
        let mut bytes = [0u8; 32];
        let length = packet.encode(&mut bytes).unwrap();
        assert_eq!(internet_checksum(&bytes[..length]), 0);
        assert_eq!(IcmpEcho::parse(&bytes[..length], true).unwrap(), packet);
        let reply = IcmpEcho::reply(7, 9, b"ping");
        let reply_length = reply.encode(&mut bytes).unwrap();
        assert_eq!(
            IcmpEcho::parse(&bytes[..reply_length], true).unwrap(),
            reply
        );
    }

    #[test]
    fn udp_checksum_and_parse() {
        let packet = UdpDatagram::new(1000, 2000, b"data");
        let mut bytes = [0u8; 32];
        let length = packet.encode(&mut bytes, IP_A, IP_B).unwrap();
        assert_ne!(u16::from_be_bytes([bytes[6], bytes[7]]), 0);
        assert_eq!(
            UdpDatagram::parse(&bytes[..length], true, IP_A, IP_B)
                .unwrap()
                .payload,
            b"data"
        );
        let length = packet
            .encode_with_checksum(
                &mut bytes,
                DHCP_BOOTSTRAP_SOURCE,
                DHCP_BOOTSTRAP_DESTINATION,
                false,
            )
            .unwrap();
        assert_eq!(&bytes[6..8], &[0, 0]);
        assert_eq!(
            UdpDatagram::parse(
                &bytes[..length],
                true,
                DHCP_BOOTSTRAP_SOURCE,
                DHCP_BOOTSTRAP_DESTINATION
            )
            .unwrap()
            .payload,
            b"data"
        );
    }

    #[test]
    fn dhcp_options_and_packet_round_trip() {
        let mac = MAC_A;
        let header = DhcpHeader::discover(0x1234_5678, mac);
        let mut options = [0u8; 32];
        let option_length = {
            let mut writer = DhcpOptionWriter::new(&mut options);
            writer
                .push_u8(DHCP_OPTION_MESSAGE_TYPE, DhcpMessageType::Discover.byte())
                .unwrap();
            writer.push_u32(DHCP_OPTION_LEASE_TIME, 3600).unwrap();
            writer.finish().unwrap()
        };
        let packet = DhcpPacket::new(header, &options[..option_length]);
        let mut bytes = [0u8; 320];
        let length = packet.encode(&mut bytes).unwrap();
        let parsed = DhcpPacket::parse(&bytes[..length]).unwrap();
        assert_eq!(parsed.header, header);
        assert_eq!(parsed.message_type().unwrap(), DhcpMessageType::Discover);
        let parsed_options = DhcpOptions::new(parsed.options);
        assert_eq!(
            parsed_options.u32(DHCP_OPTION_LEASE_TIME).unwrap(),
            Some(3600)
        );
        assert_eq!(parsed.header.client_mac(), mac);
    }

    fn response_for(
        request: &DhcpPacket<'_>,
        message_type: DhcpMessageType,
        address: Ipv4Address,
        server: Ipv4Address,
        lease_seconds: u32,
        renewal_seconds: u32,
        rebinding_seconds: u32,
    ) -> ([u8; 320], usize) {
        let mut header = request.header;
        header.op = DhcpOp::Reply;
        header.your_address = address;
        header.server_address = server;
        let mut options = [0u8; 64];
        let option_length = {
            let mut writer = DhcpOptionWriter::new(&mut options);
            writer
                .push_u8(DHCP_OPTION_MESSAGE_TYPE, message_type.byte())
                .unwrap();
            writer
                .push_ipv4(DHCP_OPTION_SERVER_IDENTIFIER, server)
                .unwrap();
            if message_type == DhcpMessageType::Ack {
                writer
                    .push_ipv4(DHCP_OPTION_SUBNET_MASK, Ipv4Address([255, 255, 255, 0]))
                    .unwrap();
                writer
                    .push_ipv4(DHCP_OPTION_ROUTER, Ipv4Address([192, 0, 2, 1]))
                    .unwrap();
                writer
                    .push_u32(DHCP_OPTION_LEASE_TIME, lease_seconds)
                    .unwrap();
                writer
                    .push_u32(DHCP_OPTION_RENEWAL_TIME, renewal_seconds)
                    .unwrap();
                writer
                    .push_u32(DHCP_OPTION_REBINDING_TIME, rebinding_seconds)
                    .unwrap();
            }
            writer.finish().unwrap()
        };
        let packet = DhcpPacket::new(header, &options[..option_length]);
        let mut bytes = [0u8; 320];
        let length = packet.encode(&mut bytes).unwrap();
        (bytes, length)
    }

    #[test]
    fn dhcp_dora_and_bootstrap_transport() {
        let mac = MAC_A;
        let server = Ipv4Address([192, 0, 2, 254]);
        let address = Ipv4Address([192, 0, 2, 42]);
        let mut client = DhcpClient::<64>::new(mac, 0xfeed_beef, 5, 3);
        let mut output = [0u8; 320];

        let discover_length = client.poll(0, &mut output).unwrap().unwrap();
        assert_eq!(client.state(), DhcpClientState::Selecting);
        assert_eq!(
            client.transmit(),
            DhcpTransmit {
                source: DHCP_BOOTSTRAP_SOURCE,
                destination: DHCP_BOOTSTRAP_DESTINATION,
                source_port: DHCP_CLIENT_PORT,
                destination_port: DHCP_SERVER_PORT,
                broadcast: true,
            }
        );
        let discover = DhcpPacket::parse(&output[..discover_length]).unwrap();
        assert_eq!(discover.message_type().unwrap(), DhcpMessageType::Discover);

        let (offer_bytes, offer_length) =
            response_for(&discover, DhcpMessageType::Offer, address, server, 0, 0, 0);
        let offer = DhcpPacket::parse(&offer_bytes[..offer_length]).unwrap();
        client.receive(1, &offer).unwrap();
        assert_eq!(client.state(), DhcpClientState::Requesting);

        let request_length = client.poll(1, &mut output).unwrap().unwrap();
        let request = DhcpPacket::parse(&output[..request_length]).unwrap();
        assert_eq!(request.message_type().unwrap(), DhcpMessageType::Request);
        let request_options = DhcpOptions::new(request.options);
        assert_eq!(
            request_options.ipv4(DHCP_OPTION_REQUESTED_IP).unwrap(),
            Some(address)
        );
        assert_eq!(
            request_options.ipv4(DHCP_OPTION_SERVER_IDENTIFIER).unwrap(),
            Some(server)
        );

        let (ack_bytes, ack_length) =
            response_for(&request, DhcpMessageType::Ack, address, server, 20, 5, 10);
        let ack = DhcpPacket::parse(&ack_bytes[..ack_length]).unwrap();
        client.receive(2, &ack).unwrap();
        assert_eq!(client.state(), DhcpClientState::Bound);
        assert_eq!(client.lease().unwrap().address, address);

        let udp = UdpDatagram::new(
            DHCP_CLIENT_PORT,
            DHCP_SERVER_PORT,
            &output[..discover_length],
        );
        let mut udp_bytes = [0u8; 400];
        let udp_length = udp
            .encode_with_checksum(
                &mut udp_bytes,
                DHCP_BOOTSTRAP_SOURCE,
                DHCP_BOOTSTRAP_DESTINATION,
                false,
            )
            .unwrap();
        assert_eq!(&udp_bytes[6..8], &[0, 0]);
        assert_eq!(
            UdpDatagram::parse(
                &udp_bytes[..udp_length],
                true,
                DHCP_BOOTSTRAP_SOURCE,
                DHCP_BOOTSTRAP_DESTINATION
            )
            .unwrap()
            .source_port,
            DHCP_CLIENT_PORT
        );
    }

    #[test]
    fn dhcp_malformed_packets_are_rejected() {
        let mut bytes = [0u8; 240];
        assert_eq!(DhcpPacket::parse(&bytes), Err(Error::Invalid));
        bytes[236..240].copy_from_slice(&DHCP_MAGIC_COOKIE.to_be_bytes());
        assert_eq!(DhcpPacket::parse(&bytes), Err(Error::Truncated));

        let mut options = DhcpOptions::new(&[DHCP_OPTION_MESSAGE_TYPE, 2, 1]);
        assert_eq!(options.next_option(), Err(Error::Truncated));

        let mut client = DhcpClient::<64>::new(MAC_A, 1, 1, 1);
        let mut output = [0u8; 320];
        let length = client.poll(0, &mut output).unwrap().unwrap();
        let packet = DhcpPacket::parse(&output[..length]).unwrap();
        let (mut offer_bytes, offer_length) =
            response_for(&packet, DhcpMessageType::Offer, IP_B, IP_A, 0, 0, 0);
        offer_bytes[4] ^= 1;
        let offer = DhcpPacket::parse(&offer_bytes[..offer_length]).unwrap();
        assert_eq!(client.receive(0, &offer), Err(Error::Unexpected));
    }

    #[test]
    fn dhcp_retry_timing_is_bounded() {
        let mut client = DhcpClient::<64>::new(MAC_A, 2, 5, 2);
        let mut output = [0u8; 320];
        assert!(client.poll(0, &mut output).unwrap().is_some());
        assert!(client.poll(4, &mut output).unwrap().is_none());
        assert!(client.poll(5, &mut output).unwrap().is_some());
        assert!(client.poll(10, &mut output).unwrap().is_none());
        assert_eq!(client.state(), DhcpClientState::Failed);
        assert_eq!(client.retransmission_deadline(), None);
    }

    #[test]
    fn dhcp_renew_rebind_and_expiry() {
        let server = Ipv4Address([192, 0, 2, 254]);
        let address = Ipv4Address([192, 0, 2, 42]);
        let mut client = DhcpClient::<64>::new(MAC_A, 3, 2, 1);
        let mut output = [0u8; 320];
        let discover_length = client.poll(0, &mut output).unwrap().unwrap();
        let discover = DhcpPacket::parse(&output[..discover_length]).unwrap();
        let (offer_bytes, offer_length) =
            response_for(&discover, DhcpMessageType::Offer, address, server, 0, 0, 0);
        let offer = DhcpPacket::parse(&offer_bytes[..offer_length]).unwrap();
        client.receive(0, &offer).unwrap();
        let request_length = client.poll(0, &mut output).unwrap().unwrap();
        let request = DhcpPacket::parse(&output[..request_length]).unwrap();
        let (ack_bytes, ack_length) =
            response_for(&request, DhcpMessageType::Ack, address, server, 20, 5, 10);
        let ack = DhcpPacket::parse(&ack_bytes[..ack_length]).unwrap();
        client.receive(0, &ack).unwrap();

        assert!(client.poll(4, &mut output).unwrap().is_none());
        let renewal_length = client.poll(5, &mut output).unwrap().unwrap();
        assert_eq!(client.state(), DhcpClientState::Renewing);
        assert!(!client.transmit().broadcast);
        assert_eq!(client.transmit().destination, server);
        assert_eq!(
            DhcpPacket::parse(&output[..renewal_length])
                .unwrap()
                .header
                .flags,
            0
        );
        assert!(client.poll(9, &mut output).unwrap().is_none());
        let rebind_length = client.poll(10, &mut output).unwrap().unwrap();
        assert_eq!(client.state(), DhcpClientState::Rebinding);
        assert!(client.transmit().broadcast);
        assert_eq!(client.transmit().destination, DHCP_BOOTSTRAP_DESTINATION);
        assert_eq!(
            DhcpPacket::parse(&output[..rebind_length])
                .unwrap()
                .header
                .flags,
            0x8000
        );
        assert!(client.poll(19, &mut output).unwrap().is_none());
        assert!(client.poll(20, &mut output).unwrap().is_none());
        assert_eq!(client.state(), DhcpClientState::Expired);
    }

    fn segment(
        source_port: u16,
        destination_port: u16,
        sequence: u32,
        acknowledgment: u32,
        flags: u16,
        data: &[u8],
    ) -> ([u8; 64], usize) {
        let segment = TcpSegment {
            source_port,
            destination_port,
            sequence,
            acknowledgment,
            flags,
            window: 512,
            checksum: 0,
            urgent: 0,
            data,
        };
        let mut bytes = [0u8; 64];
        let length = segment.encode(&mut bytes, IP_B, IP_A).unwrap();
        (bytes, length)
    }

    #[test]
    fn tcp_handshake_data_fin_and_retransmission() {
        let mut client = TcpConnection::<32>::active(IP_A, 1000, IP_B, 2000, 10, 5);
        let mut out = [0u8; 64];
        let syn_len = client.start_connect(100, &mut out).unwrap();
        assert_eq!(client.state(), TcpState::SynSent);
        assert!(client.retransmission_due(105));
        let (syn_ack_bytes, syn_ack_len) = segment(2000, 1000, 50, 11, TCP_SYN | TCP_ACK, &[]);
        let syn_ack = TcpSegment::parse(&syn_ack_bytes[..syn_ack_len], IP_B, IP_A, true).unwrap();
        let ack_len = client.on_segment(&syn_ack, 101, &mut out).unwrap().unwrap();
        assert_eq!(client.state(), TcpState::Established);
        let ack = TcpSegment::parse(&out[..ack_len], IP_A, IP_B, true).unwrap();
        assert_eq!(ack.flags, TCP_ACK);

        let data_len = client.send_data(b"ok", 110, &mut out).unwrap();
        assert_eq!(client.retransmission_deadline(), Some(115));
        let sent = TcpSegment::parse(&out[..data_len], IP_A, IP_B, true).unwrap();
        assert_eq!(sent.data, b"ok");
        assert!(client.retransmit(115, &mut out).unwrap().is_some());
        assert_eq!(client.retransmission_deadline(), Some(120));
        let (peer_ack_bytes, peer_ack_len) = segment(2000, 1000, 51, 13, TCP_ACK, &[]);
        let peer_ack =
            TcpSegment::parse(&peer_ack_bytes[..peer_ack_len], IP_B, IP_A, true).unwrap();
        assert_eq!(client.on_segment(&peer_ack, 111, &mut out).unwrap(), None);
        assert_eq!(client.retransmission_deadline(), None);

        let (incoming_bytes, incoming_len) = segment(2000, 1000, 51, 13, TCP_ACK, b"hi");
        let incoming =
            TcpSegment::parse(&incoming_bytes[..incoming_len], IP_B, IP_A, true).unwrap();
        client.on_segment(&incoming, 112, &mut out).unwrap();
        assert_eq!(client.received(), b"hi");
        let mut received = [0u8; 8];
        assert_eq!(client.take_received(&mut received), 2);
        assert_eq!(&received[..2], b"hi");

        let fin_len = client.close(120, &mut out).unwrap();
        assert_eq!(client.state(), TcpState::FinWait1);
        assert!(fin_len >= TcpSegment::MIN_HEADER_LEN);
        let (fin_ack_bytes, fin_ack_len) = segment(2000, 1000, 53, 14, TCP_FIN | TCP_ACK, &[]);
        let fin_ack = TcpSegment::parse(&fin_ack_bytes[..fin_ack_len], IP_B, IP_A, true).unwrap();
        client.on_segment(&fin_ack, 121, &mut out).unwrap();
        assert_eq!(client.state(), TcpState::Closed);
        let _ = syn_len;
    }

    #[test]
    fn tcp_listener_accepts_syn() {
        let mut server = TcpConnection::<16>::listen(IP_B, 2000, IP_A, 1000, 90, 4);
        let mut out = [0u8; 64];
        let (syn_bytes, syn_len) = segment(1000, 2000, 7, 0, TCP_SYN, &[]);
        let syn = TcpSegment::parse(&syn_bytes[..syn_len], IP_A, IP_B, true).unwrap();
        let length = server.on_segment(&syn, 1, &mut out).unwrap().unwrap();
        assert_eq!(server.state(), TcpState::SynReceived);
        let reply = TcpSegment::parse(&out[..length], IP_B, IP_A, true).unwrap();
        assert_eq!(reply.flags, TCP_SYN | TCP_ACK);
        let (ack_bytes, ack_len) = segment(1000, 2000, 8, 91, TCP_ACK, &[]);
        let ack = TcpSegment::parse(&ack_bytes[..ack_len], IP_A, IP_B, true).unwrap();
        server.on_segment(&ack, 2, &mut out).unwrap();
        assert_eq!(server.state(), TcpState::Established);
    }
}
