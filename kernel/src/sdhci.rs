//! SD card through a standard SDHCI host controller, by polling and programmed I/O (aarch64).
//!
//! Two controllers use it: the Raspberry Pi 4's Arasan EMMC2 at 0xFE340000 (where the SD slot is wired and where the
//! firmware booted us from) and QEMU's `sdhci-pci` (the test). Both follow the SDHCI register layout, but the Pi's
//! (like Linux's `sdhci-iproc` for the BCM2711) misbehaves with 8- and 16-bit register accesses, so every access here
//! is a 32-bit read, and a 16-bit or byte field is changed by read-modify-write of its word. The command and transfer-mode
//! registers share one word (0x0C) and are written together, command in the upper half, which also starts the command.
//!
//! No DMA: data moves through the buffer data port (0x20), so there are no cache or bus-address questions, and the
//! controller's interrupts only wake the CPU (the Pi's EMMC2 SPI 126: a wait that outlasts a short spin sleeps in `wfi`
//! until the controller raises its line; status bits are still read by polling, and the timer tick is the backstop). The card is reinitialised from scratch (reset, 400 kHz
//! clock, CMD0/8/55/41/2/3/9/7/ACMD6) rather than adopting what the firmware left: the sequence is short and does not
//! depend on state the firmware may have changed. SDSC cards (byte addressing) are handled; a card the firmware
//! switched to 1.8 V signalling would need a power cycle, which this driver does not do (it reports the failure).
//! Every wait is bounded.

use crate::{pci, timer};

/// Set by the first reported error (see `recover`).
static REPORTED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

const MAX_DRIVES: usize = 2;
/// Blocks per read or write command (the command's own limit is 65535).
const CHUNK_BLOCKS: usize = 128;
/// The Raspberry Pi 4's EMMC2 and its GIC distributor, by which we know we run on one.
const PI4_EMMC2: u64 = 0xfe34_0000;
const PI4_GIC_DISTRIBUTOR: u64 = 0xff84_1000;
/// The EMMC2 clock the firmware leaves, in MHz; its capabilities register does not give one.
const PI4_BASE_MHZ: u32 = 100;
/// EMMC2's GIC interrupt (SPI 126).
const PI4_INTID: u32 = 32 + 126;
/// A wait shorter than this spins; a longer one sleeps until the interrupt.
const SPIN_MICROS: u64 = 50;

static INTERRUPTS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// Set by the crash handler, which may run inside the timer interrupt: nothing can wake a `wfi` there, so waits only poll.
static POLL_ONLY: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

pub fn force_polling() {
    POLL_ONLY.store(true, core::sync::atomic::Ordering::Relaxed);
}
static WAKE_BASE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

const SDMA: usize = 0x00;
const BLOCK: usize = 0x04;
const ARGUMENT: usize = 0x08;
const COMMAND: usize = 0x0c;
const RESPONSE: usize = 0x10;
const DATA: usize = 0x20;
const PRESENT: usize = 0x24;
const CONTROL: usize = 0x28;
const CLOCK: usize = 0x2c;
const INT_STATUS: usize = 0x30;
const INT_ENABLE: usize = 0x34;
const INT_SIGNAL: usize = 0x38;
const CAPABILITIES: usize = 0x40;

const INT_COMMAND_DONE: u32 = 1 << 0;
const INT_TRANSFER_DONE: u32 = 1 << 1;
const INT_BUFFER_WRITE: u32 = 1 << 4;
const INT_BUFFER_READ: u32 = 1 << 5;
const INT_ERROR: u32 = 1 << 15;

#[derive(Clone, Copy, Debug)]
pub enum Error {
    Timeout,
    Device,
}

#[derive(Clone, Copy)]
struct Controller {
    base: u64,
    /// Card address (RCA) in the upper half, as the commands want it.
    rca: u32,
    /// SDHC and later address by block, SDSC by byte.
    block_addressed: bool,
}

static mut CONTROLLERS: [Option<Controller>; MAX_DRIVES] = [const { None }; MAX_DRIVES];

#[derive(Clone, Copy)]
pub struct Drive {
    index: usize,
    pub sectors: u64,
}

impl Drive {
    pub fn model(&self) -> [u8; 40] {
        let mut model = [b' '; 40];
        model[..7].copy_from_slice(b"SD card");
        model
    }

    pub fn read(&self, lba: u64, buffer: &mut [u8]) -> Result<(), Error> {
        let controller = controller(self.index)?;
        for (index, chunk) in buffer.chunks_mut(CHUNK_BLOCKS * 512).enumerate() {
            controller.transfer(false, lba + (index * CHUNK_BLOCKS) as u64, chunk.as_mut_ptr(), chunk.len())?;
        }
        Ok(())
    }

