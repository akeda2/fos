//! `audio-net`: streams audio between hosts, from and to the USB audio device.
//!
//! ```text
//! tape run audio-net send [proto=raw|rtp] [nic=N] [to=MAC|IP[:PORT]] [rate=48000] [ch=3-4] [bits=24]
//!                         [src=capture|tone=HZ] [secs=N] [ip=A.B.C.D/N|dhcp] [gw=IP] [pt=96]
//! tape run audio-net recv [proto=raw|rtp] [nic=N] [from=MAC] [port=5004] [chan=N] [ch=1-2] [target=MS]
//!                         [rate=48000] [bits=24] [secs=N] [ip=A.B.C.D/N|dhcp] [gw=IP]
//! ```
//!
//! **send** takes its audio from the capture side of the attached USB audio device (`src=capture`, the
//! default) or from a test tone (`src=tone=440`, which needs no audio device), cuts it into packets and sends
//! them out of NIC `N` (default 1). `ch=` picks the device's input channels to send (1-based, `ch=9-10,1`),
//! `bits=` is 16 or 24. The packet time is chosen so a packet fits one Ethernet frame: 1 ms for a stereo
//! pair, shorter for many channels.
//!
//! **recv** listens on NIC `N` for a stream, starts the device's playback and plays it. `ch=` says which
//! output channels the stream's channels go to (default 1, 2, ...). The two hosts' clocks differ slightly, so
//! the receiver keeps the playback ring near `target=` ms: by default a variable-rate resampler (4-point cubic,
//! integer) steered by a small PI servo learns the true clock ratio, a few hundred ppm, and follows it without
//! ever dropping or repeating a sample (`drift=resample`); `drift=slip` instead drops or repeats one frame when
//! the level strays. A lost packet is replaced by silence of the same length. `secs=` ends either side after
//! that long; Escape always does.
//!
//! **Protocols.** `proto=raw` (default) is a bare Ethernet frame (`audio-wire`'s format): works between two
//! hosts on one segment with no addresses at all. `proto=rtp` is standard RTP (RFC 3550) over UDP over
//! IPv4 with 16- or 24-bit linear samples (RFC 3190 `L16`/`L24`, big endian), packet times of 125 us to 1 ms
//! as in AES67, so ordinary software (ffplay, GStreamer, VLC) and AES67 devices (Dante in AES67 mode,
//! Ravenna) can receive or send it. RTP needs an IP address: `ip=192.168.1.10/24` (and `gw=` if the peer is
//! off-subnet) or `ip=dhcp`. `to=` is the destination IP, a unicast address or a multicast group
//! (239.x.y.z, no ARP needed); `port=` (default 5004) is both the source and destination UDP port. `send`
//! prints an SDP description of the stream to open on the other side. `recv` cannot learn the channel count
//! from RTP (SDP carries it), so give `chan=` (default 2) and `bits=`. There is no PTP, so clocks are not
//! synchronised: the receiver follows the sender (an AES67 device that insists on a PTP reference will not
//! accept the stream, but the packet format is exactly AES67's).
#![no_std]
#![no_main]

mod net;

use audio_wire::{ChannelSet, ETHERNET_HEADER, ETHERTYPE, Fill, HEADER_LEN, Header, MAX_RAW_PAYLOAD, MAX_RTP_PAYLOAD, Resampler, Sdp, Servo, Slip, frames_per_packet, frames_to_device, pack, unpack, wire_to_frames, write_sdp};
use net::{Inbound, NetConfig};
use tape_net::{ArpCache, Ipv4Address, MacAddress, RtpPacket};
use tape_rt::{AudioInfo, AudioStats, TapeApi, print, println};

const BROADCAST: [u8; 6] = [0xff; 6];
/// The kernel's playback PCM ring is this many bytes; its free space says how full it is.
const RING_BYTES: usize = 1 << 20;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Proto {
    Raw,
    Rtp,
}

