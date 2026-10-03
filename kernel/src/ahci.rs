//! A minimal AHCI (SATA) host bus adapter driver: find SATA disks, read their
//! identity and move sectors with DMA. Polling only, one command at a time, 512
//! byte sectors only.
//!
//! Why it exists: until now every disk except a USB stick was reached through the
//! BIOS (INT13h, one real-mode round trip per call), which a UEFI machine does
//! not have at all and which is slow even where it exists. An AHCI disk can be
//! driven natively, and unlike the USB host controllers there is no legacy
//! emulation to preserve: the BIOS merely reads its disks through the same HBA, so
//! taking the controller over (explicitly, with `sata attach`) only invalidates
//! the BIOS's own copy of that disk.
//!
//! AHCI in one paragraph. The HBA exposes up to 32 *ports*, each with a register
//! block; a SATA drive on a port is talked to through three structures in RAM
//! that the port is pointed at: a *command list* (32 headers, one per command
//! slot), a *received-FIS area* where the drive's replies land, and for each
//! command a *command table* holding the 20-byte Register H2D FIS (the ATA
//! command and its LBA/count registers) and a *PRDT*, the scatter/gather list of
//! the data buffers. Software fills slot 0's header and table, sets bit 0 of the
//! port's `CI` register, and the HBA runs the command; the bit clears when it is
//! done and the task-file register says whether it worked.

use crate::paging;
use crate::pci::{self, PciAddress};
use crate::pmem;

pub const MAX_INSTANCES: usize = 2;
/// Drives kept per controller.
pub const MAX_DRIVES: usize = 8;
pub const SECTOR_SIZE: usize = 512;
/// Most sectors one `read`/`write` call moves (the bounce buffer's size).
pub const MAX_SECTORS: usize = 16;
const TIMEOUT_TICKS: u64 = 500;

// ---- HBA registers (offsets from ABAR, PCI BAR 5) ----
const HBA_CAP: u32 = 0x00;
const HBA_GHC: u32 = 0x04;
const HBA_IS: u32 = 0x08;
const HBA_PI: u32 = 0x0c;
const HBA_CAP2: u32 = 0x24;
const HBA_BOHC: u32 = 0x28;
const PORT_BASE: u32 = 0x100;
const PORT_STRIDE: u32 = 0x80;

const GHC_AHCI_ENABLE: u32 = 1 << 31;

// ---- port registers ----
const P_CLB: u32 = 0x00;
const P_CLBU: u32 = 0x04;
const P_FB: u32 = 0x08;
const P_FBU: u32 = 0x0c;
const P_IS: u32 = 0x10;
const P_CMD: u32 = 0x18;
const P_TFD: u32 = 0x20;
const P_SIG: u32 = 0x24;
const P_SSTS: u32 = 0x28;
const P_SERR: u32 = 0x30;
const P_CI: u32 = 0x38;

const CMD_START: u32 = 1 << 0;
const CMD_FIS_RECEIVE: u32 = 1 << 4;
const CMD_FIS_RUNNING: u32 = 1 << 14;
const CMD_LIST_RUNNING: u32 = 1 << 15;
const TFD_ERROR: u32 = 1 << 0;
const TFD_DRQ: u32 = 1 << 3;
const TFD_BUSY: u32 = 1 << 7;
/// Port interrupt-status bit 30: task file error.
const IS_TASK_FILE_ERROR: u32 = 1 << 30;
const SIGNATURE_ATA: u32 = 0x0000_0101;

// ---- ATA commands ----
const ATA_IDENTIFY: u8 = 0xec;
const ATA_READ_DMA: u8 = 0xc8;
const ATA_READ_DMA_EXT: u8 = 0x25;
const ATA_WRITE_DMA: u8 = 0xca;
const ATA_WRITE_DMA_EXT: u8 = 0x35;
const ATA_FLUSH_CACHE: u8 = 0xe7;
const ATA_FLUSH_CACHE_EXT: u8 = 0xea;

