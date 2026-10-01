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
use nettest_tape::{Config, Direction, MAX_FRAME, Rng, SLOT_BYTES, parse_permille, parse_u32};
use tape_rt::{print_bytes, print_u32};

const TAPE_EVENT_ABORT: u32 = 1;
const TAPE_EVENT_BYTE_BASE: u32 = 0x100;
const ETHERNET_HEADER_LEN: usize = 14;
const LINE_CAPACITY: usize = 120;
const MAX_WORDS: usize = 8;

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
}

enum Outcome {
    Continue,
    Quit,
}

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(
    putc: Putc,
    _should_abort: extern "C" fn() -> u32,
    alloc: extern "C" fn(u32) -> u32,
    free: extern "C" fn(u32, u32),
    read_key: extern "C" fn() -> u32,
    args_ptr: u32,
    args_len: u32,
    _nic_mac: extern "C" fn(u32) -> u32,
    _nic_send: extern "C" fn(u32, u32) -> u32,
    _nic_receive: extern "C" fn(u32, u32) -> u32,
    nic_max_frame_len: extern "C" fn() -> u32,
    ticks: extern "C" fn() -> u64,
    nic_count: extern "C" fn() -> u32,
    nic_mac_at: extern "C" fn(u32, u32) -> u32,
    nic_send_at: extern "C" fn(u32, u32, u32) -> u32,
    nic_receive_at: extern "C" fn(u32, u32, u32) -> u32,
    nic_set_promiscuous_at: extern "C" fn(u32, u32) -> u32,
    _terminal_position: extern "C" fn() -> u32,
    _fs_file_length: extern "C" fn(u32, u32, u32) -> u32,
    _fs_read: extern "C" fn(u32, u32, u32, u32, u32) -> u32,
    _fs_write: extern "C" fn(u32, u32, u32, u32) -> u32,
    _fs_remove: extern "C" fn(u32, u32) -> u32,
    _audio_sample_rate: extern "C" fn() -> u32,
    _audio_channels_and_bits: extern "C" fn() -> u32,
    _audio_submit: extern "C" fn(u32, u32) -> u32,
    _audio_capture: extern "C" fn(u32, u32) -> u32,
    _audio_count: extern "C" fn() -> u32,
    _audio_sample_rate_at: extern "C" fn(u32) -> u32,
    _audio_channels_and_bits_at: extern "C" fn(u32) -> u32,
    _audio_submit_at: extern "C" fn(u32, u32, u32) -> u32,
    _audio_capture_at: extern "C" fn(u32, u32, u32) -> u32,
    _audio_capture_sample_rate: extern "C" fn() -> u32,
    _audio_capture_channels_and_bits: extern "C" fn() -> u32,
    _audio_capture_sample_rate_at: extern "C" fn(u32) -> u32,
    _audio_capture_channels_and_bits_at: extern "C" fn(u32) -> u32,
    wait_for_event: extern "C" fn(),
) {
    tape_rt::set_wait_hook(wait_for_event);
    let nic_total = nic_count() as usize;
    if nic_total < 2 {
        print_bytes(putc, b"nettest needs at least two initialized NICs.\n");
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
    };

    let arguments = if args_len == 0 {
        &[][..]
    } else {
        unsafe { core::slice::from_raw_parts(args_ptr as *const u8, args_len as usize) }
    };
    let mut ports = [0u32; 2];
    let mut port_count = 0;
    if !apply_startup(putc, &mut session, arguments, &mut ports, &mut port_count) {
        print_bytes(
            putc,
            b"Usage: tape run nettest [A B] [loss=PCT[:BURST]] [delay=MS[:JITTER]]\n\
              \x20      [rate=KBIT] [queue=N] [seed=N]   or   tape run nettest @FILE\n",
        );
        release(big, free);
        return;
    }
    if port_count == 0 {
        ports = [0, 1];
    } else if port_count != 2 || ports[0] == ports[1] {
        print_bytes(putc, b"nettest needs two distinct NIC numbers (see 'nic list').\n");
        release(big, free);
        return;
    }
    if ports.iter().any(|&port| port as usize >= nic_total) {
        print_bytes(putc, b"nettest: no such NIC (see 'nic list').\n");
        release(big, free);
        return;
    }
    let max_frame_len = (nic_max_frame_len() as usize).clamp(ETHERNET_HEADER_LEN, MAX_FRAME);

    let mut macs = [[0u8; 6]; 2];
    for (index, mac) in macs.iter_mut().enumerate() {
        if nic_mac_at(ports[index], mac.as_mut_ptr() as u32) == 0 {
            print_bytes(putc, b"nettest cannot start: failed to read a NIC MAC.\n");
            release(big, free);
            return;
        }
    }
    for index in 0..2 {
        if nic_set_promiscuous_at(ports[index], 1) == 0 {
            if index == 1 {
                let _ = nic_set_promiscuous_at(ports[0], 0);
            }
            print_bytes(putc, b"nettest cannot start: promiscuous mode failed.\n");
            release(big, free);
            return;
        }
    }

    print_bytes(putc, b"fos nettest: NIC ");
    print_u32(putc, ports[0] + 1);
    print_bytes(putc, b" <-> NIC ");
    print_u32(putc, ports[1] + 1);
    print_bytes(putc, b" (promiscuous), ");
    print_u32(putc, session.links[0].queue.capacity() as u32);
    print_bytes(putc, b" frames of queue per way.\n");
    print_bytes(putc, b"Type help for commands; Escape stops.\n");
    show(putc, &session);

    let mut frame = [0u8; MAX_FRAME];
    let mut line = [0u8; LINE_CAPACITY];
    let mut line_length = 0usize;
    print_bytes(putc, b"nettest> ");
    'run: loop {
        let now_us = ticks() * 10_000;
        let mut busy = false;

        // Receive from each port and push through that direction's impairments.
        for ingress in 0..2 {
            let received = nic_receive_at(
                ports[ingress],
                frame.as_mut_ptr() as u32,
                max_frame_len as u32,
            ) as usize;
            if received < ETHERNET_HEADER_LEN {
                continue;
            }
            busy = true;
            // Frames to or from this tape's own NICs are management traffic
            // (a `term net` session), not part of the link under test.
            let destination = &frame[0..6];
            let source = &frame[6..12];
            if macs.iter().any(|mac| mac == destination || mac == source) {
                continue;
            }
            let Session { links, rng } = &mut session;
            links[ingress].ingest(&frame[..received], now_us, rng);
        }

        // Send whatever has come due.
        for way in 0..2 {
            while let Some(length) = session.links[way].release(now_us, &mut frame) {
                busy = true;
                let ok = nic_send_at(ports[1 - way], frame.as_ptr() as u32, length as u32) != 0;
                session.links[way].record_send(length, ok);
            }
        }

        // One key per pass, so forwarding is never starved by typing.
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
        if !busy {
            wait_for_event();
        }
    }

    for &port in &ports {
        let _ = nic_set_promiscuous_at(port, 0);
    }
    print_bytes(putc, b"nettest stopped.\n");
    print_stats(putc, &session);
    release(big, free);
}

