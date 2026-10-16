//! Two-machine tests: a **master** and a **server**, both running nettest, talk over unicast Ethernet frames
//! (ethertype 0x88B5, magic `FNC1`; no IP, so it works on a bare switch or cable) and measure the physical
//! network between them. The master picks a peer, measures the clock offset, tells the server what to do
//! (stream up, down, or both), and fetches the server's counters at the end for one merged report.
//!
//! Everything is unicast to the chosen peer, so other machines on the network are not disturbed. Finding
//! servers: the master sends a unicast HELLO to every machine in the kernel's announced-name table; those
//! running `nettest serve` answer.
//!
//! **Clock.** One-way latency needs both clocks on one scale. HELLO / HELLO_REPLY is the IEEE 1588 end-to-end
//! delay request-response exchange (the same arithmetic as the `ptp` tape's slave): the master stamps `t1`, the
//! server stamps `t2` on receipt and `t3` on reply, the master stamps `t4`:
//! `offset = ((t2 - t1) + (t3 - t4)) / 2` (server clock minus master clock), `rtt = (t4 - t1) - (t3 - t2)`.
//! Eight samples are taken and the one with the smallest round trip wins, because queueing only ever adds
//! delay. The result is exact only if the path is symmetric; the error is at most half the round trip, which
//! is printed. The `ptp` tape cannot run at the same time (one tape at a time, and it does not steer `micros()`).
//! Throughput, loss, reordering and jitter do not depend on the offset at all.
//!
//! Frame: `dst src 88B5 "FNC1" type version session(u16) seq(u32) payload`, little-endian, zero padded.

use crate::{Generator, HISTOGRAM_BUCKETS, NIC_COUNTERS, Sink, SinkStats, TIMELINE_BINS, counter_delta};

pub const CTRL_ETHERTYPE: [u8; 2] = [0x88, 0xb5];
const MAGIC: &[u8; 4] = b"FNC1";
const VERSION: u8 = 3;
const HEADER: usize = 26;

const T_HELLO: u8 = 1;
const T_HELLO_REPLY: u8 = 2;
const T_START: u8 = 3;
const T_START_ACK: u8 = 4;
const T_QUERY: u8 = 5;
const T_REPORT: u8 = 6;
const T_STOP: u8 = 7;

pub const MAX_FOUND: usize = 8;
/// NICs a machine lists in its HELLO reply (the master picks the server's test NIC from them).
pub const MAX_NICS: usize = 6;
/// START's "no separate test NIC": the test uses the control NIC.
const VIA_CONTROL: u8 = 0xff;
const MAX_PENDING: usize = 16;
const SYNC_WANTED: u8 = 8;
const SYNC_MAX_SENDS: u8 = 16;
const SYNC_SPACING_US: u64 = 20_000;
const SYNC_LAST_WAIT_US: u64 = 200_000;
const DISCOVER_US: u64 = 1_500_000;
const RETRY_US: u64 = 300_000;
const RETRIES: u8 = 6;
/// After the generators' time is up: let the last frames arrive before the counters are read.
const DRAIN_US: u64 = 1_500_000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    Master,
    Server,
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Found {
    pub mac: [u8; 6],
    pub rtt_us: u32,
}

/// What a test does. `seconds` is how long each generator runs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TestParams {
    /// Master to server stream.
    pub up: bool,
    /// Server to master stream.
    pub down: bool,
    pub rate_kbit: u32,
    pub size: u16,
    pub seconds: u16,
    /// The server's test NIC, one-based; 0 = the NIC the control frames use.
    pub via: u8,
    /// The master's own test NIC, one-based; 0 = its control NIC. The master's main loop reads it.
    pub here: u8,
    /// Frames each generator sends back to back (a burst test); 0 = send for `seconds` at `rate_kbit`.
    pub count: u32,
}

impl TestParams {
    pub const fn new() -> Self {
        Self { up: true, down: true, rate_kbit: 10_000, size: 1000, seconds: 5, via: 0, here: 0, count: 0 }
    }
}

