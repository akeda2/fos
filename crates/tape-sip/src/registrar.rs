//! A SIP registrar and stateless proxy (RFC 3261 sections 10 and 16.11) for a trusted segment, UDP only.
//!
//! **Registrar.** REGISTER is answered with a digest challenge ([`auth`]); a request that proves the password in the users
//! file binds the `To` user to the address the request *came from* (so a phone behind NAT is reached on the hole it opened)
//! and to the Contact it advertised, for the `expires` it asked (at most one hour). `expires=0` or `Contact: *` removes.
//!
//! **Proxy.** Requests from an address that has a live registration are forwarded: a request URI on this server's address is
//! looked up by user; one that names an advertised contact goes to the registered address behind it; anything else goes to
//! the address in the URI. INVITE gets a `Record-Route` with `lr`, so ACK, BYE and every later in-dialog request come back
//! through here and the proxy stays in the signalling path (what keeps a NAT binding alive). Responses follow the Via
//! chain; the proxy's own Via is removed and the next is used with its `received`/`rport` (RFC 3581). The proxy keeps no
//! transaction state: the branch it adds is a hash of the incoming one, so retransmissions leave identically.
//!
//! What it does not do: media. SDP and RTP pass untouched, so audio flows peer to peer and a NAT in the path needs its
//! own help for that (a symmetric-RTP relay is not here). No TCP/TLS, no forking, no hostnames in URIs (IPv4
//! addresses only), no multiple Route values in one header line.
//!
//! The caller moves datagrams and owns the clock, as in [`crate::ua`].

use crate::auth::{self, Credentials, NonceCheck};
use crate::msg::{self, Cursor, Message, Start};
use crate::sdp::parse_ipv4;
use core::fmt::Write;

pub type Addr = ([u8; 4], u16);

pub const MAX_BINDINGS: usize = 16;
pub const MAX_EXPIRES: u32 = 3600;
const TAG: &str = "fosreg";

pub struct Config<'a> {
    /// This server's address and SIP port (what it puts in Via and Record-Route).
    pub ip: [u8; 4],
    pub port: u16,
    pub realm: &'a str,
    /// Lines of `user:password`.
    pub users: &'a str,
    /// Mixed into nonces; any value that is not guessable.
    pub secret: u32,
}

#[derive(Clone, Copy)]
struct Binding {
    used: bool,
    user: [u8; 32],
    user_len: usize,
    advertised: Addr,
    observed: Addr,
    until_ms: u64,
}

const EMPTY: Binding = Binding { used: false, user: [0; 32], user_len: 0, advertised: ([0; 4], 0), observed: ([0; 4], 0), until_ms: 0 };

impl Binding {
    fn user(&self) -> &str {
        core::str::from_utf8(&self.user[..self.user_len]).unwrap_or("")
    }
    fn live(&self, now_ms: u64) -> bool {
        self.used && now_ms < self.until_ms
    }
}

pub struct Reply {
    pub length: usize,
    pub to: Addr,
}

pub struct Server {
    bindings: [Binding; MAX_BINDINGS],
    pub registered: u32,
    pub challenged: u32,
    pub refused: u32,
    pub forwarded: u32,
}

impl Server {
    pub const fn new() -> Self {
        Server { bindings: [EMPTY; MAX_BINDINGS], registered: 0, challenged: 0, refused: 0, forwarded: 0 }
    }

    /// Where `user` is reachable now.
    pub fn lookup(&self, user: &str, now_ms: u64) -> Option<Addr> {
        self.bindings.iter().find(|b| b.live(now_ms) && b.user() == user).map(|b| b.observed)
    }

    pub fn count(&self, now_ms: u64) -> usize {
        self.bindings.iter().filter(|b| b.live(now_ms)).count()
    }

    /// The registered user and address of binding `index` among the live ones (for a status listing).
    pub fn nth(&self, index: usize, now_ms: u64) -> Option<(&str, Addr, u64)> {
        self.bindings.iter().filter(|b| b.live(now_ms)).nth(index).map(|b| (b.user(), b.observed, (b.until_ms - now_ms) / 1000))
    }

    fn is_known(&self, ip: [u8; 4], now_ms: u64) -> bool {
        self.bindings.iter().any(|b| b.live(now_ms) && b.observed.0 == ip)
    }

