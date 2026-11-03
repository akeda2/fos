//! `format diskN ...` for both kernels (docs/shell.md): says what it would erase and waits for `format diskN confirm`.
//! Only storage code: the GPT, FOSFS and exFAT writers are shared.

#[derive(Clone, Copy)]
struct PendingFormat {
    number: usize,
    label: [u8; crate::fosfs::MAX_NAME],
    label_length: usize,
    force: bool,
    /// `format diskN exfat`: a PC-readable exFAT volume instead of FOSFS.
    exfat: bool,
    /// FOSFS v4 (docs/fosfs-v4.md), the default; `format diskN v3` makes the old flat volume.
    v4: bool,
}
static mut PENDING_FORMAT: Option<PendingFormat> = None;

/// Called for every other command: a format needs its confirmation as the very next one.
pub fn cancel() {
    unsafe {
        PENDING_FORMAT = None;
        PENDING_SHRINK = None;
    }
}

/// `shrink diskN SIZE` waiting for `shrink diskN confirm`: the disk and the new size of its FOSFS partition in sectors.
static mut PENDING_SHRINK: Option<(usize, u64)> = None;

fn mib(sectors: u64) -> u64 {
    sectors.div_ceil(2048)
}

/// `shrink diskN [SIZE]`: makes the disk's FOSFS volume and partition smaller (SIZE like `512M`, `2G`, `1.5GiB`; binary
/// units, a bare number is MB), moving files that lie in the part to be cut off. The space after it is left unused, for
/// a second partition (docs/ext4.md). Says what it will do and waits for `shrink diskN confirm` as the very next command.
/// Files are kept; a power cut leaves each file where it was or where it went, and the partition is cut last.
pub fn shrink(rest: &str) {
    const USAGE: &str = "Usage: shrink diskN [SIZE]   (SIZE: 512M, 2G, 1.5GiB; a bare number is MB; then: shrink diskN confirm)";
    unsafe { PENDING_FORMAT = None };
    let mut words = rest.split_whitespace();
    let Some(number) = words.next().and_then(|word| word.strip_prefix("disk")).and_then(|digits| digits.parse::<usize>().ok()) else {
        return crate::println!("{USAGE}");
    };
    let argument = words.next();
    if number.checked_sub(1).and_then(crate::block::get).is_none() {
        return crate::println!("disk{number}: no such disk (see `disks`).");
    }
    let disk = number - 1;
    let Some(partition) = crate::gpt::find(disk, &fosfor_abi::FOSFS_TYPE_GUID, |_| true) else {
        return crate::println!("shrink: disk{number} has no FOSFS partition on a GPT.");
    };
    let volume = match crate::fosfs::Volume::mount(disk, partition.start, partition.sectors) {
        Ok(volume) => volume,
        Err(error) => return crate::println!("shrink: disk{number}: the FOSFS volume could not be read ({error:?})."),
    };
    let minimum = match volume.min_sectors() {
        Ok(minimum) => minimum,
        Err(error) => return crate::println!("shrink: disk{number}: the directory could not be read ({error:?})."),
    };
    if argument == Some("confirm") {
        let pending = unsafe { (*core::ptr::addr_of_mut!(PENDING_SHRINK)).take() };
        let Some((_, target)) = pending.filter(|&(pending, _)| pending == number) else {
            return crate::println!("shrink: nothing is waiting for confirmation on disk{number}; run `shrink disk{number} SIZE` first.");
        };
        let before = partition.sectors;
        let shrunk = match volume.shrink(target) {
            Ok(shrunk) => shrunk,
            Err(crate::fosfs::Error::DataFull) => return crate::println!("shrink: no single free run below {} MiB holds one of the files (it is too fragmented or too full); nothing was lost. Remove or copy files and try again.", mib(target)),
            Err(error) => return crate::println!("shrink: moving the files failed ({error:?}); the volume is intact, run it again."),
        };
        if let Err(error) = crate::gpt::shrink_partition(disk, &fosfor_abi::FOSFS_TYPE_GUID, target) {
            return crate::println!("shrink: the volume is now {} MiB but the partition table could not be changed ({error:?}); run `shrink disk{number} {}M confirm` again.", mib(shrunk.sectors), mib(target));
        }
        if crate::storage::boot_disk() == Some(disk) {
            crate::storage::remount_boot();
        }
        return crate::println!("disk{number}: FOSFS partition shrunk from {} MiB to {} MiB; the files are kept and {} MiB after it are free.", mib(before), mib(target), mib(before - target));
    }
    crate::println!("disk{number}: FOSFS partition {} MiB, volume {} MiB, files need at least {} MiB.", mib(partition.sectors), mib(volume.sectors), mib(minimum));
    let Some(argument) = argument else {
        return crate::println!("{USAGE}");
    };
    let Some(requested) = crate::fosfs::parse_size(argument) else {
        return crate::println!("shrink: `{argument}` is not a size (for example 512M, 2G or 1.5GiB).");
    };
    // Whole MiB, so that whatever is placed after the partition starts on an aligned sector.
    let target = requested.next_multiple_of(2048);
    if target >= partition.sectors {
        return crate::println!("shrink: {} MiB is not smaller than the partition.", mib(target));
    }
    if target < minimum.next_multiple_of(2048) {
        return crate::println!("shrink: too small: the files need {} MiB.", mib(minimum));
    }
    crate::println!("The partition and volume will become {} MiB; files beyond that are moved first. {} MiB are freed.", mib(target), mib(partition.sectors - target));
    unsafe { PENDING_SHRINK = Some((number, target)) };
    crate::println!("To go ahead, type exactly: shrink disk{number} confirm");
}

