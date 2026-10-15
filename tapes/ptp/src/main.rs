#![no_std]
#![no_main]
#![allow(dead_code)]

//! `ptp`: a PTPv2 (IEEE 1588-2008) clock over UDP/IPv4, the transport AES67 uses.
//!
//! ```text
//! tape run ptp [slave|master] [nic=N] [ip=dhcp|A.B.C.D/N] [gateway=A.B.C.D] [domain=N]
//!              [l2] [ntp=dhcp|A.B.C.D|NAME] [dns=A.B.C.D] [settime] [tz=+H[:MM]] [secs=N] [delay=SECONDS] [sync=MS] [prio1=N] [late=US] [spin] [verbose]
//! ```
//!
//! `slave` (the default) listens for Announce, Sync and Follow_Up on the PTP multicast group (224.0.1.129),
//! answers with Delay_Req and measures, once a second, how far this machine's clock (`micros()`) is from the
//! master's: the offset after the first sample's step, its spread, the one-way path delay and the rate
//! difference in ppm. It does not steer anything yet; the point is to learn what a software timestamp is
//! worth on this NIC and network before audio-net is tied to it.
//!
//! `master` announces itself (clock class 248, priority 128, an internal oscillator), sends two-step Syncs
//! every `sync=` ms (default 125, the AES67 media-profile rate) and answers Delay_Req. Its time is the time
//! since boot. Another machine can be a master too (`ptp4l -i IF -S -m` on a Linux box).
//!
//! `l2` speaks PTP over Ethernet instead (IEEE 1588 Annex F, `ptp4l -2`): ethertype 0x88F7 frames to
//! 01:1B:19:00:00:00, no IP address, DHCP or IGMP, so it also works on a bare switch or a link with no router.
//!
//! `ntp=` (master): before announcing, ask an NTP server the time (`dhcp` is the server DHCP names in option 42, else the
//! gateway; or an address or a host name, which the DNS server from DHCP option 6, `dns=A.B.C.D` or else the gateway resolves) and make the master's PTP time real: TAI, so Unix time plus 37 s, with the Announce saying timeSource NTP
//! and the UTC offset. Without it the master counts from boot and its slaves know that. The master does not follow NTP afterwards
//! (a PTP master that stepped would unlock its slaves); restart it to resync, an oscillator drifts a few seconds a day.
//! With `l2` the NTP exchange still needs an IP address (`ip=`), used for that one exchange.
//!
//! `settime` (slave): once locked (8 samples) to a master that has real time, set this machine's clock and RTC from it, then the tape ends.
//! Without `secs=` it gives up after 60 s so a start file is not held up by a master that is down.
//! The RTC holds local time, so say the zone with `tz=` (hours east of UTC: `+2`, `-5`, `+5:30`; default 0, which is UTC).
//!
//! Timestamps are taken in software: receive time when the polling loop reads the frame, transmit time just
//! before the frame is handed to the NIC. A master by default (a slave must not: wake-up latency makes its receive times `late`) sleeps between polls (`tape_rt::idle`): the CPU halts
//! until the NIC's interrupt (about 10 us of wake-up added to the receive time on clock1's MSI e1000e; measured), or on a
//! polled NIC waits in 100 us slices. `spin` polls flat out (the slave default): the smallest error, a core at full speed (and warm). `idle` on a slave trades accuracy for a cool CPU.
//!
//! A Sync that sat in the NIC's ring while the loop was busy (a print, a send) would carry a receive time that
//! is too late; when the last empty poll is more than `late=` us (default 300) before the frame, the Sync is
//! counted as `late` and not measured.
//!
//! Escape stops it. The protocol and the arithmetic live in `lib.rs`, with host tests (`cargo test --lib`).

use tape_ip::{igmp, net};

use ptp_tape::{Clock, DELAY_REQ, EVENT_PORT, GENERAL_PORT, GROUP, Header, Rate, Slave, Window, build_announce, build_delay_req, build_delay_resp, build_follow_up, build_sync, interval_ns, port_identity};
use tape_net::{ArpCache, Ipv4Address, MacAddress};
use tape_rt::{TapeApi, println};

