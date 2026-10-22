//! I2S capture on the Pi 4's PCM block (BCM2711, aarch64 only), first stage: the Maltex audio HAT, an I2S *master* (it makes the
//! 3.072 MHz bit clock and the 48 kHz frame clock; the Pi is the slave) with 24-bit audio left-justified in 32-bit slots.
//! Pins (alt0): GPIO18 PCM_CLK (bit clock), GPIO19 PCM_FS (frame clock), GPIO20 PCM_DIN, GPIO21 PCM_DOUT (not used yet).
//!
//! Shell: `i2s probe` counts edges on the three input pins with the pins left alone (is the card clocking?); `i2s capture
//! [SECONDS] [clki=0|1] [fsi=0|1]` receives and prints per second the level of each channel and whether the low byte of
//! every word is zero (24-bit data in 32-bit slots: ~100% when the bit alignment is right).
//!
//! PCM registers (base 0xFE203000, Linux `bcm2835-i2s.c`): CS_A 0x00 (EN b0, RXON b1, TXON b2, TXCLR b3, RXCLR b4, DMAEN b9, RXERR b16,
//! RXSEX b23 sign extend, STBY b25), FIFO_A 0x04, MODE_A 0x08 (FSLEN 9:0, FLEN 19:10 = frame bits - 1, FSI b20 invert frame sync,
//! FSM b21 frame sync slave, CLKI b22 invert bit clock, CLKM b23 clock slave, FRXP b25 packed RX), RXC_A 0x0c (per channel: WID 3:0,
//! POS 13:4, EN b14, WEX b15; channel 1 is the same at bit 16 up: WID 19:16, POS 29:20, EN b30, WEX b31; width = WID + 8 + 16*WEX),
//! DREQ_A 0x14 (RX_REQ 6:0, TX_REQ 14:8, RX_PANIC 22:16, TX_PANIC 30:24). I2S data starts one bit clock after the frame edge:
//! channel 1 at position 1, channel 2 at 33. The 64-word FIFO is emptied by DMA (DREQ 3 for RX).
//!
//! DMA (legacy controller 0xFE007000, channel 5; Linux's bcm2711 mask leaves 0, 2, 4, 5 and the high channels to the ARM): two
//! control blocks form a ring over a 64 KiB buffer in the non-cacheable pool. A control block is 8 words: TI (PERMAP 20:16,
//! SRC_DREQ b10, WAIT_RESP b3, DEST_INC b4), source, destination, length, stride, next, 2 reserved; addresses are *bus*
//! addresses: RAM at 0xC0000000 + physical (so the pool must lie in the first GiB), the PCM FIFO at 0x7E203004. The write position
//! is read back from the channel's DEST_AD register (0x10); no interrupt is used.
//!
//! Unverified until it runs on the Pi: the clock polarity (`clki`, `fsi`), the DMA channel, the STBY wait.

use crate::{gpio, memory, paging, timer};
use core::ptr::{read_volatile, write_volatile};

const PCM: u64 = 0xfe20_3000;
const DMA: u64 = 0xfe00_7000;
const DMA_CHANNEL: u64 = 5;
const DMA_ENABLE: u64 = DMA + 0xff0;
const PCM_FIFO_BUS: u32 = 0x7e20_3004;
const RAM_BUS: u64 = 0xc000_0000;
const RING_BYTES: u64 = 64 * 1024;
const HALF: u64 = RING_BYTES / 2;
const PINS: [usize; 3] = [18, 19, 20];

static mut RING: u64 = 0;

fn pcm(offset: u64) -> *mut u32 {
    (PCM + offset) as *mut u32
}

