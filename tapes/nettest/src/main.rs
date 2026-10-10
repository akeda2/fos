//! `nettest`: a two-port layer-2 link impairment tape.
//!
//! It sits between two NICs like the `bridge` tape, but instead of learning
//! MACs it simply carries every frame from one port to the other and, on the
//! way, makes the link behave badly: random or bursty loss, delay with jitter,
//! a bandwidth limit with a finite queue. Each direction (`ab` = port A to
//! port B, `ba` = the reverse) is configured separately.
//!
//! ```text
//! tape run nettest [A B] [loss=PCT[:BURST]] [delay=MS[:JITTER]] [rate=KBIT]
//!                        [queue=N] [seed=N]      (prefix a name with ab. or ba.)
//! tape run nettest @nettest.cfg                  (one command per line)
//! ```
//!
//! With no port numbers it uses NICs 1 and 2. While it runs, a `nettest>`
//! prompt on the keyboard (or the `term net` session) accepts the same
//! settings as commands: `loss [ab|ba] PCT [burst N]`, `delay [ab|ba] MS
//! [JITTER]`, `rate [ab|ba] KBIT`, `queue [ab|ba] N`, `seed N`, `show`,
//! `stats`, `reset [stats]`, `help`, `quit`. Escape also stops it. Time has the
//! kernel's 10 ms tick granularity. The engine itself lives in `lib.rs`.
#![no_std]
#![no_main]

extern crate nettest_tape;

use core::ptr::addr_of_mut;
use nettest_tape::{
    ArpInfo, Config, Direction, GEN_HEADER_LEN, Generator, HISTOGRAM_BOUNDS_US, MAX_FRAME, PRESETS,
    Rng, SLOT_BYTES, Sink, Sweep, UdpPlan, build_arp, parse_arp, parse_ip, parse_mac, parse_permille, parse_u32, preset,
};
use tape_rt::{TapeApi, micros, print_bytes, print_u32};

fn wait_for_event() {
    tape_rt::idle();
}

const TAPE_EVENT_ABORT: u32 = 1;
const TAPE_EVENT_BYTE_BASE: u32 = 0x100;
const ETHERNET_HEADER_LEN: usize = 14;
const LINE_CAPACITY: usize = 120;
const MAX_WORDS: usize = 24;

/// Queue storage used when the kernel's big pool cannot supply more (a QEMU
/// guest with 128 MiB of RAM or less has none): 256 slots per direction.
const FALLBACK_SLOTS: usize = 256;
static mut FALLBACK_STORAGE: [u8; 2 * FALLBACK_SLOTS * SLOT_BYTES] =
    [0; 2 * FALLBACK_SLOTS * SLOT_BYTES];
/// Request for the big pool: 4 MiB, i.e. about 1365 slots per direction.
const BIG_BYTES: u32 = 4 * 1024 * 1024;

type Putc = extern "C" fn(u32);

const DIRECTION_NAMES: [&[u8]; 2] = [b"a>b", b"b>a"];

struct Session<'a> {
    links: [Direction<'a>; 2],
    rng: Rng,
    /// The statistics panel (`view on`); see `View`.
    view: View,
    /// False while the startup arguments are being applied.
    started: bool,
    /// The traffic generator and sink (see the module doc), each tied to a NIC
    /// of its own. Zero-based NIC indexes; `None` means the role is off.
    generator: Generator,
    sink: Sink,
    gen_nic: Option<u32>,
    sink_nic: Option<u32>,
    /// `gen.to=MAC` was given; otherwise frames go to the local sink's MAC, or
    /// are broadcast when there is no local sink.
    destination_explicit: bool,
    /// `gen.run=on`: start sending as soon as the tape is up (after
    /// `gen.delay` seconds: links often need a moment to come up).
    gen_autostart: bool,
    gen_delay_s: u32,
    /// Routable mode pieces (`gen.ip`, `gen.dst`, `gen.gw`, `gen.port`); the
    /// generator's `UdpPlan` is rebuilt from them whenever one changes.
    udp_source: Option<[u8; 4]>,
    udp_destination: Option<[u8; 4]>,
    udp_gateway: Option<[u8; 4]>,
    udp_port: u16,
    /// When the generator last asked for its next hop's MAC.
    last_arp_us: u64,
    /// A rate sweep (`gen sweep`), and whether one was configured to begin at
    /// launch (`gen.sweep=...`).
    sweep: Sweep,
    sweep_at_launch: bool,
    /// Bidirectional mode (`bidir=on`): the sink's NIC also generates a stream
    /// back (`rgen`) and the generator's NIC also receives one (`rsink`), so both
    /// directions of a link or router are loaded at once with the same NICs.
    bidir: bool,
    rgen: Generator,
    rsink: Sink,
    rev_gateway: Option<[u8; 4]>,
    last_rev_arp_us: u64,
    /// FOSFS write callback (for `save`), the file `gen.save=NAME` writes when a
    /// test finishes, and when the end-of-test summary is due (0 = not pending).
    fs_write: Option<fn(usize, u32, usize, u32) -> u32>,
    save_name: [u8; 16],
    save_name_len: usize,
    summary_due_us: u64,
    /// MACs of the generator's and the sink's NICs (the reverse stream addresses
    /// the other end of the pair by them).
    gen_mac: [u8; 6],
    sink_mac: [u8; 6],
    /// The two-port impairment link is active (not when only gen/sink given).
    limiter: bool,
    /// The impaired link's two NICs and their MACs, plus what is needed to
    /// re-point any role while the tape runs (`link`, `gen nic`, `sink nic`).
    ports: [u32; 2],
    macs: [[u8; 6]; 2],
    nic_total: usize,
    mac_at: fn(u32, usize) -> u32,
    promisc_at: fn(u32, u32) -> u32,
    /// Passes of the polling loop and when it started: a long average pass is
    /// the first sign that the tape itself, not the link, is the bottleneck.
    passes: u64,
    loop_started_us: u64,
}

enum Outcome {
    Continue,
    Quit,
    /// A command that was not understood or had a bad value (already
    /// reported at the prompt; startup turns it into a usage message).
    Error,
}

extern "C" fn silent(_byte: u32) {}

