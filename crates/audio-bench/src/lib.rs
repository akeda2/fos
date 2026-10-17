//! The pure parts of `audio-net bench`: the control and data frames, the receiver's lateness analysis and the
//! choice of the lowest safe settings. No I/O here, so it has host tests; `tapes/audio-net/src/bench.rs` does the
//! sending, receiving and the master/server conversation.
//!
//! **What is measured.** A sender streams numbered packets of the stream's real size at the stream's real rate
//! (packet time `P`). The receiver notes when each arrives. A receiver that keeps a ring of `target` ms plays packet
//! `n` at (first arrival + n*P + target), so packet `n` is *late* when it arrives more than `target` after its
//! schedule. The schedule is the lower envelope of the arrivals (the minima of half-second windows, a straight line
//! through them, which also absorbs the two sound-card clocks' ppm difference, as the real receiver's resampler
//! does). A packet's *lateness* is its distance above that line; the smallest `target` that leaves at most the
//! tolerated share of packets late or lost is the step's `need`. This is what the receiver's ring level has to
//! cover, so it predicts the underruns of a real run without any audio device.
//!
//! Control frames (ethertype 0x88B5, magic `ABN1`, little endian, zero padded) go unicast between master and server:
//! `dst src 88B5 "ABN1" kind version session(u16) seq(u32) payload`. Data frames carry the stream's own ethertype
//! and destination so the switch treats them like the stream: `dst src ethertype "ABD1" session step seq send_us`,
//! zero padded to the stream's frame length.
#![no_std]

pub const CTRL_ETHERTYPE: u16 = 0x88b5;
pub const CTRL_HEADER: usize = 26;
pub const DATA_HEADER: usize = 14 + 20;
pub const VERSION: u8 = 1;
const MAGIC: &[u8; 4] = b"ABN1";
const DATA_MAGIC: &[u8; 4] = b"ABD1";

pub const K_HELLO: u8 = 1;
pub const K_HELLO_REPLY: u8 = 2;
pub const K_STEP: u8 = 3;
pub const K_STEP_ACK: u8 = 4;
pub const K_QUERY: u8 = 5;
pub const K_REPORT: u8 = 6;
pub const K_ABORT: u8 = 7;

/// Lateness is counted in buckets of this many microseconds, up to `BUCKETS` of them (25.6 ms).
pub const BUCKET_US: u32 = 25;
pub const BUCKETS: usize = 1024;
/// `StepResult::need_us` when the lateness is beyond the histogram or the loss alone exceeds the tolerance.
pub const NEED_NONE: u32 = u32::MAX;
const EMPTY: i32 = i32::MIN;
/// Length of the windows whose minima define the schedule line.
const WINDOW_NS: u64 = 500_000_000;

/// The ptime steps of a sweep, microseconds.
pub const PTIMES_US: [u32; 5] = [125, 250, 500, 1000, 2000];

/// What the master asks of a step: who sends, what the frames look like, how long and how strict.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Params {
    pub step: u16,
    /// 1: the master sends and the server receives; 0: the other way round.
    pub master_sends: bool,
    pub ethertype: u16,
    /// The data frames' destination address (the stream's own, e.g. an AVTP multicast address).
    pub destination: [u8; 6],
    /// Whole Ethernet frame length without FCS.
    pub frame_len: u16,
    /// Packet period in nanoseconds (frames per packet / rate need not be a whole number of microseconds).
    pub period_ns: u32,
    pub secs: u16,
    /// Tolerated late-or-lost packets in parts per million (100 = 0.01 %).
    pub tolerance_ppm: u32,
    /// Receiver's clock minus the sender's, microseconds (0 when both are on one PTP clock).
    pub offset_us: i64,
    /// The data stamps are PTP time rather than the local microsecond clock.
    pub ptp: bool,
}

