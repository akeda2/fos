//! `bootcode [diskN]` and `bootcode update [diskN]`: look at, and replace, the boot code of a fos disk without rewriting the
//! disk: the BIOS boot code, and the UEFI loader on the EFI System Partition.
//!
//! The BIOS boot chain (boot/bios) is not in a kernel slot, so KUP (the network kernel update) cannot change it: stage one
//! is the first 446 bytes of sector 0 (the MBR's code area; the partition table and the 0x55aa signature stay), stage two
//! is `BIOS_STAGE2_SECTORS` sectors at LBA `BIOS_STAGE2_LBA`, in the gap a GPT leaves before its first partition. Updating
//! them used to mean writing the whole stick again with `tools/write_stick.py` and losing its files. Now:
//!
//! 1. put the new `stage1.bin` and `stage2.bin` (build/bios/) and/or `loader.efi` (the UEFI loader,
//!    build/cargo-target/x86_64-unknown-uefi/release/fosfor-loader.efi) on the boot volume, e.g. `foscopy.py FILE :disk1/stage2.bin`;
//! 2. `bootcode update` announces what it would write; `bootcode update confirm` as the very next command does it.
//!
//! The UEFI loader is `\EFI\BOOT\BOOTX64.EFI` on the disk's EFI System Partition (FAT32), the one file UEFI firmware boots from,
//! so it is replaced as carefully as a boot sector: the new file is checked (a PE image for x86-64, an EFI application, carrying the
//! loader's banner), written beside the old one as `BOOTX64.NEW` and read back, and only then does one directory-sector write
//! point `BOOTX64.EFI` at it (`fat::Fs::replace`): a power cut leaves the old loader or the new one, never half of each.
//!
//! Safety, in the style of `format` and `install`: the target must be a fos disk (it has a payload partition); both files are
//! checked before anything is written (stage one exactly one sector with the signature and an empty partition-table area;
//! stage two non-empty, at most 24 sectors, starting with `cli` and carrying stage two's banner); stage two is written first
//! and read back, stage one's code last and read back, so an update interrupted half way leaves the old, working MBR code in
//! front of a complete stage two, or a stage two that is merely newer than its MBR -- never an MBR pointing at a half-written
//! stage two. It does not touch the UEFI loader on the ESP, and it never writes to the boot disk's partitions.

use fosfor_abi::{BIOS_STAGE2_LBA, BIOS_STAGE2_SECTORS, PAYLOAD_TYPE_GUID};

use crate::{block, fatvol, gpt, memory, storage};

const SECTOR: usize = 512;
const STAGE2_BYTES: usize = BIOS_STAGE2_SECTORS as usize * SECTOR;
/// Where stage one's code ends: bytes 446-509 are the partition table, 510-511 the signature.
const CODE_BYTES: usize = 446;

/// The GPT type GUID of an EFI System Partition, in on-disk byte order.
const ESP_GUID: [u8; 16] = fatvol::ESP_GUID;
const LOADER_PATH: &str = "\\EFI\\BOOT\\BOOTX64.EFI";
/// The largest loader accepted (the real one is a few hundred KiB).
const LOADER_MAX_BYTES: usize = 2 << 20;
const LOADER_BANNER: &[u8] = b"Fosfor OS loader";

/// What an announced update will write: the disk, whether the BIOS boot code is in the static buffers, and the staged loader
/// (physical address of its pages, its length in bytes), kept between the announcement and the confirmation.
#[derive(Clone, Copy)]
struct Pending {
    disk: usize,
    bios: bool,
    loader: Option<(u64, usize)>,
}

static mut PENDING: Option<Pending> = None;
static mut STAGE1_NEW: [u8; SECTOR] = [0; SECTOR];
static mut STAGE2_NEW: [u8; STAGE2_BYTES] = [0; STAGE2_BYTES];
static mut STAGE2_NOW: [u8; STAGE2_BYTES] = [0; STAGE2_BYTES];

