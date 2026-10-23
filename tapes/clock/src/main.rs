//! `clock`: a broadcast studio clock in the Wharton style: a ring of 60 red dots that light up one second at a time (a new
//! minute starts with the dot at 12), the time as big yellow seven-segment digits (24 h unless `hours=12`) with the seconds
//! below, and the date. It keeps time itself: at start, and every 15 minutes, it asks an NTP server (or, with `source=ptp`, follows a PTP master) (the one DHCP names, a
//! given address or a host name), keeps the answer against the `micros()` clock and sets the machine's clock to it. The
//! face is drawn with `gfx::dial`, the arithmetic is `tape_net::ntp`, the network calls are `tape_ip`.
#![no_std]
#![no_main]

use audio_screen::{Screen, poll_keys};
use core::fmt::Write;
use gfx::dial;
use gfx::theme::{self, mix, theme};
use ptp_tape::{EVENT_PORT, GENERAL_PORT, GROUP, Slave, build_delay_req, port_identity};
use tape_ip::{igmp, net};
use tape_net::ntp::{self, Civil};
use tape_net::{ArpCache, Ipv4Address, MacAddress};
use tape_rt::{Key, KeyDecoder, Line, TapeApi, arg_value as value, println};

tape_rt::tape_help!(
    br#"clock [@NAME] [ntp=dhcp|A.B.C.D|NAME|off] [ip=dhcp|A.B.C.D/N] [gateway=A.B.C.D]
      [dns=A.B.C.D] [nic=N] [tz=+H[:MM]] [resync=SECONDS] [hours=24|12] [utc]
      [theme=dark|classic|light] [source=auto|ntp|ptp] [domain=N] [l2] [spin] [lead=N] [bars=on]
A studio clock: 60 red dots for the seconds, the time in big yellow digits
(24 h by default) with the seconds below it, and the date. source=auto (the
default) takes the best source there is: a PTP master over Ethernet, else one
over UDP (needs an address), else NTP, else the machine's clock (RTC). It
listens for PTP for 6 s before it asks NTP; a PTP source that is silent for
30 s is given up for NTP, and PTP takes over again when it returns. The status
line names the source in use. ntp=off alone (no source=) shows only the
machine's clock. source=ntp or source=ptp use that one source.
NTP: ntp=dhcp (the default) uses the server DHCP names (option 42),
else the gateway; an address or a host name (looked up at dns=, the DHCP DNS
server or the gateway) works too. ip=A.B.C.D/N gives a fixed address instead
of DHCP. The server is asked at start and every resync= seconds (default 900;
the picture keeps running meanwhile); the answer is kept
against the machine's microsecond clock and also sets the machine's clock
and RTC when that differs by a second or more. tz= is the local time zone in
hours east of UTC; without it the zone is taken from the machine's clock when
that is roughly right (the machine clock holds local time), else 0.
source=ptp follows a PTP master only (l2: over Ethernet, ethertype 0x88F7,
no IP or DHCP needed; else UDP, group 224.0.1.129, domain=N,
default 0): the master must say its time is real (ptp master ntp=... or
settime); the clock then tracks every Sync and the machine's clock is set
once. The loop sleeps between polls (a Sync may be timed up to 10 ms late: the
clock is a few ms off); `spin` polls flat out, accurate to ~0.1 ms, CPU busy.
ntp=off only shows the machine's clock (set by `date set` or ptp settime).
utc shows UTC. Without an answer from the server the clock runs on the
machine's clock and the status line says so in amber.
lead=N (0 to 3, default 0) lights the next 5-second mark ahead of the count: it
is lit from the previous mark until N seconds later, dark for the rest and
lit for good when the count reaches it (the old studio clocks' cue). Key l
switches it off and on.
The status line and the key line sit back in dim grey and light up for 6 s after
any key (and at start); bars=on keeps them lit.
Keys (Escape quits):
  h 12/24 hours   u UTC/local   n ask the server now   k theme   l lead
  w save the settings (hours, utc, theme, lead) in clock.cfg
Settings: clock.cfg is read at start (key=value words, # comments); `@NAME` on
the command line reads that file instead (any diskN/NAME) and w saves to it.
Words on the command line win over the file.
"#
);

/// How long the status and key bars stay bright after a key (and at start).
const BARS_SECONDS: u64 = 6;
const MAX_WIDTH: u32 = 1920;
const MAX_HEIGHT: u32 = 1080;
const WEEKDAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// A PTP slave listening on the multicast group: the clock follows the master's time continuously.
struct Ptp {
    slave: Slave,
    igmp: igmp::Igmp,
    l2: bool,
    domain: u8,
    identity: [u8; 10],
    last_empty: i64,
    /// When this slave last adopted a measurement (the `micros()` clock).
    last_ok_us: Option<u64>,
}

/// A PTP source is "fresh" while it adopted a measurement this recently; `source=auto` falls back to the next one after that.
const FRESH_US: u64 = 30_000_000;
/// `source=auto` listens this long for a PTP master before it asks NTP (a Sync and its Delay_Resp take a few seconds).
const GRACE_US: u64 = 6_000_000;
/// `source=auto`: a failed NTP query is tried again after this long (else after `resync=`).
const RETRY_US: u64 = 30_000_000;

/// Where the time comes from. `Rtc` is the machine's own clock (set by `date set` or an earlier sync).
#[derive(Clone, Copy, PartialEq)]
enum Source {
    Rtc,
    PtpL2,
    PtpL3,
    Ntp,
}

/// `source=`: one source, or `Auto` for PTP over Ethernet, then PTP over UDP, then NTP, then the machine's clock.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Ntp,
    Ptp,
    Auto,
}

/// The sources and the interface to reach them on. `ptp[0]` listens over Ethernet, `ptp[1]` over UDP (IP and IGMP).
struct Net {
    nic: usize,
    config: net::NetConfig,
    server: Ipv4Address,
    arp: ArpCache<8>,
    ptp: [Option<Ptp>; 2],
    ntp: tape_ip::ntp::Client,
    has_ntp: bool,
    auto: bool,
}

/// Lets 01:1B:19:00:00:00 (PTP over Ethernet) through the card: its hash filter, else all multicast.
fn l2_multicast(api: &TapeApi, nic: usize, on: bool) {
    if !tape_rt::multicast_group(nic, ptp_tape::L2_MAC, on) {
        (api.nic_set_multicast)(nic, u32::from(on));
    }
}

/// A Sync read later than this after the last empty poll waited in the NIC's ring: its time is too late to measure with.
const LATE_NS: i64 = 300_000;

/// What the clock knows about the time.
struct Time {
    /// Unix (UTC) nanoseconds minus `micros() * 1000`, once a server has answered.
    offset_ns: Option<i64>,
    /// Minutes east of UTC.
    zone: Option<i32>,
    last_ok_us: Option<u64>,
    delay_ns: i64,
    /// How far the previous model was off when the last answer came, in nanoseconds.
    step_ns: Option<i64>,
    /// Who gave the last answer.
    source: Source,
}

fn local_ns() -> i64 {
    tape_rt::micros() as i64 * 1000
}

fn wall_seconds() -> Option<i64> {
    let w = tape_rt::wall_clock()?;
    Some(ntp::unix_from_civil(&Civil { year: w.year, month: w.month, day: w.day, hour: w.hour, minute: w.minute, second: w.second }))
}

impl Time {
    /// The shown time as seconds since 1970 in the shown zone (UTC when `utc`), if there is any time at all.
    fn shown_seconds(&self, utc: bool) -> Option<i64> {
        let zone = i64::from(self.zone.unwrap_or(0)) * 60;
        match self.offset_ns {
            Some(offset) => Some((local_ns() + offset).div_euclid(1_000_000_000) + if utc { 0 } else { zone }),
            None => Some(wall_seconds()? - if utc { zone } else { 0 }),
        }
    }

    /// Takes one NTP answer: the clock follows it from now on. The first answer also settles the zone and, if the machine's
    /// clock disagrees, sets that.
    fn adopt(&mut self, sample: ntp::Sample, zone_given: Option<i32>) -> Result<(), &'static str> {
        let unix = (local_ns() + sample.offset).div_euclid(1_000_000_000);
        if ntp::civil_from_unix(unix).year < 2024 {
            return Err("the server's date cannot be right");
        }
        if self.zone.is_none() {
            self.zone = zone_given.or_else(|| wall_seconds().and_then(|wall| ntp::guess_zone(wall, unix))).or(Some(0));
        }
        self.step_ns = self.offset_ns.map(|old| sample.offset - old);
        self.offset_ns = Some(sample.offset);
        self.delay_ns = sample.delay;
        self.last_ok_us = Some(tape_rt::micros());
        let local = (local_ns() + sample.offset + 500_000_000).div_euclid(1_000_000_000) + i64::from(self.zone.unwrap_or(0)) * 60;
        if wall_seconds().is_none_or(|wall| (wall - local).abs() >= 1) {
            let date = ntp::civil_from_unix(local);
            let wall = tape_rt::WallClock { year: date.year, month: date.month, day: date.day, hour: date.hour, minute: date.minute, second: date.second, reserved: 0 };
            tape_rt::set_wall_clock(&wall);
        }
        Ok(())
    }
}

