//! Minimal polling driver for 3Com 3C905/B/C PCI Ethernet controllers.
//!
//! These cards use the Cyclone/Tornado bus-master interface: receive and
//! transmit descriptors live in physical memory, while the device is
//! controlled through a windowed command register at BAR0/BAR1. This driver
//! intentionally implements only the raw Ethernet operations the kernel
//! already exposes to tapes. Legacy IRQs only acknowledge and defer work to
//! foreground polling; it does not add link management, checksum offload, or
//! a general descriptor allocator.

use core::cell::Cell;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use core::ptr::{read_volatile, write_volatile};

use crate::arch::x86::{inl, inw, outb, outl, outw};
use crate::nic::{Health, WatchdogResult};
use crate::pci;
use crate::pmem;
use crate::println;

const VENDOR_3COM: u16 = 0x10b7;
const DEVICE_IDS: &[u16] = &[0x9050, 0x9051, 0x9054, 0x9055, 0x9058, 0x905a, 0x9200];

const CMD_STATUS: u16 = 0x0e;
const EEPROM_COMMAND: u16 = 0x0a;
const EEPROM_DATA: u16 = 0x0c;
const PKT_STATUS: u16 = 0x20;
const DOWN_LIST_PTR: u16 = 0x24;
const UP_LIST_PTR: u16 = 0x38;
const INTERNAL_CONFIG: u16 = 0x00;
const MAX_PACKET_SIZE: u16 = 0x04;
const MEDIA_OPTIONS: u16 = 0x08;
const MEDIA_STATUS: u16 = 0x0a;

const TOTAL_RESET: u16 = 0 << 11;
const RESET_NETWORK: u16 = 1 << 2;
const SELECT_WINDOW: u16 = 1 << 11;
const RX_ENABLE: u16 = 4 << 11;
const RX_RESET: u16 = 5 << 11;
const UP_UNSTALL: u16 = (6 << 11) + 1;
const TX_ENABLE: u16 = 9 << 11;
const TX_RESET: u16 = 11 << 11;
const ACK_INTERRUPT: u16 = 13 << 11;
const SET_STATUS_ENABLE: u16 = 15 << 11;
const SET_RX_FILTER: u16 = 16 << 11;
const SET_RX_THRESHOLD: u16 = 17 << 11;
const DOWN_UNSTALL: u16 = (6 << 11) + 3;
const SET_INTERRUPT_ENABLE: u16 = 14 << 11;

const RX_STATION: u16 = 1;
const RX_BROADCAST: u16 = 4;
const RX_PROMISCUOUS: u16 = 8;

const LINK_10BASE_T: u32 = 0;
const LINK_AUI: u32 = 1;
const LINK_10BASE_2: u32 = 3;
const LINK_100BASE_FX: u32 = 5;
const LINK_MII: u32 = 6;
const LINK_AUTONEGOTIATE: u32 = 8;
const MEDIA_LINK_BEAT: u16 = 1 << 11;

const RX_COMPLETE: u32 = 1 << 15;
const RX_ERROR: u32 = 1 << 14;
const LAST_FRAGMENT: u32 = 1 << 31;
const TX_INDICATE: u32 = 1 << 15;
const DOWNLOAD_COMPLETE: u32 = 1 << 16;
const TX_ROUNDUP_DEFEAT: u32 = 1 << 28;
const EEPROM_READ: u16 = 0x80;
const EEPROM_BUSY: u16 = 0x8000;
const STATUS_INT_LATCH: u16 = 1 << 0;
const STATUS_HOST_ERROR: u16 = 1 << 1;
/// A bus-master receive finished (the card's UpComplete event).
const STATUS_UP_COMPLETE: u16 = 1 << 10;
/// The only events this driver enables, both as *indications* (`SET_STATUS_
/// ENABLE`) and as *interrupt sources* (`SET_INTERRUPT_ENABLE`). On the 3c90x
/// an event must be enabled in both masks before it can raise INT#, and each
/// command takes its mask in its low 13 bits -- the init-time `SET_STATUS_
/// ENABLE` is sent with an empty mask (polling needs no indications), so
/// `enable_interrupts` must send both masks itself; until it did, the card
/// never asserted its interrupt line at all (found on real hardware: zero
/// interrupts despite a registered IRQ). TxComplete, TxAvailable and StatsFull
/// are deliberately left out: TxComplete stays asserted until the transmit
/// status is popped, which this polling driver never does, so enabling it
/// would hold the level-triggered line high and storm.
const INTERRUPT_MASK: u16 = STATUS_UP_COMPLETE | STATUS_HOST_ERROR;