/// Forgets an announced update: the confirmation must be the very next command.
pub fn cancel() {
    if let Some(pending) = unsafe { (*core::ptr::addr_of_mut!(PENDING)).take() } {
        if let Some((address, bytes)) = pending.loader {
            memory::free(address, pages(bytes));
        }
    }
}

fn pages(bytes: usize) -> u64 {
    bytes.div_ceil(4096) as u64
}

/// FNV-1a over `bytes`, continuing from `hash`: a short fingerprint to compare two copies by (not a security measure).
fn fingerprint_step(hash: u32, bytes: &[u8]) -> u32 {
    bytes.iter().fold(hash, |hash, &byte| (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193))
}

fn fingerprint(bytes: &[u8]) -> u32 {
    fingerprint_step(0x811c_9dc5, bytes)
}

/// Reads the BIOS files from the boot volume into the static buffers and checks them. `Ok(None)` when neither is there (an
/// update of the UEFI loader alone); an error when one is missing or either is not what it should be. Returns stage two's length.
fn load_bios_files() -> Result<Option<usize>, &'static str> {
    let volume = storage::boot_volume().ok_or("the boot volume is not mounted, so there are no files to take")?;
    let stage1 = unsafe { &mut *core::ptr::addr_of_mut!(STAGE1_NEW) };
    let stage2 = unsafe { &mut *core::ptr::addr_of_mut!(STAGE2_NEW) };
    stage1.fill(0);
    stage2.fill(0);
    let mut scratch = [0u8; SECTOR + 1];
    match volume.read(b"stage1.bin", &mut scratch) {
        Ok(Some(length)) if length == SECTOR => stage1.copy_from_slice(&scratch[..SECTOR]),
        Ok(Some(_)) => return Err("stage1.bin must be exactly 512 bytes (one sector)"),
        Ok(None) => {
            // No stage one: fine if there is no stage two either (the loader alone is being updated).
            return match volume.read(b"stage2.bin", &mut stage2[..]) {
                Ok(None) => Ok(None),
                _ => Err("there is stage2.bin but no stage1.bin on the boot volume (build/bios/stage1.bin)"),
            };
        }
        Err(_) => return Err("stage1.bin is larger than one sector"),
    }
    if stage1[510] != 0x55 || stage1[511] != 0xaa {
        return Err("stage1.bin lacks the 0x55 0xaa signature");
    }
    if stage1[CODE_BYTES..510].iter().any(|&byte| byte != 0) {
        return Err("stage1.bin has code in the partition-table area (bytes 446-509)");
    }
    if stage1[..CODE_BYTES].iter().all(|&byte| byte == 0) {
        return Err("stage1.bin has no code");
    }
    let length = match volume.read(b"stage2.bin", &mut stage2[..]) {
        Ok(Some(length)) => length,
        Ok(None) => return Err("there is stage1.bin but no stage2.bin on the boot volume (build/bios/stage2.bin)"),
        Err(_) => return Err("stage2.bin is larger than the 24 sectors stage one loads"),
    };
    if length == 0 {
        return Err("stage2.bin is empty");
    }
    if stage2[0] != 0xfa {
        return Err("stage2.bin does not start with `cli` (0xfa): it is not stage two");
    }
    if !stage2[..length].windows(14).any(|window| window == b"Fosfor stage 2") {
        return Err("stage2.bin lacks stage two's banner: it is not stage two");
    }
    Ok(Some(length))
}

