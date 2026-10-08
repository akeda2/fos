//! `bootcode [diskN]` and `bootcode update [diskN]`: look at, and replace, the BIOS boot code of a fos disk without
//! rewriting the disk.
//!
//! The BIOS boot chain (boot/bios) is not in a kernel slot, so KUP (the network kernel update) cannot change it: stage one
//! is the first 446 bytes of sector 0 (the MBR's code area; the partition table and the 0x55aa signature stay), stage two
//! is `BIOS_STAGE2_SECTORS` sectors at LBA `BIOS_STAGE2_LBA`, in the gap a GPT leaves before its first partition. Updating
//! them used to mean writing the whole stick again with `tools/write_stick.py` and losing its files. Now:
//!
//! 1. put the new `stage1.bin` and `stage2.bin` (build/bios/) on the boot volume, e.g. `foscopy.py FILE :disk1/stage2.bin`;
//! 2. `bootcode update` announces what it would write; `bootcode update confirm` as the very next command does it.
//!
//! Safety, in the style of `format` and `install`: the target must be a fos disk (it has a payload partition); both files are
//! checked before anything is written (stage one exactly one sector with the signature and an empty partition-table area;
//! stage two non-empty, at most 24 sectors, starting with `cli` and carrying stage two's banner); stage two is written first
//! and read back, stage one's code last and read back, so an update interrupted half way leaves the old, working MBR code in
//! front of a complete stage two, or a stage two that is merely newer than its MBR -- never an MBR pointing at a half-written
//! stage two. It does not touch the UEFI loader on the ESP, and it never writes to the boot disk's partitions.

use fosfor_abi::{BIOS_STAGE2_LBA, BIOS_STAGE2_SECTORS, PAYLOAD_TYPE_GUID};

use crate::{block, gpt, storage};

const SECTOR: usize = 512;
const STAGE2_BYTES: usize = BIOS_STAGE2_SECTORS as usize * SECTOR;
/// Where stage one's code ends: bytes 446-509 are the partition table, 510-511 the signature.
const CODE_BYTES: usize = 446;

static mut PENDING: Option<usize> = None;
static mut STAGE1_NEW: [u8; SECTOR] = [0; SECTOR];
static mut STAGE2_NEW: [u8; STAGE2_BYTES] = [0; STAGE2_BYTES];
static mut STAGE2_NOW: [u8; STAGE2_BYTES] = [0; STAGE2_BYTES];

/// Forgets an announced update: the confirmation must be the very next command.
pub fn cancel() {
    unsafe { PENDING = None };
}

/// FNV-1a over `bytes`: a short fingerprint to compare two copies by (not a security measure).
fn fingerprint(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0x811c_9dc5u32, |hash, &byte| (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193))
}

/// Reads the two new files from the boot volume into the static buffers and checks them. Returns stage two's length.
fn load_files() -> Result<usize, &'static str> {
    let volume = storage::boot_volume().ok_or("the boot volume is not mounted, so there are no files to take")?;
    let stage1 = unsafe { &mut *core::ptr::addr_of_mut!(STAGE1_NEW) };
    let stage2 = unsafe { &mut *core::ptr::addr_of_mut!(STAGE2_NEW) };
    stage1.fill(0);
    stage2.fill(0);
    let mut scratch = [0u8; SECTOR + 1];
    match volume.read(b"stage1.bin", &mut scratch) {
        Ok(Some(length)) if length == SECTOR => stage1.copy_from_slice(&scratch[..SECTOR]),
        Ok(Some(_)) => return Err("stage1.bin must be exactly 512 bytes (one sector)"),
        Ok(None) => return Err("there is no stage1.bin on the boot volume (build/bios/stage1.bin)"),
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
        Ok(None) => return Err("there is no stage2.bin on the boot volume (build/bios/stage2.bin)"),
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
    Ok(length)
}

/// What is on `disk` now: whether the MBR carries boot code and the fingerprint of stage two's area.
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
    let (has_code, installed) = match read_installed(disk) {
        Ok(found) => found,
        Err(error) => return crate::println!("bootcode: cannot read disk{} ({error:?}).", disk + 1),
    };
    if !update {
        crate::println!("disk{}: {} MiB. BIOS boot code: stage one {}, stage two at LBA {} (fingerprint {:08x}).", disk + 1, info.sectors / 2048,
            if has_code { "present in the MBR" } else { "absent (UEFI-only disk)" }, BIOS_STAGE2_LBA, installed);
        match load_files() {
            Ok(length) => {
                let new = unsafe { &*core::ptr::addr_of!(STAGE2_NEW) };
                let same = fingerprint(new) == installed;
                crate::println!("The boot volume has stage1.bin and stage2.bin ({length} bytes); stage two there is {}.", if same { "identical to the installed one" } else { "different from the installed one" });
            }
            Err(reason) => crate::println!("Files for an update: {reason}."),
        }
        return;
    }
    if confirm {
        let pending = unsafe { (*core::ptr::addr_of_mut!(PENDING)).take() };
        if pending != Some(disk) {
            return crate::println!("bootcode: nothing is waiting for confirmation on disk{}; run `bootcode update` first.", disk + 1);
        }
        return write(disk);
    }
    let length = match load_files() {
        Ok(length) => length,
        Err(reason) => return crate::println!("bootcode: {reason}."),
    };
    crate::println!("disk{}: stage two ({length} bytes) goes to LBA {}-{} and stage one's code into the MBR's first 446 bytes;",
        disk + 1, BIOS_STAGE2_LBA, BIOS_STAGE2_LBA + BIOS_STAGE2_SECTORS - 1);
    crate::println!("the partition table and everything else on the disk stay. A mistake here can leave the disk unbootable by BIOS.");
    unsafe { PENDING = Some(disk) };
    if number.is_some() {
        crate::println!("To go ahead, type exactly: bootcode update disk{} confirm", disk + 1);
    } else {
        crate::println!("To go ahead, type exactly: bootcode update confirm");
    }
}

fn write(disk: usize) {
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