/// What `format` would do to a disk.
enum FormatPlan {
    /// Erase the volume in an existing FOSFS partition; other partitions are left alone.
    Reformat(crate::gpt::Partition),
    /// Replace the partition table with one FOSFS partition covering the disk.
    Partition { replaces: &'static str },
}

fn plan_format(disk: usize, force: bool) -> Result<FormatPlan, &'static str> {
    use crate::gpt::Summary;
    match crate::gpt::inspect(disk).ok_or("cannot read the disk")? {
        Summary::Gpt { fosfs: Some(partition), .. } => Ok(FormatPlan::Reformat(partition)),
        Summary::Blank => Ok(FormatPlan::Partition { replaces: "nothing: the disk is blank" }),
        Summary::Gpt { .. } if force => Ok(FormatPlan::Partition { replaces: "its GPT, with the partitions it lists" }),
        Summary::Unknown if force => Ok(FormatPlan::Partition { replaces: "whatever is in its first sectors (a partition table or file system)" }),
        Summary::Gpt { .. } => Err("it holds a GPT with partitions but no FOSFS one; add `force` to replace the partition table"),
        Summary::Unknown => Err("its first sectors hold data that is not a GPT (an MBR or a file system); add `force` to replace it"),
    }
}

/// `format diskN [NAME] [force]` says what it would erase and waits; `format diskN confirm`
/// (the very next command) does it. Never the boot disk. A disk with a FOSFS partition has
/// only that partition's volume cleared; a blank disk gets a fresh GPT with one FOSFS
/// partition; any other disk needs `force` because its partition table is replaced.
pub fn command(rest: &str) {
    unsafe { PENDING_SHRINK = None };
    let mut words = rest.split_whitespace();
    let Some(target) = words.next() else {
        return crate::println!("Usage: format diskN [NAME] [v3] [force]   or   format diskN exfat [LABEL]   (then: format diskN confirm)");
    };
    let Some(number) = target.strip_prefix("disk").and_then(|number| number.parse::<usize>().ok()) else {
        return crate::println!("Usage: format diskN [NAME] [v3] [force]   (then: format diskN confirm)");
    };
    let Some(info) = number.checked_sub(1).and_then(crate::block::get) else {
        return crate::println!("disk{number}: no such disk (see `disks`).");
    };
    let disk = number - 1;
    if crate::storage::boot_disk() == Some(disk) {
        return crate::println!("disk{number} is the boot disk (the running kernel's); format refuses it. Use `rm` for its files.");
    }
    let (mut confirm, mut force, mut exfat, mut v4, mut label) = (false, false, false, true, "");
    for word in words {
        match word {
            "confirm" => confirm = true,
            "force" => force = true,
            "exfat" => exfat = true,
            "v4" => v4 = true,
            "v3" => v4 = false,
            name => label = name,
        }
    }
    if !exfat && !label.is_empty() && !crate::fosfs::valid_label(label.as_bytes()) {
        return crate::println!("format: a label is up to 16 lowercase letters, digits, `.`, `_` or `-`.");
    }
    if confirm {
        let pending = unsafe { (*core::ptr::addr_of_mut!(PENDING_FORMAT)).take() };
        let Some(pending) = pending.filter(|pending| pending.number == number) else {
            return crate::println!("format: nothing is waiting for confirmation on disk{number}; run `format disk{number}` first.");
        };
        if pending.exfat {
            return match crate::fatvol::format_exfat(disk, &pending.label[..pending.label_length]) {
                Ok(megabytes) => crate::println!("disk{number}: formatted as exFAT ({megabytes} MiB, GPT). A PC can read it; `ls disk{number}`, or record to it."),
                Err(error) => crate::println!("format: disk{number}: {error}."),
            };
        }
        return do_format(disk, number, &pending.label[..pending.label_length], pending.force, pending.v4);
    }
    if exfat {
        // What the disk holds now, so that the user knows what the confirmation will destroy.
        crate::println!("disk{number}: {} MiB, {} ({}).", info.sectors / 2048, core::str::from_utf8(&info.model).unwrap_or("?").trim(), core::str::from_utf8(&info.origin[..info.origin_length]).unwrap_or("?"));
        if label.len() > 11 {
            return crate::println!("format: an exFAT label is up to 11 characters.");
        }
        if let Ok(volume) = crate::storage::volume_on(number) {
            let mut files = 0;
            let _ = volume.for_each(|_| files += 1);
            crate::println!("It holds a FOSFS volume with {files} file(s).");
        } else if let Some(mut fs) = crate::fatvol::mount(disk) {
            let mut files = 0;
            let _ = fs.list(|_| files += 1);
            crate::println!("It holds a {} volume with {files} file(s).", if fs.kind() == fat::Kind::ExFat { "exFAT" } else { "FAT32" });
        } else {
            match crate::gpt::inspect(disk) {
                Some(crate::gpt::Summary::Blank) => crate::println!("It is blank."),
                Some(crate::gpt::Summary::Gpt { .. }) => crate::println!("It holds a GPT partition table (not a volume this kernel knows)."),
                _ => crate::println!("It holds data this kernel does not recognise."),
            }
        }
        crate::println!("ALL of it will be erased: one exFAT partition of about {} MiB replaces it.", info.sectors / 2048 - 1);
        let mut stored = PendingFormat { number, label: [0; crate::fosfs::MAX_NAME], label_length: label.len(), force: false, exfat: true, v4: false };
        stored.label[..label.len()].copy_from_slice(label.as_bytes());
        unsafe { PENDING_FORMAT = Some(stored) };
        return crate::println!("To go ahead, type exactly: format disk{number} confirm");
    }
    let plan = match plan_format(disk, force) {
        Ok(plan) => plan,
        Err(reason) => return crate::println!("format: disk{number}: {reason}."),
    };
    crate::println!(
        "disk{number}: {} MiB, {} ({}).",
        info.sectors / 2048,
        core::str::from_utf8(&info.model).unwrap_or("?").trim(),
        core::str::from_utf8(&info.origin[..info.origin_length]).unwrap_or("?")
    );
    match plan {
        FormatPlan::Reformat(partition) => {
            let files = crate::fosfs::Volume::mount(disk, partition.start, partition.sectors).ok().and_then(|volume| {
                let mut files = 0;
                volume.for_each(|_| files += 1).ok()?;
                Some(files)
            });
            match files {
                Some(files) => crate::println!("Its FOSFS partition ({} MiB, {files} file(s)) will be erased; any other partition is left alone.", partition.sectors / 2048),
                None => crate::println!("Its FOSFS partition ({} MiB, no valid volume yet) will be given a fresh empty volume.", partition.sectors / 2048),
            }
        }
        FormatPlan::Partition { replaces } => {
            crate::println!("It will be repartitioned as one FOSFS partition of {} MiB, replacing {replaces}.", info.sectors / 2048 - 1);
        }
    }
    let mut stored = PendingFormat { number, label: [0; crate::fosfs::MAX_NAME], label_length: label.len(), force, exfat: false, v4 };
    stored.label[..label.len()].copy_from_slice(label.as_bytes());
    unsafe { PENDING_FORMAT = Some(stored) };
    crate::println!("To go ahead, type exactly: format disk{number} confirm");
}

