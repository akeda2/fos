//! I2S on the Pi 4's PCM block (BCM2711, aarch64 only) for the Maltex audio HAT: an I2S *master* (it makes the 3.072 MHz bit
//! clock and the 48 kHz frame clock; the Pi is the slave) with 24-bit audio left-justified in 32-bit slots, two channels.
//! Pins (alt0): GPIO18 PCM_CLK (bit clock), GPIO19 PCM_FS (frame clock), GPIO20 PCM_DIN (card to Pi), GPIO21 PCM_DOUT (Pi to card,
//! only switched to PCM while playback runs). GPIO16 selects the card's input (low AES3, high analog; jumper 4-5).
//!
//! Two faces:
//! * the audio backend (`audio.rs`, `Backend::I2s`): `present`, `start`, `read`, `write`, `fill_info` ... like `usb_audio` and
//!   `hda`. Nothing probes the HAT, so it is only offered once switched on: `i2s audio on` or `i2s-audio` in `settings`.
//!   Format: 48000 Hz, 2 channels, 24 bits left-justified in 4-byte slots (what the tapes call `bits=24`, slot 4).
//! * diagnostics: `i2s probe` counts edges on the three input pins with the pins left alone (is the card clocking?); `i2s
//!   capture [SECONDS] [clki=0|1] [fsi=0|1]` receives and prints per second the level of each channel and whether the low byte of
//!   every word is zero (~100% when the bit alignment is right).
//!
//! PCM registers (base 0xFE203000, Linux `bcm2835-i2s.c`): CS_A 0x00 (EN b0, RXON b1, TXON b2, TXCLR b3, RXCLR b4, DMAEN b9,
//! RXERR b16, RXSEX b23 sign extend, STBY b25), FIFO_A 0x04, MODE_A 0x08 (FSLEN 9:0, FLEN 19:10 = frame bits - 1, FSI b20 invert
//! frame sync, FSM b21 frame sync slave, CLKI b22 invert bit clock, CLKM b23 clock slave), RXC_A 0x0c and TXC_A 0x10 (per channel:
//! WID 3:0, POS 13:4, EN b14, WEX b15; channel 1 is the same at bit 16 up; width = WID + 8 + 16*WEX), DREQ_A 0x14 (RX_REQ 6:0,
//! TX_REQ 14:8, RX_PANIC 22:16, TX_PANIC 30:24; Linux's values). I2S data starts one bit clock after the frame edge: channel 1 at
//! position 1, channel 2 at 33. This card needs the frame clock inverted (`fsi=1`, the default), found on the real Pi.
//!
//! DMA (legacy controller 0xFE007000; Linux's bcm2711 mask leaves 0, 2, 4, 5 and the high channels to the ARM): channel 5
//! drains the RX FIFO (DREQ 3), channel 4 fills the TX FIFO (DREQ 2). Each has two control blocks that loop over a 64 KiB ring
//! in the non-cacheable pool. A control block is 8 words: TI (PERMAP 20:16, DEST_DREQ b6, SRC_INC b8 / SRC_DREQ b10, DEST_INC
//! b4, WAIT_RESP b3), source, destination, length, stride, next, 2 reserved; addresses are *bus* addresses: RAM at 0xC0000000 +
//! physical (so the pool must lie in the first GiB), the PCM FIFO at 0x7E203004. Channel registers: CS 0x00, CONBLK_AD 0x04,
//! SOURCE_AD 0x0c, DEST_AD 0x10. No interrupt is used: the position is read back from DEST_AD (RX) / SOURCE_AD (TX), and
//! turned into a running byte count each time something calls in (`read`, `write`, `poll`). That count only survives if it
//! is looked at at least once per ring revolution (170 ms), which the tapes' loops do.
//!
//! Capture drops the oldest half of the ring when the reader is late (counted as overrun bytes). Playback zeroes each part of
//! its ring once the DMA has passed it, so a late writer is heard as silence rather than a replay of old audio (counted as an
//! underrun). The first write lands 4 KiB (about 10 ms) ahead of the DMA.
//!
//! Unverified until it runs on the Pi: everything on the TX side (playback needs the card's jumper 8-9 removed to be heard).

use crate::{gpio, memory, paging, timer};
use core::ptr::{read_volatile, write_volatile};
use fosfor_abi::tape::{AudioInfo, AudioStats};

