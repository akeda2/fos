//! I2C master on the Raspberry Pi 4's BSC1 (GPIO2 = SDA, GPIO3 = SCL, alt0; the 40-pin header's I2C bus, aarch64 only).
//!
//! The BSC ("Broadcom Serial Controller", the BCM2835 peripheral guide's chapter 3, unchanged in the BCM2711) moves a
//! transfer of up to 65535 bytes on its own once `ST` is written: START, address, data through a 16-byte FIFO, STOP, then
//! `DONE`. Registers (offsets from 0xFE804000): C 0x00 control, S 0x04 status, DLEN 0x08, A 0x0c address, FIFO 0x10,
//! DIV 0x14 (SCL = core clock / DIV), DEL 0x18 (data edge delays, as Linux's `i2c-bcm2835.c` sets them), CLKT 0x1c (clock
//! stretch timeout). Status bits are write-one-to-clear for ERR (no ACK), CLKT and DONE. The input clock is the VideoCore
//! "core" clock (mailbox clock id 4), 500 MHz with the stock firmware; it is asked for, not assumed.
//!
//! Waiting follows the SD driver: poll the status, and once a wait outlasts a short spin sleep in `wfi` with the controller's
//! interrupt armed (`INTD` done, `INTT` FIFO wants data, `INTR` FIFO has data; GIC SPI 117, level). The handler only drops the
//! enables, so the line falls and the foreground (which reads the status itself) carries on; with interrupts masked the
//! arm-check-sleep sequence cannot miss a wake-up, as `wfi` wakes on a pending interrupt whether or not it is taken. The
//! timer tick is the backstop, and every wait is bounded.
//!
//! No repeated START: `write_read` is a write, a STOP, then a read. Most register-style devices (touch controllers, EEPROMs,
//! sensors) accept that; one that needs a repeated START does not work until the BSC's chained-transfer trick is tried.
//! A scan probes with a one-byte read, as `i2cdetect -r` does; a write probe would change a device's register pointer.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use crate::timer;

const BASE: u64 = 0xfe80_4000;
const PI4_GIC_DISTRIBUTOR: u64 = 0xff84_1000;
/// BSC1's GIC interrupt: VideoCore interrupt 21 (shared by the BSC units) is SPI 96 + 21.
const INTID: u32 = 32 + 117;
const SDA: usize = 2;
const SCL: usize = 3;
/// GPIO alt0 in `gpio::set_function` numbering (0 in, 1 out, 2.. alt0..).
const ALT0: u8 = 2;
/// Clock id 4 of the mailbox clock tags: the VideoCore core clock, the BSC's input.
const CORE_CLOCK: u32 = 4;

const C: u64 = 0x00;
const S: u64 = 0x04;
const DLEN: u64 = 0x08;
const A: u64 = 0x0c;
const FIFO: u64 = 0x10;
const DIV: u64 = 0x14;
const DEL: u64 = 0x18;

const C_EN: u32 = 1 << 15;
const C_INTR: u32 = 1 << 10;
const C_INTT: u32 = 1 << 9;
const C_INTD: u32 = 1 << 8;
const C_ST: u32 = 1 << 7;
const C_CLEAR: u32 = 3 << 4;
const C_READ: u32 = 1;

const S_CLKT: u32 = 1 << 9;
const S_ERR: u32 = 1 << 8;
const S_RXD: u32 = 1 << 5;
const S_TXD: u32 = 1 << 4;
const S_DONE: u32 = 1 << 1;
const S_TA: u32 = 1;

/// A wait shorter than this spins; a longer one sleeps until the interrupt.
const SPIN_MICROS: u64 = 30;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Not a Raspberry Pi 4, or its GPIO pins are not available.
    NoBus,
    /// GPIO2 or GPIO3 may not be changed (`gpio-deny`).
    Pins,
    BadArgument,
    /// Nothing acknowledged the address or a byte.
    Nack,
    /// The device stretched the clock for too long, or the transfer did not finish in time.
    Timeout,
    /// The controller's clock-stretch timeout (CLKT) fired.
    Stretch,
    /// The transfer ended before every byte moved.
    Short,
}

