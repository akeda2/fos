//! A minimal AHCI (SATA) host bus adapter driver: find SATA disks, read their identity
//! and move sectors with DMA. Polling only, one command at a time per drive (commands on
//! different drives may overlap: `start_read`, `wait`, `copy_out`; `stage`, `start_staged_write`,
//! `wait`; `start_direct`), 512-byte sectors. Data goes through a per-drive bounce buffer,
//! or with `start_direct` straight to and from the caller's memory, described by physical
//! segments (the server kernel looks them up in its page tables).
//!
//! AHCI in one paragraph. The HBA exposes up to 32 *ports*, each with a register block;
//! a SATA drive on a port is talked to through structures in RAM the port is pointed at:
//! a *command list* (32 headers, one per command slot), a *received-FIS area* where the
//! drive's replies land, and for each command a *command table* holding the 20-byte
//! Register H2D FIS (the ATA command and its LBA/count) and a *PRDT*, the scatter/gather
//! list of the data buffers. Software fills slot 0's header and table, sets bit 0 of the
//! port's `CI` register, and the HBA runs the command; the bit clears when it is done
//! and the task-file register says whether it worked.
//!
//! The firmware's own AHCI driver may have left ports running when it handed over, so
//! each port is stopped before it is pointed at fresh structures. The HBA is not reset:
//! other ports may be in use.

use crate::{memory, paging, pci, timer};

pub const MAX_CONTROLLERS: usize = 2;
pub const MAX_DRIVES: usize = 8;
/// Most sectors one `read`/`write` call moves (the bounce buffer's size).
/// Sectors per command: 128 KiB. Each command costs a fixed latency (about 0.7 ms measured on a SATA SSD with polling), so 8 KiB
/// commands capped a read at ~11 MB/s; 28-bit READ DMA takes at most 256 (count 0 means 256).
pub const MAX_SECTORS: usize = 256;
/// Most sectors one `start_direct` command moves on an LBA48 drive (4 MiB; the 16-bit count allows 32 MiB, but a bigger
/// command only lengthens the wait before a failure is seen). A drive without LBA48 takes `MAX_SECTORS`.
pub const MAX_DIRECT_SECTORS: usize = 8192;
/// Most physical segments one `start_direct` command takes: the PRDT entries that fit after the command FIS in the
/// command table's half of the structures frame.
pub const MAX_SEGMENTS: usize = (0x1000 - OFFSET_COMMAND_TABLE - 0x80) / 16;
/// The largest byte count of one PRDT entry (its 22-bit DBC field is the count minus one).
const PRD_MAX: u64 = 4 << 20;
const TIMEOUT_TICKS: u64 = 500;

const HBA_CAP: usize = 0x00;
/// CAP.S64A: the HBA takes 64-bit addresses in PRDT entries and command headers.
const CAP_S64A: u32 = 1 << 31;
const HBA_GHC: usize = 0x04;
const HBA_IS: usize = 0x08;
const HBA_PI: usize = 0x0c;
const HBA_CAP2: usize = 0x24;
const HBA_BOHC: usize = 0x28;
const PORT_BASE: usize = 0x100;
const PORT_STRIDE: usize = 0x80;
const GHC_AHCI_ENABLE: u32 = 1 << 31;

const P_CLB: usize = 0x00;
const P_CLBU: usize = 0x04;
const P_FB: usize = 0x08;
const P_FBU: usize = 0x0c;
const P_IS: usize = 0x10;
const P_CMD: usize = 0x18;
const P_TFD: usize = 0x20;
const P_SIG: usize = 0x24;
const P_SSTS: usize = 0x28;
const P_SERR: usize = 0x30;
const P_CI: usize = 0x38;

