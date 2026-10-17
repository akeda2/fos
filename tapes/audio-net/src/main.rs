//! `audio-net [device=N]`: streams audio between hosts, from and to the audio device (USB audio or HD Audio).
//!
//! ```text
//! tape run audio-net                       the settings panel (see panel.rs)
//! tape run audio-net send [proto=raw|rtp] [nic=N|MAC] [to=MAC|IP[:PORT]] [rate=48000] [ch=3-4] [bits=24]
//!                         [src=capture|tone=HZ] [secs=N] [ip=A.B.C.D/N|dhcp] [gw=IP] [pt=96]
//! tape run audio-net duplex [options of both; in=CH out=CH instead of ch=]
//! tape run audio-net serve [http=8000] [prebuffer=MS] [ch=1-2] [src=capture|tone=HZ] [bitrate=KBIT] [frame=20] [ip=A.B.C.D/N|dhcp]
//!                          [secs=N]        Ogg Opus over HTTP, for a browser or VLC (see serve.rs)
//! tape run audio-net @audio.cfg [options that override the file]
//! tape run audio-net recv [proto=raw|rtp] [nic=N|MAC] [from=MAC] [port=5004] [chan=N] [ch=1-2] [target=MS]
//!                         [group=239.1.2.3] [codec=pcm|opus]
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
//! ever dropping or repeating a sample (`drift=resample`); `drift=ptp` seeds the servo from PTP stamps (AVTP); `drift=slip` instead drops or repeats one frame when
//! the level strays. A lost packet is replaced by silence of the same length. `secs=` ends either side after
//! that long; Escape always does.
//!
//! **Delay.** The receiver's summary and the panel (`Delay ~N ms`, on the Receiving line) say how late the audio is
//! when it reaches the playback ring: one packet plus the ring level; it reads counters the receiver keeps anyway,
//! so it costs nothing. The playback device's own buffer is not in it. `send latency` (a test aid) measures the
//! whole path: it sends a click every 0.5 s and times its return on the capture input, with the receiver's output
//! cabled back to that input (mind the level: use a pad). Both ends are on the capture clock, so no sync is needed.
//!
//! **Protocols.** `proto=raw` (default) is a bare Ethernet frame (`audio-wire`'s format): works between two
//! hosts on one segment with no addresses at all. `proto=rtp` is standard RTP (RFC 3550) over UDP over
//! IPv4 with 16- or 24-bit linear samples (RFC 3190 `L16`/`L24`, big endian), packet times of 125 us to 1 ms
//! as in AES67, so ordinary software (ffplay, GStreamer, VLC) and AES67 devices (Dante in AES67 mode,
//! Ravenna) can receive or send it. RTP needs an IP address: `ip=192.168.1.10/24` (and `gw=` if the peer is
//! off-subnet) or `ip=dhcp`. `to=` is the destination IP, a unicast address or a multicast group
//! (239.x.y.z, no ARP needed); `port=` (default 5004) is both the source and destination UDP port. `send`
//! prints an SDP description of the stream to open on the other side. `recv` cannot learn the channel count
//! from RTP (SDP carries it), so give `chan=` (default 2) and `bits=`. `ptp=auto|l2|off` (default auto): a
//! sender that hears a PTPv2 clock (ptp4l, a studio clock, the `ptp` tape's master) on 224.0.1.129 (raw and AVTP
//! streams, which have no IP, listen for Ethernet PTP, 0x88F7, instead; `l2` forces that for RTP) locks to it
//! before it captures, stamps RTP and raw packets on the PTP timeline (sample periods mod 2^32; AVTP: a valid
//! presentation time, capture plus 2 ms in nanoseconds) and names the grandmaster in
//! the SDP (`ts-refclk:ptp`); with no clock it says so and counts from zero. A receiver that is locked too
//! reports the sender-capture-to-arrival delay. The sound card clocks are not steered. See `clock.rs`.
//!
//! **AVTP.** `proto=avtp` sends and receives IEEE 1722 AAF (the audio format of AVB, layer 2, ethertype 0x22f0):
//! 16- or 24-bit PCM at the standard rates, `to=` a MAC (default 91:e0:f0:00:fe:01), no addresses needed. fos has no
//! gPTP clock, so the presentation time is marked invalid and a listener plays on arrival; it does not implement
//! AVB's stream reservation (MSRP) or time synchronisation, so this talks to AVTP listeners that do not insist on them.
//!
//! **Multicast.** `send to=239.1.2.3` goes to a multicast group (MAC 01:00:5e:..., no ARP). A receiver gets it with
//! `recv group=239.1.2.3`: the tape joins the group with an IGMP report (so a snooping switch forwards it to this
//! port), answers the querier's queries, and sends a leave when it stops; the NIC is told to accept all multicast
//! while it runs. `codec=opus` (see `opus.rs`) sends Opus instead of PCM. No address needs typing: an RTP `send` with
//! no `to=` (or `to=auto`) picks 239.192.x.y from its name (after 2 s of listening for groups already in use; `group=`
//! overrides), and `recv source=auto` joins the one multicast (or to-this-machine) stream that is announced. The panel
//! does both when Peer is empty.
#![no_std]
#![no_main]

mod bench;
mod clock;
mod duplex;
use tape_ip::{igmp, net};
mod panel;
mod stats;
mod peers;
mod serve;
mod opus;

/// How long `drift=ptp` measures before it starts the servo: enough samples for a few ppm, far less than the servo's own ~45 s.
const SEED_SECS: f64 = 8.0;

use audio_wire::{ChannelSet, ETHERNET_HEADER, ETHERTYPE, Fill, HEADER_LEN, Header, MAX_RAW_PAYLOAD, MAX_RTP_PAYLOAD, Resampler, Sdp, Servo, Slip, frames_to_device, pack, unpack, wire_to_frames, write_sdp};
use net::{Inbound, NetConfig};
use tape_net::{ArpCache, Ipv4Address, MacAddress, RtpPacket};
use tape_rt::{AudioInfo, AudioStats, TapeApi, print, println};

const BROADCAST: [u8; 6] = [0xff; 6];
/// The kernel's playback PCM ring is this many bytes; its free space says how full it is.
const RING_BYTES: usize = 1 << 20;

/// What to favour: the playback ring (jitter buffer) and packet time a priority stands for. `Auto` starts as Safe at
/// the receiver and steps the ring down while the stream stays clean (see `Adapt`); its sender uses Balanced packets.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    Safe,
    Balanced,
    Low,
    /// Wired, dedicated link: 3 ms ring, 125 us packets.
    Live,
    /// Worst case (Wi-Fi, powerline, a busy shared link): a 40 ms ring and 2 ms packets.
    Wifi,
    Auto,
}

impl Priority {
    pub fn parse(word: &str) -> Option<Priority> {
        match word {
            "safe" => Some(Priority::Safe),
            "balanced" => Some(Priority::Balanced),
            "low" => Some(Priority::Low),
            "live" => Some(Priority::Live),
            "wifi" => Some(Priority::Wifi),
            "auto" => Some(Priority::Auto),
            _ => None,
        }
    }

    pub fn target_ms(self) -> u32 {
        match self {
            Priority::Safe | Priority::Auto => 12,
            Priority::Balanced => 6,
            Priority::Low | Priority::Live => 3,
            Priority::Wifi => 40,
        }
    }