/// Receive descriptors. This used to be 4, which at a 3 ms inter-frame pace
/// holds only ~12 ms of arrivals: the console output that `tape load net`
/// prints when a transfer starts (VGA plus a 115200-baud serial mirror) blocks
/// the foreground longer than that, and the ring overflowed -- "Gap in tape
/// data: expected chunk #5, got #7" a few frames into every large transfer
/// (confirmed on real hardware with interrupts off, so not an interrupt bug).
/// 16 descriptors buffered ~48 ms at that pace, but only ~1 ms at gigabit-class
/// load (a tape forwarding between NICs), so the ring is now 64 descriptors:
/// still inside the one descriptor page, with the buffers as one contiguous
/// run of pages (`rx_buffer_base + index * 4096`) rather than an array in
/// the kernel's tight `.bss`.
const RX_DESC_COUNT: usize = 64;
const RX_BUFFER_SIZE: usize = 1536;
pub const MIN_FRAME_LEN: usize = 60;
pub const MAX_FRAME_LEN: usize = 1514;

#[repr(C)]
#[derive(Clone, Copy)]
struct RxDescriptor {
    next: u32,
    status: u32,
    address: u32,
    length: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct TxDescriptor {
    next: u32,
    status: u32,
    address: u32,
    length: u32,
}

fn read16(base: u16, offset: u16) -> u16 {
    unsafe { inw(base + offset) }
}

fn write16(base: u16, offset: u16, value: u16) {
    unsafe { outw(base + offset, value) }
}

fn write32(base: u16, offset: u16, value: u32) {
    unsafe { outl(base + offset, value) }
}

fn read32(base: u16, offset: u16) -> u32 {
    unsafe { inl(base + offset) }
}

fn wait_command(base: u16) -> Result<(), InitError> {
    let mut attempts = 100_000u32;
    while read16(base, CMD_STATUS) & 0x1000 != 0 {
        attempts -= 1;
        if attempts == 0 {
            return Err(InitError::CommandTimedOut);
        }
    }
    Ok(())
}

/// Issues one command-register write only once the previous command has
/// finished (`CmdInProgress`, bit 12, clear). The 3c90x ignores or garbles a
/// command written while another is still running, and with interrupts live
/// three places write that register -- foreground `receive` (`UP_UNSTALL`),
/// the interrupt handler (acknowledges) and the init/recovery commands -- so
/// an unlucky overlap silently lost an `UP_UNSTALL`, the upload engine stayed
/// stalled and tape transfers aborted with "Gap in tape data" (found on real
/// hardware the moment interrupts started working). Waiting *before* the write
/// serialises them from either side. The bound (a few ms of port reads) keeps
/// the interrupt-handler use from ever hanging; ordinary commands finish in
/// microseconds, only the resets run long.
fn issue_command(base: u16, command: u16) {
    let mut attempts = 5_000u32;
    while read16(base, CMD_STATUS) & 0x1000 != 0 && attempts > 0 {
        attempts -= 1;
    }
    write16(base, CMD_STATUS, command);
}

fn descriptor_ptr<T>(base: u32, index: usize) -> *mut T {
    (base as usize + index * 16) as *mut T
}

pub struct ThreeCom {
    io_base: u16,
    b_revision: bool,
    mac: Cell<[u8; 6]>,
    descriptors: u32,
    rx_buffer_base: u32,
    tx_buffer: u32,
    rx_next: Cell<usize>,
    /// A frame was handed to the download engine and its completion has not been
    /// collected yet (`send` is asynchronous: it checks at the next call).
    tx_in_flight: Cell<bool>,
    overflow_count: Cell<u32>,
    rx_progress: Cell<u32>,
    tx_progress: Cell<u32>,
    recovery_count: Cell<u32>,
    recovery_failures: Cell<u32>,
    recovery_pending: AtomicBool,
    promiscuous: Cell<bool>,
    interrupts_enabled: Cell<bool>,
    irq: Option<u8>,
    interrupt_count: AtomicU32,
    interrupt_errors: AtomicU32,
    work_pending: AtomicBool,
}

struct ProbeInfo {
    io_base: u16,
    irq: Option<u8>,
    b_revision: bool,
}

#[derive(Debug)]
pub enum InitError {
    OutOfMemory,
    ResetTimedOut,
    CommandTimedOut,
    EepromTimedOut,
    InvalidMac,
}

impl ThreeCom {
    /// Finds every supported 3C905/B/C function with a usable I/O BAR.
    /// Cyclone and Tornado cards commonly expose the register window at
    /// BAR1, while some BIOSes leave a usable BAR0.
    fn probe_all(out: &mut [ProbeInfo]) -> usize {
        let mut addresses = [None; pci::MAX_DEVICE_MATCHES];
        let found = pci::find_devices(VENDOR_3COM, DEVICE_IDS, &mut addresses);
        let mut count = 0;
        for address in addresses[..found.min(addresses.len())].iter().flatten() {
            address.enable_io_and_bus_master();
            if let Some(io_base) = address.io_bar(1).or_else(|| address.io_bar(0)) {
                if count == out.len() {
                    break;
                }
                out[count] = ProbeInfo {
                    io_base,
                    irq: address.interrupt_line(),
                    b_revision: matches!(
                        address.device_id(),
                        0x9054 | 0x9055 | 0x9058 | 0x905a | 0x9200
                    ),
                };
                count += 1;
            }
        }
        count
    }

