//! `audio-net bench`: finds the lowest packet time and receive target a link carries without audible glitches.
//!
//! The master (`bench peer=NAME|MAC`) drives; the other machine only runs `benchserve` (or the panel's server).
//! For each packet time (125 us to 2 ms) the master asks the server to receive, streams numbered packets of the
//! stream's real size and rate for `stepsecs=` seconds, and has the server judge their arrival; then the roles swap
//! (both directions are measured, the larger need counts). The judging is `audio_bench::Analyzer`: how far above the
//! steady schedule the late packets arrive says what ring a receiver needs. No audio device is involved, only the
//! network path (NIC, switch, drivers), which is what decides the floor. Control frames are `audio_bench` frames
//! (ethertype 0x88b5) between the two MACs, so no IP address is needed. Clock offset: PTP when both ends are locked to
//! the same grandmaster, else a four-stamp exchange (the same arithmetic as `nettest`'s remote mode).
//!
//! Nothing is printed while a step streams or receives: a console line costs ~10 ms on real hardware.

use crate::{AVTP_DEFAULT_MAC, MacText, Options, Proto, Transport, clock, parse_mac, peers};
use audio_bench::{Analyzer, Params, PTIMES_US, StepResult, NEED_NONE};
use audio_wire::ChannelSet;
use tape_rt::{TapeApi, println};

/// Packets one step can tell apart (about 12 s at 125 us).
const SLOTS: usize = 100_000;
static mut STORAGE: [i32; SLOTS] = [0; SLOTS];
const FRAME: usize = 1600;

fn storage() -> &'static mut [i32] {
    unsafe { &mut *(&raw mut STORAGE) }
}

/// `tol=` percent ("0.01") as parts per million.
pub fn parse_percent_ppm(text: &str) -> Option<u32> {
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    let whole: u32 = whole.parse().ok()?;
    let mut ppm = whole.checked_mul(10_000)?;
    let mut scale = 1000;
    for digit in fraction.bytes() {
        if !digit.is_ascii_digit() {
            return None;
        }
        ppm += u32::from(digit - b'0') * scale;
        scale /= 10;
        if scale == 0 {
            break;
        }
    }
    (ppm <= 50_000).then_some(ppm)
}

enum Event {
    None,
    Hello { source: [u8; 6], session: u16, seq: u32, t1: u64, t2: u64 },
    HelloReply { seq: u32, t1: u64, t2: u64, t3: u64, grandmaster: Option<[u8; 8]>, t4: u64 },
    Step { source: [u8; 6], seq: u32, params: Params },
    Ack(u16),
    Query { step: u16, sent: u32 },
    Report { step: u16, sent: u32, result: StepResult },
}

struct Ctx {
    api: &'static TapeApi,
    nic: usize,
    mac: [u8; 6],
    transport: Transport,
    session: u16,
    seq: u32,
    ethertype: u16,
    rx: [u8; FRAME],
    tx: [u8; FRAME],
}

impl Ctx {
    fn open(api: &'static TapeApi, options: &Options<'static>) -> Option<Ctx> {
        if options.proto == Proto::Rtp {
            println!("audio-net bench: proto=raw or proto=avtp (layer 2); RTP is not supported here.");
            return None;
        }
        let mut mac = [0u8; 6];
        (api.nic_mac)(options.nic, mac.as_mut_ptr());
        let mut transport = Transport::from_l2(options, mac);
        transport.start_clock(api, options);
        (api.nic_set_multicast)(options.nic, 1);
        let ethertype = if options.proto == Proto::Avtp { audio_wire::avtp::ETHERTYPE } else { audio_wire::ETHERTYPE };
        Some(Ctx { api, nic: options.nic, mac, transport, session: 0, seq: 0, ethertype, rx: [0; FRAME], tx: [0; FRAME] })
    }

    fn close(&mut self) {
        (self.api.nic_set_multicast)(self.nic, 0);
        self.transport.stop_clock(self.api, self.nic);
    }

    fn aborted(&self) -> bool {
        (self.api.should_abort)() != 0
    }

    fn send(&mut self, length: usize) {
        (self.api.nic_send)(self.nic, self.tx.as_ptr(), length);
    }

    /// The stamp of "now" on the clock a step uses.
    fn stamp(ptp: bool) -> u64 {
        if ptp { clock::time_ns().map_or(0, |ns| (ns / 1000) as u64) } else { tape_rt::micros() }
    }

