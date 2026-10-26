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
use tape_net::{ArpCache, ETHERTYPE_IPV4, EthernetFrame, Ipv4Address, Ipv4Packet, MacAddress, TcpConnection, TcpSegment, TcpState};
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
