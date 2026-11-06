//! The per-machine text files on the boot volume that the server kernel honours so far: `hostname`, `settings`
//! (`deny-term-net`, `no-msi`, `keymap`, `irq-cpu N`), `start` (the boot script, rerun by the `start` command) and `debug` (`kernel-update`, `reboot-on-crash`, `reboot-delay`, `watchdog`, `crash-on-boot`). Same files and names as the embedded kernel (docs/config.md); a FOSFS v4
//! volume keeps them in `settings/`, the bare name in the root stays valid. Options the server kernel has no use for yet are
//! ignored silently.

use kernel_core::volumes;

static mut BUFFER: [u8; 4096] = [0; 4096];
static mut HOSTNAME: [u8; 15] = [0; 15];
static mut HOSTNAME_LENGTH: usize = 0;
static mut DENY_TERM_NET: bool = false;
static mut KERNEL_UPDATE: bool = false;
static mut IRQ_CPU: usize = 0;
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

/// The `keymap` shell command: changes what USB and PS/2 keyboards type until the next boot.
pub fn set_swedish(on: bool) {
    unsafe { SWEDISH = on };
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

/// `irq-cpu N` (settings): the CPU device interrupts (MSI and I/O APIC pins) are sent to; 0, the boot CPU, by default.
pub fn irq_cpu() -> usize {
    unsafe { IRQ_CPU }
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

/// Reads `name` into the shared buffer: from the boot volume (`settings/NAME`, then the root), else any other FOSFS volume,
/// else the root of the first FAT32/exFAT disk that has it (a stick a PC can edit), as the embedded kernel does.
pub(crate) fn read(name: &str) -> Option<&'static str> {
    let buffer = unsafe { &mut *core::ptr::addr_of_mut!(BUFFER) };
    let mut path = [0u8; 40];
    let length = name.len().min(31);
    path[..9].copy_from_slice(b"settings/");
    path[9..9 + length].copy_from_slice(&name.as_bytes()[..length]);
    let folder = &path[..9 + length];
    // `settings` mounted from a config volume (fstab): its copy overrides the root's.
    if let Some((label, rest)) = kernel_core::mounts::lookup(folder) {
        let volume = volumes::volume_labelled(label.as_bytes());
        match volume {
            Some((_, volume)) if matches!(volume.file_length(rest), Ok(Some(_))) => {
                println!("{name}: read from @{} (mounted at settings).", core::str::from_utf8(label.as_bytes()).unwrap_or("?"));
                return match volume.read(rest, buffer) {
                    Ok(Some(length)) => core::str::from_utf8(&buffer[..length]).ok(),
                    _ => None,
                };
            }
            None => {
                let length = kernel_core::tape_fs::read(folder, buffer);
                if length >= 0 {
                    println!("{name}: read from the FAT volume mounted at settings.");
                    return core::str::from_utf8(&buffer[..length as usize]).ok();
                }
            }
            _ => {}
        }
    }
    let has = |volume: &kernel_core::fosfs::Volume| {
        [folder, name.as_bytes()].into_iter().find(|file| matches!(volume.file_length(file), Ok(Some(_))))
    };
    let found = volumes::boot_volume().and_then(|volume| has(&volume).map(|file| (volume, file))).or_else(|| {
        (1..=crate::block::count()).find_map(|number| {
            let volume = volumes::volume_on(number).ok()?;
            has(&volume).map(|file| (volume, file))
        })
    });
    let Some((volume, file)) = found else { return read_fat(name, buffer) };
    match volume.read(file, buffer) {
        Ok(Some(length)) => core::str::from_utf8(&buffer[..length]).ok(),
        _ => None,
    }
}

/// The fallback when no FOSFS volume has `name`: the root of the first FAT32/exFAT disk that does. Disks with a FOSFS
/// volume are skipped, and a file longer than the buffer is ignored rather than cut.
fn read_fat(name: &str, buffer: &'static mut [u8; 4096]) -> Option<&'static str> {
    for number in 1..=crate::block::count() {
        if volumes::volume_on(number).is_ok() {
            continue;
        }
        let Some(mut fs) = kernel_core::fatvol::mount(number - 1) else { continue };
        let Ok(Some(entry)) = fs.entry(name.as_bytes()) else { continue };
        if entry.is_directory {
            continue;
        }
        if entry.size as usize > buffer.len() {
            println!("{name}: disk{number} has it but it is over {} bytes; ignored.", buffer.len());
            return None;
        }
        return match fs.read(&entry.file(), 0, &mut buffer[..entry.size as usize]) {
            Ok(length) => {
                println!("{name}: read from disk{number} (FAT/exFAT).");
                core::str::from_utf8(&buffer[..length]).ok()
            }
            Err(error) => {
                println!("{name}: cannot be read from disk{number} ({error:?}); ignored.");
                None
            }
        };
    }
    None
}

pub(crate) fn lines(text: &str) -> impl Iterator<Item = &str> {
    text.lines().map(str::trim).filter(|line| !line.is_empty() && !line.starts_with('#'))
}

/// Loads `hostname`, `settings` and `debug` from the boot volume (call after storage is up).
pub fn load() {
    load_fstab();
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
            if line.split_whitespace().next() == Some("irq-cpu") {
                let cpu = line.split_whitespace().nth(1).and_then(|word| word.parse::<usize>().ok()).unwrap_or(0);
                unsafe { IRQ_CPU = cpu };
                println!("settings: irq-cpu {cpu} -- device interrupts go to CPU {cpu} (CPU 0 if it is not running).");
            }
            if line.split_whitespace().next() == Some("no-msi") {
                unsafe { NO_MSI = true };
                println!("settings: no-msi -- devices are polled.");
            }
            if let Some(key @ ("foreground" | "background")) = line.split_whitespace().next() {
                let name = line.split_whitespace().nth(1).unwrap_or("");
                let (foreground, background) = if key == "foreground" { (Some(name), None) } else { (None, Some(name)) };
                if crate::fb::set_colours(foreground, background) {
                    println!("settings: {key} {name}.");
                } else {
                    println!("settings: `{line}` ignored (unknown colour, or no pixel console).");
                }
            }
            if line.split_whitespace().next() == Some("mouse-speed") {
                let percent = line.split_whitespace().nth(1).and_then(|word| word.parse::<u32>().ok());
                match percent {
                    Some(percent) if kernel_core::usb_pointer::set_speed_percent(percent) => println!("settings: mouse-speed {percent}% of the default pointer gain."),
                    _ => println!("settings: `{line}` ignored (mouse-speed PERCENT, 25-1000; 100 is the default)."),
                }
            }
            if let Some(key @ ("hwp" | "park-idle" | "rotate" | "serial-shell" | "gpio-remote" | "gpio-sim" | "gpio-deny" | "prefer-msix" | "replace-in-place")) = line.split_whitespace().next() {
                println!("settings: {key} is not supported by the server kernel yet; ignored.");
            }
            if line.split_whitespace().next() == Some("vnic") {
                crate::vnic::queue(line);
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

/// Runs the `start` file through `run`, one command per line. False if there is no such file.
pub fn run_start(mut run: impl FnMut(&str)) -> bool {
    let Some(text) = read("start") else { return false };
    // `read` shares one buffer, and the commands may read files themselves.
    let script = alloc::string::String::from(text);
    println!("start file: running {} command(s).", lines(&script).count());
    for line in lines(&script) {
        println!("start> {line}");
        run(line);
    }
    true
}

/// The `start` command, and the boot-time run: one at a time.
pub fn start(run: impl FnMut(&str)) {
    static RUNNING: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
    if RUNNING.swap(true, core::sync::atomic::Ordering::Relaxed) {
        return println!("start: the start file is already running.");
    }
    if !run_start(run) {
        println!("start: no start file on a FOSFS volume.");
    }
    RUNNING.store(false, core::sync::atomic::Ordering::Relaxed);
}

/// Fills the mount table from `fstab` before the other files are read, so a `settings` mount reaches them. A stick that is
/// still being enumerated gets up to 10 s; after that the boot volume's folders stand (`usb rescan` and `mount fstab` look again).
fn load_fstab() {
    let (mounted, _) = kernel_core::mounts::load_from(volumes::boot_volume());
    let absent = || {
        kernel_core::mounts::absent_with(|label| volumes::volume_labelled(label).is_some() || kernel_core::tape_fs::fat_labelled(label).is_some() || kernel_core::ext4vol::disk_labelled(label).is_some())
    };
    if mounted == 0 || absent() == 0 {
        return;
    }
    println!("fstab: waiting for a mounted volume (up to 10 s; the root's folders are used if it never appears).");
    for _ in 0..20 {
        crate::apic::busy_wait_ms(500);
        crate::usb::poll();
        if absent() == 0 {
            return;
        }
    }
    println!("fstab: {} mounted volume(s) still absent; the boot volume's folders stand.", absent());
}