/// The v1 tape ABI passed ~40 callbacks as parameters; v2 has one table. These small
/// functions keep the names the rest of this file was written against, now taking
/// 64-bit pointers (a buffer may live anywhere in RAM, not just below 4 GiB).
fn nic_count() -> u32 {
    (tape_rt::api().nic_count)() as u32
}
fn nic_mac_at(nic: u32, out: usize) -> u32 {
    (tape_rt::api().nic_mac)(nic as usize, out as *mut u8)
}
fn nic_send_at(nic: u32, frame: usize, length: u32) -> u32 {
    (tape_rt::api().nic_send)(nic as usize, frame as *const u8, length as usize)
}
fn nic_receive_at(nic: u32, buffer: usize, capacity: u32) -> u32 {
    (tape_rt::api().nic_receive)(nic as usize, buffer as *mut u8, capacity as usize) as u32
}
fn nic_set_promiscuous_at(nic: u32, on: u32) -> u32 {
    (tape_rt::api().nic_set_promiscuous)(nic as usize, on)
}
fn nic_max_frame_len() -> u32 {
    (tape_rt::api().nic_max_frame_len)() as u32
}
fn ticks() -> u64 {
    (tape_rt::api().ticks)()
}
fn read_key() -> u32 {
    (tape_rt::api().read_key)()
}
fn alloc(bytes: u32) -> usize {
    (tape_rt::api().alloc)(bytes as usize)
}
/// Writes a file on the boot volume; 0 on success.
fn fs_write(name: usize, name_length: u32, data: usize, length: u32) -> u32 {
    u32::from((tape_rt::api().fs_write)(name as *const u8, name_length as usize, data as *const u8, length as usize) != 0)
}

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(api: &'static TapeApi) {
    tape_rt::init(api);
    let putc = api.putc;
    let nic_total = nic_count() as usize;
    if nic_total < 1 {
        print_bytes(putc, b"nettest needs at least one initialized NIC.\n");
        return;
    }

    // Queue storage: the big pool if it has room, otherwise the static fallback.
    let big = alloc(BIG_BYTES);
    let storage: &mut [u8] = unsafe {
        if big != 0 {
            core::slice::from_raw_parts_mut(big as *mut u8, BIG_BYTES as usize)
        } else {
            &mut *addr_of_mut!(FALLBACK_STORAGE)
        }
    };
    let half = (storage.len() / 2 / SLOT_BYTES) * SLOT_BYTES;
    let (first, second) = storage.split_at_mut(half);
    let second = &mut second[..half];
    let mut session = Session {
        links: [Direction::new(first), Direction::new(second)],
        rng: Rng::new(ticks() as u32 ^ 0x5bd1_e995),
        view: View::new(),
        started: false,
        generator: Generator::new(),
        sink: Sink::new(),
        gen_nic: None,
        sink_nic: None,
        destination_explicit: false,
        gen_autostart: false,
        gen_delay_s: 0,
        udp_source: None,
        udp_destination: None,
        udp_gateway: None,
        udp_port: 9000,
        last_arp_us: 0,
        sweep: Sweep::new(),
        sweep_at_launch: false,
        bidir: false,
        rgen: Generator::new(),
        rsink: Sink::new(),
        rev_gateway: None,
        last_rev_arp_us: 0,
        gen_mac: [0; 6],
        sink_mac: [0; 6],
        fs_write: None,
        save_name: [0; 16],
        save_name_len: 0,
        summary_due_us: 0,
        limiter: true,
        ports: [0, 1],
        macs: [[0; 6]; 2],
        nic_total,
        mac_at: nic_mac_at,
        promisc_at: nic_set_promiscuous_at,
        passes: 0,
        loop_started_us: 0,
    };

    let arguments = tape_rt::args();
    let mut ports = [0u32; 2];
    let mut port_count = 0;
    if !apply_startup(putc, &mut session, arguments, &mut ports, &mut port_count) {
        print_bytes(
            putc,
            b"Usage: tape run nettest [A B] [loss=PCT[:BURST]] [delay=MS[:JITTER]]\n\
              \x20      [rate=KBIT] [queue=N] [seed=N] [gen=N] [sink=N] [gen.rate=KBIT]\n\
              \x20      [gen.size=B] [gen.to=MAC|sink] [gen.count=N] [gen.time=S]\n\
              \x20      [gen.run=on]   or   tape run nettest @FILE\n\
              Ready-made: nettest selftest GEN LINK_IN LINK_OUT SINK [settings]\n\
              \x20           nettest routertest GEN SINK [settings]\n",
        );
        release(big);
        return;
    }
    // Roles: the impairment link between two ports, a generator NIC, a sink NIC.
    // With only `gen=`/`sink=` and no port numbers there is no impairment link.
    let roles = session.gen_nic.is_some() || session.sink_nic.is_some();
    session.limiter = port_count != 0 || !roles;
    if session.limiter {
        if port_count == 0 {
            ports = [0, 1];
        } else if port_count != 2 || ports[0] == ports[1] {
            print_bytes(putc, b"nettest needs two distinct NIC numbers (see 'nic list').\n");
            release(big);
            return;
        }
    }
    let mut used = [u32::MAX; 4];
    let mut used_count = 0;
    if session.limiter {
        used[0] = ports[0];
        used[1] = ports[1];
        used_count = 2;
    }
    for role in [session.gen_nic, session.sink_nic].into_iter().flatten() {
        used[used_count] = role;
        used_count += 1;
    }
    for (position, &nic) in used[..used_count].iter().enumerate() {
        if nic as usize >= nic_total {
            print_bytes(putc, b"nettest: no such NIC (see 'nic list').\n");
            release(big);
            return;
        }
        if used[..position].contains(&nic) {
            print_bytes(putc, b"nettest: each role needs a NIC of its own ('nic map' shows the cabling).\n");
            release(big);
            return;
        }
    }
    let max_frame_len = (nic_max_frame_len() as usize).clamp(ETHERNET_HEADER_LEN, MAX_FRAME);

    let mut macs = [[0u8; 6]; 2];
    let mut gen_mac = [0u8; 6];
    let mut sink_mac = [0u8; 6];
    let mac_ok = (!session.limiter
        || (nic_mac_at(ports[0], macs[0].as_mut_ptr() as usize) != 0
            && nic_mac_at(ports[1], macs[1].as_mut_ptr() as usize) != 0))
        && session.gen_nic.is_none_or(|nic| nic_mac_at(nic, gen_mac.as_mut_ptr() as usize) != 0)
        && session.sink_nic.is_none_or(|nic| nic_mac_at(nic, sink_mac.as_mut_ptr() as usize) != 0);
    if !mac_ok {
        print_bytes(putc, b"nettest cannot start: failed to read a NIC MAC.\n");
        release(big);
        return;
    }
    if session.limiter {
        for index in 0..2 {
            if nic_set_promiscuous_at(ports[index], 1) == 0 {
                if index == 1 {
                    let _ = nic_set_promiscuous_at(ports[0], 0);
                }
                print_bytes(putc, b"nettest cannot start: promiscuous mode failed.\n");
                release(big);
                return;
            }
        }
    }
    session.gen_mac = gen_mac;
    session.sink_mac = sink_mac;
    session.ports = ports;
    session.macs = macs;
    session.fs_write = Some(fs_write);
    if !session.destination_explicit {
        session.generator.destination = if session.sink_nic.is_some() { sink_mac } else { [0xff; 6] };
    }

    print_bytes(putc, b"fos nettest: ");
    if session.limiter {
        print_bytes(putc, b"NIC ");
        print_u32(putc, ports[0] + 1);
        print_bytes(putc, b" <-> NIC ");
        print_u32(putc, ports[1] + 1);
        print_bytes(putc, b" (promiscuous), ");
        print_u32(putc, session.links[0].queue.capacity() as u32);
        print_bytes(putc, b" frames of queue per way.\n");
    } else {
        print_bytes(putc, b"no impairment link (generator/sink only).\n");
    }
    if let Some(nic) = session.gen_nic {
        print_bytes(putc, b"Generator on NIC ");
        print_u32(putc, nic + 1);
        print_bytes(putc, b"; start it with: gen start\n");
    }
    if let Some(nic) = session.sink_nic {
        print_bytes(putc, b"Sink on NIC ");
        print_u32(putc, nic + 1);
        print_bytes(putc, b".\n");
    }
    print_bytes(putc, b"Type help for commands; Escape stops.\n");
    show(putc, &session);
    session.view.ports = [ports[0] + 1, ports[1] + 1];

    let mut frame = [0u8; MAX_FRAME];
    let mut line = [0u8; LINE_CAPACITY];
    let mut line_length = 0usize;
    session.started = true;
    session.view.start_tick = micros() / 10_000;
    if session.gen_autostart && session.gen_nic.is_some() {
        let begin = micros() + u64::from(session.gen_delay_s) * 1_000_000;
        let sweep = session.sweep_at_launch;
        start_generators(&mut session, begin, sweep);
    }
    if session.view.wanted {
        view_start(putc, &mut session);
    }
    print_bytes(putc, b"nettest> ");
    let mut last_key_us = 0u64;
    session.loop_started_us = micros();
    let mut generator_was_running = session.generator.running;
    'run: loop {
        let now_us = micros();
        let mut busy = false;
        session.passes += 1;
        if session.view.on {
            view_update(putc, &mut session, now_us / 10_000);
        }

        // The sink first, so a frame is timestamped as soon as it is seen.
        if let Some(nic) = session.sink_nic {
            for _ in 0..64 {
                let received = nic_receive_at(nic, frame.as_mut_ptr() as usize, max_frame_len as u32) as usize;
                if received < ETHERNET_HEADER_LEN {
                    break;
                }
                busy = true;
                if let Some(info) = parse_arp(&frame[..received]) {
                    // Routable mode: answer ARP for the sink's own address, so a
                    // router (or the generator) can find it; and, for the reverse
                    // stream, learn the MAC of the reverse generator's next hop.
                    if !info.is_reply && Some(info.target_ip) == session.sink.udp_ip {
                        let mut reply = [0u8; 64];
                        build_arp(&mut reply, true, session.sink_mac, info.target_ip, info.sender_mac, info.sender_ip);
                        let _ = nic_send_at(nic, reply.as_ptr() as usize, 60);
                    } else if info.is_reply
                        && session.rgen.udp.is_some_and(|plan| plan.next_hop == info.sender_ip)
                    {
                        session.rgen.resolve(info.sender_mac, now_us);
                    }
                    continue;
                }
                let _ = session.sink.on_frame(&frame[..received], micros());
            }
        }
        // The generator's NIC hears the wire too (ARP, discovery...): drain it
        // so its receive ring never fills while the foreground is busy sending.
        if let Some(nic) = session.gen_nic {
            for _ in 0..8 {
                let received = nic_receive_at(nic, frame.as_mut_ptr() as usize, max_frame_len as u32) as usize;
                if received < ETHERNET_HEADER_LEN {
                    break;
                }
                // The reverse stream (bidirectional mode) ends here.
                if session.bidir && parse_arp(&frame[..received]).is_none() {
                    let _ = session.rsink.on_frame(&frame[..received], micros());
                    continue;
                }
                // Routable mode: learn the next hop's MAC from its ARP reply, and
                // answer ARP for the generator's own address.
                if let (Some(info), Some(plan)) = (parse_arp(&frame[..received]), session.generator.udp) {
                    let ArpInfo { is_reply, sender_mac, sender_ip, target_ip } = info;
                    if is_reply && sender_ip == plan.next_hop {
                        session.generator.resolve(sender_mac, now_us);
                    } else if !is_reply && target_ip == plan.source_ip {
                        let mut reply = [0u8; 64];
                        build_arp(&mut reply, true, session.gen_mac, plan.source_ip, sender_mac, sender_ip);
                        let _ = nic_send_at(nic, reply.as_ptr() as usize, 60);
                    }
                }
            }
            // The reverse generator sends from the sink's NIC.
            if session.bidir {
                if let Some(sink_nic) = session.sink_nic {
                    let mut sent = 0;
                    while let Some(length) = session.rgen.next_frame(now_us, session.sink_mac, &mut frame, sent) {
                        let ok = nic_send_at(sink_nic, frame.as_ptr() as usize, length as u32) != 0;
                        session.rgen.record_send(length, ok);
                        sent += 1;
                        busy = true;
                    }
                    if let Some(plan) = session.rgen.udp {
                        if !session.rgen.resolved && now_us.wrapping_sub(session.last_rev_arp_us) >= 500_000 {
                            session.last_rev_arp_us = now_us;
                            let mut request = [0u8; 64];
                            build_arp(&mut request, false, session.sink_mac, plan.source_ip, [0; 6], plan.next_hop);
                            let _ = nic_send_at(sink_nic, request.as_ptr() as usize, 60);
                        }
                    }
                }
            }
            // Still looking for the next hop: ask twice a second.
            if let Some(plan) = session.generator.udp {
                if !session.generator.resolved && now_us.wrapping_sub(session.last_arp_us) >= 500_000 {
                    session.last_arp_us = now_us;
                    let mut request = [0u8; 64];
                    build_arp(&mut request, false, session.gen_mac, plan.source_ip, [0; 6], plan.next_hop);
                    let _ = nic_send_at(nic, request.as_ptr() as usize, 60);
                }
            }
        }

        // Receive from each impaired port and push through that direction's faults.
        if session.limiter {
            for ingress in 0..2 {
                for _ in 0..32 {
                    let received = nic_receive_at(
                        session.ports[ingress],
                        frame.as_mut_ptr() as usize,
                        max_frame_len as u32,
                    ) as usize;
                    if received < ETHERNET_HEADER_LEN {
                        break;
                    }
                    busy = true;
                    // Frames to or from this tape's own NICs are management traffic
                    // (a `term net` session), not part of the link under test.
                    let destination = &frame[0..6];
                    let source = &frame[6..12];
                    if session.macs.iter().any(|mac| mac == destination || mac == source) {
                        continue;
                    }
                    let Session { links, rng, .. } = &mut session;
                    links[ingress].ingest(&frame[..received], now_us, rng);
                }
            }

            // Send whatever has come due.
            for way in 0..2 {
                while let Some(length) = session.links[way].release(now_us, &mut frame) {
                    busy = true;
                    let ok = nic_send_at(session.ports[1 - way], frame.as_ptr() as usize, length as u32) != 0;
                    session.links[way].record_send(length, ok);
                }
            }
        }

        // The generator: everything that has come due, up to a burst limit.
        if let Some(nic) = session.gen_nic {
            let mut sent = 0;
            while let Some(length) = session.generator.next_frame(now_us, session.gen_mac, &mut frame, sent) {
                let ok = nic_send_at(nic, frame.as_ptr() as usize, length as u32) != 0;
                session.generator.record_send(length, ok);
                sent += 1;
                busy = true;
            }
            let Session { sweep, generator, sink, .. } = &mut session;
            if let Some(result) = sweep.poll(now_us, generator, sink) {
                print_sweep_step(putc, &result);
            }
            if generator_was_running && !session.generator.running {
                print_bytes(putc, b"\ngenerator finished: ");
                print_gen_line(putc, &session);
                // Give the last frames a moment to arrive, then summarize.
                session.summary_due_us = now_us + 1_000_000;
                print_bytes(putc, b"nettest> ");
                print_bytes(putc, &line[..line_length]);
            }
            generator_was_running = session.generator.running;
        }

        if session.summary_due_us != 0 && now_us >= session.summary_due_us {
            session.summary_due_us = 0;
            print_bytes(putc, b"\n");
            print_summary(putc, &session);
            if session.save_name_len != 0 {
                let name = session.save_name;
                let length = session.save_name_len;
                save_report(putc, &session, &name[..length]);
            }
            print_bytes(putc, b"nettest> ");
            print_bytes(putc, &line[..line_length]);
        }

        // Keys: once per pass when idle, but at most once a millisecond while
        // traffic is flowing, so typing never starves forwarding or pacing.
        if !busy || now_us.wrapping_sub(last_key_us) >= 1000 {
            last_key_us = now_us;
            let event = read_key();
            if event == TAPE_EVENT_ABORT {
                putc(u32::from(b'\n'));
                break 'run;
            }
            if event >= TAPE_EVENT_BYTE_BASE {
                busy = true;
                match (event - TAPE_EVENT_BYTE_BASE) as u8 {
                    b'\r' | b'\n' => {
                        putc(u32::from(b'\n'));
                        let outcome = run_line(putc, &mut session, &line[..line_length]);
                        line_length = 0;
                        if matches!(outcome, Outcome::Quit) {
                            break 'run;
                        }
                        generator_was_running = session.generator.running;
                        print_bytes(putc, b"nettest> ");
                    }
                    // The kernel's read_key already erased the character on screen.
                    8 | 127 => line_length = line_length.saturating_sub(1),
                    byte @ 32..=126 if line_length < line.len() => {
                        line[line_length] = byte;
                        line_length += 1;
                        putc(u32::from(byte));
                    }
                    _ => {}
                }
            }
        }
        // Sleep only when nothing is pending: a running generator and queued
        // frames both have deadlines finer than the 10 ms the sleep can miss.
        let queued = session.limiter && session.links.iter().any(|link| link.queue.len() > 0);
        if !busy && !session.generator.running && !queued {
            wait_for_event();
        }
    }

    if session.limiter {
        for port in session.ports {
            let _ = nic_set_promiscuous_at(port, 0);
        }
    }
    if session.view.on {
        view_stop(putc, &mut session);
    }
    print_bytes(putc, b"nettest stopped.\n");
    print_stats(putc, &session);
    release(big);
}

