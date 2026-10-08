//! A minimal NVMe driver: find NVM Express controllers, read namespace 1's identity and
//! move sectors with DMA. Polling only (the controller's interrupts stay masked), one
//! command at a time, 512-byte logical blocks only.
//!
//! NVMe in one paragraph. The controller's registers (BAR 0) hold a few control words and,
//! from offset 0x1000, *doorbells*. Everything else is queues in RAM: a *submission
//! queue* (SQ) of 64-byte commands that software fills and then announces by writing the
//! new tail index to the queue's doorbell, and a *completion queue* (CQ) of 16-byte
//! entries the controller fills. Each completion carries a *phase bit* that the controller
//! flips every time it wraps the queue, so software finds new entries by watching for the
//! phase it expects, then tells the controller how far it has read by writing the new head
//! to the CQ doorbell. There is one *admin* queue pair (Identify, create/delete queues)
//! and any number of *I/O* queue pairs; this driver makes one of each. Data buffers are
//! described by *PRP* (physical region page) pointers: PRP1 points at the buffer, and
//! PRP2 at the second page when a transfer crosses a page boundary (up to two pages here,
//! which is all the 8 KiB bounce buffer ever needs).
//!
//! Start-up follows the specification's order: clear `CC.EN` and wait for `CSTS.RDY` to
//! drop, program the admin queue addresses and sizes, set `CC.EN` with the queue entry
//! sizes, wait for `RDY`, then Identify and create the I/O queues with admin commands.

use crate::{apic, memory, paging, pci};

pub const MAX_CONTROLLERS: usize = 4;
/// Most sectors one `read`/`write` call moves (the bounce buffer's size, two pages).
pub const MAX_SECTORS: usize = 16;
const QUEUE_ENTRIES: usize = 64;
const PAGE: usize = 4096;

const REG_CAP: usize = 0x00;
const REG_INTMS: usize = 0x0c;
const REG_CC: usize = 0x14;
const REG_CSTS: usize = 0x1c;
const REG_AQA: usize = 0x24;
const REG_ASQ: usize = 0x28;
const REG_ACQ: usize = 0x30;
const DOORBELLS: usize = 0x1000;

const CC_ENABLE: u32 = 1;
const CSTS_READY: u32 = 1;
const CSTS_FATAL: u32 = 1 << 1;

const ADMIN_CREATE_SQ: u32 = 0x01;
const ADMIN_CREATE_CQ: u32 = 0x05;
const ADMIN_IDENTIFY: u32 = 0x06;
const IO_FLUSH: u32 = 0x00;
const IO_WRITE: u32 = 0x01;
const IO_READ: u32 = 0x02;

// Layout of the six frames each controller gets.
const OFFSET_ADMIN_SQ: usize = 0 * PAGE;
const OFFSET_ADMIN_CQ: usize = 1 * PAGE;
const OFFSET_IO_SQ: usize = 2 * PAGE;
const OFFSET_IO_CQ: usize = 3 * PAGE;
const OFFSET_BOUNCE: usize = 4 * PAGE; // two pages: MAX_SECTORS * 512

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Timeout,
    /// The completion's status field (status code type and code).
    Command(u16),
    Unsupported,
}

