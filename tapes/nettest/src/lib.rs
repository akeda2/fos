//! The impairment engine behind the `nettest` tape: pure data structures with
//! no kernel callbacks, so the logic is small enough to read (and unit test) on
//! its own. One [`Direction`] models one way through the test link: it decides
//! whether each frame is lost, when it would leave (after the configured delay,
//! jitter and serialization time at the configured rate) and keeps the frames
//! that are still "in flight" in a fixed-size ring of slots.
//!
//! Time is microseconds. The kernel's tick counter runs at 100 Hz, so callers
//! pass `ticks * 10_000`; delays therefore take effect with 10 ms granularity.
#![cfg_attr(not(test), no_std)]

pub mod remote;

/// Bytes per queue slot: a 12-byte header (due time, length) plus the largest
/// Ethernet frame the NIC drivers hand a tape (1518 bytes: a VLAN-tagged full frame).
pub const SLOT_BYTES: usize = 1536;
const HEADER_BYTES: usize = 12;

/// Most frames a single frame slot can hold.
pub const MAX_FRAME: usize = SLOT_BYTES - HEADER_BYTES;

/// A small xorshift32 pseudo-random generator. Not cryptographic -- it only
/// has to be fast, never zero, and repeatable from a `seed` for a reproducible
/// test run.
pub struct Rng(u32);

impl Rng {
    pub const fn new(seed: u32) -> Self {
        Self(if seed == 0 { 0x9e37_79b9 } else { seed })
    }

    pub fn next(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }

    /// A value in `0..bound` (`bound` must be non-zero). The modulo bias is
    /// negligible for the bounds used here.
    pub fn below(&mut self, bound: u32) -> u32 {
        self.next() % bound
    }
}

/// Impairments applied to one direction.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Config {
    /// Probability of dropping a frame, in tenths of a percent (50 = 5.0%).
    pub loss_permille: u16,
    /// When a frame is lost, this many frames in a row (including it) are lost.
    pub burst: u16,
    pub delay_ms: u32,
    /// Each frame's delay varies uniformly by up to +/- this many ms.
    pub jitter_ms: u32,
    /// Link speed in kbit/s; 0 means unlimited.
    pub rate_kbit: u32,
    /// Most frames that may be in flight; more are tail-dropped.
    pub queue_limit: u32,
    /// Probability, in tenths of a percent, of delivering a frame twice.
    pub dup_permille: u16,
    /// Probability of swapping a frame with the one after it (see `Direction`).
    pub reorder_permille: u16,
    /// Probability of flipping one bit in the payload (after the Ethernet
    /// header, so the frame is still delivered to the right station).
    pub corrupt_permille: u16,
}

impl Config {
    pub const fn clean(queue_limit: u32) -> Self {
        Self {
            loss_permille: 0,
            burst: 1,
            delay_ms: 0,
            jitter_ms: 0,
            rate_kbit: 0,
            queue_limit,
            dup_permille: 0,
            reorder_permille: 0,
            corrupt_permille: 0,
        }
    }

    pub fn is_clean(&self) -> bool {
        self.loss_permille == 0
            && self.delay_ms == 0
            && self.jitter_ms == 0
            && self.rate_kbit == 0
            && !self.has_faults()
    }

    /// True if duplication, reordering or corruption is configured.
    pub fn has_faults(&self) -> bool {
        self.dup_permille != 0 || self.reorder_permille != 0 || self.corrupt_permille != 0
    }
}

/// Named starting points for a realistic bad link, as `(name, description)`.
pub const PRESETS: [(&[u8], &[u8]); 5] = [
    (b"badwifi", b"weak signal: 10% bursty loss, 40+-60 ms, 3 Mbit/s, dup+reorder"),
    (b"wifi", b"busy WiFi: 3% bursty loss, 15+-10 ms, 20 Mbit/s, some reorder"),
    (b"cellular", b"mobile data: 1% loss, 70+-30 ms, 8 Mbit/s"),
    (b"satellite", b"600+-20 ms, 1% loss, 10 Mbit/s, deep queue"),
    (b"dsl", b"slow line: 20+-2 ms, 2 Mbit/s, shallow queue"),
];

/// The impairments for a named preset (see [`PRESETS`]); `queue_limit` is the
/// most the storage allows, and presets that want a shallower queue are capped
/// by it.
pub fn preset(name: &[u8], queue_limit: u32) -> Option<Config> {
    // (loss, burst, delay, jitter, rate, queue) and (duplicate, reorder) in
    // tenths of a percent: WiFi's link-layer retries show up as both.
    let ((loss_permille, burst, delay_ms, jitter_ms, rate_kbit, queue), extra) = match name {
        b"badwifi" => ((100, 6, 40, 60, 3000, 64), (5, 20)),
        b"wifi" => ((30, 4, 15, 10, 20_000, 128), (2, 5)),
        b"cellular" => ((10, 1, 70, 30, 8000, 128), (0, 0)),
        b"satellite" => ((10, 1, 600, 20, 10_000, 512), (0, 0)),
        b"dsl" => ((0, 1, 20, 2, 2000, 48), (0, 0)),
        _ => return None,
    };
    Some(Config {
        loss_permille,
        burst,
        delay_ms,
        jitter_ms,
        rate_kbit,
        queue_limit: queue.min(queue_limit),
        dup_permille: extra.0,
        reorder_permille: extra.1,
        corrupt_permille: 0,
    })
}

/// Counters for one direction (wrapping, like every counter in this kernel).
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Stats {
    pub received: u32,
    pub lost: u32,
    pub queue_drops: u32,
    pub sent: u32,
    pub send_failed: u32,
    pub bytes_sent: u32,
    pub max_depth: u32,
    pub duplicated: u32,
    pub reordered: u32,
    pub corrupted: u32,
}

/// A ring of fixed-size frame slots over caller-provided storage.
pub struct Queue<'a> {
    storage: &'a mut [u8],
    slots: usize,
    head: usize,
    count: usize,
}

impl<'a> Queue<'a> {
    pub fn new(storage: &'a mut [u8]) -> Self {
        let slots = storage.len() / SLOT_BYTES;
        Self {
            storage,
            slots,
            head: 0,
            count: 0,
        }
    }

    pub fn capacity(&self) -> usize {
        self.slots
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn clear(&mut self) {
        self.head = 0;
        self.count = 0;
    }

    /// Queues a copy of `frame` to leave at `due_us`; false if the ring is
    /// full or the frame does not fit a slot.
    pub fn push(&mut self, frame: &[u8], due_us: u64) -> bool {
        if self.count == self.slots || frame.len() > MAX_FRAME {
            return false;
        }
        let slot = (self.head + self.count) % self.slots;
        let base = slot * SLOT_BYTES;
        self.storage[base..base + 8].copy_from_slice(&due_us.to_le_bytes());
        self.storage[base + 8..base + 12].copy_from_slice(&(frame.len() as u32).to_le_bytes());
        self.storage[base + HEADER_BYTES..base + HEADER_BYTES + frame.len()].copy_from_slice(frame);
        self.count += 1;
        true
    }

    /// Flips bit `bit` (0..8) of byte `offset` of the frame most recently
    /// pushed; false if the queue is empty or the frame is shorter.
    pub fn corrupt_last(&mut self, offset: usize, bit: u8) -> bool {
        if self.count == 0 {
            return false;
        }
        let slot = (self.head + self.count - 1) % self.slots;
        let base = slot * SLOT_BYTES;
        let length = u32::from_le_bytes(self.storage[base + 8..base + 12].try_into().unwrap()) as usize;
        if offset >= length {
            return false;
        }
        self.storage[base + HEADER_BYTES + offset] ^= 1 << (bit & 7);
        true
    }

    /// When the oldest queued frame is due.
    pub fn front_due(&self) -> Option<u64> {
        if self.count == 0 {
            return None;
        }
        let base = self.head * SLOT_BYTES;
        Some(u64::from_le_bytes(self.storage[base..base + 8].try_into().unwrap()))
    }

    /// Removes the oldest frame, copying it to `out`; returns its length.
    pub fn pop_into(&mut self, out: &mut [u8]) -> Option<usize> {
        if self.count == 0 {
            return None;
        }
        let base = self.head * SLOT_BYTES;
        let length = u32::from_le_bytes(self.storage[base + 8..base + 12].try_into().unwrap()) as usize;
        let length = length.min(out.len());
        out[..length].copy_from_slice(&self.storage[base + HEADER_BYTES..base + HEADER_BYTES + length]);
        self.head = (self.head + 1) % self.slots;
        self.count -= 1;
        Some(length)
    }
}

/// One way through the test link.
pub struct Direction<'a> {
    pub config: Config,
    pub stats: Stats,
    pub queue: Queue<'a>,
    /// Frames still to drop from a burst that has already begun.
    burst_left: u16,
    /// When the (simulated) link finishes sending what it has accepted.
    link_free_us: u64,
    /// Latest due time handed out, so jitter never reorders frames.
    last_due_us: u64,
    /// A frame held back to be swapped with the next one (`reorder`), with its
    /// length and the time it was held; it is released on its own if no
    /// further frame arrives within `HELD_LIMIT_US`.
    held: [u8; MAX_FRAME],
    held_len: usize,
    held_due_us: u64,
    held_since_us: u64,
}