/// The NTP server: the one DHCP names (else the gateway), an address, or a host name looked up in DNS. None with the reason printed.
fn find_server(api: &TapeApi, arguments: &str, spec: &str, nic: usize, config: &net::NetConfig, arp: &mut ArpCache<8>, dhcp_ntp: Option<Ipv4Address>, dhcp_dns: Option<Ipv4Address>) -> Option<Ipv4Address> {
    if spec == "dhcp" {
        match dhcp_ntp {
            Some(server) => Some(server),
            None if config.gateway.0 != [0; 4] => {
                println!("clock: DHCP names no NTP server (option 42); trying the gateway.");
                Some(config.gateway)
            }
            None => {
                println!("clock: DHCP names no NTP server and there is no gateway; give ntp=A.B.C.D.");
                None
            }
        }
    } else if let Some(server) = net::parse_ip(spec) {
        Some(server)
    } else {
        let dns = value(arguments, "dns").and_then(net::parse_ip).or(dhcp_dns).or((config.gateway.0 != [0; 4]).then_some(config.gateway));
        let Some(dns) = dns else {
            println!("clock: {spec} is a name and no DNS server is known (dns=A.B.C.D).");
            return None;
        };
        match tape_ip::dns::lookup(api, nic, config, arp, dns, tape_net::dns::PORT, spec) {
            Ok(server) => Some(server),
            Err(reason) => {
                println!("clock: {spec}: {reason}.");
                None
            }
        }
    }
}