const PCM: u64 = 0xfe20_3000;
const DMA: u64 = 0xfe00_7000;
const RX_CHANNEL: u64 = 5;
const TX_CHANNEL: u64 = 4;
const DMA_ENABLE: u64 = DMA + 0xff0;
const PCM_FIFO_BUS: u32 = 0x7e20_3004;
const RAM_BUS: u64 = 0xc000_0000;
const RING_BYTES: u64 = 64 * 1024;
const HALF: u64 = RING_BYTES / 2;
const FRAME: u64 = 8;
const RATE: u32 = 48_000;
/// Where the first written audio lands relative to the DMA, and where it resumes after an underrun.
const PRIME: u64 = 4096;
const INPUT_SELECT: usize = 16;
const PINS: [usize; 3] = [18, 19, 20];
const PIN_OUT: usize = 21;

static mut RING: u64 = 0;
static mut RUN: Run = Run {
    enabled: false,
    rx: false,
    tx: false,
    rx_last: 0,
    rx_total: 0,
    rx_read: 0,
    rx_dropped: 0,
    tx_last: 0,
    tx_consumed: 0,
    tx_written: 0,
    tx_underruns: 0,
    input: 0,
};

struct Run {
    /// Offered as an audio device.
    enabled: bool,
    rx: bool,
    tx: bool,
    /// Capture: ring offset last seen, bytes the DMA has delivered, bytes the reader has taken, bytes dropped.
    rx_last: u64,
    rx_total: u64,
    rx_read: u64,
    rx_dropped: u64,
    /// Playback: ring offset last seen, bytes the DMA has sent, bytes written by the user (including the priming silence).
    tx_last: u64,
    tx_consumed: u64,
    tx_written: u64,
    tx_underruns: u64,
    /// Input select shown by `audio modes`: 0 untouched, 1 AES3, 2 analog.
    input: u8,
}

fn run() -> &'static mut Run {
    unsafe { &mut *core::ptr::addr_of_mut!(RUN) }
}

fn pcm(offset: u64) -> *mut u32 {
    (PCM + offset) as *mut u32
}

fn dma(channel: u64, offset: u64) -> *mut u32 {
    (DMA + channel * 0x100 + offset) as *mut u32
}

fn bus(physical: u64) -> u32 {
    (physical | RAM_BUS) as u32
}

#[repr(C, align(32))]
struct ControlBlock {
    info: u32,
    source: u32,
    destination: u32,
    length: u32,
    stride: u32,
    next: u32,
    reserved: [u32; 2],
}

/// The control blocks (RX at +0 and +32, TX at +64 and +96), then the RX ring and the TX ring, allocated once from the
/// non-cacheable pool.
fn ring() -> Result<u64, &'static str> {
    let existing = unsafe { *core::ptr::addr_of!(RING) };
    if existing != 0 {
        return Ok(existing);
    }
    let base = memory::allocate_contiguous(1 + 2 * RING_BYTES / 4096, Some(1 << 32)).ok_or("no DMA memory")?;
    if base + 4096 + 2 * RING_BYTES > 0x4000_0000 {
        return Err("the DMA pool lies above 1 GiB, out of the legacy DMA controller's reach");
    }
    unsafe { *core::ptr::addr_of_mut!(RING) = base };
    Ok(base)
}

fn rx_ring(base: u64) -> u64 {
    base + 4096
}

fn tx_ring(base: u64) -> u64 {
    base + 4096 + RING_BYTES
}

/// `i2s probe`: counts level changes of the clock and data pins for 20 ms, pins untouched.
pub fn probe() {
    let mut changes = [0u32; 3];
    let mut highs = [0u32; 3];
    let mut samples = 0u32;
    let mut last = gpio::levels();
    let start = timer::micros();
    while timer::micros() - start < 20_000 {
        let now = gpio::levels();
        for (index, pin) in PINS.iter().enumerate() {
            changes[index] += ((now ^ last) >> pin & 1) as u32;
            highs[index] += (now >> pin & 1) as u32;
        }
        last = now;
        samples += 1;
    }
    crate::println!("{samples} samples in 20 ms (about {} MHz).", samples / 20_000);
    for (index, name) in ["GPIO18 bit clock", "GPIO19 frame clock", "GPIO20 data in"].iter().enumerate() {
        crate::println!("  {name}: {} changes, high {}%", changes[index], highs[index] * 100 / samples.max(1));
    }
    crate::println!("A running card shows many changes on 18 (undersampled, still thousands) and about 1920 edges on 19 in 20 ms (48 kHz).");
}