// ---- layout of the 16 KiB each drive gets (four contiguous frames) ----
const OFFSET_COMMAND_LIST: u32 = 0x000; // 1 KiB aligned: 32 headers of 32 bytes
const OFFSET_RECEIVED_FIS: u32 = 0x400; // 256 byte aligned
const OFFSET_COMMAND_TABLE: u32 = 0x800; // 128 byte aligned: CFIS, then PRDT at +0x80
const OFFSET_BOUNCE: u32 = 0x1000; // MAX_SECTORS * 512 = 8 KiB

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    NotPresent,
    InitFailed,
    Timeout,
    /// The drive reported an error (the task-file status/error registers).
    Device { status: u8, error: u8 },
    /// 4Kn or otherwise unsupported sector size, or a bad request.
    Unsupported,
}

fn read32(address: u32) -> u32 {
    unsafe { core::ptr::read_volatile(address as *const u32) }
}

fn write32(address: u32, value: u32) {
    unsafe { core::ptr::write_volatile(address as *mut u32, value) }
}

fn wait_until(ticks: u64, mut condition: impl FnMut() -> bool) -> bool {
    let start = crate::interrupts::ticks();
    loop {
        crate::debug::watchdog_pet();
        if condition() {
            return true;
        }
        if crate::interrupts::ticks().wrapping_sub(start) >= ticks {
            return condition();
        }
    }
}

/// One SATA drive on one port: everything needed to read and write it. `Copy`
/// because it holds only addresses and numbers (there is no per-drive state that
/// must persist between calls the way a USB data toggle does).
#[derive(Clone, Copy)]
pub struct Drive {
    controller: u8,
    port: u8,
    registers: u32,
    /// Command list, received-FIS area, command table and the bounce buffer.
    frame: u32,
    sectors: u64,
    lba48: bool,
}

impl Drive {
    pub fn sectors(&self) -> u64 {
        self.sectors
    }

    /// Controller number and port: the drive's identity (what `disk.rs` compares
    /// to tell that a drive is already attached).
    pub fn location(&self) -> (u8, u8) {
        (self.controller, self.port)
    }

    /// Runs one ATA DMA command through command slot 0 and waits for it.
    /// `bytes` of data move between the drive and the bounce buffer.
    fn command(&self, command: u8, lba: u64, count: u32, write: bool, bytes: u32) -> Result<(), Error> {
        let list = self.frame + OFFSET_COMMAND_LIST;
        let table = self.frame + OFFSET_COMMAND_TABLE;
        // The command table's first 64 bytes hold the Register Host-to-Device FIS.
        unsafe {
            core::ptr::write_bytes(table as *mut u8, 0, 0x90);
            let fis = table as *mut u8;
            // The 28-bit commands carry LBA bits 27:24 in the device register; the
            // 48-bit ones carry bits 47:24 in the second LBA byte group.
            let ext = matches!(command, ATA_READ_DMA_EXT | ATA_WRITE_DMA_EXT | ATA_FLUSH_CACHE_EXT);
            let bytes_of = [
                0x27u8,                // FIS type: Register H2D
                0x80,                  // bit 7: this FIS carries a command
                command,
                0,                     // features (low)
                lba as u8,
                (lba >> 8) as u8,
                (lba >> 16) as u8,
                0x40 | if ext { 0 } else { ((lba >> 24) & 0xf) as u8 }, // device: LBA mode
                if ext { (lba >> 24) as u8 } else { 0 },
                if ext { (lba >> 32) as u8 } else { 0 },
                if ext { (lba >> 40) as u8 } else { 0 },
                0,                     // features (high)
                count as u8,
                (count >> 8) as u8,
            ];
            core::ptr::copy_nonoverlapping(bytes_of.as_ptr(), fis, bytes_of.len());
            if bytes != 0 {
                // One PRDT entry: the bounce buffer; DBC is the byte count minus one.
                let entry = (table + 0x80) as *mut u32;
                core::ptr::write_volatile(entry, self.frame + OFFSET_BOUNCE);
                core::ptr::write_volatile(entry.add(1), 0);
                core::ptr::write_volatile(entry.add(2), 0);
                core::ptr::write_volatile(entry.add(3), bytes - 1);
            }
            // Command header 0: five dwords of FIS, direction, PRDT length, table address.
            let header = list as *mut u32;
            core::ptr::write_volatile(
                header,
                5 | (u32::from(write) << 6) | (u32::from(bytes != 0) << 16),
            );
            core::ptr::write_volatile(header.add(1), 0); // PRD byte count, written by the HBA
            core::ptr::write_volatile(header.add(2), table);
            core::ptr::write_volatile(header.add(3), 0);
        }
        // A drive still busy with an earlier command cannot take this one.
        if !wait_until(TIMEOUT_TICKS, || read32(self.registers + P_TFD) & (TFD_BUSY | TFD_DRQ) == 0) {
            return self.recover(Error::Timeout);
        }
        write32(self.registers + P_IS, 0xffff_ffff);
        write32(self.registers + P_CI, 1);
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
        Ok(())
    }