struct Options<'a> {
    proto: Proto,
    nic: usize,
    to: &'a str,
    rate: u32,
    channels: Option<ChannelSet>,
    chan: usize,
    bits: usize,
    tone: Option<u32>,
    target_ms: u32,
    seconds: u64,
    verbose: bool,
    ip: &'a str,
    gateway: &'a str,
    port: u16,
    payload_type: u8,
    from: Option<[u8; 6]>,
    /// Clock-drift handling: a resampler steered by a servo (default) or single-frame slips.
    resample: bool,
    /// Test aid for `send src=tone`: run the sender's clock this many ppm fast (negative: slow).
    skew_ppm: i64,
}

fn parse_mac(text: &str) -> Option<[u8; 6]> {
    let mut mac = [0u8; 6];
    let mut parts = text.split(':');
    for byte in mac.iter_mut() {
        *byte = u8::from_str_radix(parts.next()?, 16).ok()?;
    }
    parts.next().is_none().then_some(mac)
}

fn parse<'a>(words: impl Iterator<Item = &'a str>) -> Result<Options<'a>, &'static str> {
    let mut options = Options {
        proto: Proto::Raw, nic: 0, to: "", rate: 48_000, channels: None, chan: 2, bits: 24, tone: None, target_ms: 6, seconds: 0, verbose: false,
        ip: "", gateway: "", port: 5004, payload_type: 96, from: None, resample: true, skew_ppm: 0,
    };
    for word in words {
        let (key, value) = word.split_once('=').unwrap_or((word, ""));
        match key {
            "proto" => options.proto = match value { "raw" => Proto::Raw, "rtp" => Proto::Rtp, _ => return Err("proto= is raw or rtp") },
            "nic" => options.nic = value.parse::<usize>().ok().filter(|&n| n >= 1).ok_or("nic=N needs a number from 1")? - 1,
            "to" => options.to = value,
            "from" => options.from = Some(parse_mac(value).ok_or("from= takes a MAC address like 00:11:22:33:44:55")?),
            "rate" => options.rate = value.parse().map_err(|_| "rate= needs a number")?,
            "ch" => options.channels = Some(ChannelSet::parse(value).ok_or("ch= is like 1-2 or 9-10,1 (channels 1 to 32)")?),
            "chan" => options.chan = value.parse().ok().filter(|&n| (1..=audio_wire::MAX_CHANNELS).contains(&n)).ok_or("chan= is 1 to 32")?,
            "bits" => options.bits = value.parse().ok().filter(|&b| b == 16 || b == 24).ok_or("bits= is 16 or 24")?,
            "src" => match value.strip_prefix("tone=") {
                Some(frequency) => options.tone = Some(frequency.parse().map_err(|_| "src=tone=HZ needs a number")?),
                None if value == "capture" => {}
                None => return Err("src= is capture or tone=HZ"),
            },
            "target" => options.target_ms = value.parse().map_err(|_| "target= needs milliseconds")?,
            "secs" => options.seconds = value.parse().map_err(|_| "secs= needs a number")?,
            "ip" => options.ip = value,
            "gw" => options.gateway = value,
            "port" => options.port = value.parse().map_err(|_| "port= needs a number")?,
            "pt" => options.payload_type = value.parse().ok().filter(|&p| p < 128).ok_or("pt= is 0 to 127")?,
            "drift" => options.resample = match value { "resample" => true, "slip" => false, _ => return Err("drift= is resample or slip") },
            "skew" => options.skew_ppm = value.parse().map_err(|_| "skew= needs a number of ppm")?,
            "verbose" => options.verbose = true,
            _ => return Err("unknown option"),
        }
    }
    Ok(options)
}

/// A parabola per half cycle standing in for a sine (integer only): a quarter of full scale, 16 bits.
fn tone_sample(phase: u32) -> i32 {
    let half = i64::from(phase & 0x7fff);
    let magnitude = (half * (0x8000 - half)) >> 13;
    (if phase & 0x8000 != 0 { -magnitude } else { magnitude } / 4) as i32
}