fn pins_off() {
    for pin in PINS.into_iter().chain([PIN_OUT]) {
        let _ = gpio::set_function(pin, 0, false);
    }
}

/// Brings up PCM and the DMA channels for the directions asked for.
fn hw_start(rx: bool, tx: bool, invert_clock: bool, invert_frame: bool) -> Result<(), &'static str> {
    if run().rx || run().tx {
        return Err("already running");
    }
    if !gpio::available() {
        return Err("no PCM block here");
    }
    let base = ring()?;
    paging::map_device(PCM, 0x1000);
    paging::map_device(DMA, 0x1000);
    for pin in PINS.into_iter().chain(tx.then_some(PIN_OUT)) {
        if gpio::set_function(pin, 2, false).is_err() {
            pins_off();
            return Err("the PCM pins cannot be switched to the PCM function here");
        }
    }
    let (rx_buffer, tx_buffer) = (rx_ring(base), tx_ring(base));
    let rx_info = (3 << 16) | (1 << 10) | (1 << 4) | (1 << 3); // PERMAP 3 (PCM RX), SRC_DREQ, DEST_INC, WAIT_RESP
    let tx_info = (2 << 16) | (1 << 6) | (1 << 8) | (1 << 3); // PERMAP 2 (PCM TX), DEST_DREQ, SRC_INC, WAIT_RESP
    let blocks = base as *mut ControlBlock;
    unsafe {
        core::ptr::write_bytes(tx_buffer as *mut u8, 0, RING_BYTES as usize);
        for half in 0..2u64 {
            blocks.add(half as usize).write_volatile(ControlBlock {
                info: rx_info,
                source: PCM_FIFO_BUS,
                destination: bus(rx_buffer + half * HALF),
                length: HALF as u32,
                stride: 0,
                next: bus(base + (1 - half) * 32),
                reserved: [0; 2],
            });
            blocks.add(2 + half as usize).write_volatile(ControlBlock {
                info: tx_info,
                source: bus(tx_buffer + half * HALF),
                destination: PCM_FIFO_BUS,
                length: HALF as u32,
                stride: 0,
                next: bus(base + 64 + (1 - half) * 32),
                reserved: [0; 2],
            });
        }
        write_volatile(pcm(0x00), 1); // EN
        write_volatile(pcm(0x00), 1 | (1 << 3) | (1 << 4)); // clear both FIFOs
        let mode = (63 << 10) | 32 | (1 << 21) | (1 << 23) | u32::from(invert_frame) << 20 | u32::from(invert_clock) << 22;
        write_volatile(pcm(0x08), mode);
        // Two channels, 32 bits each (WID 8 + WEX 1), at positions 1 and 33, the same for both directions.
        let channel = |position: u32| (1 << 14) | (position << 4) | (1 << 15) | 8;
        let channels = (channel(1) << 16) | channel(33);
        write_volatile(pcm(0x0c), channels);
        write_volatile(pcm(0x10), channels);
        write_volatile(pcm(0x14), 0x20 | (0x30 << 8) | (0x30 << 16) | (0x10 << 24)); // RX 32/48, TX 48/16 words
        let common = 1 | (1 << 23) | (1 << 25) | (1 << 9); // EN, RXSEX, STBY, DMAEN
        write_volatile(pcm(0x00), common);
        let wait = timer::micros();
        while timer::micros() - wait < 100 {}
        write_volatile(pcm(0x00), common | (1 << 3) | (1 << 4));
        for (active, channel, blocks_at) in [(rx, RX_CHANNEL, base), (tx, TX_CHANNEL, base + 64)] {
            if !active {
                continue;
            }
            write_volatile(DMA_ENABLE as *mut u32, read_volatile(DMA_ENABLE as *const u32) | 1 << channel);
            write_volatile(dma(channel, 0x00), 1 << 31); // RESET
            let wait = timer::micros();
            while read_volatile(dma(channel, 0x00)) & (1 << 31) != 0 && timer::micros() - wait < 1000 {}
            write_volatile(dma(channel, 0x04), bus(blocks_at));
            write_volatile(dma(channel, 0x00), 1 | (1 << 28) | (8 << 16) | (8 << 20)); // ACTIVE, wait for writes, priorities
        }
        write_volatile(pcm(0x00), common | if rx { 1 << 1 } else { 0 } | if tx { 1 << 2 } else { 0 }); // RXON, TXON
    }
    let state = run();
    state.rx = rx;
    state.tx = tx;
    state.rx_last = 0;
    state.rx_total = 0;
    state.rx_read = 0;
    state.rx_dropped = 0;
    state.tx_last = 0;
    state.tx_consumed = 0;
    state.tx_written = PRIME;
    state.tx_underruns = 0;
    Ok(())
}

