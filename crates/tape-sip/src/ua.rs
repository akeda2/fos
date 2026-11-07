//! A SIP user agent for one call at a time over UDP (RFC 3261, the parts AES67 needs).
//!
//! It answers INVITE (SDP offer/answer, RFC 3264) with 200 OK and retransmits that until the ACK arrives, answers OPTIONS
//! and BYE, refuses a second call with 486, and can place a call and hang up. Offers are checked with [`sdp::check`]:
//! with `require_ptp` a stream that is not on this machine's PTP grandmaster is refused with 488 and a `Warning:` text.
//! No proxies, registration, authentication or re-INVITE: peers on one segment, called by address.
//!
//! The caller feeds datagrams to [`Ua::handle`] and sends what it returns, and calls [`Ua::poll`] regularly with the
//! clock in milliseconds for retransmissions and timeouts. A call is reported [`Event::Up`] when the answer is sent (for
//! a call taken) or received (for a call placed): the media may start then, as RTP can arrive before the ACK.

use crate::msg::{self, Cursor, Message, Start};
use crate::sdp::{self, Direction, Media};
use core::fmt::Write;

pub type Addr = ([u8; 4], u16);

pub const MAX_DATAGRAM: usize = 1400;
const T1_MS: u64 = 500;
const T_MAX_MS: u64 = 4000;
/// How long a 2xx or an INVITE is retransmitted before the call is given up (64 * T1).
const GIVE_UP_MS: u64 = 64 * T1_MS;
/// After a provisional response the other side may take this long to answer.
const RINGING_MS: u64 = 180_000;

/// What the agent needs to know about this machine, per call.
pub struct Env<'a> {
    pub ip: [u8; 4],
    pub sip_port: u16,
    /// The UDP port this machine receives RTP on.
    pub rtp_port: u16,
    pub mac: [u8; 6],
    /// The SDP session name.
    pub name: &'a str,
    /// The user part of our SIP URI.
    pub user: &'a str,
    /// Refuse streams that are not on the PTP grandmaster this machine follows (the AES67 default).
    pub require_ptp: bool,
    /// The PTP grandmaster this machine is locked to, if any.
    pub locked_to: Option<[u8; 8]>,
    pub max_channels: u8,
}

/// What we offer when placing a call.
pub struct Offer {
    /// True: we stream to the other side (`a=sendonly`); false: we listen (`a=recvonly`).
    pub send: bool,
    pub payload_type: u8,
    pub channels: u8,
    pub bits: u8,
    pub rate: u32,
    pub packet_us: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    None,
    /// The call is set up; this is the other side's stream description (its address and port are where our audio goes).
    Up(Media),
    /// The call ended (BYE received, or our BYE answered).
    Down,
    /// A call we placed was refused or had an unusable answer (SIP status code).
    Failed(u16),
}

pub struct Outcome {
    /// Bytes written to the caller's buffer to send to `to`; 0 when there is nothing to send.
    pub reply: usize,
    pub to: Addr,
    pub event: Event,
}

pub enum Poll<'a> {
    Idle,
    /// A retransmission to send.
    Send(&'a [u8], Addr),
    /// Retransmissions ran out: the call is over.
    Down,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Idle,
    /// 200 OK sent, waiting for the ACK.
    Answered,
    Up,
    /// INVITE sent, waiting for the answer.
    Calling,
    /// BYE sent, waiting for its 200.
    Ending,
}

struct Buf<const N: usize> {
    bytes: [u8; N],
    length: usize,
}

impl<const N: usize> Buf<N> {
    const fn new() -> Self {
        Buf { bytes: [0; N], length: 0 }
    }
    fn set(&mut self, text: &str) {
        self.length = 0;
        let _ = self.write_str(text);
    }
    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.bytes[..self.length]).unwrap_or("")
    }
}

impl<const N: usize> Write for Buf<N> {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        let room = N - self.length;
        let take = if text.len() <= room { text.len() } else { (0..=room).rev().find(|&i| text.is_char_boundary(i)).unwrap_or(0) };
        self.bytes[self.length..self.length + take].copy_from_slice(&text.as_bytes()[..take]);
        self.length += take;
        if take == text.len() { Ok(()) } else { Err(core::fmt::Error) }
    }
}