    /// After a failed command: stop the command engine, clear the error state and
    /// start it again, so the next command begins clean. Passes `error` through.
    fn recover(&self, error: Error) -> Result<(), Error> {
        let command = read32(self.registers + P_CMD);
        write32(self.registers + P_CMD, command & !CMD_START);
        wait_until(100, || read32(self.registers + P_CMD) & CMD_LIST_RUNNING == 0);
        write32(self.registers + P_SERR, 0xffff_ffff);
        write32(self.registers + P_IS, 0xffff_ffff);
        wait_until(100, || read32(self.registers + P_TFD) & (TFD_BUSY | TFD_DRQ) == 0);
        write32(self.registers + P_CMD, read32(self.registers + P_CMD) | CMD_START);
        Err(error)
    }

    /// Reads `buffer.len() / 512` sectors (at most `MAX_SECTORS`) starting at `lba`.
    pub fn read(&self, lba: u64, buffer: &mut [u8]) -> Result<(), Error> {
        let sectors = self.check(lba, buffer.len())?;
        let (command, count) = if self.lba48 { (ATA_READ_DMA_EXT, sectors) } else { (ATA_READ_DMA, sectors) };
        self.command(command, lba, count, false, buffer.len() as u32)?;
        unsafe {
            core::ptr::copy_nonoverlapping((self.frame + OFFSET_BOUNCE) as *const u8, buffer.as_mut_ptr(), buffer.len());
        }
        Ok(())
    }

    /// Writes `buffer.len() / 512` sectors (at most `MAX_SECTORS`) starting at
    /// `lba`, then flushes the drive's write cache, so a completed write is on
    /// the platters (FOSFS has no other way to ask).
    pub fn write(&self, lba: u64, buffer: &[u8]) -> Result<(), Error> {
        let sectors = self.check(lba, buffer.len())?;
        unsafe {
            core::ptr::copy_nonoverlapping(buffer.as_ptr(), (self.frame + OFFSET_BOUNCE) as *mut u8, buffer.len());
        }
        let command = if self.lba48 { ATA_WRITE_DMA_EXT } else { ATA_WRITE_DMA };
        self.command(command, lba, sectors, true, buffer.len() as u32)?;
        let flush = if self.lba48 { ATA_FLUSH_CACHE_EXT } else { ATA_FLUSH_CACHE };
        self.command(flush, 0, 0, false, 0)
    }

    /// Validates a request and returns its sector count.
    fn check(&self, lba: u64, bytes: usize) -> Result<u32, Error> {
        if bytes == 0 || bytes % SECTOR_SIZE != 0 || bytes / SECTOR_SIZE > MAX_SECTORS {
            return Err(Error::Unsupported);
        }
        let sectors = (bytes / SECTOR_SIZE) as u32;
        if lba + u64::from(sectors) > self.sectors || (!self.lba48 && lba + u64::from(sectors) > 1 << 28) {
            return Err(Error::Unsupported);
        }
        Ok(sectors)
    }
}

