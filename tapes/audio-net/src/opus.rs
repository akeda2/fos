//! `codec=opus`: Opus over RTP (RFC 7587), one stream per stereo pair.
//!
//! The channel set is cut into pairs (an odd last channel is a mono stream); stream `i` goes to UDP port
//! `port + 2 i` with its own SSRC, `opus/48000/2` and the same RTP timestamp as the others, so a receiver can line
//! the pairs up again by timestamp. The receiver decodes each stream with libopus, conceals a lost packet with
//! libopus's own loss concealment, puts every decoded frame on a shared timeline at its RTP timestamp, and feeds
//! the frames every stream has reached through the same resampler and servo as PCM. 48 kHz only.

use crate::net::{self, Inbound};
use crate::{Options, Transport, tone_sample};
use audio_wire::{ChannelSet, MAX_CHANNELS, Resampler, Servo, SdpOpus, device_sample_16, frames_to_device, write_sdp_opus};
use opus_codec::{Decoder, Encoder, packet_samples};
use tape_net::RtpPacket;
use tape_rt::{AudioInfo, AudioStats, TapeApi, print, println};

const RATE: u32 = 48_000;
/// Longest Opus frame handled (60 ms at 48 kHz).
const MAX_FRAME: usize = 2880;
/// Most streams (pairs) in one session.
const MAX_STREAMS: usize = MAX_CHANNELS / 2;
/// Timeline length in frames per channel: a power of two, well over the worst skew between streams.
const TIMELINE: usize = 8192;
const RING_BYTES: usize = 1 << 20;

/// Decoded audio of every stream on one timeline, indexed by RTP timestamp modulo `TIMELINE`.
static mut TIMELINE_DATA: [i32; TIMELINE * MAX_CHANNELS] = [0; TIMELINE * MAX_CHANNELS];

/// Channels carried by stream `index` of `channels` channels: 2, or 1 for an odd last channel.
fn stream_channels(index: usize, channels: usize) -> usize {
    if index == channels / 2 { 1 } else { 2 }
}