fn release(big: usize) {
    if big != 0 {
        (tape_rt::api().free)(big, BIG_BYTES as usize);
    }
}

fn is_space(byte: u8) -> bool {
    byte == b' ' || byte == b'\t' || byte == b'\r'
}

/// Splits `line` into words. Returns how many fit.
fn split_words<'a>(line: &'a [u8], words: &mut [&'a [u8]; MAX_WORDS]) -> usize {
    let mut count = 0;
    let mut start = None;
    for (index, &byte) in line.iter().enumerate() {
        if is_space(byte) {
            if let Some(begin) = start.take() {
                if count < MAX_WORDS {
                    words[count] = &line[begin..index];
                    count += 1;
                }
            }
        } else if start.is_none() {
            start = Some(index);
        }
    }
    if let Some(begin) = start {
        if count < MAX_WORDS {
            words[count] = &line[begin..];
            count += 1;
        }
    }
    count
}

/// Handles the startup arguments (`tape run nettest ...`, or the contents of an
/// `@file`): segments separated by newlines or `;`, `#` comments, optional
/// leading port numbers, then `name=value` settings or ordinary commands.
/// The ready-made test set-ups.
///
/// `selftest G A B S`: generator on NIC G, the impairment link between NICs A
/// and B, sink on NIC S -- for a machine whose ports are cabled G-A and B-S (see
/// `nic map`), a lab on one machine. `routertest G S`: generator on NIC G and
/// sink on NIC S with UDP/IP frames in both directions, for testing a router
/// (or any gateway) cabled between the two ports; it assumes the router is
/// 10.0.1.1 on G's side and 10.0.2.1 on S's side, the generator and sink are
/// 10.0.1.2 and 10.0.2.2. Both default to 100 Mbit/s of 1000-byte frames for
/// 10 seconds, starting after 3 seconds, and print a summary at the end; every
/// default can be overridden by words that follow (`gen.rate=`, `loss=`, ...).
fn scenario(
    session: &mut Session<'_>,
    routed: bool,
    nics: [u32; 4],
    ports: &mut [u32; 2],
    port_count: &mut usize,
) {
    if routed {
        session.gen_nic = Some(nics[0]);
        session.sink_nic = Some(nics[1]);
    } else {
        *ports = [nics[1], nics[2]];
        *port_count = 2;
        session.gen_nic = Some(nics[0]);
        session.sink_nic = Some(nics[3]);
    }
    for (setting, value) in [
        (&b"rate"[..], &b"100000"[..]),
        (b"size", b"1000"),
        (b"delay", b"3"),
        (b"time", b"10"),
        (b"run", b"on"),
    ] {
        gen_setting(session, setting, value);
    }
    if routed {
        for (setting, value) in [
            (&b"ip"[..], &b"10.0.1.2"[..]),
            (b"dst", b"10.0.2.2"),
            (b"gw", b"10.0.1.1"),
        ] {
            gen_setting(session, setting, value);
        }
        sink_setting(session, b"ip", b"10.0.2.2");
        session.bidir = true;
        session.rev_gateway = Some([10, 0, 2, 1]);
        rebuild_udp(session);
    }
}

/// More words than `split_words` can hold would be silently cut off.
fn too_many_words(line: &[u8]) -> bool {
    let mut words = 0;
    let mut in_word = false;
    for &byte in line {
        if is_space(byte) {
            in_word = false;
        } else if !in_word {
            in_word = true;
            words += 1;
        }
    }
    words > MAX_WORDS
}

fn apply_startup(
    putc: Putc,
    session: &mut Session<'_>,
    arguments: &[u8],
    ports: &mut [u32; 2],
    port_count: &mut usize,
) -> bool {
    for segment in arguments.split(|&byte| byte == b'\n' || byte == b';') {
        let segment = match segment.iter().position(|&byte| byte == b'#') {
            Some(hash) => &segment[..hash],
            None => segment,
        };
        if too_many_words(segment) {
            print_bytes(putc, b"nettest: too many words on one line; split it with ';'\n");
            return false;
        }
        let mut words = [&[][..]; MAX_WORDS];
        let count = split_words(segment, &mut words);
        let mut index = 0;
        if index < count && words[index] == b"nettest" {
            index += 1;
        }
        while index < count {
            let word = words[index];
            if word == b"selftest" || word == b"routertest" {
                // Ready-made set-ups; see `scenario`. They take their NIC numbers
                // right after the keyword, and later words may override anything.
                let routed = word == b"routertest";
                let needed = if routed { 2 } else { 4 };
                let mut nics = [0u32; 4];
                for slot in nics.iter_mut().take(needed) {
                    index += 1;
                    match words[..count].get(index).and_then(|word| parse_u32(word)) {
                        Some(number) if number >= 1 => *slot = number - 1,
                        _ => {
                            print_bytes(putc, b"nettest: selftest needs 4 NIC numbers (generator, link in, link out, sink); routertest needs 2 (generator, sink)\n");
                            return false;
                        }
                    }
                }
                index += 1;
                scenario(session, routed, nics, ports, port_count);
                continue;
            }
            if let Some(number) = parse_u32(word) {
                if *port_count == 2 || number == 0 {
                    return false;
                }
                ports[*port_count] = number - 1;
                *port_count += 1;
                index += 1;
            } else if let Some(equals) = word.iter().position(|&byte| byte == b'=') {
                if !apply_setting(session, &word[..equals], &word[equals + 1..]) {
                    return false;
                }
                index += 1;
            } else {
                // An ordinary command takes the rest of the segment; its normal
                // echo (`show` after a change) is suppressed while starting up.
                let start = word.as_ptr() as usize - segment.as_ptr() as usize;
                if !matches!(run_line(silent, session, &segment[start..]), Outcome::Continue) {
                    print_bytes(putc, b"nettest: cannot use '");
                    print_bytes(putc, &segment[start..]);
                    print_bytes(putc, b"'\n");
                    return false;
                }
                break;
            }
        }
    }
    true
}

/// `name[.ab|.ba]=value[:value]`.
fn apply_setting(session: &mut Session<'_>, key: &[u8], value: &[u8]) -> bool {
    // Traffic roles: `gen=N` / `sink=N` pick the NICs (one-based), `gen.KEY=V`
    // configure the generator. Handled before the value is split at ':', because
    // a MAC address (`gen.to=aa:bb:...`) contains colons.
    if key == b"bidir" {
        session.bidir = value == b"on";
        rebuild_udp(session);
        return value == b"on" || value == b"off";
    }
    if key == b"rev.gw" {
        return match parse_ip(value) {
            Some(ip) => {
                session.rev_gateway = Some(ip);
                rebuild_udp(session);
                true
            }
            None => false,
        };
    }
    if key == b"gen" || key == b"sink" {
        let Some(number) = parse_u32(value).filter(|&number| number >= 1) else { return false };
        if key == b"gen" {
            session.gen_nic = Some(number - 1);
        } else {
            session.sink_nic = Some(number - 1);
        }
        return true;
    }
    if let Some(setting) = key.strip_prefix(b"gen.") {
        return gen_setting(session, setting, value);
    }
    if let Some(setting) = key.strip_prefix(b"sink.") {
        return sink_setting(session, setting, value);
    }
    if key == b"view" {
        // Before the tape's loop starts this only records the wish; the panel
        // is drawn once the ports are known.
        session.view.wanted = value == b"on";
        return value == b"on" || value == b"off";
    }
    let (name, way) = match key.iter().position(|&byte| byte == b'.') {
        Some(dot) => (&key[..dot], parse_way(&key[dot + 1..])),
        None => (key, Some(Ways::Both)),
    };
    let Some(way) = way else { return false };
    let mut parts = [&[][..]; 2];
    let mut count = 0;
    for part in value.split(|&byte| byte == b':') {
        if count == 2 {
            return false;
        }
        parts[count] = part;
        count += 1;
    }
    configure(session, name, way, &parts[..count]).is_ok()
}

