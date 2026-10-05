//! `audio-net serve`: the Opus stream as Ogg Opus over HTTP, for trying a machine from a browser or VLC (`vlc
//! http://IP:8000/live.opus`) when the RTP receiver is out of reach, such as a phone. It is a test aid, not a
//! distribution server: a few clients at once and no authentication.
//!
//! The machine answers ARP and ping, accepts up to three TCP connections on `http=PORT` (default 8000) and serves
//! `GET /live.opus` as an endless `audio/ogg` response (one Opus packet per Ogg page, see `audio_wire::ogg`);
//! `GET /` is a page with an `<audio>` element pointing at it. Several connections matter: a phone's VLC opens a second
//! one while the first plays, and a server that dropped the first would silence it. Each client has its own send queue
//! and its own Ogg stream (serial, page numbers, granule position), so one that joins late starts cleanly.
//!
//! The TCP layer is `tape-net`'s, used as a stream: up to four segments in flight (and no more than the peer's window),
//! cumulative acknowledgements, the oldest segment retransmitted on a timer, so a phone that delays its acknowledgements
//! does not throttle the stream. Audio waits in a 16 KiB queue per client (about two seconds at 64 kbit/s); a page that
//! finds the queue full is dropped whole, which the player sees as a lost packet, so a slow client cannot make the delay
//! grow. A client that stops acknowledging for 10 s is dropped. What each client asked for and why it ended is printed
//! (`dmesg` keeps it).

use crate::net::{self, MAX_FRAME, NetConfig};
use crate::{Options, Transport, tone_sample};
use audio_wire::{MAX_CHANNELS, ChannelSet, device_sample_16, ogg};
use opus_codec::Encoder;
use tape_net::{ArpCache, ETHERTYPE_ARP, ETHERTYPE_IPV4, EthernetFrame, IcmpEcho, Ipv4Address, Ipv4Packet, MacAddress, TCP_SYN, TCP_ACK, TcpConnection, TcpSegment, TcpState};
use tape_rt::{AudioInfo, TapeApi, println};

const RATE: u32 = 48_000;
const MAX_FRAME_SAMPLES: usize = 2880;
/// Payload bytes per TCP segment, and the size of the connection's receive buffer (the request is small).
const MSS: usize = 1400;
const QUEUE: usize = 24 * 1024;
/// Audio pages are dropped above this much queued data; headers always fit in the rest.
const QUEUE_LIMIT: usize = 16 * 1024;
/// Most bytes in flight (sent, not yet acknowledged): four full segments.
const WINDOW: usize = 4 * MSS;
/// Ticks (10 ms) without an acknowledgement before the client is given up on.
const STALL_TICKS: u32 = 1000;
/// Ticks before an unacknowledged segment is sent again.
const RETRANSMIT_TICKS: u32 = 30;
const MAX_CLIENTS: usize = 3;

static mut QUEUES: [[u8; QUEUE]; MAX_CLIENTS] = [[0; QUEUE]; MAX_CLIENTS];

/// Client `slot`'s send queue as a slice (the tape is single-threaded and `Server` is its only user).
fn queue_data(slot: usize) -> &'static mut [u8] {
    unsafe { core::slice::from_raw_parts_mut((&raw mut QUEUES).cast::<u8>().add(slot * QUEUE), QUEUE) }
}

const PAGE_BODY: &str = "<!doctype html><title>fos</title><h1>fos audio-net</h1><audio controls autoplay src=\"/live.opus\"></audio>\n";
const STREAM_HEADER: &str = "HTTP/1.1 200 OK\r\nServer: fos\r\nContent-Type: audio/ogg\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n";

#[derive(Clone, Copy, PartialEq)]
enum Phase {
    /// Connected, waiting for the request.
    Request,
    /// The response header is queued; Ogg pages follow as they are encoded.
    Streaming,
    /// A complete short response is queued; close when it has been sent.
    Closing,
}