/// Whether `bytes` is a UEFI loader of ours: a PE32+ image for x86-64, of the EFI application subsystem, with the loader's banner.
fn check_loader(bytes: &[u8]) -> Result<(), &'static str> {
    let le16 = |at: usize| bytes.get(at..at + 2).map(|pair| u16::from_le_bytes([pair[0], pair[1]]));
    let le32 = |at: usize| bytes.get(at..at + 4).map(|quad| u32::from_le_bytes([quad[0], quad[1], quad[2], quad[3]]));
    if le16(0) != Some(0x5a4d) {
        return Err("loader.efi does not start with the MZ signature: it is not a PE image");
    }
    let pe = le32(0x3c).ok_or("loader.efi is truncated")? as usize;
    if le32(pe) != Some(0x0000_4550) {
        return Err("loader.efi has no PE signature");
    }
    if le16(pe + 4) != Some(0x8664) {
        return Err("loader.efi is not an x86-64 image");
    }
    let optional = pe + 24;
    if le16(optional) != Some(0x020b) {
        return Err("loader.efi is not a PE32+ (64-bit) image");
    }
    if le16(optional + 68) != Some(10) {
        return Err("loader.efi is not an EFI application (PE subsystem 10)");
    }
    if !bytes.windows(LOADER_BANNER.len()).any(|window| window == LOADER_BANNER) {
        return Err("loader.efi lacks the loader's banner: it is not the Fosfor OS loader");
    }
    Ok(())
}

/// Reads `loader.efi` from the boot volume into freshly allocated pages and checks it. `Ok(None)` if there is no such file.
fn stage_loader() -> Result<Option<(u64, usize)>, &'static str> {
    let volume = storage::boot_volume().ok_or("the boot volume is not mounted, so there are no files to take")?;
    let address = memory::allocate_contiguous(pages(LOADER_MAX_BYTES), None).ok_or("no memory to stage loader.efi in")?;
    let buffer = unsafe { core::slice::from_raw_parts_mut(address as *mut u8, LOADER_MAX_BYTES) };
    let result = match volume.read(b"loader.efi", buffer) {
        Ok(Some(length)) if length > 0 => check_loader(&buffer[..length]).map(|()| Some((address, length))),
        Ok(Some(_)) => Err("loader.efi is empty"),
        Ok(None) => Ok(None),
        Err(_) => Err("loader.efi is larger than 2 MiB"),
    };
    if !matches!(result, Ok(Some(_))) {
        memory::free(address, pages(LOADER_MAX_BYTES));
    }
    result
}

/// The disk's EFI System Partition, mounted with the working directory at `\EFI\BOOT`.
fn open_loader_directory(disk: usize) -> Result<fatvol::Fs, &'static str> {
    let mut found = None;
    gpt::find(disk, &ESP_GUID, |partition| {
        found = Some(fatvol::Partition { disk, base: partition.start, sectors: partition.sectors });
        true
    });
    let partition = found.ok_or("the disk has no EFI System Partition")?;
    let mut fs = fat::Fs::mount(partition).map_err(|_| "the EFI System Partition is not a FAT32 volume this kernel can read")?;
    fatvol::stamp(&mut fs);
    fs.enter(b"EFI").map_err(|_| "the EFI System Partition has no \\EFI directory")?;
    fs.enter(b"BOOT").map_err(|_| "the EFI System Partition has no \\EFI\\BOOT directory")?;
    Ok(fs)
}

/// The size and fingerprint of the loader file in `fs`'s working directory, or None if there is none.
fn installed_loader(fs: &mut fatvol::Fs, name: &[u8]) -> Result<Option<(u64, u32)>, &'static str> {
    let Some(file) = fs.find(name).map_err(|_| "the loader file cannot be read")? else { return Ok(None) };
    let mut hash = 0x811c_9dc5u32;
    let mut buffer = [0u8; 4096];
    let mut offset = 0u64;
    while offset < file.size {
        let count = fs.read(&file, offset, &mut buffer).map_err(|_| "the loader file cannot be read")?;
        if count == 0 {
            break;
        }
        hash = fingerprint_step(hash, &buffer[..count]);
        offset += count as u64;
        crate::debug::pet();
    }
    Ok(Some((file.size, hash)))
}

