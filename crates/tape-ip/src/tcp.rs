//! A blocking TCP client socket over the tape ABI's raw frame calls, for tapes that talk to a server (`netcheck tcp=`,
//! the SMB client). It drives `tape_net::TcpConnection`: ARP for the next hop, the handshake, retransmission, one request
//! at a time out and any amount of data in. Like `dns::lookup` it blocks (every wait has a timeout) and it does not read
//! keys, so a tape that wants Escape checks `should_abort` between calls and keeps its timeouts short.
//!
//! The connection's receive buffer (`RX` bytes) is also the window the peer is allowed to fill: segments are
//! acknowledged one by one and in order only (a lost one is resent by the peer after its timeout), which is plenty for a
//! LAN and a few Mbit/s. A `Socket` is large (about `2 * RX` plus four frame buffers), so keep it in a `static`, build it
//! with `Socket::new()` and (re)open it with `connect`.

use crate::net::{self, MAX_FRAME, NetConfig};
use tape_net::{ArpCache, ETHERTYPE_IPV4, EthernetFrame, TCP_ACK, TCP_RST, TCP_SYN, Ipv4Address, Ipv4Packet, MacAddress, TcpConnection, TcpSegment, TcpState};
use tape_rt::TapeApi;

/// How long an unacknowledged segment waits before it is sent again.
const RETRANSMIT_MICROS: u32 = 300_000;
/// The most payload one outgoing segment carries (a frame is 1514 bytes; IPv4 and TCP headers take 40, a margin the rest).
const SEND_CHUNK: usize = 1400;

pub struct Socket<const RX: usize> {
    connection: TcpConnection<RX>,
    arp: ArpCache<8>,
    nic: usize,
    config: NetConfig,
    peer: Ipv4Address,
    peer_mac: MacAddress,
    rx: [u8; MAX_FRAME],
    segment: [u8; MAX_FRAME],
    tx: [u8; MAX_FRAME],
}

impl<const RX: usize> Socket<RX> {
    pub const fn new() -> Self {
        Socket {
            connection: TcpConnection::active(Ipv4Address([0; 4]), 0, Ipv4Address([0; 4]), 0, 0, RETRANSMIT_MICROS),
            arp: ArpCache::new(),
            nic: 0,
            config: NetConfig { mac: MacAddress([0; 6]), ip: Ipv4Address([0; 4]), netmask: Ipv4Address([0; 4]), gateway: Ipv4Address([0; 4]) },
            peer: Ipv4Address([0; 4]),
            peer_mac: MacAddress([0; 6]),
            rx: [0; MAX_FRAME],
            segment: [0; MAX_FRAME],
            tx: [0; MAX_FRAME],
        }
    }

