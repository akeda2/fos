//! System commands of the mf shell that need no tape-file access: `dmesg`, `sector`, `mount`, `umount`. Same wording and
//! behaviour as the embedded kernel's `shellcmds.rs`; the mount table and volume lookup are shared in `kernel-core`.

use kernel_core::{mounts, volumes};

pub fn dmesg() {
    crate::serial::print(format_args!("{}", crate::log::contents()));
}

fn number(word: &str) -> Option<u64> {
    match word.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => word.parse().ok(),
    }
}

/// `sector diskN LBA [COUNT]`: a raw read-only dump of a disk's sectors.
pub fn sector(rest: &str) {
    const USAGE: &str = "Usage: sector diskN LBA [COUNT <= 4]   (decimal or 0x hex; read-only)";
    let mut words = rest.split_whitespace();
    let disk = words.next().and_then(|word| word.strip_prefix("disk")).and_then(|digits| digits.parse::<usize>().ok());
    let lba = words.next().and_then(number);
    let count = words.next().and_then(number).unwrap_or(1);
    let (Some(disk), Some(lba)) = (disk, lba) else { return println!("{USAGE}") };
    if disk == 0 || count == 0 || count > 4 {
        return println!("{USAGE}");
    }
    for offset in 0..count {
        let mut sector = [0u8; 512];
        if let Err(error) = crate::block::read(disk - 1, lba + offset, &mut sector) {
            return println!("disk{disk} sector {}: read failed ({error:?}).", lba + offset);
        }
        println!("disk{disk} sector {}:", lba + offset);
        let mut skipped = false;
        for (row, bytes) in sector.chunks(16).enumerate() {
            // runs of identical rows are folded into one "*" line, as hexdump does
            if row > 0 && row < 31 && bytes == &sector[(row - 1) * 16..row * 16] && bytes == &sector[(row + 1) * 16..(row + 2) * 16] {
                if !skipped {
                    println!("*");
                    skipped = true;
                }
                continue;
            }
            skipped = false;
            print!("{:03x}  ", row * 16);
            for byte in bytes {
                print!("{byte:02x} ");
            }
            print!(" ");
            for &byte in bytes {
                print!("{}", if (0x20..0x7f).contains(&byte) { byte as char } else { '.' });
            }
            println!();
        }
    }
}

pub fn mount(rest: &str) {
    let mut words = rest.split_whitespace();
    match (words.next(), words.next(), words.next()) {
        (None, ..) => {
            let mut any = false;
            mounts::each(|path, label| {
                any = true;
                let (path, label) = (core::str::from_utf8(path).unwrap_or("?"), core::str::from_utf8(label).unwrap_or("?"));
                match volumes::volume_labelled(label.as_bytes()) {
                    Some((number, _)) => println!("{path} -> @{label} (disk{number})"),
                    None if kernel_core::ext4vol::disk_labelled(label.as_bytes()).is_some() => println!("{path} -> @{label} (ext volume)"),
                    None => println!("{path} -> @{label} (not present: {path}/ is the boot volume's own folder)"),
                }
            });
            if !any {
                println!("No mounts (a `fstab` file on the boot volume lists them: PATH @LABEL per line).");
            }
        }
        (Some("fstab"), None, _) => {
            let (set, bad) = mounts::load_from(volumes::boot_volume());
            println!("fstab: {set} mount(s){}.", if bad > 0 { " and lines it could not use" } else { "" });
        }
        (Some(path), Some(label), None) if label.starts_with('@') => match mounts::add(path.as_bytes(), label[1..].as_bytes()) {
            Ok(()) => println!("{path} is now {label}{}.", if volumes::volume_labelled(label[1..].as_bytes()).is_some() || kernel_core::ext4vol::disk_labelled(label[1..].as_bytes()).is_some() { "" } else { " (not present yet: the boot volume's folder is used until it appears)" }),
            Err(mounts::Error::BadPath) => println!("mount: {path} is not a folder path (names of letters, digits, . _ - +, at most 48 characters)."),
            Err(mounts::Error::BadLabel) => println!("mount: {label} is not a volume label."),
            Err(mounts::Error::Full) => println!("mount: the table is full."),
        },
        _ => println!("Usage: mount [PATH @LABEL | fstab]"),
    }
}

