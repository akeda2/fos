//! The block-device layer: every disk the drivers found, by 512-byte sector. The same shape as the embedded kernel's
//! `block.rs` (SATA, NVMe and USB mass storage). Registration happens on the boot CPU
//! during start-up; reads and writes go straight to the drivers.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
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

/// One lock per disk for its SATA commands. A port's tag 0 (its table, the bounce buffer) and the driver's bookkeeping of
/// queued tags take one caller at a time: md's boot scan reads every disk while the service thread reads `settings` from
/// the boot disk, with no file lock between them. Taken innermost (after `tapefs::lock` and md's lock), several in
/// ascending disk order, and never across a call back into the block layer.
static PORTS: [crate::sched::SleepMutex<()>; MAX_DISKS] = [const { crate::sched::SleepMutex::new(()) }; MAX_DISKS];

/// The port locks of `disks` (indices into `DISKS`), taken in ascending order.
fn lock_ports(disks: impl Iterator<Item = usize>) -> alloc::vec::Vec<crate::sched::SleepGuard<'static, ()>> {
    let mut list: alloc::vec::Vec<usize> = disks.collect();
    list.sort_unstable();
    list.dedup();
    list.into_iter().map(|disk| PORTS[disk].lock()).collect()
}
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
        // the same commands `read_many` gives a range: straight into `buffer` where its pages allow it
        Backend::Ahci(_) => return read_group(&mut [Read { disk, lba, buffer }]).map_err(|(_, error)| error),
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

/// Reads several ranges, overlapping those on different SATA drives (`overlap`): each round starts one command on every
/// AHCI drive that still has sectors to read, then collects them all. A command reads straight into the range's buffer
/// (up to 4 MiB) when its pages allow it (`segments`), else 128 KiB through the drive's bounce buffer. A drive is a port
/// with one command slot here, so two ranges on one drive take turns. Other disks are read one after another, as `read` does.
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
    // The buffers' addresses, taken once: `finish` borrows the buffers mutably to copy out of the bounce buffer.
    let mut addresses = [0u64; MANY];
    for (index, request) in reads.iter().enumerate() {
        addresses[index] = request.buffer.as_ptr() as u64;
    }
    let _ports = lock_ports(reads.iter().zip(&drives).filter(|(_, drive)| drive.is_some()).map(|(request, _)| request.disk));
    overlap(
        &drives[..reads.len()],
        &lengths,
        |index, drive, done, left| {
            let at = lbas[index] + (done / 512) as u64;
            timed(&MANY_START, || start_command(drive, at, addresses[index] + done as u64, left, false))
        },
        |index, drive, done, command| {
            timed(&MANY_WAIT, || drive.wait())?;
            if !command.direct {
                timed(&MANY_COPY, || drive.copy_out(&mut reads[index].buffer[done..done + command.bytes]));
            }
            counted(command);
            Ok(())
        },
    )
}

/// One command `overlap` started: its byte count, and whether it moves the caller's memory itself (no bounce copy).
#[derive(Clone, Copy)]
struct Command {
    bytes: usize,
    direct: bool,
}

/// Starts the next command of a range: straight from or into the memory at `address` (up to the drive's direct limit) when
/// `segments` can describe it, else through the bounce buffer (a write is staged there first). `left` bytes remain.
fn start_command(drive: ahci::Drive, lba: u64, address: u64, left: usize, write: bool) -> Result<Command, ahci::Error> {
    let bytes = left.min(drive.max_direct_sectors() * 512);
    let mut list = [ahci::Segment { physical: 0, bytes: 0 }; ahci::MAX_SEGMENTS];
    if let Some(count) = segments(address, bytes, !write, &mut list) {
        match drive.start_direct(lba, &list[..count], write) {
            Ok(()) => return Ok(Command { bytes, direct: true }),
            Err(ahci::Error::Unsupported) => {}
            Err(error) => return Err(error),
        }
    }
    let bytes = left.min(ahci::MAX_SECTORS * 512);
    if write {
        let data = unsafe { core::slice::from_raw_parts(address as *const u8, bytes) };
        timed(&MANY_COPY, || drive.stage(data))?;
        drive.start_staged_write(lba, bytes)?;
    } else {
        drive.start_read(lba, bytes)?;
    }
    Ok(Command { bytes, direct: false })
}

