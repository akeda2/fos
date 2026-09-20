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
//! tape run router dhcp dhcp 2 health 1.1.1.1 3 10.0.3.1/24
//! tape run router dhcp-server 1 10.0.1.1/24 2 10.0.2.1/24
//! tape run router 1 10.0.1.1/24 2 10.0.2.1/24 default 1 10.0.1.254
//! ```
//!
//! `dhcp` configures NIC 1 (callback index zero); `dhcp N` adds an ordered
//! DHCP uplink. `health TARGET` verifies every DHCP uplink with fresh gateway
//! ARP plus ICMP echo, failing over after three missed cycles and failing back
//! to the highest-priority recovered lease. A trailing `default NIC GATEWAY`
//! selects a static egress when no DHCP uplink is used.
//! `dhcp-server` serves every configured interface except NIC 1, deriving a
//! pool from each subnet and allocating one in-memory lease slot per usable
//! address for this run.
//! NAT is enabled by default; `no-nat` disables source translation for
//! deployments where upstream routing already reaches downstream networks,
//! while `nat` explicitly enables it. Supplying both modes is invalid.
//! Static interfaces also accept the legacy `NIC IP NETMASK` form and the
//! separate `NIC IP /PREFIX` form.
//! Downstream TCP/UDP traffic leaving through the active DHCP uplink is source-NATed with a
//! physical-memory-backed connection table, so hosts on the upstream LAN can
//! reply without needing a route back to the downstream subnet. There is no
//! dynamic routing protocol, ICMP error generation, fragmentation, or IPv6.

use core::cmp::min;

