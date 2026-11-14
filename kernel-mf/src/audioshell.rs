//! The `audio` shell command of the server kernel: `audio` (devices and state), `audio device N|auto`, `audio tone [HZ[/HZ]]
//! [SECONDS|0] [RATE]`. The same three as the embedded kernel's `audioshell.rs`, on `kernel_core::audio`. The tone is a
//! parabola per half cycle (integer only), written only as fast as the ring has room, so any gap in the sound is an underrun
//! the driver counts (`audio` prints the totals afterwards).

use crate::apic;
use core::sync::atomic::{AtomicBool, Ordering};
use kernel_core::audio as ua;

/// Set while `audio tone ... 0` plays until a key: `shell::feed` then stops the tone with the byte instead of editing a line.
static TONE_UNTIL_KEY: AtomicBool = AtomicBool::new(false);
static TONE_KEY: AtomicBool = AtomicBool::new(false);

/// A typed byte while a tone runs until a key: true when the tone took it.
pub fn key(_byte: u8) -> bool {
    if !TONE_UNTIL_KEY.load(Ordering::Relaxed) {
        return false;
    }
    TONE_KEY.store(true, Ordering::Relaxed);
    true
}

/// The USB drivers wait on timer ticks, which only advance with interrupts on, and the shell runs with them off: turn them on
/// for the command, on CPU 0 (where the drivers' interrupt handlers run) and under the USB lock, as a tape's audio calls do.
pub fn command(words: core::str::SplitWhitespace) {
    crate::sched::pin_current(0);
    let _usb = crate::usb::lock();
    let was_on = crate::cpu::interrupts_save_and_disable();
    crate::cpu::interrupts_enable();
    run(words);
    if !was_on {
        crate::cpu::interrupts_disable();
    }
}

fn run(mut words: core::str::SplitWhitespace) {
    match words.next() {
        None => ua::status(),
        Some("device") => {
            let choice = match words.next() {
                Some("auto") => Ok(None),
                Some(text) => ua::parse_device(text).map(Some).ok_or(()),
                None => Err(()),
            };
            match choice {
                Ok(number) => match ua::choose(number) {
                    Ok(()) => ua::status(),
                    Err(reason) => println!("audio: {reason}."),
                },
                Err(()) => println!("Usage: audio device N|auto"),
            }
        }
        Some("tone") => tone(&mut words),
        Some(_) => println!("Usage: audio | audio device N|auto | audio tone [HZ[/HZ]] [SECONDS|0] [RATE]"),
    }
}

fn tone(words: &mut core::str::SplitWhitespace) {
    // HZ or LEFT/RIGHT: two tones, one per channel (even channels left, odd right).
    let usage = "Usage: audio tone [HZ[/HZ]] [SECONDS|0] [RATE]";
    let mut tones = words.next().unwrap_or("440").split('/');
    let parse_hz = |word: Option<&str>| word.and_then(|word| word.parse::<u32>().ok()).map(|hz| hz.clamp(20, 20_000));
    let Some(left) = parse_hz(tones.next()) else { return println!("{usage}") };
    let right = match tones.next() {
        Some(word) => match parse_hz(Some(word)) {
            Some(hz) => hz,
            None => return println!("{usage}"),
        },
        None => left,
    };
    // 0 seconds: play until a key is typed.
    let seconds = words.next().and_then(|word| word.parse::<u64>().ok()).unwrap_or(3).min(3600);
    let forever = seconds == 0;
    let rate = words.next().and_then(|word| word.parse::<u32>().ok()).unwrap_or(48_000);
    println!("audio: starting {rate} Hz playback...");
    if let Err(reason) = ua::start(rate, false, true) {
        return println!("audio: {reason}.");
    }
    let Some((channels, slot)) = ua::playback_format() else { return };
    let (channels, slot) = (usize::from(channels), usize::from(slot));
    let steps = [(u64::from(left) << 16) / u64::from(rate), (u64::from(right) << 16) / u64::from(rate)];
    let mut phases = [0u64; 2];
    let end = apic::micros() + seconds * 1_000_000;
    let frame_bytes = channels * slot;
    let float = ua::playback_float() && slot == 4;
    let mut chunk = [0u8; 2048];
    // No print from `start` to the end of the loop: a console line costs milliseconds, and the ring would run dry meanwhile.
    TONE_KEY.store(false, Ordering::Relaxed);
    TONE_UNTIL_KEY.store(forever, Ordering::Relaxed);
    while if forever { !TONE_KEY.load(Ordering::Relaxed) } else { apic::micros() < end } {
        crate::debug::pet();
        crate::usb::poll();
        if forever {
            crate::ps2::poll();
            if crate::serial::read_byte().is_some() {
                break;
            }
        }
        ua::poll();
        let frames = (chunk.len() / frame_bytes).min(ua::playback_space() / frame_bytes);
        if frames == 0 {
            continue;
        }
        for frame in chunk[..frames * frame_bytes].chunks_mut(frame_bytes) {
            let mut samples = [0i64; 2];
            for side in 0..2 {
                let phase = phases[side];
                let half = (phase & 0x7fff) as i64;
                let magnitude = (half * (0x8000 - half)) >> 13;
                let sample = (if phase & 0x8000 != 0 { -magnitude } else { magnitude }) / 4;
                phases[side] = (phase + steps[side]) & 0xffff;
                samples[side] = if float { i64::from(float_bits(sample as i32)) } else { sample << (8 * slot as u32 - 16) };
            }
            for (channel, sample_slot) in frame.chunks_mut(slot).enumerate() {
                let value = samples[channel & 1];
                for (index, byte) in sample_slot.iter_mut().enumerate() {
                    *byte = (value >> (8 * index)) as u8;
                }
            }
        }
        ua::write(&chunk[..frames * frame_bytes]);
    }
    TONE_UNTIL_KEY.store(false, Ordering::Relaxed);
    println!("audio: {channels} channels, {slot} bytes per sample");
    ua::status();
    ua::stop_mine();
}

/// The bit pattern of `sample / 32768` as an IEEE single, by integer arithmetic.
fn float_bits(sample: i32) -> u32 {
    if sample == 0 {
        return 0;
    }
    let magnitude = sample.unsigned_abs();
    let top = 31 - magnitude.leading_zeros();
    let mantissa = if top <= 23 { magnitude << (23 - top) } else { magnitude >> (top - 23) } & 0x7f_ffff;
    let exponent = 127 + top - 15;
    (u32::from(sample < 0) << 31) | (exponent << 23) | mantissa
}
