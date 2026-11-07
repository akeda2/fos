//! `audio-net sip`: SIP call control (`tape-sip`) in front of the RTP sender and receiver, AES67 style.
//!
//! `sip listen` waits for an INVITE; `sip call=IP[:PORT]` places a call. The SDP offer/answer decides the stream:
//! address, port, payload type, channels, bit depth and packet time. Unless `require-ptp=off`, a stream that is not
//! on the PTP grandmaster this machine follows is refused with 488 (the AES67 default). When a call is up the
//! ordinary `receive` or `send` loop runs with options built from the negotiated stream; those loops hand every
//! frame they read to [`service`] (through `Transport::igmp_service` and the send loop's [`pump`]), so SIP
//! retransmissions, BYE and OPTIONS are answered while audio flows, and a BYE or timeout ends the loop.
//!
//! Which side sends: the one whose SDP says `sendonly`. A caller that offers `sendonly` (`dir=send`, the default)
//! streams to the callee's answer; with `dir=recv` it listens on `port=`. A callee receives from a `sendonly`
//! caller and sends to a `recvonly` one.
//!
//! One call at a time, UDP only, no registration or proxies: peers on one segment, called by address.

use crate::net::{self, Inbound};
use crate::{ChannelSet, Options, Proto, Transport, clock, receive, send};
use tape_net::{ArpCache, Ipv4Address, MacAddress};
use tape_rt::{TapeApi, println};
use tape_sip::sdp::{Direction, Media};
use tape_sip::ua::{self, Env, Event, Offer, Poll, Ua};

struct Session {
    ua: Ua,
    config: net::NetConfig,
    nic: usize,
    arp: ArpCache<8>,
    name: &'static str,
    port: u16,
    sip_port: u16,
    require_ptp: bool,
    /// The newest event the loops have not looked at yet.
    pending: Event,
    ended: bool,
}

static mut SESSION: Option<Session> = None;
static mut INNER_ABORT: Option<extern "C" fn() -> u32> = None;
static mut TO_TEXT: [u8; 24] = [0; 24];

fn session() -> Option<&'static mut Session> {
    unsafe { (*(&raw mut SESSION)).as_mut() }
}

pub fn active() -> bool {
    session().is_some()
}

fn now_ms() -> u64 {
    tape_rt::micros() / 1000
}

impl Session {
    fn env(&self) -> Env<'static> {
        Env {
            ip: self.config.ip.0,
            sip_port: self.sip_port,
            rtp_port: self.port,
            mac: self.config.mac.0,
            name: self.name,
            user: "fos",
            require_ptp: self.require_ptp,
            locked_to: if clock::locked() { clock::grandmaster() } else { None },
            max_channels: 32,
        }
    }

    fn transmit(&mut self, api: &TapeApi, to: ua::Addr, datagram: &[u8]) {
        let target = Ipv4Address(to.0);
        let Some(mac) = net::resolve(api, self.nic, &self.config, &mut self.arp, target) else {
            return println!("sip: no ARP answer from {}.{}.{}.{}.", to.0[0], to.0[1], to.0[2], to.0[3]);
        };
        let mut frame = [0u8; 1514];
        if let Some(length) = net::build_udp_frame(&mut frame, &self.config, mac, target, self.sip_port, to.1, 64, datagram) {
            net::send_frame(api, self.nic, &frame[..length]);
        }
    }

    fn note(&mut self, event: Event) {
        match event {
            Event::None => {}
            Event::Down | Event::Failed(_) => {
                self.pending = event;
                self.ended = true;
            }
            Event::Up(_) => self.pending = event,
        }
    }
}