/// DHCP or a fixed address, then the sources the mode wants: PTP slaves (over Ethernet, and over UDP when there is an address)
/// and the NTP server. `Auto` goes on with what works (PTP over Ethernet needs no address). None with the reason printed.
fn connect(api: &TapeApi, arguments: &str, spec: &str, mode: Mode, ntp_wanted: bool) -> Option<Net> {
    let nic = value(arguments, "nic").and_then(|v| v.parse::<usize>().ok()).map_or(tape_rt::active_nic(), |n| n.saturating_sub(1));
    if (api.nic_count)() <= nic {
        println!("clock: this machine has no NIC {}.", nic + 1);
        return None;
    }
    let mut mac = [0u8; 6];
    (api.nic_mac)(nic, mac.as_mut_ptr());
    let ip = value(arguments, "ip").unwrap_or("dhcp");
    let auto = mode == Mode::Auto;
    let l2_only = mode == Mode::Ptp && arguments.split_whitespace().any(|w| w == "l2");
    let no_address = || (net::NetConfig { mac: MacAddress(mac), ip: Ipv4Address([0; 4]), netmask: Ipv4Address([0; 4]), gateway: Ipv4Address([0; 4]) }, None, None);
    let (config, dhcp_ntp, dhcp_dns) = if l2_only {
        no_address()
    } else if ip == "dhcp" {
        println!("clock: asking DHCP for an address on NIC {}...", nic + 1);
        match net::dhcp_with_servers(api, nic, MacAddress(mac)) {
            Some(found) => found,
            None if auto => {
                println!("clock: no DHCP answer; only PTP over Ethernet, else the machine's clock.");
                no_address()
            }
            None => {
                println!("clock: no DHCP answer; give ip=A.B.C.D/N instead. Showing the machine's clock.");
                return None;
            }
        }
    } else {
        match net::static_config(MacAddress(mac), ip, value(arguments, "gateway").unwrap_or("")) {
            Ok(config) => (config, None, None),
            Err(reason) => {
                println!("clock: {reason}.");
                return None;
            }
        }
    };
    let have_ip = config.ip.0 != [0; 4];
    let mut arp = ArpCache::<8>::new();
    let mut ptp = [None, None];
    if mode != Mode::Ntp {
        let domain = value(arguments, "domain").and_then(|v| v.parse::<u8>().ok()).unwrap_or(0);
        let identity = port_identity(mac, 1);
        let slave = |l2| Ptp { slave: Slave::new(domain, identity, 1_000_000_000), igmp: igmp::Igmp::new(GROUP), l2, domain, identity, last_empty: 0, last_ok_us: None };
        if l2_only || auto {
            l2_multicast(api, nic, true);
            println!("clock: PTP slave on NIC {} over Ethernet (0x88F7), domain {}; waiting for a master.", nic + 1, domain);
            ptp[0] = Some(slave(true));
        }
        if !l2_only && have_ip {
            let mut udp = slave(false);
            udp.igmp.join(api, nic, &config);
            println!("clock: PTP slave on NIC {} ({}.{}.{}.{}), domain {}; waiting for a master.", nic + 1, config.ip.0[0], config.ip.0[1], config.ip.0[2], config.ip.0[3], domain);
            ptp[1] = Some(udp);
        }
    }
    let mut server = None;
    if mode == Mode::Ntp || (auto && ntp_wanted && have_ip) {
        server = find_server(api, arguments, spec, nic, &config, &mut arp, dhcp_ntp, dhcp_dns);
        if server.is_none() && mode == Mode::Ntp {
            println!("clock: showing the machine's clock.");
            return None;
        }
    }
    if let Some(Ipv4Address(s)) = server {
        println!("clock: NTP server {}.{}.{}.{} (address {}.{}.{}.{}).", s[0], s[1], s[2], s[3], config.ip.0[0], config.ip.0[1], config.ip.0[2], config.ip.0[3]);
    }
    let address = server.unwrap_or(Ipv4Address([0; 4]));
    Some(Net { nic, config, server: address, arp, ptp, ntp: tape_ip::ntp::Client::new(address), has_ntp: server.is_some(), auto })
}

