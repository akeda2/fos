//! The block-device layer: every disk the drivers found, by 512-byte sector. The same shape as the embedded kernel's
//! `block.rs` (SATA, NVMe and USB mass storage). Registration happens on the boot CPU
//! during start-up; reads and writes go straight to the drivers.

use core::sync::atomic::{AtomicU64, Ordering};
use kernel_core::{ahci, nvme};

pub const MAX_DISKS: usize = 16;

#[derive(Clone, Copy)]
pub enum Backend {
    Ahci(ahci::Drive),
    Nvme(nvme::Drive),
    /// A virtio disk (QEMU on aarch64).
    #[cfg(target_arch = "aarch64")]
    Virtio(kernel_core::virtio_blk::Drive),
    #[cfg(target_arch = "aarch64")]
    Sd(crate::sdhci::Drive),
    /// An index into `usb_storage`'s device table.
    Usb(usize),
    /// The array `mdvol` assembled: the disk is virtual, its sectors come from the members.
    Md,
    /// A disk another machine exports over ATA over Ethernet (`aoedisk`), by its entry there.
    Aoe(usize),
}

#[derive(Clone, Copy)]
pub struct Disk {
    pub backend: Backend,
    pub sectors: u64,
    pub model: [u8; 40],
    pub origin: [u8; 16],
    pub origin_length: usize,
}

#[derive(Debug, Clone, Copy)]
pub enum Error {
    NoSuchDisk,
    OutOfRange,
    Device,
    /// The disk cannot do it (TRIM on a disk without it).
    Unsupported,
}

static mut DISKS: [Option<Disk>; MAX_DISKS] = [None; MAX_DISKS];
static mut COUNT: usize = 0;

pub fn register(backend: Backend, sectors: u64, model: [u8; 40], origin: &str) -> Option<usize> {
    unsafe {
        if COUNT == MAX_DISKS {
            return None;
        }
        let mut name = [0u8; 16];
        let length = origin.len().min(16);
        name[..length].copy_from_slice(&origin.as_bytes()[..length]);
        (*core::ptr::addr_of_mut!(DISKS))[COUNT] = Some(Disk { backend, sectors, model, origin: name, origin_length: length });
        COUNT += 1;
        Some(COUNT - 1)
    }
}

/// Makes disk `index` disk number 1, the others keeping their order behind it. Start-up only.
pub fn move_to_front(index: usize) -> usize {
    unsafe {
        if index < COUNT {
            (&mut *core::ptr::addr_of_mut!(DISKS))[..=index].rotate_right(1);
            return 0;
        }
    }
    index
}

/// Makes the virtual disk `index` (a stopped md array, a detached AoE disk) live again with a new size and name. False when
/// `index` is not a virtual disk.
pub fn set_virtual(index: usize, sectors: u64, model: [u8; 40]) -> bool {
    unsafe {
        match (*core::ptr::addr_of_mut!(DISKS)).get_mut(index) {
            Some(Some(disk)) if matches!(disk.backend, Backend::Md | Backend::Aoe(_)) => {
                disk.sectors = sectors;
                disk.model = model;
                true
            }
            _ => false,
        }
    }
}

pub fn count() -> usize {
    unsafe { COUNT }
}

/// The disk number of the USB mass-storage device in `usb_storage`'s slot `slot` (disk numbers move when the boot disk is put
/// first, so the slot is the stable identity).
pub fn usb_disk(slot: usize) -> Option<usize> {
    (0..count()).find(|&index| matches!(get(index), Some(Disk { backend: Backend::Usb(found), .. }) if found == slot))
}

pub fn get(index: usize) -> Option<Disk> {
    unsafe { (*core::ptr::addr_of!(DISKS)).get(index).copied().flatten() }
}

fn checked(disk: usize, lba: u64, length: usize) -> Result<Disk, Error> {
    let info = get(disk).ok_or(Error::NoSuchDisk)?;
    if length % 512 != 0 || lba + (length / 512) as u64 > info.sectors {
        return Err(Error::OutOfRange);
    }
    Ok(info)
}