    fn by_advertised(&self, addr: Addr, now_ms: u64) -> Option<Addr> {
        self.bindings.iter().find(|b| b.live(now_ms) && b.advertised == addr).map(|b| b.observed)
    }

    /// One received datagram; the reply or forwarded message to send, if any.
    pub fn handle(&mut self, now_ms: u64, src: Addr, packet: &[u8], config: &Config, out: &mut [u8]) -> Option<Reply> {
        let message = Message::parse(packet)?;
        match message.start {
            Start::Status { code, reason } => self.response(&message, code, reason, config, out),
            Start::Request { method: "REGISTER", .. } => self.register(now_ms, src, &message, config, out),
            Start::Request { method, uri } => self.proxy(now_ms, src, &message, method, uri, config, out),
        }
    }

    fn reject(&mut self, src: Addr, request: &Message, code: u16, out: &mut [u8]) -> Option<Reply> {
        self.refused += 1;
        let mut cursor = Cursor::new(out);
        head(&mut cursor, request, code);
        reply(&mut cursor, src)
    }

    fn register(&mut self, now_ms: u64, src: Addr, request: &Message, config: &Config, out: &mut [u8]) -> Option<Reply> {
        let to = request.header("to")?;
        let (user, _) = split_uri(msg::uri_of(to)).filter(|(user, _)| !user.is_empty())?;
        let now_s = (now_ms / 1000) as u32;
        let stale = match request.header("authorization").and_then(Credentials::parse) {
            None => false,
            Some(credentials) => match auth::check_nonce(config.secret, now_s, src.0, credentials.nonce) {
                NonceCheck::Good => {
                    let password = auth::password_of(config.users, user);
                    let wanted = password.map(|password| auth::response(user, config.realm, password, "REGISTER", credentials.uri, credentials.nonce, credentials.qop, credentials.nc, credentials.cnonce));
                    let right = credentials.username == user && credentials.realm == config.realm && wanted.is_some_and(|wanted| same(&wanted, credentials.response.as_bytes()));
                    if !right {
                        return self.reject(src, request, 403, out);
                    }
                    return self.bind(now_ms, src, request, user, out);
                }
                NonceCheck::Stale => true,
                NonceCheck::Bad => false,
            },
        };
        self.challenged += 1;
        let nonce = auth::make_nonce(config.secret, now_s, src.0);
        let mut cursor = Cursor::new(out);
        head(&mut cursor, request, 401);
        let _ = write!(cursor, "WWW-Authenticate: Digest realm=\"{}\", nonce=\"{}\", algorithm=MD5, qop=\"auth\"{}\r\n", config.realm, core::str::from_utf8(&nonce).unwrap_or(""), if stale { ", stale=true" } else { "" });
        reply(&mut cursor, src)
    }

    fn bind(&mut self, now_ms: u64, src: Addr, request: &Message, user: &str, out: &mut [u8]) -> Option<Reply> {
        let default_expires = request.header("expires").and_then(|v| v.parse::<u32>().ok()).unwrap_or(MAX_EXPIRES);
        for contact in request.headers("contact") {
            if contact.trim() == "*" {
                if default_expires == 0 {
                    for b in self.bindings.iter_mut().filter(|b| b.used && b.user() == user) {
                        b.used = false;
                    }
                }
                continue;
            }
            let uri = msg::uri_of(contact);
            let Some(advertised) = split_uri(uri).and_then(|(_, host)| host_port(host)) else { return self.reject(src, request, 400, out) };
            let expires = contact_expires(contact).unwrap_or(default_expires).min(MAX_EXPIRES);
            let existing = self.bindings.iter().position(|b| b.used && b.user() == user && b.advertised == advertised);
            if expires == 0 {
                if let Some(index) = existing {
                    self.bindings[index].used = false;
                }
                continue;
            }
            let Some(slot) = existing.or_else(|| self.bindings.iter().position(|b| !b.live(now_ms))) else { return self.reject(src, request, 503, out) };
            let mut binding = EMPTY;
            binding.used = true;
            binding.user_len = user.len().min(32);
            binding.user[..binding.user_len].copy_from_slice(&user.as_bytes()[..binding.user_len]);
            binding.advertised = advertised;
            binding.observed = src;
            binding.until_ms = now_ms + u64::from(expires) * 1000;
            self.bindings[slot] = binding;
            self.registered += 1;
        }
        let mut cursor = Cursor::new(out);
        head(&mut cursor, request, 200);
        for binding in self.bindings.iter().filter(|b| b.live(now_ms) && b.user() == user) {
            let a = binding.advertised;
            let _ = write!(cursor, "Contact: <sip:{user}@{}.{}.{}.{}:{}>;expires={}\r\n", a.0[0], a.0[1], a.0[2], a.0[3], a.1, (binding.until_ms - now_ms) / 1000);
        }
        reply(&mut cursor, src)
    }

