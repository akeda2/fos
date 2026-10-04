//! `audio-net`: streams audio between hosts, from and to the USB audio device.
//!
//! ```text
//! tape run audio-net send [nic=N] [to=MAC] [rate=48000] [ch=3-4] [bits=24] [src=capture|tone=HZ] [secs=N]
//! tape run audio-net recv [nic=N] [from=MAC] [ch=1-2] [target=MS] [secs=N]
//! ```
//!
//! **send** takes its audio from the capture side of the attached USB audio device (`src=capture`, the
//! default) or from a test tone (`src=tone=440`, which needs no audio device at all), cuts it into packets
//! and sends them out of NIC `N` (default 1) to `to=MAC` (default: broadcast). `ch=` picks the device's
//! input channels to send (1-based, `ch=9-10,1`), `bits=` is 16 or 24. Packet time is chosen so a packet
//! fits one Ethernet frame: 1 ms for a stereo pair, shorter for many channels.
//!
//! **recv** listens on NIC `N` for an audio stream (`from=MAC` to accept only one sender), starts the USB
//! device's playback at the stream's sample rate and plays it. `ch=` says which output channels the
//! stream's channels go to (default: 1, 2, ...). The two hosts' clocks differ slightly, so the receiver
//! keeps the playback ring near `target=` milliseconds of audio (default 6) by dropping or repeating a
//! single frame when it drifts away; a lost packet is replaced by silence of the same length, so the
//! timeline stays put. `secs=` ends either side after that long; Escape always does.
//!
//! The wire format is `audio-wire`'s raw Ethernet format (ethertype 0x88ba): big-endian 16/24-bit samples,
//! sequence numbers, a frame timestamp. Later stages add RTP over UDP (24-bit linear, AES67-style
//! packets), Opus, and a resampler instead of single-frame slips.
#![no_std]
#![no_main]

use audio_wire::{ChannelSet, ETHERNET_HEADER, ETHERTYPE, Fill, HEADER_LEN, Header, MAX_RAW_PAYLOAD, Slip, frames_per_packet, pack, unpack};
use tape_rt::{AudioInfo, AudioStats, TapeApi, print, println};

const BROADCAST: [u8; 6] = [0xff; 6];
/// The kernel's playback PCM ring is this many bytes; its free space says how full it is.
const RING_BYTES: usize = 1 << 20;