    /// Reads one frame and says what it was. Data frames of the running step go to `analyzer`.
    fn poll(&mut self, step: Option<&Params>, analyzer: &mut Option<Analyzer<'static>>) -> Event {
        let length = (self.api.nic_receive)(self.nic, self.rx.as_mut_ptr(), self.rx.len());
        let arrival = tape_rt::micros();
        if length == 0 {
            self.transport.igmp_service(self.api, self.nic, None);
            return Event::None;
        }
        let frame = &self.rx[..length];
        if length < 14 {
            return Event::None;
        }
        if frame[12..14] == audio_bench::CTRL_ETHERTYPE.to_be_bytes() {
            let Some(control) = audio_bench::parse_control(frame) else { return Event::None };
            return match control.kind {
                audio_bench::K_HELLO => audio_bench::parse_hello(frame).map_or(Event::None, |t1| Event::Hello { source: control.source, session: control.session, seq: control.seq, t1, t2: arrival }),
                audio_bench::K_HELLO_REPLY if control.session == self.session => audio_bench::parse_hello_reply(frame).map_or(Event::None, |(t1, t2, t3, grandmaster)| Event::HelloReply { seq: control.seq, t1, t2, t3, grandmaster, t4: arrival }),
                audio_bench::K_STEP if control.session == self.session => audio_bench::parse_step(frame).map_or(Event::None, |params| Event::Step { source: control.source, seq: control.seq, params }),
                audio_bench::K_STEP_ACK if control.session == self.session => audio_bench::parse_step_ack(frame).map_or(Event::None, Event::Ack),
                audio_bench::K_QUERY if control.session == self.session => audio_bench::parse_query(frame).map_or(Event::None, |(step, sent)| Event::Query { step, sent }),
                audio_bench::K_REPORT if control.session == self.session => audio_bench::parse_report(frame).map_or(Event::None, |(step, done, sent, result)| if done { Event::Report { step, sent, result } } else { Event::None }),
                _ => Event::None,
            };
        }
        if frame[12..14] == self.ethertype.to_be_bytes() {
            if let (Some(params), Some(analyzer)) = (step, analyzer.as_mut()) {
                if let Some((_, session, step, seq, sent_at)) = audio_bench::parse_data(frame, self.ethertype) {
                    if session == self.session && step == params.step {
                        let stamp = if params.ptp { clock::time_ns_at(arrival).map_or(0, |ns| ns / 1000) } else { arrival as i64 };
                        analyzer.on_packet(seq, arrival, stamp - sent_at as i64 - params.offset_us);
                    }
                }
            }
            return Event::None;
        }
        self.transport.igmp_service(self.api, self.nic, Some(&self.rx[..length]));
        Event::None
    }

    /// Streams one step's packets on an absolute schedule; returns how many were sent.
    fn stream(&mut self, params: &Params) -> u32 {
        let total = (u64::from(params.secs) * 1_000_000_000 / u64::from(params.period_ns.max(1))).min(SLOTS as u64) as u32;
        let start = tape_rt::micros();
        let mut sent = 0;
        while sent < total {
            let due = start + u64::from(sent) * u64::from(params.period_ns) / 1000;
            while tape_rt::micros() < due {
                core::hint::spin_loop();
            }
            if sent % 64 == 0 && self.aborted() {
                break;
            }
            let length = audio_bench::data_frame(&mut self.tx, params.destination, self.mac, self.ethertype, self.session, params.step, sent, Ctx::stamp(params.ptp), usize::from(params.frame_len));
            self.send(length);
            sent += 1;
        }
        sent
    }

    /// Processes frames for `micros` microseconds, feeding the analyzer.
    fn drain(&mut self, micros: u64, params: &Params, analyzer: &mut Option<Analyzer<'static>>) {
        let until = tape_rt::micros() + micros;
        while tape_rt::micros() < until && !self.aborted() {
            if matches!(self.poll(Some(params), analyzer), Event::None) {
                tape_rt::idle();
            }
        }
    }
}

fn percent(ppm: u32) -> (u32, u32) {
    (ppm / 10_000, (ppm % 10_000) / 100)
}

struct Ms(u32);

impl core::fmt::Display for Ms {
    fn fmt(&self, formatter: &mut core::fmt::Formatter) -> core::fmt::Result {
        if self.0 == NEED_NONE { write!(formatter, "  fail") } else { write!(formatter, "{:2}.{:02}", self.0 / 1000, (self.0 % 1000) / 10) }
    }
}