    #[allow(clippy::too_many_arguments)]
    fn proxy(&mut self, now_ms: u64, src: Addr, request: &Message, method: &str, uri: &str, config: &Config, out: &mut [u8]) -> Option<Reply> {
        let Some((user, host)) = split_uri(uri) else { return self.reject(src, request, 400, out) };
        let target = host_port(host);
        let ours = target.is_some_and(|t| t.0 == config.ip && (t.1 == config.port || host == format_ip(t.0).as_str()));
        if method == "OPTIONS" && ours && user.is_empty() {
            let mut cursor = Cursor::new(out);
            head(&mut cursor, request, 200);
            let _ = write!(cursor, "Allow: {}, REGISTER\r\n", msg::ALLOW);
            return reply(&mut cursor, src);
        }
        if !self.is_known(src.0, now_ms) {
            if method == "ACK" {
                return None;
            }
            return self.reject(src, request, 403, out);
        }
        let forwards = request.header("max-forwards").and_then(|v| v.trim().parse::<u32>().ok()).unwrap_or(70);
        if forwards == 0 && method != "ACK" {
            return self.reject(src, request, 483, out);
        }
        // Our own Route (the one the Record-Route we added made the client send) is consumed; a Route after it is the next hop.
        let mut routes = request.headers("route");
        let first = routes.next();
        let own_route = first.and_then(|r| split_uri(msg::uri_of(r))).and_then(|(_, h)| host_port(h)) == Some((config.ip, config.port));
        let next_route = if own_route { routes.next() } else { first };
        let route_target = next_route.and_then(|r| split_uri(msg::uri_of(r))).and_then(|(_, h)| host_port(h));
        let mut new_uri = [0u8; 80];
        let mut new_uri_len = 0;
        let next: Addr = if let Some(hop) = route_target {
            self.by_advertised(hop, now_ms).unwrap_or(hop)
        } else if ours {
            let Some(to) = self.lookup(user, now_ms) else { return self.reject(src, request, 404, out) };
            let mut cursor = Cursor::new(&mut new_uri);
            let _ = write!(cursor, "sip:{user}@{}.{}.{}.{}:{}", to.0[0], to.0[1], to.0[2], to.0[3], to.1);
            new_uri_len = cursor.length;
            to
        } else {
            let Some(target) = target else { return self.reject(src, request, 404, out) };
            self.by_advertised(target, now_ms).unwrap_or(target)
        };
        let request_uri = if new_uri_len != 0 { core::str::from_utf8(&new_uri[..new_uri_len]).unwrap_or(uri) } else { uri };
        let top = request.header("via")?;
        let seed = auth_free_hash(top, request.header("call-id").unwrap_or(""), request.header("cseq").and_then(|c| c.split(' ').next()).unwrap_or(""));
        let mut cursor = Cursor::new(out);
        let _ = write!(cursor, "{method} {request_uri} SIP/2.0\r\nVia: SIP/2.0/UDP {}.{}.{}.{}:{};branch=z9hG4bK{}\r\n", config.ip[0], config.ip[1], config.ip[2], config.ip[3], config.port, core::str::from_utf8(&seed).unwrap_or(""));
        if method == "INVITE" {
            let _ = write!(cursor, "Record-Route: <sip:{}.{}.{}.{}:{};lr>\r\n", config.ip[0], config.ip[1], config.ip[2], config.ip[3], config.port);
        }
        let (mut first_via, mut first_route) = (true, own_route);
        for line in request.lines() {
            let Some((key, value)) = line.split_once(':') else { continue };
            let key = key.trim();
            if key.eq_ignore_ascii_case("max-forwards") {
                continue;
            }
            if key.eq_ignore_ascii_case("route") && first_route {
                first_route = false;
                continue;
            }
            if key.eq_ignore_ascii_case("via") || key.eq_ignore_ascii_case("v") {
                if first_via {
                    first_via = false;
                    let _ = write!(cursor, "Via: ");
                    annotate(&mut cursor, value.trim(), src);
                    let _ = write!(cursor, "\r\n");
                    continue;
                }
            }
            let _ = write!(cursor, "{line}\r\n");
        }
        let _ = write!(cursor, "Max-Forwards: {}\r\n\r\n", forwards.saturating_sub(1).min(70));
        let length = append_body(&mut cursor, request.body.as_bytes())?;
        self.forwarded += 1;
        Some(Reply { length, to: next })
    }

