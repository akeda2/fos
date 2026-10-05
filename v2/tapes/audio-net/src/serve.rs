//! `audio-net serve`: the Opus stream as Ogg Opus over HTTP, for trying a machine from a browser or VLC (`vlc
//! http://IP:8000/live.opus`) when the RTP receiver is out of reach, such as a phone. It is a test aid, not a
//! distribution server: one client at a time (the newest connection wins) and no authentication.
//!
//! The machine answers ARP and ping, accepts one TCP connection on `http=PORT` (default 8000) and serves
//! `GET /live.opus` as an endless `audio/ogg` response (one Opus packet per Ogg page, see `audio_wire::ogg`);
//! `GET /` is a page with an `<audio>` element pointing at it. The TCP layer is `tape-net`'s: one segment in
//! flight, retransmitted on a timer, so throughput is one segment per round trip. Audio waits in a 12 KiB queue
//! (about a second at 128 kbit/s); a page that finds the queue full is dropped whole, which the player sees as a lost
//! packet, so a slow client cannot make the delay grow. A client that stops acknowledging for 10 s is dropped; a new
//! connection replaces the current one.

use crate::net::{self, MAX_FRAME, NetConfig};
use crate::{Options, Transport, tone_sample};
use audio_wire::{MAX_CHANNELS, ChannelSet, device_sample_16, ogg};
use opus_codec::Encoder;
use tape_net::{ArpCache, ETHERTYPE_ARP, ETHERTYPE_IPV4, EthernetFrame, IcmpEcho, Ipv4Address, Ipv4Packet, MacAddress, TCP_SYN, TCP_ACK, TcpConnection, TcpSegment, TcpState};
use tape_rt::{AudioInfo, TapeApi, println};


const RATE: u32 = 48_000;
const MAX_FRAME_SAMPLES: usize = 2880;
/// Payload bytes per TCP segment (`tape-net` keeps one segment in flight, so this is also its receive window).
const MSS: usize = 1400;
const QUEUE: usize = 16 * 1024;
/// Audio pages are dropped above this much queued data; headers always fit in the rest.
const QUEUE_LIMIT: usize = 12 * 1024;
/// Ticks (10 ms) without an acknowledgement before the client is given up on.
const STALL_TICKS: u32 = 1000;
/// Ticks before an unacknowledged segment is sent again.
const RETRANSMIT_TICKS: u32 = 30;

static mut QUEUE_DATA: [u8; QUEUE] = [0; QUEUE];

/// The send queue as a slice (the tape is single-threaded and `Server` is its only user).
fn queue_data() -> &'static mut [u8] {
    unsafe { core::slice::from_raw_parts_mut((&raw mut QUEUE_DATA).cast::<u8>(), QUEUE) }
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

struct Server<'a> {
    api: &'a TapeApi,
    nic: usize,
    config: NetConfig,
    port: u16,
    client: Option<Client>,
    queued: usize,
    dropped: u64,
    clients: u32,
    serial: u32,
    page_sequence: u32,
    granule: u64,
    channels: u8,
}

