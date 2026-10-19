//! The wall clock: the CMOS real-time clock read once at boot (it holds *local* time, whatever zone the owner keeps),
//! then advanced by the APIC timer's microsecond count, so nothing polls CMOS while audio is being recorded.
//!
//! The RTC is two I/O ports: write a register number to 0x70, read or write its value at 0x71. Registers 0, 2, 4, 6-9
//! hold seconds, minutes, hours, day, month and (two-digit) year, in BCD unless register B bit 2 says binary; register
//! B bit 1 says 24-hour (otherwise the hour's top bit means PM); register A bit 7 says an update is in progress, when
//! the values are unreliable; register D bit 7 says the battery kept the clock alive. The century is in a register the
//! ACPI FADT names, if it names one.
//!
//! A clock whose battery is dead reads garbage or a date long ago; `now()` then says `None` rather than inventing a
//! time, and callers fall back to a fixed stamp. `date set` fixes that for the session and writes the RTC.

use crate::{cpu, timer};
use core::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use fat::Time;

const INDEX: u16 = 0x70;
const DATA: u16 = 0x71;
/// Seconds since 2000-01-01 00:00:00 (local time) at `BASE_MICROS`, or 0 when no valid time is known.
static BASE_SECONDS: AtomicU64 = AtomicU64::new(0);
static BASE_MICROS: AtomicU64 = AtomicU64::new(0);
static CENTURY_REGISTER: AtomicU8 = AtomicU8::new(0);
/// Dates before this year are taken as "the clock was never set".
const FIRST_VALID_YEAR: u16 = 2024;

fn read_register(register: u8) -> u8 {
    cpu::outb(INDEX, register);
    cpu::inb(DATA)
}

fn write_register(register: u8, value: u8) {
    cpu::outb(INDEX, register);
    cpu::outb(DATA, value);
}

fn from_bcd(value: u8) -> u8 {
    (value >> 4) * 10 + (value & 15)
}

fn to_bcd(value: u8) -> u8 {
    (value / 10) << 4 | (value % 10)
}

/// One consistent reading of the RTC, or None if it never settles or is not valid.
fn read_rtc() -> Option<Time> {
    let century_register = CENTURY_REGISTER.load(Ordering::Relaxed);
    if read_register(0x0d) & 0x80 == 0 {
        return None; // the battery is dead: the clock lost its time
    }
    let mut previous = None;
    for _ in 0..8 {
        // Wait out an update in progress (about 2 ms every second).
        let mut guard = 0;
        while read_register(0x0a) & 0x80 != 0 && guard < 100_000 {
            guard += 1;
        }
        let raw = [read_register(0), read_register(2), read_register(4), read_register(7), read_register(8), read_register(9), if century_register != 0 { read_register(century_register) } else { 0 }];
        if previous == Some(raw) {
            let status_b = read_register(0x0b);
            let binary = status_b & 4 != 0;
            let convert = |value: u8| if binary { value } else { from_bcd(value) };
            let pm = raw[2] & 0x80 != 0 && status_b & 2 == 0;
            let mut hour = convert(raw[2] & 0x7f);
            if status_b & 2 == 0 {
                hour = hour % 12 + if pm { 12 } else { 0 };
            }
            let century = if century_register != 0 { u16::from(convert(raw[6])) } else { 20 };
            let time = Time { year: century * 100 + u16::from(convert(raw[5])), month: convert(raw[4]), day: convert(raw[3]), hour, minute: convert(raw[1]), second: convert(raw[0]) };
            return valid(&time).then_some(time);
        }
        previous = Some(raw);
    }
    None
}

fn valid(time: &Time) -> bool {
    time.year >= FIRST_VALID_YEAR && time.year < 2100 && (1..=12).contains(&time.month) && time.day >= 1 && time.day <= days_in_month(time.year, time.month) && time.hour < 24 && time.minute < 60 && time.second < 60
}