    fn init(io_base: u16, b_revision: bool, irq: Option<u8>) -> Result<Self, InitError> {
        Self::reset(io_base)?;
        let descriptors = pmem::allocate_contiguous(1).ok_or(InitError::OutOfMemory)?;
        let rx_buffer_base =
            pmem::allocate_contiguous(RX_DESC_COUNT).ok_or(InitError::OutOfMemory)?;
        let tx_buffer = pmem::allocate_contiguous(1).ok_or(InitError::OutOfMemory)?;
        let nic = Self {
            io_base,
            b_revision,
            mac: Cell::new([0; 6]),
            descriptors,
            rx_buffer_base,
            tx_buffer,
            rx_next: Cell::new(0),
            tx_in_flight: Cell::new(false),
            overflow_count: Cell::new(0),
            rx_progress: Cell::new(0),
            tx_progress: Cell::new(0),
            recovery_count: Cell::new(0),
            recovery_failures: Cell::new(0),
            recovery_pending: AtomicBool::new(false),
            promiscuous: Cell::new(false),
            interrupts_enabled: Cell::new(false),
            irq,
            interrupt_count: AtomicU32::new(0),
            interrupt_errors: AtomicU32::new(0),
            work_pending: AtomicBool::new(false),
        };
        nic.configure()?;
        Ok(nic)
    }

    fn reset(io_base: u16) -> Result<(), InitError> {
        write16(io_base, CMD_STATUS, TOTAL_RESET | RESET_NETWORK);
        wait_command(io_base).map_err(|error| match error {
            InitError::CommandTimedOut => InitError::ResetTimedOut,
            other => other,
        })
    }