/// What a receiver found out in one step.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StepResult {
    pub received: u32,
    pub expected: u32,
    pub lost: u32,
    pub reordered: u32,
    pub duplicates: u32,
    /// The largest lateness (rounded up to a bucket): the target that leaves nothing late at all.
    pub max_late_us: u32,
    /// The smallest target within the tolerance, `NEED_NONE` if there is none.
    pub need_us: u32,
    pub min_delay_us: i32,
    pub mean_delay_us: i32,
    /// The packet budget the tolerance allowed for this step.
    pub budget: u32,
}

impl StepResult {
    pub fn pass(&self) -> bool {
        self.need_us != NEED_NONE
    }
}

pub struct Writer<'a> {
    out: &'a mut [u8],
    at: usize,
}

impl<'a> Writer<'a> {
    fn new(out: &'a mut [u8]) -> Self {
        Writer { out, at: 0 }
    }
    fn put(&mut self, data: &[u8]) {
        let end = (self.at + data.len()).min(self.out.len());
        let count = end - self.at;
        self.out[self.at..end].copy_from_slice(&data[..count]);
        self.at = end;
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
    fn i32(&mut self, value: i32) {
        self.put(&value.to_le_bytes());
    }
    fn u64(&mut self, value: u64) {
        self.put(&value.to_le_bytes());
    }
    /// Pads to the Ethernet minimum and returns the frame length.
    fn finish(self) -> usize {
        let length = self.at.max(60).min(self.out.len());
        self.out[self.at..length].fill(0);
        length
    }
}

pub struct Reader<'a> {
    data: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
        let bytes = self.data.get(self.at..self.at + N)?.try_into().ok()?;
        self.at += N;
        Some(bytes)
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
    fn i32(&mut self) -> Option<i32> {
        self.take().map(i32::from_le_bytes)
    }
    fn u64(&mut self) -> Option<u64> {
        self.take().map(u64::from_le_bytes)
    }
}

/// A control frame's fixed fields; the payload follows `CTRL_HEADER`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Control {
    pub source: [u8; 6],
    pub kind: u8,
    pub session: u16,
    pub seq: u32,
}

pub fn control_header<'a>(out: &'a mut [u8], dst: [u8; 6], src: [u8; 6], kind: u8, session: u16, seq: u32) -> Writer<'a> {
    let mut writer = Writer::new(out);
    writer.put(&dst);
    writer.put(&src);
    writer.put(&CTRL_ETHERTYPE.to_be_bytes());
    writer.put(MAGIC);
    writer.u8(kind);
    writer.u8(VERSION);
    writer.u16(session);
    writer.u32(seq);
    writer
}

/// Parses a control frame addressed to `me` (or broadcast); the payload is `frame[CTRL_HEADER..]`.
pub fn parse_control(frame: &[u8]) -> Option<Control> {
    if frame.len() < CTRL_HEADER || frame[12..14] != CTRL_ETHERTYPE.to_be_bytes() || &frame[14..18] != MAGIC || frame[19] != VERSION {
        return None;
    }
    let mut reader = Reader { data: frame, at: 20 };
    Some(Control { source: frame[6..12].try_into().ok()?, kind: frame[18], session: reader.u16()?, seq: reader.u32()? })
}

pub fn hello(out: &mut [u8], dst: [u8; 6], src: [u8; 6], session: u16, seq: u32, t1: u64) -> usize {
    let mut writer = control_header(out, dst, src, K_HELLO, session, seq);
    writer.u64(t1);
    writer.finish()
}

/// The reply echoes `t1`, adds the server's receive and send stamps and says whether it is locked to a PTP clock
/// (and which grandmaster).
pub fn hello_reply(out: &mut [u8], dst: [u8; 6], src: [u8; 6], session: u16, seq: u32, t1: u64, t2: u64, t3: u64, grandmaster: Option<[u8; 8]>) -> usize {
    let mut writer = control_header(out, dst, src, K_HELLO_REPLY, session, seq);
    writer.u64(t1);
    writer.u64(t2);
    writer.u64(t3);
    writer.u8(u8::from(grandmaster.is_some()));
    writer.put(&grandmaster.unwrap_or([0; 8]));
    writer.finish()
}