/// The physical pieces of the `bytes` at kernel address `address`, found page by page in the kernel's own tables and
/// merged where they run on (a heap or `allocate_contiguous` buffer is one piece). None when a page is not mapped there
/// (a tape's memory: it is only in the tape's own tables), not writable for a read into it (`writable`), the address is
/// odd, or it takes more than `ahci::MAX_SEGMENTS` pieces: the caller then goes through the bounce buffer.
#[cfg(target_arch = "x86_64")]
fn segments(address: u64, bytes: usize, writable: bool, list: &mut [ahci::Segment; ahci::MAX_SEGMENTS]) -> Option<usize> {
    const PAGE: u64 = 4096;
    const MAX_PIECE: u64 = 4 << 20;
    if address % 2 != 0 || bytes == 0 {
        return None;
    }
    let (mut at, end, mut count) = (address, address.checked_add(bytes as u64)?, 0usize);
    while at < end {
        let take = ((at / PAGE + 1) * PAGE).min(end) - at;
        let mapping = crate::arch::mmu::translate(at)?;
        if writable && !mapping.writable {
            return None;
        }
        match count.checked_sub(1).map(|last| &mut list[last]) {
            Some(last) if last.physical + last.bytes == mapping.physical && last.bytes + take <= MAX_PIECE => last.bytes += take,
            _ if count == ahci::MAX_SEGMENTS => return None,
            _ => {
                list[count] = ahci::Segment { physical: mapping.physical, bytes: take };
                count += 1;
            }
        }
        at += take;
    }
    Some(count)
}

/// Only the x86 server kernel has SATA (and the page walk); elsewhere everything goes through the bounce buffer.
#[cfg(not(target_arch = "x86_64"))]
fn segments(_: u64, _: usize, _: bool, _: &mut [ahci::Segment; ahci::MAX_SEGMENTS]) -> Option<usize> {
    None
}

/// The tags after 0 of each disk that a read-ahead holds (bit n: tag n). Tag 0 is the one the plain calls use.
static AHEAD_TAGS: [AtomicU32; MAX_DISKS] = [const { AtomicU32::new(0) }; MAX_DISKS];

fn claim_tag(disk: usize, depth: usize) -> Option<usize> {
    (1..depth.min(32)).find(|&tag| AHEAD_TAGS[disk].fetch_or(1 << tag, Ordering::AcqRel) & (1 << tag) == 0)
}

fn release_tag(disk: usize, tag: usize) {
    AHEAD_TAGS[disk].fetch_and(!(1 << tag), Ordering::Release);
}

/// Reads `start_ahead` began, and how far each got.
pub struct Ahead {
    ranges: [Option<AheadRange>; MANY],
    count: usize,
}

#[derive(Clone, Copy)]
struct AheadRange {
    disk: usize,
    lba: u64,
    address: u64,
    bytes: usize,
    /// The drive, tag and command while the read is queued there; None once collected, or when it was not queued.
    queued: Option<(ahci::Drive, usize, Command)>,
}