pub struct Ua {
    state: State,
    call_id: Buf<80>,
    /// The From (our side) and To (theirs) of the dialog, as they go into requests we send.
    local_party: Buf<160>,
    remote_party: Buf<160>,
    remote_uri: Buf<120>,
    remote: Addr,
    media: Option<Media>,
    counter: u32,
    /// CSeq of the INVITE of this call (ours or theirs); our later requests count up from it.
    invite_cseq: u32,
    cseq: u32,
    /// The branch of our INVITE (its non-2xx ACK reuses it).
    invite_branch: u32,
    /// The last datagram of the call that may have to be sent again: our 200 OK, INVITE, ACK or BYE.
    held: [u8; MAX_DATAGRAM],
    held_length: usize,
    retransmit: bool,
    next_ms: u64,
    interval_ms: u64,
    deadline_ms: u64,
}

fn contact(env: &Env) -> Buf<80> {
    let mut contact = Buf::new();
    let a = env.ip;
    let _ = write!(contact, "sip:{}@{}.{}.{}.{}:{}", env.user, a[0], a[1], a[2], a[3], env.sip_port);
    contact
}

fn sdp_body(env: &Env, media: &Media, receive_only: bool, session: u32, out: &mut [u8]) -> usize {
    audio_wire::write_sdp(
        &audio_wire::Sdp {
            name: env.name,
            receive_only,
            idle: false,
            origin: env.ip,
            destination: env.ip,
            port: env.rtp_port,
            payload_type: media.payload_type,
            channels: usize::from(media.channels),
            rate: media.rate,
            bits: usize::from(media.bits),
            packet_us: media.packet_us,
            mac: env.mac,
            ptp: env.locked_to,
            session,
        },
        out,
    )
}

impl Ua {
    /// `seed` makes tags, branches and Call-IDs differ from another boot's (the clock will do).
    pub const fn new(seed: u32) -> Self {
        Ua {
            state: State::Idle,
            call_id: Buf::new(),
            local_party: Buf::new(),
            remote_party: Buf::new(),
            remote_uri: Buf::new(),
            remote: ([0; 4], 0),
            media: None,
            counter: seed,
            invite_cseq: 0,
            cseq: 0,
            invite_branch: 0,
            held: [0; MAX_DATAGRAM],
            held_length: 0,
            retransmit: false,
            next_ms: 0,
            interval_ms: T1_MS,
            deadline_ms: 0,
        }
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn media(&self) -> Option<&Media> {
        self.media.as_ref()
    }

    pub fn remote(&self) -> Addr {
        self.remote
    }

    fn next_id(&mut self) -> u32 {
        self.counter = self.counter.wrapping_mul(1664525).wrapping_add(1013904223);
        self.counter
    }

    fn hold(&mut self, datagram: &[u8], now: u64, retransmit: bool) {
        let length = datagram.len().min(MAX_DATAGRAM);
        self.held[..length].copy_from_slice(&datagram[..length]);
        self.held_length = length;
        self.retransmit = retransmit;
        self.interval_ms = T1_MS;
        self.next_ms = now + T1_MS;
        self.deadline_ms = now + GIVE_UP_MS;
    }

    fn reset(&mut self) {
        self.state = State::Idle;
        self.retransmit = false;
        self.held_length = 0;
        self.media = None;
    }

    /// Retransmissions and timeouts; call at least every few hundred milliseconds while a call is not up.
    pub fn poll(&mut self, now: u64) -> Poll<'_> {
        if self.state == State::Idle || !self.retransmit {
            return Poll::Idle;
        }
        if now >= self.deadline_ms {
            self.reset();
            return Poll::Down;
        }
        if now >= self.next_ms {
            self.next_ms = now + self.interval_ms;
            self.interval_ms = (self.interval_ms * 2).min(T_MAX_MS);
            return Poll::Send(&self.held[..self.held_length], self.remote);
        }
        Poll::Idle
    }

