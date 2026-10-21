//! The `audio` command both shells share (USB audio on both kernels, HD Audio on x86: the facade is `audio.rs`).
//! The loops need three things from their kernel: keep the machine alive (watchdog), service the USB controller, and
//! flush a remote terminal session; see `imp`.

/// `audio` shows the audio device (USB audio or HD Audio; `audio device N` picks one when there are several); `audio start RATE [capture|playback|both]` / `audio stop`
/// control streams; `audio capture SECONDS [RATE]` records for a few seconds and prints, per second,
/// the packet counters and the peak level of every channel (a quick way to see that a device's inputs
/// are alive without writing a tape).
pub fn command(rest: &str) {
    let mut words = rest.split_whitespace();
    use crate::audio as ua;
    match words.next() {
        None => ua::status(),
        Some("modes") => ua::print_modes(),
        Some("use") => {
            let capture = match words.next() {
                Some("capture") => true,
                Some("playback") => false,
                _ => return crate::println!("Usage: audio use capture|playback N|auto   (`audio modes` lists the numbers)"),
            };
            let mode = match words.next() {
                Some("auto") | Some("0") => 0,
                Some(word) => match word.parse::<u32>() {
                    Ok(number) => number,
                    Err(_) => return crate::println!("Usage: audio use capture|playback N|auto"),
                },
                None => return crate::println!("Usage: audio use capture|playback N|auto"),
            };
            match ua::select(capture, mode) {
                Ok(()) => crate::println!("{} mode: {}.", if capture { "Capture" } else { "Playback" }, if mode == 0 { "automatic" } else { "picked (`audio modes` shows which)" }),
                Err(reason) => crate::println!("audio: {reason}."),
            }
        }
        Some("device") => {
            let number = match words.next() {
                Some("auto") | None => None,
                Some(word) => match word.parse::<usize>() {
                    Ok(number) => Some(number),
                    Err(_) => return crate::println!("Usage: audio device N|auto   (`audio` lists the devices)"),
                },
            };
            match ua::choose(number) {
                Ok(()) => ua::status(),
                Err(reason) => crate::println!("audio: {reason}."),
            }
        }
        Some("output") => {
            let number = match words.next() {
                Some("same") | Some("auto") | None => None,
                Some(word) => match word.parse::<usize>() {
                    Ok(number) => Some(number),
                    Err(_) => return crate::println!("Usage: audio output N|same   (`audio` lists the devices)"),
                },
            };
            match ua::choose_output(number) {
                Ok(()) => ua::status(),
                Err(reason) => crate::println!("audio: {reason}."),
            }
        }
        Some("stop") => {
            ua::stop();
            ua::status();
        }
        Some("start") => {
            let rate = words.next().and_then(|word| word.parse::<u32>().ok()).unwrap_or(48_000);
            let (capture, playback) = match words.next() {
                Some("capture") => (true, false),
                Some("playback") => (false, true),
                _ => (true, true),
            };
            match ua::start(rate, capture, playback) {
                Ok(()) => ua::status(),
                Err(reason) => crate::println!("audio: {reason}."),
            }
        }
        Some("capture") => {
            let seconds = words.next().and_then(|word| word.parse::<u64>().ok()).unwrap_or(3).clamp(1, 30);
            let rate = words.next().and_then(|word| word.parse::<u32>().ok()).unwrap_or(48_000);
            if let Err(reason) = ua::start(rate, true, false) {
                return crate::println!("audio: {reason}.");
            }
            let Some((channels, slot)) = ua::capture_format() else { return };
            let (channels, slot) = (usize::from(channels), usize::from(slot));
            let mut data = [0u8; 4096];
            for second in 1..=seconds {
                let mut peaks = [0u32; 32];
                let mut carry = 0usize; // bytes of an incomplete frame left from the previous read
                let end = crate::timer::ticks() + crate::timer::TICK_HZ;
                let mut frames = 0u64;
                while crate::timer::ticks() < end {
                    pump();
                    let frame_bytes = channels * slot;
                    let room = (4096 - carry) / frame_bytes * frame_bytes;
                    let got = ua::read(&mut data[carry..carry + room]);
                    let total = carry + got;
                    let whole = total / frame_bytes * frame_bytes;
                    for frame in data[..whole].chunks(frame_bytes) {
                        for (channel, sample) in frame.chunks(slot).enumerate().take(32) {
                            // little-endian signed PCM in `slot` bytes: sign-extend, take the magnitude
                            let mut value = 0i64;
                            for (index, &byte) in sample.iter().enumerate() {
                                value |= i64::from(byte) << (8 * index);
                            }
                            let bits = 8 * slot as u32;
                            if value >> (bits - 1) != 0 {
                                value -= 1 << bits;
                            }
                            peaks[channel] = peaks[channel].max(value.unsigned_abs() as u32);
                        }
                        frames += 1;
                    }
                    data.copy_within(whole..total, 0);
                    carry = total - whole;
                }
                // Peaks, eight channels per line, and the loudest channel named (devices have up to 32 here).
                let shown = channels.min(32);
                let loudest = (0..shown).max_by_key(|&channel| peaks[channel]).unwrap_or(0);
                crate::println!("[{second:>2} s] {frames} frames; loudest channel {} (peak {})", loudest + 1, peaks[loudest]);
                for (line, group) in peaks[..shown].chunks(8).enumerate() {
                    crate::print(format_args!("   ch {:>2}-{:>2}:", line * 8 + 1, line * 8 + group.len()));
                    for peak in group {
                        crate::print(format_args!(" {peak:>8}"));
                    }
                    crate::println!();
                }
                imp::flush();
            }
            ua::status();
            ua::stop();
        }
        Some("duplex") => {
            // Capture and playback at once (the playback side gets silence, the capture side is discarded):
            // a load test of the shared xHCI event ring and both isochronous endpoints.
            let seconds = words.next().and_then(|word| word.parse::<u64>().ok()).unwrap_or(5).clamp(1, 60);
            let rate = words.next().and_then(|word| word.parse::<u32>().ok()).unwrap_or(48_000);
            if let Err(reason) = ua::start(rate, true, true) {
                return crate::println!("audio: {reason}.");
            }
            let end = crate::timer::ticks() + seconds * crate::timer::TICK_HZ;
            let mut scratch = [0u8; 4096];
            let silence = [0u8; 2048];
            while crate::timer::ticks() < end {
                pump();
                while ua::read(&mut scratch) > 0 {}
                if ua::playback_space() >= silence.len() {
                    ua::write(&silence);
                }
            }
            ua::status();
            ua::stop();
        }
        Some("tone") => {
            let frequency = words.next().and_then(|word| word.parse::<u32>().ok()).unwrap_or(440).clamp(20, 20_000);
            let seconds = words.next().and_then(|word| word.parse::<u64>().ok()).unwrap_or(3).clamp(1, 60);
            let rate = words.next().and_then(|word| word.parse::<u32>().ok()).unwrap_or(48_000);
            if let Err(reason) = ua::start(rate, false, true) {
                return crate::println!("audio: {reason}.");
            }
            let Some((channels, slot)) = ua::playback_format() else { return };
            let (channels, slot) = (usize::from(channels), usize::from(slot));
            // A phase accumulator in 1/65536 of a cycle; a parabola per half cycle stands in for a sine
            // (integer-only, and plenty to hear or to measure the fundamental).
            let step = (u64::from(frequency) << 16) / u64::from(rate);
            let mut phase = 0u64;
            let end = crate::timer::ticks() + seconds * crate::timer::TICK_HZ;
            let frame_bytes = channels * slot;
            let mut chunk = [0u8; 2048];
            while crate::timer::ticks() < end {
                pump();
                // Only as many frames as the ring has room for, so the tone never has a gap.
                let frames = (chunk.len() / frame_bytes).min(ua::playback_space() / frame_bytes);
                if frames == 0 {
                    continue;
                }
                for frame in chunk[..frames * frame_bytes].chunks_mut(frame_bytes) {
                    let half = (phase & 0x7fff) as i64;
                    let magnitude = (half * (0x8000 - half)) >> 13; // 0..32768
                    let sample = (if phase & 0x8000 != 0 { -magnitude } else { magnitude }) / 4; // a quarter of full scale
                    phase = (phase + step) & 0xffff;
                    let value = sample << (8 * slot as u32 - 16); // scale 16-bit to the slot's width
                    for sample_slot in frame.chunks_mut(slot) {
                        for (index, byte) in sample_slot.iter_mut().enumerate() {
                            *byte = (value >> (8 * index)) as u8;
                        }
                    }
                }
                ua::write(&chunk[..frames * frame_bytes]);
            }
            ua::status();
            ua::stop();
        }
        Some(_) => crate::println!("Usage: audio | audio device N|auto | audio modes | audio use capture|playback N|auto | audio start RATE [capture|playback|both] | audio stop | audio capture SECONDS [RATE] | audio tone [HZ] [SECONDS] [RATE] | audio duplex [SECONDS] [RATE]"),
    }
}

/// One pass of the housekeeping every wait loop needs: pet the watchdog, serve USB events, move the audio rings.
fn pump() {
    imp::housekeeping();
    crate::audio::poll();
}

#[cfg(target_arch = "x86_64")]
mod imp {
    pub fn housekeeping() {
        crate::debug::pet();
        crate::usb_hid::poll();
    }

    pub fn flush() {
        crate::term::flush_output();
    }
}

#[cfg(target_arch = "aarch64")]
mod imp {
    pub fn housekeeping() {
        crate::usb_kbd::poll();
    }

    pub fn flush() {}
}