use router_tape::{
    ConfigError, FileName, ForwardProtocol, InterfaceConfig, MAX_FORWARDS, PortForward,
    RouterConfig, MAX_INTERFACES, parse_config,
    route_next_hop,
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
const HEALTH_POLL_TICKS: u64 = 5 * 100;
const HEALTH_PROBE_TICKS: u64 = 50;
const HEALTH_MISSED_CYCLES: u8 = 3;
const NAT_FALLBACK_ENTRIES: usize = 32;
const NAT_DYNAMIC_BYTES: u32 = 4 * 1024 * 1024;
const NAT_TIMEOUT_TICKS: u64 = 60 * 100;
const LEASE_FILE_MAGIC: &[u8; 8] = b"FOSDHCP1";
const LEASE_FILE_VERSION: u16 = 1;
const LEASE_FILE_HEADER_SIZE: usize = 20;
const LEASE_FILE_RECORD_SIZE: usize = 16;
const LEASE_FILE_STATUS_OK: u32 = 0;
const LEASE_FILE_STATUS_NOT_FOUND: u32 = 1;
const LEASE_FILE_STATUS_BUFFER_TOO_SMALL: u32 = 3;

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
type TerminalPosition = extern "C" fn() -> u32;
type FsFileLength = extern "C" fn(u32, u32, u32) -> u32;
type FsRead = extern "C" fn(u32, u32, u32, u32, u32) -> u32;
type FsWrite = extern "C" fn(u32, u32, u32, u32) -> u32;
type FsRemove = extern "C" fn(u32, u32) -> u32;
type Ticks = extern "C" fn() -> u64;

struct RouterInterface {
    nic: u32,
    address: Ipv4Address,
    netmask: Ipv4Address,
    mac: MacAddress,
    arp: ArpCache<ARP_CACHE_SIZE>,
}

#[derive(Clone, Copy)]
struct UplinkHealth {
    interface: usize,
    gateway: Ipv4Address,
    misses: u8,
    healthy: bool,
    sequence: u16,
}

#[repr(u8)]
#[derive(Clone, Copy, Eq, PartialEq)]
enum NatState {
    Empty,
    Occupied,
    Deleted,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NatEntry {
    state: NatState,
    protocol: u8,
    inside_address: Ipv4Address,
    inside_port: u16,
    external_port: u16,
    remote_address: Ipv4Address,
    remote_port: u16,
    last_used: u64,
}

impl NatEntry {
    const fn empty() -> Self {
        Self {
            state: NatState::Empty,
            protocol: 0,
            inside_address: Ipv4Address::UNSPECIFIED,
            inside_port: 0,
            external_port: 0,
            remote_address: Ipv4Address::UNSPECIFIED,
            remote_port: 0,
            last_used: 0,
        }
    }
}

// The table is normally allocated from the tape's physical-memory pool. The
// fallback remains small so the router can still run on a machine whose E820
// map has no memory in the coarse big pool.
struct NatTable<'a> {
    entries: &'a mut [NatEntry],
    next_port: u16,
}

impl<'a> NatTable<'a> {
    fn new(entries: &'a mut [NatEntry]) -> Self {
        for entry in entries.iter_mut() {
            *entry = NatEntry::empty();
        }
        Self {
            entries,
            next_port: 49152,
        }
    }

    fn expire(&mut self, now: u64) {
        for entry in self.entries.iter_mut() {
            if entry.state == NatState::Occupied
                && now.wrapping_sub(entry.last_used) >= NAT_TIMEOUT_TICKS
            {
                entry.state = NatState::Deleted;
            }
        }
    }

    fn hash_connection(
        &self,
        protocol: u8,
        external_port: u16,
        remote_address: Ipv4Address,
        remote_port: u16,
    ) -> usize {
        let mut hash = u32::from(protocol);
        hash = hash
            .wrapping_mul(33)
            .wrapping_add(u32::from(external_port));
        hash = hash
            .wrapping_mul(33)
            .wrapping_add(address_to_u32(remote_address));
        hash = hash
            .wrapping_mul(33)
            .wrapping_add(u32::from(remote_port));
        hash as usize % self.entries.len()
    }

    fn find_inbound(
        &self,
        protocol: u8,
        external_port: u16,
        remote_address: Ipv4Address,
        remote_port: u16,
    ) -> Option<usize> {
        if self.entries.is_empty() {
            return None;
        }
        let start = self.hash_connection(protocol, external_port, remote_address, remote_port);
        for offset in 0..self.entries.len() {
            let index = (start + offset) % self.entries.len();
            let entry = &self.entries[index];
            match entry.state {
                NatState::Empty => return None,
                NatState::Deleted => {}
                NatState::Occupied
                    if entry.protocol == protocol
                        && entry.external_port == external_port
                        && entry.remote_address == remote_address
                        && entry.remote_port == remote_port =>
                {
                    return Some(index);
                }
                NatState::Occupied => {}
            }
        }
        None
    }

    fn find_slot(
        &self,
        protocol: u8,
        external_port: u16,
        remote_address: Ipv4Address,
        remote_port: u16,
    ) -> Option<usize> {
        if self.entries.is_empty() {
            return None;
        }
        let start = self.hash_connection(protocol, external_port, remote_address, remote_port);
        let mut deleted = None;
        for offset in 0..self.entries.len() {
            let index = (start + offset) % self.entries.len();
            match self.entries[index].state {
                NatState::Empty => return Some(deleted.unwrap_or(index)),
                NatState::Deleted => {
                    if deleted.is_none() {
                        deleted = Some(index);
                    }
                }
                NatState::Occupied => {}
            }
        }
        deleted
    }

    fn next_external_port(&mut self) -> u16 {
        let port = if self.next_port < 49152 {
            49152
        } else {
            self.next_port
        };
        self.next_port = self.next_port.wrapping_add(1);
        port
    }

    fn outbound(
        &mut self,
        protocol: u8,
        inside_address: Ipv4Address,
        inside_port: u16,
        remote_address: Ipv4Address,
        remote_port: u16,
        now: u64,
    ) -> Option<u16> {
        self.expire(now);
        if let Some(index) = self
            .find_inbound(protocol, inside_port, remote_address, remote_port)
            .filter(|index| self.entries[*index].inside_address == inside_address)
        {
            self.entries[index].last_used = now;
            return Some(self.entries[index].external_port);
        }
        if let Some(index) = self.entries.iter().position(|entry| {
            entry.state == NatState::Occupied
                && entry.inside_address == inside_address
                && entry.inside_port == inside_port
                && entry.remote_address == remote_address
                && entry.remote_port == remote_port
        }) {
            self.entries[index].last_used = now;
            return Some(self.entries[index].external_port);
        }

        let mut external_port = inside_port;
        for attempt in 0..=u16::MAX {
            if self
                .find_inbound(protocol, external_port, remote_address, remote_port)
                .is_none()
            {
                break;
            }
            if attempt == u16::MAX {
                return None;
            }
            external_port = self.next_external_port();
        }
        let slot = self.find_slot(protocol, external_port, remote_address, remote_port)?;
        self.entries[slot] = NatEntry {
            state: NatState::Occupied,
            protocol,
            inside_address,
            inside_port,
            external_port,
            remote_address,
            remote_port,
            last_used: now,
        };
        Some(external_port)
    }

    fn inbound(
        &mut self,
        protocol: u8,
        external_port: u16,
        remote_address: Ipv4Address,
        remote_port: u16,
        now: u64,
    ) -> Option<(Ipv4Address, u16)> {
        let index = self.find_inbound(protocol, external_port, remote_address, remote_port)?;
        let entry = &mut self.entries[index];
        entry.last_used = now;
        Some((entry.inside_address, entry.inside_port))
    }
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
    dirty: bool,
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
            dirty: false,
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
                self.dirty = true;
            }
        }
    }

    fn is_dirty(&self) -> bool {
        self.dirty
    }

    fn clear_dirty(&mut self) {
        self.dirty = false;
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
        self.dirty = true;
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
            self.dirty = true;
            Some(slot)
        })?;
        self.leases[index].state = LeaseState::Bound;
        self.leases[index].expires_at = now.wrapping_add(DHCP_LEASE_SECONDS);
        self.dirty = true;
        Some(index)
    }

    fn release(&mut self, mac: MacAddress, address: Ipv4Address) {
        if let Some(index) = self.find_mac(mac) {
            if address == Ipv4Address::UNSPECIFIED || self.address(index) == address {
                self.leases[index] = DhcpLeaseSlot::empty();
                self.dirty = true;
            }
        }
    }

    fn restore(&mut self, mac: MacAddress, address: Ipv4Address, now: u32) -> bool {
        let Some(index) = self.address_index(address) else {
            return false;
        };
        if self.leases[index].state != LeaseState::Empty || self.find_mac(mac).is_some() {
            return false;
        }
        self.leases[index] = DhcpLeaseSlot {
            state: LeaseState::Bound,
            mac,
            expires_at: now.wrapping_add(DHCP_LEASE_SECONDS),
        };
        self.dirty = true;
        true
    }
}

fn lease_file_capacity<'a>(
    servers: &[Option<DhcpServer<'a>>; MAX_INTERFACES],
) -> Option<usize> {
    servers
        .iter()
        .flatten()
        .try_fold(LEASE_FILE_HEADER_SIZE, |capacity, server| {
            capacity.checked_add(server.leases.len().checked_mul(LEASE_FILE_RECORD_SIZE)?)
        })
}

fn lease_checksum(bytes: &[u8]) -> u32 {
    let mut value = 2_166_136_261u32;
    for &byte in bytes {
        value = (value ^ u32::from(byte)).wrapping_mul(16_777_619);
    }
    value
}

