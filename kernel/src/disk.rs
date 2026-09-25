use crate::boot_info::{read_u32, read_u64, BootInfo};
use crate::interrupts;
use crate::usb_storage;

const SECTOR_SIZE: usize = 512;

/// The BIOS hard-disk drive-number range this trampoline accepts (`0x80`
/// through `0xff`); floppy drive numbers (`0x00`/`0x01`) are out of scope
/// here and would need their own, separate trampoline entry.
const FIRST_SECONDARY_DRIVE: u8 = 0x81;
/// An arbitrarily generous bound on how many additional (non-boot) disks
/// are probed for at boot -- comfortably more than this lab's machines
/// have BIOS drive letters for, and cheap to raise later if that changes.
const MAX_SECONDARY_DISKS: usize = 4;

/// The largest sector count `bios_disk_io` (`boot/stage2.S`) will accept in
/// one BIOS call, matching the kernel loader's own already-proven safe
/// per-transfer size (`0xfc00` bytes): comfortably inside a real-mode
/// segment's 64 KiB range given the trampoline's requirement that the
/// buffer be 16-byte aligned (so the real-mode offset within that segment
/// is always exactly 0). This is the assembly side's hard ceiling, not
/// necessarily what the kernel actually requests per call -- see
/// [`MAX_TRANSFER_SECTORS`] below for why the kernel deliberately asks for
/// less.
const TRAMPOLINE_MAX_SECTORS: usize = 126;

/// The largest sector count [`device_read_sectors`]/[`device_write_sectors`]
/// actually transfer in one BIOS call. This is deliberately smaller than
/// [`TRAMPOLINE_MAX_SECTORS`]: a `[u8; TRAMPOLINE_MAX_SECTORS * SECTOR_SIZE]`
/// scratch buffer (63 KiB) would not fit in the small headroom left in the
/// kernel's own reserved low-memory `.bss` region (`linker.ld`'s
/// `__kernel_end < 0x9f000` check covers the whole address range .bss
/// occupies, not just what `objcopy -O binary` ships in `kernel.bin`, since
/// physical memory for `.bss` still needs to exist even though the flat
/// image itself carries no bytes for it). 16 sectors (8 KiB) already cuts
/// the number of BIOS round trips for a large file by 16x -- each round
/// trip's own fixed real-mode-transition cost (see `call_trampoline`'s doc
/// comment) is what this batching actually amortizes -- while leaving a
/// healthy few KiB of headroom in the reserved region for future growth,
/// rather than the handful of bytes a larger buffer would leave.
pub const MAX_TRANSFER_SECTORS: usize = 16;

const _: () = assert!(MAX_TRANSFER_SECTORS <= TRAMPOLINE_MAX_SECTORS);
const _: () = assert!(MAX_TRANSFER_SECTORS > 0);

#[repr(align(16))]
struct AlignedSector([u8; SECTOR_SIZE]);

#[repr(align(16))]
struct AlignedChunk([u8; MAX_TRANSFER_SECTORS * SECTOR_SIZE]);

static mut SECTOR: AlignedSector = AlignedSector([0; SECTOR_SIZE]);
/// Scratch buffer for [`device_read_sectors`]/[`device_write_sectors`],
/// separate from the single-sector [`SECTOR`] buffer used everywhere else
/// (directory/superblock reads, which are rare and see no benefit from a
/// bigger buffer). `objcopy -O binary` bakes a `.bss` array's whole size
/// into the shipped `kernel.bin` as zero bytes (see AGENTS.md's "Tape ABI
/// grows alloc/free" section), so this deliberately stays at the smallest
/// size that lets one BIOS call reach the trampoline's own bound rather
/// than being rounded up further.
static mut CHUNK: AlignedChunk = AlignedChunk([0; MAX_TRANSFER_SECTORS * SECTOR_SIZE]);
static mut GPT_DATA_LBA: u64 = 0;
static mut GPT_DATA_SECTORS: u64 = 0;
static mut GPT_VALID: bool = false;