/// What is installed on `disk`: whether the MBR carries boot code and the fingerprint of stage two's area.
fn read_installed(disk: usize) -> Result<(bool, u32), block::Error> {
    let mut mbr = [0u8; SECTOR];
    block::read(disk, 0, &mut mbr)?;
    let installed = unsafe { &mut *core::ptr::addr_of_mut!(STAGE2_NOW) };
    block::read(disk, BIOS_STAGE2_LBA, installed)?;
    let has_code = mbr[510] == 0x55 && mbr[511] == 0xaa && mbr[..CODE_BYTES].iter().any(|&byte| byte != 0);
    Ok((has_code, fingerprint(installed)))
}

pub fn command(mut words: core::str::SplitWhitespace) {
    const USAGE: &str = "Usage: bootcode [diskN]   |   bootcode update [diskN]  (then: bootcode update [diskN] confirm)";
    let (mut update, mut confirm, mut number) = (false, false, None);
    for word in words.by_ref() {
        match word {
            "update" => update = true,
            "confirm" => confirm = true,
            other => match other.strip_prefix("disk").and_then(|digits| digits.parse::<usize>().ok()) {
                Some(value) if value > 0 => number = Some(value),
                _ => return crate::println!("{USAGE}"),
            },
        }
    }
    let Some(boot) = storage::boot_disk() else { return crate::println!("bootcode: the boot disk is unknown.") };
    let disk = number.map_or(boot, |value| value - 1);
    let Some(info) = block::get(disk) else { return crate::println!("bootcode: no such disk (see `disks`).") };
    if gpt::find(disk, &PAYLOAD_TYPE_GUID, |_| true).is_none() {
        return crate::println!("bootcode: disk{} is not a fos disk (no payload partition); it is left alone.", disk + 1);
    }
    if confirm && update {
        let pending = unsafe { (*core::ptr::addr_of_mut!(PENDING)).take() };
        return match pending {
            Some(pending) if pending.disk == disk => write(pending),
            other => {
                if let Some(Pending { loader: Some((address, bytes)), .. }) = other {
                    memory::free(address, pages(bytes)); // announced for another disk: forgotten
                }
                crate::println!("bootcode: nothing is waiting for confirmation on disk{}; run `bootcode update` first.", disk + 1)
            }
        };
    }
    cancel(); // any command but the confirmation forgets an announcement
    let (has_code, installed) = match read_installed(disk) {
        Ok(found) => found,
        Err(error) => return crate::println!("bootcode: cannot read disk{} ({error:?}).", disk + 1),
    };
    if !update {
        crate::println!("disk{}: {} MiB. BIOS boot code: stage one {}, stage two at LBA {} (fingerprint {:08x}).", disk + 1, info.sectors / 2048,
            if has_code { "present in the MBR" } else { "absent (UEFI-only disk)" }, BIOS_STAGE2_LBA, installed);
        let loader_now = open_loader_directory(disk).and_then(|mut fs| installed_loader(&mut fs, b"bootx64.efi"));
        match &loader_now {
            Ok(Some((size, hash))) => crate::println!("UEFI loader: {LOADER_PATH}, {size} bytes (fingerprint {hash:08x})."),
            Ok(None) => crate::println!("UEFI loader: the EFI System Partition has no {LOADER_PATH}."),
            Err(reason) => crate::println!("UEFI loader: {reason}."),
        }
        match load_bios_files() {
            Ok(Some(length)) => {
                let new = unsafe { &*core::ptr::addr_of!(STAGE2_NEW) };
                let same = fingerprint(new) == installed;
                crate::println!("The boot volume has stage1.bin and stage2.bin ({length} bytes); stage two there is {}.", if same { "identical to the installed one" } else { "different from the installed one" });
            }
            Ok(None) => crate::println!("The boot volume has no stage1.bin/stage2.bin (BIOS boot code) to install."),
            Err(reason) => crate::println!("Files for a BIOS boot code update: {reason}."),
        }
        match stage_loader() {
            Ok(Some((address, length))) => {
                let same = matches!(loader_now, Ok(Some((_, hash))) if hash == fingerprint(unsafe { core::slice::from_raw_parts(address as *const u8, length) }));
                crate::println!("The boot volume has loader.efi ({length} bytes); it is {}.", if same { "identical to the installed loader" } else { "different from the installed loader" });
                memory::free(address, pages(LOADER_MAX_BYTES));
            }
            Ok(None) => crate::println!("The boot volume has no loader.efi (the UEFI loader) to install."),
            Err(reason) => crate::println!("Files for a UEFI loader update: {reason}."),
        }
        return;
    }
    let bios = match load_bios_files() {
        Ok(found) => found,
        Err(reason) => return crate::println!("bootcode: {reason}."),
    };
    let loader = match stage_loader() {
        Ok(found) => found,
        Err(reason) => return crate::println!("bootcode: {reason}."),
    };
    if bios.is_none() && loader.is_none() {
        return crate::println!("bootcode: put stage1.bin and stage2.bin (BIOS boot code) and/or loader.efi (the UEFI loader) on the boot volume first.");
    }
    if loader.is_some() {
        // The loader is written to the disk's ESP: say early if there is none.
        if let Err(reason) = open_loader_directory(disk) {
            if let Some((address, _)) = loader {
                memory::free(address, pages(LOADER_MAX_BYTES));
            }
            return crate::println!("bootcode: {reason}; nothing is announced.");
        }
    }
    if let Some(length) = bios {
        crate::println!("disk{}: stage two ({length} bytes) goes to LBA {}-{} and stage one's code into the MBR's first 446 bytes;",
            disk + 1, BIOS_STAGE2_LBA, BIOS_STAGE2_LBA + BIOS_STAGE2_SECTORS - 1);
        crate::println!("the partition table and everything else on the disk stay. A mistake here can leave the disk unbootable by BIOS.");
    }
    if let Some((_, length)) = loader {
        crate::println!("disk{}: the UEFI loader ({length} bytes) replaces {LOADER_PATH} on the EFI System Partition, written beside the old one first", disk + 1);
        crate::println!("and switched in with one directory write. A mistake here can leave the disk unbootable by UEFI.");
    }
    unsafe { PENDING = Some(Pending { disk, bios: bios.is_some(), loader }) };
    if number.is_some() {
        crate::println!("To go ahead, type exactly: bootcode update disk{} confirm", disk + 1);
    } else {
        crate::println!("To go ahead, type exactly: bootcode update confirm");
    }
}