/// Asks the server and adopts the answer. The reason for a failure is ready to show.
fn sync(api: &TapeApi, network: &mut Net, time: &mut Time, zone_given: Option<i32>) -> Result<(), &'static str> {
    let sample = tape_ip::ntp::query(api, network.nic, &network.config, &mut network.arp, network.server).ok_or("no answer from the NTP server")?;
    time.adopt(sample, zone_given)?;
    time.source = Source::Ntp;
    Ok(())
}

/// True while the PTP slave `index` (0 Ethernet, 1 UDP) adopted a measurement recently.
fn ptp_fresh(network: &Net, index: usize) -> bool {
    network.ptp[index].as_ref().and_then(|ptp| ptp.last_ok_us).is_some_and(|ok| tape_rt::micros().wrapping_sub(ok) < FRESH_US)
}

fn slave_ok(ptp: Option<&mut Ptp>) {
    if let Some(ptp) = ptp {
        ptp.last_ok_us = Some(tape_rt::micros());
    }
}

/// Handles what PTP has to say: reads the frames that came in, adopts the master's time, and sends a Delay_Req when one is due.
/// Returns true when a new measurement was adopted. Called every pass of the main loop: the receive times are the clock when the
/// frame is picked up; with `spin` a Sync that waited in the ring is discarded, when sleeping every Sync is late and none is.
/// With two slaves (`source=auto`) Ethernet wins: UDP measurements are only adopted while Ethernet has none that is fresh.
fn ptp_poll(api: &TapeApi, network: &mut Net, time: &mut Time, zone_given: Option<i32>, spin: bool) -> bool {
    let l2_fresh = ptp_fresh(network, 0);
    let Net { nic, config, arp, ptp, .. } = network;
    let mut frame = [0u8; net::MAX_FRAME];
    let mut adopted = false;
    for _ in 0..8 {
        let length = (api.nic_receive)(*nic, frame.as_mut_ptr(), frame.len());
        if length == 0 {
            for ptp in ptp.iter_mut().flatten() {
                ptp.last_empty = local_ns();
            }
            break;
        }
        let t2 = local_ns();
        let (index, is_event, payload) = if let Some(message) = ptp_tape::l2_message(&frame[..length]) {
            if ptp[0].is_none() {
                continue;
            }
            (0, ptp_tape::is_event(message), message)
        } else {
            let Some(udp) = ptp[1].as_mut() else { continue };
            if udp.igmp.handle(&frame[..length], t2 as u64 / 1000) {
                continue;
            }
            let net::Inbound::Udp { destination_port, payload, .. } = net::inbound(api, *nic, config, arp, &frame[..length], EVENT_PORT..=GENERAL_PORT, 0) else { continue };
            (1, destination_port == EVENT_PORT, payload)
        };
        let Some(slave) = ptp[index].as_mut() else { continue };
        let is_sync = payload.first().is_some_and(|byte| byte & 0x0f == ptp_tape::SYNC);
        let sample = if spin && is_sync && is_event && t2 - slave.last_empty > LATE_NS {
            slave.slave.discard_sync(payload);
            None
        } else {
            slave.slave.handle(payload, t2)
        };
        let Some(sample) = sample else { continue };
        let Some((_, master)) = slave.slave.master() else { continue };
        if !master.has_real_time() {
            continue;
        }
        // sample.offset is the local clock minus the master's (TAI); the model wants Unix minus local.
        let mut offset = master.unix_ns(t2 - sample.offset) - t2;
        // The UDP slave keeps following its master, it is only not the one the clock listens to.
        if index == 1 && l2_fresh {
            slave_ok(ptp[1].as_mut());
            continue;
        }
        // A single measurement jitters by a few hundred microseconds: follow it slowly, but jump when the master moved.
        let following = matches!(time.source, Source::PtpL2 | Source::PtpL3);
        if let Some(old) = time.offset_ns.filter(|old| following && (offset - old).abs() < 50_000_000) {
            offset = old + (offset - old) / 8;
        }
        if time.adopt(ntp::Sample { offset, delay: sample.delay }, zone_given).is_ok() {
            time.source = if index == 0 { Source::PtpL2 } else { Source::PtpL3 };
            slave_ok(ptp[index].as_mut());
            adopted = true;
        }
    }
    for ptp in ptp.iter_mut().flatten() {
        if !ptp.l2 {
            ptp.igmp.poll(api, *nic, config, local_ns() as u64 / 1000);
        }
        if let Some(sequence) = ptp.slave.request_due(local_ns()) {
            let mut message = [0u8; 64];
            let length = build_delay_req(&mut message, ptp.domain, ptp.identity, sequence);
            let destination = MacAddress([0x01, 0x00, 0x5e, GROUP[1] & 0x7f, GROUP[2], GROUP[3]]);
            let mut out = [0u8; 160];
            let size = if ptp.l2 {
                Some(ptp_tape::build_l2_frame(&mut out, config.mac.0, &message[..length])).filter(|&size| size > 0)
            } else {
                net::build_udp_frame(&mut out, config, destination, Ipv4Address(GROUP), EVENT_PORT, EVENT_PORT, 1, &message[..length])
            };
            if let Some(size) = size {
                let t3 = local_ns();
                net::send_frame(api, *nic, &out[..size]);
                ptp.slave.request_sent(sequence, t3);
            }
        }
        ptp.slave.tick(local_ns());
    }
    adopted
}