pub fn parse_hello(frame: &[u8]) -> Option<u64> {
    Reader { data: frame, at: CTRL_HEADER }.u64()
}

pub fn parse_hello_reply(frame: &[u8]) -> Option<(u64, u64, u64, Option<[u8; 8]>)> {
    let mut reader = Reader { data: frame, at: CTRL_HEADER };
    let (t1, t2, t3) = (reader.u64()?, reader.u64()?, reader.u64()?);
    let locked = reader.u8()? != 0;
    let grandmaster = reader.take::<8>()?;
    Some((t1, t2, t3, locked.then_some(grandmaster)))
}

pub fn step(out: &mut [u8], dst: [u8; 6], src: [u8; 6], session: u16, seq: u32, params: &Params) -> usize {
    let mut writer = control_header(out, dst, src, K_STEP, session, seq);
    writer.u16(params.step);
    writer.u8(u8::from(params.master_sends) | u8::from(params.ptp) << 1);
    writer.u16(params.ethertype);
    writer.put(&params.destination);
    writer.u16(params.frame_len);
    writer.u32(params.period_ns);
    writer.u16(params.secs);
    writer.u32(params.tolerance_ppm);
    writer.u64(params.offset_us as u64);
    writer.finish()
}

pub fn parse_step(frame: &[u8]) -> Option<Params> {
    let mut reader = Reader { data: frame, at: CTRL_HEADER };
    let step = reader.u16()?;
    let flags = reader.u8()?;
    Some(Params {
        step,
        master_sends: flags & 1 != 0,
        ptp: flags & 2 != 0,
        ethertype: reader.u16()?,
        destination: reader.take()?,
        frame_len: reader.u16()?,
        period_ns: reader.u32()?,
        secs: reader.u16()?,
        tolerance_ppm: reader.u32()?,
        offset_us: reader.u64()? as i64,
    })
}

pub fn step_ack(out: &mut [u8], dst: [u8; 6], src: [u8; 6], session: u16, seq: u32, step: u16) -> usize {
    let mut writer = control_header(out, dst, src, K_STEP_ACK, session, seq);
    writer.u16(step);
    writer.finish()
}

pub fn parse_step_ack(frame: &[u8]) -> Option<u16> {
    Reader { data: frame, at: CTRL_HEADER }.u16()
}

/// QUERY: "tell me how step `step` went; the sender sent `sent` packets" (0 when the server is the sender).
pub fn query(out: &mut [u8], dst: [u8; 6], src: [u8; 6], session: u16, seq: u32, step: u16, sent: u32) -> usize {
    let mut writer = control_header(out, dst, src, K_QUERY, session, seq);
    writer.u16(step);
    writer.u32(sent);
    writer.finish()
}

pub fn parse_query(frame: &[u8]) -> Option<(u16, u32)> {
    let mut reader = Reader { data: frame, at: CTRL_HEADER };
    Some((reader.u16()?, reader.u32()?))
}

/// REPORT: `done` is false while the step is still running (a sending server that has not finished); `sent` is the
/// sending server's packet count.
pub fn report(out: &mut [u8], dst: [u8; 6], src: [u8; 6], session: u16, seq: u32, step: u16, done: bool, sent: u32, result: &StepResult) -> usize {
    let mut writer = control_header(out, dst, src, K_REPORT, session, seq);
    writer.u16(step);
    writer.u8(u8::from(done));
    writer.u32(sent);
    for value in [result.received, result.expected, result.lost, result.reordered, result.duplicates, result.max_late_us, result.need_us, result.budget] {
        writer.u32(value);
    }
    writer.i32(result.min_delay_us);
    writer.i32(result.mean_delay_us);
    writer.finish()
}

pub fn parse_report(frame: &[u8]) -> Option<(u16, bool, u32, StepResult)> {
    let mut reader = Reader { data: frame, at: CTRL_HEADER };
    let (step, done, sent) = (reader.u16()?, reader.u8()? != 0, reader.u32()?);
    let result = StepResult {
        received: reader.u32()?,
        expected: reader.u32()?,
        lost: reader.u32()?,
        reordered: reader.u32()?,
        duplicates: reader.u32()?,
        max_late_us: reader.u32()?,
        need_us: reader.u32()?,
        budget: reader.u32()?,
        min_delay_us: reader.i32()?,
        mean_delay_us: reader.i32()?,
    };
    Some((step, done, sent, result))
}