/// Everything the network side needs: raw needs only a MAC; RTP a full IPv4 configuration.
struct Transport {
    proto: Proto,
    mac: [u8; 6],
    config: NetConfig,
    arp: ArpCache<8>,
    port: u16,
    payload_type: u8,
}

impl Transport {
    fn open(api: &TapeApi, options: &Options) -> Option<Transport> {
        let mut mac = [0u8; 6];
        (api.nic_mac)(options.nic, mac.as_mut_ptr());
        let mut config = NetConfig { mac: MacAddress(mac), ip: Ipv4Address([0; 4]), netmask: Ipv4Address([0; 4]), gateway: Ipv4Address([0; 4]) };
        if options.proto == Proto::Rtp {
            if options.ip == "dhcp" {
                println!("DHCP: asking for an address on NIC {}...", options.nic + 1);
                config = net::dhcp(api, options.nic, MacAddress(mac)).or_else(|| {
                    println!("audio-net: no DHCP lease within 8 s.");
                    None
                })?;
            } else {
                let (ip, netmask) = net::parse_cidr(options.ip).or_else(|| {
                    println!("audio-net: proto=rtp needs ip=A.B.C.D/N (or ip=dhcp).");
                    None
                })?;
                config.ip = ip;
                config.netmask = netmask;
            }
            if !options.gateway.is_empty() {
                config.gateway = net::parse_ip(options.gateway).or_else(|| {
                    println!("audio-net: gw= is an IPv4 address.");
                    None
                })?;
            }
            let ip = config.ip.0;
            println!("Network: {}.{}.{}.{} on NIC {}.", ip[0], ip[1], ip[2], ip[3], options.nic + 1);
        }
        Some(Transport { proto: options.proto, mac, config, arp: ArpCache::new(), port: options.port, payload_type: options.payload_type })
    }
}

/// The sending side's destination, resolved once before streaming.
enum Destination {
    Raw([u8; 6]),
    Rtp { mac: MacAddress, ip: Ipv4Address, ttl: u8 },
}