/// DMA drivers that hand `address - offset` to the device need a buffer in the direct map; a thread's stack is mapped
/// elsewhere (`vm::STACK_REGION`), so such a buffer goes through a heap copy.
#[cfg(target_arch = "aarch64")]
fn on_stack(buffer: &[u8]) -> bool {
    buffer.as_ptr() as u64 >= crate::vm::STACK_REGION
}

#[cfg(target_arch = "aarch64")]
fn with_direct_read<T>(buffer: &mut [u8], transfer: impl FnOnce(&mut [u8]) -> T) -> T {
    if !on_stack(buffer) {
        return transfer(buffer);
    }
    let mut bounce = alloc::vec![0u8; buffer.len()];
    let result = transfer(&mut bounce);
    buffer.copy_from_slice(&bounce);
    result
}

#[cfg(target_arch = "aarch64")]
fn with_direct_write<T>(buffer: &[u8], transfer: impl FnOnce(&[u8]) -> T) -> T {
    if on_stack(buffer) { transfer(&alloc::vec::Vec::from(buffer)) } else { transfer(buffer) }
}

/// Reads whole sectors (`buffer.len()` a multiple of 512) from `lba`.
pub fn read(disk: usize, lba: u64, buffer: &mut [u8]) -> Result<(), Error> {
    match checked(disk, lba, buffer.len())?.backend {
        Backend::Ahci(drive) => {
            for (index, chunk) in buffer.chunks_mut(ahci::MAX_SECTORS * 512).enumerate() {
                drive.read(lba + (index * ahci::MAX_SECTORS) as u64, chunk).map_err(|_| Error::Device)?;
            }
        }
        Backend::Nvme(drive) => {
            for (index, chunk) in buffer.chunks_mut(nvme::MAX_SECTORS * 512).enumerate() {
                drive.read(lba + (index * nvme::MAX_SECTORS) as u64, chunk).map_err(|_| Error::Device)?;
            }
        }
        #[cfg(target_arch = "aarch64")]
        Backend::Virtio(drive) => with_direct_read(buffer, |buffer| drive.read(lba, buffer)).map_err(|_| Error::Device)?,
        #[cfg(target_arch = "aarch64")]
        Backend::Sd(drive) => with_direct_read(buffer, |buffer| drive.read(lba, buffer)).map_err(|_| Error::Device)?,
        Backend::Usb(device) => crate::usb::storage_read(device, lba, buffer).map_err(|_| Error::Device)?,
        Backend::Md => crate::mdvol::read(lba, buffer)?,
        Backend::Aoe(remote) => crate::aoedisk::read(remote, lba, buffer)?,
    }
    Ok(())
}

/// One range for `read_many`.
pub struct Read<'a> {
    pub disk: usize,
    pub lba: u64,
    pub buffer: &'a mut [u8],
}

/// Most ranges `read_many` overlaps at once; a longer list goes in groups of this many.
const MANY: usize = 16;