    fn command(&self, command: u16) -> Result<(), InitError> {
        issue_command(self.io_base, command);
        wait_command(self.io_base)
    }

    fn select_window(&self, window: u16) {
        issue_command(self.io_base, SELECT_WINDOW | window);
    }

    fn configure_media(&self) -> Result<(), InitError> {
        self.select_window(3);
        let mut options = read16(self.io_base, MEDIA_OPTIONS);
        if !self.b_revision {
            options &= 0x7f;
        }
        let link_type = if options & 0x0a == 0x0a
            || options & 0x0a == 0x02
            || options & 0x0a == 0x08
        {
            LINK_AUTONEGOTIATE
        } else if options & 0x40 != 0 {
            LINK_MII
        } else if options & 0x04 != 0 {
            LINK_100BASE_FX
        } else if options & 0x10 != 0 {
            LINK_10BASE_2
        } else if options & 0x20 != 0 {
            LINK_AUI
        } else {
            LINK_10BASE_T
        };
        let link_type = if self.b_revision {
            let user_config = self.read_eeprom(0x16)?;
            if user_config & 0xff00 == 0x5a00 {
                u32::from(user_config & 0x000f)
            } else {
                link_type
            }
        } else {
            link_type
        };
        let config =
            (read32(self.io_base, INTERNAL_CONFIG) & !(0x0f << 20)) | (link_type << 20);
        write32(self.io_base, INTERNAL_CONFIG, config);
        Ok(())
    }

    fn configure(&self) -> Result<(), InitError> {
        self.tx_in_flight.set(false);
        self.select_window(0);
        let mut mac = [0u8; 6];
        for (index, byte) in mac.chunks_exact_mut(2).enumerate() {
            let value = self.read_eeprom(10 + index as u16)?;
            byte.copy_from_slice(&value.to_be_bytes());
        }
        if mac.iter().all(|&byte| byte == 0) || mac == [0xff; 6] {
            return Err(InitError::InvalidMac);
        }
        self.mac.set(mac);

        self.select_window(2);
        for (index, byte) in mac.iter().enumerate() {
            unsafe { outb(self.io_base + index as u16, *byte) };
        }
        for offset in (6..12).step_by(2) {
            write16(self.io_base, offset, 0);
        }
        self.configure_media()?;

        let rx_desc = self.descriptors as usize as *mut RxDescriptor;
        for index in 0..RX_DESC_COUNT {
            let next = if index + 1 == RX_DESC_COUNT {
                self.descriptors
            } else {
                self.descriptors + ((index + 1) * 16) as u32
            };
            unsafe {
                write_volatile(
                    rx_desc.add(index),
                    RxDescriptor {
                        next,
                        status: 0,
                        address: self.rx_buffer(index),
                        length: RX_BUFFER_SIZE as u32 | LAST_FRAGMENT,
                    },
                );
            }
        }
        let tx_desc = descriptor_ptr::<TxDescriptor>(self.descriptors, RX_DESC_COUNT);
        unsafe {
            write_volatile(
                tx_desc,
                TxDescriptor {
                    next: 0,
                    status: 0,
                    address: self.tx_buffer,
                    length: 0,
                },
            );
        }

        self.command(TX_RESET)?;
        self.command(RX_RESET | 0x04)?;

        self.select_window(7);
        write32(self.io_base, PKT_STATUS, 0x20);
        write32(self.io_base, UP_LIST_PTR, self.descriptors);
        write32(self.io_base, DOWN_LIST_PTR, 0);

        self.select_window(3);
        write16(self.io_base, MAX_PACKET_SIZE, RX_BUFFER_SIZE as u16);
        self.command(SET_STATUS_ENABLE)?;
        self.command(SET_RX_THRESHOLD | (RX_BUFFER_SIZE as u16 / 4))?;
        self.command(
            SET_RX_FILTER | RX_STATION | RX_BROADCAST
                | if self.promiscuous.get() { RX_PROMISCUOUS } else { 0 },
        )?;
        self.command(RX_ENABLE)?;
        // RX_ENABLE accepts frames at the MAC, but the Cyclone/Tornado DMA
        // upload engine can remain stalled after RX_RESET. The upload list is
        // already posted above; explicitly resume it so the first broadcast
        // (such as a DHCP DISCOVER) reaches its descriptor instead of waiting
        // for a receive completion that can never occur.
        self.command(UP_UNSTALL)?;
        self.command(TX_ENABLE)?;
        self.command(ACK_INTERRUPT | 0x07ff)?;
        Ok(())
    }