/// Starts the forward generator (or sweep) and, in bidirectional mode, the
/// reverse generator with the same rate, size and limits.
fn start_generators(session: &mut Session<'_>, at_us: u64, sweep: bool) {
    if sweep {
        let Session { sweep, generator, sink, .. } = session;
        sweep.begin(at_us, generator, sink);
    } else {
        session.generator.start(at_us);
    }
    if session.bidir {
        let forward = &session.generator;
        let (rate, size, count, seconds) =
            (forward.rate_kbit, forward.size, forward.count_limit, forward.seconds_limit);
        let raw = session.rgen.udp.is_none();
        session.rgen.rate_kbit = rate;
        session.rgen.size = size;
        session.rgen.count_limit = count;
        session.rgen.seconds_limit = seconds;
        if raw {
            session.rgen.destination = session.gen_mac;
        }
        session.rgen.start(at_us);
    }
}

/// Validates and stores a sweep's `[start, end, step, seconds]`.
fn set_sweep(session: &mut Session<'_>, numbers: [u32; 4]) -> bool {
    let [start, end, step, seconds] = numbers;
    if start < 1 || end < start || step < 1 || seconds < 1 {
        return false;
    }
    let sweep = &mut session.sweep;
    (sweep.start, sweep.end, sweep.step, sweep.seconds) = (start, end, step, seconds);
    true
}

fn print_sweep_step(putc: Putc, result: &nettest_tape::StepResult) {
    print_bytes(putc, b"sweep ");
    print_u32(putc, result.offered_kbit);
    print_bytes(putc, b" kbit: sent ");
    print_u32(putc, result.sent);
    print_bytes(putc, b" rx ");
    print_u32(putc, result.received);
    print_bytes(putc, b" loss ");
    print_hundredths(putc, result.loss_hundredths);
    print_bytes(putc, b"% got ");
    print_u32(putc, result.got_kbit);
    print_bytes(putc, b" kbit lat ");
    print_u32(putc, result.latency_avg);
    putc(u32::from(b'/'));
    print_u32(putc, result.latency_max);
    print_bytes(putc, b" us\n");
}

/// `sink.ip=A.B.C.D` (answer ARP for it and accept UDP to it) and `sink.port=N`.
fn sink_setting(session: &mut Session<'_>, setting: &[u8], value: &[u8]) -> bool {
    match setting {
        b"ip" => parse_ip(value)
            .map(|ip| {
                session.sink.udp_ip = Some(ip);
                rebuild_udp(session);
            })
            .is_some(),
        b"port" => parse_u32(value)
            .filter(|&port| (1..=65535).contains(&port))
            .map(|port| {
                session.sink.udp_port = port as u16;
                session.udp_port = port as u16;
                rebuild_udp(session);
            })
            .is_some(),
        _ => false,
    }
}

/// Recomputes the generator's UDP plan once source and destination are known.
fn rebuild_udp(session: &mut Session<'_>) {
    if let (Some(source), Some(destination)) = (session.udp_source, session.udp_destination) {
        session.generator.set_udp(UdpPlan {
            source_ip: source,
            destination_ip: destination,
            next_hop: session.udp_gateway.unwrap_or(destination),
            source_port: 9001,
            destination_port: session.udp_port,
        });
        // The reverse stream swaps the ends: from the sink's address back to the
        // generator's, using the forward stream's source port as its destination.
        session.rsink.udp_ip = Some(source);
        session.rsink.udp_port = 9001;
        if let (true, Some(sink_ip)) = (session.bidir, session.sink.udp_ip) {
            session.rgen.set_udp(UdpPlan {
                source_ip: sink_ip,
                destination_ip: source,
                next_hop: session.rev_gateway.unwrap_or(source),
                source_port: session.udp_port,
                destination_port: 9001,
            });
        }
    }
}

/// One generator setting: `rate` (kbit/s), `size` (bytes), `to` (MAC or
/// `sink`), `count`, `time` (seconds) and `run` (`on` starts sending at launch).
fn gen_setting(session: &mut Session<'_>, setting: &[u8], value: &[u8]) -> bool {
    let generator = &mut session.generator;
    match setting {
        b"rate" => parse_u32(value).filter(|&rate| rate >= 1).map(|rate| generator.rate_kbit = rate).is_some(),
        b"size" => parse_u32(value)
            .filter(|&size| (60..=MAX_FRAME as u32).contains(&size) && size as usize >= GEN_HEADER_LEN)
            .map(|size| generator.size = size as usize)
            .is_some(),
        b"count" => parse_u32(value).map(|count| generator.count_limit = count).is_some(),
        b"time" => parse_u32(value).map(|seconds| generator.seconds_limit = seconds).is_some(),
        b"delay" => parse_u32(value).map(|seconds| session.gen_delay_s = seconds).is_some(),
        b"save" => {
            if !valid_file_name(value) {
                return false;
            }
            session.save_name[..value.len()].copy_from_slice(value);
            session.save_name_len = value.len();
            true
        }
        b"sweep" => {
            // START:END:STEP:SECONDS, all numbers (kbit/s, kbit/s, kbit/s, s).
            let mut numbers = [0u32; 4];
            let mut count = 0;
            for part in value.split(|&byte| byte == b':') {
                match (count < 4, parse_u32(part)) {
                    (true, Some(number)) => numbers[count] = number,
                    _ => return false,
                }
                count += 1;
            }
            if count != 4 || !set_sweep(session, numbers) {
                return false;
            }
            session.sweep_at_launch = true;
            session.gen_autostart = true;
            true
        }
        b"ip" | b"dst" | b"gw" => match parse_ip(value) {
            Some(ip) => {
                match setting {
                    b"ip" => session.udp_source = Some(ip),
                    b"dst" => session.udp_destination = Some(ip),
                    _ => session.udp_gateway = Some(ip),
                }
                rebuild_udp(session);
                true
            }
            None => false,
        },
        b"port" => match parse_u32(value).filter(|&port| (1..=65535).contains(&port)) {
            Some(port) => {
                session.udp_port = port as u16;
                rebuild_udp(session);
                true
            }
            None => false,
        },
        b"to" => {
            if value == b"sink" {
                session.destination_explicit = false;
                true
            } else if let Some(mac) = parse_mac(value) {
                generator.destination = mac;
                session.destination_explicit = true;
                true
            } else {
                false
            }
        }
        b"run" => {
            session.gen_autostart = value == b"on";
            value == b"on" || value == b"off"
        }
        _ => false,
    }
}

#[derive(Clone, Copy)]
enum Ways {
    Both,
    One(usize),
}

fn parse_way(word: &[u8]) -> Option<Ways> {
    match word {
        b"ab" | b"a>b" => Some(Ways::One(0)),
        b"ba" | b"b>a" => Some(Ways::One(1)),
        b"both" => Some(Ways::Both),
        _ => None,
    }
}

/// Applies one setting to the chosen direction(s). `args` is the value list:
/// `loss` takes PCT [BURST]; `delay` MS [JITTER]; `rate`, `queue`, `seed` one.
fn configure(session: &mut Session<'_>, name: &[u8], way: Ways, args: &[&[u8]]) -> Result<(), ()> {
    let first = args.first().copied().ok_or(())?;
    let second = args.get(1).copied();
    let selected = match way {
        Ways::Both => 0..2,
        Ways::One(index) => index..index + 1,
    };
    if name == b"seed" {
        session.rng = Rng::new(parse_u32(first).ok_or(())?);
        return Ok(());
    }
    for index in selected {
        let link = &mut session.links[index];
        let mut config: Config = link.config;
        match name {
            b"preset" => {
                config = preset(first, link.queue.capacity() as u32).ok_or(())?;
            }
            b"loss" => {
                config.loss_permille = parse_permille(first).ok_or(())?;
                if let Some(burst) = second {
                    config.burst = parse_u32(burst).ok_or(())?.clamp(1, 1000) as u16;
                }
            }
            b"delay" => {
                config.delay_ms = parse_u32(first).ok_or(())?.min(60_000);
                config.jitter_ms = match second {
                    Some(jitter) => parse_u32(jitter).ok_or(())?.min(60_000),
                    None => 0,
                };
            }
            b"rate" => config.rate_kbit = parse_u32(first).ok_or(())?,
            b"queue" => config.queue_limit = parse_u32(first).ok_or(())?,
            b"dup" => config.dup_permille = parse_permille(first).ok_or(())?,
            b"reorder" => config.reorder_permille = parse_permille(first).ok_or(())?,
            b"corrupt" => config.corrupt_permille = parse_permille(first).ok_or(())?,
            _ => return Err(()),
        }
        link.set_config(config);
    }
    Ok(())
}