struct Options {
    master: bool,
    nic: usize,
    ip: &'static str,
    gateway: &'static str,
    domain: u8,
    seconds: u32,
    delay_seconds: u32,
    sync_ms: u32,
    priority1: u8,
    late_us: i64,
    idle: bool,
    verbose: bool,
    l2: bool,
    both: bool,
    ntp: &'static str,
    dns: &'static str,
    settime: bool,
    zone_minutes: i32,
    software: bool,
}

const SETTIME_GIVE_UP_SECONDS: u32 = 60;

fn parse(arguments: &'static str) -> Result<Options, &'static str> {
    let mut options = Options { master: false, nic: tape_rt::active_nic(), ip: "dhcp", gateway: "", domain: 0, seconds: 0, delay_seconds: 1, sync_ms: 125, priority1: 128, late_us: 300, idle: true, verbose: false, l2: false, both: false, ntp: "", dns: "", settime: false, zone_minutes: 0, software: false };
    let mut idle_set = None;
    for word in arguments.split_whitespace() {
        let (key, value) = word.split_once('=').unwrap_or((word, ""));
        match key {
            "slave" => options.master = false,
            "master" => options.master = true,
            "idle" => idle_set = Some(true),
            "spin" => idle_set = Some(false),
            "l2" => options.l2 = true,
            "both" => options.both = true,
            "verbose" => options.verbose = true,
            "software" => options.software = true,
            "settime" => options.settime = true,
            "ntp" => options.ntp = if value.is_empty() { "dhcp" } else { value },
            "dns" => options.dns = value,
            "tz" => options.zone_minutes = tape_net::ntp::parse_zone(value).ok_or("tz= is hours east of UTC, like +2, -5 or +5:30")?,
            "nic" => options.nic = value.parse::<usize>().ok().filter(|&n| n >= 1).ok_or("nic= is the NIC number, from 1")? - 1,
            "ip" => options.ip = value,
            "gateway" => options.gateway = value,
            "domain" => options.domain = value.parse().map_err(|_| "domain= is 0 to 255")?,
            "secs" => options.seconds = value.parse().map_err(|_| "secs= is a number of seconds")?,
            "delay" => options.delay_seconds = value.parse::<u32>().ok().filter(|&n| n >= 1).ok_or("delay= is seconds between Delay_Req, at least 1")?,
            "sync" => options.sync_ms = value.parse::<u32>().ok().filter(|&n| (30..=2000).contains(&n)).ok_or("sync= is milliseconds, 30 to 2000")?,
            "late" => options.late_us = value.parse::<i64>().ok().filter(|&n| n >= 20).ok_or("late= is microseconds, at least 20")?,
            "prio1" => options.priority1 = value.parse().map_err(|_| "prio1= is 0 to 255")?,
            _ => return Err("unknown option"),
        }
    }
    if options.both && (!options.master || options.l2) {
        return Err("both is for a master, on UDP and Ethernet together (not with l2)");
    }
    options.idle = idle_set.unwrap_or(options.master);
    Ok(options)
}

/// Added to the local clock for the master's PTP time: zero (time since boot) until NTP says what the date is.
static EPOCH_NS: core::sync::atomic::AtomicI64 = core::sync::atomic::AtomicI64::new(0);

/// The interface whose hardware timestamps are in use, when it has them (set once at start).
static HW_NIC: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(usize::MAX);

/// A card's stamp is a count of its own clock (40 bits are enough to measure back from now).
const STAMP_MASK: u64 = (1 << 40) - 1;

/// Moves a hardware stamp onto the tape's clock: read the card's clock between two reads of ours (the middle is its
/// moment on our timeline, to about a microsecond) and subtract how long ago the stamp was. The polling loop's
/// delay between the frame's arrival and this call drops out, which is the point of the card's stamp.
fn hw_to_local(stamp: u64) -> i64 {
    let nic = HW_NIC.load(core::sync::atomic::Ordering::Relaxed);
    let before = now_ns();
    let card = tape_rt::nic_ts_clock(nic);
    let after = now_ns();
    (before + after) / 2 - (card.wrapping_sub(stamp) & STAMP_MASK) as i64
}

/// Arms the card's timestamps unless `software` was asked for; says which kind this run measures with.
fn start_hardware_stamps(options: &Options) {
    if !options.software && tape_rt::nic_ts_enable(options.nic) {
        HW_NIC.store(options.nic, core::sync::atomic::Ordering::Relaxed);
        println!("ptp: hardware timestamps on NIC {} (the card stamps PTP event frames).", options.nic + 1);
    } else {
        println!("ptp: software timestamps (the polling loop reads the clock){}.", if options.software { ", as asked" } else { ": this NIC has no hardware timestamps" });
    }
}

/// The time a frame just returned by `nic_receive` arrived: the card's stamp when it has one, else now.
fn receive_time(nic: usize) -> (i64, bool) {
    if HW_NIC.load(core::sync::atomic::Ordering::Relaxed) == nic {
        let stamp = tape_rt::nic_ts_rx(nic);
        if stamp != 0 {
            return (hw_to_local(stamp), true);
        }
    }
    (now_ns(), false)
}

fn now_ns() -> i64 {
    tape_rt::micros() as i64 * 1000 + EPOCH_NS.load(core::sync::atomic::Ordering::Relaxed)
}

struct Shown(tape_net::ntp::Civil);

impl core::fmt::Display for Shown {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let t = &self.0;
        write!(f, "{:04}-{:02}-{:02} {:02}:{:02}:{:02}", t.year, t.month, t.day, t.hour, t.minute, t.second)
    }
}