/// How a [`SecondaryDisk`] entry is actually reached. `Bios` is the
/// original, only backend: a BIOS drive number the trampoline addresses
/// exactly like the boot disk. `Usb` is a USB Mass Storage device
/// attached via [`attach_usb_disk`] -- addressed instead through
/// `kernel/src/usb_storage.rs`'s Bulk-Only Transport driver, with no BIOS
/// drive number at all. Both backends report an ordinary `sector_count`
/// and are otherwise indistinguishable to every FOSFS-level caller.
#[derive(Clone, Copy)]
enum SecondaryBackend {
    Bios(u8),
    Usb,
}

#[derive(Clone, Copy)]
struct SecondaryDisk {
    backend: SecondaryBackend,
    sector_count: u64,
}

static mut SECONDARY_DISKS: [Option<SecondaryDisk>; MAX_SECONDARY_DISKS] =
    [None, None, None, None];
static mut SECONDARY_COUNT: usize = 0;
/// The single USB Mass Storage device backing whichever [`SecondaryDisk`]
/// entry has [`SecondaryBackend::Usb`] (there is at most one -- see
/// [`attach_usb_disk`]'s own doc comment for why this driver does not yet
/// support more).
static mut USB_DISK: Option<usb_storage::MscDevice> = None;
/// Set by [`promote_boot_to_usb`] once `Device::Boot` itself has been
/// confirmed to be this exact USB device and switched over to it --
/// completely independent of [`USB_DISK`] above, since promoting the boot
/// disk and attaching an ordinary secondary USB disk are mutually
/// exclusive uses of whatever single USB Mass Storage device is
/// discovered (see that function's own doc comment).
static mut BOOT_USB_DEVICE: Option<usb_storage::MscDevice> = None;

/// Errors specific to *attaching* a USB Mass Storage disk -- distinct
/// from the ordinary [`Error`] enum, which describes a failed read/write
/// against a disk that is already attached, not why one was never
/// attached in the first place.
#[derive(Debug)]
pub enum UsbAttachError {
    /// A USB disk is already attached this boot; see [`attach_usb_disk`].
    AlreadyAttached,
    NoDevice,
    NotReady,
    Capacity(usb_storage::Error),
    /// Every [`MAX_SECONDARY_DISKS`] slot is already in use.
    Full,
}

/// Errors specific to [`promote_boot_to_usb`].
#[derive(Debug)]
pub enum BootPromoteError {
    /// `Device::Boot` is already USB-backed; see [`promote_boot_to_usb`].
    AlreadyPromoted,
    /// Could not read `disk1`'s own boot sector via BIOS before even
    /// attempting discovery -- without this baseline there is nothing to
    /// compare a discovered USB device against, so promotion refuses to
    /// proceed at all rather than guessing.
    BiosReadFailed,
    NoDevice,
    NotReady,
    Capacity(usb_storage::Error),
    /// The discovered USB device's own capacity or first-sector bytes
    /// did not match `disk1`'s BIOS-read baseline -- almost certainly a
    /// different physical disk. `Device::Boot` is left completely
    /// untouched.
    NotSameDisk,
}

/// Identifies which physical disk an FOSFS operation targets. `Boot` is the
/// disk the machine started from (GPT-discovered FOSFS partition, as
/// before); `Secondary` selects one of the additional disks found by
/// [`discover_secondary_disks`], addressed by a 0-based index into that
/// list (`disk2` in shell command syntax is index 0, `disk3` is index 1,
/// and so on).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Device {
    Boot,
    Secondary(usize),
}

const FOSFS_PARTITION_TYPE: [u8; 16] = [
    0x62, 0x73, 0x6f, 0x0f, 0x00, 0x00, 0x53, 0x4f, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x04,
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    OutOfRange,
    Unavailable,
    FirmwareFailure,
}

pub fn sector_count(info: &BootInfo) -> u64 {
    read_u64(core::ptr::addr_of!(info.disk_sector_count))
}

pub fn data_partition_lba(info: &BootInfo) -> u64 {
    if unsafe { GPT_VALID } {
        return unsafe { GPT_DATA_LBA };
    }
    read_u64(core::ptr::addr_of!(info.data_partition_lba))
}

pub fn data_partition_sectors(info: &BootInfo) -> u64 {
    if unsafe { GPT_VALID } {
        return unsafe { GPT_DATA_SECTORS };
    }
    read_u64(core::ptr::addr_of!(info.data_partition_sectors))
}

