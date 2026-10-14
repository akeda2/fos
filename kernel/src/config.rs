//! The optional per-machine files on the boot FOSFS volume (else any other FOSFS volume, else the root of a FAT32/exFAT
//! disk), the same ones v1 reads:
//!
//! * `hostname`: the base name announced in discovery replies (`NAME` + the interface
//!   number, so `lab` becomes `lab1`);
//! * `settings`: `deny-term-net` closes the network terminal, `no-msi` keeps NICs polling;
//!   `serial-shell [PORT] [BAUD]` puts the shell on a serial port as well (COM2-COM4, numbered as `fterm list` does; off unless asked);
//!   `keymap us|sv` picks the keyboard layout (US by default; sv is Swedish/Finnish);
//!   `foreground gray|green|amber|black` and `background black|blue|gray` set the pixel console's colours (not in
//!   BIOS text mode, which stays grey on black);
//! * `debug`: `reboot-on-crash`, `reboot-delay N`, `crash-on-boot`; `watchdog [N]` arms the
//!   software watchdog and `kernel-update` enables KUP (both in `debug.rs`/`term.rs`);
//! * `start`: a command script run once at boot (up to 4096 bytes; blank lines and
//!   lines starting with `#` are skipped).
//!
//! One line per option, `#` comments. A missing file is simply the default.

use crate::storage;

const FILE_CAPACITY: usize = 4096;

pub struct Debug {
    pub reboot_on_crash: bool,
    pub reboot_delay_seconds: u32,
    pub crash_on_boot: bool,
    pub watchdog_seconds: Option<u32>,
    pub kernel_update: bool,
}

static mut DEBUG: Debug = Debug { reboot_on_crash: false, reboot_delay_seconds: 3, crash_on_boot: false, watchdog_seconds: None, kernel_update: false };
static mut DENY_TERM_NET: bool = false;
static mut NO_MSI: bool = false;
static mut PREFER_MSIX: bool = false;
static mut HOSTNAME: [u8; 16] = [0; 16];
static mut HOSTNAME_LENGTH: usize = 0;
static mut BUFFER: [u8; FILE_CAPACITY] = [0; FILE_CAPACITY];

pub fn debug() -> &'static Debug {
    unsafe { &*core::ptr::addr_of!(DEBUG) }
}

/// `no-msi` in the `settings` file: NICs stay on polling (a bring-up aid for hardware whose
/// MSI misbehaves).
pub fn no_msi() -> bool {
    unsafe { NO_MSI }
}

/// `prefer-msix` in `settings`: try MSI-X before MSI on devices that support both (a test
/// hook: MSI is enough for every part this kernel drives).
pub fn prefer_msix() -> bool {
    unsafe { PREFER_MSIX }
}

pub fn deny_term_net() -> bool {
    unsafe { DENY_TERM_NET }
}

/// The base hostname from the `hostname` file, if there is one.
pub fn hostname() -> Option<&'static str> {
    unsafe { (HOSTNAME_LENGTH != 0).then(|| core::str::from_utf8(core::slice::from_raw_parts(core::ptr::addr_of!(HOSTNAME) as *const u8, HOSTNAME_LENGTH)).unwrap_or("")) }
}

/// Reads `name` into the shared buffer (from the boot volume, else the first other disk that has it); the text, if present.
fn read(name: &str) -> Option<&'static str> {
    let buffer = unsafe { &mut *core::ptr::addr_of_mut!(BUFFER) };
    let Some(volume) = storage::volume_with_file(name.as_bytes()) else { return read_fat(name, buffer) };
    match volume.read(name.as_bytes(), buffer) {
        Ok(Some(length)) => core::str::from_utf8(&buffer[..length]).ok(),
        Ok(None) => None,
        Err(error) => {
            crate::println!("{name}: cannot be read ({error:?}); ignored.");
            None
        }
    }
}

