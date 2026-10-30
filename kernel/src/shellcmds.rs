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
    Command::new("memory", "memory", "physical memory, managed and free.", |_, _| memory()).alias(&["mem"]),
    Command::new("disks", "disks", "the disks the drivers found (the boot disk is disk1).", |_, _| disks()),
    Command::new("clear", "clear", "clears the screen.", |_, _| crate::print(format_args!("\x1b[2J"))),
    Command::new("reset", "reset", "restarts the machine (through the firmware, so a pending kernel slot is tried).", |_, _| restart()).alias(&["reboot"]),
    Command::new("shutdown", "shutdown", "powers off (a Pi 4 halts in its bootloader).", |_, _| power_off()).alias(&["halt"]),
    Command::new("about", "about", "what this system is.", |_, _| about()).alias(&["version"]),
    Command::new("hexdump", "hexdump ADDRESS [LENGTH <= 4096]", "dumps mapped memory (decimal or 0x hex).", |_, rest| {
        let mut words = rest.split_whitespace();
        let address = words.next().and_then(number);
        let length = words.next().and_then(number).unwrap_or(64);
        match address {
            Some(address) if length <= 4096 => hexdump(address, length as usize),
            _ => crate::println!("Usage: hexdump ADDRESS [LENGTH <= 4096]  (decimal or 0x hex)"),
        }
    }),
    Command::new("fsck", "fsck [diskN|@LABEL]", "checks a FOSFS volume's directory (default: the boot volume).", |_, rest| fsck(rest)),
    Command::new("nic", "nic [list|sniff [N]|promisc N on|off|peers|discover|use N|map|send|recv]", "the network interfaces: list, watch the frames, promiscuous mode, the other fos machines.", |_, rest| crate::nicshell::command(rest)),
    Command::new("usb", "usb [rescan|debug [off]]", "the USB controllers and what is plugged in; `rescan` looks again; `debug` adds PORTSC and port events.", |_, rest| crate::usbshell::command(rest)),
    Command::new("kup", "kup serial [BAUD]", "takes a kernel update over the console serial port.", |_, rest| crate::kupshell::command(rest)),
    #[cfg(target_arch = "x86_64")]
    Command::new("recv", "recv NAME", "receives a file over the console serial port (tools/send_file_serial.py).", |_, rest| crate::kupshell::recv(rest)),
    Command::new("audio", "audio [device|output|use|start|stop|capture|duplex|tone|modes] ...", "the audio devices, streams and quick checks (`audio help` lists them).", |_, rest| crate::audioshell::command(rest)),
    Command::new("run", "run NAME [ARGS] [&N]", "runs a tape (`NAME [ARGS]` does too); `&N` keeps it in the background on NIC N.", |host, rest| {
        if rest.is_empty() {
            crate::println!("run: which tape? (run NAME [ARGS])");
        } else {
            host.run_tape(rest);
        }
    }),
    Command::new("grow", "grow diskN", "makes the disk's FOSFS volume fill the disk (files are kept).", |_, rest| grow(rest)),
    Command::new("mount", "mount [PATH @LABEL | fstab]", "shows the mount table, mounts the volume @LABEL at the folder PATH of the boot volume, or reloads fstab.", |_, rest| mount(rest)),
    Command::new("umount", "umount PATH", "removes a mount; PATH is then the boot volume's own folder again.", |_, rest| umount(rest)),
    Command::new("label", "label [diskN|@LABEL [NAME|-]]", "shows or sets a FOSFS volume's label (@LABEL names a volume).", |_, rest| label(rest)),
    Command::new("format", "format diskN [NAME] [force] | format diskN exfat [LABEL]", "erases a disk's FOSFS volume, or makes it one exFAT volume for PCs; asks first (then `format diskN confirm`).", |_, rest| crate::formatshell::command(rest)),
    Command::new("debug", "debug [status|slots|crash|hang|video ...]", "kernel-update setting and the A/B slots; `crash` and `hang` test the crash record and the watchdog; `video` (x86 BIOS boots) the video mode.", |_, rest| debug(rest)),
    Command::new("gpio", "gpio [@HOST] [list|read N|set N 0|1|mode N in|out 0|1|altK|pull N none|up|down]", "the Raspberry Pi's GPIO pins, here or on another machine (needs the synckey file for @HOST).", |_, rest| crate::gpio_shell::command(rest)),
    Command::new("tapesync", "tapesync [HOST] [apply]", "pulls newer tapes from one or all peers (needs the synckey file).", |_, rest| crate::tapesync::command(rest.split_whitespace())),
    Command::new("sha256", "sha256 [diskN/]NAME...", "prints each file's SHA-256 and length (what tools/fosadm.py compares to the build, also over serial).", |_, rest| sha256(rest)),
    Command::new("pci", "pci [read B:D.F [OFFSET [LENGTH]] | write B:D.F OFFSET VALUE]", "lists PCI functions; read/write reach configuration space.", |_, rest| crate::diag::pci_command(rest.split_whitespace())),
    Command::new("cpuinfo", "cpuinfo", "what the CPU reports.", |_, _| crate::diag::cpuinfo()),
    Command::new("bootinfo", "bootinfo", "what the loader handed over.", |_, _| crate::diag::bootinfo()),
    Command::new("memmap", "memmap", "the firmware's memory map as the loader passed it.", |_, _| crate::diag::memmap()),
    Command::new("irqstat", "irqstat", "interrupt state (x86: the I/O APIC pins; Pi: the counts).", |_, _| crate::diag::irqstat()).alias(&["irqs"]),
    Command::new("mmio", "mmio r8|r16|r32|r64 ADDR | mmio w8|w16|w32|w64 ADDR VALUE", "reads or writes mapped memory and device registers.", |_, rest| crate::diag::mmio(rest.split_whitespace())),
    #[cfg(target_arch = "x86_64")]
    Command::new("io", "io r8|r16|r32 PORT | io w8|w16|w32 PORT VALUE", "raw I/O port access.", |_, rest| crate::diag::io(rest.split_whitespace())),
    Command::new("copy", "copy SRC NAME DST [NEWNAME] | HOST[:diskN]/NAME DST [NEWNAME] | SRC NAME HOST[:diskN]", "copies a file between volumes or to and from another machine (see `nic peers`).", |host, rest| {
        if !crate::netfs::copy_peer(rest) {
            local_copy(host, rest);
        }
    }),
    Command::new("sector", "sector diskN LBA [COUNT <= 4]", "raw read-only dump of disk sectors (a GPT header, the slot metadata).", |_, rest| sector(rest)),
    Command::new("start", "start", "runs the start file again (as at boot).", |_, _| start()),
    Command::new("dmesg", "dmesg", "the boot log.", |_, _| dmesg()),
    Command::new("echo", "echo TEXT", "prints its arguments.", |_, rest| crate::println!("{rest}")),
];