/// Longest a frame waits to be swapped with a successor, in microseconds.
const HELD_LIMIT_US: u64 = 50_000;

impl<'a> Direction<'a> {
    pub fn new(storage: &'a mut [u8]) -> Self {
        let queue = Queue::new(storage);
        let limit = queue.capacity() as u32;
        Self {
            config: Config::clean(limit),
            stats: Stats::default(),
            queue,
            burst_left: 0,
            link_free_us: 0,
            last_due_us: 0,
            held: [0; MAX_FRAME],
            held_len: 0,
            held_due_us: 0,
            held_since_us: 0,
        }
    }

    /// Changes the configuration; the queue limit is capped at the storage.
    pub fn set_config(&mut self, config: Config) {
        let cap = self.queue.capacity() as u32;
        self.config = Config {
            queue_limit: config.queue_limit.clamp(1, cap.max(1)),
            burst: config.burst.max(1),
            ..config
        };
        self.burst_left = 0;
    }

    /// Decides the fate of one received frame: lost, tail-dropped, or queued
    /// with the time it should leave.
    pub fn ingest(&mut self, frame: &[u8], now_us: u64, rng: &mut Rng) {
        self.stats.received = self.stats.received.wrapping_add(1);
        if self.burst_left > 0 {
            self.burst_left -= 1;
            self.stats.lost = self.stats.lost.wrapping_add(1);
            return;
        }
        let config = self.config;
        if config.loss_permille > 0 && rng.below(1000) < u32::from(config.loss_permille) {
            self.burst_left = config.burst.saturating_sub(1);
            self.stats.lost = self.stats.lost.wrapping_add(1);
            return;
        }
        if self.queue.len() as u32 >= config.queue_limit {
            self.stats.queue_drops = self.stats.queue_drops.wrapping_add(1);
            return;
        }
        let mut due = now_us + u64::from(config.delay_ms) * 1000;
        if config.jitter_ms > 0 {
            let span = u64::from(config.jitter_ms) * 2000 + 1;
            let offset = u64::from(rng.next()) % span;
            due = (due + offset).saturating_sub(u64::from(config.jitter_ms) * 1000).max(now_us);
        }
        if config.rate_kbit > 0 {
            // Serialization time of this frame on a link of `rate_kbit`:
            // bits / (rate * 1000 bit/s) seconds = bits * 1000 / rate microseconds.
            let transmit = frame.len() as u64 * 8 * 1000 / u64::from(config.rate_kbit);
            self.link_free_us = self.link_free_us.max(now_us) + transmit;
            due = due.max(self.link_free_us);
        }
        due = due.max(self.last_due_us);
        self.last_due_us = due;

        // Reordering holds this frame back so the next one overtakes it. Only
        // one frame is held at a time; a frame arriving while one is held
        // releases it right behind itself.
        if self.held_len == 0
            && config.reorder_permille > 0
            && rng.below(1000) < u32::from(config.reorder_permille)
            && frame.len() <= MAX_FRAME
        {
            self.held[..frame.len()].copy_from_slice(frame);
            self.held_len = frame.len();
            self.held_due_us = due;
            self.held_since_us = now_us;
            self.stats.reordered = self.stats.reordered.wrapping_add(1);
            return;
        }
        if !self.queue.push(frame, due) {
            self.stats.queue_drops = self.stats.queue_drops.wrapping_add(1);
            return;
        }
        if config.corrupt_permille > 0 && rng.below(1000) < u32::from(config.corrupt_permille) {
            // Leave the Ethernet header intact so the frame still reaches the
            // right station; a real link would drop a bad frame at the FCS, but
            // a faulty bridge or driver passes damaged payloads on.
            if frame.len() > 14 {
                let offset = 14 + rng.below((frame.len() - 14) as u32) as usize;
                let bit = rng.below(8) as u8;
                if self.queue.corrupt_last(offset, bit) {
                    self.stats.corrupted = self.stats.corrupted.wrapping_add(1);
                }
            }
        }
        if config.dup_permille > 0 && rng.below(1000) < u32::from(config.dup_permille) {
            if (self.queue.len() as u32) < config.queue_limit && self.queue.push(frame, due) {
                self.stats.duplicated = self.stats.duplicated.wrapping_add(1);
            }
        }
        self.flush_held();
        self.stats.max_depth = self.stats.max_depth.max(self.queue.len() as u32);
    }

    /// Queues the held frame (if any) behind whatever was just queued.
    fn flush_held(&mut self) {
        if self.held_len == 0 {
            return;
        }
        let due = self.held_due_us.max(self.last_due_us);
        self.last_due_us = due;
        let length = self.held_len;
        self.held_len = 0;
        if !self.queue.push(&self.held[..length], due) {
            self.stats.queue_drops = self.stats.queue_drops.wrapping_add(1);
        }
    }

    /// If the oldest queued frame is due, removes it into `out`.
    pub fn release(&mut self, now_us: u64, out: &mut [u8]) -> Option<usize> {
        if self.held_len > 0 && now_us.saturating_sub(self.held_since_us) >= HELD_LIMIT_US {
            self.flush_held();
        }
        if self.queue.front_due()? > now_us {
            return None;
        }
        self.queue.pop_into(out)
    }

    pub fn record_send(&mut self, length: usize, ok: bool) {
        if ok {
            self.stats.sent = self.stats.sent.wrapping_add(1);
            self.stats.bytes_sent = self.stats.bytes_sent.wrapping_add(length as u32);
        } else {
            self.stats.send_failed = self.stats.send_failed.wrapping_add(1);
        }
    }

    /// Restores a clean link and empties the queue (counters are kept).
    pub fn reset(&mut self) {
        self.config = Config::clean(self.queue.capacity() as u32);
        self.queue.clear();
        self.burst_left = 0;
        self.link_free_us = 0;
        self.last_due_us = 0;
        self.held_len = 0;
    }
}

// ---------------------------------------------------------------------------
// Traffic generator and sink. (`remote.rs` runs them on two machines at once.)
//
// The generator emits numbered, timestamped frames at a constant bit rate; the
// sink checks them off. Because both run in the same tape on the same machine
// they share one clock, so the sink's `now - tx timestamp` is a true one-way
// latency with no clock synchronisation to worry about.

/// Ethertype of generated frames (IEEE 802 local experimental range).
pub const GEN_ETHERTYPE: [u8; 2] = [0x88, 0xba];
const GEN_MAGIC: &[u8; 4] = b"FNT1";
/// Bytes of Ethernet header plus the generator's own header (magic, stream,
/// sequence number, timestamp): the smallest frame that can carry them.
pub const GEN_HEADER_LEN: usize = 14 + 4 + 2 + 4 + 8;
/// Values `tape_rt::nic_counters` returns: speed, duplex, then error totals.
pub const NIC_COUNTERS: usize = 9;