/// Per pass of a stream loop: retransmissions when `frame` is None, else a received frame. True if SIP took it.
pub fn service(api: &TapeApi, frame: Option<&[u8]>) -> bool {
    let Some(session) = session() else { return false };
    let now = now_ms();
    let Some(frame) = frame else {
        let mut copy = [0u8; ua::MAX_DATAGRAM];
        let (length, to) = match session.ua.poll(now) {
            Poll::Send(bytes, to) => {
                copy[..bytes.len()].copy_from_slice(bytes);
                (bytes.len(), to)
            }
            Poll::Down => {
                session.note(Event::Down);
                return false;
            }
            Poll::Idle => return false,
        };
        session.transmit(api, to, &copy[..length]);
        return false;
    };
    let (source, source_port, payload) = match net::inbound(api, session.nic, &session.config, &mut session.arp, frame, session.sip_port..=session.sip_port, 0) {
        Inbound::Udp { source, source_port, payload, .. } => (source, source_port, payload),
        _ => return false,
    };
    if frame.len() >= 12 {
        // The peer's MAC is on the frame just read: keep it so the reply needs no ARP exchange.
        let mut mac = [0u8; 6];
        mac.copy_from_slice(&frame[6..12]);
        let same_subnet = (0..4).all(|i| source.0[i] & session.config.netmask.0[i] == session.config.ip.0[i] & session.config.netmask.0[i]);
        if same_subnet {
            session.arp.insert(source, MacAddress(mac), 0);
        }
    }
    let env = session.env();
    let mut out = [0u8; ua::MAX_DATAGRAM];
    let outcome = session.ua.handle(now, (source.0, source_port), payload, &env, &mut out);
    if outcome.reply != 0 {
        session.transmit(api, outcome.to, &out[..outcome.reply]);
    }
    session.note(outcome.event);
    true
}

/// The send loop's per-pass network service (it reads no frames of its own): PTP and SIP.
pub fn pump(api: &TapeApi, frame: &mut [u8]) {
    let Some(nic) = session().map(|session| session.nic) else { return };
    for _ in 0..4 {
        let length = (api.nic_receive)(nic, frame.as_mut_ptr(), frame.len());
        if length == 0 {
            break;
        }
        if !clock::consume(api, &frame[..length], tape_rt::micros()) {
            service(api, Some(&frame[..length]));
        }
    }
    clock::poll(api);
    service(api, None);
}

extern "C" fn call_or_abort() -> u32 {
    if session().is_some_and(|session| session.ended) {
        return 1;
    }
    unsafe { (*(&raw const INNER_ABORT)).map_or(0, |inner| inner()) }
}