pub fn umount(rest: &str) {
    match rest.split_whitespace().next() {
        Some(path) if mounts::remove(path.as_bytes()) => println!("{path} is no longer a mount point."),
        Some(path) => println!("umount: {path} is not mounted."),
        None => println!("Usage: umount PATH"),
    }
}

/// `label` lists every volume's label; `label diskN|@LABEL` shows one; `label diskN|@LABEL
/// NAME` sets it and `label diskN|@LABEL -` clears it. A label may be on one volume only.
pub fn label(rest: &str) {
    let mut words = rest.split_whitespace();
    let Some(target) = words.next() else {
        for number in 1..=crate::block::count() {
            match crate::storage::volume_on(number) {
                Ok(volume) => match volume.label() {
                    Ok(label) if !label.is_empty() => println!("disk{number}: @{}", core::str::from_utf8(label.as_bytes()).unwrap_or("?")),
                    Ok(_) => println!("disk{number}: (no label)"),
                    Err(error) => println!("disk{number}: {error:?}"),
                },
                Err(crate::storage::VolumeError::NoPartition) | Err(crate::storage::VolumeError::NotFormatted) => match kernel_core::tape_fs::fat_label_of(number) {
                    Some(Some((bytes, length))) if length > 0 => println!("disk{number}: @{} (exFAT)", core::str::from_utf8(&bytes[..length]).unwrap_or("?")),
                    Some(_) => println!("disk{number}: (no label, exFAT)"),
                    None => println!("disk{number}: (no FOSFS volume)"),
                },
                Err(error) => println!("disk{number}: {error}"),
            }
        }
        return;
    };
    let (number, volume) = if let Some(label) = target.strip_prefix('@') {
        match crate::storage::volume_labelled(label.as_bytes()) {
            Some(found) => found,
            None => match kernel_core::tape_fs::fat_labelled(label.as_bytes()) {
                Some((number, _)) => return exfat_label(number, words.next()),
                None => return println!("@{label}: no volume carries that label."),
            },
        }
    } else if let Some(number) = target.strip_prefix("disk").and_then(|number| number.parse::<usize>().ok()) {
        match crate::storage::volume_on(number) {
            Ok(volume) => (number, volume),
            Err(error) => {
                if kernel_core::tape_fs::fat_label_of(number).is_some() {
                    return exfat_label(number, words.next());
                }
                return println!("disk{number}: {error}.");
            }
        }
    } else {
        return println!("Usage: label [diskN|@LABEL [NAME|-]]");
    };
    let Some(name) = words.next() else {
        match volume.label() {
            Ok(label) if !label.is_empty() => println!("disk{number}: @{}", core::str::from_utf8(label.as_bytes()).unwrap_or("?")),
            Ok(_) => println!("disk{number}: (no label)"),
            Err(error) => println!("disk{number}: {error:?}"),
        }
        return;
    };
    let wanted = if name == "-" { "" } else { name };
    if !wanted.is_empty() {
        if !kernel_core::fosfs::valid_name(wanted.as_bytes()) {
            return println!("label: up to 16 lowercase letters, digits, `.`, `_` or `-`.");
        }
        if let Some((other, _)) = crate::storage::volume_labelled(wanted.as_bytes()).filter(|(other, _)| *other != number) {
            return println!("label: disk{other} already carries @{wanted}.");
        }
    }
    match volume.set_label(wanted.as_bytes()) {
        Ok(()) if wanted.is_empty() => println!("disk{number}: label cleared."),
        Ok(()) => println!("disk{number} is now @{wanted}."),
        Err(error) => println!("label: {error:?}"),
    }
}