fn release(big: u32, free: extern "C" fn(u32, u32)) {
    if big != 0 {
        free(big, BIG_BYTES);
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
        let mut words = [&[][..]; MAX_WORDS];
        let count = split_words(segment, &mut words);
        let mut index = 0;
        if index < count && words[index] == b"nettest" {
            index += 1;
        }
        while index < count {
            let word = words[index];
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
                // An ordinary command takes the rest of the segment.
                let start = word.as_ptr() as usize - segment.as_ptr() as usize;
                return matches!(run_line(putc, session, &segment[start..]), Outcome::Continue);
            }
        }
    }
    true
}

/// `name[.ab|.ba]=value[:value]`.
fn apply_setting(session: &mut Session<'_>, key: &[u8], value: &[u8]) -> bool {
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
        b"loss" | b"delay" | b"rate" | b"queue" | b"seed" => {
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
            } else {
                show(putc, session);
            }
        }
        _ => print_bytes(putc, b"unknown command; type help\n"),
    }
    Outcome::Continue
}

fn print_help(putc: Putc) {
    print_bytes(
        putc,
        b"loss [ab|ba] PCT [burst N]   drop PCT% (0.1 steps); N lost in a row\n\
          delay [ab|ba] MS [JITTER]    hold frames MS ms +/- JITTER ms\n\
          rate [ab|ba] KBIT            link speed; 0 = unlimited\n\
          queue [ab|ba] N              frames allowed in flight\n\
          seed N | show | stats        repeatable randomness; settings; counters\n\
          reset [stats]                clean link, or clear the counters\n\
          quit                         stop (Escape also stops)\n",
    );
}

fn print_tenths(putc: Putc, permille: u16) {
    print_u32(putc, u32::from(permille / 10));
    print_bytes(putc, b".");
    print_u32(putc, u32::from(permille % 10));
}

fn show(putc: Putc, session: &Session<'_>) {
    for (index, link) in session.links.iter().enumerate() {
        let config = &link.config;
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
        putc(u32::from(b'\n'));
    }
}

fn print_stats(putc: Putc, session: &Session<'_>) {
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
        if stats.send_failed > 0 {
            print_bytes(putc, b"      send failures ");
            print_u32(putc, stats.send_failed);
            putc(u32::from(b'\n'));
        }
    }
}
