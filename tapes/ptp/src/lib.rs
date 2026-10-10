//! PTPv2 (IEEE 1588-2008) over UDP/IPv4, the transport AES67 uses: message codecs, a slave-only clock
//! measurement and the pieces a small master needs. Pure and fixed-buffer, so it is tested on the host;
//! `main.rs` owns the network and the clock.
//!
//! What is implemented: the default delay request-response (end-to-end) mechanism, two-step and one-step
//! Sync, multicast on 224.0.1.129 (event messages on port 319, general messages on port 320), the best-master
//! comparison on the Announce data set, and correction fields. What is not: peer delay, boundary or
//! transparent clock behaviour, unicast negotiation, management messages.
//!
//! All times are signed nanoseconds in an `i64` (PTP's 48-bit seconds + 32-bit nanoseconds fit for centuries).
//! A correction field is nanoseconds scaled by 2^16 on the wire and is converted to plain nanoseconds here.
//!
//! The slave only *measures*: it keeps its own clock as it is and reports how far it is from the master's. The
//! first sample fixes a base offset (a real slave would step its clock there, and the two clocks may count from
//! different epochs), so `residual` is the offset after that step.
#![no_std]

pub const GROUP: [u8; 4] = [224, 0, 1, 129];
pub const EVENT_PORT: u16 = 319;
pub const GENERAL_PORT: u16 = 320;

pub const SYNC: u8 = 0x0;
pub const DELAY_REQ: u8 = 0x1;
pub const FOLLOW_UP: u8 = 0x8;
pub const DELAY_RESP: u8 = 0x9;
pub const ANNOUNCE: u8 = 0xB;

pub const HEADER_LEN: usize = 34;
pub const FLAG_TWO_STEP: u16 = 0x0200;

/// A clock identity (EUI-64) and a port number, the way PTP names a port.
pub type PortIdentity = [u8; 10];

/// The port identity of a NIC: its MAC as an EUI-64 (`ff:fe` in the middle) and `port` as the port number.
pub fn port_identity(mac: [u8; 6], port: u16) -> PortIdentity {
    [mac[0], mac[1], mac[2], 0xff, 0xfe, mac[3], mac[4], mac[5], (port >> 8) as u8, port as u8]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub message_type: u8,
    pub length: u16,
    pub domain: u8,
    pub flags: u16,
    /// Nanoseconds (the wire value is scaled by 2^16).
    pub correction: i64,
    pub source: PortIdentity,
    pub sequence: u16,
    pub control: u8,
    pub log_interval: i8,
}

impl Header {
    pub fn parse(bytes: &[u8]) -> Option<Header> {
        if bytes.len() < HEADER_LEN || bytes[1] & 0x0f != 2 {
            return None;
        }
        let length = u16::from_be_bytes([bytes[2], bytes[3]]);
        if usize::from(length) > bytes.len() || usize::from(length) < HEADER_LEN {
            return None;
        }
        let mut source = [0u8; 10];
        source.copy_from_slice(&bytes[20..30]);
        let scaled = i64::from_be_bytes(bytes[8..16].try_into().ok()?);
        Some(Header {
            message_type: bytes[0] & 0x0f,
            length,
            domain: bytes[4],
            flags: u16::from_be_bytes([bytes[6], bytes[7]]),
            correction: scaled >> 16,
            source,
            sequence: u16::from_be_bytes([bytes[30], bytes[31]]),
            control: bytes[32],
            log_interval: bytes[33] as i8,
        })
    }

    fn write(&self, out: &mut [u8]) {
        out[..HEADER_LEN].fill(0);
        out[0] = self.message_type;
        out[1] = 2;
        out[2..4].copy_from_slice(&self.length.to_be_bytes());
        out[4] = self.domain;
        out[6..8].copy_from_slice(&self.flags.to_be_bytes());
        out[8..16].copy_from_slice(&(self.correction << 16).to_be_bytes());
        out[20..30].copy_from_slice(&self.source);
        out[30..32].copy_from_slice(&self.sequence.to_be_bytes());
        out[32] = self.control;
        out[33] = self.log_interval as u8;
    }
}

