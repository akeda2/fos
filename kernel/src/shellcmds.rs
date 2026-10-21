//! The shell commands both kernels have in common and that need no hardware beyond the timer, the clock, the keyboard and
//! the task list (docs/shell.md). Each port adds this table to its own; where the two differ only in a detail (the RTC or a
//! software clock, which keyboard driver) a `cfg` picks it.

use fosfor_shell::Command;

use crate::timer;

#[cfg(target_arch = "aarch64")]
use crate::usb_kbd as keyboard;
#[cfg(target_arch = "x86_64")]
use crate::keyboard;

pub static COMMANDS: &[Command] = &[
    Command::new("sleep", "sleep SECONDS", "waits (at most 60 s) without polling anything, as a slow command would.", |_, rest| sleep(rest)),
    Command::new("uptime", "uptime", "time since the timer started.", |_, _| uptime()),
    Command::new("date", "date [set YYYY-MM-DD HH:MM[:SS]]", "the clock (local time), or sets it.", |_, rest| date(rest)),
    Command::new("keymap", "keymap [us|sv]", "the local keyboard layout (sv: Swedish/Finnish).", |_, rest| keymap(rest)),
    Command::new("ps", "ps", "lists the running tapes.", |_, _| crate::tasks::list()).alias(&["tasks"]),
    Command::new("kill", "kill N|NAME|all", "stops a background tape.", |_, rest| crate::tasks::kill(rest.split_whitespace().next())),
];

fn sleep(rest: &str) {
    let seconds = rest.split_whitespace().next().and_then(|word| word.parse::<u64>().ok()).unwrap_or(1).min(60);
    let end = timer::ticks() + seconds * timer::TICK_HZ;
    while timer::ticks() < end {
        #[cfg(target_arch = "x86_64")]
        crate::debug::pet();
        crate::cpu::sleep();
    }
    crate::println!("Slept {seconds} s.");
}

fn uptime() {
    let ticks = timer::ticks();
    #[cfg(target_arch = "x86_64")]
    crate::println!("{} ticks at {} Hz ({} s); spurious legacy-PIC interrupts: {}", ticks, timer::TICK_HZ, ticks / timer::TICK_HZ, crate::apic::legacy_pic_interrupts());
    #[cfg(target_arch = "aarch64")]
    crate::println!("{} ticks at {} Hz ({} s)", ticks, timer::TICK_HZ, ticks / timer::TICK_HZ);
}

fn keymap(rest: &str) {
    let name = rest.split_whitespace().next();
    let Some(name) = name else {
        return crate::println!("keymap: {} (keymap us|sv changes it).", if keyboard::layout() == fosfor_abi::keymap::Layout::Us { "us" } else { "sv" });
    };
    match keyboard::layout_named(name) {
        Some(layout) => {
            keyboard::set_layout(layout);
            crate::println!("keymap: {name}.");
        }
        None => crate::println!("keymap: unknown layout `{name}` (us or sv)."),
    }
}

fn date(rest: &str) {
    let mut words = rest.split_whitespace();
    match words.next() {
        None => match crate::clock::now() {
            #[cfg(target_arch = "x86_64")]
            Some(time) => crate::println!("{} (local time, from the RTC)", crate::clock::Display(time)),
            #[cfg(target_arch = "aarch64")]
            Some(time) => crate::println!("{} (local time, software clock)", crate::clock::Display(time)),
            None => crate::println!("The clock is not set. `date set YYYY-MM-DD HH:MM`, or run the clock tape."),
        },
        Some("set") => {
            let parsed = (|| {
                let (date, time) = (words.next()?, words.next()?);
                let mut parts = date.split('-');
                let (year, month, day) = (parts.next()?.parse().ok()?, parts.next()?.parse().ok()?, parts.next()?.parse().ok()?);
                let mut parts = time.split(':');
                let (hour, minute) = (parts.next()?.parse().ok()?, parts.next()?.parse().ok()?);
                let second = parts.next().map_or(Some(0), |word| word.parse().ok())?;
                Some(fat::Time { year, month, day, hour, minute, second })
            })();
            match parsed {
                None => crate::println!("Usage: date set YYYY-MM-DD HH:MM[:SS]   (local time)"),
                Some(time) => match crate::clock::set(&time) {
                    Ok(()) => crate::println!("The clock is {}.", crate::clock::Display(time)),
                    Err(reason) => crate::println!("date: {reason}"),
                },
            }
        }
        Some(_) => crate::println!("Usage: date | date set YYYY-MM-DD HH:MM[:SS]"),
    }
}