/// What the master measured for one packet time.
#[derive(Clone, Copy)]
struct Row {
    per_packet: usize,
    up: Option<StepResult>,
    down: Option<StepResult>,
}

/// The settings a finished bench recommends (for the panel to apply).
#[derive(Clone, Copy)]
pub struct Outcome {
    pub ptime_us: u32,
    pub target_ms: u32,
}

/// Runs the benchmark against `options.peer`. Returns the recommendation, if any step passed in both directions.
pub fn master(api: &'static TapeApi, options: &Options<'static>) -> Option<Outcome> {
    let mut ctx = Ctx::open(api, options)?;
    let outcome = run_master(&mut ctx, options);
    ctx.close();
    outcome
}

fn resolve_peer(api: &TapeApi, text: &str) -> Option<[u8; 6]> {
    if text.is_empty() {
        println!("audio-net bench: peer=NAME|MAC names the machine running `audio-net benchserve`.");
        return None;
    }
    parse_mac(text).or_else(|| peers::kernel_peer(api, text).map(|peer| peer.mac)).or_else(|| {
        println!("audio-net bench: '{text}' has not announced itself (`nic peers`); use its MAC address.");
        None
    })
}

fn run_master(ctx: &mut Ctx, options: &Options<'static>) -> Option<Outcome> {
    let api = ctx.api;
    let peer = resolve_peer(api, options.peer)?;
    let channels = options.channels.unwrap_or(ChannelSet(0b11)).count();
    let bits = options.bits;
    let (header, max_payload) = if options.proto == Proto::Avtp { (audio_wire::avtp::HEADER_LEN, 1500 - audio_wire::avtp::HEADER_LEN) } else { (audio_wire::HEADER_LEN, audio_wire::MAX_RAW_PAYLOAD) };
    let destination = parse_mac(options.to).unwrap_or(if options.proto == Proto::Avtp { AVTP_DEFAULT_MAC } else { [0xff; 6] });
    let (whole, hundredths) = percent(options.tol_ppm);
    println!("audio-net bench: {} {} Hz {}-bit x{} to {} via NIC {} (MAC {}).", if options.proto == Proto::Avtp { "AVTP" } else { "raw" }, options.rate, bits, channels, MacText(peer), options.nic + 1, MacText(ctx.mac));
    println!("Tolerance {whole}.{hundredths:02} % late or lost ({} ppm); {} s per step; Escape stops.", options.tol_ppm, options.step_secs);

    if options.ptp {
        if clock::wait_for_lock(api) {
            println!("PTP: locked to a clock here.");
        } else if ctx.aborted() {
            return None;
        }
    }
    ctx.session = (tape_rt::micros() as u16) | 1;

    // Clock offset: the sample with the shortest round trip (the others queued somewhere).
    let mut best: Option<(i64, i64, Option<[u8; 8]>)> = None;
    for attempt in 0..16 {
        ctx.seq += 1;
        let t1 = tape_rt::micros();
        let length = audio_bench::hello(&mut ctx.tx, peer, ctx.mac, ctx.session, ctx.seq, t1);
        ctx.send(length);
        let until = tape_rt::micros() + 300_000;
        while tape_rt::micros() < until && !ctx.aborted() {
            if let Event::HelloReply { seq, t1, t2, t3, grandmaster, t4 } = ctx.poll(None, &mut None) {
                if seq == ctx.seq {
                    let round_trip = (t4 as i64 - t1 as i64) - (t3 as i64 - t2 as i64);
                    let offset = ((t2 as i64 - t1 as i64) + (t3 as i64 - t4 as i64)) / 2;
                    if best.is_none_or(|(rtt, _, _)| round_trip < rtt) {
                        best = Some((round_trip, offset, grandmaster));
                    }
                    break;
                }
            } else {
                tape_rt::idle();
            }
        }
        if best.is_some() && attempt >= 7 {
            break;
        }
    }
    let Some((round_trip, offset, grandmaster)) = best else {
        println!("audio-net bench: {} does not answer; start `audio-net benchserve` there (same network, same proto).", MacText(peer));
        return None;
    };
    let ptp = options.ptp && clock::locked() && grandmaster.is_some() && grandmaster == clock::grandmaster();
    let offset_us = if ptp { 0 } else { offset };
    println!("Peer answers: round trip {round_trip} us; clocks: {}.", if ptp { "PTP, same grandmaster" } else { "measured offset (no shared PTP clock; absolute delays are rough)" });

    let mut rows = [Row { per_packet: 0, up: None, down: None }; PTIMES_US.len()];
    let mut step = 0u16;
    'sweep: for (index, &ptime) in PTIMES_US.iter().enumerate() {
        let per_packet = audio_wire::frames_per_packet_within(options.rate, channels, bits, max_payload, ptime);
        if rows[..index].iter().any(|row| row.per_packet == per_packet) {
            continue;
        }
        rows[index].per_packet = per_packet;
        let period_ns = (per_packet as u64 * 1_000_000_000 / u64::from(options.rate)) as u32;
        let secs = options.step_secs.min((SLOTS as u64 * u64::from(period_ns) / 1_000_000_000) as u16).max(1);
        let frame_len = (14 + header + per_packet * channels * bits / 8).max(60) as u16;
        for master_sends in [true, false] {
            step += 1;
            let destination = if master_sends || destination[0] & 1 != 0 { destination } else { ctx.mac };
            let params = Params { step, master_sends, ethertype: ctx.ethertype, destination, frame_len, period_ns, secs, tolerance_ppm: options.tol_ppm, offset_us: if master_sends { offset_us } else { -offset_us }, ptp };
            let result = run_step(ctx, peer, &params);
            if ctx.aborted() {
                break 'sweep;
            }
            let label = if master_sends { "up  " } else { "down" };
            match result {
                Some(result) => {
                    println!("{label} {:4} us {:3} fr: rx {}/{} lost {} late {} need {} ms", ptime, per_packet, result.received, result.expected, result.lost, Ms(result.max_late_us), Ms(result.need_us));
                    if result.duplicates + result.reordered != 0 {
                        println!("     {} duplicated, {} out of order", result.duplicates, result.reordered);
                    }
                    if master_sends { rows[index].up = Some(result) } else { rows[index].down = Some(result) }
                }
                None => println!("{label} {ptime:4} us: no result (the peer stopped answering)."),
            }
        }
    }
    if ctx.aborted() {
        println!("audio-net bench: stopped.");
        return None;
    }
    summary(options, &rows)
}