/// Builds one data frame (zero padded to `frame_len`); returns its length.
pub fn data_frame(out: &mut [u8], dst: [u8; 6], src: [u8; 6], ethertype: u16, session: u16, step: u16, seq: u32, send_us: u64, frame_len: usize) -> usize {
    let length = frame_len.clamp(DATA_HEADER.max(60), out.len());
    out[..length].fill(0);
    let mut writer = Writer::new(&mut out[..length]);
    writer.put(&dst);
    writer.put(&src);
    writer.put(&ethertype.to_be_bytes());
    writer.put(DATA_MAGIC);
    writer.u16(session);
    writer.u16(step);
    writer.u32(seq);
    writer.u64(send_us);
    length
}

/// A data frame of this ethertype: (source, session, step, seq, send_us).
pub fn parse_data(frame: &[u8], ethertype: u16) -> Option<([u8; 6], u16, u16, u32, u64)> {
    if frame.len() < DATA_HEADER || frame[12..14] != ethertype.to_be_bytes() || &frame[14..18] != DATA_MAGIC {
        return None;
    }
    let mut reader = Reader { data: frame, at: 18 };
    Some((frame[6..12].try_into().ok()?, reader.u16()?, reader.u16()?, reader.u32()?, reader.u64()?))
}

/// The receiver of one step: remembers each packet's arrival, then judges the lot in `finish`.
pub struct Analyzer<'a> {
    /// Arrival of packet `n` relative to the first arrival and the ideal schedule, microseconds; `EMPTY` if not seen.
    x: &'a mut [i32],
    period_ns: u64,
    first: Option<(u32, u64)>,
    received: u32,
    max_seq: u32,
    reordered: u32,
    duplicates: u32,
    min_delay: i64,
    sum_delay: i64,
}

impl<'a> Analyzer<'a> {
    /// `storage` holds one entry per packet that can be told apart; later packets are counted but not analysed.
    pub fn new(storage: &'a mut [i32], period_ns: u64) -> Self {
        storage.fill(EMPTY);
        Analyzer { x: storage, period_ns: period_ns.max(1), first: None, received: 0, max_seq: 0, reordered: 0, duplicates: 0, min_delay: i64::MAX, sum_delay: 0 }
    }

    /// One packet arrived: its sequence number, local arrival time, and the one-way delay (arrival minus send stamp
    /// minus the clock offset), all in microseconds.
    pub fn on_packet(&mut self, seq: u32, arrival_us: u64, delay_us: i64) {
        let Some((first_seq, first_arrival)) = self.first else {
            self.first = Some((seq, arrival_us));
            return self.store(seq, 0, delay_us);
        };
        if seq < self.max_seq {
            self.reordered += 1;
        }
        let ideal_ns = u64::from(seq.wrapping_sub(first_seq)) * self.period_ns;
        let x = (arrival_us.wrapping_sub(first_arrival) as i64) - (ideal_ns / 1000) as i64;
        self.store(seq, x, delay_us);
    }

    fn store(&mut self, seq: u32, x: i64, delay_us: i64) {
        if self.received == 0 || seq > self.max_seq {
            self.max_seq = seq;
        }
        self.received += 1;
        self.min_delay = self.min_delay.min(delay_us);
        self.sum_delay += delay_us;
        if let Some(slot) = self.x.get_mut(seq as usize) {
            if *slot != EMPTY {
                self.duplicates += 1;
            }
            *slot = x.clamp(i64::from(i32::MIN) + 1, i64::from(i32::MAX)) as i32;
        }
    }

