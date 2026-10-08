//! A minimal AHCI (SATA) host bus adapter driver: find SATA disks, read their identity
//! and move sectors with DMA. Polling only, one command at a time, 512-byte sectors.
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

use crate::{apic, memory, paging, pci};

pub const MAX_CONTROLLERS: usize = 2;
pub const MAX_DRIVES: usize = 8;
/// Most sectors one `read`/`write` call moves (the bounce buffer's size).
pub const MAX_SECTORS: usize = 16;
const TIMEOUT_TICKS: u64 = 500;

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
const ATA_FLUSH_CACHE: u8 = 0xe7;
const ATA_FLUSH_CACHE_EXT: u8 = 0xea;

// Layout of the 16 KiB each drive gets (four contiguous frames).
const OFFSET_COMMAND_LIST: usize = 0x000; // 1 KiB aligned: 32 headers of 32 bytes
const OFFSET_RECEIVED_FIS: usize = 0x400; // 256-byte aligned
const OFFSET_COMMAND_TABLE: usize = 0x800; // 128-byte aligned: CFIS, then the PRDT at +0x80
const OFFSET_BOUNCE: usize = 0x1000; // MAX_SECTORS * 512 = 8 KiB

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Timeout,
    Device { status: u8, error: u8 },
    Unsupported,
}

fn read32(address: usize) -> u32 {
    unsafe { (address as *const u32).read_volatile() }
}
fn write32(address: usize, value: u32) {
    unsafe { (address as *mut u32).write_volatile(value) }
}

fn wait_until(ticks: u64, mut condition: impl FnMut() -> bool) -> bool {
    let start = apic::ticks();
    loop {
        crate::debug::pet();
        if condition() {
            return true;
        }
        if apic::ticks().wrapping_sub(start) >= ticks {
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
    pub model: [u8; 40],
}

impl Drive {
    /// Runs one ATA DMA command through command slot 0 and waits for it.
    fn command(&self, command: u8, lba: u64, count: u32, write: bool, bytes: u32) -> Result<(), Error> {
        let list = self.frame + OFFSET_COMMAND_LIST;
        let table = self.frame + OFFSET_COMMAND_TABLE;
        unsafe {
            core::ptr::write_bytes(table as *mut u8, 0, 0x90);
            // 28-bit commands carry LBA bits 27:24 in the device register; 48-bit ones
            // carry bits 47:24 in the second LBA byte group.
            let ext = matches!(command, ATA_READ_DMA_EXT | ATA_WRITE_DMA_EXT | ATA_FLUSH_CACHE_EXT);
            let fis = [
                0x27u8, // FIS type: Register Host-to-Device
                0x80,   // bit 7: this FIS carries a command
                command,
                0,
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
            core::ptr::copy_nonoverlapping(fis.as_ptr(), table as *mut u8, fis.len());
            if bytes != 0 {
                // One PRDT entry: the bounce buffer; DBC is the byte count minus one.
                let entry = (table + 0x80) as *mut u32;
                entry.write_volatile((self.frame + OFFSET_BOUNCE) as u32);
                entry.add(1).write_volatile(0);
                entry.add(2).write_volatile(0);
                entry.add(3).write_volatile(bytes - 1);
            }
            let header = list as *mut u32;
            header.write_volatile(5 | (u32::from(write) << 6) | (u32::from(bytes != 0) << 16)); // 5 FIS dwords, direction, PRDT length
            header.add(1).write_volatile(0);
            header.add(2).write_volatile(table as u32);
            header.add(3).write_volatile(0);
        }
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
        unsafe { core::ptr::copy_nonoverlapping((self.frame + OFFSET_BOUNCE) as *const u8, buffer.as_mut_ptr(), buffer.len()) };
        Ok(())
    }

    /// Writes sectors, then flushes the drive's write cache so a completed write is on
    /// the platters.
    pub fn write(&self, lba: u64, buffer: &[u8]) -> Result<(), Error> {
        let sectors = self.check(lba, buffer.len())?;
        unsafe { core::ptr::copy_nonoverlapping(buffer.as_ptr(), (self.frame + OFFSET_BOUNCE) as *mut u8, buffer.len()) };
        self.command(if self.lba48 { ATA_WRITE_DMA_EXT } else { ATA_WRITE_DMA }, lba, sectors, true, buffer.len() as u32)?;
        self.command(if self.lba48 { ATA_FLUSH_CACHE_EXT } else { ATA_FLUSH_CACHE }, 0, 0, false, 0)
    }
}

/// Starts every AHCI controller (PCI class 0x01, subclass 0x06, interface 0x01) and
/// calls `found` with each drive that identifies itself. Returns the drive count.
pub fn init_all(mut found: impl FnMut(Drive, usize, u8)) -> usize {
    let mut drives = 0;
    let mut controller = 0;
    for device in pci::devices().filter(|d| d.class == 0x01 && d.subclass == 0x06 && d.interface == 0x01) {
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
        if let Some(drive) = init_port(registers) {
            found(drive, controller, port);
            count += 1;
        }
    }
    count
}

/// Stops a port's engines, points it at fresh structures, restarts it, identifies the
/// drive. `None` if anything fails (the port is just skipped).
fn init_port(registers: usize) -> Option<Drive> {
    // Both engines (ST, FRE) must be off before their base addresses may change.
    write32(registers + P_CMD, read32(registers + P_CMD) & !(CMD_START | CMD_FIS_RECEIVE));
    if !wait_until(100, || read32(registers + P_CMD) & (CMD_LIST_RUNNING | CMD_FIS_RUNNING) == 0) {
        return None;
    }
    let frame = memory::allocate_contiguous(4, Some(1 << 32))? as usize; // zeroed
    write32(registers + P_CLB, (frame + OFFSET_COMMAND_LIST) as u32);
    write32(registers + P_CLBU, 0);
    write32(registers + P_FB, (frame + OFFSET_RECEIVED_FIS) as u32);
    write32(registers + P_FBU, 0);
    write32(registers + P_SERR, 0xffff_ffff);
    write32(registers + P_IS, 0xffff_ffff);
    write32(registers + P_CMD, read32(registers + P_CMD) | CMD_FIS_RECEIVE);
    if !wait_until(100, || read32(registers + P_TFD) & (TFD_BUSY | TFD_DRQ) == 0) {
        memory::free(frame as u64, 4);
        return None;
    }
    write32(registers + P_CMD, read32(registers + P_CMD) | CMD_START);

    let mut drive = Drive { registers, frame, sectors: 0, lba48: false, model: [b' '; 40] };
    drive.command(ATA_IDENTIFY, 0, 0, false, 512).ok()?;
    // IDENTIFY DEVICE data: 256 little-endian words in the bounce buffer.
    let words = (frame + OFFSET_BOUNCE) as *const u16;
    let word = |index: usize| unsafe { words.add(index).read_volatile() };
    for index in 0..20 {
        let value = word(27 + index); // model: two characters per word, high byte first
        drive.model[index * 2] = (value >> 8) as u8;
        drive.model[index * 2 + 1] = value as u8;
    }
    drive.lba48 = word(83) & (1 << 10) != 0;
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