/// Reads one entry of the secondary-disk table without ever materializing a
/// shared reference to the whole `static mut` array (disallowed under the
/// 2024 edition's `static_mut_refs` lint) -- a single-element raw-pointer
/// read/copy is sufficient since `SecondaryDisk` is `Copy`.
fn secondary_disk(index: usize) -> Option<SecondaryDisk> {
    if index >= MAX_SECONDARY_DISKS {
        return None;
    }
    unsafe { core::ptr::addr_of!(SECONDARY_DISKS[index]).read() }
}

/// The BIOS drive number a [`Device`] should be addressed by, or `None`
/// for one with no BIOS drive number at all -- the boot disk always has
/// one, but a [`SecondaryBackend::Usb`] disk deliberately does not, since
/// it is never addressed through the BIOS trampoline (see `transfer`/
/// `transfer_chunk`'s own backend dispatch).
fn drive_number(info: &BootInfo, device: Device) -> Option<u8> {
    match device {
        Device::Boot => Some(info.boot_drive),
        Device::Secondary(index) => match secondary_disk(index)?.backend {
            SecondaryBackend::Bios(drive_number) => Some(drive_number),
            SecondaryBackend::Usb => None,
        },
    }
}

/// Whether `device` is USB-backed -- either an ordinary
/// [`SecondaryBackend::Usb`] disk, or `Device::Boot` itself once
/// [`promote_boot_to_usb`] has switched it over. `transfer`/
/// `transfer_chunk`'s branch point between the BIOS trampoline and
/// `usb_storage`'s Bulk-Only Transport driver.
fn is_usb_backed(device: Device) -> bool {
    match device {
        Device::Boot => unsafe { (*core::ptr::addr_of!(BOOT_USB_DEVICE)).is_some() },
        Device::Secondary(index) => {
            matches!(secondary_disk(index).map(|disk| disk.backend), Some(SecondaryBackend::Usb))
        }
    }
}

/// How many sectors are addressable on `device`, for bounds-checking reads
/// and writes.
pub fn device_sector_count(info: &BootInfo, device: Device) -> u64 {
    match device {
        Device::Boot => sector_count(info),
        Device::Secondary(index) => {
            secondary_disk(index).map_or(0, |disk| disk.sector_count)
        }
    }
}

/// Where FOSFS's own data lives on `device`. The boot disk keeps its
/// GPT-discovered (or loader-provided) partition offset; a secondary disk
/// has no partition table at all -- it is formatted as one flat FOSFS
/// volume starting at LBA 0, spanning the whole disk, since it is never
/// booted from and so has no need to also be a standards-compliant
/// bootable image (see AGENTS.md's "A second (non-boot) disk" section).
pub fn device_data_partition_lba(info: &BootInfo, device: Device) -> u64 {
    match device {
        Device::Boot => data_partition_lba(info),
        Device::Secondary(_) => 0,
    }
}

pub fn device_data_partition_sectors(info: &BootInfo, device: Device) -> u64 {
    match device {
        Device::Boot => data_partition_sectors(info),
        Device::Secondary(_) => device_sector_count(info, device),
    }
}

/// How many additional (non-boot) disks were found by
/// [`discover_secondary_disks`].
pub fn secondary_count() -> usize {
    unsafe { SECONDARY_COUNT }
}

/// Probes BIOS drives `0x81`, `0x82`, ... (up to [`MAX_SECONDARY_DISKS`])
/// for a second disk to use for config/log storage, alongside the boot
/// disk. Unlike the boot drive (handed to the loader for free in `DL` at
/// boot), nothing tells the kernel whether an additional disk is even
/// attached, so each candidate drive number is checked with the same
/// `AH=41h`/`AH=48h` BIOS Enhanced Disk Drive calls a real partitioning
/// tool would use, via the trampoline's new probe operation.
pub fn discover_secondary_disks(info: &BootInfo) {
    unsafe {
        SECONDARY_COUNT = 0;
    }
    for offset in 0..MAX_SECONDARY_DISKS as u8 {
        let drive_number = FIRST_SECONDARY_DRIVE + offset;
        let Some(sector_count) = probe_drive(info, drive_number) else {
            continue;
        };
        if sector_count == 0 {
            continue;
        }
        let count = unsafe {
            SECONDARY_DISKS[SECONDARY_COUNT] = Some(SecondaryDisk {
                backend: SecondaryBackend::Bios(drive_number),
                sector_count,
            });
            SECONDARY_COUNT += 1;
            SECONDARY_COUNT
        };
        crate::println!(
            "disk{}: found (BIOS drive 0x{:02x}, {} sectors).",
            count + 1,
            drive_number,
            sector_count
        );
    }
}

