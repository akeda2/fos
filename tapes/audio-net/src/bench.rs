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

use crate::net::{self, Inbound};
use crate::{AVTP_DEFAULT_MAC, MacText, Options, Proto, Transport, clock, parse_mac, peers};
use audio_bench::{Analyzer, Params, PTIMES_US, StepResult, NEED_NONE};
use audio_wire::ChannelSet;
use tape_net::{Ipv4Address, MacAddress};
use tape_rt::{TapeApi, println};

/// RTP mode: control and data are UDP datagrams to this port (the next one after SAP's 9875, so the panel's presence
/// loop, which already reads that range, notices a master calling). Their payload is an `audio_bench` frame without
/// its 14-byte Ethernet header.
pub const BENCH_PORT: u16 = 9876;
/// IPv4 + UDP header bytes a datagram adds to the 14-byte Ethernet-shaped frame the bench builds.
const UDP_OVERHEAD: usize = 20 + 8;

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
    Abort,
    Query { step: u16, sent: u32 },
    Report { step: u16, sent: u32, result: StepResult },
}

struct Ctx {
    api: &'static TapeApi,
    nic: usize,
    mac: [u8; 6],
    transport: Transport,
    /// RTP: where datagrams go (the peer; on a server the master that last called) and the frames' own buffer.
    rtp: bool,
    dest_ip: Ipv4Address,
    dest_mac: MacAddress,
    from: Option<(Ipv4Address, MacAddress)>,
    /// The master's view of the server's MAC (None outside a master run).
    peer: Option<[u8; 6]>,
    session: u16,
    seq: u32,
    ethertype: u16,
    rx: [u8; FRAME],
    tx: [u8; FRAME],
    wire: [u8; FRAME],
}

impl Ctx {
    fn open(api: &'static TapeApi, options: &Options<'static>) -> Option<Ctx> {
        let rtp = options.proto == Proto::Rtp;
        let mut mac = [0u8; 6];
        (api.nic_mac)(options.nic, mac.as_mut_ptr());
        let mut transport = if rtp { Transport::open(api, options)? } else { Transport::from_l2(options, mac) };
        transport.start_clock(api, options);
        if !rtp {
            (api.nic_set_multicast)(options.nic, 1);
        }
        let ethertype = if options.proto == Proto::Avtp { audio_wire::avtp::ETHERTYPE } else { audio_wire::ETHERTYPE };
        Some(Ctx { api, nic: options.nic, mac, transport, rtp, dest_ip: Ipv4Address([0; 4]), dest_mac: MacAddress([0; 6]), from: None, peer: None, session: 0, seq: 0, ethertype, rx: [0; FRAME], tx: [0; FRAME], wire: [0; FRAME] })
    }

    fn close(&mut self) {
        if !self.rtp {
            (self.api.nic_set_multicast)(self.nic, 0);
        }
        self.transport.stop_clock(self.api, self.nic);
    }

    fn aborted(&self) -> bool {
        (self.api.should_abort)() != 0
    }

    /// Sends the frame built in `tx`; over RTP its payload goes in a UDP datagram to `dest_ip`.
    fn send(&mut self, length: usize) {
        if self.rtp {
            if let Some(total) = net::build_udp_frame(&mut self.wire, &self.transport.config, self.dest_mac, self.dest_ip, BENCH_PORT, BENCH_PORT, 64, &self.tx[14..length]) {
                (self.api.nic_send)(self.nic, self.wire.as_ptr(), total);
            }
            return;
        }
        (self.api.nic_send)(self.nic, self.tx.as_ptr(), length);
    }

    /// Sends to a given peer (L2: the MAC in the frame; RTP: the server's own address and MAC).
    fn target(&mut self, ip: Ipv4Address, mac: MacAddress) {
        self.dest_ip = ip;
        self.dest_mac = mac;
    }

    /// The stamp of "now" on the clock a step uses.
    fn stamp(ptp: bool) -> u64 {
        if ptp { clock::time_ns().map_or(0, |ns| (ns / 1000) as u64) } else { tape_rt::micros() }
    }