    /// Opens a connection to `remote:port` through `nic`. Errors are sentences for the tape to print.
    pub fn connect(&mut self, api: &TapeApi, nic: usize, config: &NetConfig, remote: Ipv4Address, port: u16, timeout_micros: u64) -> Result<(), &'static str> {
        self.nic = nic;
        self.config = *config;
        self.peer = remote;
        self.peer_mac = net::resolve(api, nic, config, &mut self.arp, remote).ok_or("no ARP answer from the host (or its gateway)")?;
        let start = tape_rt::micros();
        // A fresh local port and sequence number for every connection: a server that still holds the previous one in
        // TIME_WAIT ignores a SYN on the same port with a lower sequence number.
        let local_port = 40_000 + ((start as u32).wrapping_mul(2_654_435_761) >> 16) as u16 % 20_000;
        let sequence = (start as u32) ^ u32::from_be_bytes([config.mac.0[0], config.mac.0[1], config.mac.0[2], config.mac.0[3]]);
        self.connection.reopen(config.ip, local_port, remote, port, sequence, RETRANSMIT_MICROS);
        let length = self.connection.start_connect(start as u32, &mut self.segment).map_err(|_| "cannot build the SYN")?;
        self.transmit(api, length);
        while self.connection.state() != TcpState::Established {
            if self.connection.state() == TcpState::Closed {
                return Err("the connection was refused");
            }
            if tape_rt::micros() > start + timeout_micros {
                return Err("the connection timed out");
            }
            self.step(api);
        }
        Ok(())
    }

    pub fn is_open(&self) -> bool {
        self.connection.state() == TcpState::Established
    }

    /// Sends `data` and returns once the peer has acknowledged all of it (one segment at a time).
    pub fn send(&mut self, api: &TapeApi, data: &[u8], timeout_micros: u64) -> Result<(), &'static str> {
        for chunk in data.chunks(RX.min(SEND_CHUNK)) {
            let length = self.connection.send_data(chunk, tape_rt::micros() as u32, &mut self.segment).map_err(|_| "the connection is not open")?;
            self.transmit(api, length);
            let deadline = tape_rt::micros() + timeout_micros;
            while self.connection.retransmission_deadline().is_some() {
                if self.connection.state() == TcpState::Closed {
                    return Err("the connection was reset");
                }
                if tape_rt::micros() > deadline {
                    return Err("the peer did not acknowledge");
                }
                self.step(api);
            }
        }
        Ok(())
    }

    /// Waits for data and copies what has arrived (at most `out.len()` bytes) to `out`. `Ok(0)` means the peer closed.
    pub fn recv(&mut self, api: &TapeApi, out: &mut [u8], timeout_micros: u64) -> Result<usize, &'static str> {
        let deadline = tape_rt::micros() + timeout_micros;
        loop {
            let count = self.connection.take_received(out);
            if count > 0 {
                return Ok(count);
            }
            if !matches!(self.connection.state(), TcpState::Established | TcpState::FinWait1 | TcpState::FinWait2) {
                return Ok(0);
            }
            if tape_rt::micros() > deadline {
                return Err("timed out waiting for data");
            }
            self.step(api);
        }
    }

    /// Fills `out` completely. `timeout_micros` is how long the peer may stay silent between pieces.
    pub fn recv_exact(&mut self, api: &TapeApi, out: &mut [u8], timeout_micros: u64) -> Result<(), &'static str> {
        let mut filled = 0;
        while filled < out.len() {
            match self.recv(api, &mut out[filled..], timeout_micros)? {
                0 => return Err("the connection closed early"),
                count => filled += count,
            }
        }
        Ok(())
    }

    /// Says goodbye (FIN) and waits a moment for the acknowledgement. The socket can be `connect`ed again afterwards.
    pub fn close(&mut self, api: &TapeApi) {
        if matches!(self.connection.state(), TcpState::Established | TcpState::CloseWait) {
            if let Ok(length) = self.connection.close(tape_rt::micros() as u32, &mut self.segment) {
                self.transmit(api, length);
                let deadline = tape_rt::micros() + 500_000;
                while self.connection.retransmission_deadline().is_some() && tape_rt::micros() < deadline {
                    self.step(api);
                }
            }
        }
    }

    /// Handles one received frame (ARP is answered, our connection's segments are processed and acknowledged) or idles,
    /// then resends the outstanding segment if its timer is due.
    fn step(&mut self, api: &TapeApi) {
        let length = (api.nic_receive)(self.nic, self.rx.as_mut_ptr(), self.rx.len());
        let now = tape_rt::micros() as u32;
        if length == 0 {
            tape_rt::idle();
        } else if let Ok(ethernet) = EthernetFrame::parse(&self.rx[..length]) {
            if ethernet.ethertype != ETHERTYPE_IPV4 {
                let _ = net::inbound(api, self.nic, &self.config, &mut self.arp, &self.rx[..length], 0..=0, now);
            } else if let Ok(packet) = Ipv4Packet::parse(ethernet.payload, true) {
                if packet.header.protocol == 6 && packet.header.source == self.peer && packet.header.destination == self.config.ip {
                    if let Ok(segment) = TcpSegment::parse(packet.payload, self.peer, self.config.ip, true) {
                        if let Ok(Some(reply)) = self.connection.on_segment(&segment, now, &mut self.segment) {
                            self.transmit(api, reply);
                        }
                    }
                }
            }
        }
        if let Ok(Some(resend)) = self.connection.retransmit(now, &mut self.segment) {
            self.transmit(api, resend);
        }
    }

    /// Sends the TCP segment in `self.segment[..length]` as an Ethernet frame to the peer.
    fn transmit(&mut self, api: &TapeApi, length: usize) {
        if let Some(total) = net::build_ip_frame(&mut self.tx, &self.config, self.peer_mac, self.peer, 6, &self.segment[..length]) {
            net::send_frame(api, self.nic, &self.tx[..total]);
        }
    }
}