    /// `sent`: how many packets the sender sent (the master knows or is told); 0 means "up to the last one heard".
    pub fn finish(&self, sent: u32, tolerance_ppm: u32) -> StepResult {
        let mut result = StepResult { received: self.received, ..StepResult::default() };
        let heard = if self.received == 0 { 0 } else { self.max_seq + 1 };
        result.expected = sent.max(heard);
        let unique = self.received.saturating_sub(self.duplicates);
        result.lost = result.expected.saturating_sub(unique);
        result.budget = ((u64::from(result.expected) * u64::from(tolerance_ppm)).div_ceil(1_000_000)) as u32;
        result.reordered = self.reordered;
        result.duplicates = self.duplicates;
        if self.received > 0 {
            result.min_delay_us = self.min_delay.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
            result.mean_delay_us = (self.sum_delay / i64::from(self.received)).clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
        }
        let (intercept, slope) = self.schedule_line();
        let mut histogram = [0u32; BUCKETS + 1];
        let mut highest = 0usize;
        let mut counted = 0u32;
        for (seq, &x) in self.x.iter().enumerate() {
            if x == EMPTY {
                continue;
            }
            let late = (f64::from(x) - (intercept + slope * seq as f64)).max(0.0) as u32;
            let bucket = ((late / BUCKET_US) as usize).min(BUCKETS);
            histogram[bucket] += 1;
            highest = highest.max(bucket);
            counted += 1;
        }
        result.max_late_us = if highest >= BUCKETS { NEED_NONE } else { (highest as u32 + 1) * BUCKET_US };
        if counted == 0 || result.lost > result.budget {
            result.need_us = NEED_NONE;
            return result;
        }
        // Packets above bucket b are late for a target of (b+1) buckets; allow what the lost ones left of the budget.
        let allowed = result.budget - result.lost;
        let mut above = 0u32;
        let mut bucket = BUCKETS;
        loop {
            if bucket == BUCKETS {
                above += histogram[BUCKETS];
                if above > allowed {
                    result.need_us = NEED_NONE;
                    return result;
                }
            } else if above + histogram[bucket] > allowed {
                result.need_us = (bucket as u32 + 1) * BUCKET_US;
                return result;
            } else {
                above += histogram[bucket];
            }
            if bucket == 0 {
                result.need_us = BUCKET_US;
                return result;
            }
            bucket -= 1;
        }
    }

    /// The lower envelope of the arrivals: a line through the minimum of each half-second window (least squares),
    /// `(intercept, slope per packet)`.
    fn schedule_line(&self) -> (f64, f64) {
        let window = ((WINDOW_NS / self.period_ns) as usize).max(8);
        let (mut n, mut sx, mut sy, mut sxx, mut sxy) = (0f64, 0f64, 0f64, 0f64, 0f64);
        let mut global_min = i32::MAX;
        for (index, chunk) in self.x.chunks(window).enumerate() {
            let Some(minimum) = chunk.iter().copied().filter(|&x| x != EMPTY).min() else { continue };
            global_min = global_min.min(minimum);
            let (px, py) = ((index * window) as f64 + (window as f64) / 2.0, f64::from(minimum));
            n += 1.0;
            sx += px;
            sy += py;
            sxx += px * px;
            sxy += px * py;
        }
        if n < 2.0 || n * sxx - sx * sx < 1.0 {
            return (if global_min == i32::MAX { 0.0 } else { f64::from(global_min) }, 0.0);
        }
        let slope = (n * sxy - sx * sy) / (n * sxx - sx * sx);
        let intercept = (sy - slope * sx) / n;
        // The mean of minima sits above the true floor; lower the line to touch the lowest of them.
        let mut drop = 0f64;
        for (index, chunk) in self.x.chunks(window).enumerate() {
            let Some(minimum) = chunk.iter().copied().filter(|&x| x != EMPTY).min() else { continue };
            let px = (index * window) as f64 + (window as f64) / 2.0;
            drop = drop.max(intercept + slope * px - f64::from(minimum));
        }
        (intercept - drop, slope)
    }
}

