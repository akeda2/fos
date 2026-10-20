//! The aarch64 shell: the line editor (`lineedit`, shared with x86) on the serial console, and the commands that need
//! only what the port has so far: files on FOSFS volumes, disks, memory, time. The x86 shell (`term.rs`) is tied to the
//! network terminal and the PC drivers; this one grows towards the same command set (tapes next) as modules are ported.

use crate::{block, fosfs, lineedit, memory, storage, timer};

/// Prompts, then serves the serial console forever: poll a byte, edit, run a finished line.
static mut START_SCRIPT: [u8; 4096] = [0; 4096];

/// Runs the boot volume's `start` file, one command per line (blank lines and `#` lines skipped), as on x86. A tape in it
/// holds the shell until it ends (Esc), so a long-running one belongs on the last line.
fn start_script(announce_missing: bool) {
    let Some(volume) = storage::boot_volume() else { return };
    let buffer = unsafe { &mut *core::ptr::addr_of_mut!(START_SCRIPT) };
    let length = match volume.read(b"start", buffer) {
        Ok(Some(length)) => length,
        Ok(None) => return if announce_missing { crate::println!("start: no start file on the boot volume.") },
        Err(error) => return crate::println!("start: cannot be read ({error:?})."),
    };
    let script = core::str::from_utf8(&buffer[..length]).unwrap_or("");
    let lines = || script.lines().map(str::trim).filter(|line| !line.is_empty() && !line.starts_with('#') && *line != "start");
    crate::println!("start file: running {} command(s).", lines().count());
    for line in lines() {
        crate::println!("start> {line}");
        execute(line);
    }
}

pub fn run() -> ! {
    lineedit::reset();
    start_script(false);
    crate::print(format_args!("\nType `help` for the commands.\nfos> "));
    loop {
        crate::usb_kbd::poll();
        match crate::arch::aarch64::pl011::read_byte().or_else(crate::usb_kbd::pop) {
            Some(byte) => {
                let mut line = [0u8; lineedit::LINE_MAX];
                if let Some(length) = lineedit::feed(byte, &mut line) {
                    if let Ok(text) = core::str::from_utf8(&line[..length]) {
                        execute(text.trim());
                    }
                    crate::print(format_args!("fos> "));
                }
            }
            // Background tapes (`run NAME &N`) get a pass; the CPU sleeps only when they are all idle.
            // The 100 Hz timer interrupt wakes this at least every 10 ms.
            None => {
                // Frames for the network service (discovery, kernel update) first; more may be waiting.
                if crate::netservice::poll() {
                    continue;
                }
                if !crate::tasks::pass() {
                    crate::cpu::sleep();
                }
            }
        }
    }
}