/// How long a connection may sit without a segment before its slot is taken back: one still being set up or closed (a
/// lost handshake, a peer that vanished mid-close), and an established one that has gone quiet.
const HALF_OPEN_MICROS: u64 = 3_000_000;
const IDLE_MICROS: u64 = 10_000_000;
/// With every slot taken, a SYN may evict the quietest established connection if it has been silent this long.
const EVICT_MICROS: u64 = 1_000_000;
/// The most unacknowledged bytes `Server::send` keeps in flight (the peer's window may be larger).
/// How long `send_buffered` spins for the next acknowledgement before it sleeps.
const ACK_SPIN_MICROS: u64 = 2_000;
const FLIGHT: usize = 44 * SEND_CHUNK;

/// What `send_buffered` keeps per slot: a copy of the bytes sent and not yet acknowledged (the newest `stream_in_flight()`
/// bytes before `tx_len`), so a send can return while its tail is still in flight and the caller reuses its buffer.
const TX_KEEP: usize = 2 * FLIGHT;

struct Slot<const RX: usize> {
    connection: TcpConnection<RX>,
    peer_mac: MacAddress,
    last: u64,
    tx_keep: [u8; TX_KEEP],
    tx_len: usize,
}

/// A TCP server for tapes: `N` connections at once on one port, each with an `RX`-byte receive buffer (also its window).
/// Like `Socket` it is blocking where data goes out (`send` waits for the peer's acknowledgements, serving the other slots
/// meanwhile) and polled where it comes in: call `poll` in the tape's loop, then look at the slots. A slot is `Listen`
/// until a SYN takes it; `recycle` (or an idle timeout, or a reset) gives it back. Replies go to the MAC the peer's
/// frames came from, so no ARP exchange is needed to answer. Keep one in a `static`: it is large.
pub struct Server<const RX: usize, const N: usize> {
    slots: [Slot<RX>; N],
    arp: ArpCache<8>,
    nic: usize,
    config: NetConfig,
    port: u16,
    sequence: u32,
    rx: [u8; MAX_FRAME],
    segment: [u8; MAX_FRAME],
    tx: [u8; MAX_FRAME],
    tx_dropped: u32,
    idle_micros: u64,
}

impl<const RX: usize, const N: usize> Server<RX, N> {
    pub const fn new() -> Self {
        const NO_ADDRESS: Ipv4Address = Ipv4Address([0; 4]);
        Server {
            slots: [const { Slot { connection: TcpConnection::listen(NO_ADDRESS, 0, NO_ADDRESS, 0, 0, RETRANSMIT_MICROS), peer_mac: MacAddress([0; 6]), last: 0, tx_keep: [0; TX_KEEP], tx_len: 0 } }; N],
            arp: ArpCache::new(),
            nic: 0,
            config: NetConfig { mac: MacAddress([0; 6]), ip: NO_ADDRESS, netmask: NO_ADDRESS, gateway: NO_ADDRESS },
            port: 0,
            sequence: 0,
            rx: [0; MAX_FRAME],
            segment: [0; MAX_FRAME],
            tx: [0; MAX_FRAME],
            tx_dropped: 0,
            idle_micros: IDLE_MICROS,
        }
    }

    /// How long an established connection may be silent before its slot is taken back (default 10 s). A server whose
    /// clients idle on purpose (an SMB mount sends an echo every 60 s) sets it above that; a SYN that finds every slot
    /// taken still evicts the quietest connection, so a vanished client cannot lock the others out.
    pub fn set_idle(&mut self, micros: u64) {
        self.idle_micros = micros;
    }