fn run_step(ctx: &mut Ctx, peer: [u8; 6], params: &Params) -> Option<StepResult> {
    let mut analyzer = (!params.master_sends).then(|| Analyzer::new(storage(), u64::from(params.period_ns)));
    let mut acked = false;
    for _ in 0..10 {
        ctx.seq += 1;
        let length = audio_bench::step(&mut ctx.tx, peer, ctx.mac, ctx.session, ctx.seq, params);
        ctx.send(length);
        let until = tape_rt::micros() + 300_000;
        while tape_rt::micros() < until && !acked && !ctx.aborted() {
            match ctx.poll(Some(params), &mut analyzer) {
                Event::Ack(step) if step == params.step => acked = true,
                Event::None => tape_rt::idle(),
                _ => {}
            }
        }
        if acked || ctx.aborted() {
            break;
        }
    }
    if !acked {
        return None;
    }
    let mut sent = 0;
    if params.master_sends {
        sent = ctx.stream(params);
        ctx.drain(100_000, params, &mut analyzer);
    } else {
        ctx.drain(u64::from(params.secs) * 1_000_000 + 300_000, params, &mut analyzer);
    }
    let deadline = tape_rt::micros() + 10_000_000;
    let mut last_query = 0;
    while tape_rt::micros() < deadline && !ctx.aborted() {
        let now = tape_rt::micros();
        if now.wrapping_sub(last_query) > 300_000 {
            last_query = now;
            ctx.seq += 1;
            let length = audio_bench::query(&mut ctx.tx, peer, ctx.mac, ctx.session, ctx.seq, params.step, sent);
            ctx.send(length);
        }
        match ctx.poll(Some(params), &mut analyzer) {
            Event::Report { step, sent: reported, result } if step == params.step => {
                return Some(match analyzer {
                    Some(analyzer) => analyzer.finish(reported, params.tolerance_ppm),
                    None => result,
                });
            }
            Event::None => tape_rt::idle(),
            _ => {}
        }
    }
    None
}