/// Runs one command line from the prompt or a config file.
fn run_line(putc: Putc, session: &mut Session<'_>, line: &[u8]) -> Outcome {
    let line = match line.iter().position(|&byte| byte == b'#') {
        Some(hash) => &line[..hash],
        None => line,
    };
    let mut words = [&[][..]; MAX_WORDS];
    let count = split_words(line, &mut words);
    if count == 0 {
        return Outcome::Continue;
    }
    match words[0] {
        b"quit" | b"q" | b"exit" => return Outcome::Quit,
        b"help" | b"h" | b"?" => print_help(putc),
        b"show" => show(putc, session),
        b"stats" => print_stats(putc, session),
        b"reset" => {
            if count > 1 && words[1] == b"stats" {
                for link in session.links.iter_mut() {
                    link.stats = Default::default();
                }
                print_bytes(putc, b"counters cleared\n");
            } else {
                for link in session.links.iter_mut() {
                    link.reset();
                }
                print_bytes(putc, b"link restored to clean\n");
            }
        }
        b"view" => match words.get(1).copied() {
            // Before the loop starts (a config file line) only note the wish.
            Some(b"on") if count == 2 && !session.started => session.view.wanted = true,
            Some(b"on") if count == 2 => view_start(putc, session),
            Some(b"off") if count == 2 => view_stop(putc, session),
            _ => print_bytes(putc, b"use: view on | view off\n"),
        },
        b"link" => {
            let arg = words.get(1).copied().unwrap_or(b"");
            let second = words.get(2).copied().unwrap_or(b"");
            return relink(putc, session, arg, second);
        }
        b"summary" => print_summary(putc, session),
        b"save" => {
            let name = words.get(1).copied().unwrap_or(b"");
            if !valid_file_name(name) {
                print_bytes(putc, b"use: save NAME (up to 16 of a-z 0-9 . _ -)\n");
                return Outcome::Error;
            }
            save_report(putc, session, name);
        }
        b"clock" => {
            // Diagnostic: how fine and how steady is the microsecond clock?
            let mut previous = tape_rt::micros();
            let (mut smallest, mut largest, mut backwards) = (u64::MAX, 0u64, 0u32);
            for _ in 0..2000 {
                let now = tape_rt::micros();
                if now < previous {
                    backwards += 1;
                } else if now > previous {
                    smallest = smallest.min(now - previous);
                    largest = largest.max(now - previous);
                }
                previous = now;
            }
            print_bytes(putc, b"clock steps us: smallest ");
            print_u32(putc, smallest as u32);
            print_bytes(putc, b", largest ");
            print_u32(putc, largest as u32);
            print_bytes(putc, b", backwards ");
            print_u32(putc, backwards);
            putc(u32::from(b'\n'));
        }
        b"gen" => return gen_command(putc, session, &words[1..count]),
        b"sink" => match words.get(1).copied() {
            Some(b"nic") => {
                return retarget(putc, session, Role::Sink, words.get(2).copied().unwrap_or(b""));
            }
            Some(b"reset") => {
                session.sink.reset();
                session.rsink.reset();
                print_bytes(putc, b"sink counters cleared\n");
            }
            Some(setting @ (b"ip" | b"port")) => {
                if !sink_setting(session, setting, words.get(2).copied().unwrap_or(b"")) {
                    print_bytes(putc, b"bad value; type help\n");
                    return Outcome::Error;
                }
                print_sink_line(putc, session);
            }
            None | Some(b"show") => print_sink_line(putc, session),
            Some(b"hist") => print_histogram(putc, &session.sink.stats, b"forward"),
            _ => {
                print_bytes(putc, b"use: sink [show|reset]\n");
                return Outcome::Error;
            }
        },
        b"presets" => {
            for (name, description) in PRESETS {
                print_bytes(putc, name);
                print_bytes(putc, b"  ");
                print_bytes(putc, description);
                putc(u32::from(b'\n'));
            }
        }
        b"loss" | b"delay" | b"rate" | b"queue" | b"seed" | b"preset" | b"dup" | b"reorder"
        | b"corrupt" => {
            // An optional direction word first: `loss ab 5`.
            let (way, args) = match words.get(1).and_then(|word| parse_way(word)) {
                Some(way) if count > 1 => (way, &words[2..count]),
                _ => (Ways::Both, &words[1..count]),
            };
            // `loss 5 burst 3` and `loss 5 3` both give a burst length.
            let mut cleaned = [&[][..]; MAX_WORDS];
            let mut used = 0;
            for &word in args {
                if word != b"burst" && used < MAX_WORDS {
                    cleaned[used] = word;
                    used += 1;
                }
            }
            if configure(session, words[0], way, &cleaned[..used]).is_err() {
                print_bytes(putc, b"bad value; type help\n");
                return Outcome::Error;
            }
            show(putc, session);
        }
        _ => {
            print_bytes(putc, b"unknown command; type help\n");
            return Outcome::Error;
        }
    }
    Outcome::Continue
}

/// `gen start|stop|rate KBIT|size BYTES|to MAC|to sink|count N|time SECONDS|show`.
fn gen_command(putc: Putc, session: &mut Session<'_>, words: &[&[u8]]) -> Outcome {
    let Some(&verb) = words.first() else {
        print_gen_config(putc, session);
        return Outcome::Continue;
    };
    if verb == b"nic" {
        return retarget(putc, session, Role::Gen, words.get(1).copied().unwrap_or(b""));
    }
    if session.gen_nic.is_none() && verb != b"show" {
        print_bytes(putc, b"no generator NIC: start with gen=N (see 'nic map')\n");
        return Outcome::Error;
    }
    match verb {
        b"start" => {
            if !session.started {
                session.gen_autostart = true; // from a config file: start with the loop
            } else {
                start_generators(session, tape_rt::micros(), false);
                print_bytes(putc, b"generator started\n");
            }
        }
        b"bidir" => {
            session.bidir = words.get(1).copied() == Some(b"on");
            rebuild_udp(session);
            print_bytes(putc, if session.bidir { b"bidirectional: the sink NIC sends back too\n" } else { b"one direction\n" });
        }
        b"sweep" => {
            let mut numbers = [0u32; 4];
            for (slot, word) in numbers.iter_mut().zip(words[1..].iter()) {
                *slot = parse_u32(word).unwrap_or(0);
            }
            if words.len() != 5 || !set_sweep(session, numbers) {
                print_bytes(putc, b"use: gen sweep START END STEP SECONDS (kbit/s, seconds)\n");
                return Outcome::Error;
            }
            if !session.started {
                session.sweep_at_launch = true;
                session.gen_autostart = true;
            } else {
                start_generators(session, tape_rt::micros(), true);
                print_bytes(putc, b"sweep started\n");
            }
        }
        b"stop" => {
            session.sweep.active = false;
            session.generator.stop();
            session.rgen.stop();
            print_bytes(putc, b"generator stopped: ");
            print_gen_line(putc, session);
        }
        b"show" => print_gen_config(putc, session),
        b"rate" | b"size" | b"to" | b"count" | b"time" | b"ip" | b"dst" | b"gw" | b"port" => {
            let value = words.get(1).copied().unwrap_or(b"");
            if !gen_setting(session, verb, value) {
                print_bytes(putc, b"bad value; type help\n");
                return Outcome::Error;
            }
            if session.started && verb == b"to" && !session.destination_explicit {
                print_bytes(putc, b"(gen to sink takes effect at the next launch)\n");
            }
            print_gen_config(putc, session);
        }
        _ => {
            print_bytes(putc, b"use: gen start|stop|sweep|bidir|rate|size|to|count|time|show\n");
            return Outcome::Error;
        }
    }
    Outcome::Continue
}

enum Role {
    Gen,
    Sink,
}

/// NICs in use right now, as one-based-checked zero-based numbers.
fn nic_in_use(session: &Session<'_>, nic: u32, skip: Option<&Role>) -> bool {
    (session.limiter && session.ports.contains(&nic))
        || (!matches!(skip, Some(Role::Gen)) && session.gen_nic == Some(nic))
        || (!matches!(skip, Some(Role::Sink)) && session.sink_nic == Some(nic))
}

/// `gen nic N|off` / `sink nic N|off`: move a role to another NIC without
/// leaving the tape. The old traffic is stopped (a generator) or its counters
/// cleared (a sink), since they described a different wire.
fn retarget(putc: Putc, session: &mut Session<'_>, role: Role, arg: &[u8]) -> Outcome {
    let new = if arg == b"off" {
        None
    } else {
        match parse_u32(arg).filter(|&number| number >= 1) {
            Some(number) if (number as usize) <= session.nic_total => Some(number - 1),
            _ => {
                print_bytes(putc, b"use: nic N (see 'nic map') or nic off\n");
                return Outcome::Error;
            }
        }
    };
    if let Some(nic) = new {
        if nic_in_use(session, nic, Some(&role)) {
            print_bytes(putc, b"that NIC already has another role\n");
            return Outcome::Error;
        }
    }
    let mut mac = [0u8; 6];
    if let Some(nic) = new {
        if (session.mac_at)(nic, mac.as_mut_ptr() as usize) == 0 {
            print_bytes(putc, b"failed to read that NIC's MAC\n");
            return Outcome::Error;
        }
    }
    match role {
        Role::Gen => {
            session.sweep.active = false;
            session.generator.stop();
            session.rgen.stop();
            session.gen_nic = new;
            session.gen_mac = mac;
            if !session.destination_explicit {
                session.generator.destination =
                    if session.sink_nic.is_some() { session.sink_mac } else { [0xff; 6] };
            }
            session.rsink.reset();
            print_bytes(putc, b"generator stopped; ");
        }
        Role::Sink => {
            session.sink_nic = new;
            session.sink_mac = mac;
            session.sink.reset();
            session.rsink.reset();
            if !session.destination_explicit && session.gen_nic.is_some() {
                session.generator.destination = if new.is_some() { mac } else { [0xff; 6] };
            }
            print_bytes(putc, b"sink counters cleared; ");
        }
    }
    match new {
        Some(nic) => {
            print_u32(putc, nic + 1);
            print_bytes(putc, b" is now the NIC\n");
        }
        None => print_bytes(putc, b"role switched off\n"),
    }
    Outcome::Continue
}

/// `link A B` / `link off`: re-point the impaired link at two other NICs.
/// Promiscuous mode moves with it; queued frames of the old ports are dropped.
fn relink(putc: Putc, session: &mut Session<'_>, first: &[u8], second: &[u8]) -> Outcome {
    if first == b"off" {
        if session.limiter {
            for port in session.ports {
                let _ = (session.promisc_at)(port, 0);
            }
        }
        session.limiter = false;
        print_bytes(putc, b"impairment link off\n");
        return Outcome::Continue;
    }
    let numbers = (parse_u32(first), parse_u32(second));
    let (Some(a), Some(b)) = numbers else {
        print_bytes(putc, b"use: link A B (NIC numbers) | link off\n");
        return Outcome::Error;
    };
    let total = session.nic_total as u32;
    if a < 1 || b < 1 || a > total || b > total || a == b {
        print_bytes(putc, b"link needs two distinct NIC numbers (see 'nic list')\n");
        return Outcome::Error;
    }
    let (a, b) = (a - 1, b - 1);
    for nic in [a, b] {
        if session.gen_nic == Some(nic) || session.sink_nic == Some(nic) {
            print_bytes(putc, b"that NIC already has another role\n");
            return Outcome::Error;
        }
    }
    let mut macs = [[0u8; 6]; 2];
    if (session.mac_at)(a, macs[0].as_mut_ptr() as usize) == 0
        || (session.mac_at)(b, macs[1].as_mut_ptr() as usize) == 0
    {
        print_bytes(putc, b"failed to read a NIC MAC\n");
        return Outcome::Error;
    }
    if session.limiter {
        for port in session.ports {
            let _ = (session.promisc_at)(port, 0);
        }
    }
    if (session.promisc_at)(a, 1) == 0 || (session.promisc_at)(b, 1) == 0 {
        let _ = (session.promisc_at)(a, 0);
        session.limiter = false;
        print_bytes(putc, b"promiscuous mode failed; link is off\n");
        return Outcome::Error;
    }
    for link in session.links.iter_mut() {
        link.reset();
    }
    session.limiter = true;
    session.ports = [a, b];
    session.macs = macs;
    session.view.ports = [a + 1, b + 1];
    print_bytes(putc, b"impairment link moved\n");
    Outcome::Continue
}