fn send(api: &'static TapeApi, options: &Options) {
    let tone = options.tone;
    let mut info = AudioInfo::default();
    let device = (api.audio_info)(&mut info) != 0 && info.capture_channels != 0;
    if tone.is_none() && !device {
        return println!("audio-net send: no USB audio device with inputs (use src=tone=440 to send a test tone).");
    }
    let (device_channels, slot) = (info.capture_channels as usize, info.capture_slot_bytes as usize);
    let selection = options.channels.unwrap_or(ChannelSet(0b11));
    let channels = selection.count();
    if tone.is_none() && selection.channels().any(|channel| channel >= device_channels) {
        return println!("audio-net send: the device has {device_channels} input channels; ch= asks for more.");
    }
    let bits = options.bits;
    let Some(mut transport) = Transport::open(api, options) else { return };
    let max_payload = if options.proto == Proto::Rtp { MAX_RTP_PAYLOAD } else { MAX_RAW_PAYLOAD };
    let per_packet = frames_per_packet(options.rate, channels, bits, max_payload);
    let frame_bytes = channels * bits / 8;
    let packet_us = u64::from(per_packet as u32) * 1_000_000 / u64::from(options.rate);
    // `skew=` (tone source only) paces packets as if the sender's clock were off by that many ppm.
    let paced_us_x1000 = (packet_us as i64 * 1000 * (1_000_000 - options.skew_ppm) / 1_000_000) as u64;
    let destination = match options.proto {
        Proto::Raw => Destination::Raw(if options.to.is_empty() { BROADCAST } else { match parse_mac(options.to) { Some(mac) => mac, None => return println!("audio-net send: to= takes a MAC address for proto=raw.") } }),
        Proto::Rtp => {
            let (host, _) = options.to.split_once(':').unwrap_or((options.to, ""));
            let Some(ip) = net::parse_ip(host) else { return println!("audio-net send: proto=rtp needs to=IP (a unicast address or a multicast group).") };
            let Some(mac) = net::resolve(api, options.nic, &transport.config, &mut transport.arp, ip) else {
                return println!("audio-net send: no answer from {host} (ARP); is it on this subnet, or is gw= set?");
            };
            Destination::Rtp { mac, ip, ttl: if audio_wire::is_multicast_address(ip.0) { 32 } else { 64 } }
        }
    };
    if tone.is_none() {
        let status = (api.audio_start)(options.rate, 1, 0);
        if status != 0 {
            return println!("audio-net send: cannot start capture at {} Hz (code {status}); the device offers {:?}.", options.rate, &info.rates[..info.rate_count as usize]);
        }
    }
    println!(
        "Sending {channels} channel(s), {bits}-bit, {} Hz, {per_packet} frames ({packet_us} us) per packet, from {} over {}.",
        options.rate,
        if tone.is_some() { "a test tone" } else { "the capture device" },
        if options.proto == Proto::Rtp { "RTP/UDP" } else { "raw Ethernet" }
    );
    let stream = (tape_rt::micros() as u32) ^ 0x5a5a_a5a5;
    if let Destination::Rtp { ip, .. } = destination {
        let mut text = [0u8; 600];
        let length = write_sdp(&Sdp { origin: transport.config.ip.0, destination: ip.0, port: options.port, payload_type: options.payload_type, channels, rate: options.rate, bits, packet_us: packet_us as u32, mac: transport.mac, session: stream }, &mut text);
        println!("SDP for the receiver (save as stream.sdp; e.g. ffplay -protocol_whitelist file,udp,rtp stream.sdp):");
        println!("{}", core::str::from_utf8(&text[..length]).unwrap_or(""));
    }
    let mut frame = [0u8; 1514];
    let mut payload = [0u8; 1514];
    let mut capture = [0u8; 8192];
    let mut held = 0usize; // bytes of device-format audio waiting in `capture`
    let device_frame = device_channels * slot;
    let (mut sequence, mut timestamp, mut sent, mut phase) = (0u16, 0u32, 0u64, 0u32);
    let started = tape_rt::micros();
    let mut next_report = started + 1_000_000;
    loop {
        if (api.should_abort)() != 0 {
            break;
        }
        if options.seconds != 0 && tape_rt::micros() >= started + options.seconds * 1_000_000 {
            break;
        }
        let have_packet = if let Some(frequency) = tone {
            // Paced by the clock: packet number `sent` is due at started + sent * packet time.
            if tape_rt::micros() >= started + sent * paced_us_x1000 / 1000 {
                for index in 0..per_packet {
                    let sample = tone_sample(phase) << 8; // 16-bit tone as a 24-bit value
                    phase = (phase + (u64::from(frequency) * 65_536 / u64::from(options.rate)) as u32) & 0xffff;
                    for channel in 0..channels {
                        let at = (index * channels + channel) * bits / 8;
                        let bytes = sample.to_be_bytes();
                        let wire = &bytes[1..];
                        payload[at..at + bits / 8].copy_from_slice(if bits == 24 { wire } else { &wire[..2] });
                    }
                }
                true
            } else {
                false
            }
        } else {
            // Paced by the capture device: send a packet whenever a whole one is available.
            let got = (api.audio_read)(capture[held..].as_mut_ptr(), capture.len() - held);
            held += got;
            if held >= per_packet * device_frame {
                pack(&capture[..per_packet * device_frame], slot, device_channels, per_packet, &selection, bits, &mut payload);
                capture.copy_within(per_packet * device_frame..held, 0);
                held -= per_packet * device_frame;
                true
            } else {
                false
            }
        };
        if !have_packet {
            tape_rt::idle();
            continue;
        }
        let length = per_packet * frame_bytes;
        let total = match destination {
            Destination::Raw(mac) => {
                frame[0..6].copy_from_slice(&mac);
                frame[6..12].copy_from_slice(&transport.mac);
                frame[12..14].copy_from_slice(&ETHERTYPE.to_be_bytes());
                Header { first: sent == 0, sequence, timestamp, stream, channels: channels as u8, bits: bits as u8, frames: per_packet as u16, rate: options.rate }.encode(&mut frame[ETHERNET_HEADER..]);
                frame[ETHERNET_HEADER + HEADER_LEN..ETHERNET_HEADER + HEADER_LEN + length].copy_from_slice(&payload[..length]);
                Some(ETHERNET_HEADER + HEADER_LEN + length)
            }
            Destination::Rtp { mac, ip, ttl } => {
                let rtp = RtpPacket::new(transport.payload_type, false, sequence, timestamp, stream, &payload[..length]);
                net::build_rtp_frame(&mut frame, &transport.config, mac, ip, transport.port, ttl, &rtp)
            }
        };
        if let Some(total) = total {
            (api.nic_send)(options.nic, frame.as_ptr(), total);
        }
        sequence = sequence.wrapping_add(1);
        timestamp = timestamp.wrapping_add(per_packet as u32);
        sent += 1;
        if tape_rt::micros() >= next_report {
            next_report += 1_000_000;
            print!(".");
        }
    }
    if tone.is_none() {
        (api.audio_stop)();
    }
    println!();
    println!("Sent {sent} packets ({} frames).", sent * per_packet as u64);
}