fn is_leap(year: u16) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn days_in_month(year: u16, month: u8) -> u8 {
    match month {
        2 => 28 + u8::from(is_leap(year)),
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Seconds since 2000-01-01 00:00:00 for a valid date.
fn to_seconds(time: &Time) -> u64 {
    let mut days = 0u64;
    for year in 2000..time.year {
        days += 365 + u64::from(is_leap(year));
    }
    for month in 1..time.month {
        days += u64::from(days_in_month(time.year, month));
    }
    days += u64::from(time.day) - 1;
    ((days * 24 + u64::from(time.hour)) * 60 + u64::from(time.minute)) * 60 + u64::from(time.second)
}

fn from_seconds(seconds: u64) -> Time {
    let (second, minute, hour) = ((seconds % 60) as u8, (seconds / 60 % 60) as u8, (seconds / 3600 % 24) as u8);
    let mut days = seconds / 86_400;
    let mut year = 2000u16;
    loop {
        let length = 365 + u64::from(is_leap(year));
        if days < length {
            break;
        }
        days -= length;
        year += 1;
    }
    let mut month = 1u8;
    while days >= u64::from(days_in_month(year, month)) {
        days -= u64::from(days_in_month(year, month));
        month += 1;
    }
    Time { year, month, day: days as u8 + 1, hour, minute, second }
}

/// Reads the RTC and starts the software clock from it. `century` is the FADT's century register (0 if none). Needs the
/// APIC timer to be running (it advances the clock).
pub fn init(century: u8) {
    CENTURY_REGISTER.store(century, Ordering::Relaxed);
    match read_rtc() {
        Some(time) => {
            set_base(&time);
            crate::println!("Clock: {} (the RTC, taken as local time).", Display(time));
        }
        None => crate::println!("Clock: the RTC holds no valid time (dead battery?); files get a fixed date until `date set`."),
    }
}

fn set_base(time: &Time) {
    BASE_MICROS.store(timer::micros(), Ordering::Relaxed);
    BASE_SECONDS.store(to_seconds(time), Ordering::Relaxed);
}

/// The local time now, or None if the clock was never set.
pub fn now() -> Option<Time> {
    let base = BASE_SECONDS.load(Ordering::Relaxed);
    if base == 0 {
        return None;
    }
    let elapsed = timer::micros().saturating_sub(BASE_MICROS.load(Ordering::Relaxed)) / 1_000_000;
    Some(from_seconds(base + elapsed))
}

/// `YYYY-MM-DD HH:MM:SS`.
pub struct Display(pub Time);

impl core::fmt::Display for Display {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let time = &self.0;
        write!(formatter, "{:04}-{:02}-{:02} {:02}:{:02}:{:02}", time.year, time.month, time.day, time.hour, time.minute, time.second)
    }
}

/// Sets the clock to `time` (local) and writes it to the RTC. Err is a reason for the user.
pub fn set(time: &Time) -> Result<(), &'static str> {
    if !valid(time) {
        return Err("that is not a date (2024-01-01 or later, e.g. `date set 2026-10-06 14:30`)");
    }
    set_base(time);
    // Stop the RTC's own updates (register B bit 7) while its fields are written, and keep it in 24-hour mode.
    let status_b = read_register(0x0b);
    let binary = status_b & 4 != 0;
    write_register(0x0b, status_b | 0x80 | 2);
    let convert = |value: u8| if binary { value } else { to_bcd(value) };
    write_register(0, convert(time.second));
    write_register(2, convert(time.minute));
    write_register(4, convert(time.hour));
    write_register(7, convert(time.day));
    write_register(8, convert(time.month));
    write_register(9, convert((time.year % 100) as u8));
    let century = CENTURY_REGISTER.load(Ordering::Relaxed);
    if century != 0 {
        write_register(century, convert((time.year / 100) as u8));
    }
    write_register(0x0b, (status_b | 2) & !0x80);
    Ok(())
}