fn write(pending: Pending) {
    if pending.bios {
        write_bios(pending.disk);
    }
    if let Some((address, length)) = pending.loader {
        write_loader(pending.disk, address, length);
        memory::free(address, pages(LOADER_MAX_BYTES));
    }
}

fn write_bios(disk: usize) {
    let stage1 = unsafe { &*core::ptr::addr_of!(STAGE1_NEW) };
    let stage2 = unsafe { &*core::ptr::addr_of!(STAGE2_NEW) };
    // Stage two first: until stage one's code is replaced, the old MBR still loads (a stage two of the same size).
    crate::println!("Writing stage two ({} sectors at LBA {})...", BIOS_STAGE2_SECTORS, BIOS_STAGE2_LBA);
    if let Err(error) = block::write(disk, BIOS_STAGE2_LBA, stage2) {
        return crate::println!("bootcode: writing stage two failed ({error:?}); the MBR is untouched.");
    }
    crate::debug::pet();
    let check = unsafe { &mut *core::ptr::addr_of_mut!(STAGE2_NOW) };
    if block::read(disk, BIOS_STAGE2_LBA, check).is_err() || check[..] != stage2[..] {
        return crate::println!("bootcode: stage two read back different; the MBR is untouched. Write the stick again if the disk no longer boots.");
    }
    crate::println!("Writing stage one's code into the MBR...");
    let mut mbr = [0u8; SECTOR];
    if block::read(disk, 0, &mut mbr).is_err() {
        return crate::println!("bootcode: cannot read the MBR; stage two is new, the MBR is old.");
    }
    mbr[..CODE_BYTES].copy_from_slice(&stage1[..CODE_BYTES]);
    mbr[510] = 0x55;
    mbr[511] = 0xaa;
    if block::write(disk, 0, &mbr).is_err() {
        return crate::println!("bootcode: writing the MBR failed.");
    }
    let mut back = [0u8; SECTOR];
    if block::read(disk, 0, &mut back).is_err() || back != mbr {
        return crate::println!("bootcode: the MBR read back different. Write the stick again with tools/write_stick.py if the disk no longer boots.");
    }
    crate::println!("Done: both read back identical. The new boot code runs at the next BIOS boot (`reboot`).");
}

