//! `record NAME [SECONDS] [RATE]`: records from the USB audio device's inputs into a WAV file, straight to disk.
//!
//! ```text
//! tape run record take1 10        10 seconds at 48000 Hz into take1.wav
//! tape run record take1 0 96000   until Escape (or the volume is full) at 96000 Hz
//! tape run record take1 5 48000 tone=440   a test tone instead of the device (no audio hardware needed)
//! tape run record disk2/take1 0   onto another disk's volume (`@LABEL/take1` works too)
//! ```
//!
//! The audio is written to the volume as it arrives, in 256 KiB blocks, so the length of a take is limited by the
//! free space of the volume (and the WAV format's 4 GiB), not by RAM. A FOSFS file is one contiguous run of sectors,
//! so the space is reserved when the recording starts: `SECONDS` worth, or the largest free run when it is 0. The
//! file length (and the WAV header) are brought up to date every two seconds, so a power cut or reset keeps what was
//! recorded up to then. The file holds every input channel the device has (6 for a Zoom H6 in multichannel mode,
//! 22 for an RME UFX), in the device's own sample width (16 or 24 bit), as plain PCM, which most editors read.
//! If the disk is slower than the audio the capture ring (1 MiB, about a third of a second of 22 channels) overruns:
//! the report says so, and the take has a gap of that size.
#![no_std]
#![no_main]

use tape_rt::{AudioInfo, AudioStats};
use tape_rt::{TapeApi, print, println};

/// Bytes written to disk at a time: a multiple of the sector size, small enough that one disk write takes well under
/// the capture ring's headroom even on a slow USB stick.
const CHUNK: usize = 256 << 10;
const WAV_LIMIT: u64 = 0xffff_ffff - 36;

fn parse_args(text: &str) -> Option<(&str, u64, u32, Option<u32>)> {
    let mut words = text.split_ascii_whitespace();
    let name = words.next()?;
    let seconds = words.next().map_or(Some(10), |word| word.parse().ok())?;
    let rate = words.next().map_or(Some(48_000), |word| word.parse().ok())?;
    let tone = match words.next() {
        Some(word) => Some(word.strip_prefix("tone=")?.parse().ok()?),
        None => None,
    };
    Some((name, seconds, rate, tone))
}

/// A parabola per half cycle standing in for a sine (integer only), a quarter of full scale, 16 bits.
fn tone_sample(phase: u32) -> i16 {
    let half = i64::from(phase & 0x7fff);
    let magnitude = (half * (0x8000 - half)) >> 13;
    (if phase & 0x8000 != 0 { -magnitude } else { magnitude } / 4) as i16
}

