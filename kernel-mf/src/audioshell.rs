//! The `audio` shell command of the server kernel: `audio` (devices and state), `device`, `output`, `modes`, `use`, `start`,
//! `stop`, `capture`, `duplex` and `tone`, as in the embedded kernel's `audioshell.rs`, on `kernel_core::audio`; `capture`
//! here also takes `tone=HZ` to play while it listens (a cable loop check) and gives peaks in 1/32768 of full scale. The tone is a
//! parabola per half cycle (integer only), written only as fast as the ring has room, so any gap in the sound is an underrun
//! the driver counts (`audio` prints the totals afterwards).

use crate::time;
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
        Some("output") => {
            let choice = match words.next() {
                Some("same") | Some("auto") | None => Ok(None),
                Some(text) => ua::parse_device(text).map(Some).ok_or(()),
            };
            match choice {
                Ok(number) => match ua::choose_output(number) {
                    Ok(()) => ua::status(),
                    Err(reason) => println!("audio: {reason}."),
                },
                Err(()) => println!("Usage: audio output N|same   (`audio` lists the devices)"),
            }
        }
        Some("modes") => ua::print_modes(),
        Some("use") => {
            let usage = "Usage: audio use capture|playback N|auto   (`audio modes` lists the numbers)";
            let capture = match words.next() {
                Some("capture") => true,
                Some("playback") => false,
                _ => return println!("{usage}"),
            };
            let mode = match words.next() {
                Some("auto") => 0,
                Some(word) => match word.parse::<u32>() {
                    Ok(number) => number,
                    Err(_) => return println!("{usage}"),
                },
                None => return println!("{usage}"),
            };
            match ua::select(capture, mode) {
                Ok(()) => println!("{} mode: {}.", if capture { "Capture" } else { "Playback" }, if mode == 0 { "automatic" } else { "picked (`audio modes` shows which)" }),
                Err(reason) => println!("audio: {reason}."),
            }
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
                Err(reason) => println!("audio: {reason}."),
            }
        }
        Some("stop") => {
            ua::stop_mine();
            ua::status();
        }
        Some("capture") => capture(&mut words),
        Some("duplex") => duplex(&mut words),
        Some("tone") => tone(&mut words),
        Some(_) => println!("Usage: audio | audio device N|auto | audio output N|same | audio modes | audio use capture|playback N|auto | audio start RATE [capture|playback|both] | audio stop | audio capture [SECONDS] [RATE] [tone=HZ[/HZ]] | audio duplex [SECONDS] [RATE] | audio tone [HZ[/HZ]] [SECONDS|0] [RATE]"),
    }
}

/// One pass of what every wait loop here needs: the watchdog, the USB controllers, the audio rings.
fn pump() {
    crate::debug::pet();
    crate::usb::poll();
    ua::poll();
}

/// `HZ` or `LEFT/RIGHT`, each clamped to 20..20000 Hz.
fn parse_tones(word: &str) -> Option<(u32, u32)> {
    let mut tones = word.split('/');
    let parse_hz = |word: Option<&str>| word.and_then(|word| word.parse::<u32>().ok()).map(|hz| hz.clamp(20, 20_000));
    let left = parse_hz(tones.next())?;
    let right = match tones.next() {
        Some(word) => parse_hz(Some(word))?,
        None => left,
    };
    Some((left, right))
}

/// Two tones, one per channel (even channels left, odd right), at a quarter of full scale: a phase accumulator in 1/65536
/// of a cycle and a parabola per half cycle (integer only, plenty to hear or to measure).
struct Tone {
    steps: [u64; 2],
    phases: [u64; 2],
    slot: usize,
    float: bool,
}

impl Tone {
    fn new((left, right): (u32, u32), rate: u32, slot: usize, float: bool) -> Tone {
        Tone { steps: [(u64::from(left) << 16) / u64::from(rate), (u64::from(right) << 16) / u64::from(rate)], phases: [0; 2], slot, float }
    }

