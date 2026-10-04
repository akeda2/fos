//! `record NAME [SECONDS] [RATE]`: records from the USB audio device's inputs into a WAV file.
//!
//! ```text
//! tape run record take1 10        10 seconds at 48000 Hz into take1.wav
//! tape run record take1 10 96000
//! ```
//!
//! The audio is captured into RAM (so a slow disk cannot lose samples), then written as one
//! WAV file when the time is up or Escape is pressed (what was captured so far is kept).
//! The file holds every input channel the device has (6 for a Zoom H6 in multichannel mode), in
//! the device's own sample width (16 or 24 bit), as plain PCM, which most editors read; it is
//! limited by the FOSFS volume's free space and by 64 MiB of RAM.
#![no_std]
#![no_main]

use tape_rt::{TapeApi, print, println};
use tape_rt::{AudioInfo, AudioStats};

const MAX_BYTES: usize = 64 << 20;

fn parse_args(text: &str) -> Option<(&str, u32, u32)> {
    let mut words = text.split_ascii_whitespace();
    let name = words.next()?;
    let seconds = words.next().map_or(Some(10), |word| word.parse().ok())?;
    let rate = words.next().map_or(Some(48_000), |word| word.parse().ok())?;
    Some((name, seconds, rate))
}

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(api: &'static TapeApi) {
    tape_rt::init(api);
    let arguments = core::str::from_utf8(tape_rt::args()).unwrap_or("");
    let Some((name, seconds, rate)) = parse_args(arguments) else {
        return println!("Usage: tape run record NAME [SECONDS] [RATE]   (NAME.wav is written)");
    };
    let mut info = AudioInfo::default();
    if (api.audio_info)(&mut info) == 0 {
        return println!("record: no USB audio device.");
    }
    if info.capture_channels == 0 {
        return println!("record: the device has no inputs.");
    }
    let (channels, slot, bits) = (info.capture_channels as usize, info.capture_slot_bytes as usize, info.capture_bits);
    let frame_bytes = channels * slot;
    let target = (rate as usize * frame_bytes * seconds as usize).min(MAX_BYTES / frame_bytes * frame_bytes);
    let mut file_name = [0u8; 20];
    let mut length = 0;
    for &byte in name.as_bytes().iter().take(12) {
        file_name[length] = byte;
        length += 1;
    }
    if !name.contains('.') {
        file_name[length..length + 4].copy_from_slice(b".wav");
        length += 4;
    }
    let file_name = &file_name[..length];
    let buffer = (api.alloc)(44 + target);
    if buffer == 0 {
        return println!("record: not enough memory for {} MiB.", target >> 20);
    }
    let status = (api.audio_start)(rate, 1, 0);
    if status != 0 {
        (api.free)(buffer, 44 + target);
        return println!("record: cannot start capture at {rate} Hz (code {status}); the device offers {:?}.", &info.rates[..info.rate_count as usize]);
    }
    println!("Recording {channels} channel(s), {bits}-bit, {rate} Hz for {seconds} s into {}. Escape stops early.", core::str::from_utf8(file_name).unwrap_or("?"));
    let data = buffer + 44;
    let mut filled = 0usize;
    let mut next_report = tape_rt::micros() + 1_000_000;
    let mut aborted = false;
    while filled < target {
        let got = (api.audio_read)((data + filled) as *mut u8, target - filled);
        filled += got;
        if (api.should_abort)() != 0 {
            aborted = true;
            break;
        }
        if got == 0 {
            tape_rt::idle();
        }
        if tape_rt::micros() >= next_report {
            next_report += 1_000_000;
            print!(".");
        }
    }
    let mut stats = AudioStats::default();
    (api.audio_stats)(&mut stats);
    (api.audio_stop)();
    filled = filled / frame_bytes * frame_bytes;
    println!();
    // WAV header: RIFF, "fmt " (PCM), "data".
    let header = unsafe { core::slice::from_raw_parts_mut(buffer as *mut u8, 44) };
    header[0..4].copy_from_slice(b"RIFF");
    header[4..8].copy_from_slice(&((36 + filled) as u32).to_le_bytes());
    header[8..16].copy_from_slice(b"WAVEfmt ");
    header[16..20].copy_from_slice(&16u32.to_le_bytes());
    header[20..22].copy_from_slice(&1u16.to_le_bytes()); // PCM
    header[22..24].copy_from_slice(&(channels as u16).to_le_bytes());
    header[24..28].copy_from_slice(&rate.to_le_bytes());
    header[28..32].copy_from_slice(&((rate as usize * frame_bytes) as u32).to_le_bytes());
    header[32..34].copy_from_slice(&(frame_bytes as u16).to_le_bytes());
    header[34..36].copy_from_slice(&((slot * 8) as u16).to_le_bytes());
    header[36..40].copy_from_slice(b"data");
    header[40..44].copy_from_slice(&(filled as u32).to_le_bytes());
    let written = (api.fs_write)(file_name.as_ptr(), file_name.len(), buffer as *const u8, 44 + filled);
    (api.free)(buffer, 44 + target);
    println!(
        "Captured {} frames ({} s){}; {} transfer error(s), {} overrun(s).",
        filled / frame_bytes,
        filled / frame_bytes / rate as usize,
        if aborted { ", stopped early" } else { "" },
        stats.capture_errors,
        stats.capture_overruns
    );
    if written == 0 {
        println!("Wrote {} ({} bytes).", core::str::from_utf8(file_name).unwrap_or("?"), 44 + filled);
    } else {
        println!("record: writing the file failed (code {written}); the volume may be full.");
    }
}