/// The server's counters, as it reports them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PeerReport {
    pub sink: SinkStats,
    pub gen_sent: u32,
    pub gen_failed: u32,
    /// The server's test NIC over the test: link speed and duplex, then the change in each error counter
    /// (`tape_rt::nic_counters` order). All zero from a server that does not report them.
    pub counters: [u64; NIC_COUNTERS],
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Event {
    Found(Found),
    DiscoveryDone(u8),
    Synced { rtt_us: u32, offset_us: i64, samples: u8 },
    Started,
    Finished,
    Failed(&'static str),
    /// Server side: a master started a test.
    Serving { up: bool, down: bool },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Idle,
    Discover { until: u64 },
    Sync,
    Ready,
    Starting,
    Running { until: u64 },
    Querying,
}

pub struct Remote {
    pub role: Role,
    my_mac: [u8; 6],
    session: u16,
    pub peer: Option<[u8; 6]>,
    phase: Phase,
    pub found: [Found; MAX_FOUND],
    pub found_count: usize,
    pending: [[u8; 6]; MAX_PENDING],
    pending_len: usize,
    pending_next: usize,
    /// Server clock minus master clock, and the round trip it was measured with.
    pub offset_us: i64,
    pub rtt_us: u32,
    sync_sent: u8,
    sync_replies: u8,
    best_rtt: u32,
    best_offset: i64,
    next_at: u64,
    pub test: TestParams,
    seq: u32,
    tries: u8,
    pub report: Option<PeerReport>,
    events: [Option<Event>; 8],
    /// Server: the master's MAC and the START already acted on (a repeated one is only acknowledged again).
    master: [u8; 6],
    last_start: Option<u32>,
    /// This machine's NICs (set by the tape), and the peer's as its HELLO reply listed them.
    nics: [[u8; 6]; MAX_NICS],
    nic_count: usize,
    pub peer_nics: [[u8; 6]; MAX_NICS],
    pub peer_nic_count: usize,
    /// Server: the NIC (zero-based) the running test's streams use, if not the control NIC.
    pub test_nic: Option<u8>,
    /// Master: the master's test-NIC MAC, and where the next frame from `poll` must go.
    here_mac: [u8; 6],
    sync_target: [u8; 6],
    sync_path: bool,
    /// `poll` just built a frame for the test path (the master's `here` NIC), not the control path.
    pub out_on_test_path: bool,
    /// The tape's reading of its test NIC's counters, refreshed before a control frame is handled, and the reading taken
    /// when the test started. A server reports their difference; a master keeps its own in `own_counters`.
    pub local_counters: [u64; NIC_COUNTERS],
    pub start_counters: [u64; NIC_COUNTERS],
    pub own_counters: [u64; NIC_COUNTERS],
}

struct Writer<'a> {
    bytes: &'a mut [u8],
    at: usize,
}

impl Writer<'_> {
    fn put(&mut self, data: &[u8]) {
        self.bytes[self.at..self.at + data.len()].copy_from_slice(data);
        self.at += data.len();
    }
    fn u8(&mut self, value: u8) {
        self.put(&[value]);
    }
    fn u16(&mut self, value: u16) {
        self.put(&value.to_le_bytes());
    }
    fn u32(&mut self, value: u32) {
        self.put(&value.to_le_bytes());
    }
    fn u64(&mut self, value: u64) {
        self.put(&value.to_le_bytes());
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
        let slice = self.bytes.get(self.at..self.at + N)?;
        self.at += N;
        slice.try_into().ok()
    }
    fn u8(&mut self) -> Option<u8> {
        self.take::<1>().map(|b| b[0])
    }
    fn u16(&mut self) -> Option<u16> {
        self.take().map(u16::from_le_bytes)
    }
    fn u32(&mut self) -> Option<u32> {
        self.take().map(u32::from_le_bytes)
    }
    fn u64(&mut self) -> Option<u64> {
        self.take().map(u64::from_le_bytes)
    }
}

impl Remote {
    pub fn new(role: Role, my_mac: [u8; 6], session: u16) -> Self {
        Self {
            role,
            my_mac,
            session,
            peer: None,
            phase: Phase::Idle,
            found: [Found::default(); MAX_FOUND],
            found_count: 0,
            pending: [[0; 6]; MAX_PENDING],
            pending_len: 0,
            pending_next: 0,
            offset_us: 0,
            rtt_us: 0,
            sync_sent: 0,
            sync_replies: 0,
            best_rtt: u32::MAX,
            best_offset: 0,
            next_at: 0,
            test: TestParams::new(),
            seq: 0,
            tries: 0,
            report: None,
            events: [None; 8],
            master: [0; 6],
            last_start: None,
            nics: [[0; 6]; MAX_NICS],
            nic_count: 0,
            peer_nics: [[0; 6]; MAX_NICS],
            peer_nic_count: 0,
            test_nic: None,
            here_mac: my_mac,
            sync_target: [0; 6],
            sync_path: false,
            out_on_test_path: false,
            local_counters: [0; NIC_COUNTERS],
            start_counters: [0; NIC_COUNTERS],
            own_counters: [0; NIC_COUNTERS],
        }
    }