/// A PTP timestamp (48-bit seconds, 32-bit nanoseconds) as signed nanoseconds.
pub fn read_timestamp(bytes: &[u8]) -> Option<i64> {
    let bytes = bytes.get(..10)?;
    let seconds = (u64::from(bytes[0]) << 40) | (u64::from(bytes[1]) << 32) | u64::from(u32::from_be_bytes([bytes[2], bytes[3], bytes[4], bytes[5]]));
    let nanos = u32::from_be_bytes([bytes[6], bytes[7], bytes[8], bytes[9]]);
    if nanos >= 1_000_000_000 {
        return None;
    }
    i64::try_from(seconds).ok()?.checked_mul(1_000_000_000)?.checked_add(i64::from(nanos))
}

pub fn write_timestamp(out: &mut [u8], nanoseconds: i64) {
    let nanoseconds = nanoseconds.max(0) as u64;
    let seconds = nanoseconds / 1_000_000_000;
    out[0..2].copy_from_slice(&((seconds >> 32) as u16).to_be_bytes());
    out[2..6].copy_from_slice(&(seconds as u32).to_be_bytes());
    out[6..10].copy_from_slice(&((nanoseconds % 1_000_000_000) as u32).to_be_bytes());
}

/// What an Announce says about a clock: the best-master comparison looks at exactly these (lower is better).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Clock {
    pub priority1: u8,
    pub class: u8,
    pub accuracy: u8,
    pub variance: u16,
    pub priority2: u8,
    pub identity: [u8; 8],
    pub steps_removed: u16,
    pub time_source: u8,
}

impl Clock {
    /// An ordinary clock: not a grandmaster-capable class, accuracy unknown, an internal oscillator.
    pub fn ordinary(mac: [u8; 6]) -> Clock {
        let id = port_identity(mac, 0);
        let mut identity = [0u8; 8];
        identity.copy_from_slice(&id[..8]);
        Clock { priority1: 128, class: 248, accuracy: 0xfe, variance: 0xffff, priority2: 128, identity, steps_removed: 0, time_source: 0xa0 }
    }

    fn key(&self) -> ([u8; 5], [u8; 8]) {
        let variance = self.variance.to_be_bytes();
        ([self.priority1, self.class, self.accuracy, variance[0], variance[1]], self.identity)
    }

    /// True if `self` is a better master than `other` (priority 2 sits between variance and identity).
    pub fn better_than(&self, other: &Clock) -> bool {
        let (a, b) = (self.key(), other.key());
        if a.0 != b.0 {
            return a.0 < b.0;
        }
        if self.priority2 != other.priority2 {
            return self.priority2 < other.priority2;
        }
        a.1 < b.1
    }
}

pub const ANNOUNCE_LEN: usize = 64;
pub const SYNC_LEN: usize = 44;
pub const DELAY_RESP_LEN: usize = 54;

fn header(message_type: u8, length: usize, domain: u8, flags: u16, source: PortIdentity, sequence: u16, control: u8, log_interval: i8) -> Header {
    Header { message_type, length: length as u16, domain, flags, correction: 0, source, sequence, control, log_interval }
}

pub fn build_announce(out: &mut [u8], domain: u8, source: PortIdentity, sequence: u16, clock: &Clock, log_interval: i8) -> usize {
    header(ANNOUNCE, ANNOUNCE_LEN, domain, 0, source, sequence, 5, log_interval).write(out);
    out[34..ANNOUNCE_LEN].fill(0);
    out[47] = clock.priority1;
    out[48] = clock.class;
    out[49] = clock.accuracy;
    out[50..52].copy_from_slice(&clock.variance.to_be_bytes());
    out[52] = clock.priority2;
    out[53..61].copy_from_slice(&clock.identity);
    out[61..63].copy_from_slice(&clock.steps_removed.to_be_bytes());
    out[63] = clock.time_source;
    ANNOUNCE_LEN
}

/// A two-step Sync (origin timestamp zero; the Follow_Up carries the time) unless `one_step_time` is given.
pub fn build_sync(out: &mut [u8], domain: u8, source: PortIdentity, sequence: u16, one_step_time: Option<i64>, log_interval: i8) -> usize {
    header(SYNC, SYNC_LEN, domain, if one_step_time.is_some() { 0 } else { FLAG_TWO_STEP }, source, sequence, 0, log_interval).write(out);
    write_timestamp(&mut out[34..44], one_step_time.unwrap_or(0));
    SYNC_LEN
}