/// `label` for an exFAT volume: up to 11 characters of any case (it is matched ignoring case), `-` clears it.
fn exfat_label(number: usize, name: Option<&str>) {
    let Some(name) = name else {
        return match kernel_core::tape_fs::fat_label_of(number) {
            Some(Some((bytes, length))) if length > 0 => println!("disk{number}: @{} (exFAT)", core::str::from_utf8(&bytes[..length]).unwrap_or("?")),
            _ => println!("disk{number}: (no label, exFAT)"),
        };
    };
    let wanted = if name == "-" { "" } else { name };
    if wanted.starts_with('@') {
        return println!("label: write the label without the leading @ (`label disk{number} {}`); @ only names it.", &wanted[1..]);
    }
    if !wanted.is_empty() {
        let other = crate::storage::volume_labelled(wanted.as_bytes()).map(|(other, _)| other).or_else(|| kernel_core::tape_fs::fat_labelled(wanted.as_bytes()).map(|(other, _)| other));
        if let Some(other) = other.filter(|&other| other != number) {
            return println!("label: disk{other} already carries @{wanted}.");
        }
    }
    match kernel_core::tape_fs::set_fat_label(number, wanted.as_bytes()) {
        Ok(()) if wanted.is_empty() => println!("disk{number}: label cleared."),
        Ok(()) => println!("disk{number} is now @{wanted}."),
        Err(fat::Error::BadName) => println!("label: an exFAT label is up to 11 printable characters (no / : * ? and the like)."),
        Err(error) => println!("label: {error:?}"),
    }
}

/// `grow diskN`: makes the disk's FOSFS volume fill the disk. A stick flashed from the 96 MiB image keeps a 32 MiB data
/// partition and leaves the rest of the stick unused; this moves the backup partition table to the real end of the disk,
/// extends the FOSFS partition to it and the volume to the partition. Files are kept (a FOSFS volume is extents addressed from
/// its start, so new space is just free space). Only a FOSFS partition that is the last on the disk can grow.
pub fn grow(rest: &str) {
    let Some(number) = rest.split_whitespace().next().and_then(|word| word.strip_prefix("disk")).and_then(|digits| digits.parse::<usize>().ok()) else {
        return println!("Usage: grow diskN   (make the disk's FOSFS volume fill the disk; files are kept)");
    };
    if number.checked_sub(1).and_then(crate::block::get).is_none() {
        return println!("disk{number}: no such disk (see `disks`).");
    }
    let disk = number - 1;
    let before = match crate::storage::volume_on(number) {
        Ok(volume) => volume.sectors,
        Err(error) => return println!("grow: disk{number}: {error}."),
    };
    use kernel_core::gpt::GrowError;
    let partition = match kernel_core::gpt::grow_last_partition(disk, &fosfor_abi::FOSFS_TYPE_GUID) {
        Ok(partition) => partition,
        Err(GrowError::NothingToGrow) => {
            // The partition may already fill the disk while the volume does not (an interrupted grow): finish that.
            match kernel_core::gpt::find(disk, &fosfor_abi::FOSFS_TYPE_GUID, |_| true) {
                Some(partition) if partition.sectors > before + 2048 => partition,
                _ => return println!("disk{number}: the FOSFS volume ({} MiB) already fills the disk.", before / 2048),
            }
        }
        Err(GrowError::NoGpt) => return println!("grow: disk{number} has no GPT this kernel understands."),
        Err(GrowError::NoPartition) => return println!("grow: disk{number} has no FOSFS partition."),
        Err(GrowError::NotLast) => return println!("grow: another partition lies after the FOSFS one; only the last partition can grow."),
        Err(GrowError::Disk) => return println!("grow: disk{number}: reading or writing the partition table failed."),
    };
    let volume = match kernel_core::fosfs::Volume::mount(disk, partition.start, partition.sectors).and_then(|volume| volume.grow(partition.sectors)) {
        Ok(volume) => volume,
        Err(error) => return println!("grow: the partition grew but the volume could not ({error:?}); run `grow disk{number}` again."),
    };
    if crate::storage::boot_disk() == Some(disk) {
        crate::storage::remount_boot();
    }
    println!("disk{number}: FOSFS volume grown from {} MiB to {} MiB; the files are unchanged.", before / 2048, volume.sectors / 2048);
}

/// `fsck [repair] [diskN|@LABEL]`: the same volume names as the file commands.
pub fn fsck(rest: &str) {
    let (repair, rest) = match rest.strip_prefix("repair") {
        Some(after) if after.is_empty() || after.starts_with(' ') => (true, after.trim()),
        _ => (false, rest),
    };
    let run = |volume: &kernel_core::fosfs::Volume| if repair { kernel_core::fosfs::repair(volume) } else { kernel_core::fosfs::report(volume) };
    if rest.is_empty() {
        match crate::storage::boot_volume() {
            Some(volume) => run(&volume),
            None => println!("fsck: no FOSFS volume is mounted (see `disks`)."),
        }
    } else if let Some(label) = rest.strip_prefix('@') {
        match crate::storage::volume_labelled(label.as_bytes()) {
            Some((_, volume)) => run(&volume),
            None => println!("fsck: no volume carries the label @{label} (see `label`)."),
        }
    } else if let Some(number) = rest.strip_prefix("disk").and_then(|digits| digits.parse().ok()) {
        match crate::storage::volume_on(number) {
            Ok(volume) => run(&volume),
            Err(error) => println!("fsck: {rest}: {error}."),
        }
    } else {
        println!("Usage: fsck [repair] [diskN|@LABEL]");
    }
}