/// Asks the NTP server and, if it answers, makes `now_ns()` the TAI time. True if the master's time is now real.
fn adopt_ntp_time(api: &TapeApi, options: &Options, config: &net::NetConfig, from_dhcp: Option<Ipv4Address>, dhcp_dns: Option<Ipv4Address>) -> bool {
    let server = if options.ntp == "dhcp" {
        match from_dhcp {
            Some(server) => server,
            None if config.gateway.0 != [0; 4] => {
                println!("ptp: DHCP names no NTP server (option 42); trying the gateway.");
                config.gateway
            }
            None => {
                println!("ptp: DHCP names no NTP server and there is no gateway; give ntp=A.B.C.D. The master counts from boot.");
                return false;
            }
        }
    } else {
        match net::parse_ip(options.ntp) {
            Some(server) => server,
            None => {
                let dns = options.dns.is_empty().then_some(dhcp_dns).flatten().or_else(|| net::parse_ip(options.dns)).or((config.gateway.0 != [0; 4]).then_some(config.gateway));
                let Some(dns) = dns else {
                    println!("ptp: {} is a name and no DNS server is known (dns=A.B.C.D). The master counts from boot.", options.ntp);
                    return false;
                };
                println!("ptp: asking DNS server {}.{}.{}.{} for {}...", dns.0[0], dns.0[1], dns.0[2], dns.0[3], options.ntp);
                let mut arp = ArpCache::<8>::new();
                match tape_ip::dns::lookup(api, options.nic, config, &mut arp, dns, tape_net::dns::PORT, options.ntp) {
                    Ok(server) => server,
                    Err(reason) => {
                        println!("ptp: {}: {reason}. The master counts from boot.", options.ntp);
                        return false;
                    }
                }
            }
        }
    };
    println!("ptp: asking NTP server {}.{}.{}.{} for the time...", server.0[0], server.0[1], server.0[2], server.0[3]);
    let mut arp = ArpCache::<8>::new();
    let Some(sample) = tape_ip::ntp::query(api, options.nic, config, &mut arp, server) else {
        println!("ptp: no answer from the NTP server. The master counts from boot, so slaves will not set their clocks from it.");
        return false;
    };
    let local = tape_rt::micros() as i64 * 1000;
    let date = tape_net::ntp::civil_from_unix((local + sample.offset) / 1_000_000_000);
    if date.year < 2024 {
        println!("ptp: the NTP server says {}, which cannot be right. The master counts from boot.", Shown(date));
        return false;
    }
    EPOCH_NS.store(sample.offset + tape_net::ntp::TAI_MINUS_UTC * 1_000_000_000, core::sync::atomic::Ordering::Relaxed);
    println!("ptp: NTP time {} UTC (round trip {} us). The master's PTP time is now TAI from it.", Shown(date), Us(sample.delay));
    true
}