pub fn build_follow_up(out: &mut [u8], domain: u8, source: PortIdentity, sequence: u16, precise_origin: i64, log_interval: i8) -> usize {
    header(FOLLOW_UP, SYNC_LEN, domain, 0, source, sequence, 2, log_interval).write(out);
    write_timestamp(&mut out[34..44], precise_origin);
    SYNC_LEN
}

pub fn build_delay_req(out: &mut [u8], domain: u8, source: PortIdentity, sequence: u16) -> usize {
    header(DELAY_REQ, SYNC_LEN, domain, 0, source, sequence, 1, 0x7f).write(out);
    write_timestamp(&mut out[34..44], 0);
    SYNC_LEN
}

pub fn build_delay_resp(out: &mut [u8], domain: u8, source: PortIdentity, requester: PortIdentity, sequence: u16, received: i64, log_interval: i8) -> usize {
    header(DELAY_RESP, DELAY_RESP_LEN, domain, 0, source, sequence, 3, log_interval).write(out);
    write_timestamp(&mut out[34..44], received);
    out[44..54].copy_from_slice(&requester);
    DELAY_RESP_LEN
}

/// The Announce data set of a message, if it is an Announce.
pub fn parse_announce(message: &[u8]) -> Option<Clock> {
    if message.len() < ANNOUNCE_LEN {
        return None;
    }
    let mut identity = [0u8; 8];
    identity.copy_from_slice(&message[53..61]);
    Some(Clock {
        priority1: message[47],
        class: message[48],
        accuracy: message[49],
        variance: u16::from_be_bytes([message[50], message[51]]),
        priority2: message[52],
        identity,
        steps_removed: u16::from_be_bytes([message[61], message[62]]),
        time_source: message[63],
    })
}

/// One measurement: the local clock minus the master's, the one-way path delay, and the offset after the first
/// sample's step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sample {
    pub offset: i64,
    pub residual: i64,
    pub delay: i64,
    pub sequence: u16,
}

#[derive(Clone, Copy, Default, Debug)]
pub struct Counters {
    pub announces: u32,
    pub syncs: u32,
    pub follow_ups: u32,
    pub delay_resps: u32,
    pub sync_lost: u32,
    /// Syncs not measured because they were read late (see `discard_sync`).
    pub late: u32,
    pub steps: u32,
    pub masters: u32,
}

struct Master {
    source: PortIdentity,
    clock: Clock,
    last_announce: i64,
    timeout: i64,
}

#[derive(Clone, Copy)]
struct PendingSync {
    sequence: u16,
    t2: i64,
    correction: i64,
}

/// A step bigger than this (the master's clock jumped, or a different master) restarts the base offset.
const STEP_LIMIT: i64 = 100_000_000;

pub struct Slave {
    domain: u8,
    identity: PortIdentity,
    master: Option<Master>,
    pending: Option<PendingSync>,
    /// The latest master-to-slave difference (t2 - t1 - corrections), for the delay computation.
    last_ms: Option<i64>,
    request_sequence: u16,
    request_sent: Option<(u16, i64)>,
    last_request: Option<i64>,
    last_sync_sequence: Option<u16>,
    delay_interval: i64,
    delay: Option<i64>,
    delay_samples: [i64; DELAY_WINDOW],
    delay_count: usize,
    base: Option<i64>,
    pub counters: Counters,
}

/// The path delay is the median of this many recent measurements: one slow exchange (a queued frame, a late
/// poll) inflates a single sample and the median ignores it. Until `DELAY_WARM_UP` are in, requests go out
/// faster than the configured rate so that the estimate settles in a second or two.
const DELAY_WINDOW: usize = 9;
const DELAY_WARM_UP: usize = 5;
const WARM_UP_INTERVAL: i64 = 200_000_000;

impl Slave {
    pub fn new(domain: u8, identity: PortIdentity, delay_interval_ns: i64) -> Slave {
        Slave {
            domain,
            identity,
            master: None,
            pending: None,
            last_ms: None,
            request_sequence: 0,
            request_sent: None,
            last_request: None,
            last_sync_sequence: None,
            delay_interval: delay_interval_ns,
            delay: None,
            delay_samples: [0; DELAY_WINDOW],
            delay_count: 0,
            base: None,
            counters: Counters::default(),
        }
    }