/// `sha256 NAME...`: the digest, size and name of each file, read through the shared file calls.
pub fn sha256(host: &mut dyn fosfor_shell::Host, rest: &str) {
    if rest.split_whitespace().next().is_none() {
        return println!("Usage: sha256 [diskN/]NAME...");
    }
    for reference in rest.split_whitespace() {
        let Ok(size) = host.length(reference) else {
            println!("sha256: {reference}: not found");
            continue;
        };
        let ok = host.with_buffer((size as usize).max(1), &mut |host, buffer| {
            let Ok(length) = host.read(reference, buffer) else { return };
            let digest = tape_sync::sha256(&buffer[..length]);
            let mut hex = [0u8; 64];
            for (index, byte) in digest.iter().enumerate() {
                hex[index * 2] = b"0123456789abcdef"[usize::from(byte >> 4)];
                hex[index * 2 + 1] = b"0123456789abcdef"[usize::from(byte & 15)];
            }
            println!("{}  {length}  {reference}", core::str::from_utf8(&hex).unwrap_or(""));
        });
        if !ok {
            println!("sha256: {reference}: no memory for it");
        }
    }
}

pub fn keymap(rest: &str) {
    match rest.split_whitespace().next() {
        None => println!("keymap: {} (keymap us|sv changes it).", if crate::config::swedish() { "sv" } else { "us" }),
        Some("us") => {
            crate::config::set_swedish(false);
            println!("keymap: us.");
        }
        Some("sv" | "se" | "fi") => {
            crate::config::set_swedish(true);
            println!("keymap: sv.");
        }
        Some(name) => println!("keymap: unknown layout `{name}` (us or sv)."),
    }
}

/// COM1 as the serial update protocol's link.
struct Com1;

impl crate::kup_serial::Link for Com1 {
    fn read_byte(&mut self) -> Option<u8> {
        crate::serial::read_byte()
    }
    fn write_byte(&mut self, byte: u8) {
        crate::serial::write_byte(byte);
    }
    fn micros(&self) -> u64 {
        crate::apic::micros()
    }
}

/// Room for a kernel slot from the heap, or None.
fn staging() -> Option<alloc::vec::Vec<u8>> {
    let mut buffer = alloc::vec::Vec::new();
    buffer.try_reserve_exact(fosfor_abi::SLOT_SECTORS as usize * 512 + 512).ok()?;
    buffer.resize(buffer.capacity(), 0);
    Some(buffer)
}

/// `kup serial`: a kernel image from `tools/send_kernel_serial.py` over COM1 into the inactive slot, then a restart to try it.
/// COM1 keeps its speed, as on the embedded x86 kernel. The disk lock is held for the transfer.
pub fn kup(rest: &str) {
    let mut words = rest.split_whitespace();
    if words.next() != Some("serial") {
        return println!("Usage: kup serial   (the sender is tools/send_kernel_serial.py)");
    }
    if words.next().is_some() {
        return println!("kup serial: COM1 keeps its speed on this kernel (give no BAUD).");
    }
    let Some(mut staging) = staging() else { return println!("kup: no memory for the image.") };
    println!("KUP serial: ready (the sender starts within 30 s).");
    let result = {
        let _disk = crate::tapefs::lock();
        crate::kup_serial::serve(&mut Com1, &mut staging)
    };
    match result {
        Ok(slot) => {
            println!("KUP serial: image verified and written to slot {slot} (pending); restarting to try it.");
            crate::apic::busy_wait_ms(200);
            crate::slots::mark_clean_stop();
            crate::power::reset();
        }
        Err(message) => println!("KUP serial: {message}; nothing was written."),
    }
}