    pub fn write(&self, lba: u64, buffer: &[u8]) -> Result<(), Error> {
        let controller = controller(self.index)?;
        for (index, chunk) in buffer.chunks(CHUNK_BLOCKS * 512).enumerate() {
            controller.transfer(true, lba + (index * CHUNK_BLOCKS) as u64, chunk.as_ptr() as *mut u8, chunk.len())?;
        }
        Ok(())
    }
}

fn controller(index: usize) -> Result<Controller, Error> {
    unsafe { (*core::ptr::addr_of!(CONTROLLERS))[index].ok_or(Error::Device) }
}

impl Controller {
    fn read(&self, offset: usize) -> u32 {
        unsafe { ((self.base + offset as u64) as *const u32).read_volatile() }
    }

    fn write(&self, offset: usize, value: u32) {
        unsafe { ((self.base + offset as u64) as *mut u32).write_volatile(value) }
    }

    fn wait(&self, offset: usize, mask: u32, wanted: u32, microseconds: u64) -> Result<(), Error> {
        let start = timer::micros();
        while self.read(offset) & mask != wanted {
            if timer::micros() - start > microseconds {
                return Err(Error::Timeout);
            }
        }
        Ok(())
    }

    /// Resets the controller and its card clock/power; leaves the clock at 400 kHz, 1-bit bus.
    fn reset(&mut self, pi4: bool) -> Result<(), Error> {
        self.write(CLOCK, self.read(CLOCK) | 1 << 24); // software reset for all
        self.wait(CLOCK, 1 << 24, 0, 100_000)?;
        // Bus power on, 3.3 V (control register byte 1: 0x0e voltage select, bit 0 power).
        self.write(CONTROL, (self.read(CONTROL) & !0xff00) | 0x0f00);
        timer::busy_wait_ms(2);
        // Status bits are polled: all enabled in the status register, none routed to an interrupt line.
        self.write(INT_ENABLE, 0xffff_feff);
        self.write(INT_SIGNAL, 0);
        let capabilities_mhz = (self.read(CAPABILITIES) >> 8) & 0xff;
        let base_mhz = if pi4 || capabilities_mhz == 0 { PI4_BASE_MHZ } else { capabilities_mhz };
        self.set_clock(base_mhz, 400_000)
    }

    /// Sets the card clock to the highest of base/(2N), N a power of two (or the base itself), not above `hertz`.
    fn set_clock(&self, base_mhz: u32, hertz: u32) -> Result<(), Error> {
        let base = base_mhz * 1_000_000;
        let mut divisor = 0u32; // N; 0 = undivided
        if base > hertz {
            divisor = 1;
            while base / (2 * divisor) > hertz && divisor < 128 {
                divisor *= 2;
            }
        }
        // Clock control word: bits 15:8 divisor, 2 card clock on, 1 internal clock stable, 0 internal clock on; byte 2 data timeout.
        let keep = self.read(CLOCK) & 0x00ff_0000 | 0x000e_0000;
        self.write(CLOCK, keep | divisor << 8 | 1);
        self.wait(CLOCK, 2, 2, 50_000)?;
        self.write(CLOCK, keep | divisor << 8 | 1 | 4);
        timer::busy_wait_ms(1);
        Ok(())
    }

    /// One command; returns the first response word and fills `response` with all four.
    fn command(&self, index: u32, argument: u32, flags: u32, mode: u32, response: &mut [u32; 4]) -> Result<(), Error> {
        let data_busy = flags & 0x20 != 0 || flags & 3 == 3;
        self.wait(PRESENT, 1 | if data_busy { 2 } else { 0 }, 0, 100_000)?;
        self.write(INT_STATUS, 0xffff_ffff);
        self.write(ARGUMENT, argument);
        self.write(COMMAND, (index << 8 | flags) << 16 | mode);
        let result = self.wait_status(INT_COMMAND_DONE);
        if result.is_err() {
            self.recover();
            return result;
        }
        for (word, slot) in response.iter_mut().enumerate() {
            *slot = self.read(RESPONSE + word * 4);
        }
        self.write(INT_STATUS, INT_COMMAND_DONE);
        if flags & 3 == 3 {
            // R1b: the card holds DAT0 low while busy; the controller reports transfer complete at the end.
            if let Err(error) = self.wait_status(INT_TRANSFER_DONE) {
                self.recover();
                return Err(error);
            }
            self.write(INT_STATUS, INT_TRANSFER_DONE);
        }
        Ok(())
    }