fn lease_get_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn lease_get_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn lease_put_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn lease_put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn restore_lease_file<'a>(
    bytes: &[u8],
    interfaces: &[RouterInterface; MAX_INTERFACES],
    servers: &mut [Option<DhcpServer<'a>>; MAX_INTERFACES],
    now: u32,
) -> Result<usize, ()> {
    if bytes.len() < LEASE_FILE_HEADER_SIZE
        || &bytes[..8] != LEASE_FILE_MAGIC
        || lease_get_u16(bytes, 8) != LEASE_FILE_VERSION
        || lease_get_u16(bytes, 10) != LEASE_FILE_RECORD_SIZE as u16
    {
        return Err(());
    }
    let record_count = usize::try_from(lease_get_u32(bytes, 12)).map_err(|_| ())?;
    let expected_length = LEASE_FILE_HEADER_SIZE
        .checked_add(record_count.checked_mul(LEASE_FILE_RECORD_SIZE).ok_or(())?)
        .ok_or(())?;
    if expected_length != bytes.len() || lease_get_u32(bytes, 16) != lease_checksum(&bytes[20..]) {
        return Err(());
    }

    let mut restored = 0;
    for record in bytes[LEASE_FILE_HEADER_SIZE..].chunks_exact(LEASE_FILE_RECORD_SIZE) {
        let nic = lease_get_u32(record, 0);
        let address = address_from_u32(lease_get_u32(record, 4));
        let mut mac = [0u8; 6];
        mac.copy_from_slice(&record[8..14]);
        for (index, interface) in interfaces.iter().enumerate() {
            if interface.nic == nic {
                if let Some(server) = servers[index].as_mut() {
                    if server.restore(MacAddress::new(mac), address, now) {
                        restored += 1;
                    }
                }
                break;
            }
        }
    }
    for server in servers.iter_mut().flatten() {
        server.clear_dirty();
    }
    Ok(restored)
}

fn flush_lease_file<'a>(
    name: FileName,
    buffer_address: u32,
    capacity: usize,
    interfaces: &[RouterInterface; MAX_INTERFACES],
    servers: &mut [Option<DhcpServer<'a>>; MAX_INTERFACES],
    fs_write: FsWrite,
) -> u32 {
    if capacity > u32::MAX as usize {
        return LEASE_FILE_STATUS_BUFFER_TOO_SMALL;
    }
    let buffer = unsafe {
        core::slice::from_raw_parts_mut(buffer_address as *mut u8, capacity)
    };
    buffer.fill(0);
    buffer[..8].copy_from_slice(LEASE_FILE_MAGIC);
    lease_put_u16(buffer, 8, LEASE_FILE_VERSION);
    lease_put_u16(buffer, 10, LEASE_FILE_RECORD_SIZE as u16);
    let mut record_count = 0u32;
    let mut offset = LEASE_FILE_HEADER_SIZE;
    for (index, server) in servers.iter().enumerate() {
        let Some(server) = server.as_ref() else {
            continue;
        };
        for (lease_index, lease) in server.leases.iter().enumerate() {
            if lease.state != LeaseState::Bound {
                continue;
            }
            if offset + LEASE_FILE_RECORD_SIZE > capacity {
                return LEASE_FILE_STATUS_BUFFER_TOO_SMALL;
            }
            lease_put_u32(buffer, offset, interfaces[index].nic);
            lease_put_u32(buffer, offset + 4, address_to_u32(server.address(lease_index)));
            buffer[offset + 8..offset + 14].copy_from_slice(&lease.mac.0);
            offset += LEASE_FILE_RECORD_SIZE;
            record_count = record_count.saturating_add(1);
        }
    }
    lease_put_u32(buffer, 12, record_count);
    lease_put_u32(buffer, 16, lease_checksum(&buffer[LEASE_FILE_HEADER_SIZE..offset]));
    fs_write(
        name.as_bytes().as_ptr() as u32,
        name.len as u32,
        buffer_address,
        offset as u32,
    )
}

fn lease_servers_dirty<'a>(servers: &[Option<DhcpServer<'a>>; MAX_INTERFACES]) -> bool {
    servers.iter().flatten().any(DhcpServer::is_dirty)
}

fn persist_leases<'a>(
    putc: Putc,
    name: FileName,
    storage: &mut Option<(u32, usize)>,
    needs_flush: &mut bool,
    interfaces: &[RouterInterface; MAX_INTERFACES],
    servers: &mut [Option<DhcpServer<'a>>; MAX_INTERFACES],
    fs_write: FsWrite,
    free: Free,
) -> bool {
    let Some((address, capacity)) = *storage else {
        return false;
    };
    let status = flush_lease_file(name, address, capacity, interfaces, servers, fs_write);
    if status == LEASE_FILE_STATUS_OK {
        for server in servers.iter_mut().flatten() {
            server.clear_dirty();
        }
        *needs_flush = false;
        true
    } else {
        print_bytes(putc, b"DHCP lease persistence disabled after write error (status ");
        print_u32(putc, status);
        print_bytes(putc, b").\n");
        free(address, capacity as u32);
        *storage = None;
        false
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
        ConfigError::ConflictingNatMode => &b"nat and no-nat cannot both be configured"[..],
        ConfigError::MissingLeaseFileName => {
            &b"dhcp-lease-file must be followed by a FOSFS file name"[..]
        },
        ConfigError::BadLeaseFileName => &b"invalid DHCP lease file name"[..],
        ConfigError::LeaseFileRequiresDhcpServer => {
            &b"dhcp-lease-file requires dhcp-server"[..]
        },
        ConfigError::MissingForwardValue => {
            &b"forward must name protocol, external port, and internal IPv4 address"[..]
        }
        ConfigError::BadForwardProtocol => &b"forward protocol must be tcp or udp"[..],
        ConfigError::BadForwardPort => &b"forward ports must be decimal values from 1 through 65535"[..],
        ConfigError::BadForwardAddress => &b"forward internal address must be a unicast IPv4 address"[..],
        ConfigError::TooManyForwards => &b"at most 16 port-forward rules may be configured"[..],
        ConfigError::DuplicateForward => {
            &b"forward external ports and internal endpoints must be unique per protocol"[..]
        }
        ConfigError::ForwardTargetNotInternal => {
            &b"forward internal address is not on a configured non-primary subnet"[..]
        }
        ConfigError::TooManyDhcpUplinks => &b"at most four DHCP uplinks may be configured"[..],
        ConfigError::DuplicateDhcpUplink => &b"the same DHCP uplink was selected more than once"[..],
        ConfigError::MissingHealthTarget => &b"health must be followed by an IPv4 address"[..],
        ConfigError::BadHealthTarget => &b"health target must be a unicast IPv4 address"[..],
    };
    print_bytes(putc, b"Router configuration error: ");
    print_bytes(putc, message);
    print_bytes(
        putc,
        b". Usage: tape run router [dhcp [NIC]]... [health ADDRESS] [dhcp-server] [dhcp-lease-file NAME] [nat|no-nat] [dns SERVER [SERVER]] [forward tcp|udp EXTERNAL_PORT INTERNAL_IP [INTERNAL_PORT]] \
NIC IP NETMASK ... [default NIC GATEWAY] (or NIC IP/PREFIX)\n",
    );
}