struct Options<'a> {
    nic: usize,
    peer: Option<[u8; 6]>,
    rate: u32,
    channels: Option<ChannelSet>,
    bits: usize,
    tone: Option<u32>,
    target_ms: u32,
    seconds: u64,
    verbose: bool,
    rest: &'a str,
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
    let mut options = Options { nic: 0, peer: None, rate: 48_000, channels: None, bits: 24, tone: None, target_ms: 6, seconds: 0, verbose: false, rest: "" };
    for word in words {
        let (key, value) = word.split_once('=').unwrap_or((word, ""));
        match key {
            "nic" => options.nic = value.parse::<usize>().ok().filter(|&n| n >= 1).ok_or("nic=N needs a number from 1")? - 1,
            "to" | "from" => options.peer = Some(parse_mac(value).ok_or("a MAC address looks like 00:11:22:33:44:55")?),
            "rate" => options.rate = value.parse().map_err(|_| "rate= needs a number")?,
            "ch" => options.channels = Some(ChannelSet::parse(value).ok_or("ch= is like 1-2 or 9-10,1 (channels 1 to 32)")?),
            "bits" => options.bits = value.parse().ok().filter(|&b| b == 16 || b == 24).ok_or("bits= is 16 or 24")?,
            "src" => match value.strip_prefix("tone=") {
                Some(frequency) => options.tone = Some(frequency.parse().map_err(|_| "src=tone=HZ needs a number")?),
                None if value == "capture" => {}
                None => return Err("src= is capture or tone=HZ"),
            },
            "target" => options.target_ms = value.parse().map_err(|_| "target= needs milliseconds")?,
            "verbose" => options.verbose = true,
            "secs" => options.seconds = value.parse().map_err(|_| "secs= needs a number")?,
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

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(api: &'static TapeApi) {
    tape_rt::init(api);
    let arguments = core::str::from_utf8(tape_rt::args()).unwrap_or("");
    let mut words = arguments.split_ascii_whitespace();
    let mode = words.next().unwrap_or("");
    let options = match parse(words) {
        Ok(options) => options,
        Err(reason) => return println!("audio-net: {reason}. Usage: audio-net send|recv [nic=N] [to=MAC] [rate=R] [ch=A-B] [bits=16|24] [src=capture|tone=HZ] [target=MS] [secs=N]"),
    };
    let _ = options.rest;
    if (api.nic_count)() <= options.nic {
        return println!("audio-net: this machine has no NIC {}.", options.nic + 1);
    }
    match mode {
        "send" => send(api, &options),
        "recv" => receive(api, &options),
        _ => println!("Usage: tape run audio-net send|recv [options]   (see the tape's header for the options)"),
    }
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
    let per_packet = frames_per_packet(options.rate, channels, bits, MAX_RAW_PAYLOAD);
    let frame_bytes = channels * bits / 8;
    let device_frame = device_channels * slot;
    let mut mac = [0u8; 6];
    (api.nic_mac)(options.nic, mac.as_mut_ptr());
    let destination = options.peer.unwrap_or(BROADCAST);
    if tone.is_none() {
        let status = (api.audio_start)(options.rate, 1, 0);
        if status != 0 {
            return println!("audio-net send: cannot start capture at {} Hz (code {status}); the device offers {:?}.", options.rate, &info.rates[..info.rate_count as usize]);
        }
    }
    println!(
        "Sending {channels} channel(s), {bits}-bit, {} Hz, {per_packet} frames ({} us) per packet, from {} to {}.",
        options.rate,
        u64::from(per_packet as u32) * 1_000_000 / u64::from(options.rate),
        if tone.is_some() { "a test tone" } else { "the capture device" },
        if options.peer.is_some() { "one peer" } else { "everyone (broadcast)" }
    );
    // The stream id: anything that differs between runs (the time).
    let stream = (tape_rt::micros() as u32) ^ 0x5a5a_a5a5;
    let mut frame = [0u8; 1514];
    let mut capture = [0u8; 8192];
    let mut held = 0usize; // bytes of device-format audio waiting in `capture`
    let (mut sequence, mut timestamp, mut sent, mut phase) = (0u16, 0u32, 0u64, 0u32);
    let started = tape_rt::micros();
    let mut next_report = started + 1_000_000;
    let packet_us = u64::from(per_packet as u32) * 1_000_000 / u64::from(options.rate);
    loop {
        if (api.should_abort)() != 0 {
            break;
        }
        if options.seconds != 0 && tape_rt::micros() >= started + options.seconds * 1_000_000 {
            break;
        }
        let have_packet = if let Some(frequency) = tone {
            // Paced by the clock: packet number `sent` is due at started + sent * packet time.
            if tape_rt::micros() >= started + sent * packet_us {
                for index in 0..per_packet {
                    let sample = tone_sample(phase) << 8; // 16-bit tone as a 24-bit value
                    phase = (phase + (u64::from(frequency) * 65_536 / u64::from(options.rate)) as u32) & 0xffff;
                    for channel in 0..channels {
                        let at = HEADER_LEN + ETHERNET_HEADER + (index * channels + channel) * bits / 8;
                        let bytes = sample.to_be_bytes(); // [x, high, mid, low] of the 24-bit value in a 32-bit int
                        let wire = &bytes[1..];
                        frame[at..at + bits / 8].copy_from_slice(if bits == 24 { wire } else { &wire[..2] });
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
                let written = pack(&capture[..per_packet * device_frame], slot, device_channels, per_packet, &selection, bits, &mut frame[ETHERNET_HEADER + HEADER_LEN..]);
                debug_assert_eq!(written, per_packet * frame_bytes);
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
        frame[0..6].copy_from_slice(&destination);
        frame[6..12].copy_from_slice(&mac);
        frame[12..14].copy_from_slice(&ETHERTYPE.to_be_bytes());
        Header { first: sequence == 0 && sent == 0, sequence, timestamp, stream, channels: channels as u8, bits: bits as u8, frames: per_packet as u16, rate: options.rate }.encode(&mut frame[ETHERNET_HEADER..]);
        let length = ETHERNET_HEADER + HEADER_LEN + per_packet * frame_bytes;
        (api.nic_send)(options.nic, frame.as_ptr(), length);
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

fn receive(api: &'static TapeApi, options: &Options) {
    let mut info = AudioInfo::default();
    if (api.audio_info)(&mut info) == 0 || info.playback_channels == 0 {
        return println!("audio-net recv: no USB audio device with outputs.");
    }
    let (device_channels, slot) = (info.playback_channels as usize, info.playback_slot_bytes as usize);
    let device_frame = device_channels * slot;
    // Start playback now, at `rate=` (default 48000): starting a USB stream takes a while (control requests,
    // Configure Endpoint), and doing it when the first packet arrives made the frames that came meanwhile pile
    // up and overflow. A stream at another rate restarts it, once.
    let mut running_rate = options.rate;
    let status = (api.audio_start)(running_rate, 0, 1);
    if status != 0 {
        return println!("audio-net recv: cannot start playback at {running_rate} Hz (code {status}); the device offers {:?}.", &info.rates[..info.rate_count as usize]);
    }
    println!("Listening on NIC {} for an audio stream; playing on {device_channels} output channel(s) of {}-bit. Escape stops.", options.nic + 1, info.playback_bits);
    let mut frame = [0u8; 1514];
    let mut staging = [0u8; 8192];
    let mut locked: Option<(u32, [u8; 6])> = None;
    let (mut expected, mut running) = (0u16, true);
    let (mut packets, mut lost, mut dropped, mut repeated, mut foreign) = (0u64, 0u64, 0u64, 0u64, 0u64);
    let mut fill = Fill::new(0);
    let mut placement = [None; audio_wire::MAX_CHANNELS];
    let mut wire_channels = 0usize;
    let mut last_packet = tape_rt::micros();
    let started = tape_rt::micros();
    let mut next_report = started + 1_000_000;
    loop {
        if (api.should_abort)() != 0 {
            break;
        }
        if options.seconds != 0 && tape_rt::micros() >= started + options.seconds * 1_000_000 {
            break;
        }
        let length = (api.nic_receive)(options.nic, frame.as_mut_ptr(), frame.len());
        if length == 0 {
            if locked.is_some() && tape_rt::micros() > last_packet + 2_000_000 {
                println!("\nThe sender went quiet; waiting for a stream again.");
                locked = None; // playback keeps running (silence), ready for the next stream
            }
            tape_rt::idle();
            continue;
        }
        if length < ETHERNET_HEADER + HEADER_LEN || frame[12..14] != ETHERTYPE.to_be_bytes() {
            continue; // not ours: ARP, a term session, ...
        }
        let Some(header) = Header::parse(&frame[ETHERNET_HEADER..length]) else { continue };
        let sender: [u8; 6] = frame[6..12].try_into().unwrap();
        if options.peer.is_some_and(|wanted| wanted != sender) {
            continue;
        }
        match locked {
            None => {
                locked = Some((header.stream, sender));
                expected = header.sequence;
                wire_channels = usize::from(header.channels);
                // Where the stream's channels go on the device: ch= if given, else 1, 2, 3...
                let targets = options.channels.unwrap_or(ChannelSet((1u64 << wire_channels.min(device_channels)).wrapping_sub(1) as u32));
                for (index, device_channel) in targets.channels().enumerate().take(wire_channels) {
                    placement[index] = Some(device_channel);
                }
                if header.rate != running_rate {
                    (api.audio_stop)();
                    let status = (api.audio_start)(header.rate, 0, 1);
                    if status != 0 {
                        println!("audio-net recv: cannot start playback at {} Hz (code {status}); the device offers {:?}.", header.rate, &info.rates[..info.rate_count as usize]);
                        running = false;
                        locked = None;
                        continue;
                    }
                    running_rate = header.rate;
                }
                running = true;
                let target_frames = (header.rate as usize * options.target_ms as usize / 1000).max(1);
                fill = Fill::new(target_frames);
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
                println!("Locked onto a stream: {} channel(s), {}-bit, {} Hz, from {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}.", header.channels, header.bits, header.rate, sender[0], sender[1], sender[2], sender[3], sender[4], sender[5]);
            }
            Some((stream, from)) if stream != header.stream || from != sender => {
                foreign += 1;
                continue; // a second sender: first one wins
            }
            _ => {}
        }
        last_packet = tape_rt::micros();
        // Sequence: a gap is lost packets; fill their time with silence so the audio stays aligned.
        let gap = header.sequence.wrapping_sub(expected);
        if gap != 0 && gap < 0x8000 {
            lost += u64::from(gap);
            if options.verbose {
                println!(" [gap of {gap} before sequence {} at packet {packets}]", header.sequence);
            }
            // A short gap (a few lost packets) is bridged with silence of the same length so the timeline
            // stays put; a long one (the receiver was busy, a cable was pulled) is a resync: no silence.
            if gap <= 8 {
                let mut remaining = (usize::from(gap) * usize::from(header.frames)).min(2_048) * device_frame;
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
        expected = header.sequence.wrapping_add(1);
        // Convert, with at most one frame dropped or repeated if the ring level has drifted.
        let frames = usize::from(header.frames).min(staging.len() / device_frame - 1);
        let level = (RING_BYTES - {
            (api.audio_info)(&mut info);
            info.playback_space as usize
        }) / device_frame;
        let payload = &frame[ETHERNET_HEADER + HEADER_LEN..length];
        if level > fill.target() * 4 + 64 {
            dropped += frames as u64; // far too full (after a stall): discard whole packets to catch up quickly
            packets += 1;
            continue;
        }
        let mut out_frames = frames;
        let skip = match fill.advise(level) {
            Slip::Drop => {
                dropped += 1;
                1
            }
            _ => 0,
        };
        let wire_skip = skip * wire_channels * usize::from(header.bits / 8);
        out_frames -= skip.min(out_frames);
        unpack(&payload[wire_skip..], wire_channels, usize::from(header.bits), out_frames, &placement[..wire_channels], slot, device_channels, &mut staging);
        if fill.advise(level) == Slip::Repeat && out_frames > 0 {
            repeated += 1;
            staging.copy_within((out_frames - 1) * device_frame..out_frames * device_frame, out_frames * device_frame);
            out_frames += 1;
        }
        let bytes = out_frames * device_frame;
        let written = (api.audio_write)(staging.as_ptr(), bytes);
        if written < bytes {
            // The ring is full (the sender runs fast, or the device stalled): the rest is dropped.
            dropped += 1;
        }
        packets += 1;
        if tape_rt::micros() >= next_report {
            next_report += 1_000_000;
            print!(".");
            if options.verbose {
                println!(" [{packets} packets, {lost} lost, level {level} frames (target {}), {dropped} dropped, {repeated} repeated]", fill.target());
            }
        }
    }
    let mut stats = AudioStats::default();
    (api.audio_stats)(&mut stats);
    if running {
        (api.audio_stop)();
    }
    println!();
    println!("Received {packets} packets: {lost} lost, {dropped} frame(s) dropped and {repeated} repeated for clock drift, {foreign} from another sender; playback {} packets, {} underruns, {} transfer errors.", stats.playback_packets, stats.playback_underruns, stats.playback_errors);
}