fn media_options(base: &Options<'static>, media: &Media, send: bool) -> Options<'static> {
    let mut options = base.clone();
    options.proto = Proto::Rtp;
    options.rate = media.rate;
    options.bits = usize::from(media.bits);
    options.chan = usize::from(media.channels);
    options.payload_type = media.payload_type;
    options.ptime_us = media.packet_us.clamp(125, 4000);
    options.ptime_given = true;
    if send {
        let mask = if media.channels >= 32 { u32::MAX } else { (1u32 << media.channels) - 1 };
        if options.channels.is_none_or(|set| set.count() != usize::from(media.channels)) {
            options.channels = Some(ChannelSet(mask));
        }
        let a = media.address;
        let text = unsafe { &mut *(&raw mut TO_TEXT) };
        let mut cursor = tape_sip::msg::Cursor::new(text);
        let _ = core::fmt::Write::write_fmt(&mut cursor, format_args!("{}.{}.{}.{}:{}", a[0], a[1], a[2], a[3], media.port));
        let length = cursor.length;
        options.to = core::str::from_utf8(&unsafe { &*(&raw const TO_TEXT) }[..length]).unwrap_or("");
        options.port = media.port;
    }
    options
}

pub fn run(real: &'static TapeApi, options: &Options<'static>) {
    let mut options = options.clone();
    options.proto = Proto::Rtp;
    let Some(mut transport) = Transport::open(real, &options) else { return };
    transport.start_clock(real, &options);
    let name = if options.name.is_empty() { "fos" } else { options.name };
    unsafe {
        SESSION = Some(Session {
            ua: Ua::new(tape_rt::micros() as u32),
            config: transport.config,
            nic: options.nic,
            arp: ArpCache::new(),
            name,
            port: options.port,
            sip_port: options.sip_port,
            require_ptp: options.require_ptp,
            pending: Event::None,
            ended: false,
        });
        INNER_ABORT = Some(real.should_abort);
    }
    let api = tape_rt::abort::wrap_with(real, call_or_abort);
    let ip = transport.config.ip.0;
    println!("SIP: {}.{}.{}.{}:{}, {}; PTP {}. Escape stops.", ip[0], ip[1], ip[2], ip[3], options.sip_port, if options.sip_call.is_empty() { "waiting for a call" } else { "calling" }, if options.require_ptp { "required" } else { "not required" });
    let mut placed: Option<bool> = None;
    if let Some(target) = parse_target(options.sip_call, options.sip_port) {
        let send = !options.sip_recv;
        placed = Some(send);
        let offer = Offer { send, payload_type: options.payload_type, channels: options.chan as u8, bits: options.bits as u8, rate: options.rate, packet_us: options.ptime_us };
        // Let the PTP clock look around first: the offer states the grandmaster we follow.
        wait_a_little(real, 1500);
        let session = session().unwrap();
        let env = session.env();
        let mut out = [0u8; ua::MAX_DATAGRAM];
        let length = session.ua.call(now_ms(), target, &offer, &env, &mut out);
        if length != 0 {
            session.transmit(api, target, &out[..length]);
        }
    } else if !options.sip_call.is_empty() {
        println!("sip: call= is IP or IP:PORT.");
    }
    let mut frame = [0u8; 1514];
    while real_running() {
        pump(api, &mut frame);
        tape_rt::idle();
        let session = session().unwrap();
        match core::mem::replace(&mut session.pending, Event::None) {
            Event::Up(media) => {
                let send = placed.unwrap_or(media.direction == Direction::RecvOnly);
                let a = media.address;
                println!("SIP: call up, {} {}-bit {} ch {} Hz, {} us packets, {} {}.{}.{}.{}:{}.", if send { "sending" } else { "receiving" }, media.bits, media.channels, media.rate, media.packet_us, if send { "to" } else { "from" }, a[0], a[1], a[2], a[3], media.port);
                let run = media_options(&options, &media, send);
                if send { send_stream(api, &run) } else { receive(api, &run) }
                hangup(api);
                println!("SIP: call over.");
                placed = None;
                if let Some(session) = self::session() {
                    session.ended = false;
                    session.pending = Event::None;
                }
                if !options.sip_call.is_empty() {
                    break;
                }
            }
            Event::Failed(code) => {
                println!("SIP: call failed ({code}).");
                session.ended = false;
                if !options.sip_call.is_empty() {
                    break;
                }
            }
            Event::Down => session.ended = false,
            Event::None => {}
        }
    }
    hangup(api);
    unsafe { SESSION = None };
    transport.stop_clock(real, options.nic);
}

fn send_stream(api: &'static TapeApi, options: &Options<'static>) {
    send(api, options);
}

fn real_running() -> bool {
    unsafe { (*(&raw const INNER_ABORT)).is_some_and(|inner| inner() == 0) }
}

/// Ends a call that is still up with a BYE and gives the answer a moment to arrive.
fn hangup(api: &TapeApi) {
    let Some(session) = session() else { return };
    if session.ended {
        return;
    }
    let env = session.env();
    let mut out = [0u8; ua::MAX_DATAGRAM];
    let length = session.ua.hangup(now_ms(), &env, &mut out);
    if length == 0 {
        return;
    }
    session.transmit(api, session.ua.remote(), &out[..length]);
    let until = tape_rt::micros() + 700_000;
    let mut frame = [0u8; 1514];
    while tape_rt::micros() < until && session.ua.state() == ua::State::Ending {
        pump(api, &mut frame);
        tape_rt::idle();
    }
}

fn wait_a_little(api: &TapeApi, ms: u64) {
    let until = tape_rt::micros() + ms * 1000;
    let mut frame = [0u8; 1514];
    while tape_rt::micros() < until && (api.should_abort)() == 0 {
        pump(api, &mut frame);
        tape_rt::idle();
    }
}

fn parse_target(text: &str, default_port: u16) -> Option<ua::Addr> {
    if text.is_empty() {
        return None;
    }
    let (host, port) = match text.split_once(':') {
        Some((host, port)) => (host, port.parse().ok()?),
        None => (text, default_port),
    };
    Some((tape_sip::sdp::parse_ipv4(host)?, port))
}