    pub fn master(&self) -> Option<(PortIdentity, Clock)> {
        self.master.as_ref().map(|master| (master.source, master.clock))
    }

    pub fn path_delay(&self) -> Option<i64> {
        self.delay
    }

    /// Forgets a master that has stopped announcing (three announce intervals of silence).
    pub fn tick(&mut self, now: i64) {
        if self.master.as_ref().is_some_and(|master| now - master.last_announce > master.timeout) {
            self.master = None;
            self.pending = None;
            self.last_ms = None;
            self.delay = None;
            self.base = None;
            self.request_sent = None;
        }
    }

    /// A Delay_Req to send now (rate limited), as its sequence number; build it with `build_delay_req` and
    /// report the transmit time with `request_sent`.
    pub fn request_due(&mut self, now: i64) -> Option<u16> {
        self.master.as_ref()?;
        self.last_ms?;
        let interval = if self.delay_count < DELAY_WARM_UP { self.delay_interval.min(WARM_UP_INTERVAL) } else { self.delay_interval };
        if self.last_request.is_some_and(|last| now - last < interval) {
            return None;
        }
        self.last_request = Some(now);
        self.request_sequence = self.request_sequence.wrapping_add(1);
        Some(self.request_sequence)
    }

    pub fn request_sent(&mut self, sequence: u16, t3: i64) {
        self.request_sent = Some((sequence, t3));
    }

    /// Handles one received message; `t2` is the local receive time in nanoseconds (it matters for event
    /// messages, Sync). Returns a sample when a Sync/Follow_Up pair completes and the path delay is known.
    pub fn handle(&mut self, message: &[u8], t2: i64) -> Option<Sample> {
        let header = Header::parse(message)?;
        if header.domain != self.domain || header.source == self.identity {
            return None;
        }
        match header.message_type {
            ANNOUNCE => {
                let clock = parse_announce(message)?;
                self.counters.announces += 1;
                let timeout = 3 * interval_ns(header.log_interval.max(-3));
                match &mut self.master {
                    Some(master) if master.source == header.source => {
                        master.clock = clock;
                        master.last_announce = t2;
                        master.timeout = timeout;
                    }
                    Some(master) if !clock.better_than(&master.clock) => {}
                    _ => {
                        let counters = Counters { masters: self.counters.masters + 1, ..self.counters };
                        *self = Slave::new(self.domain, self.identity, self.delay_interval);
                        self.counters = counters;
                        self.master = Some(Master { source: header.source, clock, last_announce: t2, timeout });
                    }
                }
                None
            }
            SYNC if self.is_master(&header) => {
                self.counters.syncs += 1;
                if let Some(last) = self.last_sync_sequence {
                    self.counters.sync_lost += u32::from(header.sequence.wrapping_sub(last).wrapping_sub(1));
                }
                self.last_sync_sequence = Some(header.sequence);
                if header.flags & FLAG_TWO_STEP != 0 {
                    self.pending = Some(PendingSync { sequence: header.sequence, t2, correction: header.correction });
                    None
                } else {
                    let t1 = read_timestamp(message.get(34..)?)?;
                    self.complete(header.sequence, t1, t2, header.correction)
                }
            }
            FOLLOW_UP if self.is_master(&header) => {
                let pending = self.pending.take().filter(|pending| pending.sequence == header.sequence)?;
                self.counters.follow_ups += 1;
                let t1 = read_timestamp(message.get(34..)?)?;
                self.complete(header.sequence, t1, pending.t2, pending.correction + header.correction)
            }
            DELAY_RESP if self.is_master(&header) => {
                if message.len() < DELAY_RESP_LEN || message[44..54] != self.identity {
                    return None;
                }
                let (_, t3) = self.request_sent.take().filter(|&(sequence, _)| sequence == header.sequence)?;
                let t4 = read_timestamp(message.get(34..)?)?;
                self.counters.delay_resps += 1;
                let sm = t4 - t3 - header.correction;
                if let Some(ms) = self.last_ms {
                    self.delay_samples[self.delay_count % DELAY_WINDOW] = (ms + sm) / 2;
                    self.delay_count += 1;
                    let mut sorted = self.delay_samples;
                    let filled = self.delay_count.min(DELAY_WINDOW);
                    sorted[..filled].sort_unstable();
                    self.delay = Some(sorted[filled / 2]);
                }
                None
            }
            _ => None,
        }
    }