/// A discovered HBA and the drives behind it.
pub struct Ahci {
    instance: u8,
    abar: u32,
    drives: [Option<Drive>; MAX_DRIVES],
    models: [[u8; 40]; MAX_DRIVES],
    drive_count: usize,
}

impl Ahci {
    pub fn instance(&self) -> u8 {
        self.instance
    }

    pub fn bar(&self) -> u32 {
        self.abar
    }

    pub fn drive_count(&self) -> usize {
        self.drive_count
    }

    pub fn drive(&self, index: usize) -> Option<Drive> {
        self.drives.get(index).copied().flatten()
    }

    /// The drive's model string from IDENTIFY (trailing spaces trimmed by the caller).
    pub fn model(&self, index: usize) -> &[u8; 40] {
        &self.models[index.min(MAX_DRIVES - 1)]
    }
}

static mut CONTROLLERS: [Option<Ahci>; MAX_INSTANCES] = [const { None }; MAX_INSTANCES];
static mut CONTROLLER_COUNT: usize = 0;
static mut INITIALIZED: bool = false;

/// Starts every AHCI controller and identifies its drives. Idempotent, lazily
/// triggered by the `sata` commands (never at boot: see the module comment).
pub fn initialize_all() -> Result<usize, Error> {
    unsafe {
        if INITIALIZED {
            return if CONTROLLER_COUNT == 0 { Err(Error::NotPresent) } else { Ok(CONTROLLER_COUNT) };
        }
        INITIALIZED = true;
    }
    let mut matches: [Option<PciAddress>; MAX_INSTANCES] = [None; MAX_INSTANCES];
    let found = pci::find_ahci_controllers(&mut matches);
    if found == 0 {
        return Err(Error::NotPresent);
    }
    let mut failed = false;
    for address in matches[..found.min(MAX_INSTANCES)].iter().flatten() {
        if init_controller(*address).is_err() {
            failed = true;
        }
    }
    unsafe {
        if CONTROLLER_COUNT == 0 {
            Err(if failed { Error::InitFailed } else { Error::NotPresent })
        } else {
            Ok(CONTROLLER_COUNT)
        }
    }
}

pub fn count() -> usize {
    initialize_all().unwrap_or(0)
}

pub fn get(index: usize) -> Option<&'static Ahci> {
    initialize_all().ok()?;
    unsafe {
        if index >= CONTROLLER_COUNT {
            return None;
        }
        (*core::ptr::addr_of!(CONTROLLERS))[index].as_ref()
    }
}

fn init_controller(address: PciAddress) -> Result<(), Error> {
    let instance = unsafe { CONTROLLER_COUNT } as u8;
    if usize::from(instance) >= MAX_INSTANCES {
        return Err(Error::InitFailed);
    }
    address.force_power_state_d0();
    address.enable_memory_and_bus_master();
    // The AHCI base address register (ABAR) is BAR 5.
    let (abar, size) = address.memory_bar_region(5).ok_or(Error::InitFailed)?;
    paging::map_mmio(abar, size).map_err(|_| Error::InitFailed)?;

    // BIOS/OS handoff (CAP2.BOH): ask for ownership, give the firmware a moment.
    if read32(abar + HBA_CAP2) & 1 != 0 {
        let control = read32(abar + HBA_BOHC);
        if control & 1 != 0 {
            write32(abar + HBA_BOHC, control | 2);
            wait_until(100, || read32(abar + HBA_BOHC) & 1 == 0);
        }
    }
    // AHCI mode on. No HBA reset: the BIOS may be using other ports, and each
    // port is configured individually below.
    write32(abar + HBA_GHC, read32(abar + HBA_GHC) | GHC_AHCI_ENABLE);
    write32(abar + HBA_IS, 0xffff_ffff);

    let mut controller = Ahci {
        instance,
        abar,
        drives: [None; MAX_DRIVES],
        models: [[b' '; 40]; MAX_DRIVES],
        drive_count: 0,
    };
    let implemented = read32(abar + HBA_PI);
    let _ = HBA_CAP;
    for port in 0..32u8 {
        if implemented & (1 << port) == 0 || controller.drive_count >= MAX_DRIVES {
            continue;
        }
        let registers = abar + PORT_BASE + u32::from(port) * PORT_STRIDE;
        // A drive is there when the SATA status says device present with PHY
        // communication established (DET = 3) and the interface is active (IPM = 1).
        let status = read32(registers + P_SSTS);
        if status & 0xf != 3 || (status >> 8) & 0xf != 1 || read32(registers + P_SIG) != SIGNATURE_ATA {
            continue;
        }
        if let Some((drive, model)) = init_port(instance, port, registers) {
            controller.models[controller.drive_count] = model;
            controller.drives[controller.drive_count] = Some(drive);
            controller.drive_count += 1;
        }
    }
    unsafe {
        CONTROLLERS[usize::from(instance)] = Some(controller);
        CONTROLLER_COUNT += 1;
    }
    Ok(())
}