    /// This machine's NIC MACs in NIC order; the HELLO reply lists them for the master.
    pub fn set_nics(&mut self, macs: &[[u8; 6]]) {
        self.nic_count = macs.len().min(MAX_NICS);
        self.nics[..self.nic_count].copy_from_slice(&macs[..self.nic_count]);
    }

    /// The MAC a test stream of the running test uses on the peer: its test NIC, else the control peer.
    fn peer_test_mac(&self) -> Option<[u8; 6]> {
        let peer = self.peer?;
        match self.test.via {
            0 => Some(peer),
            via => self.peer_nics[..self.peer_nic_count].get(usize::from(via) - 1).copied(),
        }
    }

    /// True when the test uses another path than the control frames (a different NIC at either end).
    fn separate_path(params: &TestParams) -> bool {
        params.via != 0 || params.here != 0
    }

    pub fn take_event(&mut self) -> Option<Event> {
        let first = self.events.iter().position(|event| event.is_some())?;
        let event = self.events[first].take();
        // Keep order: shift the rest down.
        for index in first..self.events.len() - 1 {
            self.events[index] = self.events[index + 1].take();
        }
        event
    }

    fn push_event(&mut self, event: Event) {
        if let Some(slot) = self.events.iter_mut().find(|slot| slot.is_none()) {
            *slot = Some(event);
        }
    }

    /// True while a test is starting, running or being read out.
    pub fn busy(&self) -> bool {
        matches!(self.phase, Phase::Starting | Phase::Running { .. } | Phase::Querying) || (self.phase == Phase::Sync && self.sync_path)
    }

    pub fn ready(&self) -> bool {
        self.phase == Phase::Ready
    }

    pub fn syncing(&self) -> bool {
        self.phase == Phase::Sync
    }

    /// Probe these machines (unicast HELLO each); the ones running a server answer and land in `found`.
    pub fn discover(&mut self, macs: &[[u8; 6]], now_us: u64) {
        if self.role != Role::Master || self.busy() {
            return;
        }
        self.found_count = 0;
        self.pending_len = macs.len().min(MAX_PENDING);
        self.pending[..self.pending_len].copy_from_slice(&macs[..self.pending_len]);
        self.pending_next = 0;
        self.phase = Phase::Discover { until: now_us + DISCOVER_US };
    }

    /// Choose the peer and measure the clock offset (the `Synced` event comes when done).
    pub fn select(&mut self, mac: [u8; 6], now_us: u64) {
        if self.role != Role::Master || self.busy() {
            return;
        }
        self.peer = Some(mac);
        self.peer_nic_count = 0;
        self.sync_target = mac;
        self.sync_path = false;
        self.phase = Phase::Sync;
        self.sync_sent = 0;
        self.sync_replies = 0;
        self.best_rtt = u32::MAX;
        self.next_at = now_us;
    }