fn do_format(disk: usize, number: usize, label: &[u8], force: bool, v4: bool) {
    let partition = match plan_format(disk, force) {
        Ok(FormatPlan::Reformat(partition)) => partition,
        Ok(FormatPlan::Partition { .. }) => match crate::gpt::write_single_fosfs_partition(disk) {
            Ok(partition) => {
                crate::println!("disk{number}: wrote a new partition table with one FOSFS partition ({} MiB).", partition.sectors / 2048);
                partition
            }
            Err(error) => return crate::println!("format: disk{number}: writing the partition table failed ({error:?})."),
        },
        Err(reason) => return crate::println!("format: disk{number}: {reason}."),
    };
    match crate::fosfs::Volume::format_version(disk, partition.start, partition.sectors, label, if v4 { 4 } else { 3 }) {
        Ok(_) if label.is_empty() => crate::println!("disk{number}: formatted; an empty FOSFS v{} volume of {} MiB.", if v4 { 4 } else { 3 }, partition.sectors / 2048),
        Ok(_) => crate::println!("disk{number}: formatted as @{}; an empty FOSFS v{} volume of {} MiB.", core::str::from_utf8(label).unwrap_or("?"), if v4 { 4 } else { 3 }, partition.sectors / 2048),
        Err(error) => crate::println!("format: disk{number}: {error:?}"),
    }
}