/// Whether a USB Mass Storage disk is already attached this boot -- at
/// most one is ever supported (see [`attach_usb_disk`]'s own doc
/// comment), so a caller must check this before attempting a second one.
pub fn usb_disk_attached() -> bool {
    unsafe { (*core::ptr::addr_of!(USB_DISK)).is_some() }
}

/// Discovers a USB Mass Storage (Bulk-Only Transport) device and attaches
/// it as a new secondary disk, usable immediately afterward through the
/// exact same `diskN` selector every BIOS-backed secondary disk already
/// uses (`disk::device_from_number`/`strip_disk_selector` need no changes
/// at all: a fresh entry appended to [`SECONDARY_DISKS`] just becomes the
/// next available `diskN`). Returns the new disk's 0-based secondary
/// index (`diskN` = index + 2) on success.
///
/// Deliberately **not** called automatically at boot, unlike
/// [`discover_secondary_disks`]'s cheap, side-effect-free BIOS probes:
/// discovering a USB Mass Storage device means calling
/// `uhci::initialize_all()`, which resets *every* UHCI controller found --
/// the exact operation confirmed, on a real Pentium 4, to break BIOS's own
/// "USB Legacy Support" `INT13h` emulation for the rest of that boot (see
/// AGENTS.md's "USB audio and USB-boot media do not reliably coexist"
/// section). An operator must explicitly opt into this each boot (the
/// `usb storage attach` shell command), on a machine they know is not
/// still relying on BIOS to serve its own boot disk afterward -- the same
/// deliberate, explicit-action convention every other UHCI-touching
/// command (`usb`, `usb play`, `usb capture`, `audio`) already follows.
/// At most one USB disk is supported: `usb_storage::open()` itself only
/// ever discovers the first Bulk-Only Transport device found, and a
/// second `attach_usb_disk` call while one is already attached is
/// rejected outright (`UsbAttachError::AlreadyAttached`) rather than
/// silently replacing the live device behind an already-assigned `diskN`.
///
/// **Confirmed, not just theoretical**: testing this very feature under
/// QEMU found that SeaBIOS's own USB legacy support had *already* made
/// the same physical USB Mass Storage device appear as an ordinary BIOS
/// secondary disk (`disk2`, BIOS drive `0x81`, discovered for free by
/// `discover_secondary_disks`'s boot-time probe) before this function
/// ever ran -- calling this then immediately broke `disk2`'s own BIOS-
/// backed access (`FirmwareFailure` on the very next FOSFS command
/// against it), exactly as this doc comment already warned in the
/// abstract, but now demonstrated concretely rather than only inferred
/// from the real-hardware Pentium 4 finding above. The freshly attached
/// native disk (a *different* `diskN`, e.g. `disk3` in that same test)
/// remained fully readable/writable throughout -- only the now-redundant
/// BIOS-backed path to the identical device broke. A real machine with
/// any USB Mass Storage device already plugged in should be assumed to
/// carry this same risk for *that* device's own BIOS-visible `diskN`,
/// not only for `disk1`/the boot disk specifically.
pub fn attach_usb_disk() -> Result<usize, UsbAttachError> {
    if usb_disk_attached() {
        return Err(UsbAttachError::AlreadyAttached);
    }
    let mut device = usb_storage::open().ok_or(UsbAttachError::NoDevice)?;
    if !device.wait_until_ready(10) {
        return Err(UsbAttachError::NotReady);
    }
    let (block_count, _block_size) = device.read_capacity_10().map_err(UsbAttachError::Capacity)?;

    unsafe {
        if SECONDARY_COUNT >= MAX_SECONDARY_DISKS {
            return Err(UsbAttachError::Full);
        }
        let index = SECONDARY_COUNT;
        SECONDARY_DISKS[index] = Some(SecondaryDisk {
            backend: SecondaryBackend::Usb,
            sector_count: u64::from(block_count),
        });
        SECONDARY_COUNT += 1;
        USB_DISK = Some(device);
        Ok(index)
    }
}