const CMD_START: u32 = 1 << 0;
const CMD_FIS_RECEIVE: u32 = 1 << 4;
const CMD_FIS_RUNNING: u32 = 1 << 14;
const CMD_LIST_RUNNING: u32 = 1 << 15;
const TFD_ERROR: u32 = 1 << 0;
const TFD_DRQ: u32 = 1 << 3;
const TFD_BUSY: u32 = 1 << 7;
const IS_TASK_FILE_ERROR: u32 = 1 << 30;
const SIGNATURE_ATA: u32 = 0x0000_0101;

const ATA_IDENTIFY: u8 = 0xec;
const ATA_READ_DMA: u8 = 0xc8;
const ATA_READ_DMA_EXT: u8 = 0x25;
const ATA_WRITE_DMA: u8 = 0xca;
const ATA_WRITE_DMA_EXT: u8 = 0x35;
const ATA_DATA_SET_MANAGEMENT: u8 = 0x06;
const ATA_FLUSH_CACHE: u8 = 0xe7;
const ATA_FLUSH_CACHE_EXT: u8 = 0xea;

// Layout of what each drive gets: one frame of structures, then the bounce buffer (`FRAMES` contiguous frames in all).
const OFFSET_COMMAND_LIST: usize = 0x000; // 1 KiB aligned: 32 headers of 32 bytes
const OFFSET_RECEIVED_FIS: usize = 0x400; // 256-byte aligned
const OFFSET_COMMAND_TABLE: usize = 0x800; // 128-byte aligned: CFIS, then the PRDT at +0x80
const OFFSET_BOUNCE: usize = 0x1000; // MAX_SECTORS * 512 = 128 KiB
const FRAMES: usize = 1 + MAX_SECTORS * 512 / 4096;

/// A physically contiguous piece of the caller's memory for `start_direct`: an even address and an even byte count (AHCI
/// 1.3.1, 4.2.3.3: DBA bit 0 is reserved, DBC must say an even count).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segment {
    pub physical: u64,
    pub bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Timeout,
    Device { status: u8, error: u8 },
    Unsupported,
}

fn read32(address: usize) -> u32 {
    unsafe { (memory::phys_to_virt(address as u64) as *const u32).read_volatile() }
}
fn write32(address: usize, value: u32) {
    unsafe { (memory::phys_to_virt(address as u64) as *mut u32).write_volatile(value) }
}

fn wait_until(ticks: u64, mut condition: impl FnMut() -> bool) -> bool {
    let start = timer::ticks();
    loop {
        crate::debug::pet();
        if condition() {
            return true;
        }
        if timer::ticks().wrapping_sub(start) >= ticks {
            return condition();
        }
        core::hint::spin_loop();
    }
}

/// One SATA drive on one port.
#[derive(Clone, Copy)]
pub struct Drive {
    registers: usize,
    frame: usize,
    pub sectors: u64,
    lba48: bool,
    /// CAP.S64A of its HBA: PRDT entries may point above 4 GiB.
    dma64: bool,
    /// IDENTIFY word 169 bit 0: DATA SET MANAGEMENT with the TRIM feature.
    can_trim: bool,
    pub model: [u8; 40],
}

impl Drive {
    /// Runs one ATA DMA command through command slot 0 and waits for it.
    fn command(&self, command: u8, lba: u64, count: u32, write: bool, bytes: u32) -> Result<(), Error> {
        self.command_with(command, 0, lba, count, write, bytes)
    }

    /// `command` with a feature byte (DATA SET MANAGEMENT's TRIM is feature 1).
    fn command_with(&self, command: u8, feature: u8, lba: u64, count: u32, write: bool, bytes: u32) -> Result<(), Error> {
        self.issue_bounce(command, feature, lba, count, write, bytes)?;
        self.complete()
    }

    /// `issue` with the bounce buffer as the one data segment (none when `bytes` is 0).
    fn issue_bounce(&self, command: u8, feature: u8, lba: u64, count: u32, write: bool, bytes: u32) -> Result<(), Error> {
        let bounce = [Segment { physical: (self.frame + OFFSET_BOUNCE) as u64, bytes: u64::from(bytes) }];
        self.issue(command, feature, lba, count, write, if bytes == 0 { &[] } else { &bounce })
    }

