//! The impairment engine behind the `nettest` tape: pure data structures with
//! no kernel callbacks, so the logic is small enough to read (and unit test) on
//! its own. One [`Direction`] models one way through the test link: it decides
//! whether each frame is lost, when it would leave (after the configured delay,
//! jitter and serialization time at the configured rate) and keeps the frames
//! that are still "in flight" in a fixed-size ring of slots.
//!
//! Time is microseconds. The kernel's tick counter runs at 100 Hz, so callers
//! pass `ticks * 10_000`; delays therefore take effect with 10 ms granularity.
#![no_std]

/// Bytes per queue slot: a 12-byte header (due time, length) plus the largest
/// Ethernet frame the NIC drivers hand a tape (1514 bytes).
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
        }
    }

    pub fn is_clean(&self) -> bool {
        self.loss_permille == 0 && self.delay_ms == 0 && self.jitter_ms == 0 && self.rate_kbit == 0
    }
}

/// Named starting points for a realistic bad link, as `(name, description)`.
pub const PRESETS: [(&[u8], &[u8]); 5] = [
    (b"badwifi", b"weak signal: 10% bursty loss, 40+-60 ms, 3 Mbit/s"),
    (b"wifi", b"busy WiFi: 3% bursty loss, 15+-10 ms, 20 Mbit/s"),
    (b"cellular", b"mobile data: 1% loss, 70+-30 ms, 8 Mbit/s"),
    (b"satellite", b"600+-20 ms, 1% loss, 10 Mbit/s, deep queue"),
    (b"dsl", b"slow line: 20+-2 ms, 2 Mbit/s, shallow queue"),
];

/// The impairments for a named preset (see [`PRESETS`]); `queue_limit` is the
/// most the storage allows, and presets that want a shallower queue are capped
/// by it.
pub fn preset(name: &[u8], queue_limit: u32) -> Option<Config> {
    let (loss_permille, burst, delay_ms, jitter_ms, rate_kbit, queue) = match name {
        b"badwifi" => (100, 6, 40, 60, 3000, 64),
        b"wifi" => (30, 4, 15, 10, 20_000, 128),
        b"cellular" => (10, 1, 70, 30, 8000, 128),
        b"satellite" => (10, 1, 600, 20, 10_000, 512),
        b"dsl" => (0, 1, 20, 2, 2000, 48),
        _ => return None,
    };
    Some(Config {
        loss_permille,
        burst,
        delay_ms,
        jitter_ms,
        rate_kbit,
        queue_limit: queue.min(queue_limit),
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
}

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
        if !self.queue.push(frame, due) {
            self.stats.queue_drops = self.stats.queue_drops.wrapping_add(1);
            return;
        }
        self.stats.max_depth = self.stats.max_depth.max(self.queue.len() as u32);
    }

    /// If the oldest queued frame is due, removes it into `out`.
    pub fn release(&mut self, now_us: u64, out: &mut [u8]) -> Option<usize> {
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
    }
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