/// Signed nanoseconds shown as microseconds with one decimal.
struct Us(i64);

impl core::fmt::Display for Us {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let tenths = (self.0.unsigned_abs() + 50) / 100;
        let mut text = [0u8; 24];
        let mut length = 0;
        let mut put = |byte: u8| {
            text[length] = byte;
            length += 1;
        };
        if self.0 < 0 && tenths != 0 {
            put(b'-');
        }
        let mut digits = [0u8; 20];
        let mut count = 0;
        let mut whole = tenths / 10;
        loop {
            digits[count] = b'0' + (whole % 10) as u8;
            count += 1;
            whole /= 10;
            if whole == 0 {
                break;
            }
        }
        for index in (0..count).rev() {
            put(digits[index]);
        }
        put(b'.');
        put(b'0' + (tenths % 10) as u8);
        f.pad(core::str::from_utf8(&text[..length]).unwrap_or("?"))
    }
}

static L2: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

fn l2() -> bool {
    L2.load(core::sync::atomic::Ordering::Relaxed)
}

/// The PTP message in a received frame, and whether it is an event message, whichever transport is in use.
fn incoming<'a>(l2: bool, api: &TapeApi, nic: usize, config: &net::NetConfig, arp: &mut ArpCache<8>, frame: &'a [u8]) -> Option<(bool, &'a [u8])> {
    if l2 {
        return ptp_tape::l2_message(frame).map(|message| (ptp_tape::is_event(message), message));
    }
    match net::inbound(api, nic, config, arp, frame, EVENT_PORT..=GENERAL_PORT, 0) {
        net::Inbound::Udp { destination_port, payload, .. } => Some((destination_port == EVENT_PORT, payload)),
        _ => None,
    }
}

/// Sends one PTP message; returns the time it left: the card's transmit stamp when it has one, else the clock just
/// before the frame was handed over.
fn send(l2: bool, api: &TapeApi, nic: usize, config: &net::NetConfig, port: u16, payload: &[u8]) -> i64 {
    let mut frame = [0u8; 160];
    let length = if l2 {
        ptp_tape::build_l2_frame(&mut frame, config.mac.0, payload)
    } else {
        let destination = MacAddress(audio_wire::multicast_mac(GROUP));
        net::build_udp_frame(&mut frame, config, destination, Ipv4Address(GROUP), port, port, 1, payload).unwrap_or(0)
    };
    if length == 0 {
        return now_ns();
    }
    let before = now_ns();
    if HW_NIC.load(core::sync::atomic::Ordering::Relaxed) == nic {
        let (_, stamp) = tape_rt::nic_send_stamped(nic, &frame[..length]);
        if stamp != 0 {
            return hw_to_local(stamp);
        }
        return before;
    }
    net::send_frame(api, nic, &frame[..length]);
    before
}

/// Waits between polls: sleeps (master default, or `idle`), or spins (slave default, `spin`: the smallest receive-time error).
fn wait(options: &Options) {
    if options.idle {
        tape_rt::idle();
    } else {
        core::hint::spin_loop();
    }
}

