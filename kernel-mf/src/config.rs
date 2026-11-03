//! The per-machine text files on the boot volume that the server kernel honours so far: `hostname`, `settings`
//! (`deny-term-net`) and `debug` (`kernel-update`, `reboot-on-crash`, `reboot-delay`, `watchdog`, `crash-on-boot`). Same files and names as the embedded kernel (docs/config.md); a FOSFS v4
//! volume keeps them in `settings/`, the bare name in the root stays valid. Options the server kernel has no use for yet are
//! ignored silently.

use kernel_core::volumes;

static mut BUFFER: [u8; 4096] = [0; 4096];
static mut HOSTNAME: [u8; 15] = [0; 15];
static mut HOSTNAME_LENGTH: usize = 0;
static mut DENY_TERM_NET: bool = false;
static mut KERNEL_UPDATE: bool = false;
static mut REBOOT_ON_CRASH: bool = false;
static mut REBOOT_DELAY: u32 = 3;
static mut CRASH_ON_BOOT: bool = false;
static mut WATCHDOG: Option<u32> = None;
static mut SWEDISH: bool = false;
static mut NO_MSI: bool = false;

/// `keymap sv` (or `se`, `fi`) in `settings`: what USB keyboards type. US otherwise.
pub fn swedish() -> bool {
    unsafe { SWEDISH }
}

/// `no-msi` in `settings`: devices keep polling instead of taking message-signalled interrupts.
pub fn no_msi() -> bool {
    unsafe { NO_MSI }
}

/// The base name from the `hostname` file, if there is one.
pub fn hostname() -> Option<&'static str> {
    unsafe { (HOSTNAME_LENGTH != 0).then(|| core::str::from_utf8(&(&*core::ptr::addr_of!(HOSTNAME))[..HOSTNAME_LENGTH]).unwrap_or("")) }
}

pub fn deny_term_net() -> bool {
    unsafe { DENY_TERM_NET }
}

/// `kernel-update` in the `debug` file: KUP is answered only when it is there.
pub fn kernel_update() -> bool {
    unsafe { KERNEL_UPDATE }
}

/// `reboot-on-crash`: reset after a fatal exception instead of halting.
pub fn reboot_on_crash() -> bool {
    unsafe { REBOOT_ON_CRASH }
}

/// `reboot-delay N`: seconds the crash report stays up before the reset.
pub fn reboot_delay() -> u32 {
    unsafe { REBOOT_DELAY }
}

/// `crash-on-boot` (tests only).
pub fn crash_on_boot() -> bool {
    unsafe { CRASH_ON_BOOT }
}

/// `watchdog [N]`: seconds without progress before the reset.
pub fn watchdog_seconds() -> Option<u32> {
    unsafe { WATCHDOG }
}

fn read(name: &str) -> Option<&'static str> {
    let volume = volumes::boot_volume()?;
    let buffer = unsafe { &mut *core::ptr::addr_of_mut!(BUFFER) };
    let mut path = [0u8; 40];
    path[..9].copy_from_slice(b"settings/");
    let length = name.len().min(31);
    path[9..9 + length].copy_from_slice(&name.as_bytes()[..length]);
    let length = match volume.read(&path[..9 + length], buffer) {
        Ok(Some(length)) => length,
        _ => match volume.read(name.as_bytes(), buffer) {
            Ok(Some(length)) => length,
            _ => return None,
        },
    };
    core::str::from_utf8(&buffer[..length]).ok()
}

fn lines(text: &str) -> impl Iterator<Item = &str> {
    text.lines().map(str::trim).filter(|line| !line.is_empty() && !line.starts_with('#'))
}

/// Loads `hostname`, `settings` and `debug` from the boot volume (call after storage is up).
pub fn load() {
    if let Some(text) = read("hostname") {
        let name = text.lines().next().unwrap_or("").trim();
        let take = name.len().min(15);
        unsafe {
            (&mut *core::ptr::addr_of_mut!(HOSTNAME))[..take].copy_from_slice(&name.as_bytes()[..take]);
            HOSTNAME_LENGTH = take;
        }
        println!("hostname file: this machine announces itself as {}1.", &name[..take]);
    }
    if let Some(text) = read("settings") {
        for line in lines(text) {
            let mut words = line.split_whitespace();
            if words.next() == Some("keymap") && matches!(words.next(), Some("sv" | "se" | "fi")) {
                unsafe { SWEDISH = true };
                println!("settings: keymap sv -- USB keyboards type the Swedish/Finnish layout.");
            }
            if line.split_whitespace().next() == Some("no-msi") {
                unsafe { NO_MSI = true };
                println!("settings: no-msi -- devices are polled.");
            }
            if line.split_whitespace().next() == Some("deny-term-net") {
                unsafe { DENY_TERM_NET = true };
                println!("settings: deny-term-net -- the network terminal is closed.");
            }
        }
    }
    if let Some(text) = read("debug") {
        for line in lines(text) {
            let mut words = line.split_whitespace();
            let number = |word: Option<&str>, default: u32| word.and_then(|w| w.parse().ok()).unwrap_or(default);
            match words.next() {
                Some("kernel-update") => {
                    unsafe { KERNEL_UPDATE = true };
                    println!("debug: kernel-update -- the network kernel-update service (KUP) is on.");
                }
                Some("reboot-on-crash") => unsafe { REBOOT_ON_CRASH = true },
                Some("reboot-delay") => unsafe { REBOOT_DELAY = number(words.next(), 3).min(60) },
                Some("crash-on-boot") => unsafe { CRASH_ON_BOOT = true },
                Some("watchdog") => unsafe { WATCHDOG = Some(number(words.next(), 60)) },
                _ => println!("debug: unknown line `{line}` ignored."),
            }
        }
    }
}