    fn wait_status(&self, bit: u32) -> Result<(), Error> {
        let start = timer::micros();
        let wakes = WAKE_BASE.load(core::sync::atomic::Ordering::Relaxed) == self.base && !POLL_ONLY.load(core::sync::atomic::Ordering::Relaxed);
        loop {
            let status = self.read(INT_STATUS);
            if status & INT_ERROR != 0 {
                return Err(Error::Device);
            }
            if status & bit != 0 {
                return Ok(());
            }
            if timer::micros() - start > 500_000 {
                return Err(Error::Timeout);
            }
            if wakes && timer::micros() - start > SPIN_MICROS {
                // Raise the line for this bit (or an error); with interrupts masked the check and the sleep cannot miss it,
                // as `wfi` wakes on a pending interrupt whether or not it is taken.
                let was = crate::cpu::interrupts_save_and_disable();
                self.write(INT_SIGNAL, bit | INT_ERROR);
                if self.read(INT_STATUS) & (bit | INT_ERROR) == 0 {
                    crate::cpu::sleep();
                }
                crate::cpu::interrupts_restore(was);
            }
        }
    }

    /// After an error: reset the command and data lines and clear the status. The first error is reported (once) with the
    /// controller's own view, because a card that stops answering after one failed write has otherwise left no trace.
    fn recover(&self) {
        if !REPORTED.swap(true, core::sync::atomic::Ordering::Relaxed) {
            crate::println!("SD: error: interrupt status {:#x}, present state {:#x}, clock {:#x}.", self.read(INT_STATUS),
                            self.read(PRESENT), self.read(CLOCK));
        }
        self.write(CLOCK, self.read(CLOCK) | 3 << 25);
        let _ = self.wait(CLOCK, 3 << 25, 0, 100_000);
        self.write(INT_STATUS, 0xffff_ffff);
    }

    fn r1(&self, index: u32, argument: u32) -> Result<u32, Error> {
        let mut response = [0; 4];
        self.command(index, argument, 0x1a, 0, &mut response)?; // 48-bit, CRC and index checked
        Ok(response[0])
    }

    /// Application command: CMD55 with the card's address, then `index`.
    fn application(&self, index: u32, argument: u32, flags: u32, response: &mut [u32; 4]) -> Result<(), Error> {
        self.r1(55, self.rca)?;
        self.command(index, argument, flags, 0, response)
    }

    /// Brings the card to the transfer state; returns its capacity in 512-byte sectors.
    fn card_init(&mut self) -> Result<u64, Error> {
        let mut response = [0; 4];
        self.rca = 0;
        self.command(0, 0, 0, 0, &mut response)?;
        timer::busy_wait_ms(2);
        // CMD8: a version 2 card echoes the voltage range and check pattern; an older one does not answer.
        let version2 = self.command(8, 0x1aa, 0x1a, 0, &mut response).is_ok() && response[0] & 0xfff == 0x1aa;
        let start = timer::micros();
        let high_capacity = loop {
            // ACMD41 with the voltage window 2.7-3.6 V, and "host supports high capacity" for a version 2 card.
            self.application(41, 0x00ff_8000 | if version2 { 1 << 30 } else { 0 }, 0x02, &mut response)?;
            if response[0] & 1 << 31 != 0 {
                break response[0] & 1 << 30 != 0;
            }
            if timer::micros() - start > 1_000_000 {
                return Err(Error::Timeout);
            }
            timer::busy_wait_ms(10);
        };
        self.block_addressed = high_capacity;
        self.command(2, 0, 0x09, 0, &mut response)?; // CID, 136-bit response
        self.command(3, 0, 0x1a, 0, &mut response)?;
        self.rca = response[0] & 0xffff_0000;
        self.command(9, self.rca, 0x09, 0, &mut response)?; // CSD
        let csd = u128::from(response[3]) << 96 | u128::from(response[2]) << 64 | u128::from(response[1]) << 32 | u128::from(response[0]);
        // The response omits the CSD's CRC byte, so CSD bit n is bit n-8 here.
        let field = |high: u32, low: u32| ((csd >> (low - 8)) & ((1 << (high - low + 1)) - 1)) as u64;
        let sectors = if field(127, 126) == 1 {
            (field(69, 48) + 1) * 1024
        } else {
            let blocks = (field(73, 62) + 1) << (field(49, 47) + 2);
            blocks << field(83, 80) >> 9
        };
        self.command(7, self.rca, 0x1b, 0, &mut response)?; // select; R1b
        if !self.block_addressed {
            self.r1(16, 512)?;
        }
        self.application(6, 2, 0x1a, &mut response)?; // 4-bit bus
        self.write(CONTROL, self.read(CONTROL) | 2);
        Ok(sectors)
    }