    /// Fills slot 0's header and table and sets its bit in `CI`: the HBA runs the command from here on, and `complete`
    /// waits for it. Every port has its own command list, table and bounce buffer, so commands on different drives may be
    /// in flight at once (`start_read`); one drive takes one command at a time.
    fn issue(&self, command: u8, feature: u8, lba: u64, count: u32, write: bool, segments: &[Segment]) -> Result<(), Error> {
        let list = self.frame + OFFSET_COMMAND_LIST;
        let table = self.frame + OFFSET_COMMAND_TABLE;
        unsafe {
            core::ptr::write_bytes(memory::phys_to_virt(table as u64) as *mut u8, 0, 0x80 + 16 * segments.len());
            // 28-bit commands carry LBA bits 27:24 in the device register; 48-bit ones
            // carry bits 47:24 in the second LBA byte group.
            let ext = matches!(command, ATA_READ_DMA_EXT | ATA_WRITE_DMA_EXT | ATA_FLUSH_CACHE_EXT | ATA_DATA_SET_MANAGEMENT);
            let fis = [
                0x27u8, // FIS type: Register Host-to-Device
                0x80,   // bit 7: this FIS carries a command
                command,
                feature,
                lba as u8,
                (lba >> 8) as u8,
                (lba >> 16) as u8,
                0x40 | if ext { 0 } else { ((lba >> 24) & 0xf) as u8 }, // device: LBA mode
                if ext { (lba >> 24) as u8 } else { 0 },
                if ext { (lba >> 32) as u8 } else { 0 },
                if ext { (lba >> 40) as u8 } else { 0 },
                0,
                count as u8,
                (count >> 8) as u8,
            ];
            core::ptr::copy_nonoverlapping(fis.as_ptr(), memory::phys_to_virt(table as u64) as *mut u8, fis.len());
            // One PRDT entry per segment: base address (low, high), reserved, DBC = byte count minus one.
            for (index, segment) in segments.iter().enumerate() {
                let entry = memory::phys_to_virt((table + 0x80 + 16 * index) as u64) as *mut u32;
                entry.write_volatile(segment.physical as u32);
                entry.add(1).write_volatile((segment.physical >> 32) as u32);
                entry.add(2).write_volatile(0);
                entry.add(3).write_volatile(segment.bytes as u32 - 1);
            }
            let header = memory::phys_to_virt(list as u64) as *mut u32;
            header.write_volatile(5 | (u32::from(write) << 6) | ((segments.len() as u32) << 16)); // 5 FIS dwords, direction, PRDT length
            header.add(1).write_volatile(0);
            header.add(2).write_volatile(table as u32);
            header.add(3).write_volatile(0);
        }
        if !wait_until(TIMEOUT_TICKS, || read32(self.registers + P_TFD) & (TFD_BUSY | TFD_DRQ) == 0) {
            return self.recover(Error::Timeout);
        }
        write32(self.registers + P_IS, 0xffff_ffff);
        // The data a write sends (bounce buffer or the caller's memory) is written with ordinary stores: they must all be
        // done before the doorbell, and the compiler may not move them past it.
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        write32(self.registers + P_CI, 1);
        Ok(())
    }