    pub fn handle(&mut self, now: u64, src: Addr, packet: &[u8], env: &Env, out: &mut [u8]) -> Outcome {
        let Some(message) = Message::parse(packet) else { return Outcome { reply: 0, to: src, event: Event::None } };
        match message.start {
            Start::Request { method, .. } => self.request(now, src, &message, method, env, out),
            Start::Status { code, .. } => self.status(now, &message, code, env, out),
        }
    }

    fn in_dialog(&self, call_id: &str) -> bool {
        self.state != State::Idle && self.call_id.as_str() == call_id
    }

    /// A response to `request`; the To tag is the dialog's when the request belongs to the current call, else a fresh one.
    fn respond(&mut self, request: &Message, code: u16, warning: Option<&str>, body: &[u8], env: &Env, out: &mut [u8]) -> usize {
        let mut tag = Buf::<24>::new();
        match msg::tag_of(self.local_party.as_str()).filter(|_| self.in_dialog(request.header("call-id").unwrap_or(""))) {
            Some(own) => tag.set(own),
            None => {
                let id = self.next_id();
                let _ = write!(tag, "{id:08x}");
            }
        }
        msg::write_response(out, request, code, tag.as_str(), contact(env).as_str(), warning, body)
    }

    fn request(&mut self, now: u64, src: Addr, request: &Message, method: &str, env: &Env, out: &mut [u8]) -> Outcome {
        let same = self.in_dialog(request.header("call-id").unwrap_or(""));
        let mut outcome = Outcome { reply: 0, to: src, event: Event::None };
        match method {
            "OPTIONS" => outcome.reply = self.respond(request, 200, None, b"", env, out),
            "INVITE" if same => {
                // a retransmission gets our answer again; a re-INVITE is not supported
                if matches!(self.state, State::Answered | State::Up) && request.cseq().map(|c| c.0) == Some(self.invite_cseq) {
                    let length = self.held_length.min(out.len());
                    out[..length].copy_from_slice(&self.held[..length]);
                    outcome.reply = length;
                } else {
                    outcome.reply = self.respond(request, 488, Some("re-INVITE not supported"), b"", env, out);
                }
            }
            "INVITE" if self.state != State::Idle => outcome.reply = self.respond(request, 486, None, b"", env, out),
            "INVITE" => return self.invite(now, src, request, env, out),
            "ACK" if same && self.state == State::Answered => {
                self.state = State::Up;
                self.retransmit = false;
            }
            "ACK" => {}
            "BYE" if same => {
                outcome.reply = self.respond(request, 200, None, b"", env, out);
                self.reset();
                outcome.event = Event::Down;
            }
            "BYE" | "CANCEL" => outcome.reply = self.respond(request, 481, None, b"", env, out),
            _ => outcome.reply = self.respond(request, 405, None, b"", env, out),
        }
        outcome
    }

