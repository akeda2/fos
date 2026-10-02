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
//! `dhcp-server` serves every configured interface except the DHCP uplinks, deriving a
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
    ConfigError, FileName, ForwardProtocol, InterfaceConfig, MAX_FORWARDS, MAX_INTERFACES,
    PortForward, RouterConfig, parse_config, route_next_hop, routing_configs_for_active_uplink,
};
use tape_net::{
    ArpCache, ArpOperation, ArpPacket, DHCP_CLIENT_PORT, DHCP_MAX_DNS_SERVERS,
    DHCP_OPTION_BROADCAST_ADDRESS, DHCP_OPTION_DNS, DHCP_OPTION_LEASE_TIME,
    DHCP_OPTION_MESSAGE_TYPE, DHCP_OPTION_REBINDING_TIME, DHCP_OPTION_RENEWAL_TIME,
    DHCP_OPTION_REQUESTED_IP, DHCP_OPTION_ROUTER, DHCP_OPTION_SERVER_IDENTIFIER,
    DHCP_OPTION_SUBNET_MASK, DHCP_SERVER_PORT, DhcpClient, DhcpClientState,
    DhcpMessageType, DhcpOp, DhcpOptionWriter, DhcpOptions, DhcpPacket, ETHERTYPE_ARP,
    ETHERTYPE_IPV4, EthernetFrame, Ipv4Address, Ipv4Header, Ipv4Packet, MacAddress, UdpDatagram,
};
use tape_rt::{print_bytes, print_u32};

const MAX_FRAME_LEN: usize = 1514;
// How long resolve_mac waits before resending an ARP request for the same
// still-unresolved (egress, target) pair. 100 ticks (~1s at this kernel's
// 100 Hz PIT) is a deliberately short retry, not a giving-up deadline --
// resolve_mac never blocks or gives up itself; a destination that never
// answers just keeps missing the cache and dropping packets, the same as
// an ordinary IP stack's incomplete ARP entry would.
const ARP_RETRY_TICKS: u64 = 100;
const ARP_CACHE_SIZE: usize = 8;
// Minimum spacing between synchronous DHCP lease-file writes triggered from
// inside the per-ingress forwarding loop. persist_leases performs a
// blocking FOSFS fs_write, stalling every interface's forwarding for the
// write's duration; without this, a burst of simultaneous lease requests
// (e.g. many clients booting at once) could trigger one such write per
// DHCP transaction. 100 ticks (~1s at 100 Hz) batches that burst into
// occasional writes instead, while the loop's own dirty-flag bookkeeping
// (server.is_dirty()/lease_file_needs_flush) still guarantees a pending
// change is eventually flushed -- both from a later loop iteration once
// the interval has passed, and unconditionally once the router exits.
const LEASE_FLUSH_MIN_TICKS: u64 = 100;
// Ipv4Header::flags_fragment layout (RFC 791): bit 13 (0x2000) is the
// more-fragments flag, and the low 13 bits are the fragment offset in
// 8-byte units. Either one being nonzero means this packet is not a
// complete, unfragmented datagram.
const IPV4_FLAG_MORE_FRAGMENTS: u16 = 0x2000;
const IPV4_FRAGMENT_OFFSET_MASK: u16 = 0x1fff;
const BROADCAST_IP: Ipv4Address = Ipv4Address([255, 255, 255, 255]);
// One bounded DhcpClient retry cycle (DHCP_RETRY_SECONDS between attempts,
// DHCP_MAX_RETRIES attempts) exhausts itself and reports `Failed` in well
// under DHCP_RETRY_SECONDS * DHCP_MAX_RETRIES seconds -- there is no
// exponential backoff, just a fixed interval. The overall timeout must be
// large enough for configure_dhcp to restart several fresh cycles (see
// below), not just survive one: a switch still settling STP/link state, or
// a first DISCOVER burst simply getting lost, is a transient condition a
// second attempt usually clears, not a reason to abandon startup.
const DHCP_TIMEOUT_SECONDS: u32 = 30;
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
const NAT_PORT_FIRST: u16 = 49_152;
const NAT_PORT_COUNT: u16 = u16::MAX - NAT_PORT_FIRST + 1;
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
    // The active uplink health round's own confirmed (ip, mac) pair for this
    // interface's default gateway, held outside the general ArpCache. That
    // shared cache has only ARP_CACHE_SIZE entries and inserts one for every
    // ARP frame this interface merely observes -- on a busy real-world LAN
    // segment (unrelated neighbors' broadcasts, gratuitous ARPs, ...) that
    // was enough to evict the gateway's own entry via ordinary LRU pressure
    // between health rounds, forcing forward_ipv4's very next off-subnet
    // packet into a fresh, blocking resolve_mac call -- reintroducing the
    // same multi-second stalls health's non-blocking redesign was meant to
    // eliminate, confirmed on real hardware. Pinning the gateway's own MAC
    // here, refreshed every health round by advance_uplink_health, keeps it
    // immune to that unrelated cache churn.
    gateway_pin: Option<(Ipv4Address, MacAddress)>,
    // A cache-miss target resolve_mac is currently waiting on a reply for,
    // and the tick before which it will not resend the ARP request for that
    // same target. resolve_mac never blocks: on a miss it fires at most one
    // request per ARP_RETRY_TICKS window and drops the packet that
    // triggered it, exactly like an ordinary IP stack drops (rather than
    // waits on) a packet that misses its ARP cache. The reply -- like any
    // other frame -- is picked up by the main loop's own unconditional
    // process_arp call on its next read, populating `arp` in time for later
    // packets to the same destination.
    pending_arp: Option<Ipv4Address>,
    pending_arp_retry: u64,
}

#[derive(Clone, Copy)]
struct UplinkHealth {
    interface: usize,
    gateway: Ipv4Address,
    misses: u8,
    healthy: bool,
    sequence: u16,
    phase: HealthPhase,
    deadline: u64,
}