fn mac_text(id: &[u8]) -> [u8; 23] {
    let mut text = [b':'; 23];
    for (index, byte) in id.iter().take(8).enumerate() {
        text[index * 3] = b"0123456789abcdef"[usize::from(byte >> 4)];
        text[index * 3 + 1] = b"0123456789abcdef"[usize::from(byte & 15)];
    }
    text
}

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(api: &'static TapeApi) {
    tape_rt::init(api);
    let arguments = core::str::from_utf8(tape_rt::args()).unwrap_or("");
    let options = match parse(arguments) {
        Ok(options) => options,
        Err(reason) => return println!("ptp: {reason}. Usage: tape run ptp [slave|master] [nic=N] [ip=dhcp|A.B.C.D/N] [secs=N] ..."),
    };
    if (api.nic_count)() <= options.nic {
        return println!("ptp: this machine has no NIC {}.", options.nic + 1);
    }
    let mut mac = [0u8; 6];
    (api.nic_mac)(options.nic, mac.as_mut_ptr());
    L2.store(options.l2, core::sync::atomic::Ordering::Relaxed);
    if !options.master && !options.ntp.is_empty() {
        return println!("ptp: ntp= is for the master; a slave takes its time from the master (settime).");
    }
    let wants_ntp = !options.ntp.is_empty();
    let mut dhcp_ntp = None;
    let mut dhcp_dns = None;
    let config = if options.l2 && !wants_ntp {
        net::NetConfig { mac: MacAddress(mac), ip: Ipv4Address([0; 4]), netmask: Ipv4Address([0; 4]), gateway: Ipv4Address([0; 4]) }
    } else if options.ip == "dhcp" {
        println!("ptp: asking DHCP for an address on NIC {}...", options.nic + 1);
        match net::dhcp_with_servers(api, options.nic, MacAddress(mac)) {
            Some((config, ntp, dns)) => {
                dhcp_ntp = ntp;
                dhcp_dns = dns;
                config
            }
            None => return println!("ptp: no DHCP answer; give ip=A.B.C.D/N instead."),
        }
    } else {
        match net::static_config(MacAddress(mac), options.ip, options.gateway) {
            Ok(config) => config,
            Err(reason) => return println!("ptp: {reason}."),
        }
    };
    if options.l2 {
        println!("ptp {}: NIC {}, Ethernet (0x88F7 to 01:1B:19:00:00:00), domain {}. Escape stops.", if options.master { "master" } else { "slave" }, options.nic + 1, options.domain);
        (api.nic_set_multicast)(options.nic, 1);
    } else {
        if options.both {
            (api.nic_set_multicast)(options.nic, 1);
        }
        println!("ptp {}: NIC {} {}.{}.{}.{}, domain {}, group 224.0.1.129{}. Escape stops.", if options.master { "master" } else { "slave" }, options.nic + 1, config.ip.0[0], config.ip.0[1], config.ip.0[2], config.ip.0[3], options.domain, if options.both { " and Ethernet (0x88F7)" } else { "" });
    }
    let mut igmp = igmp::Igmp::new(GROUP);
    if !options.l2 {
        igmp.join(api, options.nic, &config);
    }
    let ntp_synced = wants_ntp && adopt_ntp_time(api, &options, &config, dhcp_ntp, dhcp_dns);
    if options.master {
        master(api, &options, &config, mac, ntp_synced);
    } else {
        slave(api, &options, &config, mac);
    }
    if options.l2 || options.both {
        (api.nic_set_multicast)(options.nic, 0);
    }
    if !options.l2 {
        igmp.leave(api, options.nic, &config);
    }
}