struct Client {
    tcp: TcpConnection<MSS>,
    mac: MacAddress,
    ip: Ipv4Address,
    port: u16,
    phase: Phase,
    /// Tick of the last segment from the client or sent to it.
    progress: u32,
    /// Bytes in this client's queue, the first `unacked` of which are sent but not yet acknowledged.
    queued: usize,
    unacked: usize,
    /// This client's Ogg stream: serial, next page number, granule position.
    serial: u32,
    page_sequence: u32,
    granule: u64,
    dropped: u64,
    retransmits: u64,
    sent: u64,
    opened: u32,
}

impl Client {
    fn queue(&mut self, slot: usize, bytes: &[u8], limit: usize) -> bool {
        if self.queued + bytes.len() > limit {
            return false;
        }
        queue_data(slot)[self.queued..self.queued + bytes.len()].copy_from_slice(bytes);
        self.queued += bytes.len();
        true
    }
}

/// A fixed buffer that `write!` can fill (the response header with its `Content-Length`).
struct Text<'a>(&'a mut [u8], usize);

impl core::fmt::Write for Text<'_> {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        let end = self.1 + text.len();
        self.0.get_mut(self.1..end).ok_or(core::fmt::Error)?.copy_from_slice(text.as_bytes());
        self.1 = end;
        Ok(())
    }
}

enum Route {
    Stream,
    Page,
    NotFound,
    Bad,
}

/// The value of header `name` (case-insensitive, with its colon) in the request, trimmed and cut to 40 characters.
fn header_value<'a>(request: &'a [u8], name: &str) -> Option<&'a str> {
    let text = core::str::from_utf8(request).ok()?;
    text.split("\r\n").skip(1).find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case(name).then(|| {
            let value = value.trim();
            &value[..value.char_indices().nth(40).map_or(value.len(), |(index, _)| index)]
        })
    })
}

struct Server<'a> {
    api: &'a TapeApi,
    nic: usize,
    config: NetConfig,
    port: u16,
    clients: [Option<Client>; MAX_CLIENTS],
    connections: u32,
    /// Audio pages dropped and segments retransmitted over all clients, for the closing line.
    dropped: u64,
    channels: u8,
}

/// Wraps `payload` (an ICMP message or a TCP segment) in IPv4 and Ethernet and sends it.
fn send_ip_frame(api: &TapeApi, nic: usize, config: &NetConfig, mac: MacAddress, ip: Ipv4Address, protocol: u8, payload: &[u8]) {
    let mut frame = [0u8; MAX_FRAME];
    if let Some(total) = net::build_ip_frame(&mut frame, config, mac, ip, protocol, payload) {
        net::send_frame(api, nic, &frame[..total]);
    }
}

