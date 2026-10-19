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
        "help" => crate::println!("help version uptime mem disks ls [diskN] cat NAME echo TEXT"),
        "version" => crate::println!("Fosfor OS (aarch64 port): shell, FOSFS, no tapes yet."),
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
        "echo" => crate::println!("{rest}"),
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

fn list(argument: &str) {
    let Some(volume) = volume(argument) else {
        crate::println!("ls: no FOSFS volume there (use `ls` or `ls diskN`).");
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
    let Some(volume) = volume("") else {
        crate::println!("cat: no boot volume.");
        return;
    };
    let buffer = unsafe { &mut *core::ptr::addr_of_mut!(BUFFER) };
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