fn dma(offset: u64) -> *mut u32 {
    (DMA + DMA_CHANNEL * 0x100 + offset) as *mut u32
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

/// The control blocks (one frame) and the ring (16 frames), allocated once from the non-cacheable pool.
fn ring() -> Result<u64, &'static str> {
    let existing = unsafe { *core::ptr::addr_of!(RING) };
    if existing != 0 {
        return Ok(existing);
    }
    let base = memory::allocate_contiguous(1 + RING_BYTES / 4096, Some(1 << 32)).ok_or("no DMA memory")?;
    if base + 4096 + RING_BYTES > 0x4000_0000 {
        return Err("the DMA pool lies above 1 GiB, out of the legacy DMA controller's reach");
    }
    unsafe { *core::ptr::addr_of_mut!(RING) = base };
    Ok(base)
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

fn stop() {
    unsafe {
        write_volatile(dma(0x00), 1 << 31);
        let cs = read_volatile(pcm(0x00));
        write_volatile(pcm(0x00), cs & !((1 << 1) | (1 << 9)));
        write_volatile(pcm(0x00), 0);
    }
}

fn setup(base: u64, invert_clock: bool, invert_frame: bool) {
    let blocks = base as *mut ControlBlock;
    let buffer = base + 4096;
    let info = (3 << 16) | (1 << 10) | (1 << 4) | (1 << 3); // PERMAP 3 (PCM RX), SRC_DREQ, DEST_INC, WAIT_RESP
    unsafe {
        for half in 0..2u64 {
            blocks.add(half as usize).write_volatile(ControlBlock {
                info,
                source: PCM_FIFO_BUS,
                destination: bus(buffer + half * HALF),
                length: HALF as u32,
                stride: 0,
                next: bus(base + (1 - half) * 32),
                reserved: [0; 2],
            });
        }
        write_volatile(pcm(0x00), 1); // EN
        write_volatile(pcm(0x00), 1 | (1 << 3) | (1 << 4)); // clear both FIFOs
        let mode = (63 << 10) | 32 | (1 << 21) | (1 << 23) | u32::from(invert_frame) << 20 | u32::from(invert_clock) << 22;
        write_volatile(pcm(0x08), mode);
        // Two channels, 32 bits each (WID 8 + WEX 1), at positions 1 and 33.
        let channel = |position: u32| (1 << 14) | (position << 4) | (1 << 15) | 8;
        write_volatile(pcm(0x0c), (channel(1) << 16) | channel(33));
        write_volatile(pcm(0x14), (0x30 << 16) | 0x20); // RX_PANIC 48, RX_REQ 32 words
        write_volatile(pcm(0x00), 1 | (1 << 23) | (1 << 25) | (1 << 9)); // EN, RXSEX, STBY, DMAEN
        let wait = timer::micros();
        while timer::micros() - wait < 100 {}
        write_volatile(pcm(0x00), 1 | (1 << 23) | (1 << 25) | (1 << 9) | (1 << 3) | (1 << 4));
        write_volatile(DMA_ENABLE as *mut u32, read_volatile(DMA_ENABLE as *const u32) | 1 << DMA_CHANNEL);
        write_volatile(dma(0x00), 1 << 31); // RESET
        let wait = timer::micros();
        while read_volatile(dma(0x00)) & (1 << 31) != 0 && timer::micros() - wait < 1000 {}
        write_volatile(dma(0x04), bus(base));
        write_volatile(dma(0x00), 1 | (1 << 28) | (8 << 16) | (8 << 20)); // ACTIVE, wait for writes, priorities
        write_volatile(pcm(0x00), 1 | (1 << 23) | (1 << 25) | (1 << 9) | (1 << 1)); // RXON
    }
}

fn level_text(peak: u32) -> (u32, u32) {
    if peak == 0 {
        return (0, 0);
    }
    let bits = 32 - peak.leading_zeros();
    (1, (24 - bits) * 6)
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
    let base = match ring() {
        Ok(base) => base,
        Err(reason) => return crate::println!("i2s: {reason}."),
    };
    paging::map_device(PCM, 0x1000);
    paging::map_device(DMA, 0x1000);
    for pin in PINS {
        if gpio::set_function(pin, 2, false).is_err() {
            return crate::println!("i2s: GPIO{pin} cannot be switched to the PCM function here.");
        }
    }
    setup(base, invert_clock, invert_frame);
    crate::println!("i2s: receiving {seconds} s (clki={}, fsi={}).", u8::from(invert_clock), u8::from(invert_frame));
    let buffer = base + 4096;
    let mut read = 0u64;
    let (mut frames, mut zero_low, mut words, mut overruns) = (0u64, 0u64, 0u64, 0u64);
    let mut peaks = [0u32; 2];
    let mut next_report = timer::micros() + 1_000_000;
    let end = timer::micros() + seconds * 1_000_000;
    while timer::micros() < end {
        crate::cpu::sleep();
        let write = unsafe { read_volatile(dma(0x10)) } as u64 & 0x3fff_ffff;
        let write = write.wrapping_sub(buffer & 0x3fff_ffff) % RING_BYTES & !7;
        let available = (write + RING_BYTES - read) % RING_BYTES;
        if available > RING_BYTES - 8192 {
            overruns += 1;
        }
        while read != write {
            for channel in 0..2usize {
                let word = unsafe { read_volatile((buffer + read + 4 * channel as u64) as *const u32) } as i32;
                peaks[channel] = peaks[channel].max((word >> 8).unsigned_abs());
                zero_low += u64::from(word & 0xff == 0);
                words += 1;
            }
            frames += 1;
            read = (read + 8) % RING_BYTES;
        }
        if timer::micros() >= next_report {
            next_report += 1_000_000;
            let status = unsafe { read_volatile(pcm(0x00)) };
            crate::println!(
                "  {frames} frames, peak L {:#08x} R {:#08x} (about -{}/-{} dBFS), low byte zero {}%, rx error {}, overruns {overruns}",
                peaks[0],
                peaks[1],
                level_text(peaks[0]).1,
                level_text(peaks[1]).1,
                zero_low * 100 / words.max(1),
                status >> 16 & 1
            );
            unsafe {
                crate::println!(
                    "  dma cs {:#x} debug {:#x} cb {:#x} dest {:#x} left {}; pcm cs {:#x}",
                    read_volatile(dma(0x00)),
                    read_volatile(dma(0x20)),
                    read_volatile(dma(0x04)),
                    read_volatile(dma(0x10)),
                    read_volatile(dma(0x14)),
                    status
                );
            }
            (frames, zero_low, words) = (0, 0, 0);
            peaks = [0; 2];
        }
    }
    stop();
    for pin in PINS {
        let _ = gpio::set_function(pin, 0, false);
    }
    crate::println!("i2s: stopped; the pins are inputs again.");
}

pub fn command(rest: &str) {
    let mut words = rest.splitn(2, ' ');
    match words.next() {
        Some("probe") => probe(),
        Some("capture") => capture(words.next().unwrap_or("")),
        _ => crate::println!("Usage: i2s probe | i2s capture [SECONDS] [clki=0|1] [fsi=0|1]"),
    }
}