fn read32(address: usize) -> u32 {
    unsafe { (address as *const u32).read_volatile() }
}
fn write32(address: usize, value: u32) {
    unsafe { (address as *mut u32).write_volatile(value) }
}
fn read64(address: usize) -> u64 {
    u64::from(read32(address)) | u64::from(read32(address + 4)) << 32
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

/// One submission/completion queue pair and where its doorbells are.
struct Queue {
    submission: usize,
    completion: usize,
    submission_doorbell: usize,
    completion_doorbell: usize,
    tail: usize,
    head: usize,
    /// The phase bit the next new completion will carry (starts at 1).
    phase: u32,
}

impl Queue {
    fn new(submission: usize, completion: usize, registers: usize, stride: usize, id: usize) -> Queue {
        Queue {
            submission,
            completion,
            submission_doorbell: registers + DOORBELLS + 2 * id * stride,
            completion_doorbell: registers + DOORBELLS + (2 * id + 1) * stride,
            tail: 0,
            head: 0,
            phase: 1,
        }
    }

    /// Submits one command (16 dwords; the command identifier is filled in here) and
    /// polls for its completion. With one command in flight the completion at the head
    /// is always ours.
    fn execute(&mut self, mut command: [u32; 16], timeout_ticks: u64) -> Result<(), Error> {
        command[0] = (command[0] & 0xffff) | (self.tail as u32) << 16;
        let slot = (self.submission + self.tail * 64) as *mut u32;
        for (index, word) in command.iter().enumerate() {
            unsafe { slot.add(index).write_volatile(*word) };
        }
        self.tail = (self.tail + 1) % QUEUE_ENTRIES;
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        write32(self.submission_doorbell, self.tail as u32);

        let entry = (self.completion + self.head * 16) as *const u32;
        let phase = self.phase;
        let arrived = wait_until(timeout_ticks, || unsafe { (entry.add(3).read_volatile() >> 16) & 1 == phase });
        if !arrived {
            return Err(Error::Timeout);
        }
        let status = unsafe { entry.add(3).read_volatile() };
        self.head += 1;
        if self.head == QUEUE_ENTRIES {
            self.head = 0;
            self.phase ^= 1;
        }
        write32(self.completion_doorbell, self.head as u32);
        match (status >> 17) as u16 & 0x7fff {
            0 => Ok(()),
            code => Err(Error::Command(code)),
        }
    }
}

struct Controller {
    registers: usize,
    frame: usize,
    io: Queue,
    timeout_ticks: u64,
}

static mut CONTROLLERS: [Option<Controller>; MAX_CONTROLLERS] = [const { None }; MAX_CONTROLLERS];

/// One NVMe namespace (always namespace 1) as the block layer sees it.
#[derive(Clone, Copy)]
pub struct Drive {
    index: usize,
    pub sectors: u64,
    pub model: [u8; 40],
}

impl Drive {
    fn controller(&self) -> &'static mut Controller {
        unsafe { (*core::ptr::addr_of_mut!(CONTROLLERS))[self.index].as_mut().unwrap() }
    }

    fn check(&self, lba: u64, bytes: usize) -> Result<u32, Error> {
        if bytes == 0 || bytes % 512 != 0 || bytes / 512 > MAX_SECTORS {
            return Err(Error::Unsupported);
        }
        let sectors = (bytes / 512) as u32;
        if lba + u64::from(sectors) > self.sectors {
            return Err(Error::Unsupported);
        }
        Ok(sectors)
    }

    fn transfer(&self, opcode: u32, lba: u64, sectors: u32) -> Result<(), Error> {
        let controller = self.controller();
        let bounce = controller.frame + OFFSET_BOUNCE;
        let mut command = [0u32; 16];
        command[0] = opcode;
        command[1] = 1; // namespace 1
        command[6] = bounce as u32; // PRP1
        command[7] = (bounce as u64 >> 32) as u32;
        if sectors > 8 {
            command[8] = (bounce + PAGE) as u32; // PRP2: the second page
            command[9] = ((bounce + PAGE) as u64 >> 32) as u32;
        }
        command[10] = lba as u32; // starting LBA, low and high dwords
        command[11] = (lba >> 32) as u32;
        command[12] = sectors - 1; // number of logical blocks, zero based
        let timeout = controller.timeout_ticks;
        controller.io.execute(command, timeout)
    }

    /// Reads `buffer.len() / 512` sectors (at most `MAX_SECTORS`) starting at `lba`.
    pub fn read(&self, lba: u64, buffer: &mut [u8]) -> Result<(), Error> {
        let sectors = self.check(lba, buffer.len())?;
        self.transfer(IO_READ, lba, sectors)?;
        let bounce = self.controller().frame + OFFSET_BOUNCE;
        unsafe { core::ptr::copy_nonoverlapping(bounce as *const u8, buffer.as_mut_ptr(), buffer.len()) };
        Ok(())
    }

    /// Writes sectors, then flushes so a completed write is on the media.
    pub fn write(&self, lba: u64, buffer: &[u8]) -> Result<(), Error> {
        let sectors = self.check(lba, buffer.len())?;
        let bounce = self.controller().frame + OFFSET_BOUNCE;
        unsafe { core::ptr::copy_nonoverlapping(buffer.as_ptr(), bounce as *mut u8, buffer.len()) };
        self.transfer(IO_WRITE, lba, sectors)?;
        let controller = self.controller();
        let mut flush = [0u32; 16];
        flush[0] = IO_FLUSH;
        flush[1] = 1;
        let timeout = controller.timeout_ticks;
        controller.io.execute(flush, timeout)
    }
}

/// Starts every NVMe controller (PCI class 0x01, subclass 0x08, interface 0x02) and
/// calls `found` with each namespace-1 drive that identifies itself. Returns the count.
pub fn init_all(mut found: impl FnMut(Drive, usize)) -> usize {
    let mut drives = 0;
    let mut controller = 0;
    for device in pci::devices().filter(|d| d.class == 0x01 && d.subclass == 0x08 && d.interface == 0x02) {
        if controller == MAX_CONTROLLERS {
            break;
        }
        if let Some(drive) = init_controller(&device, controller) {
            found(drive, controller);
            drives += 1;
        }
        controller += 1;
    }
    drives
}