fn slave(api: &TapeApi, options: &Options, config: &net::NetConfig, mac: [u8; 6]) {
    let identity = port_identity(mac, 1);
    let mut slave = Slave::new(options.domain, identity, i64::from(options.delay_seconds) * 1_000_000_000);
    let mut igmp = igmp::Igmp::new(GROUP);
    let mut arp = ArpCache::<8>::new();
    let (mut second, mut total) = (Window::default(), Window::default());
    let mut delays = Window::default();
    let mut rate = Rate::new();
    let (mut jitter_sum, mut jitter_seconds, mut last_rate) = (0i64, 0i64, None);
    let mut frame = [0u8; net::MAX_FRAME];
    let start = now_ns();
    let mut next_report = start + 1_000_000_000;
    let mut masters_seen = 0;
    let mut steps_seen = 0;
    let mut silent = 0u32;
    let mut last_empty = now_ns();
    let (mut time_set, mut refusal_shown) = (false, false);
    start_hardware_stamps(options);
    println!("ptp: waiting for an Announce...");
    // A start-file `ptp settime` must not hold the shell for ever when the master is down.
    let limit = if options.seconds == 0 && options.settime { SETTIME_GIVE_UP_SECONDS } else { options.seconds };
    loop {
        let now = now_ns();
        if (api.should_abort)() != 0 || (options.settime && time_set) || (limit != 0 && now - start >= i64::from(limit) * 1_000_000_000) {
            if options.settime && !time_set && limit != 0 && now - start >= i64::from(limit) * 1_000_000_000 {
                println!("ptp: settime: no usable master within {limit} s; the clock is unchanged.");
            }
            break;
        }
        let mut busy = false;
        for _ in 0..8 {
            let length = (api.nic_receive)(options.nic, frame.as_mut_ptr(), frame.len());
            if length == 0 {
                last_empty = now_ns();
                break;
            }
            // The receive time is read as soon as the frame is in hand. If the last poll that found nothing is
            // long ago (a print, a send), the frame may have waited and its time is too late to measure with.
            let (t2, hardware) = receive_time(options.nic);
            let late = !hardware && t2 - last_empty > options.late_us * 1000;
            busy = true;
            if !l2() && igmp.handle(&frame[..length], t2 as u64 / 1000) {
                continue;
            }
            if let Some((_, payload)) = incoming(l2(), api, options.nic, config, &mut arp, &frame[..length]) {
                let is_sync = payload.first().is_some_and(|byte| byte & 0x0f == ptp_tape::SYNC);
                let sample = if late && is_sync {
                    slave.discard_sync(payload);
                    None
                } else {
                    slave.handle(payload, t2)
                };
                if let Some(sample) = sample {
                    second.add(sample.residual);
                    total.add(sample.residual);
                    delays.add(sample.delay);
                    rate.add(t2, sample.offset);
                    if options.settime && !time_set && total.count >= 8 {
                        time_set = set_clock_from_master(&slave, options, t2 - sample.offset, &mut refusal_shown);
                    }
                    if options.verbose {
                        println!("  seq {} offset {} us (raw {} ms) delay {} us", sample.sequence, Us(sample.residual), sample.offset / 1_000_000, Us(sample.delay));
                    }
                }
            }
        }
        if !l2() {
            igmp.poll(api, options.nic, config, (now / 1000) as u64);
        }
        slave.tick(now);
        if slave.counters.masters != masters_seen {
            masters_seen = slave.counters.masters;
            rate.reset();
            if let Some((source, clock)) = slave.master() {
                let text = mac_text(&source);
                println!("ptp: master {} priority {}/{} class {}", core::str::from_utf8(&text).unwrap_or("?"), clock.priority1, clock.priority2, clock.class);
            }
        }
        if slave.counters.steps != steps_seen {
            steps_seen = slave.counters.steps;
            rate.reset();
            println!("ptp: the master's clock stepped; measuring from the new offset.");
        }
        if let Some(sequence) = slave.request_due(now) {
            let mut message = [0u8; 64];
            let length = build_delay_req(&mut message, options.domain, identity, sequence);
            let t3 = send(l2(), api, options.nic, config, EVENT_PORT, &message[..length]);
            slave.request_sent(sequence, t3);
        }
        if now >= next_report {
            next_report += 1_000_000_000;
            let seconds = (now - start) / 1_000_000_000;
            if second.count > 1 {
                jitter_sum += second.deviation();
                jitter_seconds += 1;
            }
            last_rate = rate.tenths_ppm().or(last_rate);
            if second.count > 0 {
                match rate.tenths_ppm() {
                    Some(tenths) => println!("{:>4}s off {:>7} sd {:>6} [{:>7},{:>7}] dly {:>6} {:+}.{} ppm n{}", seconds, Us(second.mean()), Us(second.deviation()), Us(second.min), Us(second.max), Us(slave.path_delay().unwrap_or(0)), tenths / 10, (tenths % 10).abs(), second.count),
                    None => println!("{:>4}s off {:>7} sd {:>6} [{:>7},{:>7}] dly {:>6} n{}", seconds, Us(second.mean()), Us(second.deviation()), Us(second.min), Us(second.max), Us(slave.path_delay().unwrap_or(0)), second.count),
                }
                silent = 0;
            } else {
                silent += 1;
                if silent == 5 || silent % 30 == 0 {
                    let c = slave.counters;
                    println!("{:>4}s no sample yet: announce {} sync {} follow-up {} delay-resp {}", seconds, c.announces, c.syncs, c.follow_ups, c.delay_resps);
                }
            }
            second = Window::default();
        }
        if !busy {
            wait(options);
        }
    }
    let c = slave.counters;
    println!("ptp: {} samples; announce {} sync {} follow-up {} delay-resp {} sync lost {} late {} steps {}", total.count, c.announces, c.syncs, c.follow_ups, c.delay_resps, c.sync_lost, c.late, c.steps);
    if total.count > 0 {
        println!("ptp: offset mean {} us, sd {} us, min {} us, max {} us; path delay {} us (min {}, max {})", Us(total.mean()), Us(total.deviation()), Us(total.min), Us(total.max), Us(delays.mean()), Us(delays.min), Us(delays.max));
        // The overall spread includes any rate difference between the clocks; the per-second spread is the jitter.
        let jitter = if jitter_seconds > 0 { jitter_sum / jitter_seconds } else { 0 };
        match last_rate {
            Some(tenths) => println!("ptp: jitter {} us (mean of the per-second sd); clock rate {:+}.{} ppm against the master", Us(jitter), tenths / 10, (tenths % 10).abs()),
            None => println!("ptp: jitter {} us (mean of the per-second sd); too short a run for the clock rate", Us(jitter)),
        }
    }
}