impl Error {
    pub fn text(self) -> &'static str {
        match self {
            Error::NoBus => "no I2C bus here (Raspberry Pi 4 only)",
            Error::Pins => "GPIO2/GPIO3 may not be changed (gpio-deny)",
            Error::BadArgument => "bad address or length",
            Error::Nack => "no acknowledge",
            Error::Timeout => "timeout",
            Error::Stretch => "clock stretched too long",
            Error::Short => "transfer ended early",
        }
    }
}

static STARTED: AtomicBool = AtomicBool::new(false);
static SPEED_HZ: AtomicU32 = AtomicU32::new(100_000);
static INTERRUPTS: AtomicU64 = AtomicU64::new(0);

fn read(offset: u64) -> u32 {
    unsafe { ((BASE + offset) as *const u32).read_volatile() }
}

fn write(offset: u64, value: u32) {
    unsafe { ((BASE + offset) as *mut u32).write_volatile(value) }
}

/// IRQ context: drop the interrupt enables so the level line falls; the foreground reads the status itself.
fn interrupt(_intid: u32) {
    INTERRUPTS.fetch_add(1, Ordering::Relaxed);
    write(C, read(C) & (C_EN | C_READ));
}

pub fn interrupt_count() -> u64 {
    INTERRUPTS.load(Ordering::Relaxed)
}

pub fn available() -> bool {
    crate::gic::bases().0 == PI4_GIC_DISTRIBUTOR && crate::gpio::available()
}

/// The core clock in Hz, from the firmware.
fn core_clock() -> Option<u32> {
    static CACHED: AtomicU32 = AtomicU32::new(0);
    if CACHED.load(Ordering::Relaxed) == 0 {
        let mut reply = [0u32; 2];
        crate::mailbox::tag(0x30002, &[CORE_CLOCK], &mut reply)?;
        CACHED.store(reply[1], Ordering::Relaxed);
    }
    Some(CACHED.load(Ordering::Relaxed))
}

/// Maps the controller, gives GPIO2/3 their I2C function and hooks the interrupt, once.
fn start() -> Result<(), Error> {
    if !available() {
        return Err(Error::NoBus);
    }
    if STARTED.swap(true, Ordering::AcqRel) {
        return Ok(());
    }
    crate::paging::map_device(BASE, 0x1000);
    crate::gic::register(INTID, true, interrupt);
    write(C, C_EN | C_CLEAR);
    Ok(())
}

fn pins() -> Result<(), Error> {
    for pin in [SDA, SCL] {
        if crate::gpio::function(pin) != Some(ALT0) {
            crate::gpio::set_function(pin, ALT0, false).map_err(|_| Error::Pins)?;
        }
    }
    Ok(())
}

pub fn speed() -> u32 {
    SPEED_HZ.load(Ordering::Relaxed)
}

/// SCL speed for the next transfers, 10 kHz to 1 MHz.
pub fn set_speed(hz: u32) -> Result<(), Error> {
    if !(10_000..=1_000_000).contains(&hz) {
        return Err(Error::BadArgument);
    }
    SPEED_HZ.store(hz, Ordering::Relaxed);
    Ok(())
}

/// The divider and the SCL speed it gives for the current setting (the core clock rounded up to an even divider).
pub fn divider() -> Option<(u32, u32, u32)> {
    let clock = core_clock()?;
    let div = (clock.div_ceil(speed()) + 1 & !1).clamp(2, 0xfffe);
    Some((clock, div, clock / div))
}