    /// Master: begin a test. The generator starts when the server acknowledges.
    pub fn start_test(
        &mut self,
        params: TestParams,
        now_us: u64,
        generator: &mut Generator,
        sink: &mut Sink,
        here_mac: [u8; 6],
    ) -> Result<(), &'static str> {
        let Some(peer) = self.peer.filter(|_| self.phase == Phase::Ready) else {
            return Err("no peer is ready: pick one with 'peer' first");
        };
        if params.via != 0 && usize::from(params.via) > self.peer_nic_count {
            return Err("the peer has no such NIC (its NICs are listed when the peer is picked)");
        }
        self.test = params;
        self.here_mac = here_mac;
        let target = self.peer_test_mac().unwrap_or(peer);
        generator.stop();
        generator.rate_kbit = params.rate_kbit;
        generator.size = usize::from(params.size);
        generator.count_limit = params.count;
        generator.seconds_limit = u32::from(params.seconds);
        generator.destination = target;
        sink.reset();
        sink.clock_offset_us = -self.offset_us;
        self.report = None;
        self.seq = self.seq.wrapping_add(1);
        self.tries = 0;
        self.next_at = now_us;
        if Self::separate_path(&params) {
            // Probe the test path first: it must work, and its delay (not the control path's) sets the clock.
            self.sync_target = target;
            self.sync_path = true;
            self.sync_sent = 0;
            self.sync_replies = 0;
            self.best_rtt = u32::MAX;
            self.phase = Phase::Sync;
        } else {
            self.phase = Phase::Starting;
        }
        Ok(())
    }

    fn header<'a>(&self, out: &'a mut [u8], dst: [u8; 6], kind: u8, seq: u32) -> Writer<'a> {
        let length = out.len().min(500);
        out[..length].fill(0);
        let mut writer = Writer { bytes: &mut out[..length], at: 0 };
        writer.put(&dst);
        writer.put(&self.my_mac);
        writer.put(&CTRL_ETHERTYPE);
        writer.put(MAGIC);
        writer.u8(kind);
        writer.u8(VERSION);
        writer.u16(self.session);
        writer.u32(seq);
        writer
    }

    /// Frames lengths are at least 60 (Ethernet minimum); the drivers pad too, but be exact.
    fn finish(writer: Writer<'_>) -> usize {
        writer.at.max(60)
    }

    fn hello(&self, out: &mut [u8], dst: [u8; 6], seq: u32, now_us: u64) -> usize {
        let mut writer = self.header(out, dst, T_HELLO, seq);
        writer.u64(now_us);
        Self::finish(writer)
    }

    /// Master timers. Returns a frame to send, if one is due; call it every pass of the loop.
    pub fn poll(&mut self, now_us: u64, out: &mut [u8]) -> Option<usize> {
        if self.role != Role::Master {
            return None;
        }
        self.out_on_test_path = false;
        match self.phase {
            Phase::Discover { until } => {
                if self.pending_next < self.pending_len {
                    let dst = self.pending[self.pending_next];
                    let seq = self.pending_next as u32;
                    self.pending_next += 1;
                    return Some(self.hello(out, dst, seq, now_us));
                }
                if now_us >= until {
                    self.phase = Phase::Idle;
                    self.push_event(Event::DiscoveryDone(self.found_count as u8));
                }
                None
            }
            Phase::Sync => {
                let peer = self.sync_target;
                let done = self.sync_replies >= SYNC_WANTED
                    || (self.sync_sent >= SYNC_MAX_SENDS && now_us >= self.next_at + SYNC_LAST_WAIT_US);
                if done {
                    if self.sync_replies == 0 && self.sync_path {
                        self.phase = Phase::Ready;
                        self.push_event(Event::Failed(
                            "no answer on the test path: check the cabling, 'via' and 'here' NIC numbers",
                        ));
                    } else if self.sync_replies == 0 {
                        self.phase = Phase::Idle;
                        self.peer = None;
                        self.push_event(Event::Failed("no answer from the peer (is it running 'nettest serve'?)"));
                    } else if self.sync_path {
                        self.offset_us = self.best_offset;
                        self.rtt_us = self.best_rtt;
                        self.sync_path = false;
                        self.seq = self.seq.wrapping_add(1);
                        self.tries = 0;
                        self.next_at = now_us;
                        self.phase = Phase::Starting;
                    } else {
                        self.offset_us = self.best_offset;
                        self.rtt_us = self.best_rtt;
                        self.phase = Phase::Ready;
                        self.push_event(Event::Synced {
                            rtt_us: self.best_rtt,
                            offset_us: self.best_offset,
                            samples: self.sync_replies,
                        });
                    }
                    return None;
                }
                if self.sync_sent < SYNC_MAX_SENDS && now_us >= self.next_at {
                    let seq = u32::from(self.sync_sent);
                    self.sync_sent += 1;
                    self.next_at = now_us + SYNC_SPACING_US;
                    self.out_on_test_path = self.sync_path;
                    return Some(self.hello(out, peer, seq, now_us));
                }
                None
            }
            Phase::Starting => {
                let peer = self.peer?;
                if now_us < self.next_at {
                    return None;
                }
                self.tries += 1;
                if self.tries > RETRIES {
                    self.phase = Phase::Ready;
                    self.push_event(Event::Failed("the peer did not acknowledge the test"));
                    return None;
                }
                self.next_at = now_us + RETRY_US;
                let (test, offset, seq, here_mac) = (self.test, self.offset_us, self.seq, self.here_mac);
                let mut writer = self.header(out, peer, T_START, seq);
                writer.u8(u8::from(test.up) | (u8::from(test.down) << 1));
                writer.u32(test.rate_kbit);
                writer.u16(test.size);
                writer.u16(test.seconds);
                writer.u64(offset as u64);
                writer.u8(if test.via == 0 { VIA_CONTROL } else { test.via - 1 });
                writer.put(&here_mac);
                writer.u32(test.count);
                Some(Self::finish(writer))
            }
            Phase::Running { until } => {
                if now_us >= until {
                    self.phase = Phase::Querying;
                    self.seq = self.seq.wrapping_add(1);
                    self.tries = 0;
                    self.next_at = now_us;
                }
                None
            }
            Phase::Querying => {
                let peer = self.peer?;
                if now_us < self.next_at {
                    return None;
                }
                self.tries += 1;
                if self.tries > RETRIES {
                    self.phase = Phase::Ready;
                    self.push_event(Event::Failed("no report from the peer"));
                    return None;
                }
                self.next_at = now_us + RETRY_US;
                let seq = self.seq;
                let writer = self.header(out, peer, T_QUERY, seq);
                Some(Self::finish(writer))
            }
            Phase::Idle | Phase::Ready => None,
        }
    }

    /// Master: ends a running test early (the peer is told too).
    pub fn stop_test(&mut self, generator: &mut Generator, out: &mut [u8]) -> Option<usize> {
        generator.stop();
        let peer = self.peer?;
        if self.busy() {
            self.phase = Phase::Ready;
            let seq = self.seq;
            let writer = self.header(out, peer, T_STOP, seq);
            return Some(Self::finish(writer));
        }
        None
    }

    /// A control frame arrived. `now_us` is the time it was read. Returns a reply to send, if any.
    pub fn handle(
        &mut self,
        frame: &[u8],
        now_us: u64,
        generator: &mut Generator,
        sink: &mut Sink,
        out: &mut [u8],
    ) -> Option<usize> {
        if frame.len() < HEADER || frame[12..14] != CTRL_ETHERTYPE || &frame[14..18] != MAGIC || frame[19] != VERSION {
            return None;
        }
        let kind = frame[18];
        let session = u16::from_le_bytes([frame[20], frame[21]]);
        let seq = u32::from_le_bytes(frame[22..26].try_into().ok()?);
        let source: [u8; 6] = frame[6..12].try_into().ok()?;
        let mut body = Reader { bytes: &frame[HEADER..], at: 0 };
        match (self.role, kind) {
            (Role::Server, T_HELLO) => {
                let t1 = body.u64()?;
                self.session = session;
                let mut writer = self.header(out, source, T_HELLO_REPLY, seq);
                writer.u64(t1);
                writer.u64(now_us);
                writer.u64(now_us);
                writer.u8(self.nic_count as u8);
                for index in 0..self.nic_count {
                    let mac = self.nics[index];
                    writer.put(&mac);
                }
                Some(Self::finish(writer))
            }
            (Role::Server, T_START) => {
                let flags = body.u8()?;
                let rate = body.u32()?;
                let size = body.u16()?;
                let seconds = body.u16()?;
                let offset = body.u64()? as i64;
                let via = body.u8().unwrap_or(VIA_CONTROL);
                let here_mac = body.take::<6>().filter(|mac| *mac != [0; 6]).unwrap_or(source);
                let count = body.u32().unwrap_or(0);
                self.session = session;
                self.master = source;
                if self.last_start != Some(seq) {
                    self.last_start = Some(seq);
                    generator.stop();
                    sink.reset();
                    sink.clock_offset_us = offset;
                    self.test_nic = (usize::from(via) < self.nic_count).then_some(via);
                    if flags & 2 != 0 {
                        generator.rate_kbit = rate;
                        generator.size = usize::from(size);
                        generator.count_limit = count;
                        generator.seconds_limit = u32::from(seconds);
                        generator.destination = here_mac;
                        generator.start(now_us);
                    }
                    self.push_event(Event::Serving { up: flags & 1 != 0, down: flags & 2 != 0 });
                }
                let mut writer = self.header(out, source, T_START_ACK, seq);
                writer.u8(0);
                Some(Self::finish(writer))
            }
            (Role::Server, T_QUERY) => {
                self.session = session;
                let stats = sink.stats;
                let (sent, failed) = (generator.stats.sent, generator.stats.send_failed);
                let mut writer = self.header(out, source, T_REPORT, seq);
                writer.u32(stats.received);
                writer.u64(stats.bytes);
                writer.u32(stats.lost);
                writer.u32(stats.duplicated);
                writer.u32(stats.reordered);
                writer.u32(stats.latency_min);
                writer.u32(stats.latency_max);
                writer.u64(stats.latency_sum);
                writer.u32(stats.jitter);
                writer.u64(stats.first_us);
                writer.u64(stats.last_us);
                for count in stats.histogram {
                    writer.u32(count);
                }
                for count in stats.timeline {
                    writer.u32(count);
                }
                writer.u32(sent);
                writer.u32(failed);
                for value in counter_delta(&self.local_counters, &self.start_counters) {
                    writer.u64(value);
                }
                Some(Self::finish(writer))
            }
            (Role::Server, T_STOP) => {
                generator.stop();
                None
            }
            (Role::Master, T_HELLO_REPLY) if session == self.session => {
                let t1 = body.u64()? as i64;
                let t2 = body.u64()? as i64;
                let t3 = body.u64()? as i64;
                let t4 = now_us as i64;
                let rtt = ((t4 - t1) - (t3 - t2)).clamp(0, i64::from(u32::MAX)) as u32;
                let offset = ((t2 - t1) + (t3 - t4)) / 2;
                match self.phase {
                    Phase::Discover { .. } => {
                        if !self.found[..self.found_count].iter().any(|found| found.mac == source)
                            && self.found_count < MAX_FOUND
                        {
                            let found = Found { mac: source, rtt_us: rtt };
                            self.found[self.found_count] = found;
                            self.found_count += 1;
                            self.push_event(Event::Found(found));
                        }
                    }
                    Phase::Sync if source == self.sync_target => {
                        if !self.sync_path && self.sync_replies == 0 {
                            let count = usize::from(body.u8().unwrap_or(0)).min(MAX_NICS);
                            self.peer_nic_count = 0;
                            for _ in 0..count {
                                let Some(mac) = body.take::<6>() else { break };
                                self.peer_nics[self.peer_nic_count] = mac;
                                self.peer_nic_count += 1;
                            }
                        }
                        self.sync_replies += 1;
                        if rtt < self.best_rtt {
                            self.best_rtt = rtt;
                            self.best_offset = offset;
                        }
                    }
                    _ => {}
                }
                None
            }
            (Role::Master, T_START_ACK) if self.phase == Phase::Starting && seq == self.seq && Some(source) == self.peer => {
                sink.clock_offset_us = -self.offset_us;
                if self.test.up {
                    generator.start(now_us);
                }
                self.phase = Phase::Running { until: now_us + u64::from(self.test.seconds) * 1_000_000 + DRAIN_US };
                self.push_event(Event::Started);
                None
            }
            (Role::Master, T_REPORT) if self.phase == Phase::Querying && seq == self.seq && Some(source) == self.peer => {
                let mut stats = SinkStats {
                    received: body.u32()?,
                    bytes: body.u64()?,
                    lost: body.u32()?,
                    duplicated: body.u32()?,
                    reordered: body.u32()?,
                    latency_min: body.u32()?,
                    latency_max: body.u32()?,
                    latency_sum: body.u64()?,
                    jitter: body.u32()?,
                    first_us: body.u64()?,
                    last_us: body.u64()?,
                    ..SinkStats::default()
                };
                for index in 0..HISTOGRAM_BUCKETS {
                    stats.histogram[index] = body.u32()?;
                }
                for index in 0..TIMELINE_BINS {
                    stats.timeline[index] = body.u32()?;
                }
                let gen_sent = body.u32()?;
                let gen_failed = body.u32()?;
                let mut counters = [0u64; NIC_COUNTERS];
                for value in counters.iter_mut() {
                    // An older server stops after `gen_failed`: its counters stay 0 (unknown).
                    *value = body.u64().unwrap_or(0);
                }
                self.own_counters = counter_delta(&self.local_counters, &self.start_counters);
                self.report = Some(PeerReport { sink: stats, gen_sent, gen_failed, counters });
                self.phase = Phase::Ready;
                self.push_event(Event::Finished);
                None
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MASTER: [u8; 6] = [2, 0, 0, 0, 0, 1];
    const SERVER: [u8; 6] = [2, 0, 0, 0, 0, 2];

    /// Server clock = master clock + OFFSET; each way takes DELAY us.
    const OFFSET: i64 = 5_000_000;
    const DELAY: u64 = 150;

    struct Rig {
        master: Remote,
        server: Remote,
        mgen: Generator,
        msink: Sink,
        sgen: Generator,
        ssink: Sink,
        now: u64,
        events: Vec<Event>,
    }

    impl Rig {
        fn new() -> Self {
            Self {
                master: Remote::new(Role::Master, MASTER, 7),
                server: Remote::new(Role::Server, SERVER, 0),
                mgen: Generator::new(),
                msink: Sink::new(),
                sgen: Generator::new(),
                ssink: Sink::new(),
                now: 1_000_000,
                events: Vec::new(),
            }
        }

        /// One step of 100 us: the master polls, frames cross with DELAY.
        fn step(&mut self) {
            let mut out = [0u8; 512];
            if let Some(length) = self.master.poll(self.now, &mut out) {
                let frame = out[..length].to_vec();
                let at_server = (self.now + DELAY) as i64 + OFFSET;
                let mut reply = [0u8; 512];
                if let Some(length) =
                    self.server.handle(&frame, at_server as u64, &mut self.sgen, &mut self.ssink, &mut reply)
                {
                    // As the tape does: the reply leaves from the NIC the frame came in on.
                    if self.server.nics[..self.server.nic_count].iter().any(|mac| mac[..] == frame[0..6]) {
                        reply[6..12].copy_from_slice(&frame[0..6]);
                    }
                    let back = reply[..length].to_vec();
                    let mut none = [0u8; 512];
                    let _ = self.master.handle(&back, self.now + 2 * DELAY, &mut self.mgen, &mut self.msink, &mut none);
                }
            }
            while let Some(event) = self.master.take_event() {
                self.events.push(event);
            }
            self.now += 100;
        }

        fn run_until(&mut self, mut done: impl FnMut(&Self) -> bool) {
            for _ in 0..400_000 {
                if done(self) {
                    return;
                }
                self.step();
            }
            panic!("did not finish; events {:?}", self.events);
        }
    }

    #[test]
    fn discovers_and_syncs_the_clock() {
        let mut rig = Rig::new();
        rig.master.discover(&[[2, 0, 0, 0, 0, 9], SERVER], rig.now);
        rig.run_until(|rig| rig.events.iter().any(|event| matches!(event, Event::DiscoveryDone(_))));
        assert_eq!(rig.master.found_count, 1);
        assert_eq!(rig.master.found[0].mac, SERVER);
        assert!(rig.master.found[0].rtt_us.abs_diff(2 * DELAY as u32) <= 1);
        rig.master.select(SERVER, rig.now);
        rig.run_until(|rig| rig.master.ready());
        assert!(rig.master.offset_us.abs_diff(OFFSET) <= 1, "offset {}", rig.master.offset_us);
        assert!(rig.master.rtt_us.abs_diff(2 * DELAY as u32) <= 1);
    }

    #[test]
    fn a_silent_peer_fails_the_sync() {
        let mut rig = Rig::new();
        rig.master.select([2, 0, 0, 0, 0, 9], rig.now);
        let mut out = [0u8; 512];
        for _ in 0..100_000 {
            let _ = rig.master.poll(rig.now, &mut out);
            rig.now += 100;
        }
        assert!(matches!(rig.master.take_event(), Some(Event::Failed(_))));
    }

    #[test]
    fn a_test_runs_both_ways_and_reports() {
        let mut rig = Rig::new();
        rig.master.select(SERVER, rig.now);
        rig.run_until(|rig| rig.master.ready());
        let params = TestParams { rate_kbit: 8_000, seconds: 1, ..TestParams::new() };
        rig.master.start_test(params, rig.now, &mut rig.mgen, &mut rig.msink, MASTER).unwrap();
        rig.run_until(|rig| rig.events.contains(&Event::Started));
        assert!(rig.mgen.running && rig.sgen.running);
        assert_eq!(rig.sgen.destination, MASTER);
        assert_eq!(rig.ssink.clock_offset_us, rig.master.offset_us);
        assert_eq!(rig.msink.clock_offset_us, -rig.master.offset_us);
        // Frames from the master carry master time; the server's sink corrects by the offset.
        let mut frame = [0u8; 1000];
        let length = rig.mgen.next_frame(rig.now + 10_000, MASTER, &mut frame, 0).unwrap();
        let sent_us = u64::from_le_bytes(frame[24..32].try_into().unwrap());
        assert!(rig.ssink.on_frame(&frame[..length], (rig.now as i64 + 10_000 + OFFSET + 300) as u64));
        assert!(sent_us <= rig.now + 10_000);
        let latency = rig.ssink.stats.latency_min;
        assert!((290..=310).contains(&latency), "latency {latency}");
        rig.run_until(|rig| rig.events.contains(&Event::Finished));
        let report = rig.master.report.unwrap();
        assert_eq!(report.sink.received, 1);
        assert_eq!(report.gen_failed, 0);
    }

    #[test]
    fn the_report_carries_the_servers_counter_changes() {
        let mut rig = Rig::new();
        rig.master.select(SERVER, rig.now);
        rig.run_until(|rig| rig.master.ready());
        let params = TestParams { rate_kbit: 8_000, seconds: 1, ..TestParams::new() };
        rig.master.start_test(params, rig.now, &mut rig.mgen, &mut rig.msink, MASTER).unwrap();
        rig.server.start_counters = [1000, 1, 10, 0, 5, 0, 0, 0, 0];
        rig.server.local_counters = [1000, 1, 12, 3, 5, 0, 7, 0, 0];
        rig.master.start_counters = [1000, 1, 0, 0, 0, 0, 0, 0, 0];
        rig.master.local_counters = [100, 0, 0, 0, 9, 0, 0, 0, 0];
        rig.run_until(|rig| rig.events.contains(&Event::Finished));
        let report = rig.master.report.unwrap();
        // Speed and duplex as the server saw them at the end, every error counter as a change over the test.
        assert_eq!(report.counters, [1000, 1, 2, 3, 0, 0, 7, 0, 0]);
        assert_eq!(rig.master.own_counters, [100, 0, 0, 0, 9, 0, 0, 0, 0]);
    }

    #[test]
    fn a_test_can_use_other_nics_than_the_control_ones() {
        const SERVER_TEST: [u8; 6] = [2, 0, 0, 0, 0, 0x22];
        const MASTER_TEST: [u8; 6] = [2, 0, 0, 0, 0, 0x11];
        let mut rig = Rig::new();
        rig.server.set_nics(&[SERVER, SERVER_TEST]);
        rig.master.select(SERVER, rig.now);
        rig.run_until(|rig| rig.master.ready());
        assert_eq!(rig.master.peer_nic_count, 2);
        assert_eq!(rig.master.peer_nics[1], SERVER_TEST);
        let params = TestParams { rate_kbit: 8_000, seconds: 1, via: 2, here: 2, ..TestParams::new() };
        rig.master.start_test(params, rig.now, &mut rig.mgen, &mut rig.msink, MASTER_TEST).unwrap();
        rig.run_until(|rig| rig.events.contains(&Event::Started));
        assert_eq!(rig.mgen.destination, SERVER_TEST, "up goes to the server's test NIC");
        assert_eq!(rig.sgen.destination, MASTER_TEST, "down goes to the master's test NIC");
        assert_eq!(rig.server.test_nic, Some(1));
        let params = TestParams { via: 3, ..params };
        rig.run_until(|rig| rig.events.contains(&Event::Finished));
        assert!(rig.master.start_test(params, rig.now, &mut rig.mgen, &mut rig.msink, MASTER_TEST).is_err());
    }

    #[test]
    fn a_dead_test_path_fails_before_the_test_starts() {
        let mut rig = Rig::new();
        rig.server.set_nics(&[SERVER, [2, 0, 0, 0, 0, 0x22]]);
        rig.master.select(SERVER, rig.now);
        rig.run_until(|rig| rig.master.ready());
        let params = TestParams { via: 2, ..TestParams::new() };
        rig.master.start_test(params, rig.now, &mut rig.mgen, &mut rig.msink, MASTER).unwrap();
        // Nothing answers: do not step the server.
        let mut out = [0u8; 512];
        for _ in 0..100_000 {
            let _ = rig.master.poll(rig.now, &mut out);
            rig.now += 100;
        }
        assert!(matches!(rig.master.take_event(), Some(Event::Failed(_))));
        assert!(rig.master.ready());
    }

    #[test]
    fn a_repeated_start_does_not_restart_the_server() {
        let mut server = Remote::new(Role::Server, SERVER, 0);
        let mut master = Remote::new(Role::Master, MASTER, 3);
        let (mut mgen, mut msink, mut sgen, mut ssink) = (Generator::new(), Sink::new(), Generator::new(), Sink::new());
        master.peer = Some(SERVER);
        master.phase = Phase::Ready;
        master.start_test(TestParams::new(), 0, &mut mgen, &mut msink, MASTER).unwrap();
        let mut out = [0u8; 512];
        let length = master.poll(0, &mut out).unwrap();
        let frame = out[..length].to_vec();
        let mut reply = [0u8; 512];
        server.handle(&frame, 10, &mut sgen, &mut ssink, &mut reply).unwrap();
        sgen.stats.sent = 5;
        server.handle(&frame, 20, &mut sgen, &mut ssink, &mut reply).unwrap();
        assert_eq!(sgen.stats.sent, 5, "the repeat must not restart the generator");
    }

    #[test]
    fn ignores_foreign_frames() {
        let mut server = Remote::new(Role::Server, SERVER, 0);
        let (mut g, mut s) = (Generator::new(), Sink::new());
        let mut out = [0u8; 512];
        assert!(server.handle(&[0u8; 60], 0, &mut g, &mut s, &mut out).is_none());
    }
}