/// Whether `Device::Boot` has already been switched over to a native USB
/// backend by [`promote_boot_to_usb`].
pub fn boot_is_usb() -> bool {
    is_usb_backed(Device::Boot)
}

/// Discovers a USB Mass Storage device and, **only if it can be confirmed
/// to be the exact same physical disk `Device::Boot` already is**,
/// switches `Device::Boot`'s own sector transfers over to it -- closing
/// the real-hardware Pentium 4 gap this whole driver was built for: once
/// promoted, ordinary disk access no longer depends on BIOS's "USB Legacy
/// Support" `INT13h` emulation staying intact, so a later `usb`/`usb
/// play`/`usb capture`/`audio` command (or another `attach_usb_disk` for
/// a *different* device) can no longer break `disk1` merely by resetting
/// a UHCI controller.
///
/// fos has no reliable way to ask BIOS "is this drive number a USB
/// device" or to correlate a discovered USB device with a specific BIOS
/// drive number -- so this does **not** use the weaker "assume the only
/// discovered device is the boot device" heuristic the original scoping
/// plan first considered. Instead it cross-validates using the disk's
/// actual content: `disk1`'s own LBA 0 (its MBR -- present and BIOS-
/// readable on every fos image regardless of GPT/partition layout) is
/// read via the *ordinary* BIOS path first, strictly before any UHCI
/// controller is ever touched (discovering the USB device below may
/// itself break BIOS's access to this exact disk, per the confirmed
/// mechanism `attach_usb_disk` documents -- so this baseline read is a
/// one-shot opportunity, taken before that risk exists). Only if the
/// discovered USB device's own LBA 0 bytes match that baseline exactly,
/// and its own reported capacity is *at least* as large as BIOS's own
/// figure for `disk1`, is `Device::Boot` switched over; otherwise it is
/// almost certainly a different physical disk, and `Device::Boot` is left
/// completely untouched (`BootPromoteError::NotSameDisk`). Capacity is
/// deliberately a floor, not an exact match: confirmed on real hardware
/// that a USB-boot BIOS's own legacy emulation can report a *smaller*
/// capacity for `disk1` than the physical device truly has -- it reports
/// the geometry baked into the boot image's own GPT (the original
/// image's size, e.g. 16 MiB) rather than querying the real flash
/// media's true capacity, so an exact-match requirement would wrongly
/// refuse the device's own real boot media. The MBR content match is the
/// real identity check (two independent physical disks sharing a
/// byte-identical MBR is not realistically possible by accident), though
/// not an absolute proof -- deliberately documented as "confirmed, not
/// just assumed," matching this project's own "heuristic, not
/// certainty" framing for this whole follow-on.
///
/// Deliberately still does **not** fix `reboot soft`: that path re-enters
/// `boot/stage2.S` from scratch and always depends on BIOS to reload the
/// kernel, completely independent of this function's own, purely
/// Rust-level `BOOT_USB_DEVICE` state (which a fresh boot always
/// reinitializes to `None` regardless). If `disk1`'s BIOS access is
/// already broken by the time a soft reboot is attempted, it fails
/// exactly as already documented in AGENTS.md's "USB audio and USB-boot
/// media do not reliably coexist" section, promoted or not.
pub fn promote_boot_to_usb(info: &BootInfo) -> Result<(), BootPromoteError> {
    if is_usb_backed(Device::Boot) {
        return Err(BootPromoteError::AlreadyPromoted);
    }
    let baseline = read_sector(info, 0, |sector| *sector).map_err(|_| BootPromoteError::BiosReadFailed)?;
    let boot_sectors = device_sector_count(info, Device::Boot);

    let mut device = usb_storage::open().ok_or(BootPromoteError::NoDevice)?;
    if !device.wait_until_ready(10) {
        return Err(BootPromoteError::NotReady);
    }
    let (block_count, _block_size) = device.read_capacity_10().map_err(BootPromoteError::Capacity)?;
    let mut candidate = [0u8; SECTOR_SIZE];
    if device.read_10(0, &mut candidate).is_err() {
        return Err(BootPromoteError::NotSameDisk);
    }
    // A real USB-boot BIOS commonly reports a *smaller* capacity for
    // `disk1` than the physical device actually has -- confirmed on real
    // hardware: USB Legacy emulation reports the drive geometry baked
    // into the boot image's own GPT (the original image's size, e.g.
    // 16 MiB) rather than querying the real flash media's true capacity.
    // So the discovered device's capacity only needs to be *at least* as
    // large as BIOS's own figure (it must have had room to hold whatever
    // was imaged onto it in the first place), not exactly equal -- the
    // real identity check is the MBR content match just below: a
    // byte-for-byte 512-byte match is what actually makes this
    // "confirmed, not just assumed."
    if u64::from(block_count) < boot_sectors || candidate != baseline {
        return Err(BootPromoteError::NotSameDisk);
    }

    unsafe {
        BOOT_USB_DEVICE = Some(device);
    }
    Ok(())
}

