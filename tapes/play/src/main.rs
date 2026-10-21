//! `play NAME`: plays a WAV file on the audio device's outputs (USB audio or HD Audio).
//!
//! ```text
//! tape run play tone.wav
//! tape run play device=2 tone.wav
//! ```
//!
//! `device=N` picks the audio device (numbered as the `audio` command lists them) for this run only; without it the
//! operator's choice, or the automatic one, is used. It needs a kernel with tape API version 3. Every audio tape takes it.
//!
//! Reads PCM WAV files of 8, 16, 24 or 32 bits. The file's sample rate must be one the device
//! offers; its sample width and channel count are converted to the device's playback format (a
//! mono file is copied to every output channel, extra file channels are dropped, missing ones are
//! silent). Escape stops. The whole file is read into RAM first (up to 64 MiB).
#![no_std]
#![no_main]

use tape_rt::{AudioInfo, AudioStats, TapeApi, FS_TOO_BIG, println};

const MAX_BYTES: usize = 64 << 20;

struct Wav {
    channels: usize,
    rate: u32,
    bits: usize,
    /// IEEE float samples (32 bits), format tag 3 or the extensible IEEE_FLOAT subtype.
    float: bool,
    data: usize,
    length: usize,
}

fn parse(bytes: &[u8]) -> Option<Wav> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return None;
    }
    let (mut channels, mut rate, mut bits, mut format) = (0usize, 0u32, 0usize, 0u16);
    let mut subtype = 0u16;
    let mut at = 12;
    while at + 8 <= bytes.len() {
        let size = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().ok()?) as usize;
        let body = at + 8;
        match &bytes[at..at + 4] {
            b"fmt " if body + 16 <= bytes.len() => {
                format = u16::from_le_bytes(bytes[body..body + 2].try_into().ok()?);
                channels = usize::from(u16::from_le_bytes(bytes[body + 2..body + 4].try_into().ok()?));
                rate = u32::from_le_bytes(bytes[body + 4..body + 8].try_into().ok()?);
                bits = usize::from(u16::from_le_bytes(bytes[body + 14..body + 16].try_into().ok()?));
                if size >= 40 && body + 26 <= bytes.len() {
                    subtype = u16::from_le_bytes(bytes[body + 24..body + 26].try_into().ok()?); // the sub-format GUID's first word
                }
            }
            b"data" => {
                // 1 = PCM, 3 = IEEE float, 0xfffe = extensible (its sub-format says which)
                let float = format == 3 || (format == 0xfffe && subtype == 3);
                if (format != 1 && format != 3 && format != 0xfffe) || channels == 0 || !matches!(bits, 8 | 16 | 24 | 32) || (float && bits != 32) {
                    return None;
                }
                let length = size.min(bytes.len() - body);
                return Some(Wav { channels, rate, bits, float, data: body, length });
            }
            _ => {}
        }
        at = body + size + (size & 1);
    }
    None
}

/// One file sample as -1..1 (a float file's samples as they are).
fn file_sample(bytes: &[u8], wav: &Wav) -> f32 {
    if wav.float {
        audio_sample::Format::new(4, 32, true).read(bytes)
    } else {
        sample(bytes, wav.bits) as f32 * (1.0 / 2147483648.0)
    }
}