    /// Starts listening on `port` through `nic` with the address `config` gives.
    pub fn start(&mut self, nic: usize, config: &NetConfig, port: u16) {
        self.nic = nic;
        self.config = *config;
        self.port = port;
        self.sequence = (tape_rt::micros() as u32) ^ u32::from_be_bytes([config.mac.0[2], config.mac.0[3], config.mac.0[4], config.mac.0[5]]);
        for index in 0..N {
            self.recycle(index);
        }
    }

    pub fn state(&self, slot: usize) -> TcpState {
        self.slots[slot].connection.state()
    }

    /// The peer's address and port of a connected slot.
    pub fn peer(&self, slot: usize) -> (Ipv4Address, u16) {
        self.slots[slot].connection.remote()
    }

    /// What the peer has sent so far and not been taken yet.
    pub fn received(&self, slot: usize) -> &[u8] {
        self.slots[slot].connection.received()
    }

    pub fn take(&mut self, api: &TapeApi, slot: usize, out: &mut [u8]) -> usize {
        let connection = &mut self.slots[slot].connection;
        let closed = connection.advertised_window() == 0;
        let count = connection.take_received(out);
        // The window we last offered was 0: the peer is waiting to hear that room is back.
        if closed && count > 0 && connection.advertised_window() > 0 {
            if let Ok(length) = connection.window_update(&mut self.segment) {
                self.transmit(api, slot, length);
            }
        }
        count
    }

    /// True while the peer can still be answered (the connection is established, perhaps half-closed by the peer).
    pub fn is_open(&self, slot: usize) -> bool {
        matches!(self.slots[slot].connection.state(), TcpState::Established | TcpState::CloseWait)
    }

    /// True once the peer has said goodbye (FIN): it sends nothing more, and until we `close` too the slot stays taken.
    pub fn peer_closed(&self, slot: usize) -> bool {
        self.slots[slot].connection.state() == TcpState::CloseWait
    }

    /// Gives the slot back: it listens for a new connection.
    pub fn recycle(&mut self, slot: usize) {
        self.sequence = self.sequence.wrapping_add(64_000);
        let (ip, port, sequence) = (self.config.ip, self.port, self.sequence);
        self.slots[slot].connection.reopen_listen(ip, port, sequence, RETRANSMIT_MICROS);
        self.slots[slot].last = tape_rt::micros();
        self.slots[slot].tx_len = 0;
    }

    /// Handles the frames waiting (up to `budget` of them), resends what is due, takes back dead slots. Returns whether
    /// any frame arrived, so a tape can idle when nothing did.
    pub fn poll(&mut self, api: &TapeApi, budget: usize) -> bool {
        let mut any = false;
        for _ in 0..budget {
            let length = (api.nic_receive)(self.nic, self.rx.as_mut_ptr(), self.rx.len());
            if length == 0 {
                break;
            }
            any = true;
            self.handle(api, length);
        }
        self.housekeeping(api);
        any
    }