/// One received packet, as the receiver's common code sees it.
struct Packet {
    stream: u32,
    source: [u8; 8],
    sequence: u16,
    channels: usize,
    bits: usize,
    rate: u32,
    frames: usize,
    /// Where the samples are in the frame buffer.
    payload: core::ops::Range<usize>,
}

/// Classifies one frame for the active protocol. For RTP this also answers ARP.
fn classify(api: &TapeApi, options: &Options, transport: &mut Transport, frame: &[u8]) -> Option<Packet> {
    match transport.proto {
        Proto::Raw => {
            if frame.len() < ETHERNET_HEADER + HEADER_LEN || frame[12..14] != ETHERTYPE.to_be_bytes() {
                return None;
            }
            let header = Header::parse(&frame[ETHERNET_HEADER..])?;
            let sender: [u8; 6] = frame[6..12].try_into().ok()?;
            if options.from.is_some_and(|wanted| wanted != sender) {
                return None;
            }
            let mut source = [0u8; 8];
            source[..6].copy_from_slice(&sender);
            let start = ETHERNET_HEADER + HEADER_LEN;
            Some(Packet { stream: header.stream, source, sequence: header.sequence, channels: usize::from(header.channels), bits: usize::from(header.bits), rate: header.rate, frames: usize::from(header.frames), payload: start..start + header.payload_len() })
        }
        Proto::Rtp => {
            let now = (api.ticks)() as u32;
            let (source, payload) = match net::inbound(api, options.nic, &transport.config, &mut transport.arp, frame, transport.port, now) {
                Inbound::Udp { source, payload, .. } => (source, payload),
                Inbound::Rejected(reason) => {
                    if options.verbose {
                        println!(" [dropped a {}-byte frame: {reason}]", frame.len());
                    }
                    return None;
                }
                _ => return None,
            };
            let rtp = match RtpPacket::parse(payload) {
                Ok(rtp) => rtp,
                Err(_) => {
                    if options.verbose {
                        println!(" [dropped a {}-byte UDP payload: not RTP]", payload.len());
                    }
                    return None;
                }
            };
            let frame_bytes = options.chan * options.bits / 8;
            let frames = rtp.payload.len() / frame_bytes;
            if frames == 0 {
                return None;
            }
            let start = rtp.payload.as_ptr() as usize - frame.as_ptr() as usize;
            let mut id = [0u8; 8];
            id[..4].copy_from_slice(&source.0);
            Some(Packet { stream: rtp.ssrc, source: id, sequence: rtp.sequence_number, channels: options.chan, bits: options.bits, rate: options.rate, frames, payload: start..start + frames * frame_bytes })
        }
    }
}