fn execute(line: &str) {
    let (word, rest) = line.split_once(' ').map_or((line, ""), |(word, rest)| (word, rest.trim()));
    match word {
        "" => {}
        "help" if !rest.is_empty() => {
            if !show_tape_help(rest) {
                crate::println!("help: {rest} has no help screen.");
            }
        }
        "help" => crate::println!(
            "help [NAME] version uptime mem disks ls [diskN] cat [diskN/|esp/]NAME write NAME TEXT\nrm NAME cp SRC DST cmp A B fsck [diskN] echo TEXT grow diskN sleep N clear keymap us|sv\nrun NAME [ARGS] [&N] ps kill N|NAME|all dmesg irqs usb nic [rx] slots debug\nkup serial [BAUD] reset acpi peek ADDR [N] poke ADDR VALUE (hex; bring-up)"
        ),
        "start" => start_script(true),
        "version" => crate::println!("Fosfor OS (aarch64 port): shell, FOSFS, tapes (`run NAME`)."),
        "uptime" => {
            let seconds = timer::micros() / 1_000_000;
            crate::println!("up {}:{:02}:{:02}", seconds / 3600, seconds / 60 % 60, seconds % 60);
        }
        "mem" | "memory" => {
            let stats = memory::stats();
            let mib = |frames: u64| frames * memory::FRAME / (1024 * 1024);
            crate::println!("Physical memory: {} MiB managed, {} MiB free.", mib(stats.frames), mib(stats.free));
        }
        "disks" => {
            for index in 0..block::count() {
                let disk = block::get(index).unwrap();
                crate::println!(
                    "disk{}: {} MiB, {} ({})",
                    index + 1,
                    disk.sectors / 2048,
                    core::str::from_utf8(&disk.model).unwrap_or("?").trim(),
                    core::str::from_utf8(&disk.origin[..disk.origin_length]).unwrap_or("?")
                );
            }
        }
        "ls" => list(rest),
        "cat" => cat(rest),
        "cp" | "copy" => copy(rest),
        "cmp" => compare(rest),
        "fsck" => match volume(rest) {
            Some(volume) => fosfs::report(&volume),
            None => crate::println!("fsck: no FOSFS volume there (use `fsck` or `fsck diskN`)."),
        },
        "sector" => sector(rest),
        "audio" => audio(rest),
        "date" => date(rest),
        "display" => display(rest),
        "run" => run_tape(rest),
        "ps" | "tasks" => crate::tasks::list(),
        "kill" => crate::tasks::kill(rest.split_whitespace().next()),
        "keymap" => match crate::usb_kbd::layout_named(rest) {
            Some(layout) => crate::usb_kbd::set_layout(layout),
            None => crate::println!("keymap: us or sv."),
        },
        "echo" => crate::println!("{rest}"),
        "acpi" => crate::arch::aarch64::acpi::list(|signature, address, length| {
            crate::println!("{} at {:#x}, {} bytes", core::str::from_utf8(&signature).unwrap_or("????"), address, length)
        }),
        "dmesg" => crate::dmesg(),
        "nic" if rest == "rx" => crate::genet::show_rx(false),
        "nic" if rest == "rx multicast" => crate::genet::show_rx(true),
        "nic" if rest == "promisc on" || rest == "promisc off" => crate::println!("Genet promiscuous {}: {}.", &rest[8..], if crate::genet::set_promiscuous(rest == "promisc on") { "ok" } else { "no NIC" }),
        "nic" => crate::genet::status(),
        "usb" => crate::println!("usb: {} new keyboard(s) or disk(s).", crate::usb_kbd::probe_all()),
        "recv" => recv_serial(rest),
        "kup" if rest == "serial" || rest.starts_with("serial ") => kup_serial(rest["serial".len()..].trim()),
        "slots" => crate::slots::show(),
        "debug" if matches!(rest, "" | "status" | "slots") => {
            crate::println!("kernel-update {}", crate::netservice::kernel_update_enabled());
            crate::slots::show();
        }
        "irqs" | "irqstat" => {
            let (uart, dropped) = crate::arch::aarch64::pl011::interrupt_counts();
            crate::println!("timer {}, uart {uart} (bytes dropped {dropped}), sd {}, genet {}", crate::gic::irq_ticks(), crate::sdhci::interrupt_count(), crate::genet::interrupt_count());
            for vector in 0..crate::msi::COUNT as u8 {
                if crate::msi::count(vector) != 0 {
                    crate::println!("msi {vector}: {}", crate::msi::count(vector));
                }
            }
        }
        "reset" | "reboot" => {
            crate::println!("Restarting.");
            crate::arch::aarch64::power::reset()
        }
        "grow" => grow(rest),
        "write" => write_file(rest),
        "rm" => remove_file(rest),
        "sleep" => {
            let seconds = rest.parse::<u64>().unwrap_or(1).min(60);
            let end = timer::ticks() + seconds * timer::TICK_HZ;
            while timer::ticks() < end {
                crate::cpu::sleep();
            }
            crate::println!("Slept {seconds} s.");
        }
        "clear" => crate::print(format_args!("\x1b[2J")),
        "peek" => peek(rest),
        "poke" => poke(rest),
        // `diskN/NAME ARGS`, `@LABEL/NAME` and `esp/NAME` run that tape without `run`, as on x86.
        other if other.contains('/') => run_tape(line),
        // A dotless file name is taken for a tape (`ed NAME`), as on x86.
        other if !other.contains('.') && storage::volume_with_file(other.as_bytes()).is_some() => run_tape(line),
        other => crate::println!("{other}: unknown command (try `help`)."),
    }
}

/// The volume `diskN` names, or the boot volume for an empty argument.
fn volume(argument: &str) -> Option<fosfs::Volume> {
    if argument.is_empty() {
        return storage::boot_volume();
    }
    let number = argument.strip_prefix("disk").and_then(|digits| digits.parse().ok())?;
    storage::volume_on(number).ok()
}

/// Splits `diskN/NAME` into its volume and the file name; a plain name is on the boot volume.
fn locate(name: &str) -> Option<(fosfs::Volume, &str)> {
    match name.split_once('/') {
        Some((disk, file)) => Some((volume(disk)?, file)),
        None => Some((storage::boot_volume()?, name)),
    }
}