impl Server<'_> {
    fn send_ip(&self, mac: MacAddress, ip: Ipv4Address, protocol: u8, payload: &[u8]) {
        send_ip_frame(self.api, self.nic, &self.config, mac, ip, protocol, payload);
    }

    /// One received Ethernet frame: ARP and ping are answered, TCP segments for our port go to their client.
    fn frame(&mut self, bytes: &[u8], arp: &mut ArpCache<8>, now: u32) {
        let Ok(ethernet) = EthernetFrame::parse(bytes) else { return };
        match ethernet.ethertype {
            ETHERTYPE_ARP => {
                let _ = net::inbound(self.api, self.nic, &self.config, arp, bytes, 1..=0, now);
            }
            ETHERTYPE_IPV4 => {
                let Ok(packet) = Ipv4Packet::parse(ethernet.payload, true) else { return };
                if packet.header.destination != self.config.ip || packet.header.source == self.config.ip {
                    return;
                }
                let (source, mac) = (packet.header.source, ethernet.source);
                match packet.header.protocol {
                    1 => {
                        let Ok(echo) = IcmpEcho::parse(packet.payload, true) else { return };
                        if echo.message_type != IcmpEcho::REQUEST {
                            return;
                        }
                        let mut reply = [0u8; MAX_FRAME];
                        if let Ok(length) = IcmpEcho::reply(echo.identifier, echo.sequence, echo.payload).encode(&mut reply) {
                            self.send_ip(mac, source, 1, &reply[..length]);
                        }
                    }
                    6 => {
                        let Ok(segment) = TcpSegment::parse(packet.payload, source, packet.header.destination, true) else { return };
                        if segment.destination_port == self.port {
                            self.segment(&segment, mac, source, now);
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn segment(&mut self, segment: &TcpSegment, mac: MacAddress, ip: Ipv4Address, now: u32) {
        let known = self.clients.iter().position(|client| client.as_ref().is_some_and(|client| client.ip == ip && client.port == segment.source_port));
        let slot = match known {
            Some(slot) => slot,
            None => {
                if segment.flags & (TCP_SYN | TCP_ACK) != TCP_SYN {
                    return; // not a new connection and not one we know
                }
                // A free slot, else the oldest client that is not streaming (a finished page, a probe).
                let free = self.clients.iter().position(Option::is_none).or_else(|| {
                    (0..MAX_CLIENTS).filter(|&slot| self.clients[slot].as_ref().is_some_and(|client| client.phase != Phase::Streaming)).min_by_key(|&slot| self.clients[slot].as_ref().map_or(0, |client| client.opened))
                });
                let Some(slot) = free else {
                    println!("HTTP: a fourth connection from {}.{}.{}.{} was ignored (all {MAX_CLIENTS} are streaming).", ip.0[0], ip.0[1], ip.0[2], ip.0[3]);
                    return;
                };
                let iss = tape_rt::micros() as u32;
                let tcp = TcpConnection::<MSS>::listen(self.config.ip, self.port, ip, segment.source_port, iss, RETRANSMIT_TICKS);
                self.clients[slot] = Some(Client { tcp, mac, ip, port: segment.source_port, phase: Phase::Request, progress: now, queued: 0, unacked: 0, serial: 0, page_sequence: 2, granule: 0, dropped: 0, retransmits: 0, sent: 0, opened: now });
                self.connections += 1;
                println!("HTTP client {}.{}.{}.{}:{} connected (slot {slot}).", ip.0[0], ip.0[1], ip.0[2], ip.0[3], segment.source_port);
                slot
            }
        };
        let Some(client) = self.clients[slot].as_mut() else { return };
        client.mac = mac;
        client.progress = now;
        let mut out = [0u8; 1500];
        if let Ok(Some(length)) = client.tcp.on_segment(segment, now, &mut out) {
            let (mac, ip) = (client.mac, client.ip);
            self.send_ip(mac, ip, 6, &out[..length]);
        }
    }

    /// Reads each new request once it is complete and queues the response.
    fn answer(&mut self) {
        let channels = self.channels;
        for slot in 0..MAX_CLIENTS {
            let Some(client) = self.clients[slot].as_mut() else { continue };
            if client.phase != Phase::Request {
                continue;
            }
            let request = client.tcp.received();
            let route = match ogg::http_get_path(request) {
                Ok(None) => continue,
                Err(()) => Route::Bad,
                Ok(Some(path)) => match path.split('?').next().unwrap_or("") {
                    "/live.opus" | "/stream" => Route::Stream,
                    "/" => Route::Page,
                    _ => Route::NotFound,
                },
            };
            println!("HTTP request from slot {slot}: {} (Range: {}, agent: {})", request.split(|&byte| byte == b'\r').next().and_then(|line| core::str::from_utf8(line).ok()).unwrap_or("?"), header_value(request, "Range").unwrap_or("none"), header_value(request, "User-Agent").unwrap_or("none"));
            let mut sink = [0u8; MSS];
            while client.tcp.take_received(&mut sink) > 0 {}
            let mut text = [0u8; 256];
            match route {
                Route::Stream => {
                    client.phase = Phase::Streaming;
                    client.queue(slot, STREAM_HEADER.as_bytes(), QUEUE);
                    client.serial = tape_rt::micros() as u32;
                    client.page_sequence = 2;
                    client.granule = 0;
                    let mut page = [0u8; 128];
                    let length = ogg::write_page(&mut page, 2, 0, client.serial, 0, &ogg::opus_head(channels, RATE)).unwrap_or(0);
                    client.queue(slot, &page[..length], QUEUE);
                    let mut tags = [0u8; 32];
                    let tags_length = ogg::opus_tags("fos audio-net", &mut tags);
                    let length = ogg::write_page(&mut page, 0, 0, client.serial, 1, &tags[..tags_length]).unwrap_or(0);
                    client.queue(slot, &page[..length], QUEUE);
                }
                Route::Page => {
                    client.phase = Phase::Closing;
                    let mut writer = Text(&mut text, 0);
                    let _ = core::fmt::Write::write_fmt(&mut writer, format_args!("HTTP/1.1 200 OK\r\nServer: fos\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", PAGE_BODY.len()));
                    let length = writer.1;
                    client.queue(slot, &text[..length], QUEUE);
                    client.queue(slot, PAGE_BODY.as_bytes(), QUEUE);
                }
                Route::NotFound | Route::Bad => {
                    client.phase = Phase::Closing;
                    let status = if matches!(route, Route::NotFound) { "404 Not Found" } else { "400 Bad Request" };
                    let mut writer = Text(&mut text, 0);
                    let _ = core::fmt::Write::write_fmt(&mut writer, format_args!("HTTP/1.1 {status}\r\nServer: fos\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"));
                    let length = writer.1;
                    client.queue(slot, &text[..length], QUEUE);
                }
            }
        }
    }

    /// Does what is due for every client: frees the queue bytes it acknowledged, retransmits the oldest unacknowledged data
    /// when the timer is up, sends new data while the window allows (up to `WINDOW` bytes in flight and whatever the peer
    /// advertises, several segments at once so a slow acknowledgement does not throttle the stream), finishes a short
    /// response with a FIN; and forgets a client that is gone or stalled.
    fn pump(&mut self, now: u32) {
        let (api, nic, config) = (self.api, self.nic, self.config);
        for slot in 0..MAX_CLIENTS {
            let Some(client) = self.clients[slot].as_mut() else { continue };
            let (mac, ip) = (client.mac, client.ip);
            let mut out = [0u8; 1500];
            let queue = queue_data(slot);
            // Bytes the peer acknowledged since the last pass leave the queue.
            let in_flight = client.tcp.stream_in_flight().min(client.unacked);
            if client.unacked > in_flight {
                let acked = client.unacked - in_flight;
                queue.copy_within(acked..client.queued, 0);
                client.queued -= acked;
                client.unacked = in_flight;
                client.progress = now;
            }
            if let Ok(Some(length)) = client.tcp.retransmit(now, &mut out) {
                send_ip_frame(api, nic, &config, mac, ip, 6, &out[..length]); // the handshake
            }
            if let Ok(Some(length)) = client.tcp.stream_retransmit(&queue[..client.unacked.min(MSS)], now, &mut out) {
                send_ip_frame(api, nic, &config, mac, ip, 6, &out[..length]);
                client.retransmits += 1;
            }
            if client.tcp.state() == TcpState::CloseWait && client.phase != Phase::Closing {
                client.phase = Phase::Closing; // the player closed its end: finish with a FIN
                client.queued = 0;
                client.unacked = 0;
            }
            if matches!(client.phase, Phase::Streaming | Phase::Closing) {
                loop {
                    let in_flight = client.tcp.stream_in_flight();
                    let window = client.tcp.peer_window().min(WINDOW);
                    let room = if in_flight == 0 { window.max(1) } else { window.saturating_sub(in_flight) }; // an empty pipe always sends: a zero window is probed
                    let count = (client.queued - client.unacked).min(MSS).min(room);
                    if count == 0 {
                        break;
                    }
                    match client.tcp.stream_send(&queue[client.unacked..client.unacked + count], now, &mut out) {
                        Ok(length) => {
                            send_ip_frame(api, nic, &config, mac, ip, 6, &out[..length]);
                            client.unacked += count;
                            client.sent += count as u64;
                        }
                        Err(_) => break,
                    }
                }
            }
            if client.phase == Phase::Closing && client.queued == 0 {
                if let Ok(length) = client.tcp.close(now, &mut out) {
                    send_ip_frame(api, nic, &config, mac, ip, 6, &out[..length]);
                }
            }
            let reason = if client.tcp.state() == TcpState::Closed {
                Some("connection closed")
            } else if now.wrapping_sub(client.progress) > STALL_TICKS {
                Some("no sign of life for 10 s")
            } else {
                None
            };
            if let Some(reason) = reason {
                println!("HTTP client {}.{}.{}.{}:{} gone after {} s: {reason} ({} bytes sent, {} pages dropped, {} segments retransmitted).", ip.0[0], ip.0[1], ip.0[2], ip.0[3], client.port, now.wrapping_sub(client.opened) / 100, client.sent, client.dropped, client.retransmits);
                self.dropped += client.dropped;
                self.clients[slot] = None;
            }
        }
    }

    fn streaming(&self) -> bool {
        self.clients.iter().flatten().any(|client| client.phase == Phase::Streaming)
    }

    /// One encoded Opus packet of `samples` samples per channel becomes an Ogg page for every streaming client, or is
    /// dropped for one that is behind.
    fn push_packet(&mut self, packet: &[u8], samples: u32) {
        for slot in 0..MAX_CLIENTS {
            let Some(client) = self.clients[slot].as_mut() else { continue };
            if client.phase != Phase::Streaming {
                continue;
            }
            client.granule += u64::from(samples);
            let mut page = [0u8; 1400];
            let Some(length) = ogg::write_page(&mut page, 0, u64::from(ogg::PRE_SKIP) + client.granule, client.serial, client.page_sequence, packet) else { continue };
            client.page_sequence = client.page_sequence.wrapping_add(1);
            if !client.queue(slot, &page[..length], QUEUE_LIMIT) {
                client.dropped += 1;
            }
        }
    }

    /// Pages dropped so far, over the clients that are still connected and those that left.
    fn dropped_total(&self) -> u64 {
        self.dropped + self.clients.iter().flatten().map(|client| client.dropped).sum::<u64>()
    }
}

pub fn serve(api: &'static TapeApi, options: &Options) {
    if options.in_mode != 0 {
        (api.audio_select)(0, options.in_mode);
    }
    let mut info = AudioInfo::default();
    let device = (api.audio_info)(&mut info) != 0 && info.capture_channels != 0;
    if options.tone.is_none() && !device {
        return println!("audio-net serve: no USB audio device with inputs (use src=tone=440 to serve a test tone).");
    }
    let (mut device_channels, mut slot) = (info.capture_channels as usize, info.capture_slot_bytes as usize);
    let selection = options.channels.unwrap_or(ChannelSet(0b11));
    let channels = selection.count();
    if channels > 2 {
        return println!("audio-net serve: Ogg Opus here is mono or stereo; ch= names at most two channels.");
    }
    if options.tone.is_none() && selection.channels().any(|channel| channel >= device_channels) {
        return println!("audio-net serve: the device has {device_channels} input channels; ch= asks for more.");
    }
    if !matches!(options.frame_x10, 25 | 50 | 100 | 200 | 400 | 600) {
        return println!("audio-net serve: frame= is 2.5, 5, 10, 20, 40 or 60 (milliseconds).");
    }
    let frame_samples = options.frame_x10 as usize * 48 / 10;
    let Some(transport) = Transport::open(api, options) else { return };
    let Ok(mut encoder) = Encoder::new(RATE as i32, channels as i32, options.bitrate as i32 * channels as i32 / 2, options.complexity as i32, options.frame_x10 < 200) else {
        return println!("audio-net serve: cannot create an Opus encoder.");
    };
    if options.tone.is_none() {
        let status = (api.audio_start)(RATE, 1, 0);
        if status != 0 {
            return println!("audio-net serve: cannot start capture at 48000 Hz (code {status}); the device offers {:?}.", &info.rates[..info.rate_count as usize]);
        }
        (api.audio_info)(&mut info);
        (device_channels, slot) = (info.capture_channels as usize, info.capture_slot_bytes as usize);
        if selection.channels().any(|channel| channel >= device_channels) {
            (api.audio_stop)();
            return println!("audio-net serve: the capture mode at 48000 Hz has {device_channels} channel(s); ch= asks for more.");
        }
    }
    let ip = transport.config.ip.0;
    println!("Serving Ogg Opus, {channels} channel(s), {} kbit/s per pair, {}.{} ms frames, at http://{}.{}.{}.{}:{}/live.opus", options.bitrate / 1000, options.frame_x10 / 10, options.frame_x10 % 10, ip[0], ip[1], ip[2], ip[3], options.http);
    println!("(open http://{}.{}.{}.{}:{}/ for a page with a player; the machine answers ping.)", ip[0], ip[1], ip[2], ip[3], options.http);
    let device_frame = device_channels * slot;
    let capture_bytes = device_frame * frame_samples; // 0 for a tone: no device
    let capture_address = if capture_bytes == 0 { 1 } else { (api.alloc)(capture_bytes * 2) };
    if capture_address == 0 {
        return println!("audio-net serve: out of memory.");
    }
    let capture = unsafe { core::slice::from_raw_parts_mut(if capture_bytes == 0 { core::ptr::NonNull::dangling().as_ptr() } else { capture_address as *mut u8 }, capture_bytes * 2) };
    let channel_list: [usize; MAX_CHANNELS] = {
        let mut list = [0; MAX_CHANNELS];
        for (index, channel) in selection.channels().enumerate() {
            list[index] = channel;
        }
        list
    };
    let mut server = Server { api, nic: options.nic, config: transport.config, port: options.http, clients: [const { None }; MAX_CLIENTS], connections: 0, dropped: 0, channels: channels as u8 };
    let mut arp = ArpCache::<8>::new();
    let mut held = 0usize;
    let mut pcm = [0i16; MAX_FRAME_SAMPLES * 2];
    let mut packet = [0u8; 1275];
    let mut frame = [0u8; MAX_FRAME];
    let (mut sent, mut phase) = (0u64, 0u32);
    let frame_us = frame_samples as u64 * 1_000_000 / u64::from(RATE);
    let started = tape_rt::micros();
    crate::stats::get().rate = RATE;
    loop {
        if (api.should_abort)() != 0 || (options.seconds != 0 && tape_rt::micros() >= started + options.seconds * 1_000_000) {
            break;
        }
        let now = (api.ticks)() as u32;
        let mut busy = false;
        for _ in 0..8 {
            let length = (api.nic_receive)(options.nic, frame.as_mut_ptr(), frame.len());
            if length == 0 {
                break;
            }
            busy = true;
            server.frame(&frame[..length], &mut arp, now);
        }
        server.answer();
        server.pump(now);
        let ready = if options.tone.is_some() {
            tape_rt::micros() >= started + sent * frame_us
        } else {
            held += (api.audio_read)(capture[held..].as_mut_ptr(), capture.len() - held);
            held >= capture_bytes
        };
        if !ready {
            if !busy {
                tape_rt::idle();
            }
            continue;
        }
        // The frame's samples, whether or not anyone listens: the meters on the panel show what would be sent.
        for index in 0..frame_samples {
            for lane in 0..channels {
                pcm[index * channels + lane] = if let Some(frequency) = options.tone {
                    (tone_sample(phase.wrapping_add((frequency * 65_536 / RATE) * index as u32) & 0xffff)) as i16
                } else {
                    let at = index * device_frame + channel_list[lane] * slot;
                    device_sample_16(&capture[at..at + slot])
                };
            }
        }
        crate::stats::peaks_from_i16(&pcm[..frame_samples * channels], channels, frame_samples, 0, true);
        if server.streaming() {
            if let Ok(length) = encoder.encode(&pcm[..frame_samples * channels], frame_samples, &mut packet) {
                server.push_packet(&packet[..length], frame_samples as u32);
                let stats = crate::stats::get();
                stats.sent += 1;
                stats.lost = server.dropped_total(); // the live view's "Lost": audio pages dropped because the client was behind
            }
        }
        if let Some(frequency) = options.tone {
            phase = phase.wrapping_add((frequency * 65_536 / RATE) * frame_samples as u32) & 0xffff;
        } else {
            capture.copy_within(capture_bytes..held, 0);
            held -= capture_bytes;
        }
        sent += 1;
    }
    if options.tone.is_none() {
        (api.audio_stop)();
    }
    if capture_bytes != 0 {
        (api.free)(capture_address, capture_bytes * 2);
    }
    println!("Served {} connection(s); {} audio pages were dropped because a client was behind.", server.connections, server.dropped_total());
}
