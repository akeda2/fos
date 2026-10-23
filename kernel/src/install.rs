//! `install diskN [LABEL] [bare] [force]`: makes another disk bootable by copying the running system onto it
//! (DOS's `sys`, plus the files). The new disk gets the boot disk's layout with fresh GUIDs, so that the kernel's
//! boot-disk discovery (which matches the payload partition's GUID) can tell the two apart:
//!
//! 1. a new GPT: an EFI System Partition and a payload partition the same size as the boot disk's, and a FOSFS
//!    partition filling the rest of the disk;
//! 2. the ESP is copied sector for sector (it holds only `\EFI\BOOT\BOOTX64.EFI`, so the firmware's removable-media
//!    fallback boots it; no NVRAM boot entry is written), and so is the BIOS boot chain if the boot disk has one
//!    (stage one in the MBR's code area, stage two in the GPT gap at LBA 64), so the copy boots under BIOS too;
//! 3. the payload gets the kernel that is *running* (the slot named in BootInfo) as slot 0, and fresh slot metadata
//!    (active slot 0, nothing pending, no crash history), so the copy boots exactly what this boot booted;
//! 4. the FOSFS volume is formatted and every file of the boot volume is copied (`bare` skips the files).
//!
//! Like `format` it announces what it will do and waits for `install diskN confirm` as the very next command, never
//! writes to the boot disk, and needs `force` to replace a partition table that is not a fos disk's (a blank disk or
//! a disk that already has a fos payload partition, i.e. an earlier install, does not).

use fosfor_abi::{slot_start, SlotMetadata, BIOS_STAGE2_LBA, BIOS_STAGE2_SECTORS, FOSFS_TYPE_GUID, PAYLOAD_TYPE_GUID};

use crate::gpt::{self, Partition, ESP_TYPE_GUID};
use crate::{block, fosfs, storage};

/// Smallest FOSFS partition worth installing (the boot image's own is 32 MiB).
const MIN_FOSFS_SECTORS: u64 = 8 * 2048;
/// Sectors per copy step (64 KiB; every driver accepts it).
const CHUNK_SECTORS: usize = 128;

#[derive(Clone, Copy)]
struct Pending {
    number: usize,
    label: [u8; fosfs::MAX_NAME],
    label_length: usize,
    bare: bool,
}
static mut PENDING: Option<Pending> = None;
static mut CHUNK: [u8; CHUNK_SECTORS * 512] = [0; CHUNK_SECTORS * 512];

/// Forgets an announced install: the confirmation must be the very next command.
pub fn cancel() {
    unsafe { PENDING = None };
}

/// The boot disk's three partitions, found by type.
struct Source {
    esp: Partition,
    payload: Partition,
    fosfs: Partition,
}

fn source(disk: usize) -> Result<Source, &'static str> {
    let esp = gpt::find(disk, &ESP_TYPE_GUID, |_| true).ok_or("the boot disk has no EFI System Partition")?;
    let payload = gpt::find(disk, &PAYLOAD_TYPE_GUID, |_| true).ok_or("the boot disk has no payload partition")?;
    let fosfs = gpt::find(disk, &FOSFS_TYPE_GUID, |_| true).ok_or("the boot disk has no FOSFS partition")?;
    Ok(Source { esp, payload, fosfs })
}