fn probe_drive(info: &BootInfo, drive_number: u8) -> Option<u64> {
    call_trampoline(
        info,
        2,
        0,
        drive_number,
        core::ptr::addr_of!(SECTOR) as u32,
        1,
    )
    .ok()?;
    let sector = unsafe { &*core::ptr::addr_of!(SECTOR.0) };
    Some(get_u64(sector, 0))
}

/// Discovers the FOSFS partition from the primary GPT, falling back to the
/// loader-provided geometry if an older image has no valid GPT.
pub fn initialize(info: &BootInfo) {
    let disk_sectors = sector_count(info);
    let Ok(valid) = read_sector(info, 1, |sector| {
        if &sector[..8] != b"EFI PART"
            || get_u32(sector, 8) != 0x0001_0000
            || get_u32(sector, 12) < 92
        {
            return None;
        }
        let entries_lba = get_u64(sector, 72);
        let entry_count = get_u32(sector, 80);
        let entry_size = get_u32(sector, 84);
        if entries_lba >= disk_sectors
            || entry_count == 0
            || entry_count > 128
            || entry_size < 128
            || entry_size > SECTOR_SIZE as u32
        {
            return None;
        }
        Some((entries_lba, entry_count, entry_size))
    }) else {
        return;
    };
    let Some((entries_lba, entry_count, entry_size)) = valid else {
        return;
    };

    for index in 0..entry_count as u64 {
        let byte_offset = index * entry_size as u64;
        let lba = entries_lba + byte_offset / SECTOR_SIZE as u64;
        let offset = (byte_offset % SECTOR_SIZE as u64) as usize;
        let Ok(entry) = read_sector(info, lba, |sector| {
            if offset + 48 > SECTOR_SIZE {
                return None;
            }
            if sector[offset..offset + 16] != FOSFS_PARTITION_TYPE {
                return None;
            }
            Some((get_u64(sector, offset + 32), get_u64(sector, offset + 40)))
        }) else {
            return;
        };
        let Some((first, last)) = entry else {
            continue;
        };
        if first == 0 || last < first || last >= disk_sectors {
            return;
        }
        unsafe {
            GPT_DATA_LBA = first;
            GPT_DATA_SECTORS = last - first + 1;
            GPT_VALID = true;
        }
        crate::println!(
            "GPT: discovered FOSFS partition at LBA {} + {} sectors.",
            first,
            last - first + 1
        );
        return;
    }
}

pub fn read_sector<T>(
    info: &BootInfo,
    lba: u64,
    operation: impl FnOnce(&[u8; SECTOR_SIZE]) -> T,
) -> Result<T, Error> {
    device_read_sector(info, Device::Boot, lba, operation)
}

/// Same as [`read_sector`], but against an explicit [`Device`] rather than
/// always the boot disk -- the one addition FOSFS operations on a second
/// disk need.
pub fn device_read_sector<T>(
    info: &BootInfo,
    device: Device,
    lba: u64,
    operation: impl FnOnce(&[u8; SECTOR_SIZE]) -> T,
) -> Result<T, Error> {
    transfer(info, device, lba, 0)?;
    let sector = unsafe { &*core::ptr::addr_of!(SECTOR.0) };
    Ok(operation(sector))
}

pub fn device_write_sector(
    info: &BootInfo,
    device: Device,
    lba: u64,
    operation: impl FnOnce(&mut [u8; SECTOR_SIZE]),
) -> Result<(), Error> {
    let sector = unsafe { &mut *core::ptr::addr_of_mut!(SECTOR.0) };
    operation(sector);
    transfer(info, device, lba, 1)
}