/// `recv NAME`: one file from `tools/send_file_serial.py` over COM1, written as NAME or diskN/NAME.
pub fn recv(host: &mut dyn fosfor_shell::Host, rest: &str) {
    let Some(name) = rest.split_whitespace().next() else {
        return println!("Usage: recv NAME   (then run tools/send_file_serial.py PORT FILE NAME)");
    };
    let Some(mut staging) = staging() else { return println!("recv: no memory.") };
    println!("KUP serial: ready for file {name} (the sender starts within 30 s).");
    let result = crate::kup_serial::serve_file(&mut Com1, &mut staging, |data| host.write(name, data).map_err(|_| "the file could not be written (bad name, no space, or no such volume)"));
    match result {
        Ok(length) => println!("recv: {length} bytes written to {name}."),
        Err(message) => println!("recv: {message}; nothing was written."),
    }
}

/// `clear`: the framebuffer console understands the ANSI erase; a serial terminal does too.
pub fn clear() {
    print!("\x1b[2J");
}

/// `sleep [SECONDS <= 60]`: the calling thread sleeps, so the other CPUs and threads keep running.
pub fn sleep(rest: &str) {
    let seconds = rest.split_whitespace().next().and_then(|word| word.parse::<u64>().ok()).unwrap_or(1).min(60);
    crate::sched::sleep_ticks(seconds * crate::apic::TICK_HZ);
    println!("Slept {seconds} s.");
}

/// `io r8|r16|r32 PORT` and `io w8|w16|w32 PORT VALUE`: raw port access.
pub fn io(rest: &str) {
    use crate::cpu;
    let mut words = rest.split_whitespace();
    let usage = "Usage: io r8|r16|r32 PORT | io w8|w16|w32 PORT VALUE";
    let (Some(verb), Some(port)) = (words.next(), words.next().and_then(number)) else { return println!("{usage}") };
    if port > 0xffff {
        return println!("Ports are 16 bits.");
    }
    let port = port as u16;
    match verb {
        "r8" => println!("{port:#06x} -> {:#04x}", cpu::inb(port)),
        "r16" => println!("{port:#06x} -> {:#06x}", cpu::inw(port)),
        "r32" => println!("{port:#06x} -> {:#010x}", cpu::inl(port)),
        "w8" | "w16" | "w32" => {
            let Some(value) = words.next().and_then(number) else { return println!("Usage: io {verb} PORT VALUE") };
            match verb {
                "w8" => cpu::outb(port, value as u8),
                "w16" => cpu::outw(port, value as u16),
                _ => cpu::outl(port, value as u32),
            }
            println!("{port:#06x} <- {value:#x}");
        }
        _ => println!("{usage}"),
    }
}

fn memory_type(kind: u32) -> &'static str {
    match kind {
        0 => "reserved",
        1 => "loader code",
        2 => "loader data",
        3 => "boot code",
        4 => "boot data",
        5 => "runtime code",
        6 => "runtime data",
        7 => "usable",
        8 => "unusable",
        9 => "ACPI reclaim",
        10 => "ACPI NVS",
        11 => "MMIO",
        12 => "MMIO ports",
        13 => "PAL code",
        14 => "persistent",
        _ => "other",
    }
}

/// `memmap`: the firmware's memory map as the loader passed it, copied at boot.
pub fn memmap() {
    let Some(machine) = crate::boot::machine() else { return println!("No boot information recorded.") };
    println!("{:>18} {:>10}  type", "physical start", "size");
    let mut usable = 0u64;
    for descriptor in machine.descriptors() {
        let bytes = descriptor.pages * 4096;
        if descriptor.kind == 7 {
            usable += bytes;
        }
        let size = if bytes >= 1 << 20 { (bytes >> 20, "MiB") } else { (bytes >> 10, "KiB") };
        println!("{:#018x} {:>6} {}  {}", descriptor.physical_start, size.0, size.1, memory_type(descriptor.kind));
    }
    println!("{} MiB of conventional (usable) memory in the map.", usable >> 20);
}