fn init_controller(device: &pci::Device, index: usize) -> Option<Drive> {
    device.force_power_state_d0();
    device.enable_memory_and_bus_master();
    let (bar, size) = device.memory_bar(0)?;
    paging::map_device(bar, size);
    let registers = bar as usize;

    let cap = read64(registers + REG_CAP);
    let stride = 4usize << ((cap >> 32) & 0xf); // doorbell stride (CAP.DSTRD)
    let timeout_ticks = (((cap >> 24) & 0xff) + 1) * 50; // CAP.TO is in 500 ms units; ticks are 10 ms
    let supports_nvm_commands = (cap >> 37) & 1 != 0; // CAP.CSS bit 0
    let smallest_page = (cap >> 48) & 0xf; // CAP.MPSMIN: 0 means 4 KiB pages are allowed
    if !supports_nvm_commands || smallest_page != 0 {
        return None;
    }

    // Reset: clear EN and wait for RDY to drop. Keep the controller's interrupts masked
    // (INTMS) so a polled driver is never interrupted by an INTx line.
    write32(registers + REG_CC, read32(registers + REG_CC) & !CC_ENABLE);
    if !wait_until(timeout_ticks, || read32(registers + REG_CSTS) & CSTS_READY == 0) {
        return None;
    }
    write32(registers + REG_INTMS, 0xffff_ffff);

    let frame = memory::allocate_contiguous(6, Some(1 << 32))? as usize; // zeroed
    let mut admin = Queue::new(frame + OFFSET_ADMIN_SQ, frame + OFFSET_ADMIN_CQ, registers, stride, 0);
    write32(registers + REG_AQA, ((QUEUE_ENTRIES as u32 - 1) << 16) | (QUEUE_ENTRIES as u32 - 1));
    write32(registers + REG_ASQ, admin.submission as u32);
    write32(registers + REG_ASQ + 4, 0);
    write32(registers + REG_ACQ, admin.completion as u32);
    write32(registers + REG_ACQ + 4, 0);
    // EN, command set NVM (CSS 0), 4 KiB pages (MPS 0), 64-byte SQ entries (2^6) and
    // 16-byte CQ entries (2^4).
    write32(registers + REG_CC, CC_ENABLE | (6 << 16) | (4 << 20));
    let ready = wait_until(timeout_ticks, || {
        let status = read32(registers + REG_CSTS);
        status & (CSTS_READY | CSTS_FATAL) != 0
    });
    if !ready || read32(registers + REG_CSTS) & CSTS_FATAL != 0 {
        memory::free(frame as u64, 6);
        return None;
    }

    let bounce = frame + OFFSET_BOUNCE;
    let identify = |admin: &mut Queue, namespace: u32, what: u32| {
        let mut command = [0u32; 16];
        command[0] = ADMIN_IDENTIFY;
        command[1] = namespace;
        command[6] = bounce as u32;
        command[7] = (bounce as u64 >> 32) as u32;
        command[10] = what; // CNS: 1 = controller, 0 = namespace
        unsafe { core::ptr::write_bytes(bounce as *mut u8, 0, PAGE) };
        admin.execute(command, timeout_ticks)
    };

    let result = (|| {
        identify(&mut admin, 0, 1).ok()?;
        let mut model = [b' '; 40];
        unsafe { core::ptr::copy_nonoverlapping((bounce + 24) as *const u8, model.as_mut_ptr(), 40) }; // MN: bytes 24..64

        identify(&mut admin, 1, 0).ok()?;
        let sectors = unsafe { ((bounce) as *const u64).read_volatile() }; // NSZE
        let format = unsafe { *((bounce + 26) as *const u8) } & 0xf; // FLBAS: the active LBA format
        let lba_format = unsafe { ((bounce + 128 + 4 * usize::from(format)) as *const u32).read_volatile() };
        // LBAF: bits 15:0 metadata size, 23:16 log2 of the data size. Only plain 512-byte blocks.
        if sectors == 0 || (lba_format >> 16) & 0xff != 9 || lba_format & 0xffff != 0 {
            return None;
        }

        // I/O completion queue 1, then the submission queue 1 that reports into it.
        let mut create_cq = [0u32; 16];
        create_cq[0] = ADMIN_CREATE_CQ;
        create_cq[6] = (frame + OFFSET_IO_CQ) as u32;
        create_cq[10] = ((QUEUE_ENTRIES as u32 - 1) << 16) | 1; // queue size (zero based), queue id 1
        create_cq[11] = 1; // physically contiguous, interrupts off
        admin.execute(create_cq, timeout_ticks).ok()?;
        let mut create_sq = [0u32; 16];
        create_sq[0] = ADMIN_CREATE_SQ;
        create_sq[6] = (frame + OFFSET_IO_SQ) as u32;
        create_sq[10] = ((QUEUE_ENTRIES as u32 - 1) << 16) | 1;
        create_sq[11] = (1 << 16) | 1; // completes into CQ 1, physically contiguous
        admin.execute(create_sq, timeout_ticks).ok()?;
        Some((model, sectors))
    })();
    let Some((model, sectors)) = result else {
        write32(registers + REG_CC, read32(registers + REG_CC) & !CC_ENABLE);
        memory::free(frame as u64, 6);
        return None;
    };

    let io = Queue::new(frame + OFFSET_IO_SQ, frame + OFFSET_IO_CQ, registers, stride, 1);
    unsafe { (*core::ptr::addr_of_mut!(CONTROLLERS))[index] = Some(Controller { registers, frame, io, timeout_ticks }) };
    Some(Drive { index, sectors, model })
}