/// `label` lists every volume's label; `label diskN|@LABEL` shows one; `label diskN|@LABEL
/// NAME` sets it and `label diskN|@LABEL -` clears it. A label may be on one volume only.
fn label(rest: &str) {
    let mut words = rest.split_whitespace();
    let Some(target) = words.next() else {
        for number in 1..=crate::block::count() {
            match crate::storage::volume_on(number) {
                Ok(volume) => match volume.label() {
                    Ok(label) if !label.is_empty() => crate::println!("disk{number}: @{}", core::str::from_utf8(label.as_bytes()).unwrap_or("?")),
                    Ok(_) => crate::println!("disk{number}: (no label)"),
                    Err(error) => crate::println!("disk{number}: {error:?}"),
                },
                Err(crate::storage::VolumeError::NoPartition) | Err(crate::storage::VolumeError::NotFormatted) => crate::println!("disk{number}: (no FOSFS volume)"),
                Err(error) => crate::println!("disk{number}: {error}"),
            }
        }
        return;
    };
    let (number, volume) = if let Some(label) = target.strip_prefix('@') {
        match crate::storage::volume_labelled(label.as_bytes()) {
            Some(found) => found,
            None => return crate::println!("@{label}: no volume carries that label."),
        }
    } else if let Some(number) = target.strip_prefix("disk").and_then(|number| number.parse::<usize>().ok()) {
        match crate::storage::volume_on(number) {
            Ok(volume) => (number, volume),
            Err(error) => return crate::println!("disk{number}: {error}."),
        }
    } else {
        return crate::println!("Usage: label [diskN|@LABEL [NAME|-]]");
    };
    let Some(name) = words.next() else {
        match volume.label() {
            Ok(label) if !label.is_empty() => crate::println!("disk{number}: @{}", core::str::from_utf8(label.as_bytes()).unwrap_or("?")),
            Ok(_) => crate::println!("disk{number}: (no label)"),
            Err(error) => crate::println!("disk{number}: {error:?}"),
        }
        return;
    };
    let wanted = if name == "-" { "" } else { name };
    if !wanted.is_empty() {
        if !crate::fosfs::valid_name(wanted.as_bytes()) {
            return crate::println!("label: up to 16 lowercase letters, digits, `.`, `_` or `-`.");
        }
        if let Some((other, _)) = crate::storage::volume_labelled(wanted.as_bytes()).filter(|(other, _)| *other != number) {
            return crate::println!("label: disk{other} already carries @{wanted}.");
        }
    }
    match volume.set_label(wanted.as_bytes()) {
        Ok(()) if wanted.is_empty() => crate::println!("disk{number}: label cleared."),
        Ok(()) => crate::println!("disk{number} is now @{wanted}."),
        Err(error) => crate::println!("label: {error:?}"),
    }
}