    fn read_eeprom(&self, address: u16) -> Result<u16, InitError> {
        self.select_window(0);
        write16(self.io_base, EEPROM_COMMAND, EEPROM_READ | address);
        let mut attempts = 100_000u32;
        while read16(self.io_base, EEPROM_COMMAND) & EEPROM_BUSY != 0 {
            attempts -= 1;
            if attempts == 0 {
                return Err(InitError::EepromTimedOut);
            }
        }
        Ok(read16(self.io_base, EEPROM_DATA))
    }

    pub fn mac_address(&self) -> [u8; 6] {
        self.mac.get()
    }

    /// Reads the 3Com media-status link-beat bit. This is intentionally a
    /// small diagnostic rather than a complete MII link-state abstraction.
    pub fn link_up(&self) -> bool {
        self.select_window(4);
        read16(self.io_base, MEDIA_STATUS) & MEDIA_LINK_BEAT != 0
    }

    pub fn set_promiscuous(&self, enabled: bool) {
        self.select_window(1);
        let filter = RX_STATION | RX_BROADCAST | if enabled { RX_PROMISCUOUS } else { 0 };
        let _ = self.command(SET_RX_FILTER | filter);
        self.promiscuous.set(enabled);
    }

    pub fn overflow_count(&self) -> u32 {
        self.overflow_count.get()
    }

    pub fn health(&self) -> Health {
        Health {
            rx_progress: self.rx_progress.get(),
            tx_progress: self.tx_progress.get(),
            recoveries: self.recovery_count.get(),
            recovery_failures: self.recovery_failures.get(),
            recovery_pending: self.recovery_pending.load(Ordering::Acquire),
            rx_overruns: 0, // the card gives no overrun indication
        }
    }

    /// The 3C905 Host Error interrupt and a bounded download timeout are
    /// documented adapter failures. Reset and rebuild the existing list
    /// in place; this never allocates, frees, or moves DMA memory.
    pub fn watchdog(&self) -> WatchdogResult {
        if !self.recovery_pending.swap(false, Ordering::AcqRel) {
            return WatchdogResult::Healthy;
        }

        if Self::reset(self.io_base).is_err() || self.configure().is_err() {
            self.recovery_failures.set(self.recovery_failures.get().wrapping_add(1));
            return WatchdogResult::Failed;
        }
        self.rx_next.set(0);
        if self.interrupts_enabled.get() {
            self.enable_interrupts();
        }
        self.recovery_count.set(self.recovery_count.get().wrapping_add(1));
        WatchdogResult::Recovered
    }

    pub fn enable_interrupts(&self) {
        let _ = self.command(ACK_INTERRUPT | 0x07ff | STATUS_INT_LATCH);
        let _ = self.command(SET_STATUS_ENABLE | INTERRUPT_MASK);
        let _ = self.command(SET_INTERRUPT_ENABLE | INTERRUPT_MASK);
        self.interrupts_enabled.set(true);
    }

    /// Physical address of receive buffer `index` (one 4 KiB page each).
    fn rx_buffer(&self, index: usize) -> u32 {
        self.rx_buffer_base + (index * 4096) as u32
    }