/// Sets the machine's clock and RTC from the master's time (`master_ptp_ns`, TAI) if the master has real time.
fn set_clock_from_master(slave: &Slave, options: &Options, master_ptp_ns: i64, refusal_shown: &mut bool) -> bool {
    let Some((_, clock)) = slave.master() else { return false };
    let unix = clock.unix_ns(master_ptp_ns);
    let local = (unix + 500_000_000).div_euclid(1_000_000_000) + i64::from(options.zone_minutes) * 60;
    let date = tape_net::ntp::civil_from_unix(local);
    if !clock.has_real_time() || date.year < 2024 {
        if !*refusal_shown {
            *refusal_shown = true;
            println!("ptp: settime: the master has no real time (time source {}); start it with ntp=. The clock is unchanged.", clock.time_source);
        }
        return false;
    }
    let wall = tape_rt::WallClock { year: date.year, month: date.month, day: date.day, hour: date.hour, minute: date.minute, second: date.second, reserved: 0 };
    if tape_rt::set_wall_clock(&wall) {
        let zone = options.zone_minutes;
        println!("ptp: clock and RTC set to {} (local time, UTC{}{}:{:02}) from the master.", Shown(date), if zone < 0 { '-' } else { '+' }, zone.abs() / 60, zone.abs() % 60);
    } else {
        println!("ptp: settime: the kernel refused the date (needs ABI 6, a date from 2024 to 2099).");
    }
    true
}

/// A master message on the transport in use, or on both UDP and Ethernet with `both`.
fn send_all(options: &Options, api: &TapeApi, config: &net::NetConfig, port: u16, payload: &[u8]) {
    send(l2(), api, options.nic, config, port, payload);
    if options.both {
        send(true, api, options.nic, config, port, payload);
    }
}