enum Io<'a> {
    Write(&'a [u8]),
    Read(&'a mut [u8]),
}

fn transfer(address: u8, mut io: Io) -> Result<(), Error> {
    start()?;
    pins()?;
    let length = match &io {
        Io::Write(data) => data.len(),
        Io::Read(data) => data.len(),
    };
    if !(0x08..=0x77).contains(&address) || length == 0 || length > 0xffff {
        return Err(Error::BadArgument);
    }
    let (_, div, _) = divider().ok_or(Error::Timeout)?;
    let (fall, rise) = if div >= 5 { ((div / 16).max(1), (div / 4).max(1)) } else { (1, 1) };
    let reading = matches!(io, Io::Read(_));
    let control = C_EN | if reading { C_READ } else { 0 };
    write(C, C_EN | C_CLEAR);
    write(S, S_CLKT | S_ERR | S_DONE);
    write(DIV, div);
    write(DEL, fall << 16 | rise);
    write(A, u32::from(address));
    write(DLEN, length as u32);
    let mut moved = 0;
    if let Io::Write(data) = &io {
        while moved < length && read(S) & S_TXD != 0 && moved < 16 {
            write(FIFO, u32::from(data[moved]));
            moved += 1;
        }
    }
    write(C, control | C_ST);

    let start = timer::micros();
    // Nine clocks a byte plus the address, four times over, and a margin for a device that stretches the clock.
    let limit = 10_000 + (length as u64 + 2) * 9 * 4 * 1_000_000 / u64::from(speed());
    let result = loop {
        let status = read(S);
        if status & S_ERR != 0 {
            break Err(Error::Nack);
        }
        if status & S_CLKT != 0 {
            break Err(Error::Stretch);
        }
        match &mut io {
            Io::Write(data) => {
                if moved < length && status & S_TXD != 0 {
                    write(FIFO, u32::from(data[moved]));
                    moved += 1;
                    continue;
                }
            }
            Io::Read(data) => {
                if moved < length && status & S_RXD != 0 {
                    data[moved] = read(FIFO) as u8;
                    moved += 1;
                    continue;
                }
            }
        }
        if status & S_DONE != 0 {
            if moved == length {
                break Ok(());
            }
            // A NACK can raise DONE a moment before ERR: look again before calling it a short transfer.
            let later = timer::micros();
            while timer::micros() - later < 5 {}
            break Err(if read(S) & S_ERR != 0 { Error::Nack } else { Error::Short });
        }
        let waited = timer::micros() - start;
        if waited > limit {
            break Err(Error::Timeout);
        }
        if waited > SPIN_MICROS {
            let was = crate::cpu::interrupts_save_and_disable();
            let (enables, wanted) = if reading { (C_INTR | C_INTD, S_RXD) } else if moved < length { (C_INTT | C_INTD, S_TXD) } else { (C_INTD, 0) };
            write(C, control | enables);
            if read(S) & (S_DONE | S_ERR | S_CLKT | wanted) == 0 {
                crate::cpu::sleep();
            }
            crate::cpu::interrupts_restore(was);
        }
    };
    // DONE and ERR can come before the STOP condition is out: wait for the transfer-active bit to fall (bounded), or the
    // next transfer's start is lost and its stale DONE reads as a short transfer.
    let idle = timer::micros();
    while read(S) & S_TA != 0 && timer::micros() - idle < 1_000 {}
    write(C, C_EN | C_CLEAR);
    write(S, S_CLKT | S_ERR | S_DONE);
    result
}

/// Writes `data` to the 7-bit device `address`.
pub fn write_bytes(address: u8, data: &[u8]) -> Result<(), Error> {
    transfer(address, Io::Write(data))
}

/// Reads `data.len()` bytes from the device.
pub fn read_bytes(address: u8, data: &mut [u8]) -> Result<(), Error> {
    transfer(address, Io::Read(data))
}

/// Writes `request` (a register number), then reads the answer, with a STOP in between (see the module comment).
pub fn write_read(address: u8, request: &[u8], data: &mut [u8]) -> Result<(), Error> {
    write_bytes(address, request)?;
    read_bytes(address, data)
}

// ---- tape ABI 23 ------------------------------------------------------------------------------------------------------

fn status_code(error: Error) -> i32 {
    match error {
        Error::Nack => -1,
        Error::Timeout | Error::Stretch | Error::Short => -2,
        Error::BadArgument => -3,
        Error::NoBus => -4,
        Error::Pins => -5,
    }
}

/// `i2c_speed(hz)`: 0 returns the speed in Hz, anything else sets it (0 done, negative error).
pub extern "C" fn api_i2c_speed(hz: u32) -> i32 {
    if !available() {
        return -4;
    }
    if hz == 0 {
        return speed() as i32;
    }
    set_speed(hz).map_or_else(status_code, |()| 0)
}

/// `i2c_transfer(address, out, out_len, input, in_len)`: an optional write, then an optional read.
pub extern "C" fn api_i2c_transfer(address: u32, out: *const u8, out_len: u32, input: *mut u8, in_len: u32) -> i32 {
    if !available() {
        return -4;
    }
    if address > 0x7f || (out_len == 0 && in_len == 0) || out_len > 0xffff || in_len > 0xffff || (out_len > 0 && out.is_null()) || (in_len > 0 && input.is_null()) {
        return -3;
    }
    let result = (|| {
        if out_len > 0 {
            write_bytes(address as u8, unsafe { core::slice::from_raw_parts(out, out_len as usize) })?;
        }
        if in_len > 0 {
            read_bytes(address as u8, unsafe { core::slice::from_raw_parts_mut(input, in_len as usize) })?;
        }
        Ok(())
    })();
    result.map_or_else(status_code, |()| 0)
}

/// A tape ends: the bus goes back to 100 kHz.
pub fn tape_ended() {
    SPEED_HZ.store(100_000, Ordering::Relaxed);
}

// ---- the shell command ------------------------------------------------------------------------------------------------

const USAGE: &str = "i2c [status|speed KHZ|scan|read ADDR [N]|write ADDR BYTE...|get ADDR REG [N]]  (hex ADDR, REG, BYTE)";

fn hex(word: Option<&str>) -> Option<u8> {
    u8::from_str_radix(word?.trim_start_matches("0x"), 16).ok()
}

fn count(word: Option<&str>) -> Option<usize> {
    word.map_or(Some(1), |word| word.parse::<usize>().ok().filter(|&n| (1..=256).contains(&n)))
}

fn dump(data: &[u8]) {
    for (row, chunk) in data.chunks(16).enumerate() {
        crate::print(format_args!("{:04x}:", row * 16));
        for byte in chunk {
            crate::print(format_args!(" {byte:02x}"));
        }
        crate::println!();
    }
}

fn report(result: Result<(), Error>) {
    if let Err(error) = result {
        crate::println!("i2c: {}.", error.text());
    }
}

pub fn command(rest: &str) {
    let mut words = rest.split_whitespace();
    match words.next().unwrap_or("status") {
        "status" => {
            if !available() {
                return crate::println!("i2c: {}.", Error::NoBus.text());
            }
            match divider() {
                Some((clock, div, actual)) => crate::println!("I2C1 (SDA GPIO{SDA}, SCL GPIO{SCL}): core clock {} MHz / {div} = {} kHz (asked {}), {} interrupts.",
                    clock / 1_000_000, actual / 1000, speed() / 1000, interrupt_count()),
                None => crate::println!("i2c: the firmware does not answer for the core clock."),
            }
        }
        "speed" => match words.next().and_then(|word| word.parse::<u32>().ok()) {
            Some(khz) => report(set_speed(khz.saturating_mul(1000))),
            None => crate::println!("{USAGE}"),
        },
        "scan" => {
            let begin = timer::micros();
            let mut found = 0;
            let mut buffer = [0u8; 1];
            for address in 0x08..=0x77u8 {
                match read_bytes(address, &mut buffer) {
                    Ok(()) => {
                        crate::println!("  {address:02x}: answers");
                        found += 1;
                    }
                    Err(Error::Nack) => {}
                    Err(error) => return crate::println!("i2c: {} at {address:02x}.", error.text()),
                }
            }
            crate::println!("{found} device(s) in {} ms.", (timer::micros() - begin) / 1000);
        }
        "read" => {
            let (Some(address), Some(n)) = (hex(words.next()), count(words.next())) else { return crate::println!("{USAGE}") };
            let mut data = [0u8; 256];
            match read_bytes(address, &mut data[..n]) {
                Ok(()) => dump(&data[..n]),
                Err(error) => report(Err(error)),
            }
        }
        "write" => {
            let Some(address) = hex(words.next()) else { return crate::println!("{USAGE}") };
            let mut data = [0u8; 64];
            let mut n = 0;
            for word in words {
                match hex(Some(word)) {
                    Some(byte) if n < data.len() => {
                        data[n] = byte;
                        n += 1;
                    }
                    _ => return crate::println!("{USAGE}"),
                }
            }
            report(write_bytes(address, &data[..n]));
        }
        "get" => {
            let (Some(address), Some(register)) = (hex(words.next()), hex(words.next())) else { return crate::println!("{USAGE}") };
            let Some(n) = count(words.next()) else { return crate::println!("{USAGE}") };
            let mut data = [0u8; 256];
            match write_read(address, &[register], &mut data[..n]) {
                Ok(()) => dump(&data[..n]),
                Err(error) => report(Err(error)),
            }
        }
        _ => crate::println!("{USAGE}"),
    }
}