/// `bootinfo`: what the loader handed over (the part the kernel kept).
pub fn bootinfo() {
    let Some(machine) = crate::boot::machine() else { return println!("No boot information recorded.") };
    println!("kernel   {:#x}, {} KiB", machine.kernel_base, machine.kernel_size / 1024);
    println!("memory   map at {:#x}, {} bytes; BootInfo page {:#x}", machine.map.0, machine.map.1, machine.info_page);
    println!("acpi     RSDP {:#x}", machine.acpi_rsdp);
    println!("runtime  services table {:#x}", machine.runtime_services);
    let fb = &machine.framebuffer;
    println!("graphics {}x{} stride {} at {:#x}, pixel format {}", fb.width, fb.height, fb.stride, fb.base, fb.format);
}

/// `cpuinfo`: CPUID of the CPU running the shell.
pub fn cpuinfo() {
    use crate::cpu::cpuid;
    let (max_leaf, b, c, d) = cpuid(0, 0);
    let mut vendor = [0u8; 12];
    vendor[0..4].copy_from_slice(&b.to_le_bytes());
    vendor[4..8].copy_from_slice(&d.to_le_bytes());
    vendor[8..12].copy_from_slice(&c.to_le_bytes());
    println!("vendor     {} (highest basic CPUID leaf {max_leaf:#x})", core::str::from_utf8(&vendor).unwrap_or("?"));
    let (max_extended, ..) = cpuid(0x8000_0000, 0);
    if max_extended >= 0x8000_0004 {
        let mut brand = [0u8; 48];
        for (index, leaf) in (0x8000_0002u32..=0x8000_0004).enumerate() {
            let (a, b, c, d) = cpuid(leaf, 0);
            for (part, register) in [a, b, c, d].into_iter().enumerate() {
                brand[index * 16 + part * 4..][..4].copy_from_slice(&register.to_le_bytes());
            }
        }
        println!("brand      {}", core::str::from_utf8(&brand).unwrap_or("?").trim_matches(|c: char| c == '\0' || c == ' '));
    }
    let (signature, ebx, ecx, edx) = cpuid(1, 0);
    let (stepping, mut model, mut family) = (signature & 0xf, (signature >> 4) & 0xf, (signature >> 8) & 0xf);
    if family == 0xf {
        family += (signature >> 20) & 0xff;
    }
    if family == 0x6 || family >= 0xf {
        model |= ((signature >> 16) & 0xf) << 4;
    }
    println!("signature  family {family:#x} model {model:#x} stepping {stepping} (CPUID.1 EAX {signature:#010x})");
    println!("logical    {} logical processor(s) per package, APIC id {}", (ebx >> 16) & 0xff, ebx >> 24);
    let (_, _, ext_ecx, ext_edx) = if max_extended >= 0x8000_0001 { cpuid(0x8000_0001, 0) } else { (0, 0, 0, 0) };
    let (_, leaf7_ebx, ..) = if max_leaf >= 7 { cpuid(7, 0) } else { (0, 0, 0, 0) };
    let features = [
        ("sse2", edx & (1 << 26) != 0),
        ("sse4.2", ecx & (1 << 20) != 0),
        ("aes", ecx & (1 << 25) != 0),
        ("avx", ecx & (1 << 28) != 0),
        ("avx2", leaf7_ebx & (1 << 5) != 0),
        ("rdrand", ecx & (1 << 30) != 0),
        ("x2apic", ecx & (1 << 21) != 0),
        ("apic", edx & (1 << 9) != 0),
        ("pat", edx & (1 << 16) != 0),
        ("nx", ext_edx & (1 << 20) != 0),
        ("1g-pages", ext_edx & (1 << 26) != 0),
        ("long-mode", ext_edx & (1 << 29) != 0),
        ("lahf", ext_ecx & 1 != 0),
        ("hypervisor", ecx & (1 << 31) != 0),
    ];
    print!("features  ");
    for (name, present) in features {
        if present {
            print!(" {name}");
        }
    }
    println!();
    let invariant = max_extended >= 0x8000_0007 && cpuid(0x8000_0007, 0).3 & (1 << 8) != 0;
    println!("tsc        {}", if invariant { "invariant" } else { "not invariant" });
}

pub(crate) struct Console;

impl core::fmt::Write for Console {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        crate::serial::print(format_args!("{text}"));
        Ok(())
    }
}

/// `ext4 diskN [ls|cat|info|mkfs|mkdir|rm|mv|write] [ARGS]`: a Linux volume.
pub fn ext4(rest: &str) {
    crate::formatshell::cancel_format();
    kernel_core::ext4vol::command(rest, &mut Console);
}