    /// A response: our Via comes off, the next one says where it goes.
    fn response(&mut self, message: &Message, code: u16, reason: &str, config: &Config, out: &mut [u8]) -> Option<Reply> {
        let mut vias = message.headers("via");
        let top = vias.next()?;
        let sent_by = top.strip_prefix("SIP/2.0/UDP")?.trim_start().split(';').next()?.trim();
        if host_port(sent_by)? != (config.ip, config.port) {
            return None;
        }
        let next = vias.next()?;
        let mut parts = next.strip_prefix("SIP/2.0/UDP")?.trim_start().split(';');
        let sent = host_port(parts.next()?.trim())?;
        let (mut received, mut rport) = (None, None);
        for parameter in parts {
            let parameter = parameter.trim();
            if let Some(value) = parameter.strip_prefix("received=") {
                received = parse_ipv4(value);
            } else if let Some(value) = parameter.strip_prefix("rport=") {
                rport = value.parse().ok();
            }
        }
        let to = (received.unwrap_or(sent.0), rport.unwrap_or(sent.1));
        let mut cursor = Cursor::new(out);
        let _ = write!(cursor, "SIP/2.0 {code} {reason}\r\n");
        let mut dropped = false;
        for line in message.lines() {
            let key = line.split_once(':').map_or("", |(k, _)| k.trim());
            if !dropped && (key.eq_ignore_ascii_case("via") || key.eq_ignore_ascii_case("v")) {
                dropped = true;
                continue;
            }
            let _ = write!(cursor, "{line}\r\n");
        }
        let _ = write!(cursor, "\r\n");
        let length = append_body(&mut cursor, message.body.as_bytes())?;
        self.forwarded += 1;
        Some(Reply { length, to })
    }
}

impl Default for Server {
    fn default() -> Self {
        Self::new()
    }
}

/// Status line, the headers a response must echo, and the To tag.
fn head(cursor: &mut Cursor, request: &Message, code: u16) {
    let _ = write!(cursor, "SIP/2.0 {code} {}\r\n", msg::reason(code));
    for via in request.headers("via") {
        let _ = write!(cursor, "Via: {via}\r\n");
    }
    let _ = write!(cursor, "From: {}\r\n", request.header("from").unwrap_or(""));
    let to = request.header("to").unwrap_or("");
    if msg::tag_of(to).is_some() {
        let _ = write!(cursor, "To: {to}\r\n");
    } else {
        let _ = write!(cursor, "To: {to};tag={TAG}\r\n");
    }
    let _ = write!(cursor, "Call-ID: {}\r\nCSeq: {} {}\r\n", request.header("call-id").unwrap_or(""), request.cseq().map_or(0, |c| c.0), request.cseq().map_or("", |c| c.1));
}

/// Ends a response built with [`head`]. A response goes back to the address the request came from.
fn reply(cursor: &mut Cursor, to: Addr) -> Option<Reply> {
    let length = msg::finish(cursor, b"");
    (length != 0).then_some(Reply { length, to })
}

fn append_body(cursor: &mut Cursor, body: &[u8]) -> Option<usize> {
    let end = cursor.length + body.len();
    if cursor.overflow || end > cursor.bytes.len() {
        return None;
    }
    cursor.bytes[cursor.length..end].copy_from_slice(body);
    Some(end)
}

