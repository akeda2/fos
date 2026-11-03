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
            Ok(()) => println!("{path} is now {label}{}.", if volumes::volume_labelled(label[1..].as_bytes()).is_some() { "" } else { " (not present yet: the boot volume's folder is used until it appears)" }),
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