/// Starts reading `reads` ahead of time, for `Ahead::finish` to collect later: each range that a SATA drive can take as
/// one queued command (NCQ, a free tag, its pages in the kernel's tables, at most the drive's direct limit) goes on the
/// drive now, beside whatever else it has queued; the rest are read when `finish` is called. An md resync starts the
/// next piece this way before it waits for the current one, so every member always has a command to work on.
///
/// # Safety
/// The buffers must stay allocated and untouched until `finish` returns or the `Ahead` is dropped (dropping it waits for
/// the queued reads): the drives write into them until then.
pub unsafe fn start_ahead(reads: &mut [Read<'_>]) -> Ahead {
    let mut ahead = Ahead { ranges: [None; MANY], count: reads.len().min(MANY) };
    for (index, request) in reads.iter_mut().take(MANY).enumerate() {
        let (address, bytes) = (request.buffer.as_mut_ptr() as u64, request.buffer.len());
        let mut range = AheadRange { disk: request.disk, lba: request.lba, address, bytes, queued: None };
        if let Ok(Disk { backend: Backend::Ahci(drive), .. }) = checked(request.disk, request.lba, bytes) {
            range.queued = queue_read(request.disk, drive, request.lba, address, bytes);
        }
        ahead.ranges[index] = Some(range);
    }
    ahead
}

/// One queued read straight into the memory at `address`, on a tag of its own; None when the drive cannot take it so.
fn queue_read(disk: usize, drive: ahci::Drive, lba: u64, address: u64, bytes: usize) -> Option<(ahci::Drive, usize, Command)> {
    if drive.queue_depth() < 2 || bytes == 0 || bytes > drive.max_direct_sectors() * 512 {
        return None;
    }
    let mut list = [ahci::Segment { physical: 0, bytes: 0 }; ahci::MAX_SEGMENTS];
    let count = segments(address, bytes, true, &mut list)?;
    let tag = claim_tag(disk, drive.queue_depth())?;
    let _port = PORTS[disk].lock();
    match timed(&MANY_START, || drive.start_queued(tag, lba, &list[..count], false)) {
        Ok(()) => Some((drive, tag, Command { bytes, direct: true })),
        Err(_) => {
            release_tag(disk, tag);
            None
        }
    }
}

impl Ahead {
    /// Whether `reads` are the very ranges and buffers this was started on.
    pub fn matches(&self, reads: &[Read<'_>]) -> bool {
        reads.len() == self.count
            && reads.iter().zip(&self.ranges).all(|(request, range)| {
                range.is_some_and(|range| {
                    (range.disk, range.lba, range.address, range.bytes)
                        == (request.disk, request.lba, request.buffer.as_ptr() as u64, request.buffer.len())
                })
            })
    }

    /// Collects the reads into `reads` (the ranges `start_ahead` was given, `matches`): waits for the queued ones and
    /// reads the rest now, as `read_many` would. On failure: the index of a range that failed, once every queued read
    /// has been waited for.
    pub fn finish(mut self, reads: &mut [Read<'_>]) -> Result<(), (usize, Error)> {
        if !self.matches(reads) {
            drop(self);
            return read_many(reads);
        }
        let mut failed = None;
        let mut queued = [false; MANY];
        for index in 0..self.count {
            let Some(range) = self.ranges[index].as_mut() else { continue };
            let Some((drive, tag, command)) = range.queued.take() else { continue };
            queued[index] = true;
            let result = timed(&MANY_WAIT, || {
                let _port = PORTS[range.disk].lock();
                drive.wait_queued(tag)
            });
            release_tag(range.disk, tag);
            match result {
                Ok(()) => {
                    counted(command);
                    MANY_AHEAD.fetch_add(1, Ordering::Relaxed);
                }
                Err(_) => failed = failed.or(Some(index)),
            }
        }
        if let Some(index) = failed {
            return Err((index, Error::Device));
        }
        let mut which = [0usize; MANY];
        let mut rest = alloc::vec::Vec::new();
        for (index, request) in reads.iter_mut().enumerate().filter(|(index, _)| !queued[*index]) {
            which[rest.len()] = index;
            rest.push(Read { disk: request.disk, lba: request.lba, buffer: &mut *request.buffer });
        }
        read_many(&mut rest).map_err(|(index, error)| (which[index], error))
    }
}

impl Drop for Ahead {
    /// Waits for what is still queued (the buffers must not be given back while a drive writes into them).
    fn drop(&mut self) {
        for range in self.ranges.iter_mut().flatten() {
            if let Some((drive, tag, _)) = range.queued.take() {
                let _port = PORTS[range.disk].lock();
                let _ = drive.wait_queued(tag);
                release_tag(range.disk, tag);
            }
        }
    }
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
        write_group(group, true).map_err(|(index, error)| (first + index, error))?;
        first += MANY;
    }
    Ok(())
}

/// `guard`: refuse sectors an AoE export of a file owns (false only for `write_with`, whose callers checked or are the export).
fn write_group(writes: &[Write<'_>], guard: bool) -> Result<(), (usize, Error)> {
    let mut drives: [Option<ahci::Drive>; MANY] = [None; MANY];
    let mut lengths = [0usize; MANY];
    for (index, request) in writes.iter().enumerate() {
        if guard {
            unguarded(request.disk, request.lba, request.buffer).map_err(|error| (index, error))?;
        }
        match checked(request.disk, request.lba, request.buffer.len()).map_err(|error| (index, error))?.backend {
            Backend::Ahci(drive) => drives[index] = Some(drive),
            _ => write_with(request.disk, request.lba, request.buffer, false).map_err(|error| (index, error))?,
        }
        lengths[index] = request.buffer.len();
    }
    let _ports = lock_ports(writes.iter().zip(&drives).filter(|(_, drive)| drive.is_some()).map(|(request, _)| request.disk));
    overlap(
        &drives[..writes.len()],
        &lengths,
        |index, drive, done, left| {
            let (at, address) = (writes[index].lba + (done / 512) as u64, writes[index].buffer.as_ptr() as u64 + done as u64);
            // `start_command` times a bounce copy as copying itself; the rest is starting
            let begin = crate::arch::micros();
            let copied = MANY_COPY.load(Ordering::Relaxed);
            let command = start_command(drive, at, address, left, true);
            let spent = crate::arch::micros().saturating_sub(begin);
            MANY_START.fetch_add(spent.saturating_sub(MANY_COPY.load(Ordering::Relaxed) - copied), Ordering::Relaxed);
            command
        },
        |_, drive, _, command| {
            timed(&MANY_WAIT, || drive.wait())?;
            counted(command);
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
    /// Of those, the ones that moved the caller's memory itself (no bounce copy).
    pub direct: u64,
    pub bytes: u64,
    /// Of those, reads `start_ahead` queued before they were waited for.
    pub ahead: u64,
}

impl ManyTimes {
    /// What happened between `earlier` and `self`.
    pub fn since(&self, earlier: &ManyTimes) -> ManyTimes {
        ManyTimes {
            start_us: self.start_us - earlier.start_us,
            wait_us: self.wait_us - earlier.wait_us,
            copy_us: self.copy_us - earlier.copy_us,
            commands: self.commands - earlier.commands,
            direct: self.direct - earlier.direct,
            bytes: self.bytes - earlier.bytes,
            ahead: self.ahead - earlier.ahead,
        }
    }
}

static MANY_START: AtomicU64 = AtomicU64::new(0);
static MANY_WAIT: AtomicU64 = AtomicU64::new(0);
static MANY_COPY: AtomicU64 = AtomicU64::new(0);
static MANY_COMMANDS: AtomicU64 = AtomicU64::new(0);
static MANY_DIRECT: AtomicU64 = AtomicU64::new(0);
static MANY_BYTES: AtomicU64 = AtomicU64::new(0);
static MANY_AHEAD: AtomicU64 = AtomicU64::new(0);

pub fn many_times() -> ManyTimes {
    ManyTimes {
        start_us: MANY_START.load(Ordering::Relaxed),
        wait_us: MANY_WAIT.load(Ordering::Relaxed),
        copy_us: MANY_COPY.load(Ordering::Relaxed),
        commands: MANY_COMMANDS.load(Ordering::Relaxed),
        direct: MANY_DIRECT.load(Ordering::Relaxed),
        bytes: MANY_BYTES.load(Ordering::Relaxed),
        ahead: MANY_AHEAD.load(Ordering::Relaxed),
    }
}

/// Runs `work`, adding its wall time to `counter` (the TSC clock: a few nanoseconds a reading).
fn timed<R>(counter: &AtomicU64, work: impl FnOnce() -> R) -> R {
    let begin = crate::arch::micros();
    let result = work();
    counter.fetch_add(crate::arch::micros().saturating_sub(begin), Ordering::Relaxed);
    result
}

fn counted(command: Command) {
    MANY_COMMANDS.fetch_add(1, Ordering::Relaxed);
    MANY_DIRECT.fetch_add(u64::from(command.direct), Ordering::Relaxed);
    MANY_BYTES.fetch_add(command.bytes as u64, Ordering::Relaxed);
}

/// The rounds `read_many` and `write_many` share. Each round starts one command on every AHCI drive (`drives[i]`, None
/// for a range already done another way) that still has bytes of its `lengths[i]` to move, skipping a drive that already
/// has one this round, then finishes them all in order. `start(i, drive, done, left)` issues the next command of range
/// `i` (from byte `done`, `left` bytes to go) and says how much it covers; `finish(i, drive, done, command)` waits for it.
/// Every started command is finished before an error is returned, so no drive is left busy.
fn overlap(
    drives: &[Option<ahci::Drive>],
    lengths: &[usize],
    mut start: impl FnMut(usize, ahci::Drive, usize, usize) -> Result<Command, ahci::Error>,
    mut finish: impl FnMut(usize, ahci::Drive, usize, Command) -> Result<(), ahci::Error>,
) -> Result<(), (usize, Error)> {
    let mut done = [0usize; MANY];
    loop {
        let mut started: [Option<Command>; MANY] = [None; MANY];
        let mut failed = None;
        for (index, drive) in drives.iter().enumerate() {
            let Some(drive) = *drive else { continue };
            let left = lengths[index] - done[index];
            let busy = (0..index).any(|other| started[other].is_some() && drives[other].is_some_and(|d| d.same_port(&drive)));
            if left == 0 || busy {
                continue;
            }
            match start(index, drive, done[index], left) {
                Ok(command) => started[index] = Some(command),
                Err(_) => {
                    failed = Some(index);
                    break;
                }
            }
        }
        if started.iter().all(Option::is_none) {
            return failed.map_or(Ok(()), |index| Err((index, Error::Device)));
        }
        for index in 0..drives.len() {
            let Some(command) = started[index] else { continue };
            match finish(index, drives[index].unwrap(), done[index], command) {
                Ok(()) => done[index] += command.bytes,
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
        Backend::Ahci(drive) => {
            let _port = PORTS[disk].lock();
            drive.flush().map_err(|_| Error::Device)
        }
        Backend::Md => crate::mdvol::flush(),
        Backend::Aoe(remote) => crate::aoedisk::flush(remote),
        _ => Ok(()),
    }
}

fn write_with(disk: usize, lba: u64, buffer: &[u8], flush: bool) -> Result<(), Error> {
    match checked(disk, lba, buffer.len())?.backend {
        // the same commands `write_many` gives a range (straight from `buffer` where its pages allow it), then the flush
        Backend::Ahci(drive) => {
            write_group(&[Write { disk, lba, buffer }], false).map_err(|(_, error)| error)?;
            if flush {
                let _port = PORTS[disk].lock();
                drive.flush().map_err(|_| Error::Device)?;
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
        Backend::Ahci(drive) => {
            let _port = PORTS[disk].lock();
            drive.discard(ranges).map_err(|error| if error == ahci::Error::Unsupported { Error::Unsupported } else { Error::Device })
        }
        Backend::Nvme(drive) => drive.discard(ranges).map_err(|error| if error == nvme::Error::Unsupported { Error::Unsupported } else { Error::Device }),
        Backend::Usb(_) | Backend::Md | Backend::Aoe(_) => Err(Error::Unsupported),
        #[cfg(target_arch = "aarch64")]
        Backend::Virtio(_) => Err(Error::Unsupported),
        #[cfg(target_arch = "aarch64")]
        Backend::Sd(_) => Err(Error::Unsupported),
    }
}