    fn invite(&mut self, now: u64, src: Addr, request: &Message, env: &Env, out: &mut [u8]) -> Outcome {
        let mut outcome = Outcome { reply: 0, to: src, event: Event::None };
        let to = request.header("to").unwrap_or("");
        let refusal: Option<(u16, &str)> = if msg::tag_of(to).is_some() {
            Some((481, ""))
        } else if request.body.is_empty() {
            Some((488, "an SDP offer is required"))
        } else if request.header("content-type").is_some_and(|t| !t.get(..15).is_some_and(|prefix| prefix.eq_ignore_ascii_case("application/sdp"))) {
            Some((415, ""))
        } else {
            None
        };
        let parsed = match refusal {
            Some(_) => None,
            None => Some(sdp::parse(request.body)),
        };
        let offered = match (refusal, parsed) {
            (Some((code, text)), _) => Err((code, text)),
            (None, Some(Ok(media))) => sdp::check(&media, env.require_ptp, env.locked_to, env.max_channels).map(|_| media).map_err(|r| (488, r.text())),
            (None, Some(Err(error))) => Err((
                488,
                match error {
                    sdp::SdpError::NoAudio => "no audio stream",
                    sdp::SdpError::NoAddress => "no connection address",
                    sdp::SdpError::Unsupported(text) => text,
                },
            )),
            (None, None) => unreachable!(),
        };
        let offered = match offered {
            Ok(media) => media,
            Err((code, text)) => {
                outcome.reply = self.respond(request, code, (!text.is_empty()).then_some(text), b"", env, out);
                return outcome;
            }
        };
        let id = self.next_id();
        let mut local = Buf::<160>::new();
        let _ = write!(local, "{to};tag={id:08x}");
        self.call_id.set(request.header("call-id").unwrap_or(""));
        self.remote_party.set(request.header("from").unwrap_or(""));
        self.local_party.set(local.as_str());
        self.remote_uri.set(msg::uri_of(request.header("contact").unwrap_or_else(|| request.header("from").unwrap_or(""))));
        self.remote = src;
        self.invite_cseq = request.cseq().map_or(0, |c| c.0);
        self.cseq = 0;
        self.state = State::Answered;
        self.media = Some(offered);
        // we receive unless the offer says it only listens
        let mut body = [0u8; 700];
        let length = sdp_body(env, &offered, offered.direction != Direction::RecvOnly, id, &mut body);
        outcome.reply = self.respond(request, 200, None, &body[..length], env, out);
        if outcome.reply == 0 {
            self.reset();
            return outcome;
        }
        self.hold(&out[..outcome.reply], now, true);
        outcome.event = Event::Up(offered);
        outcome
    }

    fn write_request(&self, out: &mut [u8], method: &str, cseq: u32, branch: u32, env: &Env, body: &[u8]) -> usize {
        let a = env.ip;
        let mut cursor = Cursor::new(out);
        let _ = write!(cursor, "{method} {} SIP/2.0\r\nVia: SIP/2.0/UDP {}.{}.{}.{}:{};branch=z9hG4bK{branch:08x}\r\nMax-Forwards: 70\r\n", self.remote_uri.as_str(), a[0], a[1], a[2], a[3], env.sip_port);
        let _ = write!(cursor, "From: {}\r\nTo: {}\r\nCall-ID: {}\r\nCSeq: {cseq} {method}\r\n", self.local_party.as_str(), self.remote_party.as_str(), self.call_id.as_str());
        let _ = write!(cursor, "Contact: <{}>\r\nAllow: {}\r\n", contact(env).as_str(), msg::ALLOW);
        msg::finish(&mut cursor, body)
    }