/// Where everything is, from the screen size: a round dial between the status line and the key line.
#[derive(Clone, Copy)]
struct Geometry {
    width: i32,
    height: i32,
    cx: i32,
    cy: i32,
    d: i32,
    ring: i32,
    small: i32,
    big: i32,
}

impl Geometry {
    fn new(width: u32, height: u32) -> Self {
        let (width, height) = (width as i32, height as i32);
        let d = (width.min(height - 58) - 24).max(120);
        Self { width, height, cx: width / 2, cy: 34 + (height - 58) / 2, d, ring: d / 2 - d / 52, small: (d / 90).max(2), big: (d / 52).max(3) }
    }

    fn scaled(&self, per_mille: i32, least: i32) -> i32 {
        (self.d * per_mille / 1000).max(least)
    }
}

/// The seconds dots: a saturated red (the themes' alarm red looks pink on a black face), darker on the light theme.
fn dot_red() -> u32 {
    if theme().name == "light" { 0xd01010 } else { 0xff1a1a }
}

fn draw_grid(screen: &mut Screen, g: &Geometry) {
    let mut canvas = screen.grid();
    canvas.clear(theme().background);
    let unlit = mix(theme().background, dot_red(), 1, 20);
    for index in 0..60 {
        let (x, y) = dial::ring_position(index, g.cx, g.cy, g.ring);
        dial::fill_disc(&mut canvas, x, y, if index % 5 == 0 { g.big } else { g.small }, unlit);
    }
}

struct View {
    /// Seconds after a 5-second mark during which the next mark is already lit (0 = never, at most 3).
    lead: u32,
    /// The lead to come back to when `l` switches it on.
    lead_on: u32,
    twelve: bool,
    utc: bool,
}

fn draw_face(canvas: &mut gfx::Canvas, g: &Geometry, seconds: i64, view: &View) {
    let date = ntp::civil_from_unix(seconds);
    let colour = theme().text;
    let ghost = dial::ghost(theme().background, colour);
    // With a cue the 12 o'clock dot is dim like the rest at 55-59, so the cue for the minute mark shows and goes dark too.
    let first = if view.lead > 0 && date.second >= 55 { 1 } else { 0 };
    for index in first..=u32::from(date.second) {
        let (x, y) = dial::ring_position(index, g.cx, g.cy, g.ring);
        dial::fill_disc(canvas, x, y, if index % 5 == 0 { g.big } else { g.small }, dot_red());
    }
    // The next 5-second mark is lit ahead of the count for `lead` seconds after the previous one, dark again for the
    // rest, and lit for good when the count arrives. 
    let into = u32::from(date.second) % 5;
    if view.lead > 0 && into <= view.lead {
        let (x, y) = dial::ring_position((u32::from(date.second) / 5 * 5 + 5) % 60, g.cx, g.cy, g.ring);
        dial::fill_disc(canvas, x, y, g.big, dot_red());
    }

    let hour = if view.twelve { (date.hour + 11) % 12 + 1 } else { date.hour };
    let tens = if view.twelve && hour < 10 { None } else { Some(hour / 10) };
    let digits = [tens, Some(hour % 10), Some(date.minute / 10), Some(date.minute % 10)];
    let (w, h, t) = (g.scaled(125, 6), g.scaled(220, 12), g.scaled(28, 2));
    let (pair_gap, colon_gap, colon_w) = (g.scaled(20, 2), g.scaled(15, 2), g.scaled(30, 3));
    let total = 4 * w + 2 * pair_gap + 2 * colon_gap + colon_w;
    let (mut x, y) = (g.cx - total / 2, g.cy - g.scaled(190, 0));
    for (index, d) in digits.into_iter().enumerate() {
        dial::digit(canvas, x, y, w, h, t, d, colour, ghost);
        x += w;
        if index == 0 || index == 2 {
            x += pair_gap;
        } else if index == 1 {
            x += colon_gap;
            dial::colon(canvas, x + (colon_w - t) / 2, y, t, h, colour);
            x += colon_w + colon_gap;
        }
    }

    let (sw, sh, st) = (g.scaled(57, 4), g.scaled(100, 8), g.scaled(14, 2));
    let sgap = g.scaled(12, 2);
    let (mut x, y) = (g.cx - (2 * sw + sgap) / 2, g.cy + g.scaled(90, 0));
    dial::digit(canvas, x, y, sw, sh, st, Some(date.second / 10), colour, ghost);
    x += sw + sgap;
    dial::digit(canvas, x, y, sw, sh, st, Some(date.second % 10), colour, ghost);
    let scale = if g.d >= 560 { 2 } else { 1 };
    if view.twelve {
        canvas.text_scaled(x + sw + sgap * 2, y + sh - 16 * scale, if date.hour >= 12 { "PM" } else { "AM" }, scale as u32, theme().dim);
    }

    let mut text = Line::<32>::new();
    let _ = write!(text, "{} {} {} {}", WEEKDAYS[ntp::weekday(seconds) as usize], date.day, MONTHS[(date.month as usize).clamp(1, 12) - 1], date.year);
    let width = text.as_str().len() as i32 * 8 * scale;
    canvas.text_scaled(g.cx - width / 2, g.cy + g.scaled(255, 0), text.as_str(), scale as u32, theme().dim);
    if view.utc {
        let width = 3 * 8 * scale;
        canvas.text_scaled(g.cx - width / 2, g.cy + g.scaled(255, 0) + 18 * scale, "UTC", scale as u32, theme().amber);
    }
}