/// What a test cost the NIC: link speed and duplex as they are now, every error counter as the change since `start`.
pub fn counter_delta(now: &[u64; NIC_COUNTERS], start: &[u64; NIC_COUNTERS]) -> [u64; NIC_COUNTERS] {
    let mut delta = *now;
    for index in 2..NIC_COUNTERS {
        delta[index] = now[index].saturating_sub(start[index]);
    }
    delta
}

/// Smallest frame the generator will build (an Ethernet minimum frame).
/// Mean frame size of the 7:4:1 mix of 64, 576 and 1500 byte frames.
pub const MIX_MEAN_SIZE: usize = 354;
pub const GEN_MIN_SIZE: usize = 60;
/// Frames sent per call at most, so a long stall cannot turn into one huge
/// burst that overruns the far end's receive ring.
const GEN_MAX_BURST: u32 = 64;
/// If the schedule falls this far behind (microseconds) it is restarted from
/// now instead of catching up, and the event is counted.
const GEN_MAX_LAG_US: u64 = 200_000;

// ---------------------------------------------------------------------------
// Routable mode: the same numbered, timestamped payload carried in an
// Ethernet / IPv4 / UDP frame, so it can cross a router. The generator ARPs for
// its next hop (the gateway, or the destination itself on the same subnet) and
// the sink answers ARP for its own address.

pub const ETHERTYPE_IPV4: [u8; 2] = [0x08, 0x00];
pub const ETHERTYPE_ARP: [u8; 2] = [0x08, 0x06];
/// Ethernet (14) + IPv4 (20) + UDP (8).
pub const UDP_HEADERS_LEN: usize = 42;
/// Where the test header (magic, stream, sequence, timestamp) starts and ends
/// in a UDP frame.
const UDP_TEST_END: usize = UDP_HEADERS_LEN + 18;

/// Addresses and ports of a routable stream.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct UdpPlan {
    pub source_ip: [u8; 4],
    pub destination_ip: [u8; 4],
    /// Whose MAC the frames are addressed to: the gateway, or the destination.
    pub next_hop: [u8; 4],
    pub source_port: u16,
    pub destination_port: u16,
}