    /// Fills `out` with whole frames of `frame_bytes`.
    fn fill(&mut self, out: &mut [u8], frame_bytes: usize) {
        let slot = self.slot;
        for frame in out.chunks_exact_mut(frame_bytes) {
            let mut samples = [0i64; 2];
            for side in 0..2 {
                let phase = self.phases[side];
                let half = (phase & 0x7fff) as i64;
                let magnitude = (half * (0x8000 - half)) >> 13;
                let sample = (if phase & 0x8000 != 0 { -magnitude } else { magnitude }) / 4;
                self.phases[side] = (phase + self.steps[side]) & 0xffff;
                samples[side] = if self.float { i64::from(float_bits(sample as i32)) } else { sample << (8 * slot as u32 - 16) };
            }
            for (channel, sample_slot) in frame.chunks_mut(slot).enumerate() {
                let value = samples[channel & 1];
                for (index, byte) in sample_slot.iter_mut().enumerate() {
                    *byte = (value >> (8 * index)) as u8;
                }
            }
        }
    }

    /// Writes as many frames as the playback ring has room for, so the tone never has a gap of its own making.
    fn feed(&mut self, chunk: &mut [u8], frame_bytes: usize) {
        let frames = (chunk.len() / frame_bytes).min(ua::playback_space() / frame_bytes);
        if frames > 0 {
            self.fill(&mut chunk[..frames * frame_bytes], frame_bytes);
            ua::write(&chunk[..frames * frame_bytes]);
        }
    }
}

/// `audio capture [SECONDS] [RATE] [tone=HZ[/HZ]]`: records for a few seconds and prints, per second, the frames and the peak
/// of every channel in 1/32768 of full scale (whatever the sample width), so a device's inputs can be checked without a
/// tape. With `tone=`, the output plays a tone meanwhile: with outputs cabled to inputs, the cabled channels show it.
fn capture(words: &mut core::str::SplitWhitespace) {
    let usage = "Usage: audio capture [SECONDS] [RATE] [tone=HZ[/HZ]]";
    let (mut seconds, mut rate, mut tones) = (3u64, 48_000u32, None);
    for (position, word) in words.enumerate() {
        if let Some(value) = word.strip_prefix("tone=") {
            let Some(pair) = parse_tones(value) else { return println!("{usage}") };
            tones = Some(pair);
        } else if let Ok(number) = word.parse::<u32>() {
            if position == 0 { seconds = u64::from(number).clamp(1, 30) } else { rate = number }
        } else {
            return println!("{usage}");
        }
    }
    if let Err(reason) = ua::start(rate, true, tones.is_some()) {
        return println!("audio: {reason}.");
    }
    let Some((channels, slot)) = ua::capture_format() else { return ua::stop_mine() };
    let (channels, slot) = (usize::from(channels), usize::from(slot));
    let float = ua::capture_float() && slot == 4;
    let frame_bytes = channels * slot;
    let mut tone = tones.and_then(|pair| {
        let (out_channels, out_slot) = ua::playback_format()?;
        let out_slot = usize::from(out_slot);
        Some((Tone::new(pair, rate, out_slot, ua::playback_float() && out_slot == 4), usize::from(out_channels) * out_slot))
    });
    let mut chunk = [0u8; 4096];
    let mut data = [0u8; 8192];
    let mut carry = 0;
    for second in 1..=seconds {
        let mut peaks = [0u32; 32];
        let mut frames = 0u64;
        let end = time::micros() + 1_000_000;
        while time::micros() < end {
            pump();
            if let Some((tone, out_frame)) = tone.as_mut() {
                tone.feed(&mut chunk, *out_frame);
            }
            let room = (data.len() - carry) / frame_bytes * frame_bytes;
            let total = carry + ua::read(&mut data[carry..carry + room]);
            let whole = total / frame_bytes * frame_bytes;
            for frame in data[..whole].chunks(frame_bytes) {
                for (channel, sample) in frame.chunks(slot).enumerate().take(32) {
                    peaks[channel] = peaks[channel].max(peak(sample, float));
                }
                frames += 1;
            }
            data.copy_within(whole..total, 0);
            carry = total - whole;
        }
        let shown = channels.min(32);
        let loudest = (0..shown).max_by_key(|&channel| peaks[channel]).unwrap_or(0);
        println!("[{second:>2} s] {frames} frames; loudest channel {} (peak {})", loudest + 1, peaks[loudest]);
        for (line, group) in peaks[..shown].chunks(8).enumerate() {
            let values: alloc::vec::Vec<_> = group.iter().map(|peak| alloc::format!("{peak:>6}")).collect();
            println!("   ch {:>2}-{:>2}: {}", line * 8 + 1, line * 8 + group.len(), values.join(" "));
        }
    }
    ua::status();
    ua::stop_mine();
}