fn summary(options: &Options<'static>, rows: &[Row]) -> Option<Outcome> {
    let mut needs = [NEED_NONE; PTIMES_US.len()];
    let mut clean = [NEED_NONE; PTIMES_US.len()];
    println!("\nptime   up need  up clean  down need down clean  (milliseconds)");
    for (index, row) in rows.iter().enumerate() {
        let (Some(up), Some(down)) = (row.up, row.down) else { continue };
        needs[index] = up.need_us.max(down.need_us);
        clean[index] = up.max_late_us.max(down.max_late_us);
        println!("{:4} us {} {}   {}  {}", PTIMES_US[index], Ms(up.need_us), Ms(up.max_late_us), Ms(down.need_us), Ms(down.max_late_us));
    }
    let outcome = |needs: &[u32; PTIMES_US.len()]| audio_bench::best(needs).map(|(index, target)| (PTIMES_US[index], target));
    let tolerant = outcome(&needs);
    let strict = outcome(&clean);
    let (whole, hundredths) = percent(options.tol_ppm);
    match tolerant {
        Some((ptime, target)) => println!("\nLowest within {whole}.{hundredths:02} %: ptime={ptime} target={target}  (about {} us one way in the buffers)", audio_bench::delay_us(ptime, target)),
        None => println!("\nNo packet time passed in both directions within {whole}.{hundredths:02} %: the link drops or delays too much; try a longer stepsecs, another switch port or tol=1."),
    }
    if let Some((ptime, target)) = strict {
        println!("Without a single late packet: ptime={ptime} target={target}  (about {} us)", audio_bench::delay_us(ptime, target));
    }
    if options.tol_ppm >= 100 {
        println!("Note: {} ppm is a late or lost packet about every {} s at 1 ms packets; run tol=0 for a margin.", options.tol_ppm, 1_000_000 / options.tol_ppm.max(1));
    }
    let (ptime_us, target_ms) = tolerant?;
    Some(Outcome { ptime_us, target_ms })
}

/// What the server remembers of the step in progress.
struct Serving {
    params: Option<Params>,
    analyzer: Option<Analyzer<'static>>,
    sent: u32,
    peer: [u8; 6],
    printed: u16,
}

/// `benchserve`: answers a master until Escape.
pub fn serve(api: &'static TapeApi, options: &Options<'static>) {
    let Some(mut ctx) = Ctx::open(api, options) else { return };
    println!("audio-net benchserve: NIC {} (MAC {}), {}. Waiting for a master; Escape stops.", options.nic + 1, MacText(ctx.mac), if options.proto == Proto::Avtp { "AVTP" } else { "raw" });
    let mut state = Serving { params: None, analyzer: None, sent: 0, peer: [0; 6], printed: 0 };
    while !ctx.aborted() {
        let params = state.params;
        let event = ctx.poll(params.as_ref(), &mut state.analyzer);
        match event {
            Event::None => tape_rt::idle(),
            Event::Hello { source, session, seq, t1, t2 } => {
                if ctx.session != session {
                    println!("Master {} (session {session}).", MacText(source));
                }
                ctx.session = session;
                state = Serving { params: None, analyzer: None, sent: 0, peer: source, printed: 0 };
                let grandmaster = if clock::locked() { clock::grandmaster() } else { None };
                let length = audio_bench::hello_reply(&mut ctx.tx, source, ctx.mac, session, seq, t1, t2, tape_rt::micros(), grandmaster);
                ctx.send(length);
            }
            Event::Step { source, seq, params } => {
                let length = audio_bench::step_ack(&mut ctx.tx, source, ctx.mac, ctx.session, seq, params.step);
                ctx.send(length);
                if state.params.is_some_and(|current| current.step == params.step) {
                    continue; // a repeated STEP: already running
                }
                state.peer = source;
                state.params = Some(params);
                state.sent = 0;
                if params.master_sends {
                    state.analyzer = Some(Analyzer::new(storage(), u64::from(params.period_ns)));
                } else {
                    state.analyzer = None;
                    state.sent = ctx.stream(&params);
                }
            }
            Event::Query { step, sent } => {
                let Some(params) = state.params.filter(|params| params.step == step) else { continue };
                let (reported, result) = match state.analyzer.as_ref() {
                    Some(analyzer) => (sent, analyzer.finish(sent, params.tolerance_ppm)),
                    None => (state.sent, StepResult::default()),
                };
                let length = audio_bench::report(&mut ctx.tx, state.peer, ctx.mac, ctx.session, ctx.seq, step, true, reported, &result);
                ctx.send(length);
                if params.master_sends && state.printed != step {
                    state.printed = step;
                    println!("step {step}: rx {}/{} lost {} need {} ms", result.received, result.expected, result.lost, Ms(result.need_us));
                }
            }
            _ => {}
        }
    }
    ctx.close();
}
