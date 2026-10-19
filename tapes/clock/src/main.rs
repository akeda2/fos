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
      [theme=dark|classic|light] [source=ntp|ptp] [domain=N] [l2] [spin] [lead=N]
A studio clock: 60 red dots for the seconds, the time in big yellow digits
(24 h by default) with the seconds below it, and the date. The time comes from
an NTP server: ntp=dhcp (the default) uses the server DHCP names (option 42),
else the gateway; an address or a host name (looked up at dns=, the DHCP DNS
server or the gateway) works too. ip=A.B.C.D/N gives a fixed address instead
of DHCP. The server is asked at start and every resync= seconds (default 900;
the picture keeps running meanwhile); the answer is kept
against the machine's microsecond clock and also sets the machine's clock
and RTC when that differs by a second or more. tz= is the local time zone in
hours east of UTC; without it the zone is taken from the machine's clock when
that is roughly right (the machine clock holds local time), else 0.
source=ptp follows a PTP master instead (l2: over Ethernet, ethertype 0x88F7,
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
Keys (Escape quits):
  h 12/24 hours   u UTC/local   n ask the server now   k theme   l lead
  w save the settings (hours, utc, theme, lead) in clock.cfg
Settings: clock.cfg is read at start (key=value words, # comments); `@NAME` on
the command line reads that file instead (any diskN/NAME) and w saves to it.
Words on the command line win over the file.
"#
);

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
}