    /// Waits for the command `issue` started: `CI` bit 0 clears when it is done, a task-file error ends it early.
    fn complete(&self) -> Result<(), Error> {
        let mut failed = false;
        let finished = wait_until(TIMEOUT_TICKS, || {
            if read32(self.registers + P_CI) & 1 == 0 {
                return true;
            }
            if read32(self.registers + P_IS) & IS_TASK_FILE_ERROR != 0 {
                failed = true;
                return true;
            }
            false
        });
        let task_file = read32(self.registers + P_TFD);
        if !finished {
            return self.recover(Error::Timeout);
        }
        if failed || task_file & TFD_ERROR != 0 {
            return self.recover(Error::Device { status: task_file as u8, error: (task_file >> 8) as u8 });
        }
        // What the device wrote by DMA is read with ordinary loads after this: none may be done before the command was.
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    /// After a failed command: stop the command engine, clear the error state, start it
    /// again so the next command begins clean.
    fn recover(&self, error: Error) -> Result<(), Error> {
        write32(self.registers + P_CMD, read32(self.registers + P_CMD) & !CMD_START);
        wait_until(100, || read32(self.registers + P_CMD) & CMD_LIST_RUNNING == 0);
        write32(self.registers + P_SERR, 0xffff_ffff);
        write32(self.registers + P_IS, 0xffff_ffff);
        wait_until(100, || read32(self.registers + P_TFD) & (TFD_BUSY | TFD_DRQ) == 0);
        write32(self.registers + P_CMD, read32(self.registers + P_CMD) | CMD_START);
        Err(error)
    }

    fn check(&self, lba: u64, bytes: usize) -> Result<u32, Error> {
        if bytes == 0 || bytes % 512 != 0 || bytes / 512 > MAX_SECTORS {
            return Err(Error::Unsupported);
        }
        let sectors = (bytes / 512) as u32;
        if lba + u64::from(sectors) > self.sectors || (!self.lba48 && lba + u64::from(sectors) > 1 << 28) {
            return Err(Error::Unsupported);
        }
        Ok(sectors)
    }

    /// Reads `buffer.len() / 512` sectors (at most `MAX_SECTORS`) starting at `lba`.
    pub fn read(&self, lba: u64, buffer: &mut [u8]) -> Result<(), Error> {
        let sectors = self.check(lba, buffer.len())?;
        self.command(if self.lba48 { ATA_READ_DMA_EXT } else { ATA_READ_DMA }, lba, sectors, false, buffer.len() as u32)?;
        unsafe { core::ptr::copy_nonoverlapping(memory::phys_to_virt((self.frame + OFFSET_BOUNCE) as u64) as *const u8, buffer.as_mut_ptr(), buffer.len()) };
        Ok(())
    }

    /// `read` in steps, so reads on several drives overlap (`block::read_many` on the server kernel): this one starts a
    /// read of `bytes` (at most `MAX_SECTORS` sectors) at `lba` and returns while the drive works; `wait` waits for it and
    /// `copy_out` takes it from the bounce buffer. Nothing else may use this drive in between (its one command slot and
    /// bounce buffer are busy). The steps are separate calls so the caller can time each (`md`'s job report).
    pub fn start_read(&self, lba: u64, bytes: usize) -> Result<(), Error> {
        let sectors = self.check(lba, bytes)?;
        self.issue_bounce(if self.lba48 { ATA_READ_DMA_EXT } else { ATA_READ_DMA }, 0, lba, sectors, false, bytes as u32)
    }

    /// Starts a read (`write` false) or write of the sectors at `lba` straight from or into the memory `segments` describe,
    /// in order, without the bounce buffer; `wait` finishes it. Their bytes add up to whole sectors, at most
    /// `max_direct_sectors`, in at most `MAX_SEGMENTS` pieces of up to 4 MiB each, every address and count even, all
    /// below 4 GiB unless the HBA takes 64-bit addresses. Anything else is `Unsupported` and nothing is started: the caller
    /// goes through the bounce buffer instead. The memory must stay put until `wait` returns.
    pub fn start_direct(&self, lba: u64, segments: &[Segment], write: bool) -> Result<(), Error> {
        let bytes: u64 = segments.iter().map(|segment| segment.bytes).sum();
        let fits = |segment: &Segment| {
            segment.bytes != 0
                && segment.bytes <= PRD_MAX
                && segment.physical % 2 == 0
                && segment.bytes % 2 == 0
                && (self.dma64 || segment.physical + segment.bytes <= 1 << 32)
        };
        if segments.is_empty() || segments.len() > MAX_SEGMENTS || !segments.iter().all(fits) || bytes % 512 != 0 {
            return Err(Error::Unsupported);
        }
        let sectors = bytes / 512;
        if sectors > self.max_direct_sectors() as u64 || lba + sectors > self.sectors || (!self.lba48 && lba + sectors > 1 << 28) {
            return Err(Error::Unsupported);
        }
        let command = match (write, self.lba48) {
            (false, true) => ATA_READ_DMA_EXT,
            (false, false) => ATA_READ_DMA,
            (true, true) => ATA_WRITE_DMA_EXT,
            (true, false) => ATA_WRITE_DMA,
        };
        // A count of 0 in the FIS means 256 sectors (28-bit) or 65536 (48-bit); `max_direct_sectors` stays below both.
        self.issue(command, 0, lba, sectors as u32, write, segments)
    }

    /// Most sectors `start_direct` takes in one command.
    pub fn max_direct_sectors(&self) -> usize {
        if self.lba48 { MAX_DIRECT_SECTORS } else { MAX_SECTORS }
    }

    /// Waits for the command `start_read`, `start_staged_write` or `start_direct` began.
    pub fn wait(&self) -> Result<(), Error> {
        self.complete()
    }

    /// After `wait` for a read: its first `buffer.len()` bytes (the count `start_read` was given) from the bounce buffer.
    pub fn copy_out(&self, buffer: &mut [u8]) {
        let length = buffer.len().min(MAX_SECTORS * 512);
        unsafe { core::ptr::copy_nonoverlapping(memory::phys_to_virt((self.frame + OFFSET_BOUNCE) as u64) as *const u8, buffer.as_mut_ptr(), length) };
    }

    /// `write_deferred` in steps, like `start_read`: `stage` copies the data into this drive's bounce buffer (at most
    /// `MAX_SECTORS` sectors), `start_staged_write` starts the command on the same byte count, `wait` waits for it. No cache
    /// flush (`flush` makes it durable).
    pub fn stage(&self, buffer: &[u8]) -> Result<(), Error> {
        if buffer.len() > MAX_SECTORS * 512 {
            return Err(Error::Unsupported);
        }
        unsafe { core::ptr::copy_nonoverlapping(buffer.as_ptr(), memory::phys_to_virt((self.frame + OFFSET_BOUNCE) as u64) as *mut u8, buffer.len()) };
        Ok(())
    }

    /// Starts writing the `bytes` that `stage` put in the bounce buffer to `lba`.
    pub fn start_staged_write(&self, lba: u64, bytes: usize) -> Result<(), Error> {
        let sectors = self.check(lba, bytes)?;
        self.issue_bounce(if self.lba48 { ATA_WRITE_DMA_EXT } else { ATA_WRITE_DMA }, 0, lba, sectors, true, bytes as u32)
    }

    /// Whether `other` is this same drive (port): two commands on one port cannot overlap.
    pub fn same_port(&self, other: &Drive) -> bool {
        self.registers == other.registers
    }

    /// TRIM: DATA SET MANAGEMENT (feature 1) over `ranges` (start, sectors). The payload is 8-byte entries, a 48-bit LBA
    /// and a 16-bit sector count, 64 to a sector; up to `MAX_SECTORS` payload sectors go in one command. What is read from
    /// a trimmed range afterwards is undefined. `Unsupported` unless IDENTIFY says the drive does TRIM (and LBA48).
    pub fn discard(&self, ranges: &[(u64, u64)]) -> Result<(), Error> {
        if !self.can_trim || !self.lba48 {
            return Err(Error::Unsupported);
        }
        const PER_COMMAND: usize = MAX_SECTORS * 64;
        let payload = memory::phys_to_virt((self.frame + OFFSET_BOUNCE) as u64) as *mut u64;
        let mut used = 0;
        let send = |used: usize| -> Result<(), Error> {
            let sectors = used.div_ceil(64);
            // The last payload sector is padded with zero entries, which the drive ignores.
            unsafe { core::ptr::write_bytes(payload.add(used), 0, sectors * 64 - used) };
            self.command_with(ATA_DATA_SET_MANAGEMENT, 1, 0, sectors as u32, true, sectors as u32 * 512)
        };
        for &(start, count) in ranges {
            let (mut lba, end) = (start, start + count);
            while lba < end {
                let length = (end - lba).min(0xffff);
                unsafe { payload.add(used).write_volatile(lba | length << 48) };
                used += 1;
                lba += length;
                if used == PER_COMMAND {
                    send(used)?;
                    used = 0;
                }
            }
        }
        if used > 0 {
            send(used)?;
        }
        Ok(())
    }

    /// Writes sectors, then flushes the drive's write cache so a completed write is on
    /// the platters.
    pub fn write(&self, lba: u64, buffer: &[u8]) -> Result<(), Error> {
        self.write_deferred(lba, buffer)?;
        self.flush()
    }

    /// Writes sectors without the cache flush: the data may sit in the drive's cache until `flush`. A flush costs
    /// milliseconds on a SATA SSD, so a stream of file data is written this way and flushed once before its metadata commits.
    pub fn write_deferred(&self, lba: u64, buffer: &[u8]) -> Result<(), Error> {
        let sectors = self.check(lba, buffer.len())?;
        unsafe { core::ptr::copy_nonoverlapping(buffer.as_ptr(), memory::phys_to_virt((self.frame + OFFSET_BOUNCE) as u64) as *mut u8, buffer.len()) };
        self.command(if self.lba48 { ATA_WRITE_DMA_EXT } else { ATA_WRITE_DMA }, lba, sectors, true, buffer.len() as u32)
    }

    /// Flushes the drive's write cache.
    pub fn flush(&self) -> Result<(), Error> {
        self.command(if self.lba48 { ATA_FLUSH_CACHE_EXT } else { ATA_FLUSH_CACHE }, 0, 0, false, 0)
    }
}

/// Intel PCHs in "RAID" (Intel RST) mode: the same AHCI registers at BAR 5, but class 0x01 subclass 0x04 and interface 0, so
/// the class test alone skips them (Linux's `ahci` driver lists these IDs too). Mobile/desktop 5, 6 and 7 series.
fn is_intel_raid_mode_ahci(device: &pci::Device) -> bool {
    device.vendor == 0x8086 && device.class == 0x01 && device.subclass == 0x04 && matches!(device.id, 0x2822 | 0x2826 | 0x282a)
}

/// Starts every AHCI controller (PCI class 0x01, subclass 0x06, interface 0x01, or an Intel PCH in RAID mode) and
/// calls `found` with each drive that identifies itself. Returns the drive count.
pub fn init_all(mut found: impl FnMut(Drive, usize, u8)) -> usize {
    let mut drives = 0;
    let mut controller = 0;
    for device in pci::devices().filter(|d| (d.class == 0x01 && d.subclass == 0x06 && d.interface == 0x01) || is_intel_raid_mode_ahci(d)) {
        if controller == MAX_CONTROLLERS {
            break;
        }
        drives += init_controller(&device, controller, &mut found);
        controller += 1;
    }
    drives
}

fn init_controller(device: &pci::Device, controller: usize, found: &mut impl FnMut(Drive, usize, u8)) -> usize {
    device.force_power_state_d0();
    device.enable_memory_and_bus_master();
    // The AHCI base address register (ABAR) is BAR 5.
    let Some((abar, size)) = device.memory_bar(5) else { return 0 };
    paging::map_device(abar, size);
    let abar = abar as usize;
    // BIOS/OS handoff (CAP2.BOH): ask for ownership, give the firmware a moment.
    if read32(abar + HBA_CAP2) & 1 != 0 {
        let control = read32(abar + HBA_BOHC);
        if control & 1 != 0 {
            write32(abar + HBA_BOHC, control | 2);
            wait_until(100, || read32(abar + HBA_BOHC) & 1 == 0);
        }
    }
    write32(abar + HBA_GHC, read32(abar + HBA_GHC) | GHC_AHCI_ENABLE);
    write32(abar + HBA_IS, 0xffff_ffff);
    let implemented = read32(abar + HBA_PI);
    let mut count = 0;
    for port in 0..32u8 {
        if implemented & (1 << port) == 0 || count >= MAX_DRIVES {
            continue;
        }
        let registers = abar + PORT_BASE + usize::from(port) * PORT_STRIDE;
        // A drive is there when SATA status says device present with PHY communication
        // established (DET = 3), the interface is active (IPM = 1), and it is ATA.
        let status = read32(registers + P_SSTS);
        if status & 0xf != 3 || (status >> 8) & 0xf != 1 || read32(registers + P_SIG) != SIGNATURE_ATA {
            continue;
        }
        if let Some(drive) = init_port(registers, read32(abar + HBA_CAP) & CAP_S64A != 0) {
            found(drive, controller, port);
            count += 1;
        }
    }
    count
}

/// Stops a port's engines, points it at fresh structures, restarts it, identifies the
/// drive. `None` if anything fails (the port is just skipped).
fn init_port(registers: usize, dma64: bool) -> Option<Drive> {
    // Both engines (ST, FRE) must be off before their base addresses may change.
    write32(registers + P_CMD, read32(registers + P_CMD) & !(CMD_START | CMD_FIS_RECEIVE));
    if !wait_until(100, || read32(registers + P_CMD) & (CMD_LIST_RUNNING | CMD_FIS_RUNNING) == 0) {
        return None;
    }
    let frame = memory::allocate_contiguous(FRAMES as u64, Some(1 << 32))? as usize; // zeroed
    write32(registers + P_CLB, (frame + OFFSET_COMMAND_LIST) as u32);
    write32(registers + P_CLBU, 0);
    write32(registers + P_FB, (frame + OFFSET_RECEIVED_FIS) as u32);
    write32(registers + P_FBU, 0);
    write32(registers + P_SERR, 0xffff_ffff);
    write32(registers + P_IS, 0xffff_ffff);
    write32(registers + P_CMD, read32(registers + P_CMD) | CMD_FIS_RECEIVE);
    if !wait_until(100, || read32(registers + P_TFD) & (TFD_BUSY | TFD_DRQ) == 0) {
        memory::free(frame as u64, FRAMES as u64);
        return None;
    }
    write32(registers + P_CMD, read32(registers + P_CMD) | CMD_START);

    let mut drive = Drive { registers, frame, sectors: 0, lba48: false, dma64, can_trim: false, model: [b' '; 40] };
    drive.command(ATA_IDENTIFY, 0, 0, false, 512).ok()?;
    // IDENTIFY DEVICE data: 256 little-endian words in the bounce buffer.
    let words = memory::phys_to_virt((frame + OFFSET_BOUNCE) as u64) as *const u16;
    let word = |index: usize| unsafe { words.add(index).read_volatile() };
    for index in 0..20 {
        let value = word(27 + index); // model: two characters per word, high byte first
        drive.model[index * 2] = (value >> 8) as u8;
        drive.model[index * 2 + 1] = value as u8;
    }
    drive.lba48 = word(83) & (1 << 10) != 0;
    drive.can_trim = word(169) & 1 != 0;
    drive.sectors = if drive.lba48 {
        u64::from(word(100)) | u64::from(word(101)) << 16 | u64::from(word(102)) << 32 | u64::from(word(103)) << 48
    } else {
        u64::from(word(60)) | u64::from(word(61)) << 16
    };
    // Word 106 bit 12: the logical sector is not 512 bytes. Only 512 is supported.
    if word(106) & (1 << 14) != 0 && word(106) & (1 << 12) != 0 {
        return None;
    }
    (drive.sectors != 0).then_some(drive)
}