    /// A Sync that sat in the NIC's ring while the caller was busy has a receive time that is too late. It is
    /// counted and skipped, so that it neither measures nor is mistaken for a lost one.
    pub fn discard_sync(&mut self, message: &[u8]) {
        if let Some(header) = Header::parse(message).filter(|h| h.message_type == SYNC && h.domain == self.domain && self.is_master(h)) {
            self.counters.late += 1;
            self.last_sync_sequence = Some(header.sequence);
            self.pending = None;
        }
    }

    fn is_master(&self, header: &Header) -> bool {
        self.master.as_ref().is_some_and(|master| master.source == header.source)
    }

    fn complete(&mut self, sequence: u16, t1: i64, t2: i64, correction: i64) -> Option<Sample> {
        let ms = t2 - t1 - correction;
        self.last_ms = Some(ms);
        let delay = self.delay?;
        let offset = ms - delay;
        let mut base = *self.base.get_or_insert(offset);
        if (offset - base).abs() > STEP_LIMIT {
            self.counters.steps += 1;
            base = offset;
            self.base = Some(base);
        }
        Some(Sample { offset, residual: offset - base, delay, sequence })
    }
}

pub fn interval_ns(log_interval: i8) -> i64 {
    let log = i32::from(log_interval).clamp(-10, 10);
    if log >= 0 { 1_000_000_000i64 << log } else { 1_000_000_000i64 >> -log }
}

/// A growing window of residuals: count, mean, extremes and standard deviation, with integer arithmetic.
#[derive(Clone, Copy, Default)]
pub struct Window {
    pub count: u32,
    sum: i128,
    sum_squares: i128,
    pub min: i64,
    pub max: i64,
}

impl Window {
    pub fn add(&mut self, value: i64) {
        if self.count == 0 {
            self.min = value;
            self.max = value;
        }
        self.min = self.min.min(value);
        self.max = self.max.max(value);
        self.count += 1;
        self.sum += i128::from(value);
        self.sum_squares += i128::from(value) * i128::from(value);
    }

    pub fn mean(&self) -> i64 {
        if self.count == 0 { 0 } else { (self.sum / i128::from(self.count)) as i64 }
    }

    /// The standard deviation, in the same unit as the values.
    pub fn deviation(&self) -> i64 {
        if self.count < 2 {
            return 0;
        }
        let n = i128::from(self.count);
        let variance = (self.sum_squares - self.sum * self.sum / n) / n;
        isqrt(variance.max(0) as u128) as i64
    }
}

fn isqrt(value: u128) -> u128 {
    if value < 2 {
        return value;
    }
    let mut x = value;
    let mut y = x.div_ceil(2);
    while y < x {
        x = y;
        y = (x + value / x) / 2;
    }
    x
}

/// How fast the local clock runs against the master's, from offsets taken at least a second apart, over about
/// the last 30 s; in tenths of a part per million (positive: the local clock is fast).
#[derive(Clone, Copy)]
pub struct Rate {
    points: [(i64, i64); 32],
    next: usize,
    filled: usize,
}

impl Default for Rate {
    fn default() -> Rate {
        Rate::new()
    }
}

impl Rate {
    pub const fn new() -> Rate {
        Rate { points: [(0, 0); 32], next: 0, filled: 0 }
    }

    pub fn reset(&mut self) {
        self.next = 0;
        self.filled = 0;
    }

    /// Records (local time, raw offset), at most once a second.
    pub fn add(&mut self, now: i64, offset: i64) {
        if self.filled > 0 {
            let last = self.points[(self.next + 31) % 32];
            if now - last.0 < 1_000_000_000 {
                return;
            }
        }
        self.points[self.next] = (now, offset);
        self.next = (self.next + 1) % 32;
        self.filled = (self.filled + 1).min(32);
    }