/// Stops a port's engines, points it at fresh structures, restarts it and
/// identifies the drive. `None` if anything fails (the port is just skipped).
fn init_port(controller: u8, port: u8, registers: u32) -> Option<(Drive, [u8; 40])> {
    // Both the command list engine (ST) and the FIS receive engine (FRE) must be
    // off before their base addresses may change.
    let command = read32(registers + P_CMD);
    write32(registers + P_CMD, command & !(CMD_START | CMD_FIS_RECEIVE));
    if !wait_until(100, || read32(registers + P_CMD) & (CMD_LIST_RUNNING | CMD_FIS_RUNNING) == 0) {
        return None;
    }
    let frame = pmem::allocate_contiguous(4)?;
    unsafe { core::ptr::write_bytes(frame as *mut u8, 0, 4 * pmem::FRAME_SIZE as usize) };
    write32(registers + P_CLB, frame + OFFSET_COMMAND_LIST);
    write32(registers + P_CLBU, 0);
    write32(registers + P_FB, frame + OFFSET_RECEIVED_FIS);
    write32(registers + P_FBU, 0);
    write32(registers + P_SERR, 0xffff_ffff);
    write32(registers + P_IS, 0xffff_ffff);
    write32(registers + P_CMD, read32(registers + P_CMD) | CMD_FIS_RECEIVE);
    // ST may only be set once the drive is no longer busy.
    if !wait_until(100, || read32(registers + P_TFD) & (TFD_BUSY | TFD_DRQ) == 0) {
        pmem::free(frame);
        return None;
    }
    write32(registers + P_CMD, read32(registers + P_CMD) | CMD_START);

    let mut drive = Drive { controller, port, registers, frame, sectors: 0, lba48: false };
    if drive.command(ATA_IDENTIFY, 0, 0, false, 512).is_err() {
        return None;
    }
    // IDENTIFY DEVICE data: 256 little-endian words in the bounce buffer.
    let words = (frame + OFFSET_BOUNCE) as *const u16;
    let word = |index: usize| unsafe { core::ptr::read_volatile(words.add(index)) };
    // Words 27..=46 are the model, two characters per word, high byte first.
    let mut model = [b' '; 40];
    for index in 0..20 {
        let value = word(27 + index);
        model[index * 2] = (value >> 8) as u8;
        model[index * 2 + 1] = value as u8;
    }
    let lba48 = word(83) & (1 << 10) != 0;
    let sectors = if lba48 {
        u64::from(word(100)) | u64::from(word(101)) << 16 | u64::from(word(102)) << 32 | u64::from(word(103)) << 48
    } else {
        u64::from(word(60)) | u64::from(word(61)) << 16
    };
    // Word 106 bit 12: the logical sector is not 256 words (512 bytes). This
    // driver, like FOSFS, only speaks 512-byte sectors.
    if word(106) & (1 << 14) != 0 && word(106) & (1 << 12) != 0 {
        return None;
    }
    if sectors == 0 {
        return None;
    }
    drive.sectors = sectors;
    drive.lba48 = lba48;
    Some((drive, model))
}