fn print_startup(
    putc: Putc,
    interfaces: &[RouterInterface; MAX_INTERFACES],
    count: usize,
    default_route: Option<(u32, Ipv4Address)>,
    forwards: &[Option<PortForward>; MAX_FORWARDS],
    forward_count: usize,
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
    for forward in forwards[..forward_count].iter().flatten() {
        print_bytes(putc, b"  forward ");
        print_bytes(
            putc,
            match forward.protocol {
                ForwardProtocol::Tcp => b"tcp "[..].as_ref(),
                ForwardProtocol::Udp => b"udp "[..].as_ref(),
            },
        );
        print_u32(putc, u32::from(forward.external_port));
        print_bytes(putc, b" -> ");
        print_ip(putc, forward.internal_address);
        print_bytes(putc, b":");
        print_u32(putc, u32::from(forward.internal_port));
        print_bytes(putc, b" (NIC 1 external)\n");
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
    nic: u32,
    receive_buffer: &mut [u8; MAX_FRAME_LEN],
    max_frame_len: usize,
) -> Option<DhcpLease> {
    print_bytes(
        putc,
        b"DHCP: broadcasting DISCOVER and waiting for a lease...\n",
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
                    nic,
                    own_mac,
                    &payload[..length],
                    MacAddress::BROADCAST,
                    transmit.source,
                    transmit.destination,
                    DHCP_CLIENT_PORT,
                    DHCP_SERVER_PORT,
                    max_frame_len,
                ) {
                    print_bytes(putc, b"DHCP failed: selected NIC rejected the request frame.\n");
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
            print_bytes(putc,             b"DHCP lease acquired: ");
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
            nic_receive_at(nic, receive_buffer.as_mut_ptr() as u32, max_frame_len as u32) as usize;
        if length != 0 {
            receive_dhcp(
                &receive_buffer[..min(length, MAX_FRAME_LEN)],
                &mut client,
                now,
            );
        }
    }
}

fn internet_checksum(bytes: &[u8]) -> u16 {
        let mut sum = 0u32;
        let mut chunks = bytes.chunks_exact(2);
        for word in &mut chunks {
            sum += u32::from(u16::from_be_bytes([word[0], word[1]]));
        }
        if let Some(&byte) = chunks.remainder().first() {
            sum += u32::from(byte) << 8;
        }
        while sum >> 16 != 0 {
            sum = (sum & 0xffff) + (sum >> 16);
        }
        !(sum as u16)
    }

fn send_icmp_echo(
        interface: &RouterInterface,
        target: Ipv4Address,
        target_mac: MacAddress,
        sequence: u16,
        nic_send_at: NicSendAt,
        max_frame_len: usize,
    ) -> bool {
        let mut icmp = [0u8; 8];
        icmp[0] = 8;
        icmp[4..6].copy_from_slice(&(interface.nic as u16 + 1).to_be_bytes());
        icmp[6..8].copy_from_slice(&sequence.to_be_bytes());
        let checksum = internet_checksum(&icmp);
        icmp[2..4].copy_from_slice(&checksum.to_be_bytes());
        let packet = Ipv4Packet {
            header: Ipv4Header::new(interface.address, target, 1, icmp.len()),
            payload: &icmp,
        };
        let mut ip = [0u8; MAX_FRAME_LEN];
        let Ok(ip_len) = packet.encode(&mut ip) else {
            return false;
        };
        let ethernet = EthernetFrame::new(target_mac, interface.mac, ETHERTYPE_IPV4, &ip[..ip_len]);
        let mut frame = [0u8; MAX_FRAME_LEN];
        let Ok(length) = ethernet.encode(&mut frame) else {
            return false;
        };
        length <= max_frame_len
            && nic_send_at(interface.nic, frame.as_ptr() as u32, length as u32) != 0
    }

fn is_icmp_echo_reply(
        frame: &[u8],
        interface: &RouterInterface,
        target: Ipv4Address,
        sequence: u16,
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
        packet.header.protocol == 1
            && packet.header.source == target
            && packet.header.destination == interface.address
            && packet.payload.len() >= 8
            && packet.payload[0] == 0
            && packet.payload[1] == 0
            && internet_checksum(packet.payload) == 0
            && packet.payload[4..6] == (interface.nic as u16 + 1).to_be_bytes()
            && packet.payload[6..8] == sequence.to_be_bytes()
    }

#[allow(clippy::too_many_arguments)]
fn poll_uplink_health(
        health: &mut UplinkHealth,
        target: Ipv4Address,
        interfaces: &mut [RouterInterface; MAX_INTERFACES],
        count: usize,
        receive_buffer: &mut [u8; MAX_FRAME_LEN],
        nic_send_at: NicSendAt,
        nic_receive_at: NicReceiveAt,
        should_abort: ShouldAbort,
        ticks: Ticks,
        max_frame_len: usize,
    ) -> bool {
        let interface = health.interface;
        // Each probe requires a fresh gateway answer, not merely a still-valid
        // cache entry from a previous cycle.
        interfaces[interface].arp.clear();
        let _ = send_arp_request(&interfaces[interface], health.gateway, nic_send_at, max_frame_len);
        let deadline = ticks().wrapping_add(HEALTH_PROBE_TICKS);
        let mut gateway_mac = None;
        while ticks().wrapping_sub(deadline) >= (1u64 << 63) {
            if should_abort() == TAPE_EVENT_ABORT {
                return false;
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
                process_arp(
                    &mut interfaces[index],
                    &receive_buffer[..min(length, MAX_FRAME_LEN)],
                    ticks(),
                    nic_send_at,
                    max_frame_len,
                );
            }
            gateway_mac = interfaces[interface]
                .arp
                .lookup(health.gateway, ticks() as u32);
            if gateway_mac.is_some() {
                break;
            }
        }
        let Some(gateway_mac) = gateway_mac else {
            return false;
        };
        health.sequence = health.sequence.wrapping_add(1);
        if !send_icmp_echo(
            &interfaces[interface],
            target,
            gateway_mac,
            health.sequence,
            nic_send_at,
            max_frame_len,
        ) {
            return false;
        }
        let deadline = ticks().wrapping_add(HEALTH_PROBE_TICKS);
        while ticks().wrapping_sub(deadline) >= (1u64 << 63) {
            if should_abort() == TAPE_EVENT_ABORT {
                return false;
            }
            let length = nic_receive_at(
                interfaces[interface].nic,
                receive_buffer.as_mut_ptr() as u32,
                max_frame_len as u32,
            ) as usize;
            if length == 0 {
                continue;
            }
            let frame = &receive_buffer[..min(length, MAX_FRAME_LEN)];
            process_arp(&mut interfaces[interface], frame, ticks(), nic_send_at, max_frame_len);
            if is_icmp_echo_reply(frame, &interfaces[interface], target, health.sequence) {
                return true;
            }
        }
        false
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

fn transport_ports(payload: &[u8], protocol: u8) -> Option<(u16, u16)> {
    let header_len = match protocol {
        6 => 20,
        17 => 8,
        _ => return None,
    };
    if payload.len() < header_len {
        return None;
    }
    Some((
        u16::from_be_bytes([payload[0], payload[1]]),
        u16::from_be_bytes([payload[2], payload[3]]),
    ))
}

fn checksum_replace(checksum: u16, old: u16, new: u16) -> u16 {
    let mut sum = u32::from(!checksum) + u32::from(!old) + u32::from(new);
    while (sum >> 16) != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

// TCP and UDP checksums include the IPv4 pseudo-header. Recalculate only the
// changed address and port words so transport options and payload bytes stay
// untouched while NAT rewrites the packet.
fn rewrite_transport(
    payload: &mut [u8],
    protocol: u8,
    old_address: Ipv4Address,
    new_address: Ipv4Address,
    old_port: u16,
    new_port: u16,
    source: bool,
) -> bool {
    let (checksum_offset, port_offset) = match protocol {
        6 if payload.len() >= 20 => (16, if source { 0 } else { 2 }),
        17 if payload.len() >= 8 => (6, if source { 0 } else { 2 }),
        _ => return false,
    };
    let checksum =
        u16::from_be_bytes([payload[checksum_offset], payload[checksum_offset + 1]]);
    let port = u16::from_be_bytes([payload[port_offset], payload[port_offset + 1]]);
    if port != old_port {
        return false;
    }
    payload[port_offset..port_offset + 2].copy_from_slice(&new_port.to_be_bytes());
    if checksum != 0 {
        let mut updated = checksum;
        updated = checksum_replace(
            updated,
            u16::from_be_bytes([old_address.0[0], old_address.0[1]]),
            u16::from_be_bytes([new_address.0[0], new_address.0[1]]),
        );
        updated = checksum_replace(
            updated,
            u16::from_be_bytes([old_address.0[2], old_address.0[3]]),
            u16::from_be_bytes([new_address.0[2], new_address.0[3]]),
        );
        updated = checksum_replace(updated, old_port, new_port);
        let updated = if protocol == 17 && updated == 0 {
            0xffff
        } else {
            updated
        };
        payload[checksum_offset..checksum_offset + 2].copy_from_slice(&updated.to_be_bytes());
    }
    true
}

fn forward_protocol(protocol: u8) -> Option<ForwardProtocol> {
    match protocol {
        6 => Some(ForwardProtocol::Tcp),
        17 => Some(ForwardProtocol::Udp),
        _ => None,
    }
}

fn inbound_forward(
    forwards: &[Option<PortForward>; MAX_FORWARDS],
    count: usize,
    protocol: u8,
    external_port: u16,
) -> Option<PortForward> {
    let protocol = forward_protocol(protocol)?;
    forwards[..count]
        .iter()
        .flatten()
        .copied()
        .find(|forward| forward.protocol == protocol && forward.external_port == external_port)
}

fn reply_forward(
    forwards: &[Option<PortForward>; MAX_FORWARDS],
    count: usize,
    protocol: u8,
    internal_address: Ipv4Address,
    internal_port: u16,
) -> Option<PortForward> {
    let protocol = forward_protocol(protocol)?;
    forwards[..count]
        .iter()
        .flatten()
        .copied()
        .find(|forward| {
            forward.protocol == protocol
                && forward.internal_address == internal_address
                && forward.internal_port == internal_port
        })
}

fn forward_ipv4(
    ingress: usize,
    frame: &[u8],
    interfaces: &mut [RouterInterface; MAX_INTERFACES],
    configs: &[Option<InterfaceConfig>; MAX_INTERFACES],
    count: usize,
    default_route: Option<router_tape::DefaultRoute>,
    active_uplink: u32,
    nic_send_at: NicSendAt,
    nic_receive_at: NicReceiveAt,
    should_abort: ShouldAbort,
    ticks: Ticks,
    nat: &mut NatTable,
    nat_enabled: bool,
    forwards: &[Option<PortForward>; MAX_FORWARDS],
    forward_count: usize,
    receive_buffer: &mut [u8; MAX_FRAME_LEN],
    transport_buffer: &mut [u8; MAX_FRAME_LEN],
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
    let now = ticks();
    let mut header = packet.header;
    let payload_len = packet.payload.len();
    let mut payload_in_transport_buffer = false;
    if interfaces[ingress].nic == active_uplink
        && packet.header.destination == interfaces[ingress].address
    {
        let Some((source_port, destination_port)) =
            transport_ports(packet.payload, packet.header.protocol)
        else {
            return ForwardResult::Dropped;
        };
        let translation = if let Some(forward) =
            inbound_forward(forwards, forward_count, packet.header.protocol, destination_port)
        {
            Some((forward.internal_address, forward.internal_port))
        } else if nat_enabled {
            nat.inbound(
                packet.header.protocol,
                destination_port,
                packet.header.source,
                source_port,
                now,
            )
        } else {
            None
        };
        let Some((inside_address, inside_port)) = translation else {
            return ForwardResult::Dropped;
        };
        transport_buffer[..payload_len].copy_from_slice(packet.payload);
        if !rewrite_transport(
            &mut transport_buffer[..payload_len],
            packet.header.protocol,
            packet.header.destination,
            inside_address,
            destination_port,
            inside_port,
            false,
        ) {
            return ForwardResult::Dropped;
        }
        header.destination = inside_address;
        payload_in_transport_buffer = true;
    }
    interfaces[ingress]
        .arp
        .insert(header.source, ethernet.source, now as u32);
    if is_local(interfaces, count, header.destination)
        || is_broadcast_or_multicast(header.destination)
    {
        return ForwardResult::Dropped;
    }
    let Some((egress, next_hop)) =
        route_next_hop(configs, count, header.destination, default_route)
    else {
        return ForwardResult::Dropped;
    };
    if egress == ingress || header.ttl <= 1 {
        return ForwardResult::Dropped;
    }
    let mut payload_rewritten = payload_in_transport_buffer;
    if interfaces[ingress].nic != active_uplink && interfaces[egress].nic == active_uplink {
        if let Some((source_port, destination_port)) =
            transport_ports(packet.payload, header.protocol)
        {
            let static_forward =
                reply_forward(forwards, forward_count, header.protocol, header.source, source_port);
            let external_port = if let Some(forward) = static_forward {
                forward.external_port
            } else if nat_enabled {
                let Some(port) = nat.outbound(
                    header.protocol,
                    header.source,
                    source_port,
                    header.destination,
                    destination_port,
                    now,
                ) else {
                    return ForwardResult::Dropped;
                };
                port
            } else {
                0
            };
            if static_forward.is_some() || nat_enabled {
                if !payload_rewritten {
                    transport_buffer[..payload_len].copy_from_slice(packet.payload);
                }
                if !rewrite_transport(
                    &mut transport_buffer[..payload_len],
                    header.protocol,
                    header.source,
                    interfaces[egress].address,
                    source_port,
                    external_port,
                    true,
                ) {
                    return ForwardResult::Dropped;
                }
                payload_rewritten = true;
                header.source = interfaces[egress].address;
            }
        }
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
    header.ttl -= 1;
    let forwarded = Ipv4Packet {
        header,
        payload: if payload_rewritten {
            &transport_buffer[..payload_len]
        } else {
            packet.payload
        },
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
    _terminal_position: TerminalPosition,
    fs_file_length: FsFileLength,
    fs_read: FsRead,
    fs_write: FsWrite,
    _fs_remove: FsRemove,
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
    if parsed.dhcp_uplinks[..parsed.dhcp_uplink_count].iter().flatten().any(|nic| *nic as usize >= nic_total) {
        print_bytes(putc, b"Router configuration error: a DHCP NIC is not present.\n");
        return;
    }
    let max_frame_len = min(nic_max_frame_len() as usize, MAX_FRAME_LEN);
    if max_frame_len < 60 {
        print_bytes(putc, b"Router cannot start: NIC frame limit is too small.\n");
        return;
    }

    let mut interfaces = [const { RouterInterface::empty() }; MAX_INTERFACES];
    let mut dhcp_servers: [Option<DhcpServer<'_>>; MAX_INTERFACES] =
        core::array::from_fn(|_| None);
    let mut dhcp_allocations = [None; MAX_INTERFACES];
    let mut configs: [Option<InterfaceConfig>; MAX_INTERFACES] = [None; MAX_INTERFACES];
    let mut uplinks: [Option<UplinkHealth>; MAX_INTERFACES] = [None; MAX_INTERFACES];
    let mut interface_count = 0;
    let mut dns_servers = [Ipv4Address::UNSPECIFIED; DHCP_MAX_DNS_SERVERS];
    let mut dns_server_count = parsed.dns_server_count;
    for (index, server) in parsed.dns_servers.iter().flatten().enumerate() {
        dns_servers[index] = *server;
    }
    let mut receive_buffer = [0u8; MAX_FRAME_LEN];

    for (priority, nic) in parsed.dhcp_uplinks[..parsed.dhcp_uplink_count].iter().flatten().enumerate() {
        let mut mac = [0u8; 6];
        if nic_mac_at(*nic, mac.as_mut_ptr() as u32) == 0 {
            print_bytes(putc, b"Router cannot start: failed to read a DHCP NIC MAC.\n");
            return;
        }
        let own_mac = MacAddress::new(mac);
        let Some(lease) = configure_dhcp(putc, should_abort, nic_send_at, nic_receive_at, ticks, own_mac, *nic, &mut receive_buffer, max_frame_len) else {
            print_bytes(putc, b"Router cannot start: DHCP uplink did not acquire a lease.\n");
            return;
        };
        if lease.router == Ipv4Address::UNSPECIFIED {
            print_bytes(putc, b"Router cannot start: DHCP uplink supplied no gateway.\n");
            return;
        }
        interfaces[interface_count] = RouterInterface { nic: *nic, address: lease.address, netmask: lease.subnet_mask, mac: own_mac, arp: ArpCache::new() };
        configs[interface_count] = Some(InterfaceConfig { nic: *nic, address: lease.address, netmask: lease.subnet_mask });
        uplinks[priority] = Some(UplinkHealth { interface: interface_count, gateway: lease.router, misses: 0, healthy: true, sequence: 0 });
        interface_count += 1;
        if dns_server_count == 0 && priority == 0 {
            dns_server_count = lease.dns_server_count;
            dns_servers[..dns_server_count].copy_from_slice(&lease.dns_servers[..dns_server_count]);
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
            if parsed.dhcp_uplinks[..parsed.dhcp_uplink_count]
                .iter().flatten().any(|uplink| *uplink == interface.nic) {
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
    let mut lease_file_storage: Option<(u32, usize)> = None;
    let mut lease_file_needs_flush = false;
    if let Some(lease_file) = parsed.dhcp_lease_file {
        match lease_file_capacity(&dhcp_servers) {
            Some(capacity) => match u32::try_from(capacity) {
                Ok(capacity_u32) => {
                    let buffer_address = alloc(capacity_u32);
                    if buffer_address == 0 {
                        print_bytes(
                            putc,
                            b"DHCP lease persistence disabled: insufficient memory for the lease file.\n",
                        );
                    } else {
                        lease_file_storage = Some((buffer_address, capacity));
                        let mut file_length = 0u64;
                        match fs_file_length(
                            lease_file.as_bytes().as_ptr() as u32,
                            lease_file.len as u32,
                            (&mut file_length as *mut u64) as u32,
                        ) {
                            LEASE_FILE_STATUS_NOT_FOUND => {
                                print_bytes(
                                    putc,
                                    b"DHCP lease persistence enabled; no existing lease file.\n",
                                );
                            }
                            LEASE_FILE_STATUS_OK
                                if file_length <= u64::from(capacity_u32) =>
                            {
                                let mut read_length = 0u32;
                                let status = fs_read(
                                    lease_file.as_bytes().as_ptr() as u32,
                                    lease_file.len as u32,
                                    buffer_address,
                                    capacity_u32,
                                    (&mut read_length as *mut u32) as u32,
                                );
                                if status == LEASE_FILE_STATUS_OK {
                                    let bytes = unsafe {
                                        core::slice::from_raw_parts(
                                            buffer_address as *const u8,
                                            read_length as usize,
                                        )
                                    };
                                    match restore_lease_file(
                                        bytes,
                                        &interfaces,
                                        &mut dhcp_servers,
                                        (ticks() / 100) as u32,
                                    ) {
                                        Ok(restored) => {
                                            print_bytes(
                                                putc,
                                                b"DHCP lease persistence restored ",
                                            );
                                            print_u32(putc, restored as u32);
                                            print_bytes(putc, b" lease(s).\n");
                                        }
                                        Err(()) => {
                                            print_bytes(
                                                putc,
                                                b"DHCP lease file was invalid; starting with empty tables.\n",
                                            );
                                            lease_file_needs_flush = true;
                                        }
                                    }
                                } else {
                                    print_bytes(
                                        putc,
                                        b"DHCP lease persistence disabled: could not read the lease file.\n",
                                    );
                                    free(buffer_address, capacity_u32);
                                    lease_file_storage = None;
                                }
                            }
                            LEASE_FILE_STATUS_OK => {
                                print_bytes(
                                    putc,
                                    b"DHCP lease file is too large; starting with empty tables.\n",
                                );
                                lease_file_needs_flush = true;
                            }
                            _ => {
                                print_bytes(
                                    putc,
                                    b"DHCP lease persistence disabled: disk is unavailable.\n",
                                );
                                free(buffer_address, capacity_u32);
                                lease_file_storage = None;
                            }
                        }
                    }
                }
                Err(_) => {
                    print_bytes(
                        putc,
                        b"DHCP lease persistence disabled: lease buffer is too large.\n",
                    );
                }
            },
            None => {
                print_bytes(
                    putc,
                    b"DHCP lease persistence disabled: lease buffer size overflowed.\n",
                );
            }
        }
    } else if parsed.dhcp_server {
        print_bytes(
            putc,
            b"DHCP lease persistence disabled; leases are memory-only.\n",
        );
    }
    let lease_file = parsed.dhcp_lease_file;
    let mut active_uplink = uplinks[0].map(|uplink| uplink.interface);
    // Run the first health sweep before accepting ordinary forwarding, rather
    // than assuming the first configured lease is reachable for one interval.
    let mut last_health_poll = ticks().wrapping_sub(HEALTH_POLL_TICKS);
    if lease_file_needs_flush {
        if let Some(name) = lease_file {
            let _ = persist_leases(
                putc,
                name,
                &mut lease_file_storage,
                &mut lease_file_needs_flush,
                &interfaces,
                &mut dhcp_servers,
                fs_write,
                free,
            );
        }
    }
    let _ = nic_set_promiscuous_at;
    print_startup(
        putc,
        &interfaces,
        interface_count,
        active_uplink
            .map(|index| (interfaces[index].nic, uplinks.iter().flatten().find(|uplink| uplink.interface == index).unwrap().gateway))
            .or(parsed.default_route.map(|route| (route.nic, route.gateway))),
        &parsed.forwards,
        parsed.forward_count,
    );
    if parsed.nat {
        print_bytes(
            putc,
            b"NAT: TCP/UDP masquerading enabled through the active DHCP uplink.\n",
        );
    } else {
        print_bytes(
            putc,
            b"NAT: disabled; upstream routing must reach downstream networks.\n",
        );
    }
    let mut arp_receive_buffer = [0u8; MAX_FRAME_LEN];
    let mut fallback_nat_entries = [NatEntry::empty(); NAT_FALLBACK_ENTRIES];
    let nat_allocation = if parsed.nat {
        alloc(NAT_DYNAMIC_BYTES)
    } else {
        0
    };
    let mut nat = if nat_allocation != 0 {
        let entry_count = NAT_DYNAMIC_BYTES as usize / core::mem::size_of::<NatEntry>();
        let entries = unsafe {
            core::slice::from_raw_parts_mut(nat_allocation as *mut NatEntry, entry_count)
        };
        print_bytes(putc, b"NAT: allocated ");
        print_u32(putc, entry_count as u32);
        print_bytes(putc, b" flow slots from physical memory.\n");
        NatTable::new(entries)
    } else {
        print_bytes(
            putc,
            b"NAT: physical-memory allocation unavailable; using 32 flow slots.\n",
        );
        NatTable::new(&mut fallback_nat_entries)
    };
    let mut ip_buffer = [0u8; MAX_FRAME_LEN];
    let mut transport_buffer = [0u8; MAX_FRAME_LEN];
    let mut output_buffer = [0u8; MAX_FRAME_LEN];
    let mut forwarded = 0u32;
    let mut dropped = 0u32;

    'router: loop {
        if parsed.health_target.is_some()
            && parsed.dhcp_uplink_count != 0
            && ticks().wrapping_sub(last_health_poll) >= HEALTH_POLL_TICKS
        {
            last_health_poll = ticks();
            let previous = active_uplink;
            for uplink in uplinks[..parsed.dhcp_uplink_count].iter_mut().flatten() {
                let target = parsed.health_target.unwrap();
                if poll_uplink_health(
                    uplink,
                    target,
                    &mut interfaces,
                    interface_count,
                    &mut arp_receive_buffer,
                    nic_send_at,
                    nic_receive_at,
                    should_abort,
                    ticks,
                    max_frame_len,
                ) {
                    uplink.misses = 0;
                    uplink.healthy = true;
                } else {
                    uplink.misses = uplink.misses.saturating_add(1);
                    if uplink.misses >= HEALTH_MISSED_CYCLES {
                        uplink.healthy = false;
                    }
                }
            }
            active_uplink = uplinks[..parsed.dhcp_uplink_count]
                .iter()
                .flatten()
                .find(|uplink| uplink.healthy)
                .map(|uplink| uplink.interface);
            if active_uplink != previous {
                match active_uplink {
                    Some(index) => {
                        print_bytes(putc, b"Uplink failover: active NIC ");
                        print_u32(putc, interfaces[index].nic + 1);
                        print_bytes(putc, b". Existing NAT flows may break.\n");
                    }
                    None => print_bytes(putc, b"Uplink failover: no healthy DHCP lease. Existing NAT flows may break.\n"),
                }
            }
        }
        let current_default = active_uplink.and_then(|index| {
            uplinks[..parsed.dhcp_uplink_count]
                .iter()
                .flatten()
                .find(|uplink| uplink.interface == index)
                .map(|uplink| router_tape::DefaultRoute {
                    nic: interfaces[index].nic,
                    gateway: uplink.gateway,
                })
        }).or(parsed.default_route);
        let active_index = active_uplink
            .or_else(|| current_default.and_then(|route| {
                interfaces[..interface_count].iter().position(|interface| interface.nic == route.nic)
            }))
            .unwrap_or(0);
        let active_nic = interfaces[active_index].nic;
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
            if parsed.dhcp_server && !parsed.dhcp_uplinks[..parsed.dhcp_uplink_count]
                .iter().flatten().any(|uplink| *uplink == interfaces[ingress].nic) {
                let mut lease_changed = false;
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
                    lease_changed = server.is_dirty();
                }
                if lease_changed || lease_file_needs_flush {
                    if let Some(name) = lease_file {
                        let _ = persist_leases(
                            putc,
                            name,
                            &mut lease_file_storage,
                            &mut lease_file_needs_flush,
                            &interfaces,
                            &mut dhcp_servers,
                            fs_write,
                            free,
                        );
                    }
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
                current_default,
                active_nic,
                nic_send_at,
                nic_receive_at,
                should_abort,
                ticks,
                &mut nat,
                parsed.nat,
                &parsed.forwards,
                parsed.forward_count,
                &mut arp_receive_buffer,
                &mut transport_buffer,
                &mut ip_buffer,
                &mut output_buffer,
                max_frame_len,
            ) {
                ForwardResult::Forwarded => forwarded += 1,
                ForwardResult::Dropped => dropped += 1,
            }
        }
    }

    if lease_file_storage.is_some() && (lease_file_needs_flush || lease_servers_dirty(&dhcp_servers))
    {
        if let Some(name) = lease_file {
            let _ = persist_leases(
                putc,
                name,
                &mut lease_file_storage,
                &mut lease_file_needs_flush,
                &interfaces,
                &mut dhcp_servers,
                fs_write,
                free,
            );
        }
    }
    for server in &mut dhcp_servers {
        *server = None;
    }
    if let Some((address, capacity)) = lease_file_storage {
        free(address, capacity as u32);
    }
    free_dhcp_allocations(&mut dhcp_allocations, free);
    drop(nat);
    if nat_allocation != 0 {
        free(nat_allocation, NAT_DYNAMIC_BYTES);
    }
    print_bytes(putc, b"Router stopped; forwarded ");
    print_u32(putc, forwarded);
    print_bytes(putc, b" IPv4 frame(s), dropped ");
    print_u32(putc, dropped);
    print_bytes(putc, b".\n");
}