fn draw_status(canvas: &mut gfx::Canvas, g: &Geometry, view: &View, time: &Time, network: Option<&Net>, resync_s: u64, message: &str, bright: bool) {
    let mut line = Line::<200>::new();
    let zone = time.zone.unwrap_or(0);
    let _ = write!(line, "CLOCK  {}  {}", if view.twelve { "12 h" } else { "24 h" }, if view.utc { "UTC" } else { "local" });
    if !view.utc && time.zone.is_some() {
        let _ = write!(line, " UTC{}{}:{:02}", if zone < 0 { '-' } else { '+' }, zone.abs() / 60, zone.abs() % 60);
    }
    let mut stale = false;
    match (time.source, network) {
        (Source::PtpL2 | Source::PtpL3, Some(n)) => {
            let l2 = time.source == Source::PtpL2;
            let domain = n.ptp[usize::from(!l2)].as_ref().map_or(0, |ptp| ptp.domain);
            let _ = write!(line, "  PTP{} domain {domain}", if l2 { " l2" } else { "" });
            let age = tape_rt::micros().wrapping_sub(time.last_ok_us.unwrap_or(0)) / 1_000_000;
            let _ = write!(line, "  locked, last Sync {age} s ago  delay {} us", time.delay_ns / 1000);
            stale = age > 10;
        }
        (Source::Ntp, Some(n)) => {
            let age = tape_rt::micros().wrapping_sub(time.last_ok_us.unwrap_or(0)) / 1_000_000;
            let s = n.server.0;
            let _ = write!(line, "  NTP {}.{}.{}.{}  synced {age} s ago  rtt {} us", s[0], s[1], s[2], s[3], time.delay_ns / 1000);
            if let Some(step) = time.step_ns {
                let _ = write!(line, "  step {:+} us", step / 1000);
            }
            stale = age > 3 * resync_s;
        }
        (_, Some(n)) if n.auto => {
            let _ = write!(line, "  auto: no PTP master or NTP answer yet, machine clock (RTC)");
            stale = true;
        }
        (_, Some(n)) if n.ptp.iter().any(Option::is_some) => {
            let _ = write!(line, "  PTP: no master with real time yet, machine clock");
            stale = true;
        }
        (_, Some(_)) => {
            let _ = write!(line, "  NTP: no answer yet, machine clock");
            stale = true;
        }
        (_, None) => {
            let _ = write!(line, "  machine clock");
        }
    }
    // The bars sit back (a third of the text colour) until a key lights them; a stale clock stays amber and visible.
    let (status, keys) = if bright { (theme().text, theme().dim) } else { (mix(theme().background, theme().text, 1, 3), mix(theme().background, theme().text, 1, 4)) };
    canvas.text(16, 6, line.as_str(), if stale { theme().amber } else { status });
    if !message.is_empty() {
        canvas.text_right(g.width - 16, 6, message, theme().amber);
    }
    canvas.text(16, g.height - 18, "h 12/24 h  u UTC/local  n sync now  k theme  l lead  w save  Esc", keys);
}

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(api: &'static TapeApi) {
    tape_rt::init(api);
    if tape_rt::fp_level() == 0 {
        return println!("clock: needs a hard-float tape build (kernel without FP support?).");
    }
    let screen_number = tape_rt::screen_arg(core::str::from_utf8(tape_rt::args()).unwrap_or(""));
    let Some((width, height)) = tape_rt::gfx_info_on(screen_number) else {
        return println!("clock: no pixel screen (BIOS text mode). Use a UEFI boot or a VBE mode.");
    };
    let config = match tape_rt::config::Config::load(api, "clock", core::str::from_utf8(tape_rt::args()).unwrap_or("")) {
        Ok(config) => config,
        Err(reason) => return println!("clock: {reason}."),
    };
    let arguments = config.text();
    gfx::theme::set_by_name(value(arguments, "theme").unwrap_or("classic"));
    if value(arguments, "theme").is_some_and(|name| !gfx::theme::set_by_name(name)) {
        return println!("clock: theme= takes dark, classic or light.");
    }
    let zone_given = match value(arguments, "tz") {
        Some(text) => match ntp::parse_zone(text) {
            Some(minutes) => Some(minutes),
            None => return println!("clock: tz= is hours east of UTC, like +2, -5 or +5:30."),
        },
        None => None,
    };
    let lead = value(arguments, "lead").and_then(|v| v.parse::<u32>().ok()).unwrap_or(0).min(3);
    let mut view = View { lead, lead_on: if lead > 0 { lead } else { 3 }, twelve: value(arguments, "hours") == Some("12"), utc: arguments.split_whitespace().any(|w| w == "utc") || value(arguments, "utc") == Some("1") };
    let resync_s = value(arguments, "resync").and_then(|v| v.parse::<u64>().ok()).unwrap_or(900).clamp(10, 86_400);
    let spec = value(arguments, "ntp").unwrap_or("dhcp");
    let mut time = Time { offset_ns: None, zone: zone_given, last_ok_us: None, delay_ns: 0, step_ns: None, source: Source::Rtc };
    // Without source=, `auto`; `ntp=off` alone still means "no network at all", as it always did.
    let mode = match value(arguments, "source") {
        Some("ntp") => Mode::Ntp,
        Some("ptp") => Mode::Ptp,
        Some("auto") => Mode::Auto,
        None if spec == "off" => Mode::Ntp,
        None => Mode::Auto,
        Some(_) => return println!("clock: source= takes auto, ptp or ntp."),
    };
    let mut network = if spec == "off" && mode == Mode::Ntp { None } else { connect(api, arguments, spec, mode, spec != "off") };
    // Auto starts from the loop (PTP gets a few seconds to show up first); a single NTP source asks right away.
    if let Some(n) = network.as_mut().filter(|n| mode == Mode::Ntp && n.has_ntp) {
        match sync(api, n, &mut time, zone_given) {
            Ok(()) => {
                let date = ntp::civil_from_unix(time.shown_seconds(false).unwrap_or(0));
                println!("clock: NTP time {:04}-{:02}-{:02} {:02}:{:02}:{:02} (zone UTC{:+}h, round trip {} us); the machine's clock is set.", date.year, date.month, date.day, date.hour, date.minute, date.second, time.zone.unwrap_or(0) / 60, time.delay_ns / 1000);
            }
            Err(reason) => println!("clock: {reason}; running on the machine's clock until it answers."),
        }
    }
    if mode != Mode::Auto && time.shown_seconds(false).is_none() {
        return println!("clock: the machine's clock is not set and there is no NTP time (date set, or ntp=).");
    }

    let g = Geometry::new(width.min(MAX_WIDTH), height.min(MAX_HEIGHT));
    let Some(mut screen) = Screen::new(api, g.width as u32, g.height as u32).map(|screen| screen.on(screen_number)) else { return println!("clock: out of memory.") };
    if !screen.open() {
        return println!("clock: cannot take the screen.");
    }
    draw_grid(&mut screen, &g);
    let mut decoder = KeyDecoder::new();
    let mut message = Line::<48>::new();
    let mut message_until = 0u64;
    let started_us = tape_rt::micros();
    let mut next_sync = if mode == Mode::Auto { started_us + GRACE_US } else { started_us + resync_s * 1_000_000 };
    let mut was_fresh = false;
    let mut announced = time.source;
    let mut last_seconds = i64::MIN;
    let mut bars_were_bright = true;
    let bars_always = value(arguments, "bars") == Some("on");
    let mut bars_until = tape_rt::micros() + BARS_SECONDS * 1_000_000;
    let spin = mode != Mode::Ntp && arguments.split_whitespace().any(|w| w == "spin");
    loop {
        let mut sync_now = false;
        let keep_going = poll_keys(api, &mut decoder, |key| {
            message.clear();
            message_until = tape_rt::micros() + 4_000_000;
            bars_until = tape_rt::micros() + BARS_SECONDS * 1_000_000;
            match key {
                Key::Char(b'h') => view.twelve = !view.twelve,
                Key::Char(b'u') => view.utc = !view.utc,
                Key::Char(b'l') => view.lead = if view.lead == 0 { view.lead_on } else { 0 },
                Key::Char(b'n') => sync_now = true,
                Key::Char(b'k') => {
                    let _ = write!(message, "theme {}", theme::next().name);
                    draw_grid(&mut screen, &g);
                }
                Key::Char(b'w') => {
                    let mut body = tape_rt::config::Settings::<160>::new();
                    body.put("hours", format_args!("{}", if view.twelve { 12 } else { 24 }));
                    body.put("lead", format_args!("{}", view.lead));
                    body.put("utc", format_args!("{}", u8::from(view.utc)));
                    body.put("theme", format_args!("{}", theme().name));
                    match config.save(api, body.as_str()) {
                        Ok(name) => {
                            let _ = write!(message, "saved {name}");
                        }
                        Err(reason) => {
                            let _ = write!(message, "save: {reason}");
                        }
                    }
                }
                _ => {}
            }
            screen.mark_dirty();
        });
        if !keep_going {
            break;
        }
        if let Some(n) = network.as_mut() {
            let any_ptp = n.ptp.iter().any(Option::is_some);
            // A query in flight owns the receive ring: its reply must not be eaten by the PTP loop.
            if any_ptp && !n.ntp.busy() {
                ptp_poll(api, n, &mut time, zone_given, spin);
            }
            let ptp_ok = ptp_fresh(n, 0) || ptp_fresh(n, 1);
            if n.auto {
                if was_fresh && !ptp_ok {
                    println!("clock: no PTP master for {} s; asking NTP.", FRESH_US / 1_000_000);
                    next_sync = tape_rt::micros();
                }
                was_fresh = ptp_ok;
            }
            if n.has_ntp {
                let wanted = !n.auto || !ptp_ok;
                if sync_now && !wanted {
                    let _ = write!(message, "PTP is locked, NTP not needed");
                    message_until = tape_rt::micros() + 4_000_000;
                    screen.mark_dirty();
                }
                if wanted && (sync_now || tape_rt::micros() >= next_sync) && !n.ntp.busy() {
                    next_sync = tape_rt::micros() + resync_s * 1_000_000;
                    if n.auto {
                        println!("clock: no PTP master yet; asking NTP.");
                    }
                    n.ntp.begin();
                }
                if let Some(result) = n.ntp.poll(api, n.nic, &n.config, &mut n.arp) {
                    message.clear();
                    message_until = tape_rt::micros() + 4_000_000;
                    match result.ok_or("no answer from the NTP server").and_then(|sample| time.adopt(sample, zone_given)) {
                        Ok(()) => {
                            time.source = Source::Ntp;
                            let _ = write!(message, "synced");
                        }
                        Err(reason) => {
                            if n.auto {
                                next_sync = tape_rt::micros() + RETRY_US;
                            }
                            let _ = write!(message, "{reason}");
                        }
                    }
                    screen.mark_dirty();
                }
            } else if sync_now && any_ptp {
                let _ = write!(message, "PTP follows the master all the time");
                message_until = tape_rt::micros() + 4_000_000;
                screen.mark_dirty();
            }
            if time.source != announced {
                announced = time.source;
                match announced {
                    Source::PtpL2 => println!("clock: PTP locked to a master over Ethernet; the machine's clock is set."),
                    Source::PtpL3 => println!("clock: PTP locked to a master over UDP; the machine's clock is set."),
                    Source::Ntp if n.auto => println!("clock: time from NTP; the machine's clock is set."),
                    _ => {}
                }
            }
        }
        let seconds = time.shown_seconds(view.utc).unwrap_or(0);
        if seconds != last_seconds {
            last_seconds = seconds;
            screen.mark_dirty();
        }
        if !message.as_str().is_empty() && tape_rt::micros() >= message_until {
            message.clear();
            screen.mark_dirty();
        }
        let bright = bars_always || tape_rt::micros() < bars_until;
        if bright != bars_were_bright {
            bars_were_bright = bright;
            screen.mark_dirty();
        }
        if screen.due() {
            let mut canvas = screen.frame();
            draw_face(&mut canvas, &g, seconds, &view);
            draw_status(&mut canvas, &g, &view, &time, network.as_ref(), resync_s, message.as_str(), bright);
            screen.present();
        }
        // A query in flight is polled flat out: its reply's arrival time is what the offset is made from.
        if spin || network.as_ref().is_some_and(|n| n.ntp.busy()) {
            core::hint::spin_loop();
        } else {
            tape_rt::idle();
        }
    }
    screen.close();
    if let Some(Net { ptp, nic, config, .. }) = network.as_mut() {
        for ptp in ptp.iter_mut().flatten() {
            if ptp.l2 {
                l2_multicast(api, *nic, false);
            } else {
                ptp.igmp.leave(api, *nic, config);
            }
        }
    }
    println!("clock: stopped.");
}