/// Reads several ranges, overlapping those on different SATA drives (`overlap`): each round starts one command of up to
/// `ahci::MAX_SECTORS` on every AHCI drive that still has sectors to read, then collects them all. A drive is a port with
/// one command slot here, so two ranges on one drive take turns. Other disks are read one after another, as `read` does.
/// An md array's members read this way move at the speed of the slowest drive, not the sum of them all (`mdvol`).
/// On failure: the index of a range that failed. Every command started is collected first, so no drive is left busy.
pub fn read_many(reads: &mut [Read<'_>]) -> Result<(), (usize, Error)> {
    let mut first = 0;
    for group in reads.chunks_mut(MANY) {
        read_group(group).map_err(|(index, error)| (first + index, error))?;
        first += MANY;
    }
    Ok(())
}

fn read_group(reads: &mut [Read<'_>]) -> Result<(), (usize, Error)> {
    let mut drives: [Option<ahci::Drive>; MANY] = [None; MANY];
    let (mut lbas, mut lengths) = ([0u64; MANY], [0usize; MANY]);
    for (index, request) in reads.iter_mut().enumerate() {
        match checked(request.disk, request.lba, request.buffer.len()).map_err(|error| (index, error))?.backend {
            Backend::Ahci(drive) => drives[index] = Some(drive),
            _ => read(request.disk, request.lba, request.buffer).map_err(|error| (index, error))?,
        }
        (lbas[index], lengths[index]) = (request.lba, request.buffer.len());
    }
    overlap(
        &drives[..reads.len()],
        &lengths,
        |index, drive, done, bytes| timed(&MANY_START, || drive.start_read(lbas[index] + (done / 512) as u64, bytes)),
        |index, drive, done, bytes| {
            timed(&MANY_WAIT, || drive.wait())?;
            timed(&MANY_COPY, || drive.copy_out(&mut reads[index].buffer[done..done + bytes]));
            counted(bytes);
            Ok(())
        },
    )
}

/// One range for `write_many`.
pub struct Write<'a> {
    pub disk: usize,
    pub lba: u64,
    pub buffer: &'a [u8],
}

/// `write_deferred` of several ranges, overlapping those on different SATA drives as `read_many` does (no cache flush:
/// `flush` each disk afterwards for durability). On failure: the index of a range that failed; the others may or may
/// not have been written.
pub fn write_many(writes: &[Write<'_>]) -> Result<(), (usize, Error)> {
    let mut first = 0;
    for group in writes.chunks(MANY) {
        write_group(group).map_err(|(index, error)| (first + index, error))?;
        first += MANY;
    }
    Ok(())
}

fn write_group(writes: &[Write<'_>]) -> Result<(), (usize, Error)> {
    let mut drives: [Option<ahci::Drive>; MANY] = [None; MANY];
    let mut lengths = [0usize; MANY];
    for (index, request) in writes.iter().enumerate() {
        unguarded(request.disk, request.lba, request.buffer).map_err(|error| (index, error))?;
        match checked(request.disk, request.lba, request.buffer.len()).map_err(|error| (index, error))?.backend {
            Backend::Ahci(drive) => drives[index] = Some(drive),
            _ => write_with(request.disk, request.lba, request.buffer, false).map_err(|error| (index, error))?,
        }
        lengths[index] = request.buffer.len();
    }
    overlap(
        &drives[..writes.len()],
        &lengths,
        |index, drive, done, bytes| {
            timed(&MANY_COPY, || drive.stage(&writes[index].buffer[done..done + bytes]))?;
            timed(&MANY_START, || drive.start_staged_write(writes[index].lba + (done / 512) as u64, bytes))
        },
        |_, drive, _, bytes| {
            timed(&MANY_WAIT, || drive.wait())?;
            counted(bytes);
            Ok(())
        },
    )
}

/// Where `read_many` and `write_many` spent their time on SATA drives, in microseconds of the caller's wall clock, since
/// boot: starting commands (filling in the command table, ringing the port), waiting for them, and copying through the
/// drives' bounce buffers; with the commands and bytes moved. `md` takes the difference over a resync or rebuild to say
/// where the job's time goes.
#[derive(Clone, Copy, Default)]
pub struct ManyTimes {
    pub start_us: u64,
    pub wait_us: u64,
    pub copy_us: u64,
    pub commands: u64,
    pub bytes: u64,
}

impl ManyTimes {
    /// What happened between `earlier` and `self`.
    pub fn since(&self, earlier: &ManyTimes) -> ManyTimes {
        ManyTimes {
            start_us: self.start_us - earlier.start_us,
            wait_us: self.wait_us - earlier.wait_us,
            copy_us: self.copy_us - earlier.copy_us,
            commands: self.commands - earlier.commands,
            bytes: self.bytes - earlier.bytes,
        }
    }
}

static MANY_START: AtomicU64 = AtomicU64::new(0);
static MANY_WAIT: AtomicU64 = AtomicU64::new(0);
static MANY_COPY: AtomicU64 = AtomicU64::new(0);
static MANY_COMMANDS: AtomicU64 = AtomicU64::new(0);
static MANY_BYTES: AtomicU64 = AtomicU64::new(0);

pub fn many_times() -> ManyTimes {
    ManyTimes {
        start_us: MANY_START.load(Ordering::Relaxed),
        wait_us: MANY_WAIT.load(Ordering::Relaxed),
        copy_us: MANY_COPY.load(Ordering::Relaxed),
        commands: MANY_COMMANDS.load(Ordering::Relaxed),
        bytes: MANY_BYTES.load(Ordering::Relaxed),
    }
}

/// Runs `work`, adding its wall time to `counter` (the TSC clock: a few nanoseconds a reading).
fn timed<R>(counter: &AtomicU64, work: impl FnOnce() -> R) -> R {
    let begin = crate::arch::micros();
    let result = work();
    counter.fetch_add(crate::arch::micros().saturating_sub(begin), Ordering::Relaxed);
    result
}

fn counted(bytes: usize) {
    MANY_COMMANDS.fetch_add(1, Ordering::Relaxed);
    MANY_BYTES.fetch_add(bytes as u64, Ordering::Relaxed);
}

/// The rounds `read_many` and `write_many` share. Each round starts one command of up to `ahci::MAX_SECTORS` on every
/// AHCI drive (`drives[i]`, None for a range already done another way) that still has bytes of its `lengths[i]` to move,
/// skipping a drive that already has one this round, then finishes them all in order. `start(i, drive, done, bytes)`
/// issues bytes `done..done + bytes` of range `i`, `finish` with the same arguments waits for it. Every started command
/// is finished before an error is returned, so no drive is left busy.
fn overlap(
    drives: &[Option<ahci::Drive>],
    lengths: &[usize],
    mut start: impl FnMut(usize, ahci::Drive, usize, usize) -> Result<(), ahci::Error>,
    mut finish: impl FnMut(usize, ahci::Drive, usize, usize) -> Result<(), ahci::Error>,
) -> Result<(), (usize, Error)> {
    let mut done = [0usize; MANY];
    loop {
        let mut started = [false; MANY];
        let mut failed = None;
        for (index, drive) in drives.iter().enumerate() {
            let Some(drive) = *drive else { continue };
            let left = lengths[index] - done[index];
            let busy = (0..index).any(|other| started[other] && drives[other].is_some_and(|d| d.same_port(&drive)));
            if left == 0 || busy {
                continue;
            }
            match start(index, drive, done[index], left.min(ahci::MAX_SECTORS * 512)) {
                Ok(()) => started[index] = true,
                Err(_) => {
                    failed = Some(index);
                    break;
                }
            }
        }
        if !started.contains(&true) {
            return failed.map_or(Ok(()), |index| Err((index, Error::Device)));
        }
        for index in (0..drives.len()).filter(|&index| started[index]) {
            let bytes = (lengths[index] - done[index]).min(ahci::MAX_SECTORS * 512);
            match finish(index, drives[index].unwrap(), done[index], bytes) {
                Ok(()) => done[index] += bytes,
                Err(_) => failed = failed.or(Some(index)),
            }
        }
        if let Some(index) = failed {
            return Err((index, Error::Device));
        }
    }
}

/// Writes whole sectors to `lba`. Sectors of a file the AoE target exports are refused (`aoetarget::guarded`): only the
/// export writes them while it lasts.
pub fn write(disk: usize, lba: u64, buffer: &[u8]) -> Result<(), Error> {
    unguarded(disk, lba, buffer)?;
    write_with(disk, lba, buffer, true)
}

/// Writes without flushing a SATA drive's cache (`flush` does); the other drivers ignore the difference.
pub fn write_deferred(disk: usize, lba: u64, buffer: &[u8]) -> Result<(), Error> {
    unguarded(disk, lba, buffer)?;
    write_with(disk, lba, buffer, false)
}

/// `write_deferred` for the AoE target itself, the one writer an exported file's sectors take.
pub fn write_exported(disk: usize, lba: u64, buffer: &[u8]) -> Result<(), Error> {
    write_with(disk, lba, buffer, false)
}

fn unguarded(disk: usize, lba: u64, buffer: &[u8]) -> Result<(), Error> {
    if crate::aoetarget::guarded(disk, lba, (buffer.len() / 512) as u64) { Err(Error::Device) } else { Ok(()) }
}

/// Makes the writes so far durable (SATA: FLUSH CACHE).
pub fn flush(disk: usize) -> Result<(), Error> {
    match checked(disk, 0, 0)?.backend {
        Backend::Ahci(drive) => drive.flush().map_err(|_| Error::Device),
        Backend::Md => crate::mdvol::flush(),
        Backend::Aoe(remote) => crate::aoedisk::flush(remote),
        _ => Ok(()),
    }
}

fn write_with(disk: usize, lba: u64, buffer: &[u8], flush: bool) -> Result<(), Error> {
    match checked(disk, lba, buffer.len())?.backend {
        Backend::Ahci(drive) => {
            for (index, chunk) in buffer.chunks(ahci::MAX_SECTORS * 512).enumerate() {
                let at = lba + (index * ahci::MAX_SECTORS) as u64;
                if flush { drive.write(at, chunk) } else { drive.write_deferred(at, chunk) }.map_err(|_| Error::Device)?;
            }
        }
        Backend::Nvme(drive) => {
            for (index, chunk) in buffer.chunks(nvme::MAX_SECTORS * 512).enumerate() {
                drive.write(lba + (index * nvme::MAX_SECTORS) as u64, chunk).map_err(|_| Error::Device)?;
            }
        }
        #[cfg(target_arch = "aarch64")]
        Backend::Virtio(drive) => with_direct_write(buffer, |buffer| drive.write(lba, buffer)).map_err(|_| Error::Device)?,
        #[cfg(target_arch = "aarch64")]
        Backend::Sd(drive) => with_direct_write(buffer, |buffer| drive.write(lba, buffer)).map_err(|_| Error::Device)?,
        Backend::Usb(device) => crate::usb::storage_write(device, lba, buffer).map_err(|_| Error::Device)?,
        Backend::Md => crate::mdvol::write(lba, buffer)?,
        Backend::Aoe(remote) => crate::aoedisk::write(remote, lba, buffer, flush)?,
    }
    Ok(())
}

/// TRIM: tells the disk these (start, sectors) ranges are free space. Contents read back afterwards are undefined.
/// SATA and NVMe SSDs only; `Unsupported` otherwise (USB mass storage for now).
pub fn discard(disk: usize, ranges: &[(u64, u64)]) -> Result<(), Error> {
    let info = get(disk).ok_or(Error::NoSuchDisk)?;
    if ranges.iter().any(|&(lba, sectors)| lba.checked_add(sectors).map_or(true, |end| end > info.sectors)) {
        return Err(Error::OutOfRange);
    }
    // A file system that frees an exported file's blocks may TRIM them; that would erase what the export serves.
    if ranges.iter().any(|&(lba, sectors)| crate::aoetarget::guarded(disk, lba, sectors)) {
        return Err(Error::Unsupported);
    }
    match info.backend {
        Backend::Ahci(drive) => drive.discard(ranges).map_err(|error| if error == ahci::Error::Unsupported { Error::Unsupported } else { Error::Device }),
        Backend::Nvme(drive) => drive.discard(ranges).map_err(|error| if error == nvme::Error::Unsupported { Error::Unsupported } else { Error::Device }),
        Backend::Usb(_) | Backend::Md | Backend::Aoe(_) => Err(Error::Unsupported),
        #[cfg(target_arch = "aarch64")]
        Backend::Virtio(_) => Err(Error::Unsupported),
        #[cfg(target_arch = "aarch64")]
        Backend::Sd(_) => Err(Error::Unsupported),
    }
}