impl Server<'_> {
    fn queue(&mut self, bytes: &[u8], limit: usize) -> bool {
        if self.queued + bytes.len() > limit {
            return false;
        }
        queue_data()[self.queued..self.queued + bytes.len()].copy_from_slice(bytes);
        self.queued += bytes.len();
        true
    }

    /// Wraps `payload` (an ICMP message or a TCP segment) in IPv4 and Ethernet and sends it.
    fn send_ip(&self, mac: MacAddress, ip: Ipv4Address, protocol: u8, payload: &[u8]) {
        let mut frame = [0u8; MAX_FRAME];
        if let Some(total) = net::build_ip_frame(&mut frame, &self.config, mac, ip, protocol, payload) {
            net::send_frame(self.api, self.nic, &frame[..total]);
        }
    }

    /// One received Ethernet frame: ARP and ping are answered, TCP segments for our port go to the client.
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
        let new_endpoint = self.client.as_ref().is_none_or(|client| client.ip != ip || client.port != segment.source_port);
        if new_endpoint && segment.flags & (TCP_SYN | TCP_ACK) == TCP_SYN {
            // A new connection takes over: players open a second connection (VLC probes first, a browser reconnects), and
            // the old one may be half closed. The old client is simply forgotten; it times out or is reset.
            let iss = tape_rt::micros() as u32;
            let tcp = TcpConnection::<MSS>::listen(self.config.ip, self.port, ip, segment.source_port, iss, RETRANSMIT_TICKS);
            self.client = Some(Client { tcp, mac, ip, port: segment.source_port, phase: Phase::Request, progress: now });
            self.queued = 0;
            self.clients += 1;
            println!("HTTP client {}.{}.{}.{}:{} connected.", ip.0[0], ip.0[1], ip.0[2], ip.0[3], segment.source_port);
        }
        let Some(client) = self.client.as_mut() else { return };
        if client.ip != ip || client.port != segment.source_port {
            return;
        }
        client.mac = mac;
        client.progress = now;
        let mut out = [0u8; 1500];
        if let Ok(Some(length)) = client.tcp.on_segment(segment, now, &mut out) {
            let (mac, ip) = (client.mac, client.ip);
            self.send_ip(mac, ip, 6, &out[..length]);
        }
    }

    /// Reads the request once it is complete and queues the response.
    fn answer(&mut self) {
        let Some(client) = self.client.as_mut() else { return };
        if client.phase != Phase::Request {
            return;
        }
        let route = match ogg::http_get_path(client.tcp.received()) {
            Ok(None) => return,
            Err(()) => Route::Bad,
            Ok(Some(path)) => match path.split('?').next().unwrap_or("") {
                "/live.opus" | "/stream" => Route::Stream,
                "/" => Route::Page,
                _ => Route::NotFound,
            },
        };
        let mut sink = [0u8; MSS];
        while client.tcp.take_received(&mut sink) > 0 {}
        let mut text = [0u8; 256];
        match route {
            Route::Stream => {
                client.phase = Phase::Streaming;
                self.queue(STREAM_HEADER.as_bytes(), QUEUE);
                self.serial = tape_rt::micros() as u32;
                self.page_sequence = 2;
                self.granule = 0;
                let mut page = [0u8; 128];
                let length = ogg::write_page(&mut page, 2, 0, self.serial, 0, &ogg::opus_head(self.channels, RATE)).unwrap_or(0);
                self.queue(&page[..length], QUEUE);
                let mut tags = [0u8; 32];
                let tags_length = ogg::opus_tags("fos audio-net", &mut tags);
                let length = ogg::write_page(&mut page, 0, 0, self.serial, 1, &tags[..tags_length]).unwrap_or(0);
                self.queue(&page[..length], QUEUE);
                println!("Streaming Ogg Opus.");
            }
            Route::Page => {
                client.phase = Phase::Closing;
                let mut writer = Text(&mut text, 0);
                let _ = core::fmt::Write::write_fmt(&mut writer, format_args!("HTTP/1.1 200 OK\r\nServer: fos\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", PAGE_BODY.len()));
                let length = writer.1;
                self.queue(&text[..length], QUEUE);
                self.queue(PAGE_BODY.as_bytes(), QUEUE);
            }
            Route::NotFound | Route::Bad => {
                client.phase = Phase::Closing;
                let status = if matches!(route, Route::NotFound) { "404 Not Found" } else { "400 Bad Request" };
                let mut writer = Text(&mut text, 0);
                let _ = core::fmt::Write::write_fmt(&mut writer, format_args!("HTTP/1.1 {status}\r\nServer: fos\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"));
                let length = writer.1;
                self.queue(&text[..length], QUEUE);
            }
        }
    }
}

impl Server<'_> {
    /// Sends what is due: a retransmission, the next chunk of the queue once the last one is acknowledged, the FIN of a
    /// finished response; and forgets a client that is gone or stalled.
    fn pump(&mut self, now: u32) {
        let Some(client) = self.client.as_mut() else { return };
        let (mac, ip) = (client.mac, client.ip);
        let mut out = [0u8; 1500];
        let mut send: Option<usize> = None;
        if let Ok(Some(length)) = client.tcp.retransmit(now, &mut out) {
            send = Some(length);
        } else if client.tcp.state() == TcpState::CloseWait && client.phase == Phase::Streaming {
            client.phase = Phase::Closing; // the player went away: finish with a FIN
            self.queued = 0;
        } else if self.queued > 0 && matches!(client.phase, Phase::Streaming | Phase::Closing) {
            let chunk = self.queued.min(MSS);
            let data = &queue_data()[..chunk];
            if let Ok(length) = client.tcp.send_data(data, now, &mut out) {
                send = Some(length);
                client.progress = now;
                queue_data().copy_within(chunk..self.queued, 0);
                self.queued -= chunk;
            }
        } else if client.phase == Phase::Closing && self.queued == 0 {
            if let Ok(length) = client.tcp.close(now, &mut out) {
                send = Some(length);
            }
        }
        let gone = client.tcp.state() == TcpState::Closed || now.wrapping_sub(client.progress) > STALL_TICKS;
        if let Some(length) = send {
            self.send_ip(mac, ip, 6, &out[..length]);
        }
        if gone {
            println!("HTTP client gone ({} audio pages dropped so far).", self.dropped);
            self.client = None;
            self.queued = 0;
        }
    }

    fn streaming(&self) -> bool {
        self.client.as_ref().is_some_and(|client| client.phase == Phase::Streaming)
    }

    /// One encoded Opus packet of `samples` samples per channel becomes an Ogg page, or is dropped if the client is behind.
    fn push_packet(&mut self, packet: &[u8], samples: u32) {
        self.granule += u64::from(samples);
        let mut page = [0u8; 1400];
        let Some(length) = ogg::write_page(&mut page, 0, u64::from(ogg::PRE_SKIP) + self.granule, self.serial, self.page_sequence, packet) else { return };
        self.page_sequence = self.page_sequence.wrapping_add(1);
        if !self.queue(&page[..length], QUEUE_LIMIT) {
            self.dropped += 1;
        }
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
    let mut server = Server { api, nic: options.nic, config: transport.config, port: options.http, client: None, queued: 0, dropped: 0, clients: 0, serial: 0, page_sequence: 2, granule: 0, channels: channels as u8 };
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
                crate::stats::get().sent += 1;
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
    println!("Served {} client(s); {} audio pages were dropped because a client was behind.", server.clients, server.dropped);
}