/// The NTP server (or the PTP slave) and the interface to reach it on.
struct Net {
    nic: usize,
    config: net::NetConfig,
    server: Ipv4Address,
    arp: ArpCache<8>,
    ptp: Option<Ptp>,
    ntp: tape_ip::ntp::Client,
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

/// DHCP or a fixed address, then the server (named by DHCP, an address or a name). None with the reason printed.
fn connect(api: &TapeApi, arguments: &str, spec: &str, ptp: bool) -> Option<Net> {
    let nic = value(arguments, "nic").and_then(|v| v.parse::<usize>().ok()).map_or(tape_rt::active_nic(), |n| n.saturating_sub(1));
    if (api.nic_count)() <= nic {
        println!("clock: this machine has no NIC {}.", nic + 1);
        return None;
    }
    let mut mac = [0u8; 6];
    (api.nic_mac)(nic, mac.as_mut_ptr());
    let ip = value(arguments, "ip").unwrap_or("dhcp");
    let l2 = ptp && arguments.split_whitespace().any(|w| w == "l2");
    let (config, dhcp_ntp, dhcp_dns) = if l2 {
        (net::NetConfig { mac: MacAddress(mac), ip: Ipv4Address([0; 4]), netmask: Ipv4Address([0; 4]), gateway: Ipv4Address([0; 4]) }, None, None)
    } else if ip == "dhcp" {
        println!("clock: asking DHCP for an address on NIC {}...", nic + 1);
        match net::dhcp_with_servers(api, nic, MacAddress(mac)) {
            Some(found) => found,
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
    let mut arp = ArpCache::<8>::new();
    if ptp {
        let domain = value(arguments, "domain").and_then(|v| v.parse::<u8>().ok()).unwrap_or(0);
        let mut igmp = igmp::Igmp::new(GROUP);
        if l2 {
            l2_multicast(api, nic, true);
            println!("clock: PTP slave on NIC {} over Ethernet (0x88F7), domain {}; waiting for a master.", nic + 1, domain);
        } else {
            igmp.join(api, nic, &config);
            println!("clock: PTP slave on NIC {} ({}.{}.{}.{}), domain {}; waiting for a master.", nic + 1, config.ip.0[0], config.ip.0[1], config.ip.0[2], config.ip.0[3], domain);
        }
        let slave = Slave::new(domain, port_identity(mac, 1), 1_000_000_000);
        let ptp = Ptp { slave, igmp, l2, domain, identity: port_identity(mac, 1), last_empty: 0 };
        return Some(Net { nic, config, server: Ipv4Address([0; 4]), arp, ptp: Some(ptp), ntp: tape_ip::ntp::Client::new(Ipv4Address([0; 4])) });
    }
    let server = if spec == "dhcp" {
        match dhcp_ntp {
            Some(server) => server,
            None if config.gateway.0 != [0; 4] => {
                println!("clock: DHCP names no NTP server (option 42); trying the gateway.");
                config.gateway
            }
            None => {
                println!("clock: DHCP names no NTP server and there is no gateway; give ntp=A.B.C.D. Showing the machine's clock.");
                return None;
            }
        }
    } else if let Some(server) = net::parse_ip(spec) {
        server
    } else {
        let dns = value(arguments, "dns").and_then(net::parse_ip).or(dhcp_dns).or((config.gateway.0 != [0; 4]).then_some(config.gateway));
        let Some(dns) = dns else {
            println!("clock: {spec} is a name and no DNS server is known (dns=A.B.C.D). Showing the machine's clock.");
            return None;
        };
        match tape_ip::dns::lookup(api, nic, &config, &mut arp, dns, tape_net::dns::PORT, spec) {
            Ok(server) => server,
            Err(reason) => {
                println!("clock: {spec}: {reason}. Showing the machine's clock.");
                return None;
            }
        }
    };
    let s = server.0;
    println!("clock: NTP server {}.{}.{}.{} (address {}.{}.{}.{}).", s[0], s[1], s[2], s[3], config.ip.0[0], config.ip.0[1], config.ip.0[2], config.ip.0[3]);
    Some(Net { nic, config, server, arp, ptp: None, ntp: tape_ip::ntp::Client::new(server) })
}

/// Asks the server and adopts the answer. The reason for a failure is ready to show.
fn sync(api: &TapeApi, network: &mut Net, time: &mut Time, zone_given: Option<i32>) -> Result<(), &'static str> {
    let sample = tape_ip::ntp::query(api, network.nic, &network.config, &mut network.arp, network.server).ok_or("no answer from the NTP server")?;
    time.adopt(sample, zone_given)
}

/// Handles what PTP has to say: reads the frames that came in, adopts the master's time, and sends a Delay_Req when one is due.
/// Returns true when a new measurement was adopted. Called every pass of the main loop: the receive times are the clock when the
/// frame is picked up; with `spin` a Sync that waited in the ring is discarded, when sleeping every Sync is late and none is.
fn ptp_poll(api: &TapeApi, network: &mut Net, time: &mut Time, zone_given: Option<i32>, spin: bool) -> bool {
    let Net { nic, config, arp, ptp, .. } = network;
    let Some(ptp) = ptp.as_mut() else { return false };
    let mut frame = [0u8; net::MAX_FRAME];
    let mut adopted = false;
    for _ in 0..8 {
        let length = (api.nic_receive)(*nic, frame.as_mut_ptr(), frame.len());
        if length == 0 {
            ptp.last_empty = local_ns();
            break;
        }
        let t2 = local_ns();
        let (is_event, payload) = if ptp.l2 {
            let Some(message) = ptp_tape::l2_message(&frame[..length]) else { continue };
            (ptp_tape::is_event(message), message)
        } else {
            if ptp.igmp.handle(&frame[..length], t2 as u64 / 1000) {
                continue;
            }
            let net::Inbound::Udp { destination_port, payload, .. } = net::inbound(api, *nic, config, arp, &frame[..length], EVENT_PORT..=GENERAL_PORT, 0) else { continue };
            (destination_port == EVENT_PORT, payload)
        };
        let is_sync = payload.first().is_some_and(|byte| byte & 0x0f == ptp_tape::SYNC);
        let sample = if spin && is_sync && is_event && t2 - ptp.last_empty > LATE_NS {
            ptp.slave.discard_sync(payload);
            None
        } else {
            ptp.slave.handle(payload, t2)
        };
        let Some(sample) = sample else { continue };
        let Some((_, master)) = ptp.slave.master() else { continue };
        if !master.has_real_time() {
            continue;
        }
        // sample.offset is the local clock minus the master's (TAI); the model wants Unix minus local.
        let mut offset = master.unix_ns(t2 - sample.offset) - t2;
        // A single measurement jitters by a few hundred microseconds: follow it slowly, but jump when the master moved.
        if let Some(old) = time.offset_ns.filter(|old| (offset - old).abs() < 50_000_000) {
            offset = old + (offset - old) / 8;
        }
        if time.adopt(ntp::Sample { offset, delay: sample.delay }, zone_given).is_ok() {
            adopted = true;
        }
    }
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
    let unlit = mix(theme().background, dot_red(), 2, 9);
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
    for index in 0..=u32::from(date.second) {
        let (x, y) = dial::ring_position(index, g.cx, g.cy, g.ring);
        dial::fill_disc(canvas, x, y, if index % 5 == 0 { g.big } else { g.small }, dot_red());
    }
    // The next 5-second mark is lit ahead of the count for `lead` seconds after the previous one, dark again for the
    // rest, and lit for good when the count arrives. (Before the top of the minute the 12 o'clock dot is lit already.)
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

fn draw_status(canvas: &mut gfx::Canvas, g: &Geometry, view: &View, time: &Time, network: Option<&Net>, resync_s: u64, message: &str) {
    let mut line = Line::<200>::new();
    let zone = time.zone.unwrap_or(0);
    let _ = write!(line, "CLOCK  {}  {}", if view.twelve { "12 h" } else { "24 h" }, if view.utc { "UTC" } else { "local" });
    if !view.utc && time.zone.is_some() {
        let _ = write!(line, " UTC{}{}:{:02}", if zone < 0 { '-' } else { '+' }, zone.abs() / 60, zone.abs() % 60);
    }
    let mut stale = false;
    match (network, time.last_ok_us) {
        (Some(Net { ptp: Some(ptp), .. }), ok) => {
            let _ = write!(line, "  PTP domain {}", ptp.domain);
            match ok {
                Some(ok) => {
                    let age = tape_rt::micros().wrapping_sub(ok) / 1_000_000;
                    let _ = write!(line, "  locked, last Sync {age} s ago  delay {} us", time.delay_ns / 1000);
                    stale = age > 10;
                }
                None => {
                    let _ = write!(line, "  no master with real time yet, machine clock");
                    stale = true;
                }
            }
        }
        (Some(n), Some(ok)) => {
            let age = tape_rt::micros().wrapping_sub(ok) / 1_000_000;
            let s = n.server.0;
            let _ = write!(line, "  NTP {}.{}.{}.{}  synced {age} s ago  rtt {} us", s[0], s[1], s[2], s[3], time.delay_ns / 1000);
            if let Some(step) = time.step_ns {
                let _ = write!(line, "  step {:+} us", step / 1000);
            }
            stale = age > 3 * resync_s;
        }
        (Some(_), None) => {
            let _ = write!(line, "  NTP: no answer yet, machine clock");
            stale = true;
        }
        (None, _) => {
            let _ = write!(line, "  machine clock");
        }
    }
    canvas.text(16, 6, line.as_str(), if stale { theme().amber } else { theme().text });
    if !message.is_empty() {
        canvas.text_right(g.width - 16, 6, message, theme().amber);
    }
    canvas.text(16, g.height - 18, "h 12/24 h  u UTC/local  n sync now  k theme  l lead  w save  Esc", theme().dim);
}

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(api: &'static TapeApi) {
    tape_rt::init(api);
    if tape_rt::fp_level() == 0 {
        return println!("clock: needs a hard-float tape build (kernel without FP support?).");
    }
    let Some((width, height)) = tape_rt::gfx_info() else {
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
    let mut time = Time { offset_ns: None, zone: zone_given, last_ok_us: None, delay_ns: 0, step_ns: None };
    let ptp = value(arguments, "source") == Some("ptp");
    let mut network = if spec == "off" && !ptp { None } else { connect(api, arguments, spec, ptp) };
    if let Some(n) = network.as_mut().filter(|n| n.ptp.is_none()) {
        match sync(api, n, &mut time, zone_given) {
            Ok(()) => {
                let date = ntp::civil_from_unix(time.shown_seconds(false).unwrap_or(0));
                println!("clock: NTP time {:04}-{:02}-{:02} {:02}:{:02}:{:02} (zone UTC{:+}h, round trip {} us); the machine's clock is set.", date.year, date.month, date.day, date.hour, date.minute, date.second, time.zone.unwrap_or(0) / 60, time.delay_ns / 1000);
            }
            Err(reason) => println!("clock: {reason}; running on the machine's clock until it answers."),
        }
    }
    if time.shown_seconds(false).is_none() {
        return println!("clock: the machine's clock is not set and there is no NTP time (date set, or ntp=).");
    }

    let g = Geometry::new(width.min(MAX_WIDTH), height.min(MAX_HEIGHT));
    let Some(mut screen) = Screen::new(api, g.width as u32, g.height as u32) else { return println!("clock: out of memory.") };
    if !screen.open() {
        return println!("clock: cannot take the screen.");
    }
    draw_grid(&mut screen, &g);
    let mut decoder = KeyDecoder::new();
    let mut message = Line::<48>::new();
    let mut message_until = 0u64;
    let mut next_sync = tape_rt::micros() + resync_s * 1_000_000;
    let mut last_seconds = i64::MIN;
    let spin = ptp && arguments.split_whitespace().any(|w| w == "spin");
    loop {
        let mut sync_now = false;
        let keep_going = poll_keys(api, &mut decoder, |key| {
            message.clear();
            message_until = tape_rt::micros() + 4_000_000;
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
        if let Some(n) = network.as_mut().filter(|n| n.ptp.is_some()) {
            if ptp_poll(api, n, &mut time, zone_given, spin) && time.step_ns.is_none() {
                println!("clock: PTP locked to a master; the machine's clock is set.");
            }
            if sync_now {
                let _ = write!(message, "PTP follows the master all the time");
                message_until = tape_rt::micros() + 4_000_000;
                screen.mark_dirty();
            }
        } else if let Some(n) = network.as_mut() {
            if (sync_now || tape_rt::micros() >= next_sync) && !n.ntp.busy() {
                next_sync = tape_rt::micros() + resync_s * 1_000_000;
                n.ntp.begin();
            }
            if let Some(result) = n.ntp.poll(api, n.nic, &n.config, &mut n.arp) {
                message.clear();
                message_until = tape_rt::micros() + 4_000_000;
                match result.ok_or("no answer from the NTP server").and_then(|sample| time.adopt(sample, zone_given)) {
                    Ok(()) => {
                        let _ = write!(message, "synced");
                    }
                    Err(reason) => {
                        let _ = write!(message, "{reason}");
                    }
                }
                screen.mark_dirty();
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
        if screen.due() {
            let mut canvas = screen.frame();
            draw_face(&mut canvas, &g, seconds, &view);
            draw_status(&mut canvas, &g, &view, &time, network.as_ref(), resync_s, message.as_str());
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
    if let Some(Net { ptp: Some(ptp), nic, config, .. }) = network.as_mut() {
        if ptp.l2 {
            l2_multicast(api, *nic, false);
        } else {
            ptp.igmp.leave(api, *nic, config);
        }
    }
    println!("clock: stopped.");
}