    fn handle(&mut self, api: &TapeApi, length: usize) {
        let now = tape_rt::micros();
        let Ok(ethernet) = EthernetFrame::parse(&self.rx[..length]) else { return };
        if ethernet.ethertype != ETHERTYPE_IPV4 {
            let _ = net::inbound(api, self.nic, &self.config, &mut self.arp, &self.rx[..length], 0..=0, now as u32);
            return;
        }
        let source_mac = ethernet.source;
        let Ok(packet) = Ipv4Packet::parse(ethernet.payload, true) else { return };
        if packet.header.protocol != 6 || packet.header.destination != self.config.ip {
            return;
        }
        let peer = packet.header.source;
        let Ok(segment) = TcpSegment::parse(packet.payload, peer, self.config.ip, true) else { return };
        if segment.destination_port != self.port {
            return;
        }
        let existing = (0..N).find(|&i| {
            let (ip, port) = self.slots[i].connection.remote();
            self.slots[i].connection.state() != TcpState::Listen && ip == peer && port == segment.source_port
        });
        let index = match existing {
            Some(index) => index,
            None if segment.flags & TCP_SYN != 0 => match (0..N).find(|&i| self.slots[i].connection.state() == TcpState::Listen) {
                Some(index) => {
                    self.slots[index].connection.accept_from(peer);
                    self.slots[index].peer_mac = source_mac;
                    index
                }
                None => {
                    let quietest = (0..N)
                        .filter(|&i| matches!(self.slots[i].connection.state(), TcpState::Established | TcpState::CloseWait))
                        .max_by_key(|&i| now.saturating_sub(self.slots[i].last));
                    match quietest.filter(|&i| now.saturating_sub(self.slots[i].last) > EVICT_MICROS) {
                        Some(index) => {
                            // `recycle` inline: `segment` still borrows `self.rx`.
                            self.sequence = self.sequence.wrapping_add(64_000);
                            self.slots[index].connection.reopen_listen(self.config.ip, self.port, self.sequence, RETRANSMIT_MICROS);
                            self.slots[index].tx_len = 0;
                            self.slots[index].connection.accept_from(peer);
                            self.slots[index].peer_mac = source_mac;
                            index
                        }
                        None => return,
                    }
                }
            },
            None => {
                // A segment for a connection we no longer have (an idle slot taken back): tell the peer at once, or it
                // retransmits into silence for minutes before it gives up.
                if segment.flags & TCP_RST == 0 {
                    let (sequence, acknowledgment, flags) = if segment.flags & TCP_ACK != 0 {
                        (segment.acknowledgment, 0, TCP_RST)
                    } else {
                        (0, segment.sequence.wrapping_add(segment.data.len() as u32 + u32::from(segment.flags & TCP_SYN != 0)), TCP_RST | TCP_ACK)
                    };
                    let reset = TcpSegment::new(self.port, segment.source_port, sequence, acknowledgment, flags, 0, &[]);
                    if let Ok(length) = reset.encode(&mut self.segment, self.config.ip, peer) {
                        if let Some(total) = net::build_ip_frame(&mut self.tx, &self.config, source_mac, peer, 6, &self.segment[..length]) {
                            net::send_frame(api, self.nic, &self.tx[..total]);
                        }
                    }
                }
                return;
            }
        };
        self.slots[index].last = now;
        if let Ok(Some(reply)) = self.slots[index].connection.on_segment(&segment, now as u32, &mut self.segment) {
            self.transmit(api, index, reply);
        }
        // Three duplicate acknowledgements: resend the oldest kept segment at once (`send_buffered` keeps a copy).
        if self.slots[index].connection.take_fast_retransmit() {
            let slot = &mut self.slots[index];
            let (kept, in_flight) = (slot.tx_len, slot.connection.stream_in_flight());
            if in_flight > 0 && in_flight <= kept {
                if let Ok(Some(resend)) = slot.connection.stream_resend(&slot.tx_keep[kept - in_flight..kept][..in_flight.min(SEND_CHUNK)], &mut self.segment) {
                    self.transmit(api, index, resend);
                }
            }
        }
    }

    fn housekeeping(&mut self, api: &TapeApi) {
        let now = tape_rt::micros();
        for index in 0..N {
            let state = self.slots[index].connection.state();
            let quiet = now.saturating_sub(self.slots[index].last);
            let dead = match state {
                TcpState::Listen => false,
                TcpState::Closed => self.slots[index].connection.received().is_empty(),
                TcpState::Established | TcpState::CloseWait => quiet > self.idle_micros,
                _ => quiet > HALF_OPEN_MICROS,
            };
            if dead {
                self.recycle(index);
                continue;
            }
            if state != TcpState::Listen {
                if let Ok(Some(resend)) = self.slots[index].connection.retransmit(now as u32, &mut self.segment) {
                    self.transmit(api, index, resend);
                }
                // Data `send_buffered` returned from: nobody is waiting on it, so resending it is up to this pass.
                let (kept, in_flight) = (self.slots[index].tx_len, self.slots[index].connection.stream_in_flight());
                if in_flight > 0 && in_flight <= kept {
                    let slot = &mut self.slots[index];
                    if let Ok(Some(resend)) = slot.connection.stream_retransmit(&slot.tx_keep[kept - in_flight..kept][..in_flight.min(SEND_CHUNK)], now as u32, &mut self.segment) {
                        self.transmit(api, index, resend);
                    }
                }
            }
        }
    }