/// Non-blocking health-probe state for one uplink. `Idle` means no probe is
/// currently in flight (either none has started this cycle yet, or the
/// previous one finished/timed out); `AwaitingArp`/`AwaitingIcmp` track which
/// reply `advance_uplink_health` is still waiting to see, with `deadline`
/// giving `expire_health_timeouts` a point at which to give up.
#[derive(Clone, Copy, PartialEq, Eq)]
enum HealthPhase {
    Idle,
    AwaitingArp,
    AwaitingIcmp,
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
    fn new(entries: &'a mut [NatEntry], port_seed: u64) -> Self {
        for entry in entries.iter_mut() {
            *entry = NatEntry::empty();
        }
        Self {
            entries,
            // Keep the source port in the IANA dynamic range, and vary the
            // starting point for every router run. A fixed-source-port
            // client reconnecting after a router restart otherwise recreates
            // the exact external TCP tuple while a remote server may still
            // retain its old connection state.
            next_port: NAT_PORT_FIRST + (port_seed as u16 % NAT_PORT_COUNT),
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
        let port = self.next_port;
        self.next_port = if port == u16::MAX {
            NAT_PORT_FIRST
        } else {
            port + 1
        };
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
    ) -> Option<(u16, bool)> {
        self.expire(now);
        // Look for an existing mapping for this exact internal connection
        // (inside_address/inside_port/remote_address/remote_port), so a
        // flow reuses its already-assigned external_port instead of getting
        // a second, redundant one. This used to have a hash-indexed "fast
        // path" ahead of this linear scan that called
        // find_inbound(protocol, inside_port, remote_address, remote_port)
        // -- but find_inbound's second parameter is external_port, not
        // inside_port, so that call actually searched for an entry whose
        // *external* port happened to equal this connection's *internal*
        // port. Two unrelated downstream hosts opening connections to the
        // same remote address/port could then alias onto each other's NAT
        // mapping whenever one's inside_port coincided with the other's
        // already-assigned external_port -- a real, if narrow, correctness
        // bug, not just a missed optimization. The correct check is this
        // linear scan (already present as a second, working fallback); the
        // buggy fast path added nothing but the aliasing risk and was
        // removed rather than fixed, since a linear scan over this small,
        // bounded NAT table is already cheap.
        if let Some(index) = self.entries.iter().position(|entry| {
            entry.state == NatState::Occupied
                && entry.protocol == protocol
                && entry.inside_address == inside_address
                && entry.inside_port == inside_port
                && entry.remote_address == remote_address
                && entry.remote_port == remote_port
        }) {
            self.entries[index].last_used = now;
            return Some((self.entries[index].external_port, false));
        }

        let mut external_port = self.next_external_port();
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
        Some((external_port, true))
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

#[derive(Clone, Copy)]
enum DhcpLeaseEvent {
    Offered {
        mac: MacAddress,
        address: Ipv4Address,
    },
    Bound {
        mac: MacAddress,
        address: Ipv4Address,
    },
    Released {
        mac: MacAddress,
        address: Ipv4Address,
        declined: bool,
    },
    Nak {
        mac: MacAddress,
    },
    SendFailed,
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

    fn allocate(&mut self, mac: MacAddress, now: u32) -> Option<(usize, bool)> {
        self.expire(now);
        if let Some(index) = self.find_mac(mac) {
            return Some((index, false));
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
        Some((slot, true))
    }

    fn requested(
        &mut self,
        mac: MacAddress,
        requested: Option<Ipv4Address>,
        client_address: Ipv4Address,
        start: u32,
        end: u32,
        now: u32,
    ) -> Option<(usize, bool)> {
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
        let newly_bound = self.leases[index].state != LeaseState::Bound;
        self.leases[index].state = LeaseState::Bound;
        self.leases[index].expires_at = now.wrapping_add(DHCP_LEASE_SECONDS);
        self.dirty = true;
        Some((index, newly_bound))
    }

    fn release(&mut self, mac: MacAddress, address: Ipv4Address) -> Option<Ipv4Address> {
        if let Some(index) = self.find_mac(mac) {
            if address == Ipv4Address::UNSPECIFIED || self.address(index) == address {
                let released = self.address(index);
                self.leases[index] = DhcpLeaseSlot::empty();
                self.dirty = true;
                return Some(released);
            }
        }
        None
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
            gateway_pin: None,
            pending_arp: None,
            pending_arp_retry: 0,
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
        b". Usage: tape run router [dhcp [NIC]]... [health ADDRESS] [dhcp-server] [dhcp-lease-file NAME] [nat|no-nat] [quiet] [dns SERVER [SERVER]] [forward tcp|udp EXTERNAL_PORT INTERNAL_IP [INTERNAL_PORT]] \
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
        print_bytes(putc, b" (uplink external)\n");
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
) -> Option<DhcpLeaseEvent> {
    let Ok(ethernet) = EthernetFrame::parse(frame) else {
        return None;
    };
    if ethernet.ethertype != ETHERTYPE_IPV4 {
        return None;
    }
    let Ok(packet) = Ipv4Packet::parse(ethernet.payload, true) else {
        return None;
    };
    if packet.header.protocol != 17 {
        return None;
    }
    let Ok(datagram) = UdpDatagram::parse(
        packet.payload,
        false,
        packet.header.source,
        packet.header.destination,
    ) else {
        return None;
    };
    if datagram.source_port != DHCP_CLIENT_PORT || datagram.destination_port != DHCP_SERVER_PORT {
        return None;
    }
    let Ok(request) = DhcpPacket::parse(datagram.payload) else {
        return None;
    };
    if request.header.op != DhcpOp::Request
        || request.header.hardware_type != 1
        || request.header.hardware_length != 6
    {
        return None;
    }
    let Ok(message_type) = request.message_type() else {
        return None;
    };
    let client_mac = request.header.client_mac();
    let Some((pool_start, pool_end)) = dhcp_pool(interface) else {
        return None;
    };
    let options = DhcpOptions::new(request.options);
    let requested = options.ipv4(DHCP_OPTION_REQUESTED_IP).ok().flatten();
    let server_identifier = options.ipv4(DHCP_OPTION_SERVER_IDENTIFIER).ok().flatten();

    if message_type == DhcpMessageType::Release {
        return server
            .release(client_mac, request.header.client_address)
            .map(|address| DhcpLeaseEvent::Released {
                mac: client_mac,
                address,
                declined: false,
            });
    }
    if message_type == DhcpMessageType::Decline {
        return server
            .release(
                client_mac,
                requested.unwrap_or(request.header.client_address),
            )
            .map(|address| DhcpLeaseEvent::Released {
                mac: client_mac,
                address,
                declined: true,
            });
    }
    if server_identifier.is_some_and(|value| value != interface.address) {
        return None;
    }

    let (reply_type, address, event) = match message_type {
        DhcpMessageType::Discover => {
            let Some((index, new_offer)) = server.allocate(client_mac, now) else {
                return None;
            };
            let address = server.address(index);
            (
                DhcpMessageType::Offer,
                address,
                new_offer.then_some(DhcpLeaseEvent::Offered {
                    mac: client_mac,
                    address,
                }),
            )
        }
        DhcpMessageType::Request => match server.requested(
                client_mac,
                requested,
                request.header.client_address,
                pool_start,
                pool_end,
                now,
            ) {
                Some((index, newly_bound)) => {
                    let address = server.address(index);
                    (
                        DhcpMessageType::Ack,
                        address,
                        newly_bound.then_some(DhcpLeaseEvent::Bound {
                            mac: client_mac,
                            address,
                        }),
                    )
                }
                None => (
                    DhcpMessageType::Nak,
                    Ipv4Address::UNSPECIFIED,
                    Some(DhcpLeaseEvent::Nak { mac: client_mac }),
                ),
            },
        _ => return None,
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
        return None;
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
        return None;
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
            return None;
        }
    }
    let Ok(option_length) = writer.finish() else {
        return None;
    };
    let dhcp = DhcpPacket::new(header, &option_bytes[..option_length]);
    let mut payload = [0u8; 600];
    let Ok(payload_length) = dhcp.encode(&mut payload) else {
        return None;
    };
    if !send_dhcp_payload(
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
    ) {
        return Some(DhcpLeaseEvent::SendFailed);
    }
    event
}

fn print_dhcp_lease_event(putc: Putc, nic: u32, event: DhcpLeaseEvent) {
    match event {
        DhcpLeaseEvent::Offered { mac, address } => {
            print_bytes(putc, b"DHCP: NIC ");
            print_u32(putc, nic + 1);
            print_bytes(putc, b" offered ");
            print_ip(putc, address);
            print_bytes(putc, b" to ");
            print_mac(putc, mac);
            print_bytes(putc, b".\n");
        }
        DhcpLeaseEvent::Bound { mac, address } => {
            print_bytes(putc, b"DHCP: NIC ");
            print_u32(putc, nic + 1);
            print_bytes(putc, b" leased ");
            print_ip(putc, address);
            print_bytes(putc, b" to ");
            print_mac(putc, mac);
            print_bytes(putc, b".\n");
        }
        DhcpLeaseEvent::Released {
            mac,
            address,
            declined,
        } => {
            print_bytes(putc, b"DHCP: NIC ");
            print_u32(putc, nic + 1);
            print_bytes(
                putc,
                if declined {
                    b" declined "
                } else {
                    b" released "
                },
            );
            print_ip(putc, address);
            print_bytes(putc, b" from ");
            print_mac(putc, mac);
            print_bytes(putc, b".\n");
        }
        DhcpLeaseEvent::Nak { mac } => {
            print_bytes(putc, b"DHCP: NIC ");
            print_u32(putc, nic + 1);
            print_bytes(putc, b" rejected a request from ");
            print_mac(putc, mac);
            print_bytes(putc, b".\n");
        }
        DhcpLeaseEvent::SendFailed => {
            print_bytes(putc, b"DHCP: reply transmission failed.\n");
        }
    }
}

/// Derives a fresh, essentially-random DHCP transaction ID from this NIC's
/// MAC address and the current tick count, so a restarted `DhcpClient` (one
/// whose previous cycle went `Expired` or `Failed`) doesn't reuse the exact
/// transaction ID an upstream server might still associate with the old,
/// abandoned exchange. Shared by every restart site in this file
/// (`configure_dhcp`'s own retry loop, and `poll_dhcp_renewals`'s
/// Expired/Failed restarts) so they can't drift out of sync with each other.
fn fresh_dhcp_client(own_mac: MacAddress, ticks: Ticks) -> DhcpClient<128> {
    let transaction_id = (ticks() as u32)
        ^ u32::from_be_bytes([own_mac.0[0], own_mac.0[1], own_mac.0[2], own_mac.0[3]])
        ^ u32::from_be_bytes([own_mac.0[2], own_mac.0[3], own_mac.0[4], own_mac.0[5]]);
    DhcpClient::<128>::new(own_mac, transaction_id, DHCP_RETRY_SECONDS, DHCP_MAX_RETRIES)
}

/// Outcome of one bounded DHCP-uplink acquisition attempt at boot.
/// `TimedOut` deliberately keeps the (still-cycling) client instead of
/// discarding it: this uplink NIC simply didn't hear back from a server
/// within this boot attempt's own budget, which is not distinguishable from
/// "genuinely offline" at this point -- a router built for fail-over must
/// not refuse to start just because one of several configured uplinks
/// happens to be the one still waiting, so the caller keeps that client
/// running in the background via `poll_dhcp_renewals` instead of treating
/// it as fatal. `Aborted` and `Error` remain fatal to the whole tape: the
/// former is an explicit operator request to stop, and the latter indicates
/// a NIC/callback problem that isn't specific to one DHCP server being slow
/// or absent.
enum DhcpAcquireOutcome {
    Bound(DhcpClient<128>),
    TimedOut(DhcpClient<128>),
    Aborted,
    Error,
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
) -> DhcpAcquireOutcome {
    print_bytes(
        putc,
        b"DHCP: broadcasting DISCOVER and waiting for a lease...\n",
    );
    let mut client = fresh_dhcp_client(own_mac, ticks);
    let started = (ticks() / 100) as u32;
    let mut payload = [0u8; 600];

    loop {
        if should_abort() == TAPE_EVENT_ABORT {
            print_bytes(
                putc,
                b"DHCP stopped by operator before a lease was acquired.\n",
            );
            return DhcpAcquireOutcome::Aborted;
        }
        let now = (ticks() / 100) as u32;
        if now.wrapping_sub(started) > DHCP_TIMEOUT_SECONDS {
            print_bytes(
                putc,
                b"DHCP: no server responded before the boot timeout; will keep retrying in the background.\n",
            );
            return DhcpAcquireOutcome::TimedOut(client);
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
                    return DhcpAcquireOutcome::Error;
                }
            }
            Ok(None) => {}
            Err(_) => {
                print_bytes(putc, b"DHCP failed: packet construction error.\n");
                return DhcpAcquireOutcome::Error;
            }
        }
        if client.state() == DhcpClientState::Bound {
            let Some(lease) = client.lease() else {
                print_bytes(putc, b"DHCP failed: client reported a missing lease.\n");
                return DhcpAcquireOutcome::Error;
            };
            if lease.subnet_mask == Ipv4Address::UNSPECIFIED {
                print_bytes(putc, b"DHCP failed: server supplied no subnet mask.\n");
                return DhcpAcquireOutcome::Error;
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
            return DhcpAcquireOutcome::Bound(client);
        }
        if client.state() == DhcpClientState::Failed {
            // A bounded retry cycle exhausting itself (see DHCP_TIMEOUT_SECONDS's
            // doc comment) is not necessarily a real failure -- restart a fresh
            // client with a new transaction ID and keep trying within the
            // overall budget, exactly as poll_dhcp_renewals already does after
            // boot for an Expired lease, and exactly as tapes/net-dhcp-test
            // does for its own initial acquisition. Only running out of the
            // overall DHCP_TIMEOUT_SECONDS budget (checked at the top of this
            // loop) is treated as a real, reportable failure.
            print_bytes(
                putc,
                b"DHCP: no usable offer/acknowledgement arrived; retrying...\n",
            );
            client = fresh_dhcp_client(own_mac, ticks);
            continue;
        }

        let length =
            nic_receive_at(nic, receive_buffer.as_mut_ptr() as u32, max_frame_len as u32) as usize;
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

/// Drives every DHCP-uplink interface's retained DhcpClient once per main
/// forwarding-loop iteration: fires whatever request the library's own
/// Bound/Renewing/Rebinding state machine says is due (an ordinary renewal
/// unicast to the current server, a Rebinding broadcast, or nothing at all
/// most iterations), and reacts if a lease was just renewed or the client
/// gave up and expired. This never blocks -- `DhcpClient::poll` only ever
/// returns a packet to send or `None`, exactly like `resolve_mac`'s and the
/// uplink-health redesign's own non-blocking designs elsewhere in this file.
///
/// Before this existed, `configure_dhcp` was the *only* thing that ever
/// drove a DHCP client: once it returned a lease at startup, the client
/// itself was discarded entirely, so nothing ever renewed, rebound, or even
/// noticed a lease quietly expiring. A long-running router just kept
/// forwarding traffic under a private address the upstream server had
/// already reclaimed once that lease's `expires_at` passed.
///
/// Returns true if anything this loop depends on (an interface's address or
/// an uplink's health-checked gateway) actually changed, so the caller
/// knows any state it derived from the old value may now be stale.
#[allow(clippy::too_many_arguments)]
fn poll_dhcp_renewals(
    putc: Putc,
    quiet: bool,
    dhcp_clients: &mut [Option<DhcpClient<128>>; MAX_INTERFACES],
    interfaces: &mut [RouterInterface; MAX_INTERFACES],
    configs: &mut [Option<InterfaceConfig>; MAX_INTERFACES],
    interface_count: usize,
    uplinks: &mut [Option<UplinkHealth>; MAX_INTERFACES],
    dhcp_uplink_count: usize,
    nic_send_at: NicSendAt,
    ticks: Ticks,
    max_frame_len: usize,
) -> bool {
    let mut changed = false;
    let now = (ticks() / 100) as u32;
    let mut payload = [0u8; 600];
    for index in 0..interface_count {
        let own_mac = interfaces[index].mac;
        let nic = interfaces[index].nic;
        let Some(client) = dhcp_clients[index].as_mut() else {
            continue;
        };
        let previous_state = client.state();
        if let Ok(Some(length)) = client.poll(now, &mut payload) {
            let transmit = client.transmit();
            let _ = send_dhcp_payload(
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
            );
        }
        let new_state = client.state();
        let lease = client.lease();
        // `client`'s mutable borrow ends here (its last use); everything
        // below only touches `dhcp_clients[index]` again, `interfaces`,
        // `configs`, and `uplinks`.
        //
        // Expired (a previously bound lease's server-assigned lifetime ran
        // out) and Failed (a bounded acquisition/renewal cycle exhausted its
        // own retries with no usable reply -- the same outcome
        // `configure_dhcp`'s own retry loop treats as transient, see its doc
        // comment) both mean this client's cycle is over but the uplink
        // itself may still recover, so both restart a fresh client rather
        // than leaving `dhcp_clients[index]` stuck returning `Ok(None)`
        // forever. This is what lets an uplink NIC that had no server reply
        // at all during initial boot acquisition (see the boot loop below,
        // which starts such a NIC's client already in this same situation)
        // keep trying indefinitely in the background instead of never
        // getting another chance.
        if (new_state == DhcpClientState::Expired || new_state == DhcpClientState::Failed)
            && previous_state != new_state
        {
            if !quiet {
                if new_state == DhcpClientState::Expired {
                    print_bytes(putc, b"DHCP lease expired on NIC ");
                } else {
                    print_bytes(putc, b"DHCP acquisition failed on NIC ");
                }
                print_u32(putc, nic);
                print_bytes(putc, b"; restarting acquisition.\n");
            }
            dhcp_clients[index] = Some(fresh_dhcp_client(own_mac, ticks));
            changed = true;
            continue;
        }
        if new_state != DhcpClientState::Bound {
            continue;
        }
        let Some(lease) = lease else {
            continue;
        };
        if lease.address != interfaces[index].address || lease.subnet_mask != interfaces[index].netmask
        {
            if !quiet {
                print_bytes(putc, b"DHCP lease renewed on NIC ");
                print_u32(putc, nic);
                print_bytes(putc, b": ");
                print_ip(putc, lease.address);
                print_bytes(putc, b"/");
                print_ip(putc, lease.subnet_mask);
                print_bytes(putc, b".\n");
            }
            interfaces[index].address = lease.address;
            interfaces[index].netmask = lease.subnet_mask;
            configs[index] = Some(InterfaceConfig {
                nic,
                address: lease.address,
                netmask: lease.subnet_mask,
            });
            changed = true;
        }
        if lease.router != Ipv4Address::UNSPECIFIED
            && let Some(uplink) = uplinks[..dhcp_uplink_count]
                .iter_mut()
                .flatten()
                .find(|uplink| uplink.interface == index)
            && uplink.gateway != lease.router
        {
            uplink.gateway = lease.router;
            changed = true;
        }
    }
    changed
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

/// Answers an ICMP echo request addressed to one of the router's own
/// interface addresses -- e.g. an operator pinging `192.168.0.128` to check
/// whether the router itself is reachable, as opposed to pinging *through*
/// it to a downstream host. `forward_ipv4` never generates this on its own:
/// a router that only forwards traffic between other hosts has no inherent
/// reason to also answer pings addressed to itself, and this was missing
/// entirely until an operator noticed a ping to the router's own upstream
/// address got no reply at all while diagnosing an unrelated forwarding
/// question. The reply copies the request's payload verbatim (matching
/// ordinary ICMP echo semantics -- a real `ping`'s payload bytes come back
/// unchanged) and only rewrites the type byte and checksum.
///
/// `reply_source` is the request's own *destination* address -- i.e. the
/// specific interface address that was actually pinged -- not necessarily
/// `interface.address` (the interface the request happened to arrive on).
/// A request can legitimately arrive on one interface addressed to
/// *another* configured interface's address (e.g. an upstream host pinging
/// the router's downstream address, routed in via the uplink), and the
/// reply must come from the address that was pinged, or the requester sees
/// a reply from an address it never contacted and can drop or misattribute
/// it -- the exact bug an operator found by pinging the router's downstream
/// address from outside and seeing the reply arrive from its uplink address
/// instead.
fn send_icmp_echo_reply(
    interface: &RouterInterface,
    reply_source: Ipv4Address,
    request_source: Ipv4Address,
    request_payload: &[u8],
    destination_mac: MacAddress,
    payload_buffer: &mut [u8; MAX_FRAME_LEN],
    ip_buffer: &mut [u8; MAX_FRAME_LEN],
    output_buffer: &mut [u8; MAX_FRAME_LEN],
    nic_send_at: NicSendAt,
    max_frame_len: usize,
) -> bool {
    let payload_len = request_payload.len();
    if payload_len < 8 || payload_len > payload_buffer.len() {
        return false;
    }
    payload_buffer[..payload_len].copy_from_slice(request_payload);
    payload_buffer[0] = 0; // Echo Reply (request is type 8, code 0 stays 0)
    payload_buffer[2] = 0;
    payload_buffer[3] = 0;
    let checksum = internet_checksum(&payload_buffer[..payload_len]);
    payload_buffer[2..4].copy_from_slice(&checksum.to_be_bytes());
    let packet = Ipv4Packet {
        header: Ipv4Header::new(reply_source, request_source, 1, payload_len),
        payload: &payload_buffer[..payload_len],
    };
    let Ok(ip_len) = packet.encode(ip_buffer) else {
        return false;
    };
    let ethernet = EthernetFrame::new(destination_mac, interface.mac, ETHERTYPE_IPV4, &ip_buffer[..ip_len]);
    let Ok(length) = ethernet.encode(output_buffer) else {
        return false;
    };
    length <= max_frame_len
        && nic_send_at(interface.nic, output_buffer.as_ptr() as u32, length as u32) != 0
}

/// Recognizes a genuinely fresh ARP reply from `target`, without touching
/// any interface's ArpCache. `advance_uplink_health`'s `AwaitingArp` phase
/// uses this instead of a cache lookup so a health round's freshness check
/// never has to invalidate the same cache entry ordinary forwarding relies
/// on for that same gateway -- see `start_health_round`'s doc comment for
/// why invalidating it was a real bug.
fn is_arp_reply_for(frame: &[u8], target: Ipv4Address) -> Option<MacAddress> {
    let ethernet = EthernetFrame::parse(frame).ok()?;
    if ethernet.ethertype != ETHERTYPE_ARP {
        return None;
    }
    let packet = ArpPacket::parse(ethernet.payload).ok()?;
    if packet.operation == ArpOperation::Reply && packet.sender_protocol == target {
        Some(packet.sender_hardware)
    } else {
        None
    }
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
/// Starts one non-blocking health-probe round for every configured uplink:
/// sends a fresh ARP request for the gateway, then arms a per-uplink
/// deadline. It never waits for a reply itself; replies are recognized
/// later, opportunistically, by advance_uplink_health as the ordinary
/// per-ingress loop reads frames it would read anyway.
///
/// This deliberately does *not* invalidate the interface's ArpCache entry
/// for the gateway first, even though doing so once seemed like the right
/// way to force a "genuinely fresh" answer (see ArpCache::invalidate). The
/// gateway is also the default-route next hop ordinary forwarded traffic
/// resolves through resolve_mac -- invalidating its one shared cache entry
/// every health-poll cycle created a race: if a real packet needed that
/// same MAC before this round's own reply repopulated the cache,
/// resolve_mac would block the whole router for up to ARP_TIMEOUT_TICKS
/// waiting for a fresh reply, reintroducing exactly the multi-second
/// stalls the non-blocking redesign was meant to eliminate (confirmed on
/// real hardware as periodic ~2.5s ping spikes aligned with the health
/// cycle). advance_uplink_health now verifies freshness by recognizing
/// this round's own ARP reply frame directly (`is_arp_reply_for`) instead
/// of relying on a cache lookup, so it needs no shared cache entry evicted
/// at all.
fn start_health_round(
    uplinks: &mut [Option<UplinkHealth>; MAX_INTERFACES],
    dhcp_uplink_count: usize,
    interfaces: &mut [RouterInterface; MAX_INTERFACES],
    nic_send_at: NicSendAt,
    ticks: Ticks,
    max_frame_len: usize,
) {
    for uplink in uplinks[..dhcp_uplink_count].iter_mut().flatten() {
        // An uplink that hasn't yet acquired a gateway at all (a boot-time
        // DHCP timeout, or a lease with no router option -- see the
        // uplink-acquisition loop in tape_main) has nothing to ARP for yet;
        // leave it Idle and unhealthy until poll_dhcp_renewals gives it a
        // real gateway; this also keeps active_uplink's own
        // `.find(|uplink| uplink.healthy)` from ever picking it in the
        // meantime.
        if uplink.gateway == Ipv4Address::UNSPECIFIED {
            continue;
        }
        let interface = uplink.interface;
        let _ = send_arp_request(&interfaces[interface], uplink.gateway, nic_send_at, max_frame_len);
        uplink.phase = HealthPhase::AwaitingArp;
        uplink.deadline = ticks().wrapping_add(HEALTH_PROBE_TICKS);
    }
}

/// Fails any uplink whose current probe phase has run past its deadline
/// with no reply. Called once per `'router` loop iteration, independent of
/// whether any frame arrived, since an uplink that never answers at all
/// would otherwise never time out. Returns whether any uplink's `healthy`
/// flag changed, so the caller knows whether `active_uplink` needs
/// recomputing.
fn expire_health_timeouts(
    uplinks: &mut [Option<UplinkHealth>; MAX_INTERFACES],
    dhcp_uplink_count: usize,
    ticks: Ticks,
) -> bool {
    let mut changed = false;
    let now = ticks();
    for uplink in uplinks[..dhcp_uplink_count].iter_mut().flatten() {
        if uplink.phase == HealthPhase::Idle {
            continue;
        }
        if now.wrapping_sub(uplink.deadline) >= (1u64 << 63) {
            // Deadline not yet reached (the wrapping difference is still
            // "negative" in two's-complement terms) -- keep waiting.
            continue;
        }
        uplink.phase = HealthPhase::Idle;
        uplink.misses = uplink.misses.saturating_add(1);
        if uplink.misses >= HEALTH_MISSED_CYCLES && uplink.healthy {
            uplink.healthy = false;
            changed = true;
        }
    }
    changed
}

/// Gives every uplink currently waiting on a probe reply a look at the
/// frame the ordinary per-ingress loop just read for `ingress` -- the same
/// frame `process_arp`/`forward_ipv4` are about to process anyway, so this
/// adds no extra NIC reads and never blocks. An uplink in `AwaitingArp`
/// recognizes this round's own reply directly via `is_arp_reply_for`,
/// deliberately not via `interfaces[ingress].arp`'s cache (see
/// `start_health_round`'s doc comment for why sharing that cache entry with
/// ordinary forwarding caused real stalls); an uplink in `AwaitingIcmp`
/// inspects this frame directly for its expected echo reply. Returns
/// whether any uplink's `healthy` flag changed.
#[allow(clippy::too_many_arguments)]
fn advance_uplink_health(
    uplinks: &mut [Option<UplinkHealth>; MAX_INTERFACES],
    dhcp_uplink_count: usize,
    ingress: usize,
    frame: &[u8],
    interfaces: &mut [RouterInterface; MAX_INTERFACES],
    target: Ipv4Address,
    nic_send_at: NicSendAt,
    ticks: Ticks,
    max_frame_len: usize,
) -> bool {
    let mut changed = false;
    for uplink in uplinks[..dhcp_uplink_count].iter_mut().flatten() {
        if uplink.interface != ingress {
            continue;
        }
        match uplink.phase {
            HealthPhase::Idle => {}
            HealthPhase::AwaitingArp => {
                if let Some(gateway_mac) = is_arp_reply_for(frame, uplink.gateway) {
                    interfaces[ingress].gateway_pin = Some((uplink.gateway, gateway_mac));
                    uplink.sequence = uplink.sequence.wrapping_add(1);
                    if send_icmp_echo(
                        &interfaces[ingress],
                        target,
                        gateway_mac,
                        uplink.sequence,
                        nic_send_at,
                        max_frame_len,
                    ) {
                        uplink.phase = HealthPhase::AwaitingIcmp;
                        uplink.deadline = ticks().wrapping_add(HEALTH_PROBE_TICKS);
                    } else {
                        uplink.phase = HealthPhase::Idle;
                        uplink.misses = uplink.misses.saturating_add(1);
                        if uplink.misses >= HEALTH_MISSED_CYCLES && uplink.healthy {
                            uplink.healthy = false;
                            changed = true;
                        }
                    }
                }
            }
            HealthPhase::AwaitingIcmp => {
                if is_icmp_echo_reply(frame, &interfaces[ingress], target, uplink.sequence) {
                    uplink.phase = HealthPhase::Idle;
                    uplink.misses = 0;
                    if !uplink.healthy {
                        uplink.healthy = true;
                        changed = true;
                    }
                }
            }
        }
    }
    changed
}


/// Derives the active default route, active NIC, and DHCP-uplink-filtered
/// routing table from the current failover state. Shared by the health-poll
/// block (which needs this *before* forwarding traffic seen during a probe)
/// and the ordinary per-frame forwarding loop (which needs it *after* a
/// failover may have just changed `active_uplink`), so the two call sites
/// can never drift out of sync with each other.
#[allow(clippy::too_many_arguments)]
fn compute_forwarding_context(
    uplinks: &[Option<UplinkHealth>; MAX_INTERFACES],
    dhcp_uplink_count: usize,
    active_uplink: Option<usize>,
    default_route: Option<router_tape::DefaultRoute>,
    interfaces: &[RouterInterface; MAX_INTERFACES],
    interface_count: usize,
    configs: &[Option<InterfaceConfig>; MAX_INTERFACES],
    dhcp_uplinks_config: &[Option<u32>; MAX_INTERFACES],
) -> (
    Option<router_tape::DefaultRoute>,
    u32,
    [Option<InterfaceConfig>; MAX_INTERFACES],
) {
    let current_default = active_uplink
        .and_then(|index| {
            uplinks[..dhcp_uplink_count]
                .iter()
                .flatten()
                .find(|uplink| uplink.interface == index)
                .map(|uplink| router_tape::DefaultRoute {
                    nic: interfaces[index].nic,
                    gateway: uplink.gateway,
                })
        })
        .or(default_route);
    let active_index = active_uplink
        .or_else(|| {
            current_default.and_then(|route| {
                interfaces[..interface_count]
                    .iter()
                    .position(|interface| interface.nic == route.nic)
            })
        })
        .unwrap_or(0);
    let active_nic = interfaces[active_index].nic;
    // Standby DHCP uplinks may share the active uplink's subnet. Exclude
    // them from direct-route matching so that subnet never bypasses the
    // health-selected active default route after a failover.
    let routing_configs = routing_configs_for_active_uplink(
        configs,
        interface_count,
        dhcp_uplinks_config,
        dhcp_uplink_count,
        active_uplink.map(|index| interfaces[index].nic),
    );
    (current_default, active_nic, routing_configs)
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
    release_held_frame(interface, packet.sender_protocol, packet.sender_hardware, now, nic_send_at);
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
    nic_send_at: NicSendAt,
    ticks: Ticks,
    max_frame_len: usize,
) -> Option<MacAddress> {
    // Check the pinned gateway MAC (see RouterInterface::gateway_pin) before
    // the general ArpCache: it is refreshed every health round and immune
    // to that cache's own LRU eviction from unrelated ARP traffic, so this
    // is the common, fast, non-blocking path for the default route's next
    // hop on every ordinary forwarded packet.
    if let Some((pinned_ip, pinned_mac)) = interfaces[egress].gateway_pin {
        if pinned_ip == target {
            return Some(pinned_mac);
        }
    }
    let now = ticks();
    if let Some(mac) = interfaces[egress].arp.lookup(target, now as u32) {
        return Some(mac);
    }
    // Never block waiting for a reply here -- this used to nest its own
    // nested-loop wait of up to ARP_TIMEOUT_TICKS, freezing every other
    // interface's forwarding for the whole wait on any cache miss to any
    // destination (not just a health-monitored gateway), the exact same
    // class of stall the uplink-health redesign eliminated for gateway
    // probing. Instead, a miss fires at most one ARP request per
    // ARP_RETRY_TICKS window for this (egress, target) pair and drops the
    // packet that triggered it -- exactly like an ordinary IP stack drops
    // (rather than blocks) a packet on an ARP cache miss, relying on the
    // caller's own retransmission (TCP) or next send (UDP/ICMP) to succeed
    // once a reply arrives. That reply needs no special handling here: the
    // main per-ingress loop already calls process_arp on every frame it
    // reads regardless of content, so it populates `arp` in time for a
    // later packet to the same destination to hit the cache lookup above.
    let interface = &mut interfaces[egress];
    let retry_on_cooldown = interface.pending_arp == Some(target)
        && now.wrapping_sub(interface.pending_arp_retry) >= (1u64 << 63);
    if !retry_on_cooldown {
        let _ = send_arp_request(interface, target, nic_send_at, max_frame_len);
        interface.pending_arp = Some(target);
        interface.pending_arp_retry = now.wrapping_add(ARP_RETRY_TICKS);
    }
    None
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

fn nat_endpoints(payload: &[u8], protocol: u8) -> Option<(u16, u16)> {
    match protocol {
        6 if payload.len() >= 20 => Some((
            u16::from_be_bytes([payload[0], payload[1]]),
            u16::from_be_bytes([payload[2], payload[3]]),
        )),
        17 if payload.len() >= 8 => Some((
            u16::from_be_bytes([payload[0], payload[1]]),
            u16::from_be_bytes([payload[2], payload[3]]),
        )),
        // ICMP echo uses its identifier as the flow key. NAT only translates
        // requests and matching replies; ICMP error payloads remain forwarded
        // unchanged because they quote another packet's IP and transport data.
        1 if payload.len() >= 8
            && (payload[0] == 8 || payload[0] == 0)
            && payload[1] == 0 =>
        {
            let identifier = u16::from_be_bytes([payload[4], payload[5]]);
            Some((identifier, identifier))
        }
        _ => None,
    }
}

// An echo reply preserves the router-assigned external identifier, so it no
// longer contains the inside request's identifier. The remote address plus
// external identifier uniquely selects its mapping; reserve the remote-port
// slot as zero for both directions of an ICMP echo flow.
fn nat_remote_endpoint(protocol: u8, endpoint: u16) -> u16 {
    if protocol == 1 {
        0
    } else {
        endpoint
    }
}

fn checksum_replace(checksum: u16, old: u16, new: u16) -> u16 {
    let mut sum = u32::from(!checksum) + u32::from(!old) + u32::from(new);
    while (sum >> 16) != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

// TCP and UDP checksums include the IPv4 pseudo-header, while ICMP checksums
// cover only the ICMP payload. Update just the changed words so options and
// payload bytes stay untouched while NAT rewrites the packet.
fn rewrite_transport(
    payload: &mut [u8],
    protocol: u8,
    old_address: Ipv4Address,
    new_address: Ipv4Address,
    old_port: u16,
    new_port: u16,
    source: bool,
) -> bool {
    if protocol == 1 {
        if payload.len() < 8
            || (payload[0] != 8 && payload[0] != 0)
            || payload[1] != 0
        {
            return false;
        }
        let identifier = u16::from_be_bytes([payload[4], payload[5]]);
        if identifier != old_port {
            return false;
        }
        let checksum = u16::from_be_bytes([payload[2], payload[3]]);
        payload[4..6].copy_from_slice(&new_port.to_be_bytes());
        let updated = checksum_replace(checksum, old_port, new_port);
        payload[2..4].copy_from_slice(&updated.to_be_bytes());
        return true;
    }
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
    putc: Putc,
    quiet: bool,
    ingress: usize,
    frame: &[u8],
    interfaces: &mut [RouterInterface; MAX_INTERFACES],
    configs: &[Option<InterfaceConfig>; MAX_INTERFACES],
    count: usize,
    default_route: Option<router_tape::DefaultRoute>,
    active_uplink: u32,
    nic_send_at: NicSendAt,
    ticks: Ticks,
    nat: &mut NatTable,
    nat_enabled: bool,
    forwards: &[Option<PortForward>; MAX_FORWARDS],
    forward_count: usize,
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
    // This router (like the rest of tape-net, see NETWORKING.md's stated
    // scope) has no fragment-reassembly support: nat_endpoints/
    // rewrite_transport assume packet.payload starts with a complete
    // TCP/UDP/ICMP header. A fragment offset of zero with the
    // more-fragments flag set is a first fragment whose payload happens to
    // still start with a real transport header (which could slip through
    // undetected and get NATed as if it were the whole datagram, corrupting
    // a rewrite that a never-arriving reassembly would need to complete
    // consistently), and any fragment with a nonzero offset carries no
    // transport header at all (raw payload continuation bytes are not port
    // numbers). Both cases are rejected before any NAT/transport rewrite
    // touches the payload.
    if packet.header.flags_fragment & (IPV4_FLAG_MORE_FRAGMENTS | IPV4_FRAGMENT_OFFSET_MASK) != 0 {
        return ForwardResult::Dropped;
    }
    let now = ticks();
    // An ICMP echo request addressed to one of the router's own interfaces
    // -- e.g. an operator pinging `192.168.0.128` to check whether the
    // router itself is reachable, as opposed to pinging *through* it to a
    // downstream host -- is answered here, before the active-uplink inbound
    // NAT/port-forward check just below: that check treats *any* packet
    // addressed to the active uplink's own address as inbound NAT traffic
    // needing a matching mapping, which would otherwise swallow and drop an
    // unsolicited ping to the router's own upstream address even with NAT
    // disabled. Any other protocol addressed to the router itself still
    // falls through to that check (and, for a non-uplink interface, to the
    // `is_local` drop further below) unchanged.
    if packet.header.protocol == 1
        && packet.payload.len() >= 8
        && packet.payload[0] == 8
        && packet.payload[1] == 0
        && is_local(interfaces, count, packet.header.destination)
    {
        interfaces[ingress]
            .arp
            .insert(packet.header.source, ethernet.source, now as u32);
        if let Some(destination_mac) = resolve_mac(
            ingress,
            packet.header.source,
            interfaces,
            nic_send_at,
            ticks,
            max_frame_len,
        ) {
            send_icmp_echo_reply(
                &interfaces[ingress],
                packet.header.destination,
                packet.header.source,
                packet.payload,
                destination_mac,
                transport_buffer,
                ip_buffer,
                output_buffer,
                nic_send_at,
                max_frame_len,
            );
        }
        return ForwardResult::Dropped;
    }
    let mut header = packet.header;
    let payload_len = packet.payload.len();
    let mut payload_in_transport_buffer = false;
    if interfaces[ingress].nic == active_uplink
        && packet.header.destination == interfaces[ingress].address
    {
        let Some((source_port, destination_port)) =
            nat_endpoints(packet.payload, packet.header.protocol)
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
                nat_remote_endpoint(packet.header.protocol, source_port),
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
            nat_endpoints(packet.payload, header.protocol)
        {
            let static_forward =
                reply_forward(forwards, forward_count, header.protocol, header.source, source_port);
            let external_port = if let Some(forward) = static_forward {
                forward.external_port
            } else if nat_enabled {
                let Some((port, new_mapping)) = nat.outbound(
                    header.protocol,
                    header.source,
                    source_port,
                    header.destination,
                    nat_remote_endpoint(header.protocol, destination_port),
                    now,
                ) else {
                    return ForwardResult::Dropped;
                };
                if new_mapping && !quiet {
                    print_bytes(putc, b"NAT: new ");
                    print_bytes(
                        putc,
                        if header.protocol == 1 {
                            b"ICMP"
                        } else {
                            match forward_protocol(header.protocol) {
                                Some(ForwardProtocol::Tcp) => b"TCP",
                                Some(ForwardProtocol::Udp) => b"UDP",
                                None => b"IP",
                            }
                        },
                    );
                    print_bytes(putc, b" flow ");
                    print_ip(putc, header.source);
                    print_bytes(putc, b":");
                    print_u32(putc, u32::from(source_port));
                    print_bytes(putc, b" -> ");
                    print_ip(putc, header.destination);
                    print_bytes(putc, b":");
                    print_u32(putc, u32::from(destination_port));
                    print_bytes(putc, b" via NIC ");
                    print_u32(putc, interfaces[egress].nic + 1);
                    print_bytes(putc, b" as ");
                    print_ip(putc, interfaces[egress].address);
                    print_bytes(putc, b":");
                    print_u32(putc, u32::from(port));
                    print_bytes(putc, b".\n");
                }
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
        } else if nat_enabled && matches!(header.protocol, 6 | 17) {
            // nat_endpoints returned None here because the payload is too
            // short to contain a TCP/UDP source/destination port pair, not
            // because this is genuinely different (non-TCP/UDP/ICMP-echo)
            // traffic -- NETWORKING.md's "other ICMP messages and
            // non-TCP/UDP traffic are forwarded without translation" is a
            // deliberate passthrough for that latter case and is left
            // alone below. A malformed TCP/UDP segment has no way to be
            // NATed, so forwarding it unchanged while NAT is enabled would
            // leak the downstream host's private source address onto the
            // uplink -- the same leak the inbound-NAT path already avoids
            // by dropping unmatched/unparseable traffic instead of passing
            // it through.
            return ForwardResult::Dropped;
        }
    }
    // On an ARP miss `resolve_mac` has already sent the request; the packet is
    // finished below with a blank destination MAC and held (`hold_for_arp`),
    // then sent by `process_arp` when the reply arrives. Without that, the
    // first packet of every conversation was dropped and the sender had to
    // retry (a ping's first reply came a second late).
    let resolved = resolve_mac(
        egress,
        next_hop,
        interfaces,
        nic_send_at,
        ticks,
        max_frame_len,
    );
    let destination_mac = resolved.unwrap_or(MacAddress([0; 6]));
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
    if resolved.is_none() {
        hold_for_arp(
            interfaces[egress].nic,
            next_hop,
            &output_buffer[..length],
            now,
        );
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

/// One fully built outgoing frame waiting for its destination's ARP reply
/// (destination MAC bytes still zero). A single slot is enough: ARP normally
/// answers within milliseconds, a newer miss simply replaces it, and it expires
/// after `HELD_FRAME_TICKS` so a late reply never releases a stale packet.
struct HeldFrame {
    active: bool,
    nic: u32,
    target: Ipv4Address,
    since: u64,
    length: usize,
    frame: [u8; MAX_FRAME_LEN],
}

const HELD_FRAME_TICKS: u64 = 300;
static mut HELD_FRAME: HeldFrame = HeldFrame {
    active: false,
    nic: 0,
    target: Ipv4Address([0; 4]),
    since: 0,
    length: 0,
    frame: [0; MAX_FRAME_LEN],
};

fn hold_for_arp(nic: u32, target: Ipv4Address, frame: &[u8], now: u64) {
    let held = unsafe { &mut *core::ptr::addr_of_mut!(HELD_FRAME) };
    if frame.len() > held.frame.len() {
        return;
    }
    held.frame[..frame.len()].copy_from_slice(frame);
    held.length = frame.len();
    held.nic = nic;
    held.target = target;
    held.since = now;
    held.active = true;
}

/// Sends the held frame if this ARP packet tells us the MAC it was waiting for.
fn release_held_frame(
    interface: &RouterInterface,
    sender: Ipv4Address,
    sender_mac: MacAddress,
    now: u64,
    nic_send_at: NicSendAt,
) {
    let held = unsafe { &mut *core::ptr::addr_of_mut!(HELD_FRAME) };
    if !held.active || held.nic != interface.nic || held.target != sender {
        return;
    }
    held.active = false;
    if now.wrapping_sub(held.since) > HELD_FRAME_TICKS {
        return;
    }
    held.frame[0..6].copy_from_slice(&sender_mac.0);
    let _ = nic_send_at(held.nic, held.frame.as_ptr() as u32, held.length as u32);
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
    _audio_sample_rate: extern "C" fn() -> u32,
    _audio_channels_and_bits: extern "C" fn() -> u32,
    _audio_submit: extern "C" fn(u32, u32) -> u32,
    _audio_capture: extern "C" fn(u32, u32) -> u32,
    _audio_count: extern "C" fn() -> u32,
    _audio_sample_rate_at: extern "C" fn(u32) -> u32,
    _audio_channels_and_bits_at: extern "C" fn(u32) -> u32,
    _audio_submit_at: extern "C" fn(u32, u32, u32) -> u32,
    _audio_capture_at: extern "C" fn(u32, u32, u32) -> u32,
    _audio_capture_sample_rate: extern "C" fn() -> u32,
    _audio_capture_channels_and_bits: extern "C" fn() -> u32,
    _audio_capture_sample_rate_at: extern "C" fn(u32) -> u32,
    _audio_capture_channels_and_bits_at: extern "C" fn(u32) -> u32,
    // Sleeps until the next interrupt; call after a pass that found no work.
    wait_for_event: extern "C" fn(),
) {
    tape_rt::set_wait_hook(wait_for_event);
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
    // The DHCP client for each DHCP-configured uplink interface, kept alive
    // (indexed by that interface's position in `interfaces`) for as long as
    // the router runs, not discarded once the initial lease is acquired.
    // configure_dhcp used to hand back only the resulting DhcpLease and drop
    // the DhcpClient entirely, so nothing ever renewed a lease before its
    // server-assigned expiry -- the router simply kept forwarding traffic
    // under a private address the upstream DHCP server had already reclaimed
    // for someone else. The main loop below polls/feeds each retained client
    // every iteration, exactly mirroring how the client library's own
    // Bound/Renewing/Rebinding states are driven during initial acquisition.
    let mut dhcp_clients: [Option<DhcpClient<128>>; MAX_INTERFACES] =
        core::array::from_fn(|_| None);

    let mut dns_from_uplink = false;
    for (priority, nic) in parsed.dhcp_uplinks[..parsed.dhcp_uplink_count].iter().flatten().enumerate() {
        let mut mac = [0u8; 6];
        if nic_mac_at(*nic, mac.as_mut_ptr() as u32) == 0 {
            print_bytes(putc, b"Router cannot start: failed to read a DHCP NIC MAC.\n");
            return;
        }
        let own_mac = MacAddress::new(mac);
        // A router built for fail-over must not refuse to start just
        // because one configured uplink NIC didn't hear back from a DHCP
        // server in time (or was offered a lease with no gateway) -- the
        // whole point of configuring more than one uplink is that another
        // one may still work. Every non-fatal case below still adds this
        // NIC's interface entry and keeps its DhcpClient retained in
        // `dhcp_clients`, unhealthy/unconfigured for now, so
        // poll_dhcp_renewals keeps driving it in the background and the
        // ordinary failover machinery adopts it automatically the moment it
        // does acquire a usable lease. Only an explicit operator abort or a
        // NIC/callback-level error (DhcpAcquireOutcome::Error) still halts
        // the whole router: those aren't specific to this one uplink being
        // slow or absent.
        let outcome = configure_dhcp(putc, should_abort, nic_send_at, nic_receive_at, ticks, own_mac, *nic, &mut receive_buffer, max_frame_len);
        let (client, lease) = match outcome {
            DhcpAcquireOutcome::Aborted => return,
            DhcpAcquireOutcome::Error => {
                print_bytes(putc, b"Router cannot start: DHCP uplink failed.\n");
                return;
            }
            DhcpAcquireOutcome::TimedOut(client) => (client, None),
            DhcpAcquireOutcome::Bound(client) => {
                let Some(lease) = client.lease() else {
                    print_bytes(putc, b"Router cannot start: DHCP uplink did not acquire a lease.\n");
                    return;
                };
                (client, Some(lease))
            }
        };
        let (address, netmask) = lease
            .map(|lease| (lease.address, lease.subnet_mask))
            .unwrap_or((Ipv4Address::UNSPECIFIED, Ipv4Address::UNSPECIFIED));
        let gateway = lease.map(|lease| lease.router).unwrap_or(Ipv4Address::UNSPECIFIED);
        if lease.is_some() && gateway == Ipv4Address::UNSPECIFIED {
            print_bytes(putc, b"DHCP uplink supplied no gateway; leaving it unhealthy until a renewal supplies one.\n");
        }
        interfaces[interface_count] = RouterInterface { nic: *nic, address, netmask, mac: own_mac, arp: ArpCache::new(), gateway_pin: None, pending_arp: None, pending_arp_retry: 0 };
        configs[interface_count] = lease.map(|_| InterfaceConfig { nic: *nic, address, netmask });
        uplinks[priority] = Some(UplinkHealth { interface: interface_count, gateway, misses: 0, healthy: gateway != Ipv4Address::UNSPECIFIED, sequence: 0, phase: HealthPhase::Idle, deadline: 0 });
        dhcp_clients[interface_count] = Some(client);
        interface_count += 1;
        if let Some(lease) = lease
            && dns_server_count == 0
            && !dns_from_uplink
        {
            dns_server_count = lease.dns_server_count;
            dns_servers[..dns_server_count].copy_from_slice(&lease.dns_servers[..dns_server_count]);
            dns_from_uplink = true;
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
            gateway_pin: None,
            pending_arp: None,
            pending_arp_retry: 0,
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
    // Prefer an uplink that is already marked healthy (has both a lease and
    // a gateway) over uplinks[0] unconditionally: a boot-time DHCP timeout or
    // a missing gateway option now leaves that first-priority slot present
    // but unhealthy (see the uplink-acquisition loop above), and routing
    // through it before the first health sweep corrects `active_uplink`
    // would otherwise send traffic out through a NIC with no real gateway.
    let mut active_uplink = uplinks[..parsed.dhcp_uplink_count]
        .iter()
        .flatten()
        .find(|uplink| uplink.healthy)
        .or(uplinks[0].as_ref())
        .map(|uplink| uplink.interface);
    // Run the first health sweep before accepting ordinary forwarding, rather
    // than assuming the first configured lease is reachable for one interval.
    let mut last_health_poll = ticks().wrapping_sub(HEALTH_POLL_TICKS);
    let mut last_lease_flush = ticks();
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
    if parsed.quiet {
        print_bytes(
            putc,
            b"Router event diagnostics: quiet mode suppresses routine DHCP/NAT messages.\n",
        );
    } else {
        print_bytes(
            putc,
            b"Router event diagnostics: DHCP lease and new NAT-flow messages enabled.\n",
        );
    }
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
        NatTable::new(entries, ticks())
    } else {
        // Only genuinely a fallback -- and worth reporting -- when NAT is
        // enabled at all: with `no-nat`, `nat_allocation` is deliberately
        // left at 0 with no allocation ever attempted (see above), and this
        // unused table is never consulted, so printing the same "allocation
        // unavailable" message here as for a real failed `alloc` would
        // falsely suggest the machine is short on physical memory.
        if parsed.nat {
            print_bytes(
                putc,
                b"NAT: physical-memory allocation unavailable; using 32 flow slots.\n",
            );
        }
        NatTable::new(&mut fallback_nat_entries, ticks())
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
            start_health_round(
                &mut uplinks,
                parsed.dhcp_uplink_count,
                &mut interfaces,
                nic_send_at,
                ticks,
                max_frame_len,
            );
        }
        let mut health_changed = false;
        if parsed.health_target.is_some() {
            health_changed |= expire_health_timeouts(&mut uplinks, parsed.dhcp_uplink_count, ticks);
        }
        if parsed.dhcp_uplink_count != 0 {
            health_changed |= poll_dhcp_renewals(
                putc,
                parsed.quiet,
                &mut dhcp_clients,
                &mut interfaces,
                &mut configs,
                interface_count,
                &mut uplinks,
                parsed.dhcp_uplink_count,
                nic_send_at,
                ticks,
                max_frame_len,
            );
        }
        if lease_file_storage.is_some()
            && (lease_file_needs_flush || lease_servers_dirty(&dhcp_servers))
            && ticks().wrapping_sub(last_lease_flush) >= LEASE_FLUSH_MIN_TICKS
        {
            if let Some(name) = lease_file {
                if persist_leases(
                    putc,
                    name,
                    &mut lease_file_storage,
                    &mut lease_file_needs_flush,
                    &interfaces,
                    &mut dhcp_servers,
                    fs_write,
                    free,
                ) {
                    last_lease_flush = ticks();
                }
            }
        }
        let (current_default, active_nic, routing_configs) = compute_forwarding_context(
            &uplinks,
            parsed.dhcp_uplink_count,
            active_uplink,
            parsed.default_route,
            &interfaces,
            interface_count,
            &configs,
            &parsed.dhcp_uplinks,
        );
        let mut received_any = false;
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
            received_any = true;
            let length = min(length, MAX_FRAME_LEN);
            if parsed.dhcp_server && !parsed.dhcp_uplinks[..parsed.dhcp_uplink_count]
                .iter().flatten().any(|uplink| *uplink == interfaces[ingress].nic) {
                if let Some(server) = dhcp_servers[ingress].as_mut() {
                    if let Some(event) = dhcp_reply(
                        &interfaces[ingress],
                        server,
                        &dns_servers,
                        dns_server_count,
                        &receive_buffer[..length],
                        (ticks() / 100) as u32,
                        nic_send_at,
                        max_frame_len,
                    ) {
                        if !parsed.quiet {
                            print_dhcp_lease_event(putc, interfaces[ingress].nic, event);
                        }
                    }
                }
                // Persisting is handled once per outer-loop iteration, rate
                // limited by LEASE_FLUSH_MIN_TICKS below -- not here, right
                // after every single DHCP transaction. A blocking FOSFS
                // fs_write stalls every interface's forwarding for its
                // duration; flushing after each individual lease event (as
                // this used to do) meant a burst of simultaneous lease
                // requests -- many clients booting at once -- triggered one
                // such stall per transaction. The dirty bit this event may
                // have just set (server.is_dirty()) is picked up by that
                // periodic check instead, and unconditionally by the final
                // flush once the router exits.
            }
            if let Some(client) = dhcp_clients[ingress].as_mut() {
                receive_dhcp(&receive_buffer[..length], client, (ticks() / 100) as u32);
            }
            process_arp(
                &mut interfaces[ingress],
                &receive_buffer[..length],
                ticks(),
                nic_send_at,
                max_frame_len,
            );
            if let Some(target) = parsed.health_target {
                health_changed |= advance_uplink_health(
                    &mut uplinks,
                    parsed.dhcp_uplink_count,
                    ingress,
                    &receive_buffer[..length],
                    &mut interfaces,
                    target,
                    nic_send_at,
                    ticks,
                    max_frame_len,
                );
            }
            match forward_ipv4(
                putc,
                parsed.quiet,
                ingress,
                &receive_buffer[..length],
                &mut interfaces,
                &routing_configs,
                interface_count,
                current_default,
                active_nic,
                nic_send_at,
                ticks,
                &mut nat,
                parsed.nat,
                &parsed.forwards,
                parsed.forward_count,
                &mut transport_buffer,
                &mut ip_buffer,
                &mut output_buffer,
                max_frame_len,
            ) {
                ForwardResult::Forwarded => forwarded += 1,
                ForwardResult::Dropped => dropped += 1,
            }
        }
        if health_changed {
            let previous = active_uplink;
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
        // Nothing arrived on any interface: sleep until the next frame or
        // timer tick (at most ~10 ms) instead of spinning. The timed work at
        // the top of this loop (health polls, DHCP renewals, lease flushes)
        // is checked against `ticks()` and still runs within one tick.
        if !received_any {
            wait_for_event();
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