/// Reads `count` (1..=[`MAX_TRANSFER_SECTORS`]) contiguous sectors starting
/// at `lba` in one BIOS call, exposing them as one `count * SECTOR_SIZE`
/// byte slice -- the multi-sector counterpart to [`device_read_sector`]. A
/// caller needing more sectors than [`MAX_TRANSFER_SECTORS`] must call this
/// repeatedly with successive `lba`s (see `fosfs.rs`'s chunked loops), since
/// a single BIOS call cannot itself cross the trampoline's per-transfer
/// bound.
pub fn device_read_sectors<T>(
    info: &BootInfo,
    device: Device,
    lba: u64,
    count: usize,
    operation: impl FnOnce(&[u8]) -> T,
) -> Result<T, Error> {
    transfer_chunk(info, device, lba, count, 0)?;
    let chunk = unsafe { &*core::ptr::addr_of!(CHUNK.0) };
    Ok(operation(&chunk[..count * SECTOR_SIZE]))
}

/// Same as [`device_read_sectors`], but for writes: `operation` fills up to
/// `count * SECTOR_SIZE` bytes of scratch space (already zeroed, matching
/// [`device_write_sector`]'s single-sector contract of receiving a
/// zero-initialized buffer to fill) before it is written out in one BIOS
/// call.
pub fn device_write_sectors(
    info: &BootInfo,
    device: Device,
    lba: u64,
    count: usize,
    operation: impl FnOnce(&mut [u8]),
) -> Result<(), Error> {
    if count == 0 || count > MAX_TRANSFER_SECTORS {
        return Err(Error::OutOfRange);
    }
    let chunk = unsafe { &mut *core::ptr::addr_of_mut!(CHUNK.0) };
    let region = &mut chunk[..count * SECTOR_SIZE];
    region.fill(0);
    operation(region);
    transfer_chunk(info, device, lba, count, 1)
}

fn transfer(info: &BootInfo, device: Device, lba: u64, direction: u32) -> Result<(), Error> {
    if lba >= device_sector_count(info, device) {
        return Err(Error::OutOfRange);
    }
    if is_usb_backed(device) {
        let sector = unsafe { &mut *core::ptr::addr_of_mut!(SECTOR.0) };
        return usb_transfer(device, lba, sector, direction);
    }
    let Some(drive_number) = drive_number(info, device) else {
        return Err(Error::Unavailable);
    };
    call_trampoline(
        info,
        direction,
        lba,
        drive_number,
        core::ptr::addr_of!(SECTOR) as u32,
        1,
    )
}

/// Same as [`transfer`], but for a multi-sector operation against the
/// shared [`CHUNK`] buffer.
fn transfer_chunk(
    info: &BootInfo,
    device: Device,
    lba: u64,
    count: usize,
    direction: u32,
) -> Result<(), Error> {
    if count == 0 || count > MAX_TRANSFER_SECTORS {
        return Err(Error::OutOfRange);
    }
    if lba + count as u64 > device_sector_count(info, device) {
        return Err(Error::OutOfRange);
    }
    if is_usb_backed(device) {
        let chunk = unsafe { &mut *core::ptr::addr_of_mut!(CHUNK.0) };
        return usb_transfer(device, lba, &mut chunk[..count * SECTOR_SIZE], direction);
    }
    let Some(drive_number) = drive_number(info, device) else {
        return Err(Error::Unavailable);
    };
    call_trampoline(
        info,
        direction,
        lba,
        drive_number,
        core::ptr::addr_of!(CHUNK) as u32,
        count as u32,
    )
}