    pub fn tenths_ppm(&self) -> Option<i64> {
        if self.filled < 4 {
            return None;
        }
        let newest = self.points[(self.next + 31) % 32];
        let oldest = self.points[(self.next + 32 - self.filled) % 32];
        let span = newest.0 - oldest.0;
        if span < 3_000_000_000 {
            return None;
        }
        Some(((i128::from(newest.1 - oldest.1) * 10_000_000) / i128::from(span)) as i64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MASTER: PortIdentity = [1, 2, 3, 0xff, 0xfe, 4, 5, 6, 0, 1];
    const ME: PortIdentity = [9, 9, 9, 0xff, 0xfe, 9, 9, 9, 0, 1];

    fn slave() -> Slave {
        let mut slave = Slave::new(0, ME, 1_000_000_000);
        let mut out = [0u8; 64];
        let clock = Clock::ordinary([1, 2, 3, 4, 5, 6]);
        let n = build_announce(&mut out, 0, MASTER, 1, &clock, 0);
        slave.handle(&out[..n], 0);
        slave
    }

    /// One Sync/Follow_Up and one Delay_Req/Resp exchange between a master whose clock is `ahead` ns ahead
    /// of the slave's, over a path of `down` ns master-to-slave and `up` ns slave-to-master.
    fn exchange(slave: &mut Slave, now: i64, ahead: i64, down: i64, up: i64, sequence: u16) -> Option<Sample> {
        let mut out = [0u8; 64];
        let t1 = now + ahead; // master time at departure
        let n = build_sync(&mut out, 0, MASTER, sequence, None, 0);
        slave.handle(&out[..n], now + down);
        let n = build_follow_up(&mut out, 0, MASTER, sequence, t1, 0);
        let sample = slave.handle(&out[..n], now + down);
        if let Some(request) = slave.request_due(now + down) {
            let t3 = now + down + 10;
            slave.request_sent(request, t3);
            let n = build_delay_resp(&mut out, 0, MASTER, ME, request, t3 + up + ahead, 0);
            slave.handle(&out[..n], t3 + up);
        }
        sample
    }

    #[test]
    fn timestamp_round_trip() {
        let mut bytes = [0u8; 10];
        for value in [0i64, 1, 999_999_999, 1_000_000_000, 1_700_000_000_123_456_789] {
            write_timestamp(&mut bytes, value);
            assert_eq!(read_timestamp(&bytes), Some(value));
        }
    }

    #[test]
    fn header_round_trip_and_correction_scaling() {
        let mut out = [0u8; 64];
        let n = build_sync(&mut out, 7, MASTER, 513, None, -3);
        out[8..16].copy_from_slice(&((1500i64) << 16).to_be_bytes());
        let header = Header::parse(&out[..n]).unwrap();
        assert_eq!((header.message_type, header.domain, header.sequence, header.log_interval), (SYNC, 7, 513, -3));
        assert_eq!(header.flags, FLAG_TWO_STEP);
        assert_eq!(header.correction, 1500);
        assert_eq!(header.source, MASTER);
        assert!(Header::parse(&out[..20]).is_none());
        out[1] = 1; // PTPv1
        assert!(Header::parse(&out[..n]).is_none());
    }

    #[test]
    fn symmetric_path_gives_exact_offset_and_delay() {
        let mut slave = slave();
        let mut last = None;
        for step in 0..6 {
            last = exchange(&mut slave, 1_000_000_000 * (step + 1), 5_000_000_000, 80_000, 80_000, step as u16 + 1).or(last);
        }
        let sample = last.unwrap();
        assert_eq!(sample.delay, 80_000);
        assert_eq!(sample.offset, -5_000_000_000);
        assert_eq!(sample.residual, 0);
    }

    #[test]
    fn asymmetry_shows_as_half_the_difference() {
        let mut slave = slave();
        let mut last = None;
        for step in 0..6 {
            last = exchange(&mut slave, 1_000_000_000 * (step + 1), 0, 100_000, 60_000, step as u16 + 1).or(last);
        }
        let sample = last.unwrap();
        assert_eq!(sample.delay, 80_000);
        assert_eq!(sample.offset, 20_000);
    }

    #[test]
    fn correction_fields_are_subtracted() {
        let mut slave = slave();
        let mut out = [0u8; 64];
        let mut samples = 0;
        for step in 0..4u16 {
            let now = 1_000_000_000 * (i64::from(step) + 1);
            let n = build_sync(&mut out, 0, MASTER, step, None, 0);
            out[8..16].copy_from_slice(&((30_000i64) << 16).to_be_bytes()); // a transparent clock added 30 us
            slave.handle(&out[..n], now + 130_000);
            let n = build_follow_up(&mut out, 0, MASTER, step, now, 0);
            samples += usize::from(slave.handle(&out[..n], 0).is_some_and(|s| s.offset == 0));
            if let Some(request) = slave.request_due(now) {
                slave.request_sent(request, now);
                let n = build_delay_resp(&mut out, 0, MASTER, ME, request, now + 100_000, 0);
                slave.handle(&out[..n], 0);
            }
        }
        assert_eq!(slave.path_delay(), Some(100_000));
        assert!(samples >= 1);
    }

    #[test]
    fn one_step_sync_and_foreign_messages() {
        let mut slave = slave();
        let mut out = [0u8; 64];
        let other: PortIdentity = [7; 10];
        let n = build_sync(&mut out, 0, other, 1, Some(5), 0);
        assert!(slave.handle(&out[..n], 10).is_none());
        assert_eq!(slave.counters.syncs, 0, "a Sync from a port that is not the master is ignored");
        let n = build_sync(&mut out, 1, MASTER, 1, Some(5), 0);
        assert!(slave.handle(&out[..n], 10).is_none());
        assert_eq!(slave.counters.syncs, 0, "another domain is ignored");
        let n = build_sync(&mut out, 0, MASTER, 1, Some(5), 0);
        slave.handle(&out[..n], 10);
        assert_eq!(slave.counters.syncs, 1);
    }

    #[test]
    fn lost_syncs_are_counted_and_a_silent_master_is_dropped() {
        let mut slave = slave();
        let mut out = [0u8; 64];
        for sequence in [1u16, 2, 5] {
            let n = build_sync(&mut out, 0, MASTER, sequence, Some(0), 0);
            slave.handle(&out[..n], 0);
        }
        assert_eq!(slave.counters.sync_lost, 2);
        slave.tick(2_000_000_000);
        assert!(slave.master().is_some());
        slave.tick(4_000_000_000);
        assert!(slave.master().is_none());
    }

    #[test]
    fn best_master_wins() {
        let mut slave = slave();
        let mut worse = Clock::ordinary([0, 0, 0, 0, 0, 1]);
        worse.priority1 = 200;
        let mut better = Clock::ordinary([0, 0, 0, 0, 0, 2]);
        better.priority1 = 10;
        let mut out = [0u8; 64];
        let n = build_announce(&mut out, 0, [8; 10], 1, &worse, 0);
        slave.handle(&out[..n], 1);
        assert_eq!(slave.master().unwrap().0, MASTER, "a worse clock does not displace the master");
        let n = build_announce(&mut out, 0, [9, 8, 8, 8, 8, 8, 8, 8, 0, 1], 1, &better, 0);
        slave.handle(&out[..n], 2);
        assert_eq!(slave.master().unwrap().1.priority1, 10);
    }

    #[test]
    fn a_late_sync_is_skipped_but_not_lost() {
        let mut slave = slave();
        let mut out = [0u8; 64];
        for sequence in [1u16, 2, 3] {
            let n = build_sync(&mut out, 0, MASTER, sequence, None, 0);
            if sequence == 2 {
                slave.discard_sync(&out[..n]);
            } else {
                slave.handle(&out[..n], 0);
            }
        }
        assert_eq!((slave.counters.syncs, slave.counters.late, slave.counters.sync_lost), (2, 1, 0));
    }

    #[test]
    fn window_and_rate() {
        let mut window = Window::default();
        for value in [-10, 10, -10, 10] {
            window.add(value);
        }
        assert_eq!((window.mean(), window.min, window.max, window.deviation()), (0, -10, 10, 10));
        let mut rate = Rate::new();
        for second in 0..10 {
            rate.add(second * 1_000_000_000, second * 12_000); // 12 us per second: +12 ppm
        }
        assert_eq!(rate.tenths_ppm(), Some(120));
    }
}