/// The ring target (whole milliseconds, what `target=` takes) that covers a step's `need_us` with a margin of a
/// quarter or at least 250 us; at least 1 ms.
pub fn target_ms(need_us: u32) -> u32 {
    let with_margin = u64::from(need_us) + u64::from((need_us / 4).max(250));
    (with_margin.div_ceil(1000) as u32).max(1)
}

/// The delay a (packet time, target) pair adds: one packet plus the ring, microseconds.
pub fn delay_us(ptime_us: u32, target_ms: u32) -> u32 {
    ptime_us + target_ms * 1000
}

/// Of the steps that passed in *both* directions, the one with the smallest delay (ties: the longer packet time, which
/// costs fewer packets per second). `needs[i]` is the larger of the two directions' needs for `PTIMES_US[i]`, or
/// `NEED_NONE` for a step that failed in either. Returns (index, target_ms).
pub fn best(needs: &[u32]) -> Option<(usize, u32)> {
    let mut found: Option<(usize, u32, u32)> = None;
    for (index, &need) in needs.iter().enumerate() {
        if need == NEED_NONE {
            continue;
        }
        let target = target_ms(need);
        let delay = delay_us(PTIMES_US[index], target);
        if found.is_none_or(|(_, _, best)| delay <= best) {
            found = Some((index, target, delay));
        }
    }
    found.map(|(index, target, _)| (index, target))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAC_A: [u8; 6] = [2, 0, 0, 0, 0, 1];
    const MAC_B: [u8; 6] = [2, 0, 0, 0, 0, 2];

    fn params() -> Params {
        Params { step: 3, master_sends: true, ethertype: 0x22f0, destination: [0x91, 0xe0, 0xf0, 0, 0xfe, 1], frame_len: 168, period_ns: 1_000_000, secs: 10, tolerance_ppm: 100, offset_us: -1234, ptp: true }
    }

    #[test]
    fn step_and_report_round_trip() {
        let mut frame = [0u8; 200];
        let length = step(&mut frame, MAC_B, MAC_A, 7, 9, &params());
        assert!(length >= 60);
        let control = parse_control(&frame[..length]).unwrap();
        assert_eq!((control.kind, control.session, control.seq, control.source), (K_STEP, 7, 9, MAC_A));
        assert_eq!(parse_step(&frame[..length]).unwrap(), params());
        let result = StepResult { received: 9990, expected: 10000, lost: 10, reordered: 1, duplicates: 0, max_late_us: 400, need_us: 250, min_delay_us: -5, mean_delay_us: 80, budget: 1 };
        let length = report(&mut frame, MAC_A, MAC_B, 7, 10, 3, true, 10000, &result);
        assert_eq!(parse_report(&frame[..length]).unwrap(), (3, true, 10000, result));
        let length = hello_reply(&mut frame, MAC_A, MAC_B, 7, 1, 11, 22, 33, Some([1, 2, 3, 4, 5, 6, 7, 8]));
        assert_eq!(parse_hello_reply(&frame[..length]).unwrap(), (11, 22, 33, Some([1, 2, 3, 4, 5, 6, 7, 8])));
        let length = hello_reply(&mut frame, MAC_A, MAC_B, 7, 1, 11, 22, 33, None);
        assert_eq!(parse_hello_reply(&frame[..length]).unwrap().3, None);
    }

    #[test]
    fn data_frames_keep_the_stream_ethertype_and_length() {
        let mut frame = [0xffu8; 1514];
        let length = data_frame(&mut frame, MAC_B, MAC_A, 0x22f0, 5, 2, 77, 123_456, 168);
        assert_eq!(length, 168);
        assert_eq!(parse_data(&frame[..length], 0x22f0), Some((MAC_A, 5, 2, 77, 123_456)));
        assert_eq!(parse_data(&frame[..length], 0x88ba), None);
        assert!(frame[34..length].iter().all(|&b| b == 0));
    }

    /// Feeds `count` packets at period `period_us` with a deterministic pseudo-random lateness up to `jitter_us`, the
    /// sender's clock `ppm` slow against the receiver's, and the sequence numbers in `skip` lost.
    fn run(count: u32, period_us: u64, jitter_us: u64, ppm: i64, skip: &[u32], tolerance_ppm: u32) -> StepResult {
        let mut storage = [0i32; 40_000];
        let mut analyzer = Analyzer::new(&mut storage, period_us * 1000);
        let mut state = 12345u64;
        for seq in 0..count {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            if skip.contains(&seq) {
                continue;
            }
            let jitter = if jitter_us == 0 { 0 } else { (state >> 33) % (jitter_us + 1) };
            let drift = (i64::from(seq) * period_us as i64 * ppm / 1_000_000) as u64;
            analyzer.on_packet(seq, 5_000_000 + u64::from(seq) * period_us + drift + jitter, 100 + jitter as i64);
        }
        analyzer.finish(count, tolerance_ppm)
    }

    #[test]
    fn a_steady_stream_needs_the_smallest_target() {
        let result = run(10_000, 1000, 0, 0, &[], 100);
        assert!(result.pass());
        assert_eq!((result.lost, result.need_us), (0, BUCKET_US));
        assert_eq!(result.min_delay_us, 100);
    }

    #[test]
    fn jitter_sets_the_need_and_a_clock_ratio_does_not() {
        let jittery = run(10_000, 1000, 600, 0, &[], 0);
        assert!(jittery.pass());
        assert!((575..=650).contains(&jittery.need_us), "{jittery:?}");
        // 100 ppm over 10 s is 1 ms of drift: it must not show as lateness
        let drifting = run(10_000, 1000, 600, 100, &[], 0);
        assert!((575..=650).contains(&drifting.need_us), "{drifting:?}");
        assert!((575..=650).contains(&drifting.max_late_us), "{drifting:?}");
    }

    #[test]
    fn the_tolerance_spends_on_loss_first_and_late_packets_second() {
        // budget is 1 packet in 10,000 at 100 ppm
        let one_lost = run(10_000, 1000, 600, 0, &[500], 100);
        assert_eq!((one_lost.lost, one_lost.budget), (1, 1));
        assert!(one_lost.pass());
        assert!(one_lost.need_us == one_lost.max_late_us, "no budget left for late packets: {one_lost:?}");
        let two_lost = run(10_000, 1000, 600, 0, &[500, 900], 100);
        assert!(!two_lost.pass());
        let tolerant = run(10_000, 1000, 600, 0, &[], 100);
        assert!(tolerant.need_us <= tolerant.max_late_us);
    }

    #[test]
    fn a_stall_beyond_the_histogram_fails_the_step() {
        let mut storage = [0i32; 2000];
        let mut analyzer = Analyzer::new(&mut storage, 1_000_000);
        for seq in 0..1000u32 {
            analyzer.on_packet(seq, 1_000_000 + u64::from(seq) * 1000 + if seq == 500 { 40_000 } else { 0 }, 0);
        }
        let result = analyzer.finish(1000, 0);
        assert!(!result.pass());
        assert_eq!(result.max_late_us, NEED_NONE);
    }

    #[test]
    fn reordering_and_duplicates_are_counted() {
        let mut storage = [0i32; 100];
        let mut analyzer = Analyzer::new(&mut storage, 1_000_000);
        for (index, seq) in [0u32, 1, 3, 2, 4, 4].into_iter().enumerate() {
            analyzer.on_packet(seq, 1000 + index as u64 * 1000, 0);
        }
        let result = analyzer.finish(5, 1_000_000);
        assert_eq!((result.reordered, result.duplicates, result.lost), (1, 1, 0));
    }

    #[test]
    fn the_best_step_has_the_lowest_total_delay() {
        // 125 us packets need a 2 ms ring (2.125 ms), 500 us packets need 600 us (1 ms ring: 1.5 ms), 2 ms packets pass at 3 ms
        let needs = [1900, 1100, 600, NEED_NONE, 100];
        assert_eq!(target_ms(1900), 3);
        assert_eq!(target_ms(100), 1);
        assert_eq!(best(&needs), Some((2, 1)));
        assert_eq!(best(&[NEED_NONE; 5]), None);
    }
}