pub fn command(mut words: core::str::SplitWhitespace) {
    const USAGE: &str = "Usage: install diskN [LABEL] [bare] [force]   (then: install diskN confirm)";
    let Some(number) = words.next().and_then(|word| word.strip_prefix("disk")).and_then(|digits| digits.parse::<usize>().ok()) else {
        return crate::println!("{USAGE}");
    };
    let Some(info) = number.checked_sub(1).and_then(block::get) else {
        return crate::println!("disk{number}: no such disk (see `disks`).");
    };
    let disk = number - 1;
    let Some(boot) = storage::boot_disk() else {
        return crate::println!("install: the boot disk is unknown, so there is nothing to copy.");
    };
    if boot == disk {
        return crate::println!("disk{number} is the boot disk; install copies it onto another disk.");
    }
    let (mut confirm, mut force, mut bare, mut label) = (false, false, false, "");
    for word in words {
        match word {
            "confirm" => confirm = true,
            "force" => force = true,
            "bare" => bare = true,
            name => label = name,
        }
    }
    if !label.is_empty() && !fosfs::valid_name(label.as_bytes()) {
        return crate::println!("install: a label is up to 16 lowercase letters, digits, `.`, `_` or `-`.");
    }
    if confirm {
        let pending = unsafe { (*core::ptr::addr_of_mut!(PENDING)).take() };
        let Some(pending) = pending.filter(|pending| pending.number == number) else {
            return crate::println!("install: nothing is waiting for confirmation on disk{number}; run `install disk{number}` first.");
        };
        return run(boot, disk, number, &pending);
    }
    let layout = match source(boot) {
        Ok(layout) => layout,
        Err(reason) => return crate::println!("install: {reason}."),
    };
    let needed = 2048 + layout.esp.sectors.next_multiple_of(2048) + layout.payload.sectors.next_multiple_of(2048) + MIN_FOSFS_SECTORS + 34;
    if info.sectors < needed {
        return crate::println!("install: disk{number} is {} MiB; the ESP, payload and a FOSFS volume need at least {} MiB.", info.sectors / 2048, needed.div_ceil(2048));
    }
    crate::println!("disk{number}: {} MiB, {} ({}).", info.sectors / 2048, core::str::from_utf8(&info.model).unwrap_or("?").trim(), core::str::from_utf8(&info.origin[..info.origin_length]).unwrap_or("?"));
    let fos_disk = gpt::find(disk, &PAYLOAD_TYPE_GUID, |_| true).is_some();
    match gpt::inspect(disk) {
        Some(gpt::Summary::Blank) => crate::println!("It is blank."),
        _ if fos_disk => crate::println!("It is a fos disk (it has a payload partition); it will be replaced."),
        Some(gpt::Summary::Gpt { .. }) if force => crate::println!("It holds a GPT with other partitions; they become unreachable."),
        Some(gpt::Summary::Unknown) if force => crate::println!("It holds data that is not a GPT (an MBR or a file system); it is lost."),
        Some(gpt::Summary::Gpt { .. }) => return crate::println!("install: disk{number} holds a GPT that is not a fos disk's; add `force` to replace it."),
        Some(gpt::Summary::Unknown) => return crate::println!("install: disk{number}'s first sectors hold data that is not a GPT; add `force` to replace it."),
        None => return crate::println!("install: cannot read disk{number}."),
    }
    crate::println!("It will get the boot disk's ESP ({} MiB){}, the running kernel (slot {}) and a FOSFS volume of about {} MiB{}.",
        layout.esp.sectors / 2048, if cfg!(target_arch = "aarch64") { " with the Pi firmware" } else { " and BIOS boot code" }, storage::boot_slot(), (info.sectors - needed + MIN_FOSFS_SECTORS) / 2048, if bare { ", empty" } else { " with every file of the boot volume" });
    crate::println!("ALL of disk{number} is replaced.");
    let mut stored = Pending { number, label: [0; fosfs::MAX_NAME], label_length: label.len(), bare };
    stored.label[..label.len()].copy_from_slice(label.as_bytes());
    unsafe { PENDING = Some(stored) };
    crate::println!("To go ahead, type exactly: install disk{number} confirm");
}

