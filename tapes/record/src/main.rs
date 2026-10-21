//! `record [device=N] NAME [SECONDS] [RATE]`: records from the audio device's inputs (USB audio or HD Audio) into a WAV file,
//! straight to disk.
//!
//! ```text
//! tape run record take1 10        10 seconds at 48000 Hz into take1.wav
//! tape run record take1 0 96000   until Escape (or the volume is full) at 96000 Hz
//! tape run record take1 5 48000 tone=440   a test tone instead of the device (no audio hardware needed)
//! tape run record disk2/take1 0   onto another disk's volume (`@LABEL/take1` works too)
//! ```
//!
//! The audio is written to the volume as it arrives, in 256 KiB blocks, so the length of a take is limited by the
//! free space of the volume, not by RAM (the WAV header becomes RF64 past 4 GiB). A FOSFS file is one contiguous run of sectors,
//! so the space is reserved when the recording starts: `SECONDS` worth, or the largest free run when it is 0. The
//! file length (and the WAV header) are brought up to date every two seconds, so a power cut or reset keeps what was
//! recorded up to then. The file holds every input channel the device has (6 for a Zoom H6 in multichannel mode,
//! 22 for an RME UFX), in the device's own sample width (16 or 24 bit), as plain PCM, which most editors read.
//! If the disk is slower than the audio the capture ring (1 MiB, about a third of a second of 22 channels) overruns:
//! the report says so, and the take has a gap of that size.
#![no_std]
#![no_main]

use audio_rec::recorder::Recorder;
use audio_rec::stream::StreamSink;
use audio_rec::wav::Format;
use tape_rt::{AudioInfo, AudioStats, TapeApi};
use tape_rt::{print, println};

/// Bytes written to disk at a time: a multiple of the sector size, small enough that one disk write takes well under
/// the capture ring's headroom even on a slow USB stick.
const CHUNK: usize = 256 << 10;
/// Audio is taken from the device this much at a time.
const BLOCK: usize = 64 << 10;