/// `grow diskN`: makes the disk's FOSFS volume fill the disk (the last partition grows; the files are kept), as on x86.
fn grow(argument: &str) {
    let Some(number) = argument.strip_prefix("disk").and_then(|digits| digits.parse::<usize>().ok()) else {
        return crate::println!("Usage: grow diskN   (make the disk's FOSFS volume fill the disk; files are kept)");
    };
    if number == 0 || block::get(number - 1).is_none() {
        return crate::println!("disk{number}: no such disk (see `disks`).");
    }
    let disk = number - 1;
    let before = match storage::volume_on(number) {
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
    let volume = match fosfs::Volume::mount(disk, partition.start, partition.sectors).and_then(|volume| volume.grow(partition.sectors)) {
        Ok(volume) => volume,
        Err(error) => return crate::println!("grow: the partition grew but the volume could not ({error:?}); run `grow disk{number}` again."),
    };
    if storage::boot_disk() == Some(disk) {
        storage::remount_boot();
    }
    crate::println!("disk{number}: FOSFS volume grown from {} MiB to {} MiB; the files are unchanged.", before / 2048, volume.sectors / 2048);
}

/// Prints the `.fos_help` section of the tape file NAME (NAME or diskN/NAME) without running it. False if there is none.
fn show_tape_help(name: &str) -> bool {
    const SIZE: usize = 512 * 1024;
    let Some((volume, file)) = locate(name) else { return false };
    let Some(staging) = memory::allocate_contiguous(SIZE as u64 / memory::FRAME, None) else { return false };
    let image = unsafe { core::slice::from_raw_parts_mut(staging as *mut u8, SIZE) };
    let shown = match volume.read(file.as_bytes(), image) {
        Ok(Some(length)) => match fosfor_abi::elf::section(&image[..length], ".fos_help").and_then(|text| core::str::from_utf8(text).ok()) {
            Some(text) => {
                crate::println!("{text}");
                true
            }
            None => false,
        },
        _ => false,
    };
    memory::free(staging, SIZE as u64 / memory::FRAME);
    shown
}

/// `write NAME TEXT`: a small text file (NAME or diskN/NAME), as on x86.
fn write_file(rest: &str) {
    let (name, text) = rest.split_once(' ').map_or((rest, ""), |(name, text)| (name, text.trim_start()));
    let Some((volume, file)) = locate(name).filter(|_| !name.is_empty()) else {
        // A FAT32/exFAT volume (`esp/NAME`, `diskN/NAME`): the same call a tape's `fs_write` makes.
        if name.contains('/') {
            return match crate::tape_fs::write(name.as_bytes(), text.as_bytes()) {
                0 => crate::println!("Wrote {} bytes to {name}.", text.len()),
                code => crate::println!("write: {name}: failed (code {code}; an empty file cannot be made on FAT)."),
            };
        }
        return crate::println!("Usage: write NAME TEXT   (NAME, diskN/NAME or esp/NAME)");
    };
    match volume.write(file.as_bytes(), text.as_bytes()) {
        Ok(()) => crate::println!("Wrote {} bytes to {file}.", text.len()),
        Err(error) => crate::println!("write: {error:?}"),
    }
}

/// `rm NAME`: removes a file from a FOSFS volume (NAME or diskN/NAME).
fn remove_file(name: &str) {
    let Some((volume, file)) = locate(name).filter(|_| !name.is_empty()) else {
        if name.contains('/') {
            return match crate::tape_fs::remove(name.as_bytes()) {
                0 => crate::println!("Removed {name}."),
                _ => crate::println!("rm: {name}: no such file or no such volume."),
            };
        }
        return crate::println!("Usage: rm NAME   (NAME, diskN/NAME or esp/NAME)");
    };
    match volume.remove(file.as_bytes()) {
        Ok(true) => crate::println!("Removed {file}."),
        Ok(false) => crate::println!("rm: no such file."),
        Err(error) => crate::println!("rm: {error:?}"),
    }
}

/// `cp SRC DST`: copies a file (up to 256 KiB) between volumes: `NAME`, `diskN/NAME` or `esp/NAME` (FAT32/exFAT through the tape calls).
fn copy(rest: &str) {
    const SIZE: usize = 256 * 1024;
    let Some((from, to)) = rest.split_once(' ') else {
        crate::println!("cp: cp SRC DST (names are NAME, diskN/NAME or esp/NAME).");
        return;
    };
    let (from, to) = (from.trim(), to.trim());
    let Some(staging) = memory::allocate_contiguous(SIZE as u64 / memory::FRAME, None) else {
        return crate::println!("cp: no memory.");
    };
    let buffer = unsafe { core::slice::from_raw_parts_mut(staging as *mut u8, SIZE) };
    // FOSFS volumes are read and written directly; anything else (FAT) goes through the tape file calls.
    let read = match locate(from) {
        Some((source, name)) => match source.read(name.as_bytes(), buffer) {
            Ok(Some(length)) => Some(length),
            Ok(None) => {
                crate::println!("cp: {from}: no such file.");
                None
            }
            Err(error) => {
                crate::println!("cp: {from}: {error:?} (files over 256 KiB are not copied).");
                None
            }
        },
        None => match crate::tape_fs::read(from.as_bytes(), buffer) {
            length if length >= 0 => Some(length as usize),
            code => {
                crate::println!("cp: {from}: not readable (code {code}).");
                None
            }
        },
    };
    match read {
        Some(length) => {
            let written = match locate(to) {
                Some((target, name)) => target.write(name.as_bytes(), &buffer[..length]).map_err(|_| ()),
                None => match crate::tape_fs::write(to.as_bytes(), &buffer[..length]) {
                    0 => Ok(()),
                    _ => Err(()),
                },
            };
            match written {
                Ok(()) => crate::println!("cp: {length} bytes to {to}."),
                Err(()) => crate::println!("cp: cannot write {to}."),
            }
        }
        None => {}
    }
    memory::free(staging, SIZE as u64 / memory::FRAME);
}

/// `cmp A B`: reads both files (up to 256 KiB) and says whether they are byte-identical; a read-back check of `cp`.
fn compare(rest: &str) {
    const HALF: usize = 256 * 1024;
    let Some((a, b)) = rest.split_once(' ') else {
        return crate::println!("cmp: cmp A B (names are NAME or diskN/NAME).");
    };
    let (Some((va, na)), Some((vb, nb))) = (locate(a.trim()), locate(b.trim())) else {
        return crate::println!("cmp: no FOSFS volume there.");
    };
    let Some(staging) = memory::allocate_contiguous(2 * HALF as u64 / memory::FRAME, None) else {
        return crate::println!("cmp: no memory.");
    };
    let all = unsafe { core::slice::from_raw_parts_mut(staging as *mut u8, 2 * HALF) };
    let (first, second) = all.split_at_mut(HALF);
    let result = (va.read(na.as_bytes(), first), vb.read(nb.as_bytes(), second));
    match result {
        (Ok(Some(x)), Ok(Some(y))) if x == y && first[..x] == second[..y] => crate::println!("cmp: identical, {x} bytes."),
        (Ok(Some(x)), Ok(Some(y))) => crate::println!("cmp: DIFFER ({x} and {y} bytes)."),
        _ => crate::println!("cmp: cannot read both files."),
    }
    memory::free(staging, 2 * HALF as u64 / memory::FRAME);
}

fn list(argument: &str) {
    let Some(volume) = volume(argument) else {
        // A FAT32/exFAT card a PC formatted: its root directory, through the same call a tape uses.
        let mut info: fosfor_abi::tape::FileInfo = unsafe { core::mem::zeroed() };
        let mut index = 0;
        while crate::tape_fs::list(argument.as_bytes(), index, &mut info) == 1 {
            let name = core::str::from_utf8(&info.name[..info.name_length as usize]).unwrap_or("?");
            let slash = if info.flags & fosfor_abi::tape::FILE_DIRECTORY != 0 { "/" } else { "" };
            crate::println!("{:>10}  {name}{slash}", info.length);
            index += 1;
        }
        if index == 0 {
            crate::println!("ls: no files, or no FOSFS/FAT volume there (use `ls` or `ls diskN`).");
        } else {
            crate::println!("{index} file(s).");
        }
        return;
    };
    let mut files = 0;
    let result = volume.for_each(|entry| {
        files += 1;
        crate::println!("{:>10}  {}", entry.length, core::str::from_utf8(entry.name()).unwrap_or("?"));
    });
    match result {
        Ok(()) => crate::println!("{files} file(s)."),
        Err(error) => crate::println!("ls: cannot read the directory ({error:?})."),
    }
}

/// Shows up to 16 KiB of a file as text (non-printable bytes as dots).
fn cat(name: &str) {
    static mut BUFFER: [u8; 16384] = [0; 16384];
    let buffer = unsafe { &mut *core::ptr::addr_of_mut!(BUFFER) };
    let Some((volume, name)) = locate(name) else {
        // `diskN/NAME` on a FAT32/exFAT card.
        let length = crate::tape_fs::read(name.as_bytes(), buffer);
        if length < 0 {
            crate::println!("cat: {name}: not found or not readable (code {length}).");
            return;
        }
        for &byte in &buffer[..length as usize] {
            let shown = if byte == b'\n' || (0x20..0x7f).contains(&byte) { byte } else { b'.' };
            crate::print(format_args!("{}", shown as char));
        }
        crate::println!();
        return;
    };
    match volume.read(name.as_bytes(), buffer) {
        Ok(Some(length)) => {
            for &byte in &buffer[..length] {
                let shown = if byte == b'\n' || (0x20..0x7f).contains(&byte) { byte } else { b'.' };
                crate::print(format_args!("{}", shown as char));
            }
            crate::println!();
        }
        Ok(None) => crate::println!("cat: {name}: no such file."),
        Err(error) => crate::println!("cat: {name}: {error:?} (files over 16 KiB are not shown)."),
    }
}

/// `sector diskN LBA [COUNT]`: a read-only hex dump of raw sectors (at most 4).
fn sector(rest: &str) {
    let mut words = rest.split_whitespace();
    let number = words.next().and_then(|word| word.strip_prefix("disk")).and_then(|digits| digits.parse::<usize>().ok());
    let lba = words.next().and_then(|word| word.parse::<u64>().ok());
    let count = words.next().map_or(Some(1), |word| word.parse::<usize>().ok()).filter(|count| (1..=4).contains(count));
    let (Some(number), Some(lba), Some(count)) = (number, lba, count) else {
        crate::println!("sector: sector diskN LBA [COUNT 1-4]");
        return;
    };
    let mut buffer = [0u8; 2048];
    let buffer = &mut buffer[..count * 512];
    if block::read(number.wrapping_sub(1), lba, buffer).is_err() {
        crate::println!("sector: cannot read disk{number} at {lba}.");
        return;
    }
    for (row, bytes) in buffer.chunks(16).enumerate() {
        crate::print(format_args!("{:04x}:", row * 16));
        for byte in bytes {
            crate::print(format_args!(" {byte:02x}"));
        }
        crate::println!();
    }
}

/// `run NAME [ARGS]`: loads a tape from the boot volume and runs it until it returns or Escape stops it.
fn run_tape(rest: &str) {
    // `run NAME ARGS &N`: a background tape (service) that owns NIC N.
    let (rest, service) = crate::tasks::split_suffix(rest);
    if service == Some(Err(())) {
        return crate::tasks::usage();
    }
    let (name, arguments) = rest.split_once(' ').map_or((rest, ""), |(name, arguments)| (name, arguments.trim()));
    if name.is_empty() {
        crate::println!("run: which tape? (run NAME [ARGS])");
        return;
    }
    // `run NAME help`: a tape with a `.fos_help` section shows it instead of running.
    if matches!(arguments, "help" | "-h" | "--help" | "?") && show_tape_help(name) {
        return;
    }
    if let Some(Ok(nic)) = service {
        return crate::tasks::start(name, arguments, nic);
    }
    if let Some((first, second)) = rest.split_once(" + ") {
        fn split(part: &str) -> (&str, &str) {
            let part = part.trim();
            part.split_once(' ').map_or((part, ""), |(name, arguments)| (name, arguments.trim()))
        }
        let (first, second) = (split(first), split(second));
        let loaded = crate::tape_aarch64::load_slot(first.0, 0).and_then(|one| Ok((one, crate::tape_aarch64::load_slot(second.0, 1)?)));
        // `calc 3 4 + 6 *` has a `+` too: it is a second tape only when the word after it names a tape that loads.
        if let Ok((one, two)) = loaded {
            match crate::tape_aarch64::run_two([(one, first.1), (two, second.1)]) {
                Ok(()) => crate::println!("Both tapes returned; back at the fos shell."),
                Err(reason) => crate::println!("run: {reason}."),
            }
            return;
        }
    }
    match crate::tape_aarch64::load(name).and_then(|entry| crate::tape_aarch64::run(entry, arguments)) {
        Ok(()) => crate::println!("Tape returned; back at the fos shell."),
        Err(reason) => crate::println!("run: {name}: {reason}."),
    }
}

/// Applies the `settings` file's `keymap us|sv` line from the boot volume (the only setting this port reads so far).
/// Reads the `settings` file; returns whether it says `no-msi` (leave PCIe devices on polling).
pub fn apply_settings() -> bool {
    let Some(volume) = storage::boot_volume() else { return false };
    let mut buffer = [0u8; 1024];
    let Ok(Some(length)) = volume.read(b"settings", &mut buffer) else { return false };
    let mut no_msi = false;
    for line in core::str::from_utf8(&buffer[..length]).unwrap_or("").lines() {
        let mut words = line.split_whitespace();
        let first = words.next();
        no_msi |= first == Some("no-msi");
        if first == Some("keymap") {
            match words.next().and_then(crate::usb_kbd::layout_named) {
                Some(layout) => {
                    crate::usb_kbd::set_layout(layout);
                    crate::println!("settings: {}.", line.trim());
                }
                None => crate::println!("settings: `{}` ignored (keymap us or sv).", line.trim()),
            }
        }
    }
    no_msi
}

fn hex(word: Option<&str>) -> Option<u64> {
    u64::from_str_radix(word?.trim_start_matches("0x"), 16).ok()
}

/// `peek ADDR [N]`: N 32-bit words (default 4, at most 64) from a physical address, mapped as device memory on demand.
/// A bring-up tool: an address with nothing behind it faults and the exception report says so.
fn peek(rest: &str) {
    let mut words = rest.split_whitespace();
    let Some(address) = hex(words.next()).map(|address| address & !3) else {
        crate::println!("peek ADDR [N]  (hex)");
        return;
    };
    let count = words.next().and_then(|word| word.parse::<u64>().ok()).unwrap_or(4).clamp(1, 64);
    crate::arch::aarch64::paging::map_device(address, count * 4);
    for index in 0..count {
        if index % 4 == 0 {
            crate::print(format_args!("{}{:#012x}:", if index == 0 { "" } else { "\n" }, address + index * 4));
        }
        let value = unsafe { core::ptr::read_volatile((address + index * 4) as *const u32) };
        crate::print(format_args!(" {value:08x}"));
    }
    crate::println!();
}

/// `poke ADDR VALUE`: one 32-bit device write.
fn poke(rest: &str) {
    let mut words = rest.split_whitespace();
    let (Some(address), Some(value)) = (hex(words.next()).map(|address| address & !3), hex(words.next())) else {
        crate::println!("poke ADDR VALUE  (hex)");
        return;
    };
    crate::arch::aarch64::paging::map_device(address, 4);
    unsafe { core::ptr::write_volatile(address as *mut u32, value as u32) };
}

/// The console UART as a `kup_serial` link.
struct Console;

impl crate::kup_serial::Link for Console {
    fn read_byte(&mut self) -> Option<u8> {
        crate::arch::aarch64::pl011::read_byte()
    }
    fn write_byte(&mut self, byte: u8) {
        crate::arch::aarch64::pl011::write_raw(byte);
    }
    fn micros(&self) -> u64 {
        timer::micros()
    }
}

/// `kup serial`: takes a kernel image from `tools/send_kernel_serial.py` over the console UART into the inactive slot, then
/// restarts to try it (the loader falls back to this kernel unless the new one reaches its milestone).
fn kup_serial(speed: &str) {
    let baud = if speed.is_empty() { 0 } else { speed.parse::<u32>().unwrap_or(0) };
    if !speed.is_empty() && !(9_600..=3_000_000).contains(&baud) {
        return crate::println!("kup serial [BAUD]: BAUD is 9600 to 3000000 (the sender must be told the same).");
    }
    const STAGE_BYTES: usize = 4 * 1024 * 1024;
    let Some(staging) = memory::allocate_contiguous(STAGE_BYTES as u64 / memory::FRAME, None) else {
        return crate::println!("kup: no memory for the image.");
    };
    let buffer = unsafe { core::slice::from_raw_parts_mut(staging as *mut u8, STAGE_BYTES) };
    if baud == 0 {
        crate::println!("KUP serial: ready (the sender starts within 30 s).");
    } else {
        crate::println!("KUP serial: ready at {baud} baud (the sender switches too and starts within 30 s).");
        crate::arch::aarch64::pl011::drain();
        timer::busy_wait_ms(50);
        if !crate::arch::aarch64::pl011::set_baud(baud) {
            return crate::println!("kup serial: that speed is not possible with this UART clock.");
        }
    }
    let result = crate::kup_serial::serve(&mut Console, buffer);
    if baud != 0 {
        crate::arch::aarch64::pl011::drain();
        crate::arch::aarch64::pl011::set_baud(0);
    }
    memory::free(staging, STAGE_BYTES as u64 / memory::FRAME);
    match result {
        Ok(slot) => {
            crate::println!("KUP serial: image verified and written to slot {slot} (pending); restarting to try it.");
            timer::busy_wait_ms(200);
            crate::arch::aarch64::power::reset()
        }
        Err(message) => crate::println!("KUP serial: {message}; nothing was written."),
    }
}

/// `recv NAME [BAUD]`: takes one file from `tools/send_file_serial.py` over the console UART (same framing and CRC as
/// `kup serial`) and writes it as NAME (or diskN/NAME). Puts a tape on a Pi with no network file service and no SD swap.
fn recv_serial(rest: &str) {
    let mut words = rest.split_whitespace();
    let Some(name) = words.next() else { return crate::println!("Usage: recv NAME [BAUD]   (then run tools/send_file_serial.py PORT FILE NAME)") };
    let baud = words.next().map_or(0, |word| word.parse::<u32>().unwrap_or(1));
    if baud != 0 && !(9_600..=3_000_000).contains(&baud) {
        return crate::println!("recv: BAUD is 9600 to 3000000 (the sender must be told the same).");
    }
    const STAGE_BYTES: usize = 2 * 1024 * 1024;
    let Some(staging) = memory::allocate_contiguous(STAGE_BYTES as u64 / memory::FRAME, None) else {
        return crate::println!("recv: no memory.");
    };
    let buffer = unsafe { core::slice::from_raw_parts_mut(staging as *mut u8, STAGE_BYTES) };
    if baud == 0 {
        crate::println!("KUP serial: ready for file {name} (the sender starts within 30 s).");
    } else {
        crate::println!("KUP serial: ready for file {name} at {baud} baud (the sender switches too and starts within 30 s).");
        crate::arch::aarch64::pl011::drain();
        timer::busy_wait_ms(50);
        if !crate::arch::aarch64::pl011::set_baud(baud) {
            memory::free(staging, STAGE_BYTES as u64 / memory::FRAME);
            return crate::println!("recv: that speed is not possible with this UART clock.");
        }
    }
    let result = crate::kup_serial::serve_file(&mut Console, buffer, |data| match crate::tape_fs::write(name.as_bytes(), data) {
        0 => Ok(()),
        _ => Err("the file could not be written (bad name, no space, or no such volume)"),
    });
    if baud != 0 {
        crate::arch::aarch64::pl011::drain();
        crate::arch::aarch64::pl011::set_baud(0);
    }
    memory::free(staging, STAGE_BYTES as u64 / memory::FRAME);
    match result {
        Ok(length) => crate::println!("recv: {length} bytes written to {name}."),
        Err(message) => crate::println!("recv: {message}; nothing was written."),
    }
}

/// `audio` (status), `audio modes`, `audio tone [HZ] [SECONDS]`: a first look at USB audio on this port.
fn audio(rest: &str) {
    use crate::audio as ua;
    let mut words = rest.split_whitespace();
    match words.next() {
        None => ua::status(),
        Some("modes") => ua::print_modes(),
        Some("tone") => {
            let frequency = words.next().and_then(|w| w.parse::<u32>().ok()).unwrap_or(440).clamp(20, 20_000);
            let seconds = words.next().and_then(|w| w.parse::<u64>().ok()).unwrap_or(3).clamp(1, 60);
            let rate = 48_000;
            if let Err(reason) = ua::start(rate, false, true) {
                return crate::println!("audio: {reason}.");
            }
            let Some((channels, slot)) = ua::playback_format() else { return };
            let (channels, slot) = (usize::from(channels), usize::from(slot));
            let frame_bytes = channels * slot;
            let step = (u64::from(frequency) << 16) / u64::from(rate);
            let mut phase = 0u64;
            let end = timer::ticks() + seconds * timer::TICK_HZ;
            let mut chunk = [0u8; 2048];
            while timer::ticks() < end {
                crate::usb_kbd::poll();
                ua::poll();
                let frames = (chunk.len() / frame_bytes).min(ua::playback_space() / frame_bytes);
                if frames == 0 {
                    continue;
                }
                for frame in chunk[..frames * frame_bytes].chunks_mut(frame_bytes) {
                    let half = (phase & 0x7fff) as i64;
                    let magnitude = (half * (0x8000 - half)) >> 13;
                    let sample = (if phase & 0x8000 != 0 { -magnitude } else { magnitude }) / 4;
                    phase = (phase + step) & 0xffff;
                    let value = sample << (8 * slot as u32 - 16);
                    for sample_slot in frame.chunks_mut(slot) {
                        for (index, byte) in sample_slot.iter_mut().enumerate() {
                            *byte = (value >> (8 * index)) as u8;
                        }
                    }
                }
                ua::write(&chunk[..frames * frame_bytes]);
            }
            ua::status();
            ua::stop();
        }
        Some("capture") => {
            let seconds = words.next().and_then(|w| w.parse::<u64>().ok()).unwrap_or(3).clamp(1, 30);
            if let Err(reason) = ua::start(48_000, true, false) {
                return crate::println!("audio: {reason}.");
            }
            let Some((channels, slot)) = ua::capture_format() else { return };
            let (channels, slot) = (usize::from(channels), usize::from(slot));
            let frame_bytes = channels * slot;
            let mut data = [0u8; 4096];
            for second in 1..=seconds {
                let mut peaks = [0u32; 32];
                let mut carry = 0usize;
                let mut frames = 0u64;
                let end = timer::ticks() + timer::TICK_HZ;
                while timer::ticks() < end {
                    crate::usb_kbd::poll();
                    ua::poll();
                    let room = (data.len() - carry) / frame_bytes * frame_bytes;
                    let total = carry + ua::read(&mut data[carry..carry + room]);
                    let whole = total / frame_bytes * frame_bytes;
                    for frame in data[..whole].chunks(frame_bytes) {
                        for (channel, sample) in frame.chunks(slot).enumerate().take(32) {
                            let mut value = 0i64;
                            for (index, &byte) in sample.iter().enumerate() {
                                value |= i64::from(byte) << (8 * index);
                            }
                            let bits = 8 * slot as u32;
                            if value >> (bits - 1) != 0 {
                                value -= 1 << bits;
                            }
                            // report in 16-bit units whatever the slot width
                            let magnitude = (value.unsigned_abs() >> (bits - 16)) as u32;
                            peaks[channel] = peaks[channel].max(magnitude);
                        }
                        frames += 1;
                    }
                    data.copy_within(whole..total, 0);
                    carry = total - whole;
                }
                crate::print(format_args!("[{second:>2} s] {frames} frames; peaks (of 32768):"));
                for peak in &peaks[..channels.min(32)] {
                    crate::print(format_args!(" {peak}"));
                }
                crate::println!();
            }
            ua::status();
            ua::stop();
        }
        Some(_) => crate::println!("Usage: audio | audio modes | audio tone [HZ] [SECONDS] | audio capture [SECONDS]"),
    }
}

/// `display` asks the Pi's VideoCore firmware how many HDMI outputs it drives and what each is set to (read-only: it selects
/// a display for the questions, then selects display 0 again). Meaningful on a Pi only; elsewhere the mailbox does not answer.
fn display(rest: &str) {
    use crate::mailbox::*;
    if crate::arch::aarch64::gic::bases().0 != 0xff84_1000 {
        println!("display: the VideoCore mailbox exists on a Raspberry Pi 4 only.");
        return;
    }
    if rest.starts_with("test") {
        let display: u32 = rest[4..].trim().parse().unwrap_or(1);
        match framebuffer(display, 1920, 1080) {
            Some((base, size, pitch, _, _)) => {
                println!("display {display}: framebuffer {base:#x}, {size} bytes, pitch {pitch}; drawing colour bars.");
                crate::paging::map_framebuffer(base, size);
                let (width, rows) = (pitch as u64 / 4, size / u64::from(pitch));
                let colours = [0xffffff, 0xffff00, 0x00ffff, 0x00ff00, 0xff00ff, 0xff0000, 0x0000ff, 0x808080];
                for y in 0..rows {
                    for x in 0..width {
                        let colour = colours[(x * 8 / width) as usize];
                        unsafe { ((base + (y * width + x) * 4) as *mut u32).write_volatile(colour) };
                    }
                }
            }
            None => println!("display {display}: the firmware gave no framebuffer."),
        }
        return;
    }
    let mut count = [0u32];
    if tag(GET_NUM_DISPLAYS, &[], &mut count).is_none() {
        println!("display: no VideoCore mailbox answer (not a Pi, or the tag is unknown).");
        return;
    }
    println!("display: firmware reports {} display(s).", count[0]);
    for index in 0..count[0].clamp(2, 4) {
        let mut selected = [0u32];
        if tag(SET_DISPLAY_NUM, &[index], &mut selected).is_none() {
            println!("  {index}: select refused");
            continue;
        }
        let mut id = [0u32];
        let id_known = tag(GET_DISPLAY_ID, &[index], &mut id).is_some();
        println!("  {index}: select answered {}, display id {}", selected[0], if id_known { id[0] as i64 } else { -1 });
        let (mut physical, mut virtual_size, mut depth, mut pitch) = ([0u32; 2], [0u32; 2], [0u32], [0u32]);
        let ok = tag(GET_PHYSICAL_SIZE, &[], &mut physical).is_some()
            && tag(GET_VIRTUAL_SIZE, &[], &mut virtual_size).is_some()
            && tag(GET_DEPTH, &[], &mut depth).is_some()
            && tag(GET_PITCH, &[], &mut pitch).is_some();
        if ok {
            println!("  {index}: {}x{} (buffer {}x{}), {} bpp, pitch {}", physical[0], physical[1], virtual_size[0], virtual_size[1], depth[0], pitch[0]);
        } else {
            println!("  {index}: selected, but the size tags got no answer");
        }
    }
    let _ = tag(SET_DISPLAY_NUM, &[0], &mut [0u32]);
}

/// `date` shows the clock; `date set YYYY-MM-DD HH:MM[:SS]` sets it (local time). The Pi has no RTC, so it starts unset.
fn date(rest: &str) {
    let mut words = rest.split_whitespace();
    match words.next() {
        None => match crate::clock::now() {
            Some(time) => crate::println!("{} (local time, software clock)", crate::clock::Display(time)),
            None => crate::println!("The clock is not set (no RTC here). `date set YYYY-MM-DD HH:MM`, or run the clock tape."),
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