fn hw_stop() {
    let state = run();
    if !state.rx && !state.tx {
        return;
    }
    unsafe {
        write_volatile(dma(RX_CHANNEL, 0x00), 1 << 31);
        write_volatile(dma(TX_CHANNEL, 0x00), 1 << 31);
        write_volatile(pcm(0x00), 1 | (1 << 3) | (1 << 4));
        write_volatile(pcm(0x00), 0);
    }
    pins_off();
    state.rx = false;
    state.tx = false;
}

/// Bytes the RX DMA has delivered since the start, brought up to date.
fn rx_sync() {
    let state = run();
    if !state.rx {
        return;
    }
    let base = unsafe { *core::ptr::addr_of!(RING) };
    let at = (unsafe { read_volatile(dma(RX_CHANNEL, 0x10)) } as u64 & 0x3fff_ffff).wrapping_sub(rx_ring(base) & 0x3fff_ffff);
    let offset = at % RING_BYTES & !(FRAME - 1);
    state.rx_total += (offset + RING_BYTES - state.rx_last) % RING_BYTES;
    state.rx_last = offset;
}

/// Bytes the TX DMA has sent since the start, brought up to date; what it has passed is zeroed so it plays silence if the
/// writer is late.
fn tx_sync() {
    let state = run();
    if !state.tx {
        return;
    }
    let base = unsafe { *core::ptr::addr_of!(RING) };
    let at = (unsafe { read_volatile(dma(TX_CHANNEL, 0x0c)) } as u64 & 0x3fff_ffff).wrapping_sub(tx_ring(base) & 0x3fff_ffff);
    let offset = at % RING_BYTES & !(FRAME - 1);
    let passed = (offset + RING_BYTES - state.tx_last) % RING_BYTES;
    zero(tx_ring(base), state.tx_last, passed);
    state.tx_consumed += passed;
    state.tx_last = offset;
}

fn zero(ring: u64, from: u64, length: u64) {
    let first = length.min(RING_BYTES - from);
    unsafe {
        core::ptr::write_bytes((ring + from) as *mut u8, 0, first as usize);
        core::ptr::write_bytes(ring as *mut u8, 0, (length - first) as usize);
    }
}

// ---- audio backend ------------------------------------------------------------------------------------------------------

pub fn present() -> bool {
    run().enabled && gpio::available()
}

/// The `i2s-audio [on|off]` setting and `i2s audio on|off`.
pub fn enable(on: bool) {
    if !on {
        hw_stop();
    }
    run().enabled = on;
}

pub fn apply_setting(line: &str) {
    let word = line.split_whitespace().nth(1).unwrap_or("on");
    enable(word != "off");
    crate::println!("settings: i2s-audio {word} (the Maltex HAT {} an audio device).", if word != "off" { "is" } else { "is not" });
}

pub fn label() -> &'static str {
    "I2S audio HAT (BCM2711 PCM)"
}

pub fn start(rate: u32, capture: bool, playback: bool) -> Result<(), &'static str> {
    if rate != RATE {
        return Err("the device does not offer that sample rate");
    }
    if !capture && !playback {
        return Ok(());
    }
    hw_start(capture, playback, false, true)
}

pub fn stop() {
    hw_stop();
}

pub fn poll() {
    rx_sync();
    tx_sync();
}

pub fn read(out: &mut [u8]) -> usize {
    rx_sync();
    let state = run();
    if !state.rx {
        return 0;
    }
    let mut available = state.rx_total - state.rx_read;
    if available > RING_BYTES - 2 * PRIME {
        // The reader was late: keep the newest half of the ring.
        state.rx_dropped += available - HALF;
        state.rx_read = state.rx_total - HALF;
        available = HALF;
    }
    let count = (out.len() as u64).min(available) & !(FRAME - 1);
    let buffer = rx_ring(unsafe { *core::ptr::addr_of!(RING) });
    let mut done = 0u64;
    while done < count {
        let at = (state.rx_read + done) % RING_BYTES;
        let chunk = (RING_BYTES - at).min(count - done);
        unsafe { core::ptr::copy_nonoverlapping((buffer + at) as *const u8, out[done as usize..].as_mut_ptr(), chunk as usize) };
        done += chunk;
    }
    state.rx_read += count;
    count as usize
}