    pub fn packet_us(self) -> u32 {
        match self {
            Priority::Safe => 1000,
            Priority::Balanced | Priority::Auto => 500,
            Priority::Low => 250,
            Priority::Live => 125,
            Priority::Wifi => 2000,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Proto {
    Raw,
    Rtp,
    /// IEEE 1722 AAF, the AVB layer-2 audio transport.
    Avtp,
}

/// The default destination of an AVTP stream: an address in the 91:e0:f0:00:fe:xx block IEEE 1722 reserves for streams.
const AVTP_DEFAULT_MAC: [u8; 6] = [0x91, 0xe0, 0xf0, 0x00, 0xfe, 0x01];

#[derive(Clone)]
pub struct Options<'a> {
    proto: Proto,
    /// `proto=` was given (so `source=NAME` and `to=NAME` stay on that network instead of implying RTP).
    proto_explicit: bool,
    nic: usize,
    to: &'a str,
    rate: u32,
    channels: Option<ChannelSet>,
    chan: usize,
    bits: usize,
    tone: Option<u32>,
    target_ms: u32,
    /// `priority=wifi|safe|balanced|low|live|auto`: sets `target=` and `ptime=` unless they are given.
    priority: Option<Priority>,
    /// Microseconds of audio per packet (the sender's choice; the receiver follows what arrives).
    ptime_us: u32,
    target_given: bool,
    ptime_given: bool,
    seconds: u64,
    verbose: bool,
    /// `send latency`: send silence and a click once per 0.5 s, and time the click's return on the capture input.
    latency: bool,
    ip: &'a str,
    gateway: &'a str,
    port: u16,
    payload_type: u8,
    from: Option<[u8; 6]>,
    /// Clock-drift handling: a resampler steered by a servo (default) or single-frame slips.
    resample: bool,
    /// `drift=ptp`: resample, and start the servo at the rate PTP stamps measure (AVTP with a locked clock).
    ptp_seed: bool,
    /// Test aid for `send src=tone`: run the sender's clock this many ppm fast (negative: slow).
    skew_ppm: i64,
    /// `codec=opus`: PCM (default) or Opus over RTP.
    opus: bool,
    /// Opus bit rate per stereo pair, bits/s.
    bitrate: u32,
    /// Opus frame time in tenths of a millisecond (200 = 20 ms).
    frame_x10: u32,
    complexity: u32,
    /// `recv group=239.1.2.3`: join this multicast group (IGMP) and receive it.
    group: &'a str,
    /// `name=`: what this machine announces itself as; `source=NAME`: receive the stream that sender announces.
    name: &'a str,
    source: &'a str,
    /// Set when a peer's announcement replaced this machine's choice of codec for the stream it sends / receives.
    adopted_send: bool,
    adopted_receive: bool,
    /// `inmode=N` / `outmode=N`: which of the device's modes (channels, bit depth; `audio modes` lists them) to capture and play
    /// with; 0 picks automatically.
    in_mode: u32,
    out_mode: u32,
    /// `duplex`: the device input channels to send (`in=`) and the output channels to play on (`out=`).
    in_channels: Option<ChannelSet>,
    out_channels: Option<ChannelSet>,
    /// `serve`: the TCP port of the HTTP server.
    http: u16,
    /// `serve`: milliseconds of recent audio sent to a new client at once, so that its player starts with a full buffer.
    prebuffer_ms: u32,
    /// `ptp=auto|l2|off`: look for a PTP clock and stamp and time packets with it.
    ptp: bool,
    /// `ptp=l2`: PTP over Ethernet even for RTP.
    ptp_l2: bool,
    /// `bench`: the machine running `benchserve` (a MAC or an announced name), the tolerated late-or-lost share in ppm,
    /// and the seconds each step streams.
    peer: &'a str,
    tol_ppm: u32,
    step_secs: u16,
}

pub fn parse_mac(text: &str) -> Option<[u8; 6]> {
    let mut mac = [0u8; 6];
    let mut parts = text.split(':');
    for byte in mac.iter_mut() {
        *byte = u8::from_str_radix(parts.next()?, 16).ok()?;
    }
    parts.next().is_none().then_some(mac)
}

/// The zero-based number of the NIC with this MAC address (NIC numbers change when cards are moved; MACs do not).
pub fn nic_by_mac(mac: [u8; 6]) -> Option<usize> {
    let api = tape_rt::api();
    (0..(api.nic_count)()).find(|&index| {
        let mut found = [0u8; 6];
        (api.nic_mac)(index, found.as_mut_ptr());
        found == mac
    })
}

pub fn parse<'a>(words: impl Iterator<Item = &'a str>) -> Result<Options<'a>, &'static str> {
    let mut options = Options {
        proto: Proto::Raw, proto_explicit: false, nic: 0, to: "", rate: 48_000, channels: None, chan: 2, bits: 24, tone: None, target_ms: 6, priority: None, ptime_us: 1000, target_given: false, ptime_given: false, seconds: 0, verbose: false, latency: false,
        ip: "", gateway: "", port: 5004, payload_type: 96, from: None, resample: true, ptp_seed: false, skew_ppm: 0,
        opus: false, bitrate: 128_000, frame_x10: 200, complexity: 5, group: "", name: "", source: "", adopted_send: false, adopted_receive: false, in_mode: 0, out_mode: 0, in_channels: None, out_channels: None, http: 8000, prebuffer_ms: 0, ptp: true, ptp_l2: false, peer: "", tol_ppm: 100, step_secs: 10,
    };
    for word in words {
        let (key, value) = word.split_once('=').unwrap_or((word, ""));
        match key {
            "proto" => {
                options.proto = match value { "raw" => Proto::Raw, "rtp" => Proto::Rtp, "avtp" => Proto::Avtp, _ => return Err("proto= is raw, rtp or avtp") };
                options.proto_explicit = true;
            }
            "nic" => options.nic = match parse_mac(value) {
                Some(mac) => nic_by_mac(mac).ok_or("nic=MAC: no NIC in this machine has that MAC address (`nic list` shows them)")?,
                None => value.parse::<usize>().ok().filter(|&n| n >= 1).ok_or("nic= takes a number from 1 or a MAC address")? - 1,
            },
            "to" => options.to = value,
            "from" => options.from = Some(parse_mac(value).ok_or("from= takes a MAC address like 00:11:22:33:44:55 (a machine's name goes in source=NAME)")?),
            "rate" => options.rate = value.parse().map_err(|_| "rate= needs a number")?,
            "ch" => options.channels = Some(ChannelSet::parse(value).ok_or("ch= is like 1-2 or 9-10,1 (channels 1 to 32)")?),
            "chan" => options.chan = value.parse().ok().filter(|&n| (1..=audio_wire::MAX_CHANNELS).contains(&n)).ok_or("chan= is 1 to 32")?,
            "bits" => options.bits = value.parse().ok().filter(|&b| b == 16 || b == 24).ok_or("bits= is 16 or 24")?,
            "src" => match value.strip_prefix("tone=") {
                Some(frequency) => options.tone = Some(frequency.parse().map_err(|_| "src=tone=HZ needs a number")?),
                None if value == "capture" => {}
                None => return Err("src= is capture or tone=HZ"),
            },
            "target" => {
                options.target_ms = value.parse().map_err(|_| "target= needs milliseconds")?;
                options.target_given = true;
            }
            "priority" => options.priority = Some(Priority::parse(value).ok_or("priority= is wifi, safe, balanced, low, live or auto")?),
            "ptime" => {
                options.ptime_us = value.parse().ok().filter(|&us| (125..=4000).contains(&us)).ok_or("ptime= is the packet time in microseconds, 125 to 4000")?;
                options.ptime_given = true;
            }
            "secs" => options.seconds = value.parse().map_err(|_| "secs= needs a number")?,
            "ip" => options.ip = value,
            "gw" => options.gateway = value,
            "port" => options.port = value.parse().map_err(|_| "port= needs a number")?,
            "pt" => options.payload_type = value.parse().ok().filter(|&p| p < 128).ok_or("pt= is 0 to 127")?,
            "drift" => {
                options.resample = match value { "resample" | "ptp" => true, "slip" => false, _ => return Err("drift= is resample, slip or ptp") };
                options.ptp_seed = value == "ptp";
            }
            "skew" => options.skew_ppm = value.parse().map_err(|_| "skew= needs a number of ppm")?,
            "in" => options.in_channels = Some(ChannelSet::parse(value).ok_or("in= is like 1-2 or 9-10,1")?),
            "out" => options.out_channels = Some(ChannelSet::parse(value).ok_or("out= is like 1-2 or 9-10,1")?),
            "group" => options.group = value,
            "name" => options.name = Some(value).filter(|name| !name.is_empty() && name.len() <= 24 && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')).ok_or("name= is up to 24 letters, digits, - _ .")?,
            "source" => options.source = value,
            "inmode" => options.in_mode = value.parse().map_err(|_| "inmode= needs a mode number (0 = automatic)")?,
            "outmode" => options.out_mode = value.parse().map_err(|_| "outmode= needs a mode number (0 = automatic)")?,
            "codec" => options.opus = match value { "pcm" => false, "opus" => true, _ => return Err("codec= is pcm or opus") },
            "bitrate" => options.bitrate = value.parse::<u32>().ok().filter(|&b| (6..=512).contains(&b)).ok_or("bitrate= is kbit/s per stereo pair, 6 to 512")? * 1000,
            "frame" => options.frame_x10 = match value.split_once('.') { Some((whole, tenth)) => whole.parse::<u32>().ok().zip(tenth.parse::<u32>().ok()).map(|(w, t)| w * 10 + t), None => value.parse::<u32>().ok().map(|w| w * 10) }.ok_or("frame= is milliseconds")?,
            "complexity" => options.complexity = value.parse().ok().filter(|&c| c <= 10).ok_or("complexity= is 0 to 10")?,
            "http" => options.http = value.parse().ok().filter(|&p| p != 0).ok_or("http= is a TCP port, 1 to 65535")?,
            "prebuffer" => options.prebuffer_ms = value.parse().ok().filter(|&ms| ms <= 1000).ok_or("prebuffer= is 0 to 1000 milliseconds")?,
            "peer" => options.peer = value,
            "tol" => options.tol_ppm = bench::parse_percent_ppm(value).ok_or("tol= is a percentage from 0 to 5, like 0.01")?,
            "stepsecs" => options.step_secs = value.parse().ok().filter(|&s| (2..=60).contains(&s)).ok_or("stepsecs= is 2 to 60 seconds")?,
            "verbose" => options.verbose = true,
            "latency" => options.latency = true,
            "ptp" => {
                options.ptp = match value { "auto" | "on" | "l2" => true, "off" => false, _ => return Err("ptp= is auto, l2 or off") };
                options.ptp_l2 = value == "l2";
            }
            _ => return Err("unknown option"),
        }
    }
    if let Some(priority) = options.priority {
        if !options.target_given {
            options.target_ms = priority.target_ms();
        }
        if !options.ptime_given {
            options.ptime_us = priority.packet_us();
        }
    }
    Ok(options)
}

/// A parabola per half cycle standing in for a sine (integer only): a quarter of full scale, 16 bits.
pub fn tone_sample(phase: u32) -> i32 {
    let half = i64::from(phase & 0x7fff);
    let magnitude = (half * (0x8000 - half)) >> 13;
    (if phase & 0x8000 != 0 { -magnitude } else { magnitude } / 4) as i32
}

/// Everything the network side needs: raw needs only a MAC; RTP a full IPv4 configuration.
/// A MAC address as text, for messages: the user needs the MAC and the IP address to troubleshoot.
pub struct MacText(pub [u8; 6]);

impl core::fmt::Display for MacText {
    fn fmt(&self, formatter: &mut core::fmt::Formatter) -> core::fmt::Result {
        let m = self.0;
        write!(formatter, "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", m[0], m[1], m[2], m[3], m[4], m[5])
    }
}

/// The address configuration a run settled on (NIC, config), so that a retry or the second task does not repeat DHCP.
static mut CONFIG_CACHE: Option<(usize, NetConfig)> = None;

/// Forgets the settled address configuration (the settings changed, so the next `open` starts afresh).
pub fn forget_config() {
    unsafe { CONFIG_CACHE = None };
}

/// The address the run is using (after a DHCP lease or from `ip=`), once known.
pub fn current_ip() -> Option<[u8; 4]> {
    unsafe { (*(&raw const CONFIG_CACHE)).map(|(_, config)| config.ip.0) }
}

pub struct Transport {
    proto: Proto,
    mac: [u8; 6],
    config: NetConfig,
    arp: ArpCache<8>,
    port: u16,
    payload_type: u8,
    igmp: Option<igmp::Igmp>,
    /// AVTP's 8-bit sequence number and its widened value, for the receiver.
    avtp_sequence: Option<(u8, u16)>,
    announcer: peers::Announcer,
    /// This transport started the PTP clock (and must stop it).
    ptp: bool,
}

impl Transport {
    pub fn open(api: &TapeApi, options: &Options) -> Option<Transport> {
        let mut mac = [0u8; 6];
        (api.nic_mac)(options.nic, mac.as_mut_ptr());
        let mut config = NetConfig { mac: MacAddress(mac), ip: Ipv4Address([0; 4]), netmask: Ipv4Address([0; 4]), gateway: Ipv4Address([0; 4]) };
        if let (Some((nic, cached)), Proto::Rtp) = (unsafe { *(&raw const CONFIG_CACHE) }, options.proto) {
            // A retry, or the second task of a duplex run: the address is already settled (no second DHCP exchange).
            if nic == options.nic {
                return Some(Transport { proto: options.proto, mac, config: cached, arp: ArpCache::new(), port: options.port, payload_type: options.payload_type, igmp: None, avtp_sequence: None, announcer: peers::Announcer::idle(), ptp: false });
            }
        }
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
            println!("Network: {}.{}.{}.{} on NIC {} (MAC {}).", ip[0], ip[1], ip[2], ip[3], options.nic + 1, MacText(mac));
            unsafe { CONFIG_CACHE = Some((options.nic, config)) };
        } else {
            println!("Network: NIC {} (MAC {}), layer 2 only, no IP address.", options.nic + 1, MacText(mac));
        }
        Some(Transport { proto: options.proto, mac, config, arp: ArpCache::new(), port: options.port, payload_type: options.payload_type, igmp: None, avtp_sequence: None, announcer: peers::Announcer::idle(), ptp: false })
    }

    /// A transport on an address configuration that is already settled (the panel's presence announcements);
    /// remembers it so the stream started from the panel does not repeat DHCP.
    pub fn from_config(options: &Options, mac: [u8; 6], config: NetConfig) -> Transport {
        unsafe { CONFIG_CACHE = Some((options.nic, config)) };
        Transport { proto: Proto::Rtp, mac, config, arp: ArpCache::new(), port: options.port, payload_type: options.payload_type, igmp: None, avtp_sequence: None, announcer: peers::Announcer::idle(), ptp: false }
    }

    /// A transport on a layer 2 network with nothing printed (the panel's presence announcements).
    pub fn from_l2(options: &Options, mac: [u8; 6]) -> Transport {
        let config = NetConfig { mac: MacAddress(mac), ip: Ipv4Address([0; 4]), netmask: Ipv4Address([0; 4]), gateway: Ipv4Address([0; 4]) };
        Transport { proto: options.proto, mac, config, arp: ArpCache::new(), port: options.port, payload_type: options.payload_type, igmp: None, avtp_sequence: None, announcer: peers::Announcer::idle(), ptp: false }
    }

    /// `group=`: joins the multicast group on the receive NIC. False (after a message) if it cannot.
    pub fn join_group(&mut self, api: &TapeApi, options: &Options) -> bool {
        if options.proto == Proto::Avtp {
            (api.nic_set_multicast)(options.nic, 1); // AVTP streams go to multicast MAC addresses
            return true;
        }
        if options.group.is_empty() {
            return true;
        }
        let Some(group) = net::parse_ip(options.group).filter(|group| audio_wire::is_multicast_address(group.0)) else {
            println!("audio-net: group= is a multicast address (224.0.0.0 to 239.255.255.255).");
            return false;
        };
        if options.proto != Proto::Rtp {
            println!("audio-net: group= needs proto=rtp.");
            return false;
        }
        let mut igmp = igmp::Igmp::new(group.0);
        igmp.join(api, options.nic, &self.config);
        println!("Joined multicast group {} (IGMP report sent).", options.group);
        self.igmp = Some(igmp);
        true
    }

    /// Per loop pass: sends this stream's announcement when due.
    pub fn announce_service(&mut self, api: &TapeApi, nic: usize) {
        self.announcer.poll(api, nic, &self.config);
    }

    /// Withdraws the announcement (the tape is stopping).
    pub fn announcer_start(&mut self, sdp: &[u8]) {
        self.announcer.start(sdp);
    }

    pub fn announce_stop(&mut self, api: &TapeApi, nic: usize) {
        self.announcer.withdraw(api, nic, &self.config);
    }

    /// A receiver announces where and how it listens (RTP only).
    pub fn announce_receiver(&mut self, options: &Options) {
        let mut buffer = [0u8; audio_wire::sap::MAX_NAME];
        let name = peers::session_name(options.name, self.mac, &mut buffer);
        let mut text = [0u8; 900];
        if options.proto != Proto::Rtp {
            // Layer 2: the same announcement in an Ethernet frame (see peers.rs).
            let length = peers::l2_sdp(options, self.mac, name, true, false, options.chan, &mut text);
            self.announcer.start(&text[..length]);
            return;
        }
        let length = peers::receiver_sdp(options, &self.config, name, false, &mut text);
        self.announcer.start(&text[..length]);
    }

    /// Per loop pass: sends a due IGMP answer and, given a frame, consumes it if it is IGMP. True if consumed.
    pub fn igmp_service(&mut self, api: &TapeApi, nic: usize, frame: Option<&[u8]>) -> bool {
        match frame {
            None => {
                self.announce_service(api, nic);
                clock::poll(api);
            }
            Some(frame) => {
                if clock::consume(api, frame, tape_rt::micros()) {
                    return true;
                }
            }
        }
        let Some(igmp) = self.igmp.as_mut() else { return false };
        let now = tape_rt::micros();
        igmp.poll(api, nic, &self.config, now);
        frame.is_some_and(|frame| igmp.handle(frame, now))
    }

    /// `ptp=auto`: starts listening for a PTP clock (UDP for RTP, Ethernet for raw and AVTP).
    pub fn start_clock(&mut self, api: &TapeApi, options: &Options) {
        if options.ptp && !self.ptp {
            clock::start(api, options.nic, &self.config, self.proto != Proto::Rtp || options.ptp_l2);
            self.ptp = true;
        }
    }

    pub fn stop_clock(&mut self, api: &TapeApi, nic: usize) {
        if self.ptp {
            self.ptp = false;
            clock::stop(api, nic, self.igmp.is_some() || self.proto == Proto::Avtp);
        }
    }

    pub fn leave_group(&mut self, api: &TapeApi, nic: usize) {
        self.announce_stop(api, nic);
        self.stop_clock(api, nic);
        if self.proto == Proto::Avtp {
            (api.nic_set_multicast)(nic, 0);
        }
        if let Some(mut igmp) = self.igmp.take() {
            println!("Left the multicast group ({} queries answered, {} reports sent).", igmp.queries, igmp.reports);
            igmp.leave(api, nic, &self.config);
        }
    }
}

/// The sending side's destination, resolved once before streaming.
enum Destination {
    Raw([u8; 6]),
    Rtp { mac: MacAddress, ip: Ipv4Address, ttl: u8 },
}

pub fn send(api: &'static TapeApi, options: &Options) {
    let tone = options.tone;
    if options.in_mode != 0 {
        (api.audio_select)(0, options.in_mode);
    }
    let mut info = AudioInfo::default();
    let device = (api.audio_info)(&mut info) != 0 && info.capture_channels != 0;
    if options.latency && tone.is_some() {
        return println!("audio-net send: latency needs the capture device (no src=tone).");
    }
    if tone.is_none() && !device {
        return println!("audio-net send: no audio device with inputs (use src=tone=440 to send a test tone).");
    }
    let (mut device_channels, mut slot) = (info.capture_channels as usize, info.capture_slot_bytes as usize);
    let selection = options.channels.unwrap_or(ChannelSet(0b11));
    let channels = selection.count();
    if tone.is_none() && selection.channels().any(|channel| channel >= device_channels) {
        return println!("audio-net send: the device has {device_channels} input channels; ch= asks for more.");
    }
    let bits = options.bits;
    let Some(mut transport) = Transport::open(api, options) else { return };
    let max_payload = match options.proto {
        Proto::Rtp => MAX_RTP_PAYLOAD,
        Proto::Raw => MAX_RAW_PAYLOAD,
        Proto::Avtp => 1500 - audio_wire::avtp::HEADER_LEN,
    };
    let per_packet = audio_wire::frames_per_packet_within(options.rate, channels, bits, max_payload, options.ptime_us);
    stats::get().rate = options.rate;
    let frame_bytes = channels * bits / 8;
    let packet_us = u64::from(per_packet as u32) * 1_000_000 / u64::from(options.rate);
    // `skew=` (tone source only) paces packets as if the sender's clock were off by that many ppm.
    let paced_us_x1000 = (packet_us as i64 * 1000 * (1_000_000 - options.skew_ppm) / 1_000_000) as u64;
    let destination = match options.proto {
        Proto::Raw => Destination::Raw(if options.to.is_empty() { BROADCAST } else { match parse_mac(options.to) { Some(mac) => mac, None => return println!("audio-net send: to= takes a MAC address for proto=raw.") } }),
        Proto::Avtp => Destination::Raw(if options.to.is_empty() { AVTP_DEFAULT_MAC } else { match parse_mac(options.to) { Some(mac) => mac, None => return println!("audio-net send: to= takes a MAC address for proto=avtp.") } }),
        Proto::Rtp => {
            let (host, _) = options.to.split_once(':').unwrap_or((options.to, ""));
            let Some(ip) = net::parse_ip(host) else { return println!("audio-net send: proto=rtp needs to=IP (a unicast address or a multicast group).") };
            let Some(mac) = net::resolve(api, options.nic, &transport.config, &mut transport.arp, ip) else {
                return println!("audio-net send: no answer from {host} (ARP); is it on this subnet, or is gw= set?");
            };
            println!("Sending to {host}, MAC {}.", MacText(mac.0));
            Destination::Rtp { mac, ip, ttl: if audio_wire::is_multicast_address(ip.0) { 32 } else { 64 } }
        }
    };
    // On a network with a PTP clock the packets are stamped with its time (before capture starts: waiting for the lock
    // takes seconds, and the capture buffer would overflow).
    let mut stamper = None;
    transport.start_clock(api, options);
    if transport.ptp {
        if clock::wait_for_lock(api) {
            println!("PTP: locked to a clock ({} us path delay); timestamps are on its timeline.", clock::path_delay_us().unwrap_or(0));
            stamper = Some(clock::Stamper::new(options.rate));
        } else {
            println!("PTP: {}; timestamps count from zero (ptp=off silences this).", if clock::found() { "a clock was heard but did not lock" } else { "no clock found" });
            transport.stop_clock(api, options.nic);
        }
    }
    if tone.is_none() {
        let status = (api.audio_start)(options.rate, 1, 0);
        if status != 0 {
            transport.stop_clock(api, options.nic);
            return println!("audio-net send: cannot start capture at {} Hz (code {status}); the device offers {:?}.", options.rate, &info.rates[..info.rate_count as usize]);
        }
        // The mode that runs is chosen when the stream starts (automatically: the best one that offers the rate), so ask again.
        (api.audio_info)(&mut info);
        (device_channels, slot) = (info.capture_channels as usize, info.capture_slot_bytes as usize);
        if selection.channels().any(|channel| channel >= device_channels) {
            (api.audio_stop)();
            transport.stop_clock(api, options.nic);
            return println!("audio-net send: the capture mode at {} Hz has {device_channels} channel(s); ch= asks for more (`audio modes` lists the modes, inmode= picks one).", options.rate);
        }
    }
    println!(
        "Sending {channels} channel(s), {bits}-bit, {} Hz, {per_packet} frames ({packet_us} us) per packet, from {} over {}.",
        options.rate,
        if tone.is_some() { "a test tone" } else { "the capture device" },
        proto_name(options.proto)
    );
    stats::set_send(format_args!("PCM {bits}-bit, {channels} ch{}", if options.adopted_send { " (peer's choice)" } else { "" }));
    let stream = (tape_rt::micros() as u32) ^ 0x5a5a_a5a5;
    if let Destination::Rtp { ip, .. } = destination {
        let mut text = [0u8; 600];
        let mut name_buffer = [0u8; audio_wire::sap::MAX_NAME];
        let name = peers::session_name(options.name, transport.mac, &mut name_buffer);
        let length = write_sdp(&Sdp { name, receive_only: false, idle: false, origin: transport.config.ip.0, destination: ip.0, port: options.port, payload_type: options.payload_type, channels, rate: options.rate, bits, packet_us: packet_us as u32, mac: transport.mac, ptp: stamper.as_ref().and_then(|_| clock::grandmaster()), session: stream }, &mut text);
        println!("SDP for the receiver (save as stream.sdp; e.g. ffplay -protocol_whitelist file,udp,rtp stream.sdp):");
        println!("{}", core::str::from_utf8(&text[..length]).unwrap_or(""));
        transport.announcer.start(&text[..length]);
        println!("Announcing this stream as '{name}' (SAP, every 5 s).");
    }
    if let Destination::Raw(_) = destination {
        // Layer 2: announce what is sent, so that a receiver can pick this machine by name.
        let mut text = [0u8; 600];
        let mut name_buffer = [0u8; audio_wire::sap::MAX_NAME];
        let name = peers::session_name(options.name, transport.mac, &mut name_buffer);
        let length = peers::l2_sdp(options, transport.mac, name, false, false, channels, &mut text);
        transport.announcer.start(&text[..length]);
        println!("Announcing this stream as '{name}' (layer 2, every 5 s).");
    }
    duplex::go();
    let mut frame = [0u8; 1514];
    let mut payload = [0u8; 1514];
    let mut ptp_frame = [0u8; 1514];
    let mut age_frames = 0u32;
    let mut capture = [0u8; 16384];
    let mut held = 0usize; // bytes of device-format audio waiting in `capture`
    let device_frame = device_channels * slot;
    let (mut sequence, mut timestamp, mut sent, mut phase) = (0u16, 0u32, 0u64, 0u32);
    let started = tape_rt::micros();
    let mut next_report = started + 1_000_000;
    let mut probe = LatencyProbe::new(options.rate, per_packet);
    if options.latency {
        println!("Latency probe: sending silence and a click every 0.5 s; the receiver's output must be cabled to input channel {} here.", selection.channels().next().unwrap_or(0) + 1);
    }
    // The banner and prints above took a while to print and capture has been running: audio from before now would go out as one
    // burst that the receiver's ring cannot hold.
    if tone.is_none() {
        while (api.audio_read)(capture.as_mut_ptr(), capture.len()) != 0 {}
    }
    loop {
        if (api.should_abort)() != 0 {
            break;
        }
        if options.seconds != 0 && tape_rt::micros() >= started + options.seconds * 1_000_000 {
            break;
        }
        transport.announce_service(api, options.nic);
        if transport.ptp {
            if duplex::shared() { clock::poll(api) } else { clock::pump(api, &mut ptp_frame) }
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
                age_frames = (held / device_frame) as u32;
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
        if options.latency {
            probe.packet(sent, &mut payload[..length], channels, bits);
        }
        stats::peaks_from_wire(&payload[..length], channels, bits, per_packet, true);
        let total = match destination {
            Destination::Raw(mac) if options.proto == Proto::Avtp => {
                frame[0..6].copy_from_slice(&mac);
                frame[6..12].copy_from_slice(&transport.mac);
                frame[12..14].copy_from_slice(&audio_wire::avtp::ETHERTYPE.to_be_bytes());
                // Stream id: our MAC and a unique id. With a PTP clock the presentation time is valid (capture time plus
                // 2 ms on that clock's timeline); without one tv = 0 and a listener plays on arrival, as `recv` does.
                let mut stream_id = [0u8; 8];
                stream_id[..6].copy_from_slice(&transport.mac);
                stream_id[6..].copy_from_slice(&1u16.to_be_bytes());
                let presentation = stamper.as_ref().and_then(|stamper| stamper.presentation(age_frames));
                let header = audio_wire::avtp::Header { sequence: sequence as u8, timestamp_valid: presentation.is_some(), stream_id, timestamp: presentation.unwrap_or(0), channels: channels as u16, bits: bits as u8, rate: options.rate, data_length: length as u16 };
                if !header.encode(&mut frame[ETHERNET_HEADER..]) {
                    return println!("audio-net send: AVTP has no code for {} Hz.", options.rate);
                }
                let start = ETHERNET_HEADER + audio_wire::avtp::HEADER_LEN;
                frame[start..start + length].copy_from_slice(&payload[..length]);
                Some(start + length)
            }
            Destination::Raw(mac) => {
                frame[0..6].copy_from_slice(&mac);
                frame[6..12].copy_from_slice(&transport.mac);
                frame[12..14].copy_from_slice(&ETHERTYPE.to_be_bytes());
                let stamp = stamper.as_mut().and_then(|stamper| stamper.stamp(age_frames, per_packet as u32)).unwrap_or(timestamp);
                Header { first: sent == 0, sequence, timestamp: stamp, stream, channels: channels as u8, bits: bits as u8, frames: per_packet as u16, rate: options.rate }.encode(&mut frame[ETHERNET_HEADER..]);
                frame[ETHERNET_HEADER + HEADER_LEN..ETHERNET_HEADER + HEADER_LEN + length].copy_from_slice(&payload[..length]);
                Some(ETHERNET_HEADER + HEADER_LEN + length)
            }
            Destination::Rtp { mac, ip, ttl } => {
                let stamp = stamper.as_mut().and_then(|stamper| stamper.stamp(age_frames, per_packet as u32)).unwrap_or(timestamp);
                let rtp = RtpPacket::new(transport.payload_type, false, sequence, stamp, stream, &payload[..length]);
                net::build_rtp_frame(&mut frame, &transport.config, mac, ip, transport.port, ttl, &rtp)
            }
        };
        if let Some(total) = total {
            (api.nic_send)(options.nic, frame.as_ptr(), total);
        }
        sequence = sequence.wrapping_add(1);
        timestamp = timestamp.wrapping_add(per_packet as u32);
        sent += 1;
        stats::get().sent = sent;
        if tape_rt::micros() >= next_report {
            next_report += 1_000_000;
            print!(".");
        }
    }
    if tone.is_none() {
        (api.audio_stop)();
    }
    println!();
    transport.announce_stop(api, options.nic);
    transport.stop_clock(api, options.nic);
    println!("Sent {sent} packets ({} frames).", sent * per_packet as u64);
    if options.latency {
        probe.summary();
    }
}

/// `send latency`: measures the delay from the moment a sample is handed to the network to the moment it is
/// heard again on this machine's capture input, once the receiver's output is cabled back to that input
/// (loopback). Both ends of the measurement are positions on the capture clock (frames read from the device),
/// so the two machines need no common clock. The click is written over the packet that would have carried
/// captured audio, and the packet's own capture position is the start time; the return is the first sample in
/// a later packet above the noise floor. The result is the network, the receiver's ring and playback buffers,
/// the cable and this machine's capture buffering on the way back: capture buffering on the way out is not in it.
struct LatencyProbe {
    rate: u32,
    per_packet: u32,
    noise: i32,
    noise_packets: u32,
    /// Packet number at which the click went out, while waiting for it to come back.
    waiting: Option<u64>,
    /// Packets of the click still to send after its first.
    click_left: u32,
    /// Packet counts for the time limits below, whatever the packet time.
    noise_window: u32,
    warm_up: u64,
    period: u64,
    click_packets: u32,
    min_return: u64,
    give_up: u64,
    sum_x100: u64,
    count: u32,
    min: u32,
    max: u32,
    missed: u32,
    /// Every click's latency (ms x 100) and the noise threshold, kept for the summary: a print in the send loop
    /// stalls it, the capture backlog goes out as a burst and the receiver underruns.
    results: [u32; 64],
    threshold: i32,
}

impl LatencyProbe {
    fn new(rate: u32, per_packet: usize) -> Self {
        let per_second = u64::from(rate) / per_packet.max(1) as u64;
        let ms = |ms: u64| (per_second * ms / 1000).max(1);
        Self {
            rate, per_packet: per_packet as u32, noise: 0, noise_packets: 0, waiting: None, click_left: 0,
            noise_window: ms(300) as u32, // listen to the input first
            warm_up: ms(3000), // no click before the receiver has locked on and filled its ring
            period: ms(500),
            click_packets: ms(4) as u32, // a click 4 ms long
            min_return: ms(5), // nothing comes back sooner than this: it would be noise
            give_up: ms(400),
            sum_x100: 0, count: 0, min: u32::MAX, max: 0, missed: 0, results: [0; 64], threshold: 0,
        }
    }

    fn sample(payload: &[u8], bits: usize, index: usize) -> i32 {
        let at = index * bits / 8;
        i32::from(i16::from_be_bytes([payload[at], payload[at + 1]]))
    }

    /// Called with each packet of captured audio before it is sent; leaves silence or the click in `payload`.
    fn packet(&mut self, number: u64, payload: &mut [u8], channels: usize, bits: usize) {
        let frames = self.per_packet as usize;
        let mut first_loud = None;
        let mut peak = 0;
        for frame in 0..frames {
            for channel in 0..channels {
                let level = Self::sample(payload, bits, frame * channels + channel).abs();
                peak = peak.max(level);
                if first_loud.is_none() && self.noise_packets >= self.noise_window && level > self.noise {
                    first_loud = Some(frame);
                }
            }
        }
        if self.noise_packets < self.noise_window {
            self.noise_packets += 1;
            self.noise = self.noise.max(peak);
            if self.noise_packets == self.noise_window {
                self.noise = (self.noise * 4).max(100);
                self.threshold = self.noise;
            }
        } else if let (Some(sent_at), Some(frame)) = (self.waiting.filter(|&at| number > at + self.min_return), first_loud) {
            let frames = (number - sent_at) * u64::from(self.per_packet) + frame as u64;
            let x100 = (frames * 100_000 / u64::from(self.rate)) as u32; // ms, two decimals
            self.waiting = None;
            self.sum_x100 += u64::from(x100);
            self.count += 1;
            self.min = self.min.min(x100);
            self.max = self.max.max(x100);
            if let Some(slot) = self.results.get_mut(self.count as usize - 1) {
                *slot = x100;
            }
        } else if let Some(sent_at) = self.waiting {
            if number - sent_at > self.give_up {
                self.waiting = None;
                self.missed += 1;
            }
        }
        payload.fill(0);
        if self.noise_packets >= self.noise_window && self.waiting.is_none() && self.click_left == 0 && number >= self.warm_up && number % self.period == 0 {
            self.waiting = Some(number);
            self.click_left = self.click_packets;
        }
        if self.click_left > 0 {
            self.click_left -= 1;
            self.fill_click(payload, channels, bits);
        }
    }

    fn fill_click(&self, payload: &mut [u8], channels: usize, bits: usize) {
        let frames = payload.len() / (channels * bits / 8);
        for frame in 0..frames {
            // A 1 kHz square wave at about 70 % of full scale.
            let high = (frame * 2000 / self.rate as usize) % 2 == 0;
            let bytes = (if high { 23_000i16 } else { -23_000 }).to_be_bytes();
            for channel in 0..channels {
                let at = (frame * channels + channel) * bits / 8;
                payload[at] = bytes[0];
                payload[at + 1] = bytes[1];
            }
        }
    }

    fn summary(&self) {
        println!("Noise floor on the input: peak {} (of 32768), threshold {}.", self.threshold / 4, self.threshold);
        for x100 in self.results.iter().take(self.count as usize) {
            println!("Latency {}.{:02} ms", x100 / 100, x100 % 100);
        }
        if self.missed > 0 {
            println!("{} click(s) did not come back within 400 ms (level too low, or the cable is not connected).", self.missed);
        }
        if self.count == 0 {
            return println!("Latency: no click came back ({} missed).", self.missed);
        }
        let average = (self.sum_x100 / u64::from(self.count)) as u32;
        println!("Latency over {} clicks: min {}.{:02} ms, average {}.{:02} ms, max {}.{:02} ms ({} missed).", self.count, self.min / 100, self.min % 100, average / 100, average % 100, self.max / 100, self.max % 100, self.missed);
    }
}

/// `priority=auto`: the receiver's ring target steps down (12, 6, 3 ms) after 30 s with no lost packet and no
/// underrun, and back up one step at the first of either, holding at that step for a minute. Only the receiver's
/// jitter buffer is adapted: the sender's packet time is its own choice.
struct Adapt {
    step: usize,
    clean_since: u64,
    hold_until: u64,
    lost_mark: u64,
    underrun_mark: u64,
}

impl Adapt {
    const TARGETS_MS: [u32; 3] = [12, 6, 3];
    const CLEAN_US: u64 = 30_000_000;
    const HOLD_US: u64 = 60_000_000;

    fn new(now: u64, lost: u64, underruns: u64) -> Adapt {
        Adapt { step: 0, clean_since: now, hold_until: 0, lost_mark: lost, underrun_mark: underruns }
    }

    /// Looks at the counters; returns a new target in ms when it should change.
    fn poll(&mut self, now: u64, lost: u64, underruns: u64) -> Option<u32> {
        if lost > self.lost_mark || underruns > self.underrun_mark {
            (self.lost_mark, self.underrun_mark, self.clean_since) = (lost, underruns, now);
            if self.step > 0 {
                self.step -= 1;
                self.hold_until = now + Self::HOLD_US;
                return Some(Self::TARGETS_MS[self.step]);
            }
        } else if self.step + 1 < Self::TARGETS_MS.len() && now >= self.clean_since + Self::CLEAN_US && now >= self.hold_until {
            self.step += 1;
            self.clean_since = now;
            return Some(Self::TARGETS_MS[self.step]);
        }
        None
    }
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
    /// RTP's timestamp (the sender's PTP time if it follows one); None for the other protocols.
    rtp_timestamp: Option<u32>,
    /// AVTP's presentation time (nanoseconds mod 2^32), when the sender marked it valid.
    avtp_time: Option<u32>,
}

fn proto_name(proto: Proto) -> &'static str {
    match proto {
        Proto::Raw => "raw Ethernet",
        Proto::Rtp => "RTP/UDP",
        Proto::Avtp => "AVTP (IEEE 1722 AAF)",
    }
}

/// Classifies one frame for the active protocol. For RTP this also answers ARP.
fn classify(api: &TapeApi, options: &Options, transport: &mut Transport, frame: &[u8]) -> Option<Packet> {
    match transport.proto {
        Proto::Avtp => {
            if frame.len() < ETHERNET_HEADER + audio_wire::avtp::HEADER_LEN || frame[12..14] != audio_wire::avtp::ETHERTYPE.to_be_bytes() {
                return None;
            }
            let header = audio_wire::avtp::Header::parse(&frame[ETHERNET_HEADER..])?;
            let sender: [u8; 6] = frame[6..12].try_into().ok()?;
            if options.from.is_some_and(|wanted| wanted != sender) {
                return None;
            }
            // The sequence number is 8 bits on the wire; widen it so the receiver's gap logic (16 bits) sees no
            // false jump at the wrap.
            let widened = match transport.avtp_sequence {
                None => u16::from(header.sequence),
                Some((last, wide)) => {
                    let step = header.sequence.wrapping_sub(last);
                    if step < 128 { wide.wrapping_add(u16::from(step)) } else { wide.wrapping_sub(256 - u16::from(step)) }
                }
            };
            transport.avtp_sequence = Some((header.sequence, widened));
            let mut source = [0u8; 8];
            source[..6].copy_from_slice(&sender);
            let frame_bytes = usize::from(header.channels) * usize::from(header.bits) / 8;
            let start = ETHERNET_HEADER + audio_wire::avtp::HEADER_LEN;
            let id = header.stream_id;
            Some(Packet {
                stream: u32::from_be_bytes([id[4], id[5], id[6], id[7]]),
                source,
                sequence: widened,
                channels: usize::from(header.channels),
                bits: usize::from(header.bits),
                rate: header.rate,
                frames: usize::from(header.data_length) / frame_bytes,
                payload: start..start + usize::from(header.data_length),
                rtp_timestamp: None,
                avtp_time: header.timestamp_valid.then_some(header.timestamp),
            })
        }
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
            Some(Packet { stream: header.stream, source, sequence: header.sequence, channels: usize::from(header.channels), bits: usize::from(header.bits), rate: header.rate, frames: usize::from(header.frames), payload: start..start + header.payload_len(), rtp_timestamp: Some(header.timestamp), avtp_time: None })
        }
        Proto::Rtp => {
            let now = (api.ticks)() as u32;
            let (source, payload) = match net::inbound(api, options.nic, &transport.config, &mut transport.arp, frame, transport.port..=transport.port, now) {
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
            Some(Packet { stream: rtp.ssrc, source: id, sequence: rtp.sequence_number, channels: options.chan, bits: options.bits, rate: options.rate, frames, payload: start..start + frames * frame_bytes, rtp_timestamp: Some(rtp.timestamp), avtp_time: None })
        }
    }
}

pub fn receive(api: &'static TapeApi, options: &Options) {
    if options.out_mode != 0 {
        (api.audio_select)(1, options.out_mode);
    }
    let mut info = AudioInfo::default();
    if (api.audio_info)(&mut info) == 0 || info.playback_channels == 0 {
        return println!("audio-net recv: no audio device with outputs.");
    }
    let Some(mut transport) = Transport::open(api, options) else { return };
    if !transport.join_group(api, options) {
        return;
    }
    // Start playback now, at `rate=` (default 48000): starting a USB stream takes a while (control requests,
    // Configure Endpoint), and doing it when the first packet arrives made the frames that came meanwhile pile
    // up and overflow. A stream at another rate restarts it, once.
    let mut running_rate = options.rate;
    let status = (api.audio_start)(running_rate, 0, 1);
    if status != 0 {
        return println!("audio-net recv: cannot start playback at {running_rate} Hz (code {status}); the device offers {:?}.", &info.rates[..info.rate_count as usize]);
    }
    // The mode that runs is chosen when the stream starts, so ask again what it is.
    (api.audio_info)(&mut info);
    let (device_channels, slot) = (info.playback_channels as usize, info.playback_slot_bytes as usize);
    let device_frame = device_channels * slot;
    stats::set_receive(format_args!("waiting for PCM {}-bit{}", options.bits, if options.adopted_receive { " (peer's choice)" } else { "" }));
    transport.announce_receiver(options);
    transport.start_clock(api, options);
    println!("Listening on NIC {} for {} audio; playing on {device_channels} output channel(s) of {}-bit. Escape stops.", options.nic + 1, proto_name(options.proto), info.playback_bits);
    let mut frame = [0u8; 1514];
    let mut staging = [0u8; 8192];
    let mut locked: Option<(u32, [u8; 8])> = None;
    let mut base_taken = false;
    let (mut expected, mut running) = (0u16, true);
    let (mut packets, mut lost, mut dropped, mut repeated, mut foreign) = (0u64, 0u64, 0u64, 0u64, 0u64);
    let mut fill = Fill::new(0);
    let mut servo = Servo::new(0);
    let mut adapt: Option<Adapt> = None;
    let mut resampler = Resampler::new(2);
    // drift=ptp: the sender's frame rate against its AVTP capture stamps, the card's played frames against PTP time.
    let (mut sender_fit, mut device_fit) = (audio_wire::RateFit::new(), audio_wire::RateFit::new());
    let (mut sender_time, mut sender_frames, mut previous_stamp, mut written_out) = (0i64, 0f64, None::<u32>, 0u64);
    let mut ptp_seeded: Option<i32> = None;
    let mut work = [0i32; 4096];
    let mut resampled = [0i32; 4096];
    let mut placement = [None; audio_wire::MAX_CHANNELS];
    let mut wire_channels = 0usize;
    let mut last_packet = tape_rt::micros();
    let started = tape_rt::micros();
    let mut next_report = started + 1_000_000;
    let mut level = 0usize;
    let mut trimming = false;
    let (mut work_total, mut work_max, mut slow_receive, mut slow_info, mut gaps_shown) = (0u64, 0u64, 0u64, 0u64, 0u32);
    // Ring level seen at each packet arrival (before the write), kept for the summary: min, max, and how often it held
    // less than one packet.
    let (mut level_min, mut level_max, mut level_short, mut level_samples) = (usize::MAX, 0usize, 0u64, 0u64);
    let (mut level_sum, mut level_buckets) = (0u64, [0u32; 16]);
    let (mut second_low, mut second_high) = (usize::MAX, 0usize);
    let (mut last_pass, mut second_gap) = (tape_rt::micros(), 0u64);
    loop {
        let pass = tape_rt::micros();
        second_gap = second_gap.max(pass - last_pass);
        last_pass = pass;
        if (api.should_abort)() != 0 {
            break;
        }
        if options.seconds != 0 && tape_rt::micros() >= started + options.seconds * 1_000_000 {
            break;
        }
        transport.igmp_service(api, options.nic, None);
        let t_receive = tape_rt::micros();
        let length = (api.nic_receive)(options.nic, frame.as_mut_ptr(), frame.len());
        let spent_receive = tape_rt::micros() - t_receive;
        slow_receive = slow_receive.max(if length != 0 { spent_receive } else { 0 });
        // The card's own stamp when it has one: the polling loop's delay stays out of the PTP arrival statistics.
        let received_at = if length != 0 && transport.ptp { tape_rt::rx_arrival_us(options.nic) } else { t_receive + spent_receive };
        if length == 0 {
            if locked.is_some() && tape_rt::micros() > last_packet + 2_000_000 {
                println!("\nThe sender went quiet; waiting for a stream again.");
                locked = None; // playback keeps running (silence), ready for the next stream
            }
            tape_rt::idle();
            continue;
        }
        if transport.igmp_service(api, options.nic, Some(&frame[..length])) {
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
                if !base_taken {
                    base_taken = true;
                    let mut device = AudioStats::default();
                    (api.audio_stats)(&mut device);
                    stats::get().underrun_base = device.playback_underruns;
                }
                if options.priority == Some(Priority::Auto) {
                    adapt = Some(Adapt::new(tape_rt::micros(), lost, stats::get().underrun_last.max(stats::get().underrun_base)));
                }
                let id = packet.source;
                stats::set_receive(format_args!("PCM {}-bit, {} ch{}", packet.bits, packet.channels, if options.adopted_receive { " (peer's choice)" } else { "" }));
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
        if let Some(time) = packet.avtp_time.filter(|_| clock::locked()) {
            // Presentation time = capture + 2 ms on the sender's PTP timeline (nanoseconds mod 2^32).
            let delay = i64::from((clock::time_ns_at(received_at).unwrap_or(0) as u32).wrapping_sub(time.wrapping_sub(clock::AVTP_OFFSET_NS)) as i32);
            if (-5_000_000..=500_000_000).contains(&delay) {
                stats::get().ptp_arrival(delay / 1000, last_packet);
            }
        }
        if let Some(time) = packet.avtp_time.filter(|_| options.ptp_seed && clock::locked()) {
            // The stamps wrap every 4.29 s: sum the 32-bit differences of consecutive packets.
            if let Some(previous) = previous_stamp.replace(time) {
                if packet.sequence.wrapping_sub(expected) == 0 {
                    sender_time += i64::from(time.wrapping_sub(previous) as i32);
                    sender_frames += packet.frames as f64;
                    sender_fit.push(sender_time, sender_frames);
                } else {
                    sender_fit.reset();
                    (sender_time, sender_frames) = (0, 0.0);
                }
            }
        }
        if let Some(stamp) = packet.rtp_timestamp.filter(|_| clock::locked()) {
            // A sender on the same PTP timeline: the stamp is when its first sample was captured.
            if let Some(delay) = clock::time_ns_at(received_at).and_then(|now| ptp_tape::stamp_delay(now, stamp, packet.rate, 500_000_000)) {
                stats::get().ptp_arrival(delay / 1000, last_packet);
            }
        }
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
        if options.ptp_seed && options.resample {
            if let Some(now) = clock::time_ns_at(t_info) {
                device_fit.push(now, written_out as f64 - level as f64);
            }
        }
        (level_min, level_max, level_samples) = (level_min.min(level), level_max.max(level), level_samples + 1);
        level_sum += level as u64;
        level_buckets[(level * 1000 / packet.rate.max(1) as usize).min(15)] += 1;
        (second_low, second_high) = (second_low.min(level), second_high.max(level));
        if level < frames {
            level_short += 1;
        }
        // Far too full (after a stall): discard whole packets until the ring has played down to the target, rather
        // than leaving it to the servo (limited to +-2000 ppm, so tens of seconds).
        if level > fill.target() * 2 + 96 {
            trimming = true;
        } else if level <= fill.target() {
            trimming = false;
        }
        if trimming {
            dropped += frames as u64;
            packets += 1;
            continue;
        }
        let payload = &frame[packet.payload.clone()];
        stats::peaks_from_wire(payload, wire_channels, packet.bits, frames, false);
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
            let accepted = (api.audio_write)(staging.as_ptr(), bytes);
            written_out += (accepted / device_frame) as u64;
            if accepted < bytes {
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
        let shown = stats::get();
        (shown.received, shown.lost, shown.level_frames, shown.target_frames, shown.offset_ppm, shown.rate) = (packets, lost, level as u32, fill.target() as u32, servo.offset_ppm(), packet.rate);
        shown.packet_frames = packet.frames as u32;
        {
            let mut device = AudioStats::default();
            (api.audio_stats)(&mut device);
            shown.underrun_last = device.playback_underruns;
        }
        if let Some(adapt) = adapt.as_mut() {
            if let Some(target_ms) = adapt.poll(tape_rt::micros(), lost, shown.underrun_last) {
                let frames = (packet.rate as usize * target_ms as usize / 1000).max(1);
                fill.set_target(frames);
                servo.set_target(frames);
                println!("\nauto: ring target {target_ms} ms ({}).", if target_ms < shown.target_frames * 1000 / packet.rate.max(1) { "30 s without loss" } else { "a loss or underrun" });
            }
        }
        if tape_rt::micros() >= next_report {
            next_report += 1_000_000;
            if options.ptp_seed && ptp_seeded.is_none() && sender_fit.span_secs() >= SEED_SECS && device_fit.span_secs() >= SEED_SECS {
                if let Some(ppm) = sender_fit.rate().zip(device_fit.rate()).and_then(|(sender, device)| audio_wire::rate_ratio_ppm(sender, device)) {
                    let learned = servo.offset_ppm();
                    servo.seed_ppm(ppm as i32);
                    ptp_seeded = Some(ppm as i32);
                    println!("\nPTP: sender runs {ppm:+.1} ppm against this sound card; servo started there (it had learned {learned} ppm by itself).");
                }
            }
            (shown.level_low, shown.level_high) = (if second_low == usize::MAX { 0 } else { second_low as u32 }, second_high as u32);
            (second_low, second_high) = (usize::MAX, 0);
            (shown.pass_gap_us, second_gap) = (second_gap as u32, 0);
            print!(".");
            if options.verbose && (next_report - started) / 1_000_000 % 10 == 0 {
                println!(" [{packets} packets, {lost} lost, level {level} frames (target {}), {dropped} dropped, {repeated} repeated, clock offset {} ppm]", fill.target(), servo.offset_ppm());
            }
        }
    }
    transport.leave_group(api, options.nic);
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
    if level_samples > 0 {
        let rate = stats::get().rate.max(1) as usize;
        println!("Ring at arrival: {} to {} ms, mean {} us (target {} ms); under one packet {} of {level_samples} times.", level_min * 1000 / rate, level_max * 1000 / rate, level_sum * 1_000_000 / level_samples / rate as u64, fill.target() * 1000 / rate, level_short);
        if options.verbose {
            println!("Ring at arrival by whole ms (0 to 15+): {:?}", level_buckets);
        }
    }
    if stats::get().ptp_samples > 0 {
        let shown = stats::get();
        let ring_us = i64::from(shown.level_frames) * 1_000_000 / i64::from(shown.rate.max(1));
        println!("PTP: capture to arrival {} us on average ({} to {}); with the {} us in the ring, about {} us from the sender's capture to this playback ring ({} packets).", shown.ptp_delay_us, shown.ptp_min_us, shown.ptp_max_us, ring_us, shown.ptp_delay_us + ring_us, shown.ptp_samples);
    }
    if options.ptp_seed {
        match sender_fit.rate().zip(device_fit.rate()) {
            Some((sender, device)) => {
                println!("PTP rate fit: sender {sender:.2} Hz, sound card {device:.2} Hz over {:.0} s: {:+.1} ppm; the servo ended at {} ppm.", device_fit.span_secs(), audio_wire::rate_ratio_ppm(sender, device).unwrap_or(0.0), servo.offset_ppm());
                if let Some(ppm) = ptp_seeded {
                    println!("PTP: the servo was seeded with {ppm} ppm after {SEED_SECS:.0} s.");
                }
            }
            None => println!("PTP rate fit: not enough stamped packets (needs AVTP and a locked clock)."),
        }
    }
    if options.resample {
        println!("Delay sender capture to playback ring: about {} ms (one packet plus {} ms queued); the device adds its own buffer.", stats::get().delay_ms(), stats::get().level_frames * 1000 / stats::get().rate.max(1));
        println!("Received {packets} packets: {lost} lost, {dropped} frame(s) discarded, clock offset learned {} ppm (resampled, no frames dropped or repeated for drift), {foreign} from another sender; playback {} packets, {} underruns, {} transfer errors.", servo.offset_ppm(), stats.playback_packets, stats::get().underrun_last.saturating_sub(stats::get().underrun_base), stats.playback_errors);
    } else {
        println!("Received {packets} packets: {lost} lost, {dropped} frame(s) dropped and {repeated} repeated for clock drift, {foreign} from another sender; playback {} packets, {} underruns, {} transfer errors.", stats.playback_packets, stats::get().underrun_last.saturating_sub(stats::get().underrun_base), stats.playback_errors);
    }
}

/// The configuration file's lines as option words. A line is `key value` (becomes `key=value`) or any number of
/// ready-made words (`proto=rtp`, `verbose`, `send`); `#` starts a comment.
static mut CONFIG_TEXT: [u8; 4096] = [0; 4096];
static mut CONFIG_WORDS: [u8; 4608] = [0; 4608];
pub const MAX_WORDS: usize = 64;

fn load_config(api: &TapeApi, name: &str, words: &mut [&'static str; MAX_WORDS], count: &mut usize) -> Result<(), &'static str> {
    let text = unsafe { &mut *(&raw mut CONFIG_TEXT) };
    let length = (api.fs_read)(name.as_ptr(), name.len(), text.as_mut_ptr(), text.len());
    if length < 0 {
        return Err(if length == tape_rt::FS_NOT_FOUND { "the configuration file does not exist" } else { "the configuration file cannot be read (is it over 4096 bytes?)" });
    }
    let text = core::str::from_utf8(&text[..length as usize]).map_err(|_| "the configuration file is not text")?;
    normalize(text, words, count)
}

/// Splits configuration text into option words (see `load_config`). The words live in a static buffer, so only one
/// text's words exist at a time.
pub fn normalize(text: &str, words: &mut [&'static str; MAX_WORDS], count: &mut usize) -> Result<(), &'static str> {
    let out = unsafe { &mut *(&raw mut CONFIG_WORDS) };
    let mut used = 0;
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        let mut parts = line.split_ascii_whitespace();
        let (first, second, third) = (parts.next(), parts.next(), parts.next());
        let Some(first) = first else { continue };
        let pair = !first.contains('=') && second.is_some() && third.is_none();
        let pieces: &[&str] = if pair { &[first, "=", second.unwrap_or("")] } else { &[] };
        let push = |piece: &str, out: &mut [u8; 4608], used: &mut usize| -> Result<(), &'static str> {
            if *used + piece.len() > out.len() {
                return Err("the configuration file has too many words");
            }
            out[*used..*used + piece.len()].copy_from_slice(piece.as_bytes());
            *used += piece.len();
            Ok(())
        };
        let start = used;
        if pair {
            for piece in pieces {
                push(piece, out, &mut used)?;
            }
            push(" ", out, &mut used)?;
        } else {
            for word in line.split_ascii_whitespace() {
                push(word, out, &mut used)?;
                push(" ", out, &mut used)?;
            }
        }
        for word in core::str::from_utf8(&out[start..used]).unwrap_or("").split_ascii_whitespace() {
            if *count >= MAX_WORDS {
                return Err("the configuration file has too many words");
            }
            // The words live in the static buffer for the rest of the run.
            words[*count] = unsafe { core::mem::transmute::<&str, &'static str>(word) };
            *count += 1;
        }
    }
    Ok(())
}

/// What to run, from the configuration file (`@NAME`, any `diskN/NAME` works) and then the words on the command
/// line, which override it. The mode is a bare `send`/`recv`/`duplex` or `mode=...`.
fn collect(api: &TapeApi, arguments: &'static str) -> Result<([&'static str; MAX_WORDS], usize), &'static str> {
    let mut words: [&'static str; MAX_WORDS] = [""; MAX_WORDS];
    let mut count = 0;
    for word in arguments.split_ascii_whitespace() {
        if let Some(name) = word.strip_prefix('@') {
            load_config(api, name, &mut words, &mut count)?;
        } else if count < MAX_WORDS {
            words[count] = word;
            count += 1;
        } else {
            return Err("too many words");
        }
    }
    Ok((words, count))
}

/// Turns `source=NAME` and a `to=NAME` that is not an address into the stream's parameters. False if a name could not
/// be found (or Escape was pressed while looking).
pub fn resolve_names(api: &'static TapeApi, mode: &str, options: &mut Options<'static>) -> bool {
    // With `proto=raw|avtp` given, names are looked up in layer 2 announcements; otherwise a name means RTP.
    let layer_2 = options.proto_explicit && options.proto != Proto::Rtp;
    let auto_to = matches!(mode, "send" | "duplex") && options.proto == Proto::Rtp && matches!(options.to, "" | "auto" | "multicast");
    let auto_source = mode == "recv" && !layer_2 && options.source == "auto";
    let wants_source = !options.source.is_empty() && mode == "recv" && !auto_source;
    let to_name = !auto_to && !options.to.is_empty() && net::parse_ip(options.to.split(':').next().unwrap_or("")).is_none() && parse_mac(options.to).is_none();
    let wants_listener = to_name && matches!(mode, "send" | "duplex") && (options.proto == Proto::Rtp || layer_2);
    let announces_listening = matches!(mode, "recv" | "duplex") && (!options.source.is_empty() || wants_listener);
    if !wants_source && !wants_listener && !auto_to && !auto_source {
        return true;
    }
    if !layer_2 {
        options.proto = Proto::Rtp;
    }
    let Some(mut transport) = Transport::open(api, options) else { return false };
    if auto_to {
        let mut buffer = [0u8; audio_wire::sap::MAX_NAME];
        let name = peers::session_name(options.name, transport.mac, &mut buffer);
        options.to = peers::choose_group(api, options, &mut transport, name);
    }
    // Be findable while looking: a peer that sends to our name needs our listening address before it can start,
    // and it may be waiting for the same of us. (A duplex machine looks up its `source=` later, while it sends.)
    if announces_listening {
        transport.announce_receiver(options);
    }
    if wants_listener && layer_2 && let Some(peer) = peers::kernel_peer(api, options.to) {
        // The kernel already knows the machine from its name announcements: no need to wait for a stream announcement.
        println!("'{}' is {} (known from its name announcement).", options.to, MacText(peer.mac));
        peers::set_listener_mac(options, peer.mac);
    } else if wants_listener {
        let Some(entry) = peers::find(api, options, &mut transport, options.to, true) else { return false };
        if layer_2 {
            peers::adopt_listener_l2(options, &entry);
        } else {
            peers::adopt_listener(options, &entry);
        }
    }
    if auto_source {
        let Some(entry) = peers::find_auto(api, options, &mut transport) else { return false };
        if let Err(reason) = peers::adopt_sender(options, &entry, transport.config.ip.0) {
            println!("audio-net: '{}': {reason}.", entry.session.name());
            return false;
        }
    }
    if wants_source {
        let Some(entry) = peers::find(api, options, &mut transport, options.source, false) else { return false };
        if layer_2 {
            peers::adopt_sender_l2(options, &entry);
        } else if let Err(reason) = peers::adopt_sender(options, &entry, transport.config.ip.0) {
            println!("audio-net: '{}': {reason}.", options.source);
            return false;
        }
    }
    true
}

/// `source=NAME` for the receiving half of a duplex run: looked up while the sending half is already streaming (the
/// peer's receiver may need our stream to exist before it announces the stream we are looking for).
pub fn resolve_source(api: &'static TapeApi, options: &mut Options<'static>) -> bool {
    let layer_2 = options.proto_explicit && options.proto != Proto::Rtp;
    if !layer_2 {
        options.proto = Proto::Rtp;
    }
    let Some(mut transport) = Transport::open(api, options) else { return false };
    transport.announce_receiver(options);
    let Some(entry) = peers::find(api, options, &mut transport, options.source, false) else { return false };
    if layer_2 {
        peers::adopt_sender_l2(options, &entry);
    } else if let Err(reason) = peers::adopt_sender(options, &entry, transport.config.ip.0) {
        println!("audio-net: '{}': {reason}.", options.source);
        return false;
    }
    true
}

pub fn run_once(api: &'static TapeApi, mode: &str, options: &Options<'static>) {
    match mode {
        "duplex" => duplex::run(api, options),
        "serve" => serve::serve(api, options),
        "benchserve" => bench::serve(api, options),
        "bench" => {
            bench::master(api, options, false);
        }
        "send" if options.opus => opus::send(api, options),
        "recv" if options.opus => opus::receive(api, options),
        "send" => send(api, options),
        _ => receive(api, options),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(real: &'static TapeApi) {
    tape_rt::init(real);
    let arguments = match tape_rt::take_audio_device_arg(real, core::str::from_utf8(tape_rt::args()).unwrap_or("")) {
        Ok(rest) => rest,
        Err(reason) => return println!("audio-net: {reason}."),
    };
    if arguments.trim().is_empty() {
        return panel::run(real);
    }
    let (words, count) = match collect(real, arguments) {
        Ok(found) => found,
        Err(reason) => return println!("audio-net: {reason}."),
    };
    // The mode is the last bare mode word (the command line overrides the file); everything else is an option.
    let mut mode = "";
    let mut options_words: [&'static str; MAX_WORDS] = [""; MAX_WORDS];
    let mut kept = 0;
    for &word in &words[..count] {
        match word.strip_prefix("mode=").unwrap_or(word) {
            "send" | "recv" | "duplex" | "list" | "serve" | "bench" | "benchserve" => mode = word.strip_prefix("mode=").unwrap_or(word),
            _ => {
                options_words[kept] = word;
                kept += 1;
            }
        }
    }
    if mode.is_empty() {
        return println!("Usage: tape run audio-net send|recv|duplex|serve|list [options]   or   tape run audio-net @audio.cfg [options]\n(see the header of v2/tapes/audio-net/src/main.rs, or docs/design.md, for the options and the file format)");
    }
    let mut options: Options<'static> = match parse(options_words[..kept].iter().copied()) {
        Ok(options) => options,
        Err(reason) => return println!("audio-net: {reason}. See the header of v2/tapes/audio-net/src/main.rs for the options."),
    };
    if (real.nic_count)() <= options.nic {
        return println!("audio-net: this machine has no NIC {}.", options.nic + 1);
    }
    if mode == "serve" {
        options.proto = Proto::Rtp; // HTTP needs an IP address (ip=A.B.C.D/N or ip=dhcp)
    }
    if options.opus && options.proto != Proto::Rtp {
        return println!("audio-net: codec=opus needs proto=rtp.");
    }
    if mode == "list" {
        if !(options.proto_explicit && options.proto != Proto::Rtp) {
            options.proto = Proto::Rtp;
        }
        return peers::list(tape_rt::abort::wrap(real), &options);
    }
    if mode == "bench" || mode == "benchserve" {
        return run_once(tape_rt::abort::wrap(real), mode, &options);
    }
    let by_name = !options.source.is_empty() || (!options.to.is_empty() && net::parse_ip(options.to.split(':').next().unwrap_or("")).is_none() && parse_mac(options.to).is_none());
    println!(
        "audio-net {mode}: {} {} on NIC {}{}. Escape stops.",
        if by_name { "RTP/UDP, parameters from the peer's announcement," } else { proto_name(options.proto) },
        if by_name { "" } else if options.opus { "Opus" } else { "PCM" },
        options.nic + 1,
        if options.seconds != 0 { "" } else { "; if the stream ends it is restarted every 5 s" }
    );
    // Supervise: unless Escape was pressed (or `secs=` ran out), a run that ends is an error to retry: the peer was
    // not there yet, the device was unplugged, the link dropped. A headless machine started from the `start` file
    // therefore keeps trying until the other side is up.
    let api = tape_rt::abort::wrap(real);
    loop {
        // Names instead of addresses: `source=NAME` (a receiver) and `to=NAME` (a sender) are looked up in the
        // announcements. The result is used for this run only, so a peer that moved is found again on a retry.
        let mut run_options = options.clone();
        if !resolve_names(api, mode, &mut run_options) {
            if tape_rt::abort::aborted() {
                break;
            }
            println!("audio-net: trying again in 5 s.");
            let until = tape_rt::micros() + 5_000_000;
            while tape_rt::micros() < until && (api.should_abort)() == 0 {
                tape_rt::idle();
            }
            continue;
        }
        run_once(api, mode, &run_options);
        if tape_rt::abort::aborted() || options.seconds != 0 {
            break;
        }
        println!("audio-net: the stream ended; trying again in 5 s.");
        let until = tape_rt::micros() + 5_000_000;
        while tape_rt::micros() < until && (api.should_abort)() == 0 {
            tape_rt::idle();
        }
        if tape_rt::abort::aborted() {
            break;
        }
    }
}

tape_rt::tape_help!(br#"audio-net: audio over the network (raw Ethernet, RTP or AVTP).

  tape run audio-net                the settings panel
  tape run audio-net send [proto=raw|rtp|avtp] [nic=N|MAC] [to=MAC|IP|NAME]
                    [rate=48000] [ch=3-4] [bits=24] [src=capture|tone=HZ]
                    [secs=N] [ip=A.B.C.D/N|dhcp] [ptp=auto|l2|off]
  tape run audio-net recv [proto=..] [nic=N|MAC] [from=MAC] [port=5004]
                    [ch=1-2] [target=MS] [group=239.1.2.3] [codec=pcm|opus]
  tape run audio-net duplex ...     send and receive together
  tape run audio-net serve [http=8000] [bitrate=KBIT]    Ogg Opus over HTTP
  tape run audio-net benchserve [proto=raw|avtp|rtp] [nic=N]   far machine
  tape run audio-net bench peer=NAME|MAC|IP [proto=avtp|rtp] [rate=48000] [bits=24]
                    [ch=1-2] [tol=0.01] [stepsecs=10]   find the lowest
                    ptime/target both directions carry; only this one runs
  tape run audio-net @audio.cfg     options from a file
  device=N picks the audio device. Escape stops. See AUDIO.md.
"#);