/// The 44-byte WAV header for `data_bytes` of audio.
fn wav_header(header: &mut [u8], channels: usize, slot: usize, rate: u32, data_bytes: u64) {
    let frame_bytes = channels * slot;
    header[0..4].copy_from_slice(b"RIFF");
    header[4..8].copy_from_slice(&((36 + data_bytes) as u32).to_le_bytes());
    header[8..16].copy_from_slice(b"WAVEfmt ");
    header[16..20].copy_from_slice(&16u32.to_le_bytes());
    header[20..22].copy_from_slice(&1u16.to_le_bytes()); // PCM
    header[22..24].copy_from_slice(&(channels as u16).to_le_bytes());
    header[24..28].copy_from_slice(&rate.to_le_bytes());
    header[28..32].copy_from_slice(&((rate as usize * frame_bytes) as u32).to_le_bytes());
    header[32..34].copy_from_slice(&(frame_bytes as u16).to_le_bytes());
    header[34..36].copy_from_slice(&((slot * 8) as u16).to_le_bytes());
    header[36..40].copy_from_slice(b"data");
    header[40..44].copy_from_slice(&(data_bytes as u32).to_le_bytes());
}

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(api: &'static TapeApi) {
    tape_rt::init(api);
    let arguments = core::str::from_utf8(tape_rt::args()).unwrap_or("");
    let Some((name, seconds, rate, tone)) = parse_args(arguments) else {
        return println!("Usage: tape run record NAME [SECONDS] [RATE] [tone=HZ]   (NAME.wav is written; SECONDS 0 = until Escape)");
    };
    let mut info = AudioInfo::default();
    if tone.is_none() {
        if (api.audio_info)(&mut info) == 0 {
            return println!("record: no USB audio device.");
        }
        if info.capture_channels == 0 {
            return println!("record: the device has no inputs.");
        }
    }
    // A test tone stands in for the device as 2 channels of 16 bits.
    let (channels, slot, bits) = if tone.is_some() { (2, 2, 16) } else { (info.capture_channels as usize, info.capture_slot_bytes as usize, info.capture_bits) };
    let frame_bytes = channels * slot;
    let mut file_name = [0u8; 40];
    let mut length = 0;
    for &byte in name.as_bytes().iter().take(30) {
        file_name[length] = byte;
        length += 1;
    }
    if !name.rsplit('/').next().unwrap_or(name).contains('.') {
        file_name[length..length + 4].copy_from_slice(b".wav");
        length += 4;
    }
    let file_name = &file_name[..length];
    let shown = core::str::from_utf8(file_name).unwrap_or("?");
    let wanted = if seconds == 0 { 0 } else { 44 + u64::from(rate) * frame_bytes as u64 * seconds };
    let capacity = (api.fs_stream_open)(file_name.as_ptr(), file_name.len(), wanted);
    if capacity < 0 {
        return println!("record: cannot reserve {} MiB for {shown} (code {capacity}); the volume may be full or the name invalid.", wanted >> 20);
    }
    let limit = (capacity as u64).min(WAV_LIMIT);
    let buffer = (api.alloc)(CHUNK);
    if buffer == 0 {
        return println!("record: out of memory.");
    }
    let staging = unsafe { core::slice::from_raw_parts_mut(buffer as *mut u8, CHUNK) };
    let status = if tone.is_some() { 0 } else { (api.audio_start)(rate, 1, 0) };
    if status != 0 {
        (api.free)(buffer, CHUNK);
        return println!("record: cannot start capture at {rate} Hz (code {status}); the device offers {:?}.", &info.rates[..info.rate_count as usize]);
    }
    println!(
        "Recording {channels} channel(s), {bits}-bit, {rate} Hz into {shown} ({} MiB reserved){}. Escape stops.",
        capacity >> 20,
        if seconds == 0 { ", until Escape" } else { "" }
    );
    // The first block starts with the WAV header; its first sector is kept to be rewritten as the take grows.
    let mut first_sector = [0u8; 512];
    let mut have_first = false;
    let mut filled = 44usize; // bytes in `staging`; the first 44 are the header
    let mut offset = 0u64; // file offset of staging[0]
    let mut failed = 0isize;
    let (mut aborted, mut full) = (false, false);
    let mut next_report = tape_rt::micros() + 1_000_000;
    let mut next_checkpoint = tape_rt::micros() + 2_000_000;
    let target = if seconds == 0 { u64::MAX } else { u64::from(rate) * frame_bytes as u64 * seconds };
    let mut audio_bytes = 0u64;
    let (mut write_total, mut write_max, mut writes, mut written_bytes) = (0u64, 0u64, 0u64, 0u64);
    let (started, mut produced, mut phase) = (tape_rt::micros(), 0u64, 0u32);
    loop {
        if (api.should_abort)() != 0 {
            aborted = true;
            break;
        }
        let left = (limit - offset - filled as u64).min(target - audio_bytes).min(usize::MAX as u64) as usize; // reserved space and time left
        let room = (CHUNK - filled).min(left);
        let got = if room == 0 {
            0
        } else if let Some(frequency) = tone {
            // Paced by the clock like a device: the frames due so far, as many as fit.
            let due = (tape_rt::micros() - started) * u64::from(rate) / 1_000_000;
            let frames = (due - produced).min((room / frame_bytes) as u64) as usize;
            for index in 0..frames {
                let sample = tone_sample(phase).to_le_bytes();
                phase = (phase + (u64::from(frequency) * 65_536 / u64::from(rate)) as u32) & 0xffff;
                let at = filled + index * frame_bytes;
                staging[at..at + 2].copy_from_slice(&sample);
                staging[at + 2..at + 4].copy_from_slice(&sample);
            }
            produced += frames as u64;
            frames * frame_bytes
        } else {
            (api.audio_read)(staging[filled..].as_mut_ptr(), room)
        };
        filled += got;
        audio_bytes += got as u64;
        let at_end = audio_bytes >= target || offset + filled as u64 >= limit;
        if filled == CHUNK || (at_end && filled > 0) {
            // Whole blocks keep the next offset a multiple of 512; the last, partial one is padded by the kernel.
            if offset == 0 {
                wav_header(staging, channels, slot, rate, audio_bytes);
                if filled >= 512 {
                    first_sector.copy_from_slice(&staging[..512]);
                    have_first = true;
                }
            }
            // The length (and, at a checkpoint, the header) only advance together: a reset must find a file whose
            // header matches its length. The first block carries its own header, so it can set the length.
            let length = if offset == 0 || at_end { offset + filled as u64 } else { 0 };
            let began = tape_rt::micros();
            failed = (api.fs_stream_write)(offset, staging.as_ptr(), filled, length);
            let spent = tape_rt::micros() - began;
            (write_total, write_max, writes, written_bytes) = (write_total + spent, write_max.max(spent), writes + 1, written_bytes + filled as u64);
            if failed != 0 {
                break;
            }
            offset += filled as u64;
            filled = 0;
            if at_end {
                full = audio_bytes < target;
                break;
            }
        }
        if got == 0 {
            tape_rt::idle();
        }
        let now = tape_rt::micros();
        if now >= next_report {
            next_report += 1_000_000;
            print!(".");
        }
        if now >= next_checkpoint && have_first {
            next_checkpoint = now + 2_000_000;
            // Bring the first sector's header up to date so a reset keeps the take so far.
            wav_header(&mut first_sector, channels, slot, rate, offset - 44);
            failed = (api.fs_stream_write)(0, first_sector.as_ptr(), 512, offset);
            if failed != 0 {
                break;
            }
        }
    }
    let mut stats = AudioStats::default();
    if tone.is_none() {
        (api.audio_stats)(&mut stats);
        (api.audio_stop)();
    }
    println!();
    if failed == 0 && filled > 0 {
        // The unwritten rest: Escape was pressed in the middle of a block.
        if offset == 0 {
            wav_header(staging, channels, slot, rate, audio_bytes);
            if filled >= 512 {
                first_sector.copy_from_slice(&staging[..512]);
                have_first = true;
            }
        }
        failed = (api.fs_stream_write)(offset, staging.as_ptr(), filled, offset + filled as u64);
        offset += filled as u64;
    }
    let total = offset;
    if failed == 0 && have_first {
        // Final header, with the exact data length.
        wav_header(&mut first_sector, channels, slot, rate, total - 44);
        failed = (api.fs_stream_write)(0, first_sector.as_ptr(), 512, total);
    }
    if failed == 0 {
        failed = (api.fs_stream_close)(total); // free the reserved space the take did not use
    }
    (api.free)(buffer, CHUNK);
    let frames = (total.saturating_sub(44)) / frame_bytes as u64;
    println!(
        "Recorded {frames} frames ({} s){}{}; {} transfer error(s), {} overrun(s).",
        frames / u64::from(rate),
        if aborted { ", stopped early" } else { "" },
        if full { ", the reserved space ran out" } else { "" },
        stats.capture_errors,
        stats.capture_overruns
    );
    if writes > 0 {
        println!("Disk: {writes} blocks, {} KiB/s while writing, slowest block {} ms.", written_bytes * 1000 / 1024 / (write_total / 1000).max(1), write_max / 1000);
    }
    if failed == 0 {
        println!("Wrote {shown} ({total} bytes).");
    } else {
        println!("record: writing {shown} failed (code {failed}); what was written up to the last checkpoint is kept.");
    }
}