    /// Places a call to `to`: an INVITE offering one stream. Returns the datagram length in `out` (0 when busy or too small).
    pub fn call(&mut self, now: u64, to: Addr, offer: &Offer, env: &Env, out: &mut [u8]) -> usize {
        if self.state != State::Idle {
            return 0;
        }
        let (a, ip) = (to.0, env.ip);
        let id = self.next_id();
        let mut text = Buf::<80>::new();
        let _ = write!(text, "{id:08x}@{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3]);
        self.call_id.set(text.as_str());
        let tag = self.next_id();
        let mut local = Buf::<160>::new();
        let _ = write!(local, "<sip:{}@{}.{}.{}.{}>;tag={tag:08x}", env.user, ip[0], ip[1], ip[2], ip[3]);
        self.local_party.set(local.as_str());
        let mut uri = Buf::<120>::new();
        let _ = write!(uri, "sip:call@{}.{}.{}.{}", a[0], a[1], a[2], a[3]);
        if to.1 != 5060 {
            let _ = write!(uri, ":{}", to.1);
        }
        self.remote_uri.set(uri.as_str());
        let mut party = Buf::<160>::new();
        let _ = write!(party, "<{}>", uri.as_str());
        self.remote_party.set(party.as_str());
        self.remote = to;
        self.cseq = 0;
        self.invite_cseq = 1;
        self.invite_branch = self.next_id();
        let wanted = Media {
            address: ip,
            port: env.rtp_port,
            payload_type: offer.payload_type,
            bits: offer.bits,
            channels: offer.channels,
            rate: offer.rate,
            packet_us: offer.packet_us,
            clock: sdp::RefClock::None,
            direction: Direction::SendOnly,
        };
        let mut body = [0u8; 700];
        let body_length = sdp_body(env, &wanted, !offer.send, id, &mut body);
        let length = self.write_request(out, "INVITE", 1, self.invite_branch, env, &body[..body_length]);
        if length == 0 {
            self.reset();
            return 0;
        }
        self.state = State::Calling;
        self.hold(&out[..length], now, true);
        length
    }

    /// Ends the call with a BYE. Returns the datagram length (0 when no call is up).
    pub fn hangup(&mut self, now: u64, env: &Env, out: &mut [u8]) -> usize {
        if !matches!(self.state, State::Up | State::Answered) {
            return 0;
        }
        self.cseq += 1;
        let branch = self.next_id();
        let length = self.write_request(out, "BYE", self.invite_cseq + self.cseq, branch, env, b"");
        if length == 0 {
            return 0;
        }
        self.state = State::Ending;
        self.hold(&out[..length], now, true);
        length
    }

    fn status(&mut self, now: u64, response: &Message, code: u16, env: &Env, out: &mut [u8]) -> Outcome {
        let mut outcome = Outcome { reply: 0, to: self.remote, event: Event::None };
        if !self.in_dialog(response.header("call-id").unwrap_or("")) {
            return outcome;
        }
        match (self.state, response.cseq().map_or("", |c| c.1)) {
            (State::Calling, "INVITE") if code < 200 => {
                self.retransmit = false;
                self.deadline_ms = now + RINGING_MS;
            }
            (State::Up, "INVITE") if code < 300 => {
                // our ACK was lost: the 200 came again
                let length = self.held_length.min(out.len());
                out[..length].copy_from_slice(&self.held[..length]);
                outcome.reply = length;
            }
            (State::Calling, "INVITE") if code < 300 => {
                self.remote_party.set(response.header("to").unwrap_or(""));
                if let Some(target) = response.header("contact") {
                    self.remote_uri.set(msg::uri_of(target));
                }
                // the answer has to be on the clock we would require of an offer
                let answer = sdp::parse(response.body).ok().filter(|media| sdp::check(media, env.require_ptp, env.locked_to, env.max_channels).is_ok());
                let branch = self.next_id();
                outcome.reply = self.write_request(out, "ACK", self.invite_cseq, branch, env, b"");
                self.state = State::Up;
                self.hold(&out[..outcome.reply], now, false);
                match answer {
                    Some(media) => {
                        self.media = Some(media);
                        outcome.event = Event::Up(media);
                    }
                    None => {
                        // acknowledged, now hang up what cannot be used; the BYE goes out from `poll`
                        let mut bye = [0u8; MAX_DATAGRAM];
                        let length = self.hangup(now, env, &mut bye);
                        if length > 0 {
                            self.held[..length].copy_from_slice(&bye[..length]);
                            self.held_length = length;
                            self.next_ms = now;
                        }
                        outcome.event = Event::Failed(488);
                    }
                }
            }
            (State::Calling, "INVITE") => {
                // a refusal: its ACK belongs to the INVITE's transaction, so it keeps the INVITE's branch
                self.remote_party.set(response.header("to").unwrap_or(""));
                outcome.reply = self.write_request(out, "ACK", self.invite_cseq, self.invite_branch, env, b"");
                self.reset();
                outcome.event = Event::Failed(code);
            }
            (State::Ending, "BYE") if code >= 200 => {
                self.reset();
                outcome.event = Event::Down;
            }
            _ => {}
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GM: [u8; 8] = [0, 0x1b, 0x21, 0xff, 0xfe, 0, 0, 1];
    const A: Addr = ([10, 0, 0, 1], 5060);
    const B: Addr = ([10, 0, 0, 2], 5060);

    fn env(ip: [u8; 4], require_ptp: bool, locked: bool) -> Env<'static> {
        Env { ip, sip_port: 5060, rtp_port: 5004, mac: [2, 0, 0, 0, 0, ip[3]], name: "fos", user: "fos", require_ptp, locked_to: locked.then_some(GM), max_channels: 8 }
    }

    fn offer(send: bool) -> Offer {
        Offer { send, payload_type: 98, channels: 2, bits: 24, rate: 48000, packet_us: 1000 }
    }

    fn send(ua: &mut Ua, from: Addr, to_env: &Env, datagram: &[u8], now: u64) -> (Vec<u8>, Event) {
        let mut out = [0u8; MAX_DATAGRAM];
        let outcome = ua.handle(now, from, datagram, to_env, &mut out);
        (out[..outcome.reply].to_vec(), outcome.event)
    }

    /// A places a call to B and both end up Up; returns the INVITE exchange's datagrams are consumed.
    fn establish(caller: &mut Ua, callee: &mut Ua, env_a: &Env, env_b: &Env, send_audio: bool) -> (Event, Event) {
        let mut out = [0u8; MAX_DATAGRAM];
        let length = caller.call(0, B, &offer(send_audio), env_a, &mut out);
        assert!(length > 0);
        let (ok, callee_event) = send(callee, A, env_b, &out[..length], 0);
        assert!(!ok.is_empty());
        let (ack, caller_event) = send(caller, B, env_a, &ok, 10);
        if matches!(caller_event, Event::Up(_)) {
            assert!(!ack.is_empty());
            send(callee, A, env_b, &ack, 20);
        }
        (caller_event, callee_event)
    }

    #[test]
    fn call_answer_ack_bye() {
        let (env_a, env_b) = (env(A.0, true, true), env(B.0, true, true));
        let (mut caller, mut callee) = (Ua::new(1), Ua::new(2));
        let (caller_event, callee_event) = establish(&mut caller, &mut callee, &env_a, &env_b, true);
        let Event::Up(heard) = callee_event else { panic!("callee: {callee_event:?}") };
        assert_eq!((heard.address, heard.port, heard.direction), (A.0, 5004, Direction::SendOnly));
        let Event::Up(answered) = caller_event else { panic!("caller: {caller_event:?}") };
        assert_eq!((answered.address, answered.direction), (B.0, Direction::RecvOnly));
        assert_eq!((caller.state(), callee.state()), (State::Up, State::Up));
        assert!(matches!(callee.poll(1_000_000), Poll::Idle), "no retransmission after the ACK");

        let mut out = [0u8; MAX_DATAGRAM];
        let length = caller.hangup(100, &env_a, &mut out);
        assert!(length > 0);
        let (ok, event) = send(&mut callee, A, &env_b, &out[..length], 100);
        assert_eq!(event, Event::Down);
        assert_eq!(callee.state(), State::Idle);
        let (_, event) = send(&mut caller, B, &env_a, &ok, 110);
        assert_eq!(event, Event::Down);
        assert_eq!(caller.state(), State::Idle);
    }

    #[test]
    fn the_callee_can_hang_up_too() {
        let (env_a, env_b) = (env(A.0, false, false), env(B.0, false, false));
        let (mut caller, mut callee) = (Ua::new(3), Ua::new(4));
        establish(&mut caller, &mut callee, &env_a, &env_b, false);
        let mut out = [0u8; MAX_DATAGRAM];
        let length = callee.hangup(50, &env_b, &mut out);
        let (ok, event) = send(&mut caller, B, &env_a, &out[..length], 50);
        assert_eq!(event, Event::Down);
        assert_eq!(send(&mut callee, A, &env_b, &ok, 60).1, Event::Down);
    }

    #[test]
    fn a_second_call_is_busy_and_a_stray_bye_is_481() {
        let (env_a, env_b) = (env(A.0, false, false), env(B.0, false, false));
        let (mut caller, mut callee) = (Ua::new(5), Ua::new(6));
        establish(&mut caller, &mut callee, &env_a, &env_b, true);
        let mut other = Ua::new(7);
        let mut out = [0u8; MAX_DATAGRAM];
        let length = other.call(0, B, &offer(true), &env(A.0, false, false), &mut out);
        let (busy, _) = send(&mut callee, ([10, 0, 0, 3], 5060), &env_b, &out[..length], 5);
        assert!(String::from_utf8_lossy(&busy).starts_with("SIP/2.0 486"));
        let (_, event) = send(&mut other, B, &env_a, &busy, 6);
        assert_eq!(event, Event::Failed(486));
        assert_eq!(other.state(), State::Idle);
    }

    #[test]
    fn ptp_is_required_by_default_and_can_be_waived() {
        // the caller follows a grandmaster and says so; the callee follows none
        let env_a = env(A.0, true, true);
        let mut caller = Ua::new(8);
        let mut out = [0u8; MAX_DATAGRAM];
        let length = caller.call(0, B, &offer(true), &env_a, &mut out);
        let invite = out[..length].to_vec();
        let mut strict = Ua::new(9);
        let (refusal, event) = send(&mut strict, A, &env(B.0, true, false), &invite, 0);
        let text = String::from_utf8_lossy(&refusal).into_owned();
        assert!(text.starts_with("SIP/2.0 488") && text.contains("not locked to PTP"), "{text}");
        assert_eq!((event, strict.state()), (Event::None, State::Idle));
        let (_, event) = send(&mut caller, B, &env_a, &refusal, 1);
        assert_eq!(event, Event::Failed(488));

        let mut relaxed = Ua::new(10);
        let (ok, event) = send(&mut relaxed, A, &env(B.0, false, false), &invite, 0);
        assert!(matches!(event, Event::Up(_)) && String::from_utf8_lossy(&ok).starts_with("SIP/2.0 200"));
        // the answer of an unlocked machine says localmac, so a caller that requires PTP will not take it
        let mut caller = Ua::new(11);
        let length = caller.call(0, B, &offer(true), &env_a, &mut out);
        let (ok, _) = send(&mut Ua::new(12), A, &env(B.0, false, false), &out[..length], 0);
        let (_, event) = send(&mut caller, B, &env_a, &ok, 1);
        assert_eq!(event, Event::Failed(488));
        assert_eq!(caller.state(), State::Ending, "an unusable answer is acknowledged and hung up");
        assert!(matches!(caller.poll(2), Poll::Send(bye, _) if bye.starts_with(b"BYE ")));
    }

    #[test]
    fn the_200_is_retransmitted_until_the_ack_and_then_gives_up() {
        let (env_a, env_b) = (env(A.0, false, false), env(B.0, false, false));
        let (mut caller, mut callee) = (Ua::new(13), Ua::new(14));
        let mut out = [0u8; MAX_DATAGRAM];
        let length = caller.call(0, B, &offer(true), &env_a, &mut out);
        let invite = out[..length].to_vec();
        let (ok, _) = send(&mut callee, A, &env_b, &invite, 0);
        assert!(matches!(callee.poll(100), Poll::Idle));
        assert!(matches!(callee.poll(500), Poll::Send(bytes, addr) if bytes == ok.as_slice() && addr == A));
        // the INVITE came again: the same 200
        assert_eq!(send(&mut callee, A, &env_b, &invite, 600).0, ok);
        assert!(matches!(callee.poll(40_000), Poll::Down));
        assert_eq!(callee.state(), State::Idle);
    }

    #[test]
    fn options_unknown_methods_and_garbage() {
        let env_b = env(B.0, false, false);
        let mut ua = Ua::new(15);
        let options = b"OPTIONS sip:fos@10.0.0.2 SIP/2.0\r\nVia: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK1\r\nFrom: <sip:x@10.0.0.9>;tag=1\r\nTo: <sip:fos@10.0.0.2>\r\nCall-ID: o1\r\nCSeq: 1 OPTIONS\r\n\r\n";
        let (reply, _) = send(&mut ua, A, &env_b, options, 0);
        assert!(String::from_utf8_lossy(&reply).starts_with("SIP/2.0 200") && String::from_utf8_lossy(&reply).contains("Allow: INVITE"));
        let message = String::from_utf8_lossy(options).replace("OPTIONS sip", "SUBSCRIBE sip").replace("1 OPTIONS", "1 SUBSCRIBE");
        assert!(String::from_utf8_lossy(&send(&mut ua, A, &env_b, message.as_bytes(), 0).0).starts_with("SIP/2.0 405"));
        assert!(send(&mut ua, A, &env_b, b"hello", 0).0.is_empty());
        assert_eq!(ua.state(), State::Idle);
    }
}