fn print_mac(putc: Putc, mac: &[u8; 6]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for (index, byte) in mac.iter().enumerate() {
        if index != 0 {
            putc(u32::from(b':'));
        }
        putc(u32::from(HEX[usize::from(byte >> 4)]));
        putc(u32::from(HEX[usize::from(byte & 0x0f)]));
    }
}

/// `1234` -> `12.34`.
fn print_hundredths(putc: Putc, value: u32) {
    print_u32(putc, value / 100);
    putc(u32::from(b'.'));
    putc(u32::from(b'0' + (value / 10 % 10) as u8));
    putc(u32::from(b'0' + (value % 10) as u8));
}

fn print_gen_config(putc: Putc, session: &Session<'_>) {
    print_gen_config_line(putc, session);
    putc(u32::from(b'\n'));
    print_gen_udp_line(putc, session);
}

/// The generator's settings on one line (no newline): used by the panel too.
fn print_gen_config_line(putc: Putc, session: &Session<'_>) {
    let generator = &session.generator;
    print_bytes(putc, b"gen  NIC ");
    match session.gen_nic {
        Some(nic) => print_u32(putc, nic + 1),
        None => print_bytes(putc, b"(none)"),
    }
    print_bytes(putc, b" -> ");
    print_mac(putc, &generator.destination);
    print_bytes(putc, if generator.running { "  RUNNING  " } else { "  stopped  " }.as_bytes());
    print_u32(putc, generator.rate_kbit);
    print_bytes(putc, b" kbit/s of ");
    print_u32(putc, generator.size as u32);
    print_bytes(putc, b" B frames");
    if generator.count_limit != 0 {
        print_bytes(putc, b", ");
        print_u32(putc, generator.count_limit);
        print_bytes(putc, b" frames");
    }
    if generator.seconds_limit != 0 {
        print_bytes(putc, b", ");
        print_u32(putc, generator.seconds_limit);
        print_bytes(putc, b" s");
    }
}

/// The routable-mode addresses, when set (with its own newline).
fn print_gen_udp_line(putc: Putc, session: &Session<'_>) {
    let generator = &session.generator;
    if let Some(plan) = generator.udp {
        print_bytes(putc, b"     UDP ");
        print_ip(putc, &plan.source_ip);
        print_bytes(putc, b" -> ");
        print_ip(putc, &plan.destination_ip);
        print_bytes(putc, b":");
        print_u32(putc, u32::from(plan.destination_port));
        print_bytes(putc, b" via ");
        print_ip(putc, &plan.next_hop);
        print_bytes(putc, if generator.resolved { b" (resolved)\n" } else { b" (ARP pending)\n" });
    }
}

fn print_ip(putc: Putc, ip: &[u8; 4]) {
    for (index, byte) in ip.iter().enumerate() {
        if index != 0 {
            putc(u32::from(b'.'));
        }
        print_u32(putc, u32::from(*byte));
    }
}

fn print_gen_line(putc: Putc, session: &Session<'_>) {
    let stats = &session.generator.stats;
    print_bytes(putc, b"sent ");
    print_u32(putc, stats.sent);
    print_bytes(putc, b", failed ");
    print_u32(putc, stats.send_failed);
    if stats.lagged > 0 {
        print_bytes(putc, b", fell behind schedule ");
        print_u32(putc, stats.lagged);
        print_bytes(putc, b"x");
    }
    putc(u32::from(b'\n'));
}

fn print_sink_line(putc: Putc, session: &Session<'_>) {
    print_sink_summary(putc, session);
    putc(u32::from(b'\n'));
    print_sink_latency(putc, session);
    putc(u32::from(b'\n'));
}

/// Counts, average rate and loss -- one line, no newline.
fn print_sink_summary(putc: Putc, session: &Session<'_>) {
    let stats = &session.sink.stats;
    print_bytes(putc, b"sink NIC ");
    match session.sink_nic {
        Some(nic) => print_u32(putc, nic + 1),
        None => print_bytes(putc, b"(none)"),
    }
    print_bytes(putc, b"  rx ");
    print_u32(putc, stats.received);
    print_bytes(putc, b"  avg ");
    print_u32(putc, session.sink.average_kbit() as u32);
    print_bytes(putc, b" kbit/s (now ");
    print_u32(putc, session.view.kbit.1);
    print_bytes(putc, b")  lost ");
    print_u32(putc, stats.lost);
    let expected = stats.received.saturating_add(stats.lost).max(1);
    print_bytes(putc, b" (");
    print_hundredths(putc, ((u64::from(stats.lost) * 10_000) / u64::from(expected)) as u32);
    print_bytes(putc, b"%)  dup ");
    print_u32(putc, stats.duplicated);
    print_bytes(putc, b"  reord ");
    print_u32(putc, stats.reordered);
}

/// One-way latency: the generator and sink share a clock, so it is exact.
fn print_sink_latency(putc: Putc, session: &Session<'_>) {
    let stats = &session.sink.stats;
    print_bytes(putc, b"     latency us: min ");
    print_u32(putc, stats.latency_min);
    print_bytes(putc, b"  avg ");
    print_u32(putc, (stats.latency_sum / u64::from(stats.received.max(1))) as u32);
    print_bytes(putc, b"  max ");
    print_u32(putc, stats.latency_max);
    print_bytes(putc, b"  jitter ");
    print_u32(putc, stats.jitter);
}

fn print_help(putc: Putc) {
    print_bytes(
        putc,
        b"loss [ab|ba] PCT [burst N]   drop PCT% (0.1 steps); N lost in a row\n\
          delay [ab|ba] MS [JITTER]    hold frames MS ms +/- JITTER ms\n\
          rate [ab|ba] KBIT            link speed; 0 = unlimited\n\
          queue [ab|ba] N              frames allowed in flight\n\
          dup|reorder|corrupt [ab|ba] PCT   deliver twice / swap with next / flip a bit\n\
          preset [ab|ba] NAME          a named profile; \"presets\" lists them\n\
          seed N | show | stats        repeatable randomness; settings; counters\n\
          view on|off                  live statistics panel above the prompt\n\
          reset [stats]                clean link, or clear the counters\n\
          gen start|stop|show          the traffic generator (needs gen=N)\n\
          gen nic N|off | sink nic N|off | link A B|off   re-point a role\n\
          gen rate KBIT | size BYTES   constant bit rate of whole frames\n\
          gen sweep START END STEP S   step the rate, S seconds each; table of results\n\
          gen bidir on|off             also send a stream back, sink NIC -> gen NIC\n\
          gen to MAC|sink | count N | time S    destination; stop after N / S\n\
          gen ip|dst|gw A.B.C.D | port N   send routable UDP/IP (ARP for the gateway)\n\
          sink [show|hist|reset]       the receiver (needs sink=N): loss, latency\n\
          summary | save NAME          end-of-test summary; write a report file\n\
          sink ip A.B.C.D | port N     also accept UDP/IP; answer ARP for the address\n\
          quit                         stop (Escape also stops)\n",
    );
}

fn print_tenths(putc: Putc, permille: u16) {
    print_u32(putc, u32::from(permille / 10));
    print_bytes(putc, b".");
    print_u32(putc, u32::from(permille % 10));
}

fn show(putc: Putc, session: &Session<'_>) {
    if session.gen_nic.is_some() {
        print_gen_config(putc, session);
    }
    if !session.limiter {
        return;
    }
    for index in 0..2 {
        print_config_line(putc, index, &session.links[index].config);
        putc(u32::from(b'\n'));
        if session.links[index].config.has_faults() {
            print_fault_line(putc, &session.links[index].config);
            putc(u32::from(b'\n'));
        }
    }
}

/// `dup 1.0%  reorder 2.0%  corrupt 0.5%` -- only the ones that are set.
fn print_fault_line(putc: Putc, config: &Config) {
    print_bytes(putc, b"     ");
    for (name, value) in [
        (&b"dup "[..], config.dup_permille),
        (&b"reorder "[..], config.reorder_permille),
        (&b"corrupt "[..], config.corrupt_permille),
    ] {
        if value != 0 {
            print_bytes(putc, name);
            print_tenths(putc, value);
            print_bytes(putc, b"%  ");
        }
    }
}

/// One direction's settings on one line (no newline), shared by `show` and the
/// statistics panel.
fn print_config_line(putc: Putc, index: usize, config: &Config) {
    print_bytes(putc, DIRECTION_NAMES[index]);
    if config.is_clean() {
        print_bytes(putc, b"  clean");
    } else {
        print_bytes(putc, b"  loss ");
        print_tenths(putc, config.loss_permille);
        print_bytes(putc, b"%");
        if config.burst > 1 {
            print_bytes(putc, b" x");
            print_u32(putc, u32::from(config.burst));
        }
        print_bytes(putc, b"  delay ");
        print_u32(putc, config.delay_ms);
        if config.jitter_ms > 0 {
            print_bytes(putc, b"+-");
            print_u32(putc, config.jitter_ms);
        }
        print_bytes(putc, b"ms  rate ");
        if config.rate_kbit == 0 {
            print_bytes(putc, b"off");
        } else {
            print_u32(putc, config.rate_kbit);
            print_bytes(putc, b"kbit");
        }
    }
    print_bytes(putc, b"  queue ");
    print_u32(putc, config.queue_limit);
}

