//! Pi 4 CPU clock and temperature through the VideoCore mailbox (`mailbox::tag`; aarch64 only). The firmware owns the clock: it
//! runs the ARM at the speed `config.txt` says and throttles on its own near 80 C; we can ask for another rate (the
//! firmware sets the PLL divider and the core voltage follows) and read the SoC temperature and the throttle flags.
//! Tags (firmware wiki, "Mailbox property interface"): GET_CLOCK_RATE 0x30002, GET_MAX 0x30004, GET_MIN 0x30007,
//! GET_TEMPERATURE 0x30006 (millidegrees), GET_THROTTLED 0x30046, SET_CLOCK_RATE 0x38002 (id, hz, skip turbo); clock id 3 is ARM.

use crate::mailbox::tag;

const ARM: u32 = 3;

fn clock(id: u32) -> Option<u32> {
    let mut reply = [0u32; 2];
    tag(id, &[ARM], &mut reply)?;
    Some(reply[1])
}

/// The ARM clock in MHz now, its minimum and its maximum.
pub fn mhz() -> Option<(u32, u32, u32)> {
    Some((clock(0x30002)? / 1_000_000, clock(0x30007)? / 1_000_000, clock(0x30004)? / 1_000_000))
}

/// SoC temperature in millidegrees C.
pub fn millicelsius() -> Option<u32> {
    let mut reply = [0u32; 2];
    tag(0x30006, &[0], &mut reply)?;
    Some(reply[1])
}

/// Firmware flags: bit 0 under-voltage, 1 frequency capped, 2 throttled, 3 soft temperature limit; the same bits << 16 mean
/// "has happened since boot".
pub fn throttled() -> Option<u32> {
    let mut reply = [0u32; 1];
    tag(0x30046, &[0], &mut reply)?;
    Some(reply[0])
}

/// Asks for `mhz` (clamped by the firmware to its range); returns the rate it set.
pub fn set_mhz(mhz: u32) -> Option<u32> {
    let mut reply = [0u32; 2];
    tag(0x38002, &[ARM, mhz.checked_mul(1_000_000)?, 0], &mut reply)?;
    Some(reply[1] / 1_000_000)
}

/// `settings`: `pi-clock MHZ|min|max` at boot. Returns whether the line was one of ours.
pub fn apply_setting(line: &str) -> bool {
    let mut words = line.split_whitespace();
    if words.next() != Some("pi-clock") {
        return false;
    }
    let Some((_, min, max)) = mhz() else {
        crate::println!("settings: pi-clock: the firmware does not answer.");
        return true;
    };
    let wanted = match words.next() {
        Some("min") => Some(min),
        Some("max") => Some(max),
        Some(word) => word.parse::<u32>().ok(),
        None => None,
    };
    match wanted.and_then(set_mhz) {
        Some(now) => crate::println!("settings: pi-clock {now} MHz."),
        None => crate::println!("settings: `{}` ignored (pi-clock MHZ|min|max).", line.trim()),
    }
    true
}

pub fn command(rest: &str) {
    let word = rest.split_whitespace().next();
    if let Some(word) = word {
        let Some((_, min, max)) = mhz() else { return crate::println!("cpufreq: the firmware does not answer.") };
        let target = match word {
            "min" => Some(min),
            "max" => Some(max),
            _ => word.parse::<u32>().ok().filter(|mhz| (min..=max).contains(mhz)),
        };
        let Some(target) = target else { return crate::println!("Usage: cpufreq [MHZ|min|max]  ({min}..{max} MHz)") };
        match set_mhz(target) {
            Some(now) => crate::println!("ARM clock set to {now} MHz."),
            None => crate::println!("cpufreq: the firmware refused."),
        }
        return;
    }
    match mhz() {
        Some((now, min, max)) => crate::println!("ARM clock {now} MHz (range {min}..{max})."),
        None => crate::println!("cpufreq: the firmware does not answer."),
    }
    match millicelsius() {
        Some(milli) => crate::println!("SoC temperature {}.{} C.", milli / 1000, milli % 1000 / 100),
        None => crate::println!("temperature: the firmware does not answer."),
    }
    if let Some(flags) = throttled() {
        let names = ["under-voltage", "frequency capped", "throttled", "soft temperature limit"];
        for (bit, name) in names.iter().enumerate() {
            if flags >> bit & 1 != 0 {
                crate::println!("  now: {name}");
            } else if flags >> (16 + bit) & 1 != 0 {
                crate::println!("  since boot: {name}");
            }
        }
        if flags & 0xf000f == 0 {
            crate::println!("  no throttling or under-voltage since boot.");
        }
    }
}