/// Copies the BIOS boot chain (boot/bios) from the boot disk to `disk`: stage two first, then stage one's code into the MBR's
/// code area, so a disk interrupted half way has no code in the MBR and is simply not BIOS-bootable (the protective partition
/// entry and signature `gpt::write_table` wrote stay). `Ok(false)` if the boot disk has none (a UEFI-only image).
fn copy_bios_boot_code(boot: usize, disk: usize) -> Result<bool, block::Error> {
    let mut mbr = [0u8; 512];
    block::read(boot, 0, &mut mbr)?;
    let mut stage2 = [0u8; 512];
    block::read(boot, BIOS_STAGE2_LBA, &mut stage2)?;
    let has_code = mbr[510] == 0x55 && mbr[511] == 0xaa && mbr[..446].iter().any(|&byte| byte != 0) && stage2.iter().any(|&byte| byte != 0);
    if !has_code {
        return Ok(false);
    }
    copy_sectors(boot, BIOS_STAGE2_LBA, disk, BIOS_STAGE2_LBA, BIOS_STAGE2_SECTORS)?;
    let mut target = [0u8; 512];
    block::read(disk, 0, &mut target)?;
    target[..446].copy_from_slice(&mbr[..446]);
    block::write(disk, 0, &target)?;
    Ok(true)
}

/// Copies `sectors` sectors from `from_lba` on `from` to `to_lba` on `to`, 64 KiB at a time.
fn copy_sectors(from: usize, from_lba: u64, to: usize, to_lba: u64, sectors: u64) -> Result<(), block::Error> {
    let chunk = unsafe { &mut *core::ptr::addr_of_mut!(CHUNK) };
    let mut done = 0u64;
    while done < sectors {
        let count = (sectors - done).min(CHUNK_SECTORS as u64) as usize;
        block::read(from, from_lba + done, &mut chunk[..count * 512])?;
        block::write(to, to_lba + done, &chunk[..count * 512])?;
        storage::pet();
        done += count as u64;
    }
    Ok(())
}