fn master(api: &TapeApi, options: &Options, config: &net::NetConfig, mac: [u8; 6], ntp_synced: bool) {
    let identity = port_identity(mac, 1);
    let mut clock = Clock::ordinary(mac);
    clock.priority1 = options.priority1;
    if ntp_synced {
        clock.time_source = ptp_tape::TIME_SOURCE_NTP;
        clock.utc_offset = Some(tape_net::ntp::TAI_MINUS_UTC as i16);
    }
    let mut igmp = igmp::Igmp::new(GROUP);
    let mut arp = ArpCache::<8>::new();
    let mut frame = [0u8; net::MAX_FRAME];
    let sync_interval = i64::from(options.sync_ms) * 1_000_000;
    let sync_log = match options.sync_ms {
        0..=44 => -5,
        45..=88 => -4,
        89..=177 => -3,
        178..=353 => -2,
        354..=707 => -1,
        708..=1414 => 0,
        _ => 1,
    };
    start_hardware_stamps(options);
    let start = now_ns();
    let (mut next_announce, mut next_sync, mut next_report) = (start, start, start + 5_000_000_000);
    let (mut announce_sequence, mut sync_sequence) = (0u16, 0u16);
    let (mut syncs, mut answered) = (0u32, 0u32);
    loop {
        let now = now_ns();
        if (api.should_abort)() != 0 || (options.seconds != 0 && now - start >= i64::from(options.seconds) * 1_000_000_000) {
            break;
        }
        let mut busy = false;
        for _ in 0..8 {
            let length = (api.nic_receive)(options.nic, frame.as_mut_ptr(), frame.len());
            if length == 0 {
                break;
            }
            let (t4, _) = receive_time(options.nic);
            busy = true;
            if !l2() && igmp.handle(&frame[..length], t4 as u64 / 1000) {
                continue;
            }
            // With `both` a Delay_Req can arrive over either transport; it is answered over the one it came by.
            let over_l2 = l2() || (options.both && ptp_tape::l2_message(&frame[..length]).is_some());
            if let Some((true, payload)) = incoming(over_l2, api, options.nic, config, &mut arp, &frame[..length]) {
                if let Some(request) = Header::parse(payload).filter(|h| h.message_type == DELAY_REQ && h.domain == options.domain && h.source != identity) {
                    let mut message = [0u8; 64];
                    let length = build_delay_resp(&mut message, options.domain, identity, request.source, request.sequence, t4, 0x7f);
                    send(over_l2, api, options.nic, config, GENERAL_PORT, &message[..length]);
                    answered += 1;
                }
            }
        }
        if !l2() {
            igmp.poll(api, options.nic, config, (now / 1000) as u64);
        }
        if now >= next_announce {
            next_announce += interval_ns(0);
            announce_sequence = announce_sequence.wrapping_add(1);
            let mut message = [0u8; 64];
            let length = build_announce(&mut message, options.domain, identity, announce_sequence, &clock, 0);
            send_all(options, api, config, GENERAL_PORT, &message[..length]);
        }
        if now >= next_sync {
            next_sync += sync_interval;
            if next_sync < now {
                next_sync = now + sync_interval;
            }
            sync_sequence = sync_sequence.wrapping_add(1);
            let mut message = [0u8; 64];
            let length = build_sync(&mut message, options.domain, identity, sync_sequence, None, sync_log);
            // Each transport's Follow_Up carries the time its own Sync left, so sending on both does not skew the second.
            let mut sent = [(false, 0i64); 2];
            let transports = if options.both { 2 } else { 1 };
            for (index, entry) in sent.iter_mut().enumerate().take(transports) {
                let over_l2 = if index == 0 { l2() } else { true };
                *entry = (over_l2, send(over_l2, api, options.nic, config, EVENT_PORT, &message[..length]));
            }
            for &(over_l2, t1) in &sent[..transports] {
                let length = build_follow_up(&mut message, options.domain, identity, sync_sequence, t1, sync_log);
                send(over_l2, api, options.nic, config, GENERAL_PORT, &message[..length]);
            }
            syncs += 1;
        }
        if now >= next_report {
            next_report += 5_000_000_000;
            println!("{:>4}s master: {} syncs sent, {} delay requests answered", (now - start) / 1_000_000_000, syncs, answered);
        }
        if !busy {
            wait(options);
        }
    }
    println!("ptp: {syncs} syncs sent, {answered} delay requests answered.");
}

tape_rt::tape_help!(br#"ptp: PTPv2 (IEEE 1588) clock sync, master or slave, UDP or Ethernet.

  tape run ptp [slave|master] [nic=N] [ip=dhcp|A.B.C.D/N] [gateway=A.B.C.D]
               [domain=N] [l2|both] [ntp=dhcp|A.B.C.D|NAME] [dns=A.B.C.D]
               [settime] [tz=+H[:MM]] [secs=N] [delay=SECONDS] [sync=MS] [prio1=N] [late=US]
               [spin] [verbose] [software]

  slave     measure offset, delay and rate to a master (the default)
  master    announce a clock; ntp= takes real time from an NTP server first
  ntp=NAME  a host name is looked up at the DNS server of DHCP, dns=, or the
            gateway
  nic=N     the interface (default: the one chosen with `nic use`)
  l2        PTP over Ethernet (0x88F7), no IP; ptp4l -2 compatible
  both      master: serve UDP and Ethernet at once (slaves pick one)
  settime   slave: set the clock and RTC from the master, then end (gives up
            after 60 s without secs=)
  tz=+2     hours east of UTC for settime (the RTC holds local time)
  software  do not use the NIC's hardware timestamps (I350)
  spin      poll flat out (slave default): smallest timestamp error, hot CPU
  idle      sleep between polls (master default); a slave loses accuracy
  secs=N    stop after N seconds; Escape stops at any time
"#);