/// The Via value with `received` (and the answer to `rport`) added, RFC 3581.
fn annotate(cursor: &mut Cursor, value: &str, src: Addr) {
    let mut parts = value.split(';');
    let _ = write!(cursor, "{}", parts.next().unwrap_or(""));
    for part in parts {
        let part = part.trim();
        if part == "rport" {
            let _ = write!(cursor, ";rport={}", src.1);
        } else if !part.starts_with("received=") {
            let _ = write!(cursor, ";{part}");
        }
    }
    let _ = write!(cursor, ";received={}.{}.{}.{}", src.0[0], src.0[1], src.0[2], src.0[3]);
}

/// 16 hex characters from the incoming branch, Call-ID and CSeq number: the same request always gets the same branch.
fn auth_free_hash(top_via: &str, call_id: &str, cseq: &str) -> [u8; 16] {
    let branch = top_via.split(';').find_map(|p| p.trim().strip_prefix("branch=")).unwrap_or("");
    let digest = crate::md5::hex(&crate::md5::md5(&[branch.as_bytes(), b"|", call_id.as_bytes(), b"|", cseq.as_bytes()]));
    digest[..16].try_into().unwrap()
}

/// `(user, host[:port])` of `sip:user@host:port;params` (the user is empty without an `@`).
fn split_uri(uri: &str) -> Option<(&str, &str)> {
    let rest = uri.strip_prefix("sip:")?;
    let rest = rest.split([';', '?']).next()?;
    Some(rest.split_once('@').unwrap_or(("", rest)))
}

fn host_port(text: &str) -> Option<Addr> {
    match text.split_once(':') {
        Some((host, port)) => Some((parse_ipv4(host)?, port.parse().ok()?)),
        None => Some((parse_ipv4(text)?, 5060)),
    }
}

fn format_ip(ip: [u8; 4]) -> FixedText {
    let mut text = FixedText { bytes: [0; 15], length: 0 };
    let _ = write!(text, "{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3]);
    text
}

struct FixedText {
    bytes: [u8; 15],
    length: usize,
}

impl FixedText {
    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.bytes[..self.length]).unwrap_or("")
    }
}

impl Write for FixedText {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        let end = self.length + text.len();
        if end > 15 {
            return Err(core::fmt::Error);
        }
        self.bytes[self.length..end].copy_from_slice(text.as_bytes());
        self.length = end;
        Ok(())
    }
}

/// `expires=N` on a Contact value (after the URI).
fn contact_expires(contact: &str) -> Option<u32> {
    let after = contact.rfind('>').map_or(contact, |end| &contact[end..]);
    after.split(';').skip(1).find_map(|p| p.trim().strip_prefix("expires=")?.parse().ok())
}

fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::string::String;
    use std::format;

    const CONFIG: Config = Config { ip: [10, 0, 0, 1], port: 5060, realm: "fos", users: "alice:wonder\nbob:builder\n", secret: 99 };
    const ALICE: Addr = ([10, 0, 0, 11], 5062);
    const BOB: Addr = ([10, 0, 0, 12], 5070);

    fn run(server: &mut Server, now: u64, src: Addr, text: &str) -> Option<(String, Addr)> {
        let mut out = [0u8; 1500];
        server.handle(now, src, text.as_bytes(), &CONFIG, &mut out).map(|r| (String::from_utf8(out[..r.length].to_vec()).unwrap(), r.to))
    }

    fn register_text(user: &str, contact: &str, authorization: &str, expires: u32) -> String {
        format!("REGISTER sip:10.0.0.1 SIP/2.0\r\nVia: SIP/2.0/UDP {contact};branch=z9hG4bKr1;rport\r\nFrom: <sip:{user}@10.0.0.1>;tag=a\r\nTo: <sip:{user}@10.0.0.1>\r\nCall-ID: reg-{user}\r\nCSeq: 1 REGISTER\r\nContact: <sip:{user}@{contact}>;expires={expires}\r\n{authorization}Content-Length: 0\r\n\r\n")
    }

    /// Registers `user` (password `password`) from `src` through the whole challenge exchange.
    fn register(server: &mut Server, now: u64, src: Addr, user: &str, password: &str, expires: u32) -> String {
        let contact = format!("{}.{}.{}.{}:{}", src.0[0], src.0[1], src.0[2], src.0[3], src.1);
        let (challenge, _) = run(server, now, src, &register_text(user, &contact, "", expires)).unwrap();
        assert!(challenge.starts_with("SIP/2.0 401"), "{challenge}");
        let nonce = challenge.split("nonce=\"").nth(1).unwrap().split('"').next().unwrap();
        let response = String::from_utf8(auth::response(user, "fos", password, "REGISTER", "sip:10.0.0.1", nonce, "auth", "00000001", "cn").to_vec()).unwrap();
        let header = format!("Authorization: Digest username=\"{user}\", realm=\"fos\", nonce=\"{nonce}\", uri=\"sip:10.0.0.1\", response=\"{response}\", qop=auth, nc=00000001, cnonce=\"cn\"\r\n");
        run(server, now + 10, src, &register_text(user, &contact, &header, expires)).unwrap().0
    }

    #[test]
    fn register_needs_the_password() {
        let mut server = Server::new();
        let ok = register(&mut server, 1000, ALICE, "alice", "wonder", 600);
        assert!(ok.starts_with("SIP/2.0 200"), "{ok}");
        assert!(ok.contains("Contact: <sip:alice@10.0.0.11:5062>;expires=600"), "{ok}");
        assert_eq!(server.lookup("alice", 2000), Some(ALICE));
        let bad = register(&mut server, 1000, BOB, "bob", "wrong", 600);
        assert!(bad.starts_with("SIP/2.0 403"), "{bad}");
        assert_eq!(server.lookup("bob", 2000), None);
    }

    #[test]
    fn registration_expires_and_can_be_removed() {
        let mut server = Server::new();
        register(&mut server, 0, ALICE, "alice", "wonder", 30);
        assert!(server.lookup("alice", 29_000).is_some());
        assert!(server.lookup("alice", 31_000).is_none());
        register(&mut server, 40_000, ALICE, "alice", "wonder", 600);
        register(&mut server, 41_000, ALICE, "alice", "wonder", 0);
        assert!(server.lookup("alice", 42_000).is_none());
    }

    #[test]
    fn a_stale_nonce_is_challenged_again() {
        let mut server = Server::new();
        let contact = "10.0.0.11:5062";
        let (challenge, _) = run(&mut server, 0, ALICE, &register_text("alice", contact, "", 60)).unwrap();
        let nonce = challenge.split("nonce=\"").nth(1).unwrap().split('"').next().unwrap();
        let header = format!("Authorization: Digest username=\"alice\", realm=\"fos\", nonce=\"{nonce}\", uri=\"sip:10.0.0.1\", response=\"00\"\r\n");
        let (again, _) = run(&mut server, 500_000, ALICE, &register_text("alice", contact, &header, 60)).unwrap();
        assert!(again.starts_with("SIP/2.0 401") && again.contains("stale=true"), "{again}");
    }

    fn both(server: &mut Server) {
        register(server, 0, ALICE, "alice", "wonder", 600);
        register(server, 0, BOB, "bob", "builder", 600);
    }

    const INVITE: &str = "INVITE sip:bob@10.0.0.1 SIP/2.0\r\nVia: SIP/2.0/UDP 192.168.1.5:5062;branch=z9hG4bKinv;rport\r\nMax-Forwards: 70\r\nFrom: <sip:alice@10.0.0.1>;tag=a\r\nTo: <sip:bob@10.0.0.1>\r\nCall-ID: c1\r\nCSeq: 1 INVITE\r\nContact: <sip:alice@10.0.0.11:5062>\r\nContent-Type: application/sdp\r\nContent-Length: 4\r\n\r\nv=0\n";

    #[test]
    fn invite_goes_to_the_registered_address_with_via_and_record_route() {
        let mut server = Server::new();
        both(&mut server);
        let (out, to) = run(&mut server, 5000, ALICE, INVITE).unwrap();
        assert_eq!(to, BOB);
        assert!(out.starts_with("INVITE sip:bob@10.0.0.12:5070 SIP/2.0\r\nVia: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK"), "{out}");
        assert!(out.contains("Record-Route: <sip:10.0.0.1:5060;lr>"));
        assert!(out.contains("branch=z9hG4bKinv;rport=5062;received=10.0.0.11"), "{out}");
        assert!(out.contains("Max-Forwards: 69") && out.ends_with("\r\n\r\nv=0\n"), "{out}");
        // The same request again leaves with the same branch (stateless).
        let (again, _) = run(&mut server, 5100, ALICE, INVITE).unwrap();
        assert_eq!(out, again);
    }

    #[test]
    fn responses_follow_the_via_chain_back() {
        let mut server = Server::new();
        both(&mut server);
        let (out, _) = run(&mut server, 5000, ALICE, INVITE).unwrap();
        let branch = out.split("branch=z9hG4bK").nth(1).unwrap().split("\r\n").next().unwrap();
        let ringing = format!("SIP/2.0 180 Ringing\r\nVia: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK{branch}\r\nVia: SIP/2.0/UDP 192.168.1.5:5062;branch=z9hG4bKinv;rport=5062;received=10.0.0.11\r\nFrom: <sip:alice@10.0.0.1>;tag=a\r\nTo: <sip:bob@10.0.0.1>;tag=b\r\nCall-ID: c1\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n");
        let (back, to) = run(&mut server, 5200, BOB, &ringing).unwrap();
        assert_eq!(to, ALICE, "goes to received/rport, not the private address in the Via");
        assert!(back.starts_with("SIP/2.0 180 Ringing\r\nVia: SIP/2.0/UDP 192.168.1.5:5062;"), "{back}");
        assert!(!back.contains("10.0.0.1:5060"));
        // A response whose top Via is not ours is dropped.
        assert!(run(&mut server, 5300, BOB, &back).is_none());
    }

    #[test]
    fn in_dialog_requests_follow_the_route_to_the_registered_contact() {
        let mut server = Server::new();
        both(&mut server);
        // Bob's BYE: Route is us, the request URI is Alice's advertised contact (a private address in real life).
        let bye = "BYE sip:alice@10.0.0.11:5062 SIP/2.0\r\nVia: SIP/2.0/UDP 10.0.0.12:5070;branch=z9hG4bKbye\r\nRoute: <sip:10.0.0.1:5060;lr>\r\nFrom: <sip:bob@10.0.0.1>;tag=b\r\nTo: <sip:alice@10.0.0.1>;tag=a\r\nCall-ID: c1\r\nCSeq: 1 BYE\r\nContent-Length: 0\r\n\r\n";
        let (out, to) = run(&mut server, 6000, BOB, bye).unwrap();
        assert_eq!(to, ALICE);
        assert!(out.starts_with("BYE sip:alice@10.0.0.11:5062 SIP/2.0\r\n") && !out.contains("Route:"), "{out}");
    }

    #[test]
    fn strangers_cannot_use_the_proxy_and_unknown_users_are_404() {
        let mut server = Server::new();
        both(&mut server);
        let (out, to) = run(&mut server, 5000, ([10, 0, 0, 99], 5060), INVITE).unwrap();
        assert!(out.starts_with("SIP/2.0 403") && to.0 == [10, 0, 0, 99]);
        let nobody = INVITE.replace("sip:bob@10.0.0.1 SIP", "sip:carol@10.0.0.1 SIP");
        assert!(run(&mut server, 5000, ALICE, &nobody).unwrap().0.starts_with("SIP/2.0 404"));
        let (loops, _) = run(&mut server, 5000, ALICE, &INVITE.replace("Max-Forwards: 70", "Max-Forwards: 0")).unwrap();
        assert!(loops.starts_with("SIP/2.0 483"));
    }

    #[test]
    fn options_to_the_server_is_answered_without_registration() {
        let mut server = Server::new();
        let options = "OPTIONS sip:10.0.0.1 SIP/2.0\r\nVia: SIP/2.0/UDP 10.0.0.50:5060;branch=z9hG4bKo\r\nFrom: <sip:x@10.0.0.50>;tag=1\r\nTo: <sip:10.0.0.1>\r\nCall-ID: o1\r\nCSeq: 1 OPTIONS\r\nContent-Length: 0\r\n\r\n";
        let (out, _) = run(&mut server, 0, ([10, 0, 0, 50], 5060), options).unwrap();
        assert!(out.starts_with("SIP/2.0 200") && out.contains("REGISTER"), "{out}");
    }
}