    pub fn handle_interrupt(&self) {
        let status = read16(self.io_base, CMD_STATUS) & INTERRUPT_MASK;
        if status == 0 {
            return;
        }
        // Command completion can wait, but an ISR must only acknowledge and
        // return; the card's command-in-progress bit is therefore not polled.
        issue_command(self.io_base, ACK_INTERRUPT | status | STATUS_INT_LATCH);
        self.interrupt_count.fetch_add(1, Ordering::Relaxed);
        if status & STATUS_HOST_ERROR != 0 {
            self.interrupt_errors.fetch_add(1, Ordering::Relaxed);
            self.recovery_pending.store(true, Ordering::Release);
        }
        self.work_pending.store(true, Ordering::Release);
    }

    /// The raw `IntStatus` register, unmodified, for `nic list`'s diagnostics:
    /// bit 0 is IntLatch (an enabled event is asserting the interrupt line),
    /// bit 4 RxComplete, bit 10 UpComplete (a bus-master receive finished).
    pub fn raw_status(&self) -> u16 {
        read16(self.io_base, CMD_STATUS)
    }

    /// `nic regs`: status register and receive-ring state.
    pub fn dump_registers(&self) {
        let mut complete = 0u32;
        for index in 0..RX_DESC_COUNT {
            let descriptor = descriptor_ptr::<RxDescriptor>(self.descriptors, index);
            if unsafe { read_volatile(&(*descriptor).status) } & RX_COMPLETE != 0 {
                complete += 1;
            }
        }
        println!(
            "3C905 I/O 0x{:04x}: status {:04x} (window {}), interrupts {}",
            self.io_base,
            read16(self.io_base, CMD_STATUS),
            read16(self.io_base, CMD_STATUS) >> 13,
            if self.interrupts_enabled.get() { "on" } else { "off" }
        );
        println!(
            "rx next {} ({complete} of {RX_DESC_COUNT} descriptors completed), overflow {}, recoveries {}",
            self.rx_next.get(),
            self.overflow_count.get(),
            self.recovery_count.get()
        );
    }

    pub fn interrupt_stats(&self) -> (Option<u8>, u32, u32, bool) {
        (
            self.irq,
            self.interrupt_count.load(Ordering::Relaxed),
            self.interrupt_errors.load(Ordering::Relaxed),
            self.work_pending.load(Ordering::Acquire),
        )
    }

    pub fn send(&self, frame: &[u8]) -> bool {
        if frame.len() > MAX_FRAME_LEN {
            return false;
        }
        // This driver owns one download descriptor. It cannot be rewritten
        // while the card still owns the previous packet, so first collect that
        // packet's completion -- which is normally long past, since the last
        // `send` returned as soon as it had started the transfer.
        let tx_descriptor = descriptor_ptr::<TxDescriptor>(self.descriptors, RX_DESC_COUNT);
        if self.tx_in_flight.get() {
            let mut attempts = 100_000u32;
            while unsafe { read_volatile(&(*tx_descriptor).status) } & DOWNLOAD_COMPLETE == 0 {
                attempts -= 1;
                if attempts == 0 {
                    issue_command(self.io_base, TX_RESET);
                    issue_command(self.io_base, TX_ENABLE);
                    self.tx_in_flight.set(false);
                    self.recovery_pending.store(true, Ordering::Release);
                    return false;
                }
            }
            unsafe { write_volatile(&mut (*tx_descriptor).status, 0) };
            self.tx_in_flight.set(false);
        }
        if read32(self.io_base, DOWN_LIST_PTR) != 0 {
            return false;
        }
        let padded_len = frame.len().max(MIN_FRAME_LEN);
        unsafe {
            let buffer = core::slice::from_raw_parts_mut(self.tx_buffer as *mut u8, MAX_FRAME_LEN);
            buffer[..frame.len()].copy_from_slice(frame);
            buffer[frame.len()..padded_len].fill(0);
            let descriptor = descriptor_ptr::<TxDescriptor>(self.descriptors, RX_DESC_COUNT);
            let frame_start = if self.b_revision {
                // 3C905B/C cards use the B-revision frame-start header:
                // request completion indication and disable hardware
                // packet-length rounding. The payload length belongs in the
                // final-fragment field below.
                TX_INDICATE | TX_ROUNDUP_DEFEAT
            } else {
                padded_len as u32
            };
            write_volatile(&mut (*descriptor).status, frame_start);
            write_volatile(&mut (*descriptor).length, padded_len as u32 | LAST_FRAGMENT);
        }
        write32(
            self.io_base,
            DOWN_LIST_PTR,
            self.descriptors + (RX_DESC_COUNT * 16) as u32,
        );
        issue_command(self.io_base, DOWN_UNSTALL);
        self.tx_in_flight.set(true);
        self.tx_progress.set(self.tx_progress.get().wrapping_add(1));
        true
    }