fn receive(api: &'static TapeApi, options: &Options) {
    let mut info = AudioInfo::default();
    if (api.audio_info)(&mut info) == 0 || info.playback_channels == 0 {
        return println!("audio-net recv: no USB audio device with outputs.");
    }
    let (device_channels, slot) = (info.playback_channels as usize, info.playback_slot_bytes as usize);
    let device_frame = device_channels * slot;
    let Some(mut transport) = Transport::open(api, options) else { return };
    // Start playback now, at `rate=` (default 48000): starting a USB stream takes a while (control requests,
    // Configure Endpoint), and doing it when the first packet arrives made the frames that came meanwhile pile
    // up and overflow. A stream at another rate restarts it, once.
    let mut running_rate = options.rate;
    let status = (api.audio_start)(running_rate, 0, 1);
    if status != 0 {
        return println!("audio-net recv: cannot start playback at {running_rate} Hz (code {status}); the device offers {:?}.", &info.rates[..info.rate_count as usize]);
    }
    println!("Listening on NIC {} for {} audio; playing on {device_channels} output channel(s) of {}-bit. Escape stops.", options.nic + 1, if options.proto == Proto::Rtp { "RTP/UDP" } else { "raw Ethernet" }, info.playback_bits);
    let mut frame = [0u8; 1514];
    let mut staging = [0u8; 8192];
    let mut locked: Option<(u32, [u8; 8])> = None;
    let (mut expected, mut running) = (0u16, true);
    let (mut packets, mut lost, mut dropped, mut repeated, mut foreign) = (0u64, 0u64, 0u64, 0u64, 0u64);
    let mut fill = Fill::new(0);
    let mut servo = Servo::new(0);
    let mut resampler = Resampler::new(2);
    let mut work = [0i32; 4096];
    let mut resampled = [0i32; 4096];
    let mut placement = [None; audio_wire::MAX_CHANNELS];
    let mut wire_channels = 0usize;
    let mut last_packet = tape_rt::micros();
    let started = tape_rt::micros();
    let mut next_report = started + 1_000_000;
    let mut level = 0usize;
    let (mut work_total, mut work_max, mut slow_receive, mut slow_info, mut gaps_shown) = (0u64, 0u64, 0u64, 0u64, 0u32);
    loop {
        if (api.should_abort)() != 0 {
            break;
        }
        if options.seconds != 0 && tape_rt::micros() >= started + options.seconds * 1_000_000 {
            break;
        }
        let t_receive = tape_rt::micros();
        let length = (api.nic_receive)(options.nic, frame.as_mut_ptr(), frame.len());
        let spent_receive = tape_rt::micros() - t_receive;
        slow_receive = slow_receive.max(if length != 0 { spent_receive } else { 0 });
        if length == 0 {
            if locked.is_some() && tape_rt::micros() > last_packet + 2_000_000 {
                println!("\nThe sender went quiet; waiting for a stream again.");
                locked = None; // playback keeps running (silence), ready for the next stream
            }
            tape_rt::idle();
            continue;
        }
        let Some(packet) = classify(api, options, &mut transport, &frame[..length]) else { continue };
        match locked {
            None => {
                locked = Some((packet.stream, packet.source));
                expected = packet.sequence;
                wire_channels = packet.channels;
                // Where the stream's channels go on the device: ch= if given, else 1, 2, 3...
                let targets = options.channels.unwrap_or(ChannelSet((1u64 << wire_channels.min(device_channels)).wrapping_sub(1) as u32));
                placement = [None; audio_wire::MAX_CHANNELS];
                for (index, device_channel) in targets.channels().enumerate().take(wire_channels) {
                    placement[index] = Some(device_channel);
                }
                if packet.rate != running_rate {
                    (api.audio_stop)();
                    let status = (api.audio_start)(packet.rate, 0, 1);
                    if status != 0 {
                        println!("audio-net recv: cannot start playback at {} Hz (code {status}); the device offers {:?}.", packet.rate, &info.rates[..info.rate_count as usize]);
                        running = false;
                        locked = None;
                        continue;
                    }
                    running_rate = packet.rate;
                }
                running = true;
                let target_frames = (packet.rate as usize * options.target_ms as usize / 1000).max(1);
                fill = Fill::new(target_frames);
                servo = Servo::new(target_frames);
                resampler = Resampler::new(wire_channels);
                // Pre-fill the ring with silence up to the target, so the first packets have somewhere to land.
                let silence = [0u8; 1024];
                let mut needed = target_frames * device_frame;
                while needed > 0 {
                    let written = (api.audio_write)(silence.as_ptr(), needed.min(silence.len()) / device_frame * device_frame);
                    if written == 0 {
                        break;
                    }
                    needed -= written.min(needed);
                }
                let id = packet.source;
                if options.proto == Proto::Rtp {
                    println!("Locked onto a stream: {} channel(s), {}-bit, {} Hz, from {}.{}.{}.{}.", packet.channels, packet.bits, packet.rate, id[0], id[1], id[2], id[3]);
                } else {
                    println!("Locked onto a stream: {} channel(s), {}-bit, {} Hz, from {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}.", packet.channels, packet.bits, packet.rate, id[0], id[1], id[2], id[3], id[4], id[5]);
                }
            }
            Some((stream, from)) if stream != packet.stream || from != packet.source => {
                foreign += 1;
                continue; // a second sender: first one wins
            }
            _ => {}
        }
        last_packet = tape_rt::micros();
        // Sequence: a short gap is lost packets, bridged with silence so the audio stays aligned; a long one
        // (the receiver was busy, a cable was pulled) is a resync with no silence.
        let gap = packet.sequence.wrapping_sub(expected);
        if gap != 0 && gap < 0x8000 {
            lost += u64::from(gap);
            // Printing is slow (a serial console moves ~10 ms per line): a loss message would itself lose packets.
            if options.verbose && gaps_shown < 3 {
                gaps_shown += 1;
                println!(" [gap of {gap} before sequence {} at packet {packets}]", packet.sequence);
            }
            if gap <= 8 {
                let mut remaining = (usize::from(gap) * packet.frames).min(2_048) * device_frame;
                let silence = [0u8; 1024];
                while remaining > 0 {
                    let chunk = remaining.min(silence.len()) / device_frame * device_frame;
                    if chunk == 0 || (api.audio_write)(silence.as_ptr(), chunk) == 0 {
                        break;
                    }
                    remaining -= chunk;
                }
            }
        } else if gap >= 0x8000 {
            continue; // an old or duplicated packet
        }
        expected = packet.sequence.wrapping_add(1);
        // Convert, with at most one frame dropped or repeated if the ring level has drifted.
        let frames = packet.frames.min(staging.len() / device_frame - 1);
        let t_info = tape_rt::micros();
        level = (RING_BYTES - {
            (api.audio_info)(&mut info);
            info.playback_space as usize
        }) / device_frame;
        slow_info = slow_info.max(tape_rt::micros() - t_info);
        if level > fill.target() * 4 + 64 {
            dropped += frames as u64; // far too full (after a stall): discard whole packets to catch up quickly
            packets += 1;
            continue;
        }
        let payload = &frame[packet.payload.clone()];
        let work_start = tape_rt::micros();
        if options.resample {
            // Steer the resampler from the ring level, run the packet through it, place and write the result.
            resampler.step = servo.update(level, frames, packet.rate);
            wire_to_frames(payload, wire_channels, packet.bits, frames, &mut work);
            let mut produced = 0;
            let mut out_frame = [0i32; audio_wire::MAX_CHANNELS];
            for frame_index in 0..frames {
                resampler.push(&work[frame_index * wire_channels..(frame_index + 1) * wire_channels]);
                while resampler.next_output(&mut out_frame) && (produced + 1) * wire_channels <= resampled.len() && (produced + 1) * device_frame <= staging.len() {
                    resampled[produced * wire_channels..(produced + 1) * wire_channels].copy_from_slice(&out_frame[..wire_channels]);
                    produced += 1;
                }
            }
            frames_to_device(&resampled, wire_channels, produced, &placement[..wire_channels], slot, device_channels, &mut staging);
            let bytes = produced * device_frame;
            if (api.audio_write)(staging.as_ptr(), bytes) < bytes {
                dropped += 1; // the ring is full (the sender runs fast, or the device stalled)
            }
        } else {
            let mut out_frames = frames;
            let advice = fill.advise(level);
            let skip = usize::from(advice == Slip::Drop);
            dropped += skip as u64;
            let wire_skip = skip * wire_channels * packet.bits / 8;
            out_frames -= skip.min(out_frames);
            unpack(&payload[wire_skip..], wire_channels, packet.bits, out_frames, &placement[..wire_channels], slot, device_channels, &mut staging);
            if advice == Slip::Repeat && out_frames > 0 {
                repeated += 1;
                staging.copy_within((out_frames - 1) * device_frame..out_frames * device_frame, out_frames * device_frame);
                out_frames += 1;
            }
            let bytes = out_frames * device_frame;
            if (api.audio_write)(staging.as_ptr(), bytes) < bytes {
                dropped += 1; // the ring is full (the sender runs fast, or the device stalled)
            }
        }
        let spent = tape_rt::micros() - work_start;
        work_total += spent;
        work_max = work_max.max(spent);
        packets += 1;
        if tape_rt::micros() >= next_report {
            next_report += 1_000_000;
            print!(".");
            if options.verbose && (next_report - started) / 1_000_000 % 10 == 0 {
                println!(" [{packets} packets, {lost} lost, level {level} frames (target {}), {dropped} dropped, {repeated} repeated, clock offset {} ppm]", fill.target(), servo.offset_ppm());
            }
        }
    }
    let mut stats = AudioStats::default();
    (api.audio_stats)(&mut stats);
    if running {
        (api.audio_stop)();
    }
    let _ = level;
    println!();
    if options.verbose && packets > 0 {
        println!("Processing took {} us per packet on average, {} us at worst; nic_receive up to {slow_receive} us, audio_info up to {slow_info} us.", work_total / packets, work_max);
    }
    if options.resample {
        println!("Received {packets} packets: {lost} lost, {dropped} frame(s) discarded, clock offset learned {} ppm (resampled, no frames dropped or repeated for drift), {foreign} from another sender; playback {} packets, {} underruns, {} transfer errors.", servo.offset_ppm(), stats.playback_packets, stats.playback_underruns, stats.playback_errors);
    } else {
        println!("Received {packets} packets: {lost} lost, {dropped} frame(s) dropped and {repeated} repeated for clock drift, {foreign} from another sender; playback {} packets, {} underruns, {} transfer errors.", stats.playback_packets, stats.playback_underruns, stats.playback_errors);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(api: &'static TapeApi) {
    tape_rt::init(api);
    let arguments = core::str::from_utf8(tape_rt::args()).unwrap_or("");
    let mut words = arguments.split_ascii_whitespace();
    let mode = words.next().unwrap_or("");
    let options = match parse(words) {
        Ok(options) => options,
        Err(reason) => return println!("audio-net: {reason}. See the header of v2/tapes/audio-net/src/main.rs for the options."),
    };
    if (api.nic_count)() <= options.nic {
        return println!("audio-net: this machine has no NIC {}.", options.nic + 1);
    }
    match mode {
        "send" => send(api, &options),
        "recv" => receive(api, &options),
        _ => println!("Usage: tape run audio-net send|recv [proto=raw|rtp] [options]   (see the tape's header for the options)"),
    }
}