/// The magnitude of one little-endian sample in 1/32768 of full scale: signed PCM of the slot's width, or an IEEE single.
fn peak(sample: &[u8], float: bool) -> u32 {
    if float {
        let bits = u32::from_le_bytes([sample[0], sample[1], sample[2], sample[3]]);
        return float_peak(bits);
    }
    let mut value = 0i64;
    for (index, &byte) in sample.iter().enumerate() {
        value |= i64::from(byte) << (8 * index);
    }
    let bits = 8 * sample.len() as u32;
    if value >> (bits - 1) != 0 {
        value -= 1 << bits;
    }
    (value.unsigned_abs() >> (bits - 16)) as u32
}

/// `audio duplex [SECONDS] [RATE]`: capture and playback at once, silence out and the input discarded, then the counters.
fn duplex(words: &mut core::str::SplitWhitespace) {
    let seconds = words.next().and_then(|word| word.parse::<u64>().ok()).unwrap_or(5).clamp(1, 60);
    let rate = words.next().and_then(|word| word.parse::<u32>().ok()).unwrap_or(48_000);
    if let Err(reason) = ua::start(rate, true, true) {
        return println!("audio: {reason}.");
    }
    let end = time::micros() + seconds * 1_000_000;
    let mut scratch = [0u8; 4096];
    let silence = [0u8; 2048];
    while time::micros() < end {
        pump();
        while ua::read(&mut scratch) > 0 {}
        let room = ua::playback_space().min(silence.len());
        if room > 0 {
            ua::write(&silence[..room]);
        }
    }
    ua::status();
    ua::stop_mine();
}

fn tone(words: &mut core::str::SplitWhitespace) {
    let usage = "Usage: audio tone [HZ[/HZ]] [SECONDS|0] [RATE]";
    let Some(pair) = parse_tones(words.next().unwrap_or("440")) else { return println!("{usage}") };
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
    let mut tone = Tone::new(pair, rate, slot, ua::playback_float() && slot == 4);
    let end = time::micros() + seconds * 1_000_000;
    let mut chunk = [0u8; 2048];
    // No print from `start` to the end of the loop: a console line costs milliseconds, and the ring would run dry meanwhile.
    TONE_KEY.store(false, Ordering::Relaxed);
    TONE_UNTIL_KEY.store(forever, Ordering::Relaxed);
    while if forever { !TONE_KEY.load(Ordering::Relaxed) } else { time::micros() < end } {
        pump();
        if forever {
            crate::ps2::poll();
            if crate::serial::read_byte().is_some() {
                break;
            }
        }
        tone.feed(&mut chunk, channels * slot);
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

/// |float| in 1/32768 of full scale, from the bit pattern: exponent and mantissa shifted, no float arithmetic.
fn float_peak(bits: u32) -> u32 {
    let exponent = ((bits >> 23) & 0xff) as i32;
    if exponent < 127 - 15 - 1 {
        return 0;
    }
    let mantissa = (bits & 0x7f_ffff) | 0x80_0000;
    let shift = exponent - 127 + 15 - 23;
    if shift >= 0 { mantissa.checked_shl(shift as u32).unwrap_or(u32::MAX) } else { mantissa >> (-shift).min(31) as u32 }
}