fn run(boot: usize, disk: usize, number: usize, pending: &Pending) {
    let layout = match source(boot) {
        Ok(layout) => layout,
        Err(reason) => return crate::println!("install: {reason}."),
    };
    // The kernel that is running: its slot's size comes from the boot disk's metadata sector.
    let mut sector = [0u8; 512];
    if block::read(boot, layout.payload.start, &mut sector).is_err() {
        return crate::println!("install: reading the boot disk's slot metadata failed.");
    }
    let Some(mut metadata) = SlotMetadata::parse(&sector) else {
        return crate::println!("install: the boot disk's slot metadata is not valid.");
    };
    let slot = u64::from(storage::boot_slot() & 1);
    let kernel_sectors = metadata.slot_sectors[slot as usize];
    if kernel_sectors == 0 {
        return crate::println!("install: the running kernel's slot is empty in the metadata.");
    }

    let mut made = [Partition { unique_guid: [0; 16], start: 0, sectors: 0 }; 3];
    let specs: [(&[u8; 16], &str, Option<u64>); 3] = [
        (&ESP_TYPE_GUID, "EFI System", Some(layout.esp.sectors)),
        (&PAYLOAD_TYPE_GUID, "fos payload", Some(layout.payload.sectors)),
        (&FOSFS_TYPE_GUID, "FOSFS data", None),
    ];
    if let Err(error) = gpt::write_table(disk, &specs, &mut made) {
        return crate::println!("install: disk{number}: writing the partition table failed ({error:?}).");
    }
    let [esp, payload, data] = made;
    crate::println!("disk{number}: new partition table: ESP {} MiB, payload {} MiB, FOSFS {} MiB (new GUIDs).", esp.sectors / 2048, payload.sectors / 2048, data.sectors / 2048);

    // A Pi has no BIOS boot code; its firmware files live on the ESP, which is copied below.
    match if cfg!(target_arch = "aarch64") { Ok(false) } else { copy_bios_boot_code(boot, disk) } {
        Ok(true) => crate::println!("Copied the BIOS boot code (stage one in the MBR, stage two at LBA {BIOS_STAGE2_LBA}): the disk boots on BIOS machines too."),
        Ok(false) if cfg!(target_arch = "aarch64") => {}
        Ok(false) => crate::println!("The boot disk has no BIOS boot code (a UEFI-only image): disk{number} will boot on UEFI machines only."),
        Err(error) => crate::println!("install: copying the BIOS boot code failed ({error:?}); disk{number} will boot on UEFI machines only."),
    }
    crate::println!("Copying the EFI System Partition ({} MiB)...", esp.sectors / 2048);
    if let Err(error) = copy_sectors(boot, layout.esp.start, disk, esp.start, esp.sectors) {
        return crate::println!("install: copying the ESP failed ({error:?}); disk{number} is not bootable.");
    }
    crate::println!("Copying the running kernel (slot {slot}, {} KiB) to slot 0...", u64::from(kernel_sectors) / 2);
    // The kernel first, the metadata last: a payload without valid metadata is simply not bootable.
    if let Err(error) = copy_sectors(boot, layout.payload.start + slot_start(slot), disk, payload.start + slot_start(0), u64::from(kernel_sectors)) {
        return crate::println!("install: copying the kernel failed ({error:?}); disk{number} is not bootable.");
    }
    metadata = SlotMetadata { flags: 0, monitor_hint: (0, 0), active_slot: 0, pending_slot: None, attempted: false, confirmed: false, crash_count: 0, slot_sectors: [kernel_sectors, 0], running: 0, unclean_boots: 0 };
    metadata.write_into(&mut sector);
    if block::write(disk, payload.start, &sector).is_err() {
        return crate::println!("install: writing the slot metadata failed; disk{number} is not bootable.");
    }

    let label = &pending.label[..pending.label_length];
    let volume = match fosfs::Volume::format(disk, data.start, data.sectors, label) {
        Ok(volume) => volume,
        Err(error) => return crate::println!("install: formatting the FOSFS volume failed ({error:?})."),
    };
    if pending.bare {
        return crate::println!("disk{number} is now bootable, with an empty FOSFS volume of {} MiB.", data.sectors / 2048);
    }
    let Ok(source_volume) = fosfs::Volume::mount(boot, layout.fosfs.start, layout.fosfs.sectors) else {
        return crate::println!("install: the boot volume cannot be read; disk{number} boots but has no files.");
    };
    let mut names = [[0u8; fosfs::MAX_NAME]; 64];
    let mut lengths = [0usize; 64];
    let mut count = 0;
    let _ = source_volume.for_each(|entry| {
        if count < names.len() {
            names[count][..entry.name().len()].copy_from_slice(entry.name());
            lengths[count] = entry.name().len();
            count += 1;
        }
    });
    let (mut copied, mut bytes_total) = (0, 0usize);
    for index in 0..count {
        let name = &names[index][..lengths[index]];
        let length = match source_volume.file_length(name) {
            Ok(Some(length)) => length as usize,
            _ => continue,
        };
        let frames = (length.max(1) as u64).div_ceil(crate::memory::FRAME);
        let Some(buffer) = crate::memory::allocate_contiguous(frames, None) else {
            crate::println!("install: not enough memory to copy {} ({length} bytes); skipped.", core::str::from_utf8(name).unwrap_or("?"));
            continue;
        };
        let data = unsafe { core::slice::from_raw_parts_mut(buffer as *mut u8, length) };
        let outcome = match source_volume.read(name, data) {
            Ok(Some(read)) if read == length => volume.write(name, &data[..length]).is_ok(),
            _ => false,
        };
        crate::memory::free(buffer, frames);
        if outcome {
            copied += 1;
            bytes_total += length;
        } else {
            crate::println!("install: copying {} failed.", core::str::from_utf8(name).unwrap_or("?"));
        }
    }
    crate::println!("Copied {copied} of {count} file(s) ({} KiB).", bytes_total / 1024);
    crate::println!("disk{number} is now bootable: put it first in the firmware's boot order (or boot it from the boot menu).");
}