/// The `Usb`-backed counterpart to `call_trampoline`: reads or writes
/// `buffer` (already exactly a whole number of 512-byte blocks -- one for
/// [`transfer`], up to [`MAX_TRANSFER_SECTORS`] for [`transfer_chunk`])
/// through whichever USB device backs `device` -- [`BOOT_USB_DEVICE`] for
/// a promoted `Device::Boot`, [`USB_DISK`] for an ordinary
/// `SecondaryBackend::Usb` disk -- via `usb_storage`'s `READ(10)`/
/// `WRITE(10)`. The caller has already bounds-checked `lba` against
/// [`device_sector_count`]; SCSI `READ(10)`/`WRITE(10)` only carry a
/// 32-bit LBA, so this additionally rejects (as [`Error::OutOfRange`]) an
/// address that would need more than 32 bits -- unreachable in practice
/// for any USB flash drive's actual capacity, but a real, explicit check
/// rather than a silent truncation.
fn usb_transfer(device: Device, lba: u64, buffer: &mut [u8], direction: u32) -> Result<(), Error> {
    let Ok(lba) = u32::try_from(lba) else {
        return Err(Error::OutOfRange);
    };
    let handle = match device {
        Device::Boot => unsafe { (*core::ptr::addr_of_mut!(BOOT_USB_DEVICE)).as_mut() },
        Device::Secondary(_) => unsafe { (*core::ptr::addr_of_mut!(USB_DISK)).as_mut() },
    };
    let Some(device) = handle else {
        return Err(Error::Unavailable);
    };
    let result = if direction == 0 { device.read_10(lba, buffer) } else { device.write_10(lba, buffer) };
    result.map_err(|_| Error::FirmwareFailure)
}

/// Invokes the `bios_disk_io` trampoline (`boot/stage2.S`) for one
/// operation against `drive_number`, transferring `sector_count`
/// (1..=[`MAX_TRANSFER_SECTORS`]) contiguous sectors starting at `lba`
/// to/from `buffer` (either [`SECTOR`]'s or [`CHUNK`]'s address). `lba` and
/// `sector_count` are both ignored by the probe operation (`direction ==
/// 2`), which instead fills the buffer with an 8-byte total sector count on
/// success.
fn call_trampoline(
    info: &BootInfo,
    direction: u32,
    lba: u64,
    drive_number: u8,
    buffer: u32,
    sector_count: u32,
) -> Result<(), Error> {
    let address = read_u32(core::ptr::addr_of!(info.bios_disk_io));
    if address == 0 {
        return Err(Error::Unavailable);
    }
    let routine: unsafe extern "C" fn(u32, u32, u32, u32, u32, u32) -> u32 =
        unsafe { core::mem::transmute(address as usize) };
    // The trampoline drops into real mode to run genuine BIOS code. BIOS
    // expects both the traditional real-mode IVT at address 0 (our own IDT
    // holds 8-byte protected-mode gate descriptors at a different address,
    // which would be misread as garbage 4-byte real-mode entries) and the
    // PIC's BIOS-conventional vector mapping (IRQ0 -> INT 08h, whose
    // real-mode handler advances the BDA tick counter that BIOS
    // disk-timeout logic polls). Restore both for the duration of the call,
    // with interrupts disabled around each switch so nothing can be
    // misdelivered mid-reprogramming. Keep interrupts disabled until the
    // trampoline has actually switched to real mode; while the CPU is still
    // in protected mode, IDTR=0 points at 4-byte real-mode IVT entries, not
    // valid 8-byte protected-mode gates, so an IRQ in that small window
    // would triple-fault the machine. The trampoline enables interrupts only
    // after entering real mode and disables them again before returning.
    unsafe {
        core::arch::asm!("cli", options(nomem, nostack));
    }
    interrupts::use_real_mode_idt();
    interrupts::use_bios_pic_mapping();
    let result = unsafe {
        routine(
            direction,
            lba as u32,
            (lba >> 32) as u32,
            buffer,
            drive_number as u32,
            sector_count,
        )
    };
    unsafe {
        core::arch::asm!("cli", options(nomem, nostack));
    }
    crate::gdt::restore_after_bios();
    interrupts::use_kernel_idt();
    interrupts::use_kernel_pic_mapping();
    // Some USB legacy-emulation BIOSes alter the keyboard scan-set/8042
    // translation state while handling INT 13h. Restore the Set 1 contract
    // before IRQ1 can resume delivering bytes to the shell decoder.
    crate::keyboard::restore_after_bios_disk_call();
    unsafe {
        core::arch::asm!("sti", options(nomem, nostack));
    }
    if result == 0 {
        Ok(())
    } else {
        Err(Error::FirmwareFailure)
    }
}

fn get_u32(bytes: &[u8; SECTOR_SIZE], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn get_u64(bytes: &[u8; SECTOR_SIZE], offset: usize) -> u64 {
    u64::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
        bytes[offset + 4],
        bytes[offset + 5],
        bytes[offset + 6],
        bytes[offset + 7],
    ])
}