    pub fn receive(&self, out: &mut [u8]) -> Option<usize> {
        self.work_pending.store(false, Ordering::Release);
        let index = self.rx_next.get();
        let descriptor = descriptor_ptr::<RxDescriptor>(self.descriptors, index);
        let status = unsafe { read_volatile(&(*descriptor).status) };
        if status & RX_COMPLETE == 0 {
            return None;
        }
        let length = (status & 0x1fff) as usize;
        let valid = status & RX_ERROR == 0 && length <= RX_BUFFER_SIZE;
        let copy_len = length.min(out.len()).min(MAX_FRAME_LEN);
        // Returning a completed descriptor makes its DMA buffer available to
        // the adapter immediately. Copy before clearing its status so a new
        // wire packet cannot overwrite the frame the foreground protocol is
        // about to parse (especially visible for a short DHCP exchange).
        if valid {
            unsafe {
                let source = core::slice::from_raw_parts(self.rx_buffer(index) as *const u8, copy_len);
                out[..copy_len].copy_from_slice(source);
            }
        }
        unsafe { write_volatile(&mut (*descriptor).status, 0) };
        issue_command(self.io_base, UP_UNSTALL);
        self.rx_next.set((index + 1) % RX_DESC_COUNT);
        if !valid {
            self.overflow_count.set(self.overflow_count.get() + 1);
            return None;
        }
        self.rx_progress.set(self.rx_progress.get().wrapping_add(1));
        Some(copy_len)
    }
}

const MAX_INSTANCES: usize = pci::MAX_DEVICE_MATCHES;
static mut NICS: [Option<ThreeCom>; MAX_INSTANCES] = [const { None }; MAX_INSTANCES];
static mut NIC_COUNT: usize = 0;
static mut INITIALIZED: bool = false;

#[derive(Debug)]
pub enum Status {
    NotPresent,
    InitFailed,
}

pub fn initialize_all() -> Result<usize, Status> {
    unsafe {
        if !INITIALIZED {
            INITIALIZED = true;
            let mut probe_info = [const {
                ProbeInfo {
                    io_base: 0,
                    irq: None,
                    b_revision: false,
                }
            }; MAX_INSTANCES];
            let found = ThreeCom::probe_all(&mut probe_info);
            let mut first_error = None;
            for info in &probe_info[..found] {
                match ThreeCom::init(info.io_base, info.b_revision, info.irq) {
                    Ok(initialized) => {
                        NICS[NIC_COUNT] = Some(initialized);
                        NIC_COUNT += 1;
                    }
                    Err(error) if first_error.is_none() => first_error = Some(error),
                    Err(_) => {}
                }
            }
            if NIC_COUNT == 0 {
                return                 if first_error.is_some() {
                    Err(Status::InitFailed)
                } else {
                    Err(Status::NotPresent)
                };
            }
        }
        Ok(NIC_COUNT)
    }
}

pub fn get(index: usize) -> Option<&'static ThreeCom> {
    unsafe {
        if index >= NIC_COUNT {
            return None;
        }
        NICS[index].as_ref()
    }
}