fn mount(rest: &str) {
    let mut words = rest.split_whitespace();
    match (words.next(), words.next(), words.next()) {
        (None, ..) => {
            let mut any = false;
            crate::mounts::each(|path, label| {
                any = true;
                let (path, label) = (core::str::from_utf8(path).unwrap_or("?"), core::str::from_utf8(label).unwrap_or("?"));
                match crate::storage::volume_labelled(label.as_bytes()) {
                    Some((number, _)) => crate::println!("{path} -> @{label} (disk{number})"),
                    None => crate::println!("{path} -> @{label} (not present: {path}/ is the boot volume's own folder)"),
                }
            });
            if !any {
                crate::println!("No mounts (a `fstab` file on the boot volume lists them: PATH @LABEL per line).");
            }
        }
        (Some("fstab"), None, _) => {
            let (set, bad) = crate::mounts::load();
            crate::println!("fstab: {set} mount(s){}.", if bad > 0 { " and lines it could not use" } else { "" });
        }
        (Some(path), Some(label), None) if label.starts_with('@') => match crate::mounts::add(path.as_bytes(), label[1..].as_bytes()) {
            Ok(()) => crate::println!("{path} is now {label}{}.", if crate::storage::volume_labelled(label[1..].as_bytes()).is_some() { "" } else { " (not present yet: the boot volume's folder is used until it appears)" }),
            Err(crate::mounts::Error::BadPath) => crate::println!("mount: {path} is not a folder path (names of letters, digits, . _ - +, at most 48 characters)."),
            Err(crate::mounts::Error::BadLabel) => crate::println!("mount: {label} is not a volume label."),
            Err(crate::mounts::Error::Full) => crate::println!("mount: the table is full."),
        },
        _ => crate::println!("Usage: mount [PATH @LABEL | fstab]"),
    }
}

fn umount(rest: &str) {
    match rest.split_whitespace().next() {
        Some(path) if crate::mounts::remove(path.as_bytes()) => crate::println!("{path} is no longer a mount point."),
        Some(path) => crate::println!("umount: {path} is not mounted."),
        None => crate::println!("Usage: umount PATH"),
    }
}

fn debug(rest: &str) {
    let mut words = rest.split_whitespace();
    match words.next() {
        None | Some("status") | Some("slots") => {
            #[cfg(target_arch = "x86_64")]
            {
                let debug = crate::config::debug();
                crate::println!("reboot-on-crash {}, reboot-delay {} s, kernel-update {}, crash-on-boot {}", debug.reboot_on_crash, debug.reboot_delay_seconds, debug.kernel_update, debug.crash_on_boot);
                crate::debug::slot_summary(|arguments| crate::println!("{}", arguments));
            }
            #[cfg(target_arch = "aarch64")]
            {
                let debug = crate::crash::debug();
                crate::println!("reboot-on-crash {}, reboot-delay {} s, kernel-update {}, crash-on-boot {}, watchdog {:?}", debug.reboot_on_crash, debug.reboot_delay_seconds, debug.kernel_update, debug.crash_on_boot, debug.watchdog_seconds);
                crate::slots::show();
            }
        }
        Some("crash") => {
            crate::println!("Raising an undefined-instruction exception on purpose.");
            #[cfg(target_arch = "x86_64")]
            {
                crate::term::flush();
                unsafe { core::arch::asm!("ud2") };
            }
            #[cfg(target_arch = "aarch64")]
            unsafe {
                core::arch::asm!("udf #0")
            };
        }
        Some("hang") => {
            crate::println!("Spinning with interrupts on and no progress; the watchdog (if armed) should reset the machine.");
            #[cfg(target_arch = "x86_64")]
            crate::term::flush();
            loop {
                core::hint::spin_loop();
            }
        }
        #[cfg(target_arch = "x86_64")]
        Some("video") => crate::term::debug_video(words),
        Some(_) => crate::println!("debug: status | slots | crash | hang | video [skip|auto|size WxH]"),
    }
}

