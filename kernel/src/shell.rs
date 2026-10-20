//! The aarch64 shell: the line editor (`lineedit`, shared with x86) on the serial console, and the commands that need
//! only what the port has so far: files on FOSFS volumes, disks, memory, time. The x86 shell (`term.rs`) is tied to the
//! network terminal and the PC drivers; this one grows towards the same command set (tapes next) as modules are ported.

use crate::{block, fosfs, lineedit, memory, storage, timer};

/// Prompts, then serves the serial console forever: poll a byte, edit, run a finished line.
pub fn run() -> ! {
    lineedit::reset();
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
            // The 100 Hz timer interrupt wakes this at least every 10 ms.
            None => crate::cpu::sleep(),
        }
    }
}

fn execute(line: &str) {
    let (word, rest) = line.split_once(' ').map_or((line, ""), |(word, rest)| (word, rest.trim()));
    match word {
        "" => {}
        "help" => crate::println!("help version uptime mem disks ls [diskN] cat [diskN/]NAME cp SRC DST cmp A B fsck [diskN] echo TEXT keymap us|sv run NAME [ARGS]\ndmesg irqs usb nic [rx] slots kup serial [BAUD] reset acpi peek ADDR [N] poke ADDR VALUE (hex; bring-up tools)"),
        "version" => crate::println!("Fosfor OS (aarch64 port): shell, FOSFS, tapes (`run NAME`)."),
        "uptime" => {
            let seconds = timer::micros() / 1_000_000;
            crate::println!("up {}:{:02}:{:02}", seconds / 3600, seconds / 60 % 60, seconds % 60);
        }
        "mem" => {
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
        "cp" => copy(rest),
        "cmp" => compare(rest),
        "fsck" => match volume(rest) {
            Some(volume) => fosfs::report(&volume),
            None => crate::println!("fsck: no FOSFS volume there (use `fsck` or `fsck diskN`)."),
        },
        "run" => run_tape(rest),
        "keymap" => match crate::usb_kbd::layout_named(rest) {
            Some(layout) => crate::usb_kbd::set_layout(layout),
            None => crate::println!("keymap: us or sv."),
        },
        "echo" => crate::println!("{rest}"),
        "acpi" => crate::arch::aarch64::acpi::list(|signature, address, length| {
            crate::println!("{} at {:#x}, {} bytes", core::str::from_utf8(&signature).unwrap_or("????"), address, length)
        }),
        "dmesg" => crate::dmesg(),
        "nic" if rest == "rx" => crate::genet::show_rx(),
        "nic" => crate::genet::status(),
        "usb" => crate::println!("usb: {} new keyboard(s).", crate::usb_kbd::probe_all()),
        "kup" if rest == "serial" || rest.starts_with("serial ") => kup_serial(rest["serial".len()..].trim()),
        "slots" => crate::slots::show(),
        "irqs" => {
            let (uart, dropped) = crate::arch::aarch64::pl011::interrupt_counts();
            crate::println!("timer {}, uart {uart} (bytes dropped {dropped}), sd {}, genet {}", crate::gic::irq_ticks(), crate::sdhci::interrupt_count(), crate::genet::interrupt_count());
            for vector in 0..crate::msi::COUNT as u8 {
                if crate::msi::count(vector) != 0 {
                    crate::println!("msi {vector}: {}", crate::msi::count(vector));
                }
            }
        }
        "reset" => {
            crate::println!("Restarting.");
            crate::arch::aarch64::power::reset()
        }
        "peek" => peek(rest),
        "poke" => poke(rest),
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

/// `cp SRC DST`: copies a file (up to 64 KiB) between volumes, `diskN/NAME` or a boot-volume name.
fn copy(rest: &str) {
    const SIZE: usize = 256 * 1024;
    let Some((from, to)) = rest.split_once(' ') else {
        crate::println!("cp: cp SRC DST (names are NAME or diskN/NAME).");
        return;
    };
    let (Some((source, source_name)), Some((target, target_name))) = (locate(from.trim()), locate(to.trim())) else {
        crate::println!("cp: no FOSFS volume there.");
        return;
    };
    let Some(staging) = memory::allocate_contiguous(SIZE as u64 / memory::FRAME, None) else {
        return crate::println!("cp: no memory.");
    };
    let buffer = unsafe { core::slice::from_raw_parts_mut(staging as *mut u8, SIZE) };
    match source.read(source_name.as_bytes(), buffer) {
        Ok(Some(length)) => match target.write(target_name.as_bytes(), &buffer[..length]) {
            Ok(()) => crate::println!("cp: {length} bytes to {to}."),
            Err(error) => crate::println!("cp: cannot write {to} ({error:?})."),
        },
        Ok(None) => crate::println!("cp: {from}: no such file."),
        Err(error) => crate::println!("cp: {from}: {error:?} (files over 256 KiB are not copied)."),
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

/// `run NAME [ARGS]`: loads a tape from the boot volume and runs it until it returns or Escape stops it.
fn run_tape(rest: &str) {
    let (name, arguments) = rest.split_once(' ').map_or((rest, ""), |(name, arguments)| (name, arguments.trim()));
    if name.is_empty() {
        crate::println!("run: which tape? (run NAME [ARGS])");
        return;
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