pub fn send(api: &'static TapeApi, options: &Options) {
    if options.in_mode != 0 {
        (api.audio_select)(0, options.in_mode);
    }
    let mut info = AudioInfo::default();
    let device = (api.audio_info)(&mut info) != 0 && info.capture_channels != 0;
    if options.tone.is_none() && !device {
        return println!("audio-net send: no audio device with inputs (use src=tone=440 to send a test tone).");
    }
    if options.rate != RATE {
        return println!("audio-net send: codec=opus is 48000 Hz only.");
    }
    let (mut device_channels, mut slot) = (info.capture_channels as usize, info.capture_slot_bytes as usize);
    let selection = options.channels.unwrap_or(ChannelSet(0b11));
    let channels = selection.count();
    if options.tone.is_none() && selection.channels().any(|channel| channel >= device_channels) {
        return println!("audio-net send: the device has {device_channels} input channels; ch= asks for more.");
    }
    let streams = channels.div_ceil(2);
    let frame_samples = options.frame_x10 as usize * 48 / 10;
    if !matches!(options.frame_x10, 25 | 50 | 100 | 200 | 400 | 600) {
        return println!("audio-net send: frame= is 2.5, 5, 10, 20, 40 or 60 (milliseconds).");
    }
    let Some(mut transport) = Transport::open(api, options) else { return };
    let (host, _) = options.to.split_once(':').unwrap_or((options.to, ""));
    let Some(ip) = net::parse_ip(host) else { return println!("audio-net send: codec=opus needs proto=rtp and to=IP.") };
    let Some(mac) = net::resolve(api, options.nic, &transport.config, &mut transport.arp, ip) else {
        return println!("audio-net send: no answer from {host} (ARP); is it on this subnet, or is gw= set?");
    };
    println!("Sending to {host}, MAC {}.", crate::MacText(mac.0));
    let ttl = if audio_wire::is_multicast_address(ip.0) { 32 } else { 64 };
    let mut encoders: [Option<Encoder>; MAX_STREAMS] = [const { None }; MAX_STREAMS];
    for (index, encoder) in encoders.iter_mut().enumerate().take(streams) {
        match Encoder::new(RATE as i32, stream_channels(index, channels) as i32, options.bitrate as i32 * stream_channels(index, channels) as i32 / 2, options.complexity as i32, options.frame_x10 < 200) {
            Ok(created) => *encoder = Some(created),
            Err(error) => return println!("audio-net send: cannot create an Opus encoder: {error:?}"),
        }
    }
    if options.tone.is_none() {
        let status = (api.audio_start)(RATE, 1, 0);
        if status != 0 {
            return println!("audio-net send: cannot start capture at 48000 Hz (code {status}); the device offers {:?}.", &info.rates[..info.rate_count as usize]);
        }
        (api.audio_info)(&mut info);
        (device_channels, slot) = (info.capture_channels as usize, info.capture_slot_bytes as usize);
        if selection.channels().any(|channel| channel >= device_channels) {
            (api.audio_stop)();
            return println!("audio-net send: the capture mode at 48000 Hz has {device_channels} channel(s); ch= asks for more (`audio modes` lists the modes, inmode= picks one).");
        }
    }
    crate::stats::set_send(format_args!("Opus {} kbit/s per pair, {} ch{}", options.bitrate / 1000, channels, if options.adopted_send { " (peer's choice)" } else { "" }));
    println!("Sending {channels} channel(s) as {streams} Opus stream(s), {} kbit/s per pair, {}.{} ms frames, to {host}, ports {}..{}.", options.bitrate / 1000, options.frame_x10 / 10, options.frame_x10 % 10, options.port, options.port as usize + 2 * (streams - 1));
    let session = (tape_rt::micros() as u32) ^ 0x5a5a_a5a5;
    let mut text = [0u8; 900];
    let mut name_buffer = [0u8; audio_wire::sap::MAX_NAME];
    let name = crate::peers::session_name(options.name, transport.mac, &mut name_buffer);
    let length = write_sdp_opus(&SdpOpus { name, receive_only: false, idle: false, origin: transport.config.ip.0, destination: ip.0, port: options.port, payload_type: options.payload_type, channels, packet_ms_x10: options.frame_x10, session }, &mut text);
    println!("SDP for the receiver (save as stream.sdp; e.g. ffplay -protocol_whitelist file,udp,rtp stream.sdp):");
    println!("{}", core::str::from_utf8(&text[..length]).unwrap_or(""));
    transport.announcer_start(&text[..length]);
    println!("Announcing this stream as '{name}' (SAP, every 5 s).");
    crate::stats::get().rate = RATE;
    let device_frame = device_channels * slot;
    let capture_bytes = device_frame * frame_samples; // 0 for a tone: no device
    let capture_address = if capture_bytes == 0 { 1 } else { (api.alloc)(capture_bytes * 2) };
    if capture_address == 0 {
        return println!("audio-net send: out of memory.");
    }
    // Capture is read in whatever sizes the ring offers; whole Opus frames are cut from the front.
    let capture = unsafe { core::slice::from_raw_parts_mut(if capture_bytes == 0 { core::ptr::NonNull::dangling().as_ptr() } else { capture_address as *mut u8 }, capture_bytes * 2) };
    let mut held = 0usize;
    crate::duplex::go();
    let mut pcm = [0i16; MAX_FRAME * 2];
    let mut packet = [0u8; 1275];
    let mut frame = [0u8; 1514];
    let channel_list: [usize; MAX_CHANNELS] = {
        let mut list = [0; MAX_CHANNELS];
        for (index, channel) in selection.channels().enumerate() {
            list[index] = channel;
        }
        list
    };
    let (mut sequence, mut timestamp, mut sent, mut phase) = (0u16, 0x1000u32, 0u64, 0u32);
    let frame_us = frame_samples as u64 * 1_000_000 / u64::from(RATE);
    let paced_x1000 = (frame_us as i64 * 1000 * (1_000_000 - options.skew_ppm) / 1_000_000) as u64;
    let started = tape_rt::micros();
    let mut next_report = started + 1_000_000;
    let (mut bytes_total, mut encode_total, mut encode_max) = (0u64, 0u64, 0u64);
    loop {
        if (api.should_abort)() != 0 || (options.seconds != 0 && tape_rt::micros() >= started + options.seconds * 1_000_000) {
            break;
        }
        transport.announce_service(api, options.nic);
        let ready = if options.tone.is_some() {
            tape_rt::micros() >= started + sent * paced_x1000 / 1000
        } else {
            let got = (api.audio_read)(capture[held..].as_mut_ptr(), capture.len() - held);
            held += got;
            held >= capture_bytes
        };
        if !ready {
            tape_rt::idle();
            continue;
        }
        let began = tape_rt::micros();
        // Tone: one sample stream for every channel; capture: the selected device channels of this frame.
        let tone_step = options.tone.map(|frequency| (u64::from(frequency) * 65_536 / u64::from(RATE)) as u32);
        let mut tone_block = [0i16; MAX_FRAME];
        if let Some(step) = tone_step {
            for sample in tone_block.iter_mut().take(frame_samples) {
                *sample = (tone_sample(phase)) as i16;
                phase = (phase + step) & 0xffff;
            }
        }
        for stream in 0..streams {
            let width = stream_channels(stream, channels);
            for index in 0..frame_samples {
                for lane in 0..width {
                    pcm[index * width + lane] = if tone_step.is_some() {
                        tone_block[index]
                    } else {
                        let at = index * device_frame + channel_list[stream * 2 + lane] * slot;
                        device_sample_16(&capture[at..at + slot])
                    };
                }
            }
            crate::stats::peaks_from_i16(&pcm[..frame_samples * width], width, frame_samples, stream * 2, true);
            let Some(encoder) = encoders[stream].as_mut() else { continue };
            let Ok(length) = encoder.encode(&pcm[..frame_samples * width], frame_samples, &mut packet) else { continue };
            let port = options.port + 2 * stream as u16;
            let rtp = RtpPacket::new(options.payload_type, sent == 0, sequence, timestamp, session ^ stream as u32, &packet[..length]);
            if let Some(total) = net::build_rtp_frame(&mut frame, &transport.config, mac, ip, port, ttl, &rtp) {
                (api.nic_send)(options.nic, frame.as_ptr(), total);
            }
            bytes_total += length as u64;
        }
        if options.tone.is_none() {
            capture.copy_within(capture_bytes..held, 0);
            held -= capture_bytes;
        }
        let spent = tape_rt::micros() - began;
        encode_total += spent;
        encode_max = encode_max.max(spent);
        sequence = sequence.wrapping_add(1);
        timestamp = timestamp.wrapping_add(frame_samples as u32);
        sent += 1;
        crate::stats::get().sent = sent;
        if tape_rt::micros() >= next_report {
            next_report += 1_000_000;
            print!(".");
        }
    }
    if options.tone.is_none() {
        (api.audio_stop)();
    }
    if capture_bytes != 0 {
        (api.free)(capture_address, capture_bytes * 2);
    }
    println!();
    transport.announce_stop(api, options.nic);
    println!("Sent {sent} frames x {streams} stream(s); {} bytes of Opus ({} kbit/s); encoding took {} us per frame on average, {} us at worst.", bytes_total, bytes_total * 8 * 1000 / (sent.max(1) * frame_us) , encode_total / sent.max(1), encode_max);
}