/// Writes the staged loader to `\EFI\BOOT\BOOTX64.EFI`: beside the old file as BOOTX64.NEW, read back, then switched in.
fn write_loader(disk: usize, address: u64, length: usize) {
    let bytes = unsafe { core::slice::from_raw_parts(address as *const u8, length) };
    let mut fs = match open_loader_directory(disk) {
        Ok(fs) => fs,
        Err(reason) => return crate::println!("bootcode: {reason}; the UEFI loader is untouched."),
    };
    // An earlier update may have been cut after its switch (two names, one chain) or before it (a stale, complete copy).
    let _ = fs.forget_alias(b"bootx64.new", b"bootx64.efi");
    let _ = fs.remove(b"bootx64.new");
    let has_old = matches!(fs.find(b"bootx64.efi"), Ok(Some(_)));
    let name: &[u8] = if has_old { b"bootx64.new" } else { b"bootx64.efi" };
    crate::println!("Writing the UEFI loader ({length} bytes) as {}...", if has_old { "BOOTX64.NEW beside the old one" } else { "BOOTX64.EFI (there was none)" });
    let stream = match fs.create(name, length as u64) {
        Ok(stream) => stream,
        Err(error) => return crate::println!("bootcode: no room for the new loader on the EFI System Partition ({error:?}); the old one is untouched."),
    };
    for (index, chunk) in bytes.chunks(64 * 1024).enumerate() {
        let offset = index * 64 * 1024;
        if let Err(error) = fs.write(&stream, offset as u64, chunk, (offset + chunk.len()) as u64) {
            return crate::println!("bootcode: writing the new loader failed ({error:?}); the old one is untouched.");
        }
        crate::debug::pet();
    }
    if let Err(error) = fs.finish(&stream, name, length as u64) {
        return crate::println!("bootcode: finishing the new loader failed ({error:?}); the old one is untouched.");
    }
    match installed_loader(&mut fs, name) {
        Ok(Some((size, hash))) if size == length as u64 && hash == fingerprint(bytes) => {}
        _ => return crate::println!("bootcode: the new loader read back different; the old one is untouched (a stray BOOTX64.NEW stays; the next update removes it)."),
    }
    if has_old {
        crate::println!("Switching BOOTX64.EFI to the new file (one directory-sector write)...");
        if let Err(error) = fs.replace(b"bootx64.efi", b"bootx64.new") {
            return crate::println!("bootcode: the switch failed ({error:?}); check `bootcode` before rebooting.");
        }
    }
    match installed_loader(&mut fs, b"bootx64.efi") {
        Ok(Some((size, hash))) if size == length as u64 && hash == fingerprint(bytes) => {
            crate::println!("Done: the loader reads back identical. The new loader runs at the next UEFI boot (`reboot`).");
        }
        _ => crate::println!("bootcode: BOOTX64.EFI does not read back as the new loader; check `bootcode` before rebooting."),
    }
}