    /// Sends all of `data` and returns once the peer has acknowledged it (several segments in flight at a time).
    /// `timeout_micros` is how long the peer may go without acknowledging anything new.
    pub fn send(&mut self, api: &TapeApi, slot: usize, data: &[u8], timeout_micros: u64) -> Result<(), &'static str> {
        self.slots[slot].tx_len = 0;
        let mut sent = 0;
        let mut progress = tape_rt::micros();
        while sent < data.len() || self.slots[slot].connection.stream_in_flight() > 0 {
            if !self.is_open(slot) {
                return Err("the connection was closed");
            }
            let in_flight = self.slots[slot].connection.stream_in_flight();
            let window = self.slots[slot].connection.peer_window().min(FLIGHT).max(1);
            if sent < data.len() && in_flight < window {
                let chunk = (data.len() - sent).min(SEND_CHUNK).min(RX).min(window - in_flight);
                let now = tape_rt::micros() as u32;
                let length = self.slots[slot].connection.stream_send(&data[sent..sent + chunk], now, &mut self.segment).map_err(|_| "the connection is not open")?;
                self.transmit(api, slot, length);
                sent += chunk;
                progress = tape_rt::micros();
                self.poll(api, 4);
                continue;
            }
            if !self.poll(api, 8) {
                tape_rt::idle();
            }
            let now = tape_rt::micros();
            let remaining = self.slots[slot].connection.stream_in_flight();
            if remaining < in_flight {
                progress = now;
            }
            if remaining > 0 {
                let unacknowledged = &data[sent - remaining..sent];
                if let Ok(Some(resend)) = self.slots[slot].connection.stream_retransmit(&unacknowledged[..unacknowledged.len().min(SEND_CHUNK)], now as u32, &mut self.segment) {
                    self.transmit(api, slot, resend);
                }
            }
            if now > progress + timeout_micros {
                return Err("the peer did not acknowledge");
            }
        }
        self.slots[slot].last = tape_rt::micros();
        Ok(())
    }

    /// Like `send`, but returns as soon as everything has been handed to the card, keeping a copy of what is still
    /// unacknowledged: the next message goes out behind it without the flight draining in between (one round trip per
    /// message otherwise, most of the time of a bulk transfer on a LAN). Retransmission of the tail happens in `poll`;
    /// `close` waits for it. Waits only when `FLIGHT` bytes are out, or for acknowledgements to make room.
    pub fn send_buffered(&mut self, api: &TapeApi, slot: usize, data: &[u8], timeout_micros: u64) -> Result<(), &'static str> {
        let mut sent = 0;
        let mut progress = tape_rt::micros();
        let mut acked = progress;
        while sent < data.len() {
            let before = self.slots[slot].connection.stream_in_flight();
            let taken = self.send_some(api, slot, &data[sent..])?;
            sent += taken;
            if taken > 0 {
                progress = tape_rt::micros();
                continue;
            }
            let busy = self.poll(api, 8);
            let now = tape_rt::micros();
            // `idle` sleeps until the next interrupt, a whole timer tick on the server kernel: a full flight then moved once per
            // tick (about 6 MB/s). The acknowledgements of a LAN come within a round trip, so spin while they keep coming.
            if !busy && now > acked + ACK_SPIN_MICROS {
                tape_rt::idle();
            }
            if self.slots[slot].connection.stream_in_flight() < before {
                progress = now;
                acked = now;
            }
            if now > progress + timeout_micros {
                return Err("the peer did not acknowledge");
            }
        }
        self.slots[slot].last = tape_rt::micros();
        Ok(())
    }

    /// The part of `send_buffered` that never waits: hands as much of `data` to the card as the peer's window and `FLIGHT`
    /// allow right now and returns how many bytes that was (0 when the flight is full). A server with several connections
    /// calls it from its main loop and keeps serving the others while the acknowledgements come in; the caller gives up on
    /// a peer that takes nothing for too long.
    pub fn send_some(&mut self, api: &TapeApi, slot: usize, data: &[u8]) -> Result<usize, &'static str> {
        let mut sent = 0;
        while sent < data.len() {
            if !self.is_open(slot) {
                return Err("the connection was closed");
            }
            let in_flight = self.slots[slot].connection.stream_in_flight();
            let window = self.slots[slot].connection.peer_window().min(FLIGHT).max(1);
            if in_flight >= window {
                break;
            }
            let chunk = (data.len() - sent).min(SEND_CHUNK).min(RX).min(window - in_flight);
            let entry = &mut self.slots[slot];
            // `tx_len` can be stale (older than what is in flight) after a plain `send`; with nothing in flight start over.
            let mut kept = if in_flight == 0 || entry.tx_len < in_flight { 0 } else { entry.tx_len };
            if kept < in_flight {
                return Err("the connection has data in flight that is not kept");
            }
            if kept + chunk > TX_KEEP {
                entry.tx_keep.copy_within(kept - in_flight..kept, 0);
                kept = in_flight;
            }
            entry.tx_keep[kept..kept + chunk].copy_from_slice(&data[sent..sent + chunk]);
            entry.tx_len = kept + chunk;
            let now = tape_rt::micros() as u32;
            let length = entry.connection.stream_send(&data[sent..sent + chunk], now, &mut self.segment).map_err(|_| "the connection is not open")?;
            self.transmit(api, slot, length);
            sent += chunk;
            self.poll(api, 4);
        }
        if sent > 0 {
            self.slots[slot].last = tape_rt::micros();
        }
        Ok(sent)
    }

    /// Says goodbye (FIN) and waits a moment for the acknowledgement; the slot is taken back once the peer has closed too
    /// (or by the idle timeout).
    pub fn close(&mut self, api: &TapeApi, slot: usize) {
        // Whatever `send_buffered` left in flight goes first: a FIN in the way would stop its retransmission.
        let drain_until = tape_rt::micros() + 2_000_000;
        while self.is_open(slot) && self.slots[slot].connection.stream_in_flight() > 0 && tape_rt::micros() < drain_until {
            if !self.poll(api, 8) {
                tape_rt::idle();
            }
        }
        if let Ok(length) = self.slots[slot].connection.close(tape_rt::micros() as u32, &mut self.segment) {
            self.transmit(api, slot, length);
            let deadline = tape_rt::micros() + 500_000;
            while self.slots[slot].connection.retransmission_deadline().is_some() && tape_rt::micros() < deadline {
                if !self.poll(api, 8) {
                    tape_rt::idle();
                }
            }
        }
    }

    /// Answers a peer's FIN with ours and returns at once; `poll` resends it if needed and takes the slot back when it is
    /// acknowledged. For a server with other connections waiting, unlike `close`, which waits for that acknowledgement.
    pub fn finish(&mut self, api: &TapeApi, slot: usize) {
        if let Ok(length) = self.slots[slot].connection.close(tape_rt::micros() as u32, &mut self.segment) {
            self.transmit(api, slot, length);
        }
    }

    fn transmit(&mut self, api: &TapeApi, slot: usize, length: usize) {
        let (peer, _) = self.slots[slot].connection.remote();
        if let Some(total) = net::build_ip_frame(&mut self.tx, &self.config, self.slots[slot].peer_mac, peer, 6, &self.segment[..length]) {
            // A full transmit ring refuses the frame: a data segment dropped here is a loss the peer only notices as a gap,
            // so wait for the card to take it (bounded) before counting it as lost.
            let mut tries = 0;
            while !net::send_frame(api, self.nic, &self.tx[..total]) {
                tries += 1;
                if tries > 200 {
                    self.tx_dropped += 1;
                    break;
                }
                tape_rt::idle();
            }
        }
    }

    /// Frames the card refused even after waiting (a loss `send_buffered` then has to make good by retransmitting).
    pub fn tx_dropped(&self) -> u32 {
        self.tx_dropped
    }
}