/// One received stream's state.
struct Lane {
    decoder: Decoder,
    channels: usize,
    ssrc: Option<u32>,
    /// RTP timestamp of the next frame this stream owes the timeline.
    next: u32,
    last_frame: usize,
    started: bool,
}

pub fn receive(api: &'static TapeApi, options: &Options) {
    if options.out_mode != 0 {
        (api.audio_select)(1, options.out_mode);
    }
    let mut info = AudioInfo::default();
    if (api.audio_info)(&mut info) == 0 || info.playback_channels == 0 {
        return println!("audio-net recv: no audio device with outputs.");
    }
    if options.rate != RATE {
        return println!("audio-net recv: codec=opus is 48000 Hz only.");
    }
    let (device_channels, slot) = (info.playback_channels as usize, info.playback_slot_bytes as usize);
    let device_frame = device_channels * slot;
    let channels = options.chan;
    let streams = channels.div_ceil(2);
    let Some(mut transport) = Transport::open(api, options) else { return };
    if !transport.join_group(api, options) {
        return;
    }
    let mut lanes: [Option<Lane>; MAX_STREAMS] = [const { None }; MAX_STREAMS];
    for (index, lane) in lanes.iter_mut().enumerate().take(streams) {
        let width = stream_channels(index, channels);
        match Decoder::new(RATE as i32, width as i32) {
            Ok(decoder) => *lane = Some(Lane { decoder, channels: width, ssrc: None, next: 0, last_frame: 960, started: false }),
            Err(error) => return println!("audio-net recv: cannot create an Opus decoder: {error:?}"),
        }
    }
    let status = (api.audio_start)(RATE, 0, 1);
    if status != 0 {
        return println!("audio-net recv: cannot start playback at 48000 Hz (code {status}); the device offers {:?}.", &info.rates[..info.rate_count as usize]);
    }
    (api.audio_info)(&mut info);
    let (device_channels, slot) = (info.playback_channels as usize, info.playback_slot_bytes as usize);
    let device_frame = device_channels * slot;
    let targets = options.channels.unwrap_or(ChannelSet((1u64 << channels.min(device_channels)).wrapping_sub(1) as u32));
    let mut placement = [None; MAX_CHANNELS];
    for (index, device_channel) in targets.channels().enumerate().take(channels) {
        placement[index] = Some(device_channel);
    }
    crate::stats::set_receive(format_args!("Opus, {} ch{}", channels, if options.adopted_receive { " (peer's choice)" } else { "" }));
    transport.announce_receiver(options);
    println!("Listening on NIC {} for Opus/RTP: {streams} stream(s) on ports {}..{}, {channels} channel(s). Escape stops.", options.nic + 1, options.port, options.port as usize + 2 * (streams - 1));
    let timeline = unsafe { &mut *core::ptr::addr_of_mut!(TIMELINE_DATA) };
    timeline.fill(0);
    let target_frames = (RATE as usize * options.target_ms as usize / 1000).max(1);
    let mut servo = Servo::new(target_frames);
    let mut resampler = Resampler::new(channels);
    let silence = [0u8; 1024];
    let mut needed = target_frames * device_frame;
    while needed > 0 {
        let written = (api.audio_write)(silence.as_ptr(), needed.min(silence.len()) / device_frame * device_frame);
        if written == 0 {
            break;
        }
        needed -= written.min(needed);
    }
    let mut frame = [0u8; 1514];
    let mut pcm = [0i16; MAX_FRAME * 2];
    let mut work = [0i32; 2048 * 2];
    let mut resampled = [0i32; 4096];
    let mut staging = [0u8; 8192];
    // `play` is the timeline position (RTP timestamp) of the next frame to play; set by the first packet.
    let mut play: Option<u32> = None;
    let mut locked_source: Option<[u8; 4]> = None;
    let (mut packets, mut lost, mut concealed, mut dropped, mut bad) = (0u64, 0u64, 0u64, 0u64, 0u64);
    let mut last_packet = tape_rt::micros();
    let started = tape_rt::micros();
    let mut next_report = started + 1_000_000;
    let mut level;
    let mut trimming = false;
    loop {
        if (api.should_abort)() != 0 || (options.seconds != 0 && tape_rt::micros() >= started + options.seconds * 1_000_000) {
            break;
        }
        transport.igmp_service(api, options.nic, None);
        let length = (api.nic_receive)(options.nic, frame.as_mut_ptr(), frame.len());
        if length == 0 {
            if play.is_some() && tape_rt::micros() > last_packet + 2_000_000 {
                println!("\nThe sender went quiet; waiting for a stream again.");
                play = None;
                locked_source = None;
                for lane in lanes.iter_mut().flatten() {
                    lane.ssrc = None;
                    lane.started = false;
                }
            }
            tape_rt::idle();
            continue;
        }
        if transport.igmp_service(api, options.nic, Some(&frame[..length])) {
            continue;
        }
        let now = (api.ticks)() as u32;
        let last_port = options.port + 2 * (streams as u16 - 1);
        let (source, port, payload) = match net::inbound(api, options.nic, &transport.config, &mut transport.arp, &frame[..length], options.port..=last_port, now) {
            Inbound::Udp { source, destination_port, payload, .. } => (source, destination_port, payload),
            Inbound::Rejected(reason) => {
                if options.verbose {
                    println!(" [dropped a {length}-byte frame: {reason}]");
                }
                continue;
            }
            _ => continue,
        };
        if (port - options.port) % 2 != 0 {
            continue;
        }
        let index = usize::from((port - options.port) / 2);
        if locked_source.is_some_and(|locked| locked != source.0) {
            continue;
        }
        let Ok(rtp) = RtpPacket::parse(payload) else { continue };
        let Some(lane) = lanes[index].as_mut() else { continue };
        if lane.ssrc.is_some_and(|ssrc| ssrc != rtp.ssrc) {
            continue;
        }
        let Some(samples) = packet_samples(rtp.payload).filter(|&samples| samples <= MAX_FRAME) else {
            bad += 1;
            continue;
        };
        if locked_source.is_none() {
            if crate::stats::get().underrun_base == 0 {
                let mut device = AudioStats::default();
                (api.audio_stats)(&mut device);
                crate::stats::get().underrun_base = device.playback_underruns;
            }
            println!("Receiving Opus from {}.{}.{}.{}.", source.0[0], source.0[1], source.0[2], source.0[3]);
        }
        locked_source = Some(source.0);
        last_packet = tape_rt::micros();
        if lane.ssrc.is_none() {
            lane.ssrc = Some(rtp.ssrc);
            lane.started = false;
        }
        let first_of_lane = !lane.started;
        if first_of_lane {
            lane.started = true;
            lane.next = rtp.timestamp;
        }
        // The first packet of the session anchors the timeline.
        play.get_or_insert(rtp.timestamp);
        let lane_channels = lane.channels;
        let first_channel = index * 2;
        let behind = rtp.timestamp.wrapping_sub(lane.next) as i32;
        // A short gap: conceal it, a frame at a time, so this stream stays aligned with the others. A long one
        // resynchronises (the timeline simply has silence there). An old packet is ignored.
        if behind < 0 {
            continue;
        }
        if behind > 0 {
            let missing = behind as usize / lane.last_frame.max(1);
            lost += missing as u64;
            if behind as usize <= 8 * lane.last_frame {
                let mut remaining = behind as usize;
                while remaining > 0 {
                    let piece = remaining.min(lane.last_frame).min(MAX_FRAME);
                    if lane.decoder.conceal(&mut pcm, piece).is_ok() {
                        place(timeline, lane.next, first_channel, lane_channels, &pcm[..piece * lane_channels], piece);
                        concealed += 1;
                    }
                    lane.next = lane.next.wrapping_add(piece as u32);
                    remaining -= piece;
                }
            } else {
                lane.next = rtp.timestamp;
            }
        }
        match lane.decoder.decode(rtp.payload, &mut pcm, samples) {
            Ok(decoded) => {
                place(timeline, rtp.timestamp, first_channel, lane_channels, &pcm[..decoded * lane_channels], decoded);
                crate::stats::peaks_from_i16(&pcm[..decoded * lane_channels], lane_channels, decoded, first_channel, false);
                lane.next = rtp.timestamp.wrapping_add(decoded as u32);
                lane.last_frame = decoded;
            }
            Err(_) => {
                bad += 1;
                continue;
            }
        }
        packets += 1;
        // Frames every stream has reached are ready to play. A stream that lags far behind the others (it was
        // lost, or has not started) is padded with silence rather than holding everyone back.
        let (mut lowest, mut highest) = (None::<u32>, None::<u32>);
        for lane in lanes.iter().flatten().filter(|lane| lane.started) {
            let ahead = |a: u32, b: u32| (a.wrapping_sub(b) as i32) > 0;
            lowest = Some(lowest.map_or(lane.next, |low| if ahead(low, lane.next) { lane.next } else { low }));
            highest = Some(highest.map_or(lane.next, |high| if ahead(lane.next, high) { lane.next } else { high }));
        }
        let (Some(low), Some(high)) = (lowest, highest) else { continue };
        let mut ready_until = low;
        if streams > 1 && lanes.iter().flatten().any(|lane| !lane.started) || high.wrapping_sub(low) as i32 > 5_760 {
            ready_until = high.wrapping_sub(2_880); // give up waiting for the laggard
        }
        let Some(position) = play.as_mut() else { continue };
        if first_of_lane && packets == 1 {
            *position = rtp.timestamp;
        }
        let available = ready_until.wrapping_sub(*position) as i32;
        if available <= 0 {
            continue;
        }
        // Everything the timeline holds from `position` is the frames to play.
        let mut frames = available as usize;
        if frames > TIMELINE / 2 {
            *position = ready_until.wrapping_sub(TIMELINE as u32 / 4);
            frames = TIMELINE / 4;
        }
        (api.audio_info)(&mut info);
        level = (RING_BYTES - info.playback_space as usize) / device_frame;
        // Far too full (a stall, or a sender that starts with a burst): the ring cannot be emptied, so discard
        // incoming audio, and keep discarding until the ring has played down to the target, instead of leaving
        // the rest to the servo (which is limited to +-2000 ppm and would take tens of seconds).
        if level > target_frames * 4 + 64 {
            trimming = true;
        } else if level <= target_frames {
            trimming = false;
        }
        if trimming {
            dropped += frames as u64;
            for step in 0..frames {
                let at = (position.wrapping_add(step as u32) as usize % TIMELINE) * MAX_CHANNELS;
                timeline[at..at + MAX_CHANNELS].fill(0);
            }
            *position = position.wrapping_add(frames as u32);
            continue;
        }
        resampler.step = servo.update(level, frames, RATE);
        let shown = crate::stats::get();
        (shown.received, shown.lost, shown.level_frames, shown.target_frames, shown.offset_ppm, shown.rate) = (packets, lost, level as u32, target_frames as u32, servo.offset_ppm(), RATE);
        shown.packet_frames = frames as u32;
        let mut remaining = frames;
        while remaining > 0 {
            let chunk = remaining.min(work.len() / channels);
            for step in 0..chunk {
                let at = (position.wrapping_add(step as u32) as usize % TIMELINE) * MAX_CHANNELS;
                work[step * channels..(step + 1) * channels].copy_from_slice(&timeline[at..at + channels]);
                timeline[at..at + MAX_CHANNELS].fill(0);
            }
            *position = position.wrapping_add(chunk as u32);
            remaining -= chunk;
            let mut produced = 0;
            let mut out_frame = [0i32; MAX_CHANNELS];
            for step in 0..chunk {
                resampler.push(&work[step * channels..(step + 1) * channels]);
                while resampler.next_output(&mut out_frame) && (produced + 1) * channels <= resampled.len() && (produced + 1) * device_frame <= staging.len() {
                    resampled[produced * channels..(produced + 1) * channels].copy_from_slice(&out_frame[..channels]);
                    produced += 1;
                }
            }
            frames_to_device(&resampled, channels, produced, &placement[..channels], slot, device_channels, &mut staging);
            let bytes = produced * device_frame;
            if (api.audio_write)(staging.as_ptr(), bytes) < bytes {
                dropped += 1;
            }
        }
        if tape_rt::micros() >= next_report {
            next_report += 1_000_000;
            print!(".");
            if options.verbose && (next_report - started) / 1_000_000 % 10 == 0 {
                println!(" [{packets} packets, {lost} lost ({concealed} concealed), level {} frames (target {target_frames}), clock offset {} ppm]", (RING_BYTES - info.playback_space as usize) / device_frame, servo.offset_ppm());
            }
        }
    }
    transport.leave_group(api, options.nic);
    let mut stats = AudioStats::default();
    (api.audio_stats)(&mut stats);
    (api.audio_stop)();
    println!();
    println!("Delay sender capture to playback ring: about {} ms (one frame plus {} ms queued); the device adds its own buffer.", crate::stats::get().delay_ms(), crate::stats::get().level_frames * 1000 / RATE);
    println!("Received {packets} Opus packets: {lost} lost ({concealed} concealed), {dropped} frame(s) discarded, {bad} undecodable, clock offset learned {} ppm; playback {} packets, {} underruns, {} transfer errors.", servo.offset_ppm(), stats.playback_packets, stats.playback_underruns.saturating_sub(crate::stats::get().underrun_base), stats.playback_errors);
}

/// Writes `frames` decoded frames of `width` channels (16-bit, interleaved) into the timeline at RTP timestamp
/// `at`, on channels `first..first + width`, as the 32-bit values the resampler works in.
fn place(timeline: &mut [i32], at: u32, first: usize, width: usize, pcm: &[i16], frames: usize) {
    for index in 0..frames {
        let base = (at.wrapping_add(index as u32) as usize % TIMELINE) * MAX_CHANNELS + first;
        for lane in 0..width {
            timeline[base + lane] = i32::from(pcm[index * width + lane]) << 16;
        }
    }
}