    /// Reads or writes `length` bytes (whole blocks) starting at sector `lba`, through the data port.
    fn transfer(&self, write: bool, lba: u64, data: *mut u8, length: usize) -> Result<(), Error> {
        let blocks = (length / 512) as u32;
        let address = if self.block_addressed { lba as u32 } else { (lba * 512) as u32 };
        let multiple = blocks > 1;
        let command = match (write, multiple) {
            (false, false) => 17,
            (false, true) => 18,
            (true, false) => 24,
            (true, true) => 25,
        };
        // Transfer mode: 1 block count on, 2 auto-CMD12 for a multiple transfer, 4 read, 5 multiple blocks.
        let mode = 1 << 1 | if multiple { 1 << 5 | 1 << 2 } else { 0 } | if write { 0 } else { 1 << 4 };
        self.write(SDMA, 0);
        self.write(BLOCK, blocks << 16 | 512);
        let mut response = [0; 4];
        self.command(command, address, 0x3a, mode, &mut response)?; // R1 with data
        let (ready, words) = if write { (INT_BUFFER_WRITE, true) } else { (INT_BUFFER_READ, false) };
        for block in 0..blocks as usize {
            if let Err(error) = self.wait_status(ready) {
                self.recover();
                return Err(error);
            }
            self.write(INT_STATUS, ready);
            for word in 0..128 {
                let at = unsafe { data.add(block * 512 + word * 4) } as *mut u32;
                if words {
                    self.write(DATA, unsafe { at.read_unaligned() });
                } else {
                    unsafe { at.write_unaligned(self.read(DATA)) };
                }
            }
        }
        if let Err(error) = self.wait_status(INT_TRANSFER_DONE) {
            self.recover();
            return Err(error);
        }
        self.write(INT_STATUS, INT_TRANSFER_DONE);
        Ok(())
    }
}

/// IRQ context: drop the signal enable so the level line falls (the status bits stay for the poller).
fn interrupt(_intid: u32) {
    INTERRUPTS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    let base = WAKE_BASE.load(core::sync::atomic::Ordering::Relaxed);
    if base != 0 {
        unsafe { ((base + INT_SIGNAL as u64) as *mut u32).write_volatile(0) };
    }
}

/// How many SD interrupts have been taken.
pub fn interrupt_count() -> u64 {
    INTERRUPTS.load(core::sync::atomic::Ordering::Relaxed)
}

fn bring_up(base: u64, slot: usize, pi4: bool) -> Option<Drive> {
    crate::paging::map_device(base, 0x1000);
    let mut controller = Controller { base, rca: 0, block_addressed: true };
    if controller.read(CAPABILITIES + 0x10) == 0xffff_ffff {
        return None; // nothing answers there
    }
    if let Err(error) = controller.reset(pi4) {
        crate::println!("SD: controller at {base:#x} did not reset ({error:?}).");
        return None;
    }
    if controller.read(PRESENT) & (1 << 16) == 0 && !pi4 {
        crate::println!("SD: controller at {base:#x}: no card inserted.");
        return None;
    }
    match controller.card_init() {
        Ok(sectors) => {
            // Faster, now that the card is selected and in 4-bit mode: 25 MHz (default speed, no mode switch needed).
            let base_mhz = if pi4 || (controller.read(CAPABILITIES) >> 8) & 0xff == 0 { PI4_BASE_MHZ } else { (controller.read(CAPABILITIES) >> 8) & 0xff };
            if controller.set_clock(base_mhz, 25_000_000).is_err() {
                return None;
            }
            unsafe { (*core::ptr::addr_of_mut!(CONTROLLERS))[slot] = Some(controller) };
            if pi4 {
                WAKE_BASE.store(base, core::sync::atomic::Ordering::Relaxed);
                crate::gic::register(PI4_INTID, true, interrupt);
            }
            Some(Drive { index: slot, sectors })
        }
        Err(error) => {
            crate::println!("SD: controller at {base:#x}: card initialisation failed ({error:?}).");
            None
        }
    }
}

/// Finds the SD host controllers (the Pi 4's EMMC2, PCI class 0x0805 devices) and calls `register` for each card.
pub fn init_all(mut register: impl FnMut(Drive, usize)) {
    let mut slot = 0;
    if crate::gic::bases().0 == PI4_GIC_DISTRIBUTOR {
        if let Some(drive) = bring_up(PI4_EMMC2, slot, true) {
            register(drive, slot);
            slot += 1;
        }
    }
    for device in pci::devices() {
        if slot == MAX_DRIVES {
            break;
        }
        if device.class != 0x08 || device.subclass != 0x05 {
            continue;
        }
        device.enable_memory_and_bus_master();
        let Some((base, _)) = device.memory_bar(0) else { continue };
        if let Some(drive) = bring_up(base, slot, false) {
            register(drive, slot);
            slot += 1;
        }
    }
}