fn valid_file_name(name: &[u8]) -> bool {
    !name.is_empty()
        && name.len() <= 16
        && name
            .iter()
            .all(|byte| matches!(*byte, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'))
}

/// `<N` for a bucket bound, `>5000000` for the open-ended one.
fn print_bound(putc: Putc, bound: u32) {
    if bound == u32::MAX {
        print_bytes(putc, b">5000000");
    } else {
        putc(u32::from(b'<'));
        print_u32(putc, bound);
    }
}

fn print_latency_percentiles(putc: Putc, stats: &nettest_tape::SinkStats) {
    print_bytes(putc, b"  latency us: median ");
    print_bound(putc, stats.latency_percentile(500));
    print_bytes(putc, b", 95% ");
    print_bound(putc, stats.latency_percentile(950));
    print_bytes(putc, b", 99% ");
    print_bound(putc, stats.latency_percentile(990));
    print_bytes(putc, b", max ");
    print_u32(putc, stats.latency_max);
    print_bytes(putc, b", jitter ");
    print_u32(putc, stats.jitter);
    putc(u32::from(b'\n'));
}

/// The end-of-test summary: what was sent, what arrived, loss and latency.
fn print_summary(putc: Putc, session: &Session<'_>) {
    print_bytes(putc, b"summary: sent ");
    print_u32(putc, session.generator.stats.sent);
    summarize_sink(putc, &session.sink, b"got ");
    print_latency_percentiles(putc, &session.sink.stats);
    if session.bidir {
        print_bytes(putc, b"reverse: sent ");
        print_u32(putc, session.rgen.stats.sent);
        summarize_sink(putc, &session.rsink, b"got ");
        print_latency_percentiles(putc, &session.rsink.stats);
    }
}

/// `, got N (K kbit/s over S.S s), loss P%` and a newline.
fn summarize_sink(putc: Putc, sink: &Sink, label: &[u8]) {
    let stats = &sink.stats;
    print_bytes(putc, b", ");
    print_bytes(putc, label);
    print_u32(putc, stats.received);
    print_bytes(putc, b" (");
    print_u32(putc, sink.average_kbit() as u32);
    print_bytes(putc, b" kbit/s over ");
    let span_ms = (stats.last_us.saturating_sub(stats.first_us) / 100_000) as u32; // tenths of a second
    print_u32(putc, span_ms / 10);
    putc(u32::from(b'.'));
    print_u32(putc, span_ms % 10);
    print_bytes(putc, b" s), loss ");
    let expected = stats.received.saturating_add(stats.lost).max(1);
    print_hundredths(putc, ((u64::from(stats.lost) * 10_000) / u64::from(expected)) as u32);
    print_bytes(putc, b"%\n");
}

/// Bars for the non-empty buckets of a latency histogram.
fn print_histogram(putc: Putc, stats: &nettest_tape::SinkStats, label: &[u8]) {
    print_bytes(putc, label);
    print_bytes(putc, b" latency histogram (us):\n");
    let largest = stats.histogram.iter().copied().max().unwrap_or(0).max(1);
    for (index, &count) in stats.histogram.iter().enumerate() {
        if count == 0 {
            continue;
        }
        print_bytes(putc, b"  ");
        let bound = HISTOGRAM_BOUNDS_US.get(index).copied().unwrap_or(u32::MAX);
        let mut text = [b' '; 9];
        let mut writer = [0u8; 9];
        let digits = format_bound(bound, &mut writer);
        text[9 - digits..].copy_from_slice(&writer[..digits]);
        print_bytes(putc, &text);
        putc(u32::from(b' '));
        print_u32(putc, count);
        putc(u32::from(b' '));
        let bar = ((u64::from(count) * 40).div_ceil(u64::from(largest))) as usize;
        for _ in 0..bar {
            putc(u32::from(b'#'));
        }
        putc(u32::from(b'\n'));
    }
}

/// Writes `<N` / `>5000000` right-aligned material into `out`; returns its length.
fn format_bound(bound: u32, out: &mut [u8; 9]) -> usize {
    if bound == u32::MAX {
        out[..8].copy_from_slice(b">5000000");
        return 8;
    }
    let mut digits = [0u8; 8];
    let mut count = 0;
    let mut value = bound;
    while value > 0 {
        digits[count] = b'0' + (value % 10) as u8;
        value /= 10;
        count += 1;
    }
    out[0] = b'<';
    for index in 0..count {
        out[1 + index] = digits[count - 1 - index];
    }
    1 + count
}

/// Report text is captured here so `save` can hand it to the filesystem in one
/// piece: the same print functions run, but with `capture` as their `putc`.
static mut REPORT: [u8; 3072] = [0; 3072];
static mut REPORT_LENGTH: usize = 0;

extern "C" fn capture(byte: u32) {
    unsafe {
        let report = &mut *addr_of_mut!(REPORT);
        if REPORT_LENGTH < report.len() {
            report[REPORT_LENGTH] = byte as u8;
            REPORT_LENGTH += 1;
        }
    }
}

/// Writes the settings, summary, counters and latency histogram to FOSFS file
/// `name` (replacing it), so a run leaves its numbers behind.
fn save_report(putc: Putc, session: &Session<'_>, name: &[u8]) {
    let Some(write) = session.fs_write else {
        print_bytes(putc, b"no file system access\n");
        return;
    };
    unsafe { REPORT_LENGTH = 0 };
    print_bytes(capture, b"nettest report\n");
    show(capture, session);
    print_summary(capture, session);
    print_stats(capture, session);
    print_histogram(capture, &session.sink.stats, b"forward");
    if session.bidir {
        print_histogram(capture, &session.rsink.stats, b"reverse");
    }
    let length = unsafe { REPORT_LENGTH };
    let status = write(
        name.as_ptr() as usize,
        name.len() as u32,
        addr_of_mut!(REPORT) as usize,
        length as u32,
    );
    if status == 0 {
        print_bytes(putc, b"saved ");
        print_u32(putc, length as u32);
        print_bytes(putc, b" bytes to ");
        print_bytes(putc, name);
        putc(u32::from(b'\n'));
    } else {
        print_bytes(putc, b"could not write the report (status ");
        print_u32(putc, status);
        print_bytes(putc, b")\n");
    }
}

/// The reverse stream's two lines, when bidirectional.
fn print_reverse(putc: Putc, session: &Session<'_>) {
    if !session.bidir {
        return;
    }
    let (generator, sink) = (&session.rgen.stats, &session.rsink.stats);
    print_bytes(putc, b"rev  sent ");
    print_u32(putc, generator.sent);
    print_bytes(putc, b", failed ");
    print_u32(putc, generator.send_failed);
    print_bytes(putc, b"  rx ");
    print_u32(putc, sink.received);
    print_bytes(putc, b" avg ");
    print_u32(putc, session.rsink.average_kbit() as u32);
    print_bytes(putc, b" kbit/s lost ");
    print_u32(putc, sink.lost);
    print_bytes(putc, b" dup ");
    print_u32(putc, sink.duplicated);
    print_bytes(putc, b" reord ");
    print_u32(putc, sink.reordered);
    print_bytes(putc, b"\n     latency us: min ");
    print_u32(putc, sink.latency_min);
    print_bytes(putc, b" avg ");
    print_u32(putc, (sink.latency_sum / u64::from(sink.received.max(1))) as u32);
    print_bytes(putc, b" max ");
    print_u32(putc, sink.latency_max);
    putc(u32::from(b'\n'));
}

fn print_stats(putc: Putc, session: &Session<'_>) {
    if session.passes > 0 {
        let elapsed = tape_rt::micros().saturating_sub(session.loop_started_us);
        print_bytes(putc, b"loop: ");
        print_u32(putc, session.passes as u32);
        print_bytes(putc, b" passes, ");
        print_u32(putc, (elapsed / session.passes) as u32);
        print_bytes(putc, b" us each on average\n");
    }
    if session.gen_nic.is_some() {
        print_bytes(putc, b"gen  ");
        print_gen_line(putc, session);
    }
    if session.sink_nic.is_some() {
        print_sink_line(putc, session);
    }
    print_reverse(putc, session);
    if !session.limiter {
        return;
    }
    for (index, link) in session.links.iter().enumerate() {
        let stats = &link.stats;
        print_bytes(putc, DIRECTION_NAMES[index]);
        print_bytes(putc, b"  in ");
        print_u32(putc, stats.received);
        print_bytes(putc, b"  out ");
        print_u32(putc, stats.sent);
        print_bytes(putc, b"  lost ");
        print_u32(putc, stats.lost);
        print_bytes(putc, b"  qdrop ");
        print_u32(putc, stats.queue_drops);
        print_bytes(putc, b"  queued ");
        print_u32(putc, link.queue.len() as u32);
        print_bytes(putc, b" (max ");
        print_u32(putc, stats.max_depth);
        print_bytes(putc, b")\n");
        if stats.duplicated + stats.reordered + stats.corrupted > 0 {
            print_bytes(putc, b"      duplicated ");
            print_u32(putc, stats.duplicated);
            print_bytes(putc, b"  reordered ");
            print_u32(putc, stats.reordered);
            print_bytes(putc, b"  corrupted ");
            print_u32(putc, stats.corrupted);
            putc(u32::from(b'\n'));
        }
        if stats.send_failed > 0 {
            print_bytes(putc, b"      send failures ");
            print_u32(putc, stats.send_failed);
            putc(u32::from(b'\n'));
        }
    }
}

// ---------------------------------------------------------------------------
// The optional statistics panel (`view on`, or `view=on` at startup).
//
// It uses only ANSI sequences, so it looks the same on the local screen and on
// a `term net` / serial terminal: the top PANEL_ROWS rows hold the panel,
// redrawn twice a second at absolute positions between `ESC [ s` / `ESC [ u`
// (save/restore cursor), and `ESC [ top;bottom r` confines scrolling to the
// rows below it, so command output and the prompt scroll there and never
// disturb the panel. Drawing costs a few hundred bytes per update.

const SCREEN_ROWS: u32 = 25;
const PANEL_INTERVAL_TICKS: u64 = 50;
const BAR_WIDTH: usize = 24;

struct View {
    /// `view=on` was given before the ports were known.
    wanted: bool,
    on: bool,
    ports: [u32; 2],
    start_tick: u64,
    last_tick: u64,
    previous: [(u32, u32); 2],
    rates: [(u32, u32); 2],
    /// Panel height in rows, fixed when the panel is switched on.
    rows: u32,
    /// Generator bytes sent and sink bytes received at the last refresh, and
    /// the resulting kbit/s over that interval.
    previous_bytes: (u64, u64),
    kbit: (u32, u32),
}

impl View {
    const fn new() -> Self {
        Self {
            wanted: false,
            on: false,
            ports: [1, 2],
            start_tick: 0,
            last_tick: 0,
            previous: [(0, 0); 2],
            rates: [(0, 0); 2],
            rows: 12,
            previous_bytes: (0, 0),
            kbit: (0, 0),
        }
    }
}

fn csi(putc: Putc, parameters: &[u32], final_byte: u8) {
    putc(0x1b);
    putc(u32::from(b'['));
    for (index, &value) in parameters.iter().enumerate() {
        if index != 0 {
            putc(u32::from(b';'));
        }
        print_u32(putc, value);
    }
    putc(u32::from(final_byte));
}

/// Rows the panel needs: title and rule, four per impaired direction, four for
/// the generator and sink, then a closing rule and the hint line.
fn panel_rows(session: &Session<'_>) -> u32 {
    let roles = session.gen_nic.is_some() || session.sink_nic.is_some();
    2 + if session.limiter { 8 } else { 0 } + if roles { 4 } else { 0 } + if session.bidir { 2 } else { 0 } + 2
}

fn view_start(putc: Putc, session: &mut Session<'_>) {
    session.view.on = true;
    session.view.last_tick = 0;
    session.view.rows = panel_rows(session);
    csi(putc, &[2], b'J');
    csi(putc, &[session.view.rows + 1, SCREEN_ROWS], b'r');
    csi(putc, &[SCREEN_ROWS, 1], b'H');
    view_draw(putc, session, 0);
}

fn view_stop(putc: Putc, session: &mut Session<'_>) {
    session.view.on = false;
    csi(putc, &[], b'r');
    csi(putc, &[2], b'J');
    csi(putc, &[1, 1], b'H');
}

fn view_update(putc: Putc, session: &mut Session<'_>, now_tick: u64) {
    if now_tick - session.view.last_tick < PANEL_INTERVAL_TICKS {
        return;
    }
    let elapsed = (now_tick - session.view.last_tick).max(1) as u32;
    session.view.last_tick = now_tick;
    for index in 0..2 {
        let stats = session.links[index].stats;
        let (previous_in, previous_out) = session.view.previous[index];
        // Per second: delta * 100 ticks / elapsed ticks.
        session.view.rates[index] = (
            stats.received.wrapping_sub(previous_in).saturating_mul(100) / elapsed,
            stats.sent.wrapping_sub(previous_out).saturating_mul(100) / elapsed,
        );
        session.view.previous[index] = (stats.received, stats.sent);
    }
    // kbit/s over the interval: bytes * 8 / 1000 per (elapsed / 100) seconds.
    let sent = session.generator.stats.bytes;
    let received = session.sink.stats.bytes;
    let (previous_sent, previous_received) = session.view.previous_bytes;
    let kbit = |now: u64, before: u64| (now.saturating_sub(before) * 8 * 100 / 1000 / u64::from(elapsed)) as u32;
    session.view.kbit = (kbit(sent, previous_sent), kbit(received, previous_received));
    session.view.previous_bytes = (sent, received);
    view_draw(putc, session, now_tick);
}

fn view_row(putc: Putc, row: u32) {
    csi(putc, &[row, 1], b'H');
}

fn rule(putc: Putc) {
    for _ in 0..78 {
        putc(u32::from(b'-'));
    }
}

fn view_draw(putc: Putc, session: &Session<'_>, now_tick: u64) {
    putc(0x1b);
    putc(u32::from(b'['));
    putc(u32::from(b's'));
    view_row(putc, 1);
    print_bytes(putc, b" nettest  ");
    if session.limiter {
        print_bytes(putc, b"NIC ");
        print_u32(putc, session.view.ports[0]);
        print_bytes(putc, b" <-> NIC ");
        print_u32(putc, session.view.ports[1]);
        print_bytes(putc, b"   ");
    }
    print_bytes(putc, b"up ");
    let seconds = (now_tick.saturating_sub(session.view.start_tick) / 100) as u32;
    print_two_digits(putc, seconds / 3600);
    putc(u32::from(b':'));
    print_two_digits(putc, seconds / 60 % 60);
    putc(u32::from(b':'));
    print_two_digits(putc, seconds % 60);
    csi(putc, &[], b'K');
    view_row(putc, 2);
    rule(putc);
    csi(putc, &[], b'K');
    let mut row = 3;
    if session.limiter {
        for index in 0..2 {
            draw_direction(putc, session, index, row);
            row += 4;
        }
    }
    if session.gen_nic.is_some() || session.sink_nic.is_some() {
        view_row(putc, row);
        if session.gen_nic.is_some() {
            print_gen_config_line(putc, session);
        }
        csi(putc, &[], b'K');
        view_row(putc, row + 1);
        if session.gen_nic.is_some() {
            print_bytes(putc, b"     ");
            print_gen_line_plain(putc, session);
            print_bytes(putc, b"  now ");
            print_u32(putc, session.view.kbit.0);
            print_bytes(putc, b" kbit/s");
        }
        csi(putc, &[], b'K');
        view_row(putc, row + 2);
        if session.sink_nic.is_some() {
            print_sink_summary(putc, session);
        }
        csi(putc, &[], b'K');
        view_row(putc, row + 3);
        if session.sink_nic.is_some() {
            print_sink_latency(putc, session);
        }
        csi(putc, &[], b'K');
        if session.bidir {
            // print_reverse ends each of its two lines with a newline; the panel
            // positions rows absolutely, so draw them one at a time.
            let (generator, sink) = (&session.rgen.stats, &session.rsink.stats);
            view_row(putc, row + 4);
            print_bytes(putc, b"rev  sent ");
            print_u32(putc, generator.sent);
            print_bytes(putc, b"  rx ");
            print_u32(putc, sink.received);
            print_bytes(putc, b" avg ");
            print_u32(putc, session.rsink.average_kbit() as u32);
            print_bytes(putc, b" kbit/s  lost ");
            print_u32(putc, sink.lost);
            print_bytes(putc, b"  dup ");
            print_u32(putc, sink.duplicated);
            print_bytes(putc, b"  reord ");
            print_u32(putc, sink.reordered);
            csi(putc, &[], b'K');
            view_row(putc, row + 5);
            print_bytes(putc, b"     latency us: min ");
            print_u32(putc, sink.latency_min);
            print_bytes(putc, b"  avg ");
            print_u32(putc, (sink.latency_sum / u64::from(sink.received.max(1))) as u32);
            print_bytes(putc, b"  max ");
            print_u32(putc, sink.latency_max);
            csi(putc, &[], b'K');
        }
    }
    view_row(putc, session.view.rows - 1);
    rule(putc);
    csi(putc, &[], b'K');
    view_row(putc, session.view.rows);
    print_bytes(putc, b" view off | gen start|stop | sink reset | help | quit");
    csi(putc, &[], b'K');
    putc(0x1b);
    putc(u32::from(b'['));
    putc(u32::from(b'u'));
}

fn draw_direction(putc: Putc, session: &Session<'_>, index: usize, base: u32) {
    let link = &session.links[index];
    let stats = &link.stats;
    view_row(putc, base);
    print_config_line(putc, index, &link.config);
    csi(putc, &[], b'K');
    view_row(putc, base + 1);
    print_bytes(putc, b"  in ");
    print_u32(putc, stats.received);
    print_bytes(putc, b" (");
    print_u32(putc, session.view.rates[index].0);
    print_bytes(putc, b"/s)  out ");
    print_u32(putc, stats.sent);
    print_bytes(putc, b" (");
    print_u32(putc, session.view.rates[index].1);
    print_bytes(putc, b"/s)  lost ");
    print_u32(putc, stats.lost);
    print_bytes(putc, b"  qdrop ");
    print_u32(putc, stats.queue_drops);
    csi(putc, &[], b'K');
    view_row(putc, base + 2);
    print_bytes(putc, b"  queue [");
    let limit = link.config.queue_limit.max(1) as usize;
    let depth = link.queue.len();
    let filled = (depth * BAR_WIDTH).div_ceil(limit).min(BAR_WIDTH);
    for position in 0..BAR_WIDTH {
        putc(u32::from(if position < filled { b'#' } else { b'.' }));
    }
    print_bytes(putc, b"] ");
    print_u32(putc, depth as u32);
    print_bytes(putc, b"/");
    print_u32(putc, limit as u32);
    print_bytes(putc, b"  max ");
    print_u32(putc, stats.max_depth);
    csi(putc, &[], b'K');
    view_row(putc, base + 3);
    if link.config.has_faults() || stats.duplicated + stats.reordered + stats.corrupted > 0 {
        print_fault_line(putc, &link.config);
        print_bytes(putc, b"(done: dup ");
        print_u32(putc, stats.duplicated);
        print_bytes(putc, b", reorder ");
        print_u32(putc, stats.reordered);
        print_bytes(putc, b", corrupt ");
        print_u32(putc, stats.corrupted);
        print_bytes(putc, b")");
    }
    csi(putc, &[], b'K');
}

/// `sent N, failed N` without the newline `print_gen_line` ends with.
fn print_gen_line_plain(putc: Putc, session: &Session<'_>) {
    let stats = &session.generator.stats;
    print_bytes(putc, b"sent ");
    print_u32(putc, stats.sent);
    print_bytes(putc, b", failed ");
    print_u32(putc, stats.send_failed);
    if stats.lagged > 0 {
        print_bytes(putc, b", behind ");
        print_u32(putc, stats.lagged);
        print_bytes(putc, b"x");
    }
}

fn print_two_digits(putc: Putc, value: u32) {
    putc(u32::from(b'0' + (value / 10 % 10) as u8));
    putc(u32::from(b'0' + (value % 10) as u8));
}

tape_rt::tape_help!(br#"nettest: two-port link impairment (loss, delay, rate) and traffic test.

  tape run nettest [A B] [loss=PCT[:BURST]] [delay=MS[:JITTER]] [rate=KBIT]
                   [queue=N] [seed=N]      prefix with ab. or ba. per direction
  tape run nettest @nettest.cfg            one command per line

  At the nettest> prompt: loss, delay, rate, queue, seed, show, stats,
  reset, help, quit. Escape stops. See docs/nettest.md.
"#);