/// RFC 1071 Internet checksum of `data`, as the 16-bit value to store.
pub fn internet_checksum(data: &[u8], initial: u32) -> u16 {
    let mut sum = initial;
    let mut chunks = data.chunks_exact(2);
    for pair in &mut chunks {
        sum += u32::from(u16::from_be_bytes([pair[0], pair[1]]));
    }
    if let [last] = chunks.remainder() {
        sum += u32::from(*last) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

/// Builds an Ethernet/IPv4/UDP frame of `size` bytes into `out` carrying the
/// test header; returns `size`.
fn build_udp_frame(
    out: &mut [u8],
    size: usize,
    plan: &UdpPlan,
    destination_mac: [u8; 6],
    source_mac: [u8; 6],
    sequence: u32,
    sent_us: u64,
    offload: bool,
) -> usize {
    out[0..6].copy_from_slice(&destination_mac);
    out[6..12].copy_from_slice(&source_mac);
    out[12..14].copy_from_slice(&ETHERTYPE_IPV4);
    let ip_length = (size - 14) as u16;
    out[14] = 0x45;
    out[15] = 0;
    out[16..18].copy_from_slice(&ip_length.to_be_bytes());
    out[18..20].copy_from_slice(&(sequence as u16).to_be_bytes());
    out[20..22].copy_from_slice(&0x4000u16.to_be_bytes()); // don't fragment
    out[22] = 64; // TTL
    out[23] = 17; // UDP
    out[24..26].copy_from_slice(&[0, 0]);
    out[26..30].copy_from_slice(&plan.source_ip);
    out[30..34].copy_from_slice(&plan.destination_ip);
    let header_checksum = internet_checksum(&out[14..34], 0);
    out[24..26].copy_from_slice(&header_checksum.to_be_bytes());
    let udp_length = (size - 34) as u16;
    out[34..36].copy_from_slice(&plan.source_port.to_be_bytes());
    out[36..38].copy_from_slice(&plan.destination_port.to_be_bytes());
    out[38..40].copy_from_slice(&udp_length.to_be_bytes());
    out[40..42].copy_from_slice(&[0, 0]);
    out[42..46].copy_from_slice(GEN_MAGIC);
    out[46..48].copy_from_slice(&0u16.to_le_bytes());
    out[48..52].copy_from_slice(&sequence.to_le_bytes());
    out[52..60].copy_from_slice(&sent_us.to_le_bytes());
    for (offset, byte) in out[UDP_TEST_END..size].iter_mut().enumerate() {
        *byte = (offset as u8).wrapping_add(sequence as u8);
    }
    if offload {
        return size;
    }
    // UDP checksum over the pseudo-header (source, destination, protocol, length)
    // and the whole datagram; 0 would mean "none", so a computed 0 becomes 0xffff.
    let mut pseudo = 0u32;
    for pair in plan.source_ip.chunks(2).chain(plan.destination_ip.chunks(2)) {
        pseudo += u32::from(u16::from_be_bytes([pair[0], pair[1]]));
    }
    pseudo += 17 + u32::from(udp_length);
    let udp_checksum = match internet_checksum(&out[34..size], pseudo) {
        0 => 0xffff,
        value => value,
    };
    out[40..42].copy_from_slice(&udp_checksum.to_be_bytes());
    size
}

/// What an ARP packet says.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ArpInfo {
    pub is_reply: bool,
    pub sender_mac: [u8; 6],
    pub sender_ip: [u8; 4],
    pub target_ip: [u8; 4],
}

pub fn parse_arp(frame: &[u8]) -> Option<ArpInfo> {
    if frame.len() < 42 || frame[12..14] != ETHERTYPE_ARP || frame[14..20] != [0, 1, 8, 0, 6, 4] {
        return None;
    }
    let operation = u16::from_be_bytes([frame[20], frame[21]]);
    if operation != 1 && operation != 2 {
        return None;
    }
    Some(ArpInfo {
        is_reply: operation == 2,
        sender_mac: frame[22..28].try_into().unwrap(),
        sender_ip: frame[28..32].try_into().unwrap(),
        target_ip: frame[38..42].try_into().unwrap(),
    })
}

/// An ARP request (`target_mac` all zero, sent to broadcast) or reply (sent to
/// `target_mac`), padded to the 60-byte Ethernet minimum. Returns 60.
pub fn build_arp(out: &mut [u8], reply: bool, sender_mac: [u8; 6], sender_ip: [u8; 4],
                 target_mac: [u8; 6], target_ip: [u8; 4]) -> usize {
    out[..60].fill(0);
    out[0..6].copy_from_slice(&if reply { target_mac } else { [0xff; 6] });
    out[6..12].copy_from_slice(&sender_mac);
    out[12..14].copy_from_slice(&ETHERTYPE_ARP);
    out[14..20].copy_from_slice(&[0, 1, 8, 0, 6, 4]);
    out[20..22].copy_from_slice(&(if reply { 2u16 } else { 1 }).to_be_bytes());
    out[22..28].copy_from_slice(&sender_mac);
    out[28..32].copy_from_slice(&sender_ip);
    out[32..38].copy_from_slice(&if reply { target_mac } else { [0; 6] });
    out[38..42].copy_from_slice(&target_ip);
    60
}

/// Parses `a.b.c.d`.
pub fn parse_ip(text: &[u8]) -> Option<[u8; 4]> {
    let mut ip = [0u8; 4];
    let mut count = 0;
    for part in text.split(|&byte| byte == b'.') {
        if count == 4 {
            return None;
        }
        ip[count] = u8::try_from(parse_u32(part)?).ok()?;
        count += 1;
    }
    (count == 4).then_some(ip)
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct GenStats {
    pub sent: u32,
    pub send_failed: u32,
    pub bytes: u64,
    /// Times the schedule fell too far behind and was restarted.
    pub lagged: u32,
}

/// A constant-bit-rate frame generator. The bit rate counts whole Ethernet
/// frames (header included, preamble and FCS not), so 1000 kbit/s of
/// 1000-byte frames is 125 frames a second.
pub struct Generator {
    pub running: bool,
    pub rate_kbit: u32,
    pub size: usize,
    pub destination: [u8; 6],
    /// Stop after this many frames (0 = no limit).
    pub count_limit: u32,
    /// Send the classic 7:4:1 mix of 64, 576 and 1500 byte frames instead of `size`; `rate_kbit` still paces the bytes.
    pub mix: bool,
    /// Rotate the source MAC through this many addresses (0 or 1 = the NIC's own), to fill a switch's address table.
    pub macs: u8,
    /// Stop after this many seconds (0 = no limit).
    pub seconds_limit: u32,
    pub stats: GenStats,
    /// Routable mode: send UDP/IP frames instead of raw ones. `destination`
    /// then holds the next hop's MAC once `resolved`.
    pub udp: Option<UdpPlan>,
    pub resolved: bool,
    /// Leave the UDP checksum 0 for the card to fill in; the main loop sets it for a pass when `tape_rt::tx_offload` is on.
    pub offload: bool,
    sequence: u32,
    /// When the next frame is due, in nanoseconds (finer than a microsecond so
    /// the rounding of a 12 us gigabit interval does not drift).
    next_due_ns: u64,
    started_us: u64,
}

impl Generator {
    pub const fn new() -> Self {
        Self {
            running: false,
            rate_kbit: 1000,
            size: 1000,
            destination: [0xff; 6],
            count_limit: 0,
            mix: false,
            macs: 0,
            seconds_limit: 0,
            stats: GenStats { sent: 0, send_failed: 0, bytes: 0, lagged: 0 },
            udp: None,
            offload: false,
            resolved: true,
            sequence: 0,
            next_due_ns: 0,
            started_us: 0,
        }
    }

    pub fn start(&mut self, now_us: u64) {
        self.running = true;
        self.sequence = 0;
        self.stats = GenStats::default();
        self.next_due_ns = now_us * 1000;
        self.started_us = now_us;
    }

    pub fn stop(&mut self) {
        self.running = false;
    }

    /// Sets the UDP plan; frames wait for the next hop's MAC (`resolve`).
    pub fn set_udp(&mut self, plan: UdpPlan) {
        self.udp = Some(plan);
        self.resolved = false;
    }

    /// The next hop answered ARP: from now on frames go to `mac`.
    pub fn resolve(&mut self, mac: [u8; 6], now_us: u64) {
        self.destination = mac;
        if !self.resolved {
            self.resolved = true;
            // Frames were held, not owed: start the schedule from now.
            self.next_due_ns = self.next_due_ns.max(now_us * 1000);
        }
    }

    /// Nanoseconds between frames for the current size and rate.
    pub fn interval_ns(&self) -> u64 {
        self.interval_for(self.size)
    }

    fn interval_for(&self, size: usize) -> u64 {
        // bits / (rate * 1000 bit/s) seconds = bits * 1_000_000 / rate ns.
        (size as u64 * 8 * 1_000_000) / u64::from(self.rate_kbit.max(1))
    }

    /// Frame size of the next frame: `size`, or the 7:4:1 mix (64, 576, 1500) by sequence number.
    fn frame_size(&self) -> usize {
        if !self.mix {
            return self.size;
        }
        match self.sequence % 12 {
            0..=6 => 64,
            7..=10 => 576,
            _ => 1500,
        }
    }

    /// The source MAC of the next frame: `base` with its last byte stepped through `macs` addresses (the first is the NIC's own).
    fn frame_source(&self, base: [u8; 6]) -> [u8; 6] {
        let mut mac = base;
        if self.macs > 1 {
            mac[5] = mac[5].wrapping_add((self.sequence % u32::from(self.macs)) as u8);
        }
        mac
    }

    /// If a frame is due, builds it into `out` and returns its length; call
    /// again for more (up to a burst limit per `pass`). The caller sends the
    /// frame and reports the outcome with `record_send`.
    pub fn next_frame(&mut self, now_us: u64, source: [u8; 6], out: &mut [u8], sent_this_pass: u32) -> Option<usize> {
        if !self.running || sent_this_pass >= GEN_MAX_BURST || !self.resolved {
            return None;
        }
        if (self.count_limit != 0 && self.stats.sent + self.stats.send_failed >= self.count_limit)
            || (self.seconds_limit != 0
                && now_us.saturating_sub(self.started_us) >= u64::from(self.seconds_limit) * 1_000_000)
        {
            self.running = false;
            return None;
        }
        let now_ns = now_us * 1000;
        if now_ns < self.next_due_ns {
            return None;
        }
        if now_us.saturating_sub(self.next_due_ns / 1000) > GEN_MAX_LAG_US {
            self.next_due_ns = now_ns;
            self.stats.lagged = self.stats.lagged.wrapping_add(1);
        }
        let size = self.frame_size().clamp(GEN_MIN_SIZE.max(GEN_HEADER_LEN), MAX_FRAME).min(out.len());
        let source = self.frame_source(source);
        let interval = self.interval_for(size);
        if let Some(plan) = self.udp {
            build_udp_frame(out, size, &plan, self.destination, source, self.sequence, now_us, self.offload);
            self.sequence = self.sequence.wrapping_add(1);
            self.next_due_ns += interval;
            return Some(size);
        }
        out[0..6].copy_from_slice(&self.destination);
        out[6..12].copy_from_slice(&source);
        out[12..14].copy_from_slice(&GEN_ETHERTYPE);
        out[14..18].copy_from_slice(GEN_MAGIC);
        out[18..20].copy_from_slice(&0u16.to_le_bytes());
        out[20..24].copy_from_slice(&self.sequence.to_le_bytes());
        out[24..32].copy_from_slice(&now_us.to_le_bytes());
        for (offset, byte) in out[32..size].iter_mut().enumerate() {
            *byte = (offset as u8).wrapping_add(self.sequence as u8);
        }
        self.sequence = self.sequence.wrapping_add(1);
        self.next_due_ns += interval;
        Some(size)
    }

    pub fn record_send(&mut self, length: usize, ok: bool) {
        if ok {
            self.stats.sent = self.stats.sent.wrapping_add(1);
            self.stats.bytes += length as u64;
        } else {
            self.stats.send_failed = self.stats.send_failed.wrapping_add(1);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SinkStats {
    pub received: u32,
    pub bytes: u64,
    pub lost: u32,
    pub duplicated: u32,
    pub reordered: u32,
    /// One-way latency in microseconds.
    pub latency_min: u32,
    pub latency_max: u32,
    pub latency_sum: u64,
    /// RFC 3550 interarrival jitter estimate, in microseconds.
    pub jitter: u32,
    pub first_us: u64,
    pub last_us: u64,
    /// Largest latency since the last `Sink::mark_interval` (a sweep step).
    pub interval_latency_max: u32,
    /// Latency histogram: `histogram[i]` counts frames whose one-way latency was
    /// below `HISTOGRAM_BOUNDS_US[i]` (and not below the previous bound); the
    /// last bucket takes everything slower.
    pub histogram: [u32; HISTOGRAM_BUCKETS],
    /// Frames received in each second after the first one (later seconds all land in the last bin): where in a
    /// test the loss happened.
    pub timeline: [u32; TIMELINE_BINS],
}

pub const TIMELINE_BINS: usize = 30;

impl Default for SinkStats {
    fn default() -> Self {
        Sink::new().stats
    }
}

/// Upper bounds (exclusive, microseconds) of the latency histogram's buckets.
pub const HISTOGRAM_BOUNDS_US: [u32; 32] = [
    50, 100, 200, 300, 500, 750, 1_000, 1_500, 2_000, 3_000, 4_000, 5_000, 6_000, 8_000, 10_000, 12_500, 15_000,
    20_000, 25_000, 30_000, 40_000, 50_000, 60_000, 75_000, 100_000, 125_000, 150_000, 200_000, 300_000, 500_000,
    1_000_000, 5_000_000,
];
pub const HISTOGRAM_BUCKETS: usize = HISTOGRAM_BOUNDS_US.len() + 1;

impl SinkStats {
    /// Upper bound of the bucket holding the `permille`-th thousandth of the
    /// frames (500 = median, 990 = 99th percentile); `u32::MAX` if that falls in
    /// the open-ended last bucket, 0 with no frames.
    pub fn latency_percentile(&self, permille: u32) -> u32 {
        let total: u64 = self.histogram.iter().map(|&count| u64::from(count)).sum();
        if total == 0 {
            return 0;
        }
        let target = (total * u64::from(permille)).div_ceil(1000).max(1);
        let mut seen = 0u64;
        for (index, &count) in self.histogram.iter().enumerate() {
            seen += u64::from(count);
            if seen >= target {
                return HISTOGRAM_BOUNDS_US.get(index).copied().unwrap_or(u32::MAX);
            }
        }
        u32::MAX
    }
}

/// Checks generated frames off: counts, loss, duplicates, reordering, latency.
pub struct Sink {
    pub stats: SinkStats,
    /// Routable mode: also accept UDP datagrams to this address and port.
    pub udp_ip: Option<[u8; 4]>,
    pub udp_port: u16,
    /// This machine's clock minus the sender's, for frames from another machine (`remote`); 0 when
    /// the generator is on this machine. Subtracted from every latency.
    pub clock_offset_us: i64,
    expected: u32,
    /// Bit `i` set: sequence `expected - 1 - i` has been received.
    window: u64,
    previous_latency: u32,
    seen_any: bool,
}

impl Sink {
    pub const fn new() -> Self {
        Self {
            udp_ip: None,
            udp_port: 9000,
            stats: SinkStats {
                received: 0, bytes: 0, lost: 0, duplicated: 0, reordered: 0, latency_min: 0,
                latency_max: 0, latency_sum: 0, jitter: 0, first_us: 0, last_us: 0,
                interval_latency_max: 0,
                histogram: [0; HISTOGRAM_BUCKETS],
                timeline: [0; TIMELINE_BINS],
            },
            clock_offset_us: 0,
            expected: 0,
            window: 0,
            previous_latency: 0,
            seen_any: false,
        }
    }

    pub fn reset(&mut self) {
        let (ip, port, offset) = (self.udp_ip, self.udp_port, self.clock_offset_us);
        *self = Self::new();
        self.udp_ip = ip;
        self.udp_port = port;
        self.clock_offset_us = offset;
    }

    /// Consumes a received frame if it is one of the generator's; false if not.
    pub fn on_frame(&mut self, frame: &[u8], now_us: u64) -> bool {
        let (sequence, sent_us) = if frame.len() >= GEN_HEADER_LEN
            && frame[12..14] == GEN_ETHERTYPE
            && &frame[14..18] == GEN_MAGIC
        {
            (
                u32::from_le_bytes(frame[20..24].try_into().unwrap()),
                u64::from_le_bytes(frame[24..32].try_into().unwrap()),
            )
        } else if let (Some(ip), true) = (self.udp_ip, frame.len() >= UDP_TEST_END) {
            if frame[12..14] != ETHERTYPE_IPV4
                || frame[14] >> 4 != 4
                || frame[14] & 0x0f != 5
                || frame[23] != 17
                || frame[30..34] != ip
                || u16::from_be_bytes([frame[36], frame[37]]) != self.udp_port
                || &frame[42..46] != GEN_MAGIC
            {
                return false;
            }
            (
                u32::from_le_bytes(frame[48..52].try_into().unwrap()),
                u64::from_le_bytes(frame[52..60].try_into().unwrap()),
            )
        } else {
            return false;
        };
        let latency = (now_us as i64 - sent_us as i64 - self.clock_offset_us).clamp(0, i64::from(u32::MAX)) as u32;
        let stats = &mut self.stats;
        if !self.seen_any {
            self.seen_any = true;
            self.expected = sequence;
            stats.first_us = now_us;
            stats.latency_min = latency;
        }
        stats.received = stats.received.wrapping_add(1);
        stats.bytes += frame.len() as u64;
        stats.last_us = now_us;
        stats.latency_min = stats.latency_min.min(latency);
        stats.latency_max = stats.latency_max.max(latency);
        stats.interval_latency_max = stats.interval_latency_max.max(latency);
        let bucket = HISTOGRAM_BOUNDS_US.iter().position(|&bound| latency < bound).unwrap_or(HISTOGRAM_BOUNDS_US.len());
        stats.histogram[bucket] += 1;
        let second = (now_us.saturating_sub(stats.first_us) / 1_000_000).min(TIMELINE_BINS as u64 - 1) as usize;
        stats.timeline[second] = stats.timeline[second].saturating_add(1);
        stats.latency_sum += u64::from(latency);
        let delta = latency.abs_diff(self.previous_latency);
        stats.jitter = (stats.jitter as i64 + (i64::from(delta) - i64::from(stats.jitter)) / 16) as u32;
        self.previous_latency = latency;

        let behind = self.expected.wrapping_sub(sequence); // > 0 and < 2^31: an old frame
        if sequence == self.expected {
            self.window = (self.window << 1) | 1;
            self.expected = self.expected.wrapping_add(1);
        } else if behind != 0 && behind < 0x8000_0000 {
            let distance = behind - 1;
            if distance < 64 && self.window & (1 << distance) != 0 {
                stats.duplicated = stats.duplicated.wrapping_add(1);
            } else {
                stats.reordered = stats.reordered.wrapping_add(1);
                stats.lost = stats.lost.saturating_sub(1); // it was counted lost at the gap
                if distance < 64 {
                    self.window |= 1 << distance;
                }
            }
        } else {
            let gap = sequence.wrapping_sub(self.expected);
            stats.lost = stats.lost.wrapping_add(gap);
            self.window = if gap >= 63 { 1 } else { (self.window << (gap + 1)) | 1 };
            self.expected = sequence.wrapping_add(1);
        }
        true
    }

    /// Starts a new measuring interval for `interval_latency_max`.
    pub fn mark_interval(&mut self) {
        self.stats.interval_latency_max = 0;
    }

    /// Average throughput between the first and the last frame, in kbit/s.
    pub fn average_kbit(&self) -> u64 {
        let span = self.stats.last_us.saturating_sub(self.stats.first_us);
        if span == 0 {
            return 0;
        }
        self.stats.bytes * 8 * 1000 / span
    }
}

/// A rate sweep: the generator steps its offered rate from `start` to `end` by
/// `step` kbit/s, holding each rate for `seconds`, and one `StepResult` comes out
/// per step -- offered against delivered, loss and latency -- so the point where a
/// link or a router stops keeping up is a table, not a series of manual runs.
pub struct Sweep {
    pub active: bool,
    pub start: u32,
    pub end: u32,
    pub step: u32,
    pub seconds: u32,
    current: u32,
    step_started_us: u64,
    snapshot: SweepSnapshot,
}

#[derive(Clone, Copy, Default)]
struct SweepSnapshot {
    sent: u32,
    received: u32,
    bytes: u64,
    latency_sum: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StepResult {
    pub offered_kbit: u32,
    pub sent: u32,
    pub received: u32,
    /// Loss in hundredths of a percent.
    pub loss_hundredths: u32,
    pub got_kbit: u32,
    pub latency_avg: u32,
    pub latency_max: u32,
    /// The last step of the sweep.
    pub last: bool,
}

impl Sweep {
    pub const fn new() -> Self {
        Self {
            active: false,
            start: 0,
            end: 0,
            step: 0,
            seconds: 0,
            current: 0,
            step_started_us: 0,
            snapshot: SweepSnapshot { sent: 0, received: 0, bytes: 0, latency_sum: 0 },
        }
    }

    fn take_snapshot(&mut self, generator: &Generator, sink: &mut Sink) {
        sink.mark_interval();
        self.snapshot = SweepSnapshot {
            sent: generator.stats.sent,
            received: sink.stats.received,
            bytes: sink.stats.bytes,
            latency_sum: sink.stats.latency_sum,
        };
    }

    /// Starts the sweep: sets the first rate and starts the generator.
    pub fn begin(&mut self, now_us: u64, generator: &mut Generator, sink: &mut Sink) {
        self.active = true;
        self.current = self.start;
        generator.rate_kbit = self.current.max(1);
        generator.count_limit = 0;
        generator.seconds_limit = 0;
        generator.start(now_us);
        self.step_started_us = now_us;
        self.take_snapshot(generator, sink);
    }

    /// Call every pass; returns a result when the current step has run its time.
    pub fn poll(&mut self, now_us: u64, generator: &mut Generator, sink: &mut Sink) -> Option<StepResult> {
        if !self.active || !generator.running || !generator.resolved {
            // Waiting for ARP: the clock for the step starts when frames can flow.
            if self.active && generator.running {
                self.step_started_us = now_us;
                self.take_snapshot(generator, sink);
            }
            return None;
        }
        let elapsed = now_us.saturating_sub(self.step_started_us);
        if elapsed < u64::from(self.seconds) * 1_000_000 {
            return None;
        }
        let sent = generator.stats.sent.wrapping_sub(self.snapshot.sent);
        let received = sink.stats.received.wrapping_sub(self.snapshot.received);
        let bytes = sink.stats.bytes.saturating_sub(self.snapshot.bytes);
        let latency_sum = sink.stats.latency_sum.saturating_sub(self.snapshot.latency_sum);
        let lost = sent.saturating_sub(received);
        let result = StepResult {
            offered_kbit: self.current,
            sent,
            received,
            loss_hundredths: if sent == 0 { 0 } else { (u64::from(lost) * 10_000 / u64::from(sent)) as u32 },
            got_kbit: (bytes * 8 * 1000 / elapsed.max(1)) as u32,
            latency_avg: if received == 0 { 0 } else { (latency_sum / u64::from(received)) as u32 },
            latency_max: sink.stats.interval_latency_max,
            last: false,
        };
        let next = self.current.saturating_add(self.step.max(1));
        let last = next > self.end;
        if last {
            self.active = false;
            generator.stop();
        } else {
            self.current = next;
            generator.rate_kbit = next;
            self.step_started_us = now_us;
            self.take_snapshot(generator, sink);
        }
        Some(StepResult { last, ..result })
    }
}

/// Parses `aa:bb:cc:dd:ee:ff`.
pub fn parse_mac(text: &[u8]) -> Option<[u8; 6]> {
    let mut mac = [0u8; 6];
    let mut count = 0;
    for part in text.split(|&byte| byte == b':') {
        if count == 6 || part.is_empty() || part.len() > 2 {
            return None;
        }
        let mut value = 0u8;
        for &digit in part {
            let nibble = match digit {
                b'0'..=b'9' => digit - b'0',
                b'a'..=b'f' => digit - b'a' + 10,
                b'A'..=b'F' => digit - b'A' + 10,
                _ => return None,
            };
            value = value.checked_mul(16)?.checked_add(nibble)?;
        }
        mac[count] = value;
        count += 1;
    }
    (count == 6).then_some(mac)
}

/// Parses a plain unsigned decimal number.
pub fn parse_u32(text: &[u8]) -> Option<u32> {
    if text.is_empty() {
        return None;
    }
    let mut value = 0u32;
    for &byte in text {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add(u32::from(byte - b'0'))?;
    }
    Some(value)
}

/// Parses a percentage with at most one decimal ("5", "0.5", "12.5") into
/// tenths of a percent, 0..=1000.
pub fn parse_permille(text: &[u8]) -> Option<u16> {
    let text = text.strip_suffix(b"%").unwrap_or(text);
    let (whole, tenth) = match text.iter().position(|&byte| byte == b'.') {
        Some(dot) => {
            let fraction = &text[dot + 1..];
            if fraction.len() != 1 {
                return None;
            }
            (&text[..dot], u32::from(parse_u32(fraction)?))
        }
        None => (text, 0),
    };
    let value = parse_u32(whole)?.checked_mul(10)?.checked_add(tenth)?;
    (value <= 1000).then_some(value as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn direction(storage: &mut [u8]) -> Direction<'_> {
        Direction::new(storage)
    }

    #[test]
    fn a_clean_link_forwards_immediately() {
        let mut storage = [0u8; SLOT_BYTES * 4];
        let mut link = direction(&mut storage);
        let mut rng = Rng::new(1);
        link.ingest(&[1, 2, 3], 1000, &mut rng);
        let mut out = [0u8; 16];
        assert_eq!(link.release(1000, &mut out), Some(3));
        assert_eq!(&out[..3], &[1, 2, 3]);
    }

    #[test]
    fn delay_holds_a_frame_until_due() {
        let mut storage = [0u8; SLOT_BYTES * 4];
        let mut link = direction(&mut storage);
        link.set_config(Config { delay_ms: 50, ..link.config });
        let mut rng = Rng::new(1);
        link.ingest(&[9; 20], 0, &mut rng);
        let mut out = [0u8; 64];
        assert_eq!(link.release(49_999, &mut out), None);
        assert_eq!(link.release(50_000, &mut out), Some(20));
    }

    #[test]
    fn full_loss_drops_everything_and_bursts_extend_a_loss() {
        let mut storage = [0u8; SLOT_BYTES * 8];
        let mut link = direction(&mut storage);
        link.set_config(Config { loss_permille: 1000, burst: 3, ..link.config });
        let mut rng = Rng::new(7);
        for _ in 0..5 {
            link.ingest(&[0; 10], 0, &mut rng);
        }
        assert_eq!(link.stats.lost, 5);
        assert_eq!(link.queue.len(), 0);

        link.set_config(Config { loss_permille: 0, burst: 1, ..link.config });
        link.ingest(&[0; 10], 0, &mut rng);
        assert_eq!(link.queue.len(), 1);
    }

    #[test]
    fn loss_rate_is_roughly_the_configured_one() {
        let mut storage = [0u8; SLOT_BYTES * 4];
        let mut link = direction(&mut storage);
        link.set_config(Config { loss_permille: 500, ..link.config });
        let mut rng = Rng::new(12345);
        let mut out = [0u8; 64];
        for _ in 0..2000 {
            link.ingest(&[0; 10], 0, &mut rng);
            while link.release(0, &mut out).is_some() {}
        }
        assert!((850..=1150).contains(&link.stats.lost), "{}", link.stats.lost);
    }

    #[test]
    fn rate_spaces_frames_by_their_serialization_time() {
        let mut storage = [0u8; SLOT_BYTES * 4];
        let mut link = direction(&mut storage);
        // 1000 kbit/s: a 125-byte frame takes 1 ms.
        link.set_config(Config { rate_kbit: 1000, ..link.config });
        let mut rng = Rng::new(1);
        link.ingest(&[0; 125], 0, &mut rng);
        link.ingest(&[0; 125], 0, &mut rng);
        let mut out = [0u8; 200];
        assert_eq!(link.release(999, &mut out), None);
        assert_eq!(link.release(1000, &mut out), Some(125));
        assert_eq!(link.release(1999, &mut out), None);
        assert_eq!(link.release(2000, &mut out), Some(125));
    }

    #[test]
    fn a_full_queue_tail_drops() {
        let mut storage = [0u8; SLOT_BYTES * 2];
        let mut link = direction(&mut storage);
        link.set_config(Config { delay_ms: 100, ..link.config });
        let mut rng = Rng::new(1);
        for _ in 0..3 {
            link.ingest(&[0; 10], 0, &mut rng);
        }
        assert_eq!(link.queue.len(), 2);
        assert_eq!(link.stats.queue_drops, 1);
    }

    #[test]
    fn jitter_never_reorders_frames() {
        let mut storage = [0u8; SLOT_BYTES * 16];
        let mut link = direction(&mut storage);
        link.set_config(Config { delay_ms: 20, jitter_ms: 15, ..link.config });
        let mut rng = Rng::new(99);
        for index in 0..10u8 {
            link.ingest(&[index; 4], u64::from(index) * 1000, &mut rng);
        }
        let mut out = [0u8; 8];
        let mut expected = 0u8;
        let mut now = 0u64;
        while expected < 10 {
            if link.release(now, &mut out).is_some() {
                assert_eq!(out[0], expected);
                expected += 1;
            } else {
                now += 500;
            }
        }
    }

    fn drain_all(link: &mut Direction<'_>, now: u64) -> std::vec::Vec<std::vec::Vec<u8>> {
        let mut out = [0u8; 2000];
        let mut frames = std::vec::Vec::new();
        while let Some(length) = link.release(now, &mut out) {
            frames.push(out[..length].to_vec());
        }
        frames
    }

    #[test]
    fn duplication_delivers_a_frame_twice() {
        let mut storage = [0u8; SLOT_BYTES * 8];
        let mut link = direction(&mut storage);
        link.set_config(Config { dup_permille: 1000, ..link.config });
        let mut rng = Rng::new(3);
        link.ingest(&[7; 30], 0, &mut rng);
        let frames = drain_all(&mut link, 0);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0], frames[1]);
        assert_eq!(link.stats.duplicated, 1);
    }

    #[test]
    fn reordering_swaps_a_frame_with_its_successor() {
        let mut storage = [0u8; SLOT_BYTES * 8];
        let mut link = direction(&mut storage);
        let mut rng = Rng::new(3);
        // Hold the first frame only: reorder 100% for it, then off.
        link.set_config(Config { reorder_permille: 1000, ..link.config });
        link.ingest(&[1; 20], 0, &mut rng);
        link.set_config(Config { reorder_permille: 0, ..link.config });
        link.ingest(&[2; 20], 0, &mut rng);
        link.ingest(&[3; 20], 0, &mut rng);
        let order: std::vec::Vec<u8> = drain_all(&mut link, 0).iter().map(|f| f[0]).collect();
        assert_eq!(order, [2, 1, 3]);
        assert_eq!(link.stats.reordered, 1);
    }

    #[test]
    fn a_held_frame_is_released_when_nothing_follows() {
        let mut storage = [0u8; SLOT_BYTES * 4];
        let mut link = direction(&mut storage);
        link.set_config(Config { reorder_permille: 1000, ..link.config });
        let mut rng = Rng::new(3);
        link.ingest(&[9; 20], 0, &mut rng);
        assert_eq!(drain_all(&mut link, 10_000).len(), 0);
        assert_eq!(drain_all(&mut link, HELD_LIMIT_US).len(), 1);
    }

    #[test]
    fn corruption_flips_exactly_one_payload_bit() {
        let mut storage = [0u8; SLOT_BYTES * 4];
        let mut link = direction(&mut storage);
        link.set_config(Config { corrupt_permille: 1000, ..link.config });
        let mut rng = Rng::new(11);
        let original = [0x55u8; 60];
        link.ingest(&original, 0, &mut rng);
        let frames = drain_all(&mut link, 0);
        assert_eq!(&frames[0][..14], &original[..14], "Ethernet header untouched");
        let flipped: u32 = frames[0].iter().zip(original).map(|(a, b)| (a ^ b).count_ones()).sum();
        assert_eq!(flipped, 1);
        assert_eq!(link.stats.corrupted, 1);
    }

    #[test]
    fn every_listed_preset_exists_and_unknown_ones_do_not() {
        for (name, _) in PRESETS {
            let config = preset(name, 1000).unwrap();
            assert!(!config.is_clean());
            assert!(config.burst >= 1 && config.queue_limit >= 1);
        }
        assert_eq!(preset(b"nope", 1000), None);
        assert_eq!(preset(b"satellite", 100).unwrap().queue_limit, 100);
    }

    #[test]
    fn generator_paces_frames_at_the_configured_rate() {
        let mut generator = Generator::new();
        generator.rate_kbit = 1000; // 125 frames/s of 1000 bytes: one every 8 ms
        generator.size = 1000;
        generator.start(1_000_000);
        let mut frame = [0u8; 1514];
        assert_eq!(generator.next_frame(1_000_000, [1; 6], &mut frame, 0), Some(1000));
        assert_eq!(generator.next_frame(1_000_000, [1; 6], &mut frame, 1), None);
        assert_eq!(generator.next_frame(1_007_999, [1; 6], &mut frame, 0), None);
        assert!(generator.next_frame(1_008_000, [1; 6], &mut frame, 0).is_some());
    }

    #[test]
    fn generator_mixes_sizes_and_rotates_source_macs() {
        let mut generator = Generator::new();
        generator.rate_kbit = 1_000_000;
        generator.mix = true;
        generator.macs = 4;
        generator.count_limit = 12;
        generator.start(0);
        let mut frame = [0u8; 1600];
        let (mut sizes, mut total, mut sent) = ([0usize; 3], 0, 0);
        while let Some(length) = generator.next_frame(100_000, [2, 0, 0, 0, 0, 0xfe], &mut frame, sent) {
            sizes[[64, 576, 1500].iter().position(|&size| size == length).unwrap()] += 1;
            assert_eq!(frame[11], 0xfe_u8.wrapping_add((sent % 4) as u8));
            generator.record_send(length, true);
            total += length;
            sent += 1;
        }
        assert_eq!(sizes, [7, 4, 1]);
        assert_eq!(total / 12, MIX_MEAN_SIZE);
    }

    #[test]
    fn generator_limits_a_burst_and_stops_at_its_count() {
        let mut generator = Generator::new();
        generator.rate_kbit = 1_000_000;
        generator.size = 60;
        generator.count_limit = 5;
        generator.start(0);
        let mut frame = [0u8; 100];
        let mut sent = 0;
        // Far behind schedule: still limited to GEN_MAX_BURST per pass, and 5 in all.
        while let Some(length) = generator.next_frame(10_000, [2; 6], &mut frame, sent) {
            generator.record_send(length, true);
            sent += 1;
        }
        assert_eq!(generator.stats.sent, 5);
        assert!(!generator.running);
    }

    #[test]
    fn sink_counts_loss_duplicates_reordering_and_latency() {
        let mut generator = Generator::new();
        generator.rate_kbit = 100_000_000;
        generator.size = 60;
        generator.start(0);
        let mut frames = std::vec::Vec::new();
        let mut buffer = [0u8; 100];
        for pass in 0..4 {
            while let Some(length) = generator.next_frame(1000 * (pass + 1), [2; 6], &mut buffer, 0) {
                frames.push(buffer[..length].to_vec());
                if frames.len() % 10 == 0 {
                    break;
                }
            }
        }
        assert!(frames.len() >= 10);
        let mut sink = Sink::new();
        // sequence 0,1 arrive; 2 is lost; 3 arrives; 1 again (duplicate); 4, then 2 late (reordered).
        for index in [0usize, 1, 3, 1, 4, 2] {
            assert!(sink.on_frame(&frames[index], 5_000));
        }
        assert_eq!(sink.stats.received, 6);
        assert_eq!(sink.stats.duplicated, 1);
        assert_eq!(sink.stats.reordered, 1);
        assert_eq!(sink.stats.lost, 0, "the late frame cancels the gap it was counted in");
        assert!(sink.stats.latency_min >= 1000);
        assert!(!sink.on_frame(&[0u8; 60], 0), "other traffic is not the sink's");
    }

    #[test]
    fn sink_reports_a_real_gap_as_loss() {
        let mut generator = Generator::new();
        generator.rate_kbit = 100_000_000;
        generator.size = 100;
        generator.start(0);
        let mut buffer = [0u8; 200];
        let mut frames = std::vec::Vec::new();
        for _ in 0..8 {
            let length = generator.next_frame(100, [2; 6], &mut buffer, 0).unwrap();
            frames.push(buffer[..length].to_vec());
        }
        let mut sink = Sink::new();
        for index in [0usize, 1, 2, 6, 7] {
            sink.on_frame(&frames[index], 200);
        }
        assert_eq!(sink.stats.lost, 3);
        assert_eq!(sink.stats.received, 5);
    }

    fn udp_plan() -> UdpPlan {
        UdpPlan {
            source_ip: [10, 0, 1, 2],
            destination_ip: [10, 0, 2, 2],
            next_hop: [10, 0, 1, 1],
            source_port: 9001,
            destination_port: 9000,
        }
    }

    #[test]
    fn offloaded_udp_frames_leave_the_checksum_to_the_card() {
        let mut generator = Generator::new();
        generator.rate_kbit = 1_000_000;
        generator.size = 300;
        generator.offload = true;
        generator.set_udp(udp_plan());
        generator.start(100);
        generator.resolve([0xaa; 6], 1_000);
        let mut frame = [0u8; 1514];
        let length = generator.next_frame(1_000, [2; 6], &mut frame, 0).unwrap();
        assert_eq!(length, 300);
        assert_eq!(frame[40..42], [0, 0]);
        assert_eq!(internet_checksum(&frame[14..34], 0), 0, "the IP header checksum is still ours");
    }

    #[test]
    fn udp_frames_are_valid_ipv4_and_udp() {
        let mut generator = Generator::new();
        generator.rate_kbit = 1_000_000;
        generator.size = 300;
        generator.set_udp(udp_plan());
        generator.start(100);
        let mut frame = [0u8; 1514];
        // Held until the next hop's MAC is known.
        assert_eq!(generator.next_frame(1_000, [2; 6], &mut frame, 0), None);
        generator.resolve([0xaa; 6], 1_000);
        let length = generator.next_frame(1_000, [2; 6], &mut frame, 0).unwrap();
        assert_eq!(length, 300);
        assert_eq!(frame[0..6], [0xaa; 6]);
        assert_eq!(frame[12..14], ETHERTYPE_IPV4);
        // A correct IPv4 header sums (with its own checksum) to zero.
        assert_eq!(internet_checksum(&frame[14..34], 0), 0);
        assert_eq!(u16::from_be_bytes([frame[16], frame[17]]), 286);
        // And so does the UDP datagram with its pseudo-header.
        let mut pseudo = 0u32;
        for pair in [10u8, 0, 1, 2, 10, 0, 2, 2].chunks(2) {
            pseudo += u32::from(u16::from_be_bytes([pair[0], pair[1]]));
        }
        pseudo += 17 + 266;
        assert_eq!(internet_checksum(&frame[34..300], pseudo), 0);
    }

    #[test]
    fn the_sink_accepts_udp_only_for_its_address_and_port() {
        let mut generator = Generator::new();
        generator.rate_kbit = 1_000_000;
        generator.size = 100;
        generator.set_udp(udp_plan());
        generator.resolve([1; 6], 0);
        generator.start(0);
        let mut frame = [0u8; 200];
        let length = generator.next_frame(50, [2; 6], &mut frame, 0).unwrap();
        let mut sink = Sink::new();
        assert!(!sink.on_frame(&frame[..length], 100), "no address configured yet");
        sink.udp_ip = Some([10, 0, 2, 2]);
        assert!(sink.on_frame(&frame[..length], 100));
        assert_eq!(sink.stats.received, 1);
        assert_eq!(sink.stats.latency_min, 50);
        sink.udp_port = 9999;
        assert!(!sink.on_frame(&frame[..length], 100), "wrong port");
        sink.udp_port = 9000;
        sink.udp_ip = Some([10, 0, 2, 3]);
        assert!(!sink.on_frame(&frame[..length], 100), "wrong address");
    }

    #[test]
    fn arp_round_trip_and_ip_parsing() {
        let mut buffer = [0u8; 64];
        build_arp(&mut buffer, false, [1; 6], [10, 0, 1, 2], [0; 6], [10, 0, 1, 1]);
        let request = parse_arp(&buffer).unwrap();
        assert!(!request.is_reply);
        assert_eq!(request.target_ip, [10, 0, 1, 1]);
        build_arp(&mut buffer, true, [3; 6], [10, 0, 1, 1], [1; 6], [10, 0, 1, 2]);
        assert_eq!(buffer[0..6], [1; 6]);
        let reply = parse_arp(&buffer).unwrap();
        assert!(reply.is_reply);
        assert_eq!(reply.sender_mac, [3; 6]);
        assert_eq!(parse_ip(b"192.168.0.17"), Some([192, 168, 0, 17]));
        assert_eq!(parse_ip(b"192.168.0"), None);
        assert_eq!(parse_ip(b"192.168.0.256"), None);
    }

    #[test]
    fn a_sweep_steps_the_rate_and_reports_each_step() {
        let mut generator = Generator::new();
        generator.size = 500;
        let mut sink = Sink::new();
        let mut sweep = Sweep::new();
        (sweep.start, sweep.end, sweep.step, sweep.seconds) = (1000, 3000, 1000, 2);
        sweep.begin(0, &mut generator, &mut sink);
        assert_eq!(generator.rate_kbit, 1000);
        assert!(generator.running);
        assert_eq!(sweep.poll(1_999_999, &mut generator, &mut sink), None);
        // Pretend 400 frames were sent and 300 arrived in 500-byte frames.
        generator.stats.sent = 400;
        sink.stats.received = 300;
        sink.stats.bytes = 300 * 500;
        sink.stats.latency_sum = 300 * 700;
        sink.stats.interval_latency_max = 1200;
        let first = sweep.poll(2_000_000, &mut generator, &mut sink).unwrap();
        assert_eq!(first.offered_kbit, 1000);
        assert_eq!((first.sent, first.received), (400, 300));
        assert_eq!(first.loss_hundredths, 2500);
        assert_eq!(first.got_kbit, 600); // 150000 bytes * 8 / 2 s
        assert_eq!((first.latency_avg, first.latency_max), (700, 1200));
        assert!(!first.last);
        assert_eq!(generator.rate_kbit, 2000);
        let _ = sweep.poll(4_000_000, &mut generator, &mut sink).unwrap();
        let third = sweep.poll(6_000_000, &mut generator, &mut sink).unwrap();
        assert!(third.last);
        assert_eq!(third.offered_kbit, 3000);
        assert!(!generator.running && !sweep.active);
    }

    #[test]
    fn the_latency_histogram_gives_percentiles() {
        let mut stats = SinkStats::default();
        // 90 frames under 100 us, 9 under 1 ms, 1 under 10 ms.
        stats.histogram[1] = 90;
        stats.histogram[6] = 9;
        stats.histogram[14] = 1;
        assert_eq!(stats.latency_percentile(500), 100);
        assert_eq!(stats.latency_percentile(950), 1_000);
        assert_eq!(stats.latency_percentile(999), 10_000);
        assert_eq!(SinkStats::default().latency_percentile(500), 0);
        let mut sink = Sink::new();
        let mut generator = Generator::new();
        generator.rate_kbit = 100_000_000;
        generator.size = 100;
        generator.start(0);
        let mut buffer = [0u8; 200];
        let length = generator.next_frame(100, [2; 6], &mut buffer, 0).unwrap();
        sink.on_frame(&buffer[..length], 100 + 150); // 150 us in flight
        assert_eq!(sink.stats.histogram[2], 1, "150 us falls in the 100..200 bucket");
    }

    #[test]
    fn parses_mac_addresses() {
        assert_eq!(parse_mac(b"52:54:00:ab:cd:EF"), Some([0x52, 0x54, 0x00, 0xab, 0xcd, 0xef]));
        assert_eq!(parse_mac(b"52:54:00:ab:cd"), None);
        assert_eq!(parse_mac(b"52:54:00:ab:cd:ef:01"), None);
        assert_eq!(parse_mac(b"zz:54:00:ab:cd:ef"), None);
    }

    #[test]
    fn parses_numbers_and_percentages() {
        assert_eq!(parse_permille(b"5"), Some(50));
        assert_eq!(parse_permille(b"0.5"), Some(5));
        assert_eq!(parse_permille(b"12.5%"), Some(125));
        assert_eq!(parse_permille(b"100"), Some(1000));
        assert_eq!(parse_permille(b"100.1"), None);
        assert_eq!(parse_permille(b"1.25"), None);
        assert_eq!(parse_u32(b"42"), Some(42));
        assert_eq!(parse_u32(b"4x"), None);
    }
}