    /// Reads one frame and says what it was. Data frames of the running step go to `analyzer`.
    fn poll(&mut self, step: Option<&Params>, analyzer: &mut Option<Analyzer<'static>>) -> Event {
        let mut length = (self.api.nic_receive)(self.nic, self.rx.as_mut_ptr(), self.rx.len());
        let arrival = tape_rt::micros();
        if length == 0 {
            self.transport.igmp_service(self.api, self.nic, None);
            return Event::None;
        }
        if self.rtp {
            // Back to the frame shape the parsers know: a datagram's payload behind a made-up Ethernet header.
            let ticks = (self.api.ticks)() as u32;
            let mut shaped = [0u8; FRAME];
            let shaped_length = match net::inbound(self.api, self.nic, &self.transport.config, &mut self.transport.arp, &self.rx[..length], BENCH_PORT..=BENCH_PORT, ticks) {
                Inbound::Udp { source, payload, .. } if payload.len() >= 4 && 14 + payload.len() <= FRAME => {
                    let ethertype = match &payload[..4] {
                        b"ABN1" => audio_bench::CTRL_ETHERTYPE,
                        b"ABD1" => self.ethertype,
                        _ => return Event::None,
                    };
                    shaped[6..12].copy_from_slice(&self.rx[6..12]);
                    shaped[12..14].copy_from_slice(&ethertype.to_be_bytes());
                    shaped[14..14 + payload.len()].copy_from_slice(payload);
                    if ethertype == audio_bench::CTRL_ETHERTYPE {
                        self.from = Some((source, MacAddress(self.rx[6..12].try_into().unwrap_or([0; 6]))));
                    }
                    14 + payload.len()
                }
                Inbound::Nothing => {
                    self.transport.igmp_service(self.api, self.nic, Some(&self.rx[..length]));
                    return Event::None;
                }
                _ => return Event::None,
            };
            self.rx[..shaped_length].copy_from_slice(&shaped[..shaped_length]);
            length = shaped_length;
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
                audio_bench::K_ABORT if control.session == self.session => Event::Abort,
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
            // Over RTP the frame's size counts the IP, UDP and RTP headers the datagram will carry.
            let wanted = usize::from(params.frame_len).saturating_sub(if self.rtp { UDP_OVERHEAD } else { 0 });
            let length = audio_bench::data_frame(&mut self.tx, params.destination, self.mac, self.ethertype, self.session, params.step, sent, Ctx::stamp(params.ptp), wanted);
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
/// `quiet`: no line per step (the panel's screen is only 24 rows); the summary is printed either way.
pub fn master(api: &'static TapeApi, options: &Options<'static>, quiet: bool) -> Option<Outcome> {
    let mut ctx = Ctx::open(api, options)?;
    let outcome = run_master(&mut ctx, options, quiet);
    // Tell the server it may go back to what it was doing (a few times: nothing acknowledges it).
    if let Some(peer) = ctx.peer {
        for _ in 0..3 {
            ctx.seq += 1;
            let length = audio_bench::abort(&mut ctx.tx, peer, ctx.mac, ctx.session, ctx.seq);
            ctx.send(length);
        }
    }
    ctx.close();
    outcome
}

/// Where the master sends: a MAC on layer 2, the IP address (and its MAC) over RTP. Prints why not.
fn resolve_peer(ctx: &mut Ctx, text: &str) -> Option<[u8; 6]> {
    if text.is_empty() {
        println!("audio-net bench: peer=NAME|MAC (RTP: peer=A.B.C.D) names the machine running `audio-net benchserve`.");
        return None;
    }
    if ctx.rtp {
        let Some(ip) = net::parse_ip(text) else {
            println!("audio-net bench: with proto=rtp the peer is its IP address (peer=A.B.C.D).");
            return None;
        };
        let Some(mac) = net::resolve(ctx.api, ctx.nic, &ctx.transport.config, &mut ctx.transport.arp, ip) else {
            println!("audio-net bench: {text} does not answer ARP.");
            return None;
        };
        ctx.target(ip, mac);
        return Some(mac.0);
    }
    parse_mac(text).or_else(|| peers::kernel_peer(ctx.api, text).map(|peer| peer.mac)).or_else(|| {
        println!("audio-net bench: '{text}' has not announced itself (`nic peers`); use its MAC address.");
        None
    })
}

fn run_master(ctx: &mut Ctx, options: &Options<'static>, quiet: bool) -> Option<Outcome> {
    let api = ctx.api;
    let peer = resolve_peer(ctx, options.peer)?;
    ctx.peer = Some(peer);
    let channels = options.channels.unwrap_or(ChannelSet(0b11)).count();
    let bits = options.bits;
    let (header, max_payload) = if ctx.rtp { (28 + 12, audio_wire::MAX_RTP_PAYLOAD) } else if options.proto == Proto::Avtp { (audio_wire::avtp::HEADER_LEN, 1500 - audio_wire::avtp::HEADER_LEN) } else { (audio_wire::HEADER_LEN, audio_wire::MAX_RAW_PAYLOAD) };
    let destination = if ctx.rtp { [0; 6] } else { parse_mac(options.to).unwrap_or(if options.proto == Proto::Avtp { AVTP_DEFAULT_MAC } else { [0xff; 6] }) };
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

    // Clock offset: the sample with the shortest round trip (the others queued somewhere). With PTP here, keep asking
    // (up to 12 s) until the server answers locked too: it starts its clock on the first HELLO.
    let want_ptp = options.ptp && clock::locked();
    let mut best: Option<(i64, i64)> = None;
    let mut grandmaster: Option<[u8; 8]> = None;
    let mut answers = 0;
    for _ in 0..40 {
        ctx.seq += 1;
        let t1 = tape_rt::micros();
        let length = audio_bench::hello(&mut ctx.tx, peer, ctx.mac, ctx.session, ctx.seq, t1);
        ctx.send(length);
        let until = tape_rt::micros() + 300_000;
        while tape_rt::micros() < until && !ctx.aborted() {
            if let Event::HelloReply { seq, t1, t2, t3, grandmaster: reported, t4 } = ctx.poll(None, &mut None) {
                if seq == ctx.seq {
                    let round_trip = (t4 as i64 - t1 as i64) - (t3 as i64 - t2 as i64);
                    let offset = ((t2 as i64 - t1 as i64) + (t3 as i64 - t4 as i64)) / 2;
                    if best.is_none_or(|(rtt, _)| round_trip < rtt) {
                        best = Some((round_trip, offset));
                    }
                    grandmaster = reported;
                    answers += 1;
                    break;
                }
            } else {
                tape_rt::idle();
            }
        }
        if ctx.aborted() || (answers >= 8 && (!want_ptp || grandmaster.is_some())) {
            break;
        }
    }
    let best = best.map(|(round_trip, offset)| (round_trip, offset, grandmaster));
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
            let label = if master_sends { "up  " } else { "down" };
            if quiet {
                tape_rt::print!("\rMeasuring {ptime} us packets, {}...   ", if master_sends { "master to peer" } else { "peer to master" });
            }
            let result = run_step(ctx, peer, &params);
            if ctx.aborted() {
                break 'sweep;
            }
            match result {
                Some(result) => {
                    if !quiet {
                        println!("{label} {:4} us {:3} fr: rx {}/{} lost {} late {} need {} ms", ptime, per_packet, result.received, result.expected, result.lost, Ms(result.max_late_us), Ms(result.need_us));
                        if result.duplicates + result.reordered != 0 {
                            println!("     {} duplicated, {} out of order", result.duplicates, result.reordered);
                        }
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
    if quiet {
        println!();
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
        println!("Note: {} ppm is a late or lost packet about every {} s at 1 ms packets; run tol=0 for a margin.", options.tol_ppm, 1_000_000 / options.tol_ppm.max(1) / 1000);
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

/// `benchserve`: answers masters until Escape.
pub fn serve(api: &'static TapeApi, options: &Options<'static>) {
    let Some(mut ctx) = Ctx::open(api, options) else { return };
    println!("audio-net benchserve: NIC {} (MAC {}), {}. Waiting for a master; Escape stops.", options.nic + 1, MacText(ctx.mac), proto_label(options.proto));
    serve_loop(&mut ctx, false);
    ctx.close();
}

/// The panel's server: a master called while the panel was idle. Serves that one master and returns when it says it is
/// done, goes quiet for 90 s, or Escape is pressed.
pub fn serve_panel(api: &'static TapeApi, options: &Options<'static>) {
    let Some(mut ctx) = Ctx::open(api, options) else { return };
    println!("A master is benchmarking this link ({}, NIC {}). Escape stops.", proto_label(options.proto), options.nic + 1);
    serve_loop(&mut ctx, true);
    ctx.close();
}

fn proto_label(proto: Proto) -> &'static str {
    match proto {
        Proto::Avtp => "AVTP",
        Proto::Rtp => "RTP",
        Proto::Raw => "raw",
    }
}

fn serve_loop(ctx: &mut Ctx, one_master: bool) {
    let mut state = Serving { params: None, analyzer: None, sent: 0, peer: [0; 6], printed: 0 };
    let mut last_heard = tape_rt::micros();
    while !ctx.aborted() {
        let params = state.params;
        let event = ctx.poll(params.as_ref(), &mut state.analyzer);
        if !matches!(event, Event::None) {
            last_heard = tape_rt::micros();
        } else if one_master && tape_rt::micros() - last_heard > 90_000_000 {
            break;
        }
        if matches!(event, Event::Hello { .. } | Event::Step { .. }) && ctx.rtp {
            if let Some((ip, mac)) = ctx.from {
                ctx.target(ip, mac);
            }
        }
        match event {
            Event::None => tape_rt::idle(),
            Event::Abort => {
                if one_master {
                    break;
                }
                println!("Master finished.");
                state = Serving { params: None, analyzer: None, sent: 0, peer: [0; 6], printed: 0 };
                ctx.session = 0;
            }
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
}