static mut BLOCK_BUFFER: [u8; BLOCK] = [0; BLOCK];
static mut SINK: Option<StreamSink> = None;

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

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(api: &'static TapeApi) {
    tape_rt::init(api);
    let arguments = match tape_rt::take_audio_device_arg(api, core::str::from_utf8(tape_rt::args()).unwrap_or("")) {
        Ok(rest) => rest,
        Err(reason) => return println!("record: {reason}."),
    };
    let Some((name, seconds, rate, tone)) = parse_args(arguments) else {
        return println!("Usage: tape run record [device=N] NAME [SECONDS] [RATE] [tone=HZ]   (NAME.wav is written; SECONDS 0 = until Escape)");
    };
    let mut info = AudioInfo::default();
    if tone.is_none() {
        if (api.audio_info)(&mut info) == 0 {
            return println!("record: no audio device (USB audio or HD Audio).");
        }
        if info.capture_channels == 0 {
            return println!("record: the device has no inputs.");
        }
    }
    // A test tone stands in for the device as 2 channels of 16 bits.
    let (channels, slot, bits) = if tone.is_some() { (2, 2, 16) } else { (info.capture_channels as usize, info.capture_slot_bytes as usize, info.capture_bits as usize) };
    let frame_bytes = channels * slot;
    if frame_bytes == 0 || frame_bytes > BLOCK {
        return println!("record: unusable capture format.");
    }
    // A float device is recorded unconverted, as a float WAV.
    let float = tone.is_none() && tape_rt::audio_float().0;
    let format = Format { channels: channels as u16, rate, slot: slot as u16, bits: bits as u16, float };
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
    let target = if seconds == 0 { u64::MAX } else { u64::from(rate) * frame_bytes as u64 * seconds };
    let wanted = if seconds == 0 { 0 } else { audio_rec::wav::header_len(&format) as u64 + target };
    let staging_address = (api.alloc)(CHUNK);
    if staging_address == 0 {
        return println!("record: out of memory.");
    }
    let staging: &'static mut [u8] = unsafe { core::slice::from_raw_parts_mut(staging_address as *mut u8, CHUNK) };
    unsafe { SINK = Some(StreamSink::new(api.fs_stream_open, api.fs_stream_write, api.fs_stream_close, tape_rt::micros, CHUNK / 2)) };
    let sink = unsafe { (*(&raw mut SINK)).as_mut().unwrap() };
    let mut recorder = match Recorder::begin(sink, staging, format, file_name, wanted) {
        Ok(recorder) => recorder,
        Err(error) => {
            (api.free)(staging_address, CHUNK);
            return println!("record: cannot reserve {} MiB for {shown} (code {}); the volume may be full or the name invalid.", wanted >> 20, error.0);
        }
    };
    let status = if tone.is_some() { 0 } else { (api.audio_start)(rate, 1, 0) };
    if status != 0 {
        let _ = recorder.finish(); // nothing recorded: the file is removed again
        (api.free)(staging_address, CHUNK);
        return println!("record: cannot start capture at {rate} Hz (code {status}); the device offers {:?}.", &info.rates[..info.rate_count as usize]);
    }
    println!(
        "Recording {channels} channel(s), {}{bits}-bit, {rate} Hz into {shown} ({} MiB reserved){}. Escape stops.",
        if float { "float " } else { "" },
        recorder.capacity() >> 20,
        if seconds == 0 { ", until Escape" } else { "" }
    );
    let block = unsafe { &mut *(&raw mut BLOCK_BUFFER) };
    let mut failed = 0;
    let (mut aborted, mut full) = (false, false);
    let mut next_report = tape_rt::micros() + 1_000_000;
    let mut next_checkpoint = tape_rt::micros() + 2_000_000;
    let (started, mut produced, mut phase) = (tape_rt::micros(), 0u64, 0u32);
    loop {
        if (api.should_abort)() != 0 {
            aborted = true;
            break;
        }
        let left = target - recorder.audio_bytes();
        if left == 0 {
            break;
        }
        let room = (BLOCK as u64).min(left) as usize / frame_bytes * frame_bytes;
        let got = if let Some(frequency) = tone {
            // Paced by the clock like a device: the frames due so far, as many as fit.
            let due = (tape_rt::micros() - started) * u64::from(rate) / 1_000_000;
            let frames = (due - produced).min((room / frame_bytes) as u64) as usize;
            for index in 0..frames {
                let sample = tone_sample(phase).to_le_bytes();
                phase = (phase + (u64::from(frequency) * 65_536 / u64::from(rate)) as u32) & 0xffff;
                block[index * frame_bytes..index * frame_bytes + 2].copy_from_slice(&sample);
                block[index * frame_bytes + 2..index * frame_bytes + 4].copy_from_slice(&sample);
            }
            produced += frames as u64;
            frames * frame_bytes
        } else {
            (api.audio_read)(block.as_mut_ptr(), room)
        };
        if got > 0 {
            match recorder.push(&block[..got]) {
                Ok(taken) if taken < got => {
                    full = true; // the reserved space ran out
                    break;
                }
                Ok(_) => {}
                Err(error) => {
                    failed = error.0;
                    break;
                }
            }
        } else {
            tape_rt::idle();
        }
        let now = tape_rt::micros();
        if now >= next_report {
            next_report += 1_000_000;
            print!(".");
        }
        if now >= next_checkpoint {
            next_checkpoint = now + 2_000_000;
            if let Err(error) = recorder.checkpoint() {
                failed = error.0;
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
    // The unwritten rest, the final header and the final length.
    let summary = recorder.finish();
    (api.free)(staging_address, CHUNK);
    let sink = unsafe { (*(&raw const SINK)).as_ref().unwrap() };
    match summary {
        Ok(summary) if failed == 0 => {
            println!(
                "Recorded {} frames ({} s){}{}; {} transfer error(s), {} overrun(s).",
                summary.frames,
                summary.frames / u64::from(rate),
                if aborted { ", stopped early" } else { "" },
                if full { ", the reserved space ran out" } else { "" },
                stats.capture_errors,
                stats.capture_overruns
            );
            if sink.writes > 0 {
                println!("Disk: {} blocks, {} KiB/s while writing, slowest block {} ms.", sink.writes, sink.kib_per_second(), sink.write_max_us / 1000);
            }
            println!("Wrote {shown} ({} bytes).", summary.bytes);
        }
        Ok(_) => println!("record: writing {shown} failed (code {failed}); what was written up to the last checkpoint is kept."),
        Err(error) => println!("record: writing {shown} failed (code {}); what was written up to the last checkpoint is kept.", error.0),
    }
}

tape_rt::tape_help!(br#"record: record the audio inputs into a WAV file on disk.

  tape run record [device=N] NAME [SECONDS] [RATE] [tone=HZ]

  tape run record take1 10        10 seconds at 48000 Hz into take1.wav
  tape run record take1 0 96000   until Escape, at 96000 Hz
  tape run record disk2/take1 0   onto another volume (@LABEL/take1 too)
  tone=HZ records a test tone instead of the device.
"#);