fn start() {
    #[cfg(target_arch = "x86_64")]
    {
        static RUNNING: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
        if RUNNING.swap(true, core::sync::atomic::Ordering::Relaxed) {
            return crate::println!("start: the start file is already running.");
        }
        if !crate::config::run_start(crate::term::run_line) {
            crate::println!("start: no start file on a FOSFS volume (or the FAT/exFAT root).");
        }
        RUNNING.store(false, core::sync::atomic::Ordering::Relaxed);
    }
    #[cfg(target_arch = "aarch64")]
    crate::shell::start_script(true);
}

fn dmesg() {
    // The Pi's early log also holds what the log replays onto the screen, so it goes to the serial console only.
    #[cfg(target_arch = "x86_64")]
    crate::print(format_args!("{}", crate::log::contents()));
    #[cfg(target_arch = "aarch64")]
    crate::dmesg();
}

fn restart() {
    crate::println!("Resetting.");
    #[cfg(target_arch = "x86_64")]
    {
        crate::term::flush_output();
        crate::debug::mark_clean_stop();
        crate::power::reset(crate::power::runtime_services());
    }
    #[cfg(target_arch = "aarch64")]
    crate::arch::aarch64::power::reset();
}

fn power_off() {
    crate::println!("Powering off.");
    #[cfg(target_arch = "x86_64")]
    {
        crate::term::flush_output();
        crate::debug::mark_clean_stop();
        crate::power::shutdown(crate::power::runtime_services());
        crate::println!("Shutdown returned.");
    }
    #[cfg(target_arch = "aarch64")]
    crate::arch::aarch64::power::off();
}

fn memory() {
    let stats = crate::memory::stats();
    let mib = |frames: u64| frames * crate::memory::FRAME / (1024 * 1024);
    crate::println!("Managed:   {} MiB ({} frames of 4 KiB)", mib(stats.frames), stats.frames);
    crate::println!("Free:      {} MiB ({} frames)", mib(stats.free), stats.free);
}

fn disks() {
    for index in 0..crate::block::count() {
        let disk = crate::block::get(index).unwrap();
        crate::println!(
            "disk{}: {} MiB {} {}",
            index + 1,
            disk.sectors / 2048,
            core::str::from_utf8(&disk.origin[..disk.origin_length]).unwrap_or("?"),
            core::str::from_utf8(&disk.model).unwrap_or("?").trim()
        );
    }
    match crate::storage::boot_disk() {
        Some(disk) => crate::println!("boot disk: disk{}", disk + 1),
        None => crate::println!("boot disk: not identified"),
    }
}

fn sha256(rest: &str) {
    let Some(staging) = crate::netfs::staging() else { return crate::println!("sha256: no memory for the transfer buffer.") };
    if rest.split_whitespace().next().is_none() {
        return crate::println!("Usage: sha256 [diskN/]NAME...");
    }
    for reference in rest.split_whitespace() {
        let read = crate::tape_fs::resolve_file(reference.as_bytes(), true).ok().and_then(|(volume, name)| volume.read(name, staging).ok().flatten());
        let Some(length) = read else {
            crate::println!("sha256: {reference}: not found");
            continue;
        };
        let digest = tape_sync::sha256(&staging[..length]);
        let mut hex = [0u8; 64];
        for (index, byte) in digest.iter().enumerate() {
            hex[index * 2] = b"0123456789abcdef"[usize::from(byte >> 4)];
            hex[index * 2 + 1] = b"0123456789abcdef"[usize::from(byte & 15)];
        }
        crate::println!("{}  {length}  {reference}", core::str::from_utf8(&hex).unwrap_or(""));
    }
}

