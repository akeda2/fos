//! `opus-bench [c=COMPLEXITY] [bitrate=BPS] [ms=FRAME] [ch=CHANNELS] [frames=N] [low]`: how long libopus takes on
//! this CPU. It encodes N frames of a synthetic signal (a few tones plus noise, so the encoder has real work to
//! do), decodes them again, and prints microseconds per frame (average and worst) for each direction, next to the
//! frame's duration, so the percentage of one core a stream would cost is obvious: encode 20 ms of stereo in X us
//! is X / 20000 of the CPU.
#![no_std]
#![no_main]

use opus_codec::{Decoder, Encoder};
use tape_rt::{TapeApi, println};

struct Lcg(u32);
impl Lcg {
    fn next(&mut self) -> i32 {
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (self.0 >> 16) as i32 - 32_768
    }
}

/// A parabola-per-half-cycle "sine" in 16 bits.
fn tone(phase: u32) -> i32 {
    let half = i64::from(phase & 0x7fff);
    let magnitude = (half * (0x8000 - half)) >> 13;
    (if phase & 0x8000 != 0 { -magnitude } else { magnitude }) as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(api: &'static TapeApi) {
    tape_rt::init(api);
    let (mut complexity, mut bitrate, mut frame_ms, mut channels, mut frames, mut low_delay) = (10i32, 96_000i32, 20usize, 2usize, 100usize, false);
    for word in core::str::from_utf8(tape_rt::args()).unwrap_or("").split_ascii_whitespace() {
        let (key, value) = word.split_once('=').unwrap_or((word, ""));
        match key {
            "c" => complexity = value.parse().unwrap_or(10),
            "bitrate" => bitrate = value.parse().unwrap_or(96_000),
            "ms" => frame_ms = value.parse().unwrap_or(20),
            "ch" => channels = value.parse().unwrap_or(2),
            "frames" => frames = value.parse().unwrap_or(100),
            "low" => low_delay = true,
            _ => return println!("opus-bench: unknown option {word}"),
        }
    }
    let frame_samples = 48 * frame_ms; // 48 kHz
    if !matches!(frame_ms, 2 | 5 | 10 | 20 | 40 | 60) || !(1..=2).contains(&channels) {
        return println!("opus-bench: ms= is 2 (2.5), 5, 10, 20, 40 or 60; ch= is 1 or 2.");
    }
    let Ok(mut encoder) = Encoder::new(48_000, channels as i32, bitrate, complexity, low_delay) else { return println!("opus-bench: cannot create the encoder") };
    let Ok(mut decoder) = Decoder::new(48_000, channels as i32) else { return println!("opus-bench: cannot create the decoder") };
    let pcm = (api.alloc)(frame_samples * channels * 2);
    let out = (api.alloc)(frame_samples * channels * 2);
    let packet = (api.alloc)(4096);
    if pcm == 0 || out == 0 || packet == 0 {
        return println!("opus-bench: out of memory");
    }
    let pcm = unsafe { core::slice::from_raw_parts_mut(pcm as *mut i16, frame_samples * channels) };
    let out = unsafe { core::slice::from_raw_parts_mut(out as *mut i16, frame_samples * channels) };
    let packet = unsafe { core::slice::from_raw_parts_mut(packet as *mut u8, 4096) };
    let (mut random, mut phase) = (Lcg(1), [0u32; 3]);
    let steps = [(440u64 << 16) / 48_000, (1_320u64 << 16) / 48_000, (3_140u64 << 16) / 48_000];
    let (mut encode_total, mut encode_worst, mut decode_total, mut decode_worst, mut bytes) = (0u64, 0u64, 0u64, 0u64, 0usize);
    println!("opus-bench: {frames} frames of {frame_ms} ms, {channels} channel(s), complexity {complexity}, {bitrate} bps{}.", if low_delay { ", low delay" } else { "" });
    for _ in 0..frames {
        for sample in 0..frame_samples {
            let mut value = (tone(phase[0]) + tone(phase[1]) / 2 + tone(phase[2]) / 4) / 3 + random.next() / 16;
            for (index, step) in steps.iter().enumerate() {
                phase[index] = (phase[index] + *step as u32) & 0xffff;
            }
            value = value.clamp(-32_000, 32_000);
            for channel in 0..channels {
                pcm[sample * channels + channel] = (value + channel as i32 * 200) as i16;
            }
        }
        let t0 = tape_rt::micros();
        let Ok(length) = encoder.encode(pcm, frame_samples, packet) else { return println!("opus-bench: encode failed") };
        let t1 = tape_rt::micros();
        let Ok(_) = decoder.decode(&packet[..length], out, frame_samples) else { return println!("opus-bench: decode failed") };
        let t2 = tape_rt::micros();
        encode_total += t1 - t0;
        encode_worst = encode_worst.max(t1 - t0);
        decode_total += t2 - t1;
        decode_worst = decode_worst.max(t2 - t1);
        bytes += length;
    }
    let frame_us = frame_ms as u64 * 1000;
    let encode_average = encode_total / frames as u64;
    let decode_average = decode_total / frames as u64;
    println!("encode: {encode_average} us per frame on average, {encode_worst} us worst  ({}.{} % of one core for a {frame_ms} ms frame)", encode_average * 100 / frame_us, encode_average * 1000 / frame_us % 10);
    println!("decode: {decode_average} us per frame on average, {decode_worst} us worst  ({}.{} % of one core)", decode_average * 100 / frame_us, decode_average * 1000 / frame_us % 10);
    println!("packets: {} bytes on average ({} kbit/s).", bytes / frames, bytes * 8 / frames * 1000 / frame_ms / 1000);
}