/// The fallback when no FOSFS volume has `name`: the root directory of the first FAT32/exFAT disk that does (a stick or
/// SSD a PC can write, so a hostname or start script can be edited there). Disks with a FOSFS volume are not looked at
/// again, and a file longer than the buffer is ignored rather than cut.
fn read_fat(name: &str, buffer: &'static mut [u8; FILE_CAPACITY]) -> Option<&'static str> {
    for number in 1..=crate::block::count() {
        if storage::volume_on(number).is_ok() {
            continue;
        }
        let Some(mut fs) = crate::fatvol::mount(number - 1) else { continue };
        let Ok(Some(entry)) = fs.entry(name.as_bytes()) else { continue };
        if entry.is_directory {
            continue;
        }
        if entry.size as usize > FILE_CAPACITY {
            crate::println!("{name}: disk{number} has it but it is over {FILE_CAPACITY} bytes; ignored.");
            return None;
        }
        return match fs.read(&entry.file(), 0, &mut buffer[..entry.size as usize]) {
            Ok(length) => {
                crate::println!("{name}: read from disk{number} (FAT/exFAT).");
                core::str::from_utf8(&buffer[..length]).ok()
            }
            Err(error) => {
                crate::println!("{name}: cannot be read from disk{number} ({error:?}); ignored.");
                None
            }
        };
    }
    None
}

fn lines(text: &str) -> impl Iterator<Item = &str> {
    text.lines().map(str::trim).filter(|line| !line.is_empty() && !line.starts_with('#'))
}

/// Loads `hostname`, `settings` and `debug` (not `start`, which runs later).
pub fn load() {
    if let Some(text) = read("hostname") {
        let name = text.lines().next().unwrap_or("").trim();
        let take = name.len().min(15);
        unsafe {
            core::slice::from_raw_parts_mut(core::ptr::addr_of_mut!(HOSTNAME) as *mut u8, take).copy_from_slice(&name.as_bytes()[..take]);
            HOSTNAME_LENGTH = take;
        }
        crate::println!("hostname file: this machine announces itself as {}1.", &name[..take]);
    }
    if let Some(text) = read("settings") {
        for line in lines(text) {
            match line.split_whitespace().next() {
                Some("deny-term-net") => {
                    unsafe { DENY_TERM_NET = true };
                    crate::println!("settings: deny-term-net -- the network terminal is closed.");
                }
                Some(key @ ("foreground" | "background")) => {
                    let name = line.split_whitespace().nth(1).unwrap_or("");
                    let (fg, bg) = if key == "foreground" { (Some(name), None) } else { (None, Some(name)) };
                    if crate::framebuffer::set_colors(fg, bg) {
                        crate::println!("settings: {key} {name}.");
                    } else {
                        crate::println!("settings: `{line}` ignored (unknown colour, or no pixel console).");
                    }
                }
                Some("serial-shell") => {
                    let mut words = line.split_whitespace().skip(1);
                    let number = words.next().and_then(|word| word.parse().ok()).unwrap_or(1);
                    let baud = words.next().and_then(|word| word.parse().ok()).unwrap_or(115_200);
                    match crate::uart::open_shell(number, baud) {
                        0 => crate::println!("settings: serial-shell -- the shell is also on serial port {number} at {baud} baud (8n1)."),
                        _ => crate::println!("settings: `{line}` ignored (no such serial port, or bad baud rate; `run fterm list` shows them)."),
                    }
                }
                Some("keymap") => {
                    let name = line.split_whitespace().nth(1).unwrap_or("");
                    match crate::keyboard::layout_named(name) {
                        Some(layout) => {
                            crate::keyboard::set_layout(layout);
                            crate::println!("settings: keymap {name}.");
                        }
                        None => crate::println!("settings: `{line}` ignored (keymap us or sv)."),
                    }
                }
                Some("prefer-msix") => unsafe { PREFER_MSIX = true },
                Some("park-idle") => {
                    crate::nicpark::set_automatic();
                    crate::println!("settings: park-idle -- NICs without a link are parked after a minute.");
                }
                Some("hwp") => {
                    let name = line.split_whitespace().nth(1).unwrap_or("balanced");
                    match crate::cpupower::PREFERENCES.iter().find(|entry| entry.0 == name) {
                        Some(&(_, value)) => match crate::cpupower::enable_hwp(value) {
                            Ok(_) => crate::println!("settings: hwp {name} -- the CPU picks its own speed."),
                            Err(why) => crate::println!("settings: hwp ignored: {why}."),
                        },
                        None => crate::println!("settings: `{line}` ignored (hwp performance, balanced or power)."),
                    }
                }
                Some("no-msi") => {
                    unsafe { NO_MSI = true };
                    crate::println!("settings: no-msi -- NICs stay on polling.");
                }
                _ => crate::println!("settings: unknown line `{line}` ignored."),
            }
        }
    }
    if let Some(text) = read("debug") {
        let debug = unsafe { &mut *core::ptr::addr_of_mut!(DEBUG) };
        for line in lines(text) {
            let mut words = line.split_whitespace();
            let number = |word: Option<&str>, default: u32| word.and_then(|w| w.parse().ok()).unwrap_or(default);
            match words.next() {
                Some("reboot-on-crash") => debug.reboot_on_crash = true,
                Some("reboot-delay") => debug.reboot_delay_seconds = number(words.next(), 3).min(60),
                Some("crash-on-boot") => debug.crash_on_boot = true,
                Some("watchdog") => debug.watchdog_seconds = Some(number(words.next(), 60)),
                Some("kernel-update") => debug.kernel_update = true,
                _ => crate::println!("debug: unknown line `{line}` ignored."),
            }
        }
        crate::println!(
            "debug file: reboot-on-crash {}, reboot-delay {} s, crash-on-boot {}, watchdog {:?}, kernel-update {}.",
            debug.reboot_on_crash,
            debug.reboot_delay_seconds,
            debug.crash_on_boot,
            debug.watchdog_seconds,
            debug.kernel_update
        );
    }
}

/// Runs the `start` file through `run`, one command per line. False if there is no such file.
pub fn run_start(mut run: impl FnMut(&str)) -> bool {
    let Some(text) = read("start") else { return false };
    // `read` shares one buffer, so copy before running commands that may read files.
    let mut script = [0u8; FILE_CAPACITY];
    script[..text.len()].copy_from_slice(text.as_bytes());
    let script = core::str::from_utf8(&script[..text.len()]).unwrap_or("");
    crate::println!("start file: running {} command(s).", lines(script).count());
    for line in lines(script) {
        crate::println!("start> {line}");
        run(line);
    }
    true
}