pub fn playback_space() -> usize {
    tx_sync();
    let state = run();
    if !state.tx {
        return 0;
    }
    (RING_BYTES - state.tx_written.saturating_sub(state.tx_consumed).min(RING_BYTES)) as usize & !(FRAME as usize - 1)
}

pub fn write(data: &[u8]) -> usize {
    tx_sync();
    let state = run();
    if !state.tx {
        return 0;
    }
    if state.tx_written < state.tx_consumed {
        // The DMA has played past everything written (silence, thanks to `tx_sync`): resume a little ahead of it.
        state.tx_underruns += 1;
        state.tx_written = state.tx_consumed + PRIME;
    }
    let space = RING_BYTES - (state.tx_written - state.tx_consumed).min(RING_BYTES);
    let count = (data.len() as u64).min(space) & !(FRAME - 1);
    let buffer = tx_ring(unsafe { *core::ptr::addr_of!(RING) });
    let mut done = 0u64;
    while done < count {
        let at = (state.tx_written + done) % RING_BYTES;
        let chunk = (RING_BYTES - at).min(count - done);
        unsafe { core::ptr::copy_nonoverlapping(data[done as usize..].as_ptr(), (buffer + at) as *mut u8, chunk as usize) };
        done += chunk;
    }
    state.tx_written += count;
    count as usize
}

/// (channels, bytes per slot) of the running stream.
pub fn format(capture: bool) -> Option<(u8, u8)> {
    let state = run();
    (if capture { state.rx } else { state.tx }).then_some((2, 4))
}

/// `audio use capture N`: 0 and 1 AES3, 2 analog (GPIO16 low/high; the card's jumper 4-5 hands that choice to the Pi).
/// Playback has no choice.
pub fn select(capture: bool, mode: u32) -> Result<(), &'static str> {
    if !capture {
        return if mode == 0 { Ok(()) } else { Err("no such mode") };
    }
    if mode > 2 {
        return Err("no such mode (`audio modes` lists them)");
    }
    if mode == 0 {
        run().input = 0;
        return Ok(());
    }
    gpio::set_function(INPUT_SELECT, 1, mode == 2).map_err(|_| "GPIO16 cannot be driven here")?;
    run().input = mode as u8;
    Ok(())
}

pub fn print_modes() {
    crate::println!("Playback: 2 ch, 24-bit, 48000 Hz (the card's output jumper 8-9 must be open to hear it).");
    crate::println!("Capture modes (`audio use capture N`; GPIO16 selects, needs the card's jumper 4-5):");
    for (number, name) in [(1u8, "AES3 (GPIO16 low)"), (2, "analog (GPIO16 high)")] {
        crate::println!("  {}{number}: 2 ch, 24-bit, 48000 Hz, {name}", if run().input == number { '*' } else { ' ' });
    }
}

pub fn status() {
    rx_sync();
    tx_sync();
    let state = run();
    crate::println!("I2S audio HAT: 48000 Hz, 2 ch, 24-bit (clocks come from the card).");
    crate::println!(
        "  capture: {}, {} bytes, {} dropped; playback: {}, {} bytes sent, {} underruns",
        if state.rx { "running" } else { "stopped" },
        state.rx_total,
        state.rx_dropped,
        if state.tx { "running" } else { "stopped" },
        state.tx_consumed,
        state.tx_underruns
    );
    crate::println!("  frame clock (GPIO19) is {}, input {}", if gpio::levels() >> 19 & 1 != 0 { "high" } else { "low" }, match state.input {
        1 => "AES3",
        2 => "analog",
        _ => "as the card is set",
    });
}

pub fn fill_info(info: &mut AudioInfo) -> bool {
    if !present() {
        return false;
    }
    rx_sync();
    tx_sync();
    let state = run();
    let mut out = AudioInfo::default();
    out.playback_channels = 2;
    out.playback_slot_bytes = 4;
    out.playback_bits = 24;
    out.capture_channels = 2;
    out.capture_slot_bytes = 4;
    out.capture_bits = 24;
    out.rate_count = 1;
    out.rates[0] = RATE;
    if state.rx {
        out.capture_rate = RATE;
        out.capture_available = (state.rx_total - state.rx_read).min(RING_BYTES) as u32;
    }
    if state.tx {
        out.playback_rate = RATE;
        out.playback_space = playback_space() as u32;
    }
    *info = out;
    true
}

