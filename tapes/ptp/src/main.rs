#![no_std]
#![no_main]
#![allow(dead_code)]

//! `ptp`: a PTPv2 (IEEE 1588-2008) clock over UDP/IPv4, the transport AES67 uses.
//!
//! ```text
//! tape run ptp [slave|master] [nic=N] [ip=dhcp|A.B.C.D/N] [gateway=A.B.C.D] [domain=N]
//!              [secs=N] [delay=SECONDS] [sync=MS] [prio1=N] [late=US] [idle] [verbose]
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
//! Timestamps are taken in software: receive time when the polling loop reads the frame, transmit time just
//! before the frame is handed to the NIC. By default the loop spins, to keep that error small; `idle` sleeps
//! between polls instead (up to about 100 us worse on a polled NIC, kinder to the CPU).
//!
//! A Sync that sat in the NIC's ring while the loop was busy (a print, a send) would carry a receive time that
//! is too late; when the last empty poll is more than `late=` us (default 300) before the frame, the Sync is
//! counted as `late` and not measured.
//!
//! Escape stops it. The protocol and the arithmetic live in `lib.rs`, with host tests (`cargo test --lib`).

#[path = "../../audio-net/src/igmp.rs"]
mod igmp;
#[path = "../../audio-net/src/net.rs"]
mod net;

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
}

fn parse(arguments: &'static str) -> Result<Options, &'static str> {
    let mut options = Options { master: false, nic: 0, ip: "dhcp", gateway: "", domain: 0, seconds: 0, delay_seconds: 1, sync_ms: 125, priority1: 128, late_us: 300, idle: false, verbose: false };
    for word in arguments.split_whitespace() {
        let (key, value) = word.split_once('=').unwrap_or((word, ""));
        match key {
            "slave" => options.master = false,
            "master" => options.master = true,
            "idle" => options.idle = true,
            "verbose" => options.verbose = true,
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
    Ok(options)
}

fn now_ns() -> i64 {
    tape_rt::micros() as i64 * 1000
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

fn send(api: &TapeApi, nic: usize, config: &net::NetConfig, port: u16, payload: &[u8]) {
    let mut frame = [0u8; 160];
    let destination = MacAddress(audio_wire::multicast_mac(GROUP));
    if let Some(length) = net::build_udp_frame(&mut frame, config, destination, Ipv4Address(GROUP), port, port, 1, payload) {
        net::send_frame(api, nic, &frame[..length]);
    }
}

/// Waits for a polled NIC: spins by default (the receive time is the clock reading), or sleeps.
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
    let config = if options.ip == "dhcp" {
        println!("ptp: asking DHCP for an address on NIC {}...", options.nic + 1);
        match net::dhcp(api, options.nic, MacAddress(mac)) {
            Some(config) => config,
            None => return println!("ptp: no DHCP answer; give ip=A.B.C.D/N instead."),
        }
    } else {
        match net::static_config(MacAddress(mac), options.ip, options.gateway) {
            Ok(config) => config,
            Err(reason) => return println!("ptp: {reason}."),
        }
    };
    println!("ptp {}: NIC {} {}.{}.{}.{}, domain {}, group 224.0.1.129. Escape stops.", if options.master { "master" } else { "slave" }, options.nic + 1, config.ip.0[0], config.ip.0[1], config.ip.0[2], config.ip.0[3], options.domain);
    let mut igmp = igmp::Igmp::new(GROUP);
    igmp.join(api, options.nic, &config);
    if options.master {
        master(api, &options, &config, mac);
    } else {
        slave(api, &options, &config, mac);
    }
    igmp.leave(api, options.nic, &config);
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
    println!("ptp: waiting for an Announce...");
    loop {
        let now = now_ns();
        if (api.should_abort)() != 0 || (options.seconds != 0 && now - start >= i64::from(options.seconds) * 1_000_000_000) {
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
            let t2 = now_ns();
            let late = t2 - last_empty > options.late_us * 1000;
            busy = true;
            if igmp.handle(&frame[..length], t2 as u64 / 1000) {
                continue;
            }
            if let net::Inbound::Udp { payload, .. } = net::inbound(api, options.nic, config, &mut arp, &frame[..length], EVENT_PORT..=GENERAL_PORT, 0) {
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
                    if options.verbose {
                        println!("  seq {} offset {} us (raw {} ms) delay {} us", sample.sequence, Us(sample.residual), sample.offset / 1_000_000, Us(sample.delay));
                    }
                }
            }
        }
        igmp.poll(api, options.nic, config, (now / 1000) as u64);
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
            slave.request_sent(sequence, now_ns());
            send(api, options.nic, config, EVENT_PORT, &message[..length]);
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

fn master(api: &TapeApi, options: &Options, config: &net::NetConfig, mac: [u8; 6]) {
    let identity = port_identity(mac, 1);
    let mut clock = Clock::ordinary(mac);
    clock.priority1 = options.priority1;
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
            let t4 = now_ns();
            busy = true;
            if igmp.handle(&frame[..length], t4 as u64 / 1000) {
                continue;
            }
            if let net::Inbound::Udp { destination_port: EVENT_PORT, payload, .. } = net::inbound(api, options.nic, config, &mut arp, &frame[..length], EVENT_PORT..=GENERAL_PORT, 0) {
                if let Some(request) = Header::parse(payload).filter(|h| h.message_type == DELAY_REQ && h.domain == options.domain && h.source != identity) {
                    let mut message = [0u8; 64];
                    let length = build_delay_resp(&mut message, options.domain, identity, request.source, request.sequence, t4, 0x7f);
                    send(api, options.nic, config, GENERAL_PORT, &message[..length]);
                    answered += 1;
                }
            }
        }
        igmp.poll(api, options.nic, config, (now / 1000) as u64);
        if now >= next_announce {
            next_announce += interval_ns(0);
            announce_sequence = announce_sequence.wrapping_add(1);
            let mut message = [0u8; 64];
            let length = build_announce(&mut message, options.domain, identity, announce_sequence, &clock, 0);
            send(api, options.nic, config, GENERAL_PORT, &message[..length]);
        }
        if now >= next_sync {
            next_sync += sync_interval;
            if next_sync < now {
                next_sync = now + sync_interval;
            }
            sync_sequence = sync_sequence.wrapping_add(1);
            let mut message = [0u8; 64];
            let length = build_sync(&mut message, options.domain, identity, sync_sequence, None, sync_log);
            let t1 = now_ns();
            send(api, options.nic, config, EVENT_PORT, &message[..length]);
            let length = build_follow_up(&mut message, options.domain, identity, sync_sequence, t1, sync_log);
            send(api, options.nic, config, GENERAL_PORT, &message[..length]);
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