/// One file sample as a signed value left-justified in 32 bits.
fn sample(bytes: &[u8], bits: usize) -> i32 {
    match bits {
        8 => (i32::from(bytes[0]) - 128) << 24, // 8-bit WAV is unsigned
        16 => i32::from(i16::from_le_bytes([bytes[0], bytes[1]])) << 16,
        24 => i32::from_le_bytes([0, bytes[0], bytes[1], bytes[2]]),
        _ => i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(api: &'static TapeApi) {
    tape_rt::init(api);
    let name = match tape_rt::take_audio_device_arg(api, core::str::from_utf8(tape_rt::args()).unwrap_or("").trim()) {
        Ok(rest) => rest.trim(),
        Err(reason) => return println!("play: {reason}."),
    };
    if name.is_empty() {
        return println!("Usage: tape run play [device=N] NAME.wav");
    }
    let mut info = AudioInfo::default();
    if (api.audio_info)(&mut info) == 0 {
        return println!("play: no audio device (USB audio or HD Audio).");
    }
    if info.playback_channels == 0 {
        return println!("play: the device has no outputs.");
    }
    // Read the file, growing the buffer until it fits.
    let mut capacity = 4 << 20;
    let (buffer, length) = loop {
        let buffer = (api.alloc)(capacity);
        if buffer == 0 {
            return println!("play: not enough memory.");
        }
        let result = (api.fs_read)(name.as_ptr(), name.len(), buffer as *mut u8, capacity);
        if result == FS_TOO_BIG && capacity < MAX_BYTES {
            (api.free)(buffer, capacity);
            capacity *= 4;
            continue;
        }
        if result < 0 {
            (api.free)(buffer, capacity);
            return println!("play: cannot read {name} (code {result}).");
        }
        break (buffer, result as usize);
    };
    let file = unsafe { core::slice::from_raw_parts(buffer as *const u8, length) };
    let Some(wav) = parse(file) else {
        (api.free)(buffer, capacity);
        return println!("play: {name} is not a PCM WAV file.");
    };
    let (out_channels, out_slot) = (info.playback_channels as usize, info.playback_slot_bytes as usize);
    let in_slot = wav.bits / 8;
    let out_float = tape_rt::audio_float().1;
    let out_format = audio_sample::Format::new(out_slot, info.playback_bits as usize, out_float);
    let in_frame = wav.channels * in_slot;
    let frames = wav.length / in_frame;
    println!(
        "Playing {name}: {} Hz, {} channel(s), {}{}-bit, {} frames ({} s) on {out_channels} output channel(s) of {}{}-bit.",
        wav.rate, wav.channels, if wav.float { "float " } else { "" }, wav.bits, frames, frames / wav.rate as usize, if out_float { "float " } else { "" }, info.playback_bits
    );
    let status = (api.audio_start)(wav.rate, 0, 1);
    if status != 0 {
        (api.free)(buffer, capacity);
        return println!("play: cannot start playback at {} Hz (code {status}); the device offers {:?}.", wav.rate, &info.rates[..info.rate_count as usize]);
    }
    let data = &file[wav.data..wav.data + frames * in_frame];
    let mut staging = [0u8; 8192];
    let out_frame = out_channels * out_slot;
    let (mut frame, mut staged, mut sent) = (0usize, 0usize, 0usize);
    let mut aborted = false;
    while frame < frames || sent < staged {
        // Convert as many frames as fit into the staging buffer once the previous batch is out.
        if sent == staged && frame < frames {
            staged = 0;
            sent = 0;
            while frame < frames && staged + out_frame <= staging.len() {
                let source = &data[frame * in_frame..(frame + 1) * in_frame];
                for channel in 0..out_channels {
                    if wav.float || out_float {
                        // Through f32 so a float file or device is never rounded to an integer on the way.
                        let value = if wav.channels == 1 {
                            file_sample(&source[..in_slot], &wav)
                        } else if channel < wav.channels {
                            file_sample(&source[channel * in_slot..(channel + 1) * in_slot], &wav)
                        } else {
                            0.0
                        };
                        out_format.write(value, &mut staging[staged..staged + out_slot]);
                        staged += out_slot;
                        continue;
                    }
                    // mono: every output channel gets the one input channel; otherwise channel for channel, silence for the rest
                    let value = if wav.channels == 1 {
                        sample(&source[..in_slot], wav.bits)
                    } else if channel < wav.channels {
                        sample(&source[channel * in_slot..(channel + 1) * in_slot], wav.bits)
                    } else {
                        0
                    };
                    // keep the top `out_slot` bytes of the 32-bit value
                    let bytes = value.to_le_bytes();
                    staging[staged..staged + out_slot].copy_from_slice(&bytes[4 - out_slot..]);
                    staged += out_slot;
                }
                frame += 1;
            }
        }
        let accepted = (api.audio_write)(staging[sent..staged].as_ptr(), staged - sent);
        sent += accepted;
        if (api.should_abort)() != 0 {
            aborted = true;
            break;
        }
        if accepted == 0 {
            tape_rt::idle();
        }
    }
    // Underruns are counted up to the moment the ring runs empty: the silence the device is fed while the last packets finish
    // is not one, and would show as about 300 ms worth of them.
    let mut underruns = None;
    // Let the ring drain (it is empty when the whole ring is free), then a little more for the packets in flight.
    if !aborted {
        let mut waited = 0;
        loop {
            (api.audio_info)(&mut info);
            if info.playback_space as usize >= (1 << 20) - 64 || (api.should_abort)() != 0 || waited > 120_000 {
                break;
            }
            tape_rt::idle();
            waited += 1;
        }
        let mut drained = AudioStats::default();
        (api.audio_stats)(&mut drained);
        underruns = Some(drained.playback_underruns);
        let until = tape_rt::micros() + 300_000;
        while tape_rt::micros() < until {
            let _ = (api.audio_read)(core::ptr::null_mut(), 0);
            tape_rt::idle();
        }
    }
    let mut stats = AudioStats::default();
    (api.audio_stats)(&mut stats);
    (api.audio_stop)();
    (api.free)(buffer, capacity);
    println!("{}: {} packets, {} transfer error(s), {} underrun(s).", if aborted { "Stopped" } else { "Finished" }, stats.playback_packets, stats.playback_errors, underruns.unwrap_or(stats.playback_underruns));
}

tape_rt::tape_help!(br#"play: play a WAV file on the audio outputs.

  tape run play [device=N] NAME.wav

  8, 16, 24 and 32 bit PCM and 32 bit float; the file's rate must be one the device offers.
  Escape stops.
"#);