pub fn fill_stats(stats: &mut AudioStats) {
    rx_sync();
    tx_sync();
    let state = run();
    *stats = AudioStats {
        capture_packets: state.rx_total / FRAME,
        capture_bytes: state.rx_total,
        capture_errors: 0,
        capture_overruns: state.rx_dropped,
        playback_packets: state.tx_consumed / FRAME,
        playback_errors: 0,
        playback_underruns: state.tx_underruns,
    };
}

/// (ring size, bytes waiting) of the capture ring.
pub fn buffer_info() -> (u64, u64) {
    rx_sync();
    let state = run();
    if state.rx { (RING_BYTES, (state.rx_total - state.rx_read).min(RING_BYTES)) } else { (0, 0) }
}

// ---- diagnostics --------------------------------------------------------------------------------------------------------

fn level_text(peak: u32) -> u32 {
    if peak == 0 {
        return 0;
    }
    (24 - (32 - peak.leading_zeros())) * 6
}

/// `i2s capture [SECONDS] [clki=0|1] [fsi=0|1]`
pub fn capture(rest: &str) {
    let (mut seconds, mut invert_clock, mut invert_frame) = (3u64, false, true);
    for word in rest.split_whitespace() {
        match word {
            "clki=1" => invert_clock = true,
            "clki=0" => invert_clock = false,
            "fsi=1" => invert_frame = true,
            "fsi=0" => invert_frame = false,
            _ => match word.parse::<u64>() {
                Ok(number) if (1..=30).contains(&number) => seconds = number,
                _ => return crate::println!("Usage: i2s capture [SECONDS 1..30] [clki=0|1] [fsi=0|1]"),
            },
        }
    }
    if let Err(reason) = hw_start(true, false, invert_clock, invert_frame) {
        return crate::println!("i2s: {reason}.");
    }
    crate::println!("i2s: receiving {seconds} s (clki={}, fsi={}).", u8::from(invert_clock), u8::from(invert_frame));
    let (mut frames, mut zero_low, mut words) = (0u64, 0u64, 0u64);
    let mut peaks = [0u32; 2];
    let mut chunk = [0u8; 4096];
    let mut next_report = timer::micros() + 1_000_000;
    let end = timer::micros() + seconds * 1_000_000;
    while timer::micros() < end {
        crate::cpu::sleep();
        loop {
            let count = read(&mut chunk);
            if count == 0 {
                break;
            }
            for frame in chunk[..count].chunks_exact(8) {
                for channel in 0..2 {
                    let word = i32::from_le_bytes([frame[4 * channel], frame[4 * channel + 1], frame[4 * channel + 2], frame[4 * channel + 3]]);
                    peaks[channel] = peaks[channel].max((word >> 8).unsigned_abs());
                    zero_low += u64::from(word & 0xff == 0);
                    words += 1;
                }
                frames += 1;
            }
        }
        if timer::micros() >= next_report {
            next_report += 1_000_000;
            let status = unsafe { read_volatile(pcm(0x00)) };
            crate::println!(
                "  {frames} frames, peak L {:#08x} R {:#08x} (about -{}/-{} dBFS), low byte zero {}%, rx error {}, dropped {} bytes",
                peaks[0],
                peaks[1],
                level_text(peaks[0]),
                level_text(peaks[1]),
                zero_low * 100 / words.max(1),
                status >> 16 & 1,
                run().rx_dropped
            );
            (frames, zero_low, words) = (0, 0, 0);
            peaks = [0; 2];
        }
    }
    hw_stop();
    crate::println!("i2s: stopped; the pins are inputs again.");
}

pub fn command(rest: &str) {
    let mut words = rest.splitn(2, ' ');
    match (words.next(), words.next().map(str::trim)) {
        (Some("probe"), _) => probe(),
        (Some("capture"), arguments) => capture(arguments.unwrap_or("")),
        (Some("audio"), Some("on")) => {
            enable(true);
            crate::println!("i2s: the HAT is now an audio device (`audio`, `play`, `record`; `audio device N`).");
        }
        (Some("audio"), Some("off")) => {
            enable(false);
            crate::println!("i2s: the HAT is no longer an audio device.");
        }
        _ => crate::println!("Usage: i2s probe | i2s capture [SECONDS] [clki=0|1] [fsi=0|1] | i2s audio on|off"),
    }
}
