//! The `audio` shell command of the server kernel: `audio` (devices and state), `audio device N|auto`, `audio tone [HZ]
//! [SECONDS] [RATE]`. The same three as the embedded kernel's `audioshell.rs`, on `kernel_core::audio`. The tone is a
//! parabola per half cycle (integer only), written only as fast as the ring has room, so any gap in the sound is an underrun
//! the driver counts (`audio` prints the totals afterwards).

use crate::apic;
use kernel_core::audio as ua;

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
        Some(_) => println!("Usage: audio | audio device N|auto | audio tone [HZ] [SECONDS] [RATE]"),
    }
}

fn tone(words: &mut core::str::SplitWhitespace) {
    let frequency = words.next().and_then(|word| word.parse::<u32>().ok()).unwrap_or(440).clamp(20, 20_000);
    let seconds = words.next().and_then(|word| word.parse::<u64>().ok()).unwrap_or(3).clamp(1, 60);
    let rate = words.next().and_then(|word| word.parse::<u32>().ok()).unwrap_or(48_000);
    println!("audio: starting {rate} Hz playback...");
    if let Err(reason) = ua::start(rate, false, true) {
        return println!("audio: {reason}.");
    }
    let Some((channels, slot)) = ua::playback_format() else { return };
    let (channels, slot) = (usize::from(channels), usize::from(slot));
    let step = (u64::from(frequency) << 16) / u64::from(rate);
    let mut phase = 0u64;
    let end = apic::micros() + seconds * 1_000_000;
    let frame_bytes = channels * slot;
    let float = ua::playback_float() && slot == 4;
    let mut chunk = [0u8; 2048];
    // No print from `start` to the end of the loop: a console line costs milliseconds, and the ring would run dry meanwhile.
    while apic::micros() < end {
        crate::debug::pet();
        crate::usb::poll();
        ua::poll();
        let frames = (chunk.len() / frame_bytes).min(ua::playback_space() / frame_bytes);
        if frames == 0 {
            continue;
        }
        for frame in chunk[..frames * frame_bytes].chunks_mut(frame_bytes) {
            let half = (phase & 0x7fff) as i64;
            let magnitude = (half * (0x8000 - half)) >> 13;
            let sample = (if phase & 0x8000 != 0 { -magnitude } else { magnitude }) / 4;
            phase = (phase + step) & 0xffff;
            let value = if float { i64::from(float_bits(sample as i32)) } else { sample << (8 * slot as u32 - 16) };
            for sample_slot in frame.chunks_mut(slot) {
                for (index, byte) in sample_slot.iter_mut().enumerate() {
                    *byte = (value >> (8 * index)) as u8;
                }
            }
        }
        ua::write(&chunk[..frames * frame_bytes]);
    }
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