fn sleep(rest: &str) {
    let seconds = rest.split_whitespace().next().and_then(|word| word.parse::<u64>().ok()).unwrap_or(1).min(60);
    let end = timer::ticks() + seconds * timer::TICK_HZ;
    while timer::ticks() < end {
        #[cfg(target_arch = "x86_64")]
        crate::debug::pet();
        #[cfg(target_arch = "aarch64")]
        crate::crash::pet();
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

/// A raw-disk dump to look at what is really on a disk (a GPT header, the slot metadata, a boot sector) when a driver or
/// loader misbehaves on hardware we cannot attach a debugger to.
fn sector(rest: &str) {
    // `sector diskN LBA [COUNT]`: a raw read-only dump, to look at what is really on a disk (a GPT header, the slot
    // metadata, a boot sector) when a driver or loader misbehaves on hardware we cannot attach a debugger to.
    const USAGE: &str = "Usage: sector diskN LBA [COUNT <= 4]   (decimal or 0x hex; read-only)";
    let mut words = rest.split_whitespace();
    let disk = words.next().and_then(|word| word.strip_prefix("disk")).and_then(|digits| digits.parse::<usize>().ok());
    let lba = words.next().and_then(number);
    let count = words.next().and_then(number).unwrap_or(1);
    let (Some(number), Some(lba)) = (disk, lba) else { return crate::println!("{USAGE}") };
    if number == 0 || count == 0 || count > 4 {
        return crate::println!("{USAGE}");
    }
    for offset in 0..count {
        let mut sector = [0u8; 512];
        if let Err(error) = crate::block::read(number - 1, lba + offset, &mut sector) {
            return crate::println!("disk{number} sector {}: read failed ({error:?}).", lba + offset);
        }
        crate::println!("disk{number} sector {}:", lba + offset);
        let mut skipped = false;
        for (row, bytes) in sector.chunks(16).enumerate() {
            // runs of identical rows (zeros, mostly) are folded into one "*" line, as hexdump does
            if row > 0 && row < 31 && bytes == &sector[(row - 1) * 16..row * 16] && bytes == &sector[(row + 1) * 16..(row + 2) * 16] {
                if !skipped {
                    crate::println!("*");
                    skipped = true;
                }
                continue;
            }
            skipped = false;
            crate::print(format_args!("{:03x}  ", row * 16));
            for byte in bytes {
                crate::print(format_args!("{byte:02x} "));
            }
            crate::print(format_args!(" "));
            for &byte in bytes {
                crate::print(format_args!("{}", if (0x20..0x7f).contains(&byte) { byte as char } else { '.' }));
            }
            crate::println!();
        }
    }
}

fn number(word: &str) -> Option<u64> {
    match word.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => word.parse().ok(),
    }
}

/// `grow diskN`: makes the disk's FOSFS volume fill the disk. A stick flashed from the 96 MiB image keeps a 32 MiB data
/// partition and leaves the rest of the stick unused; this moves the backup partition table to the real end of the disk,
/// extends the FOSFS partition to it and the volume to the partition. Files are kept (a FOSFS volume is extents addressed from
/// its start, so new space is just free space). Only a FOSFS partition that is the last on the disk can grow.
fn grow(rest: &str) {
    let Some(number) = rest.split_whitespace().next().and_then(|word| word.strip_prefix("disk")).and_then(|digits| digits.parse::<usize>().ok()) else {
        return crate::println!("Usage: grow diskN   (make the disk's FOSFS volume fill the disk; files are kept)");
    };
    if number.checked_sub(1).and_then(crate::block::get).is_none() {
        return crate::println!("disk{number}: no such disk (see `disks`).");
    }
    let disk = number - 1;
    let before = match crate::storage::volume_on(number) {
        Ok(volume) => volume.sectors,
        Err(error) => return crate::println!("grow: disk{number}: {error}."),
    };
    use crate::gpt::GrowError;
    let partition = match crate::gpt::grow_last_partition(disk, &fosfor_abi::FOSFS_TYPE_GUID) {
        Ok(partition) => partition,
        Err(GrowError::NothingToGrow) => {
            // The partition may already fill the disk while the volume does not (an interrupted grow): finish that.
            match crate::gpt::find(disk, &fosfor_abi::FOSFS_TYPE_GUID, |_| true) {
                Some(partition) if partition.sectors > before + 2048 => partition,
                _ => return crate::println!("disk{number}: the FOSFS volume ({} MiB) already fills the disk.", before / 2048),
            }
        }
        Err(GrowError::NoGpt) => return crate::println!("grow: disk{number} has no GPT this kernel understands."),
        Err(GrowError::NoPartition) => return crate::println!("grow: disk{number} has no FOSFS partition."),
        Err(GrowError::NotLast) => return crate::println!("grow: another partition lies after the FOSFS one; only the last partition can grow."),
        Err(GrowError::Disk) => return crate::println!("grow: disk{number}: reading or writing the partition table failed."),
    };
    let volume = match crate::fosfs::Volume::mount(disk, partition.start, partition.sectors).and_then(|volume| volume.grow(partition.sectors)) {
        Ok(volume) => volume,
        Err(error) => return crate::println!("grow: the partition grew but the volume could not ({error:?}); run `grow disk{number}` again."),
    };
    if crate::storage::boot_disk() == Some(disk) {
        crate::storage::remount_boot();
    }
    crate::println!("disk{number}: FOSFS volume grown from {} MiB to {} MiB; the files are unchanged.", before / 2048, volume.sectors / 2048);
}

#[cfg(target_arch = "x86_64")]
fn about() {
    crate::println!("Fosfor OS (fos) is a small educational 64-bit x86 operating system.");
    crate::println!("One kernel, booted by a UEFI loader or by the BIOS boot code.");
    crate::println!("Design: docs/design.md.");
}

#[cfg(target_arch = "aarch64")]
fn about() {
    crate::println!("Fosfor OS (aarch64 port): shell, FOSFS, tapes (`run NAME`).");
}

/// `xxd`-style dump: offset, hex bytes, printable characters.
pub(crate) fn dump_bytes(base: u64, bytes: &[u8]) {
    for (row, chunk) in bytes.chunks(16).enumerate() {
        crate::print(format_args!("{:#012x}: ", base + (row * 16) as u64));
        for byte in chunk {
            crate::print(format_args!("{byte:02x} "));
        }
        for _ in chunk.len()..16 {
            crate::print(format_args!("   "));
        }
        crate::print(format_args!("|"));
        for byte in chunk {
            crate::print(format_args!("{}", if (0x20..=0x7e).contains(byte) { *byte as char } else { '.' }));
        }
        crate::println!("|");
    }
}

/// Dumps memory (identity mapped), refusing pages that are not mapped instead of faulting.
fn hexdump(address: u64, length: usize) {
    let mut at = address;
    let end = address + length as u64;
    while at < end {
        let page_end = ((at & !0xfff) + 0x1000).min(end);
        if !crate::paging::is_mapped(at) {
            return crate::println!("{at:#x}: not mapped.");
        }
        let chunk = unsafe { core::slice::from_raw_parts(at as *const u8, (page_end - at) as usize) };
        dump_bytes(at, chunk);
        at = page_end;
    }
}

/// `fsck [diskN|@LABEL]`: the same volume names as the file commands.
fn fsck(rest: &str) {
    if rest.is_empty() {
        match crate::storage::boot_volume() {
            Some(volume) => crate::fosfs::report(&volume),
            None => crate::println!("fsck: no FOSFS volume is mounted (see `disks`)."),
        }
    } else if let Some(label) = rest.strip_prefix('@') {
        match crate::storage::volume_labelled(label.as_bytes()) {
            Some((_, volume)) => crate::fosfs::report(&volume),
            None => crate::println!("fsck: no volume carries the label @{label} (see `label`)."),
        }
    } else if let Some(number) = rest.strip_prefix("disk").and_then(|digits| digits.parse().ok()) {
        match crate::storage::volume_on(number) {
            Ok(volume) => crate::fosfs::report(&volume),
            Err(error) => crate::println!("fsck: {rest}: {error}."),
        }
    } else {
        crate::println!("Usage: fsck [diskN|@LABEL]");
    }
}

/// A copy between local volumes: x86 keeps its FAT-aware `copy`, the Pi uses the shared `cp`.
#[cfg(target_arch = "x86_64")]
fn local_copy(_: &mut dyn fosfor_shell::Host, rest: &str) {
    crate::term::copy_command(rest.split_whitespace());
}

#[cfg(target_arch = "aarch64")]
fn local_copy(host: &mut dyn fosfor_shell::Host, rest: &str) {
    if let Some(cp) = fosfor_shell::files::COMMANDS.iter().find(|command| command.name == "cp") {
        (cp.run)(host, rest);
    }
}
