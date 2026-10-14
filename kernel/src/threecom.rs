//! A driver for 3Com 3C905/3C905B/3C905C PCI Ethernet cards ("Cyclone"/"Tornado", `10b7:9050`-`10b7:9200`), ported from v1's
//! `threecom.rs`, which was confirmed on real cards (three of them in the AMD/nForce test machine).
//!
//! How the card works, briefly. Registers sit in an I/O-port window (BAR1 on most cards, BAR0 on some), and most of them are
//! "windowed": a `SelectWindow` command picks which bank the low 16 bytes show. Only the command/status register at offset
//! 0x0e is always visible. Frames move by bus-master DMA through linked lists of descriptors in host RAM: the *upload* list
//! (card to host, receive) is a ring of descriptors whose status word gets the `UpComplete` bit when a frame is in, and the
//! *download* list (host to card, transmit) is a list this driver keeps to a single descriptor. The card's DMA addresses are
//! 32 bits, so every buffer is taken from memory below 4 GiB.
//!
//! Hard-won rules carried over from v1 (all found on real hardware, none visible in QEMU, which has no such card):
//! * A command written while the previous one is still running (`CmdInProgress`, bit 12) is ignored or garbled, and three
//!   places write the command register (foreground receive, the interrupt handler, init/recovery), so every write goes
//!   through `issue_command`, which waits first.
//! * After `RX_RESET` the upload engine can stay stalled: `UP_UNSTALL` must be sent after the list is posted, and after
//!   every received descriptor is handed back.
//! * An event raises INT# only if it is enabled in *both* `SET_STATUS_ENABLE` and `SET_INTERRUPT_ENABLE`. `TxComplete` is
//!   left out: it stays asserted until the transmit status is popped, which this driver never does, so it would storm the
//!   level-triggered line.
//! * 16 receive descriptors were not enough (console output blocks the foreground for longer than they buffer): 64.
//!
//! Interrupts only wake the CPU (as on the other NICs): the foreground always polls the ring. The card has no MSI, so legacy
//! INTx through the I/O APIC is tried (candidate pins probed, as for the RTL8168) and kept only if a self-test interrupt,
//! raised by the card's `RequestInterrupt` command, arrives.

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, Ordering};

use crate::e1000::{Delivery, ISR, MAX_INTERFACES};
use crate::{apic, cpu, memory, pci};

pub const VENDOR_3COM: u16 = 0x10b7;
/// 3C905 (Boomerang) 9050/9051, 3C905B 9055 and its variants, 3C905C 9200.
pub const DEVICE_IDS: &[u16] = &[0x9050, 0x9051, 0x9054, 0x9055, 0x9058, 0x905a, 0x9200];

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
const DOWN_UNSTALL: u16 = (6 << 11) + 3;
const TX_ENABLE: u16 = 9 << 11;
const TX_RESET: u16 = 11 << 11;
const REQUEST_INTERRUPT: u16 = 12 << 11;
const ACK_INTERRUPT: u16 = 13 << 11;
const SET_INTERRUPT_ENABLE: u16 = 14 << 11;
const SET_STATUS_ENABLE: u16 = 15 << 11;
const SET_RX_FILTER: u16 = 16 << 11;
const SET_RX_THRESHOLD: u16 = 17 << 11;

const RX_STATION: u16 = 1;
const RX_MULTICAST: u16 = 2;
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
const DOWNLOAD_COMPLETE: u32 = 1 << 16;
const TX_ROUNDUP_DEFEAT: u32 = 1 << 28;
const EEPROM_READ: u16 = 0x80;
const EEPROM_BUSY: u16 = 0x8000;
const STATUS_INT_LATCH: u16 = 1 << 0;
const STATUS_HOST_ERROR: u16 = 1 << 1;
const STATUS_TX_COMPLETE: u16 = 1 << 2;
const STATUS_INT_REQUESTED: u16 = 1 << 6;
const STATUS_UP_COMPLETE: u16 = 1 << 10;
const INTERRUPT_MASK: u16 = STATUS_UP_COMPLETE | STATUS_HOST_ERROR;

const RX_COUNT: usize = 64;
const RX_BUFFER_SIZE: usize = 1536;
const MIN_FRAME: usize = 60;
pub const MAX_FRAME: usize = 1514;

#[repr(C)]
#[derive(Clone, Copy)]
struct Descriptor {
    next: u32,
    status: u32,
    address: u32,
    length: u32,
}

#[derive(Debug)]
pub enum Error {
    NoIoBar,
    NotResponding,
    OutOfMemory,
    ResetTimedOut,
    CommandTimedOut,
    EepromTimedOut,
    InvalidMac,
}

fn read16(base: u16, offset: u16) -> u16 {
    cpu::inw(base + offset)
}
fn write16(base: u16, offset: u16, value: u16) {
    cpu::outw(base + offset, value)
}
fn read32(base: u16, offset: u16) -> u32 {
    cpu::inl(base + offset)
}
fn write32(base: u16, offset: u16, value: u32) {
    cpu::outl(base + offset, value)
}

/// Waits (bounded) for the previous command to finish, then writes the next: see the rules above.
fn issue_command(base: u16, command: u16) {
    let mut attempts = 5_000u32;
    while read16(base, CMD_STATUS) & 0x1000 != 0 && attempts > 0 {
        attempts -= 1;
    }
    write16(base, CMD_STATUS, command);
}

fn wait_command(base: u16) -> Result<(), Error> {
    for _ in 0..100_000u32 {
        if read16(base, CMD_STATUS) & 0x1000 == 0 {
            return Ok(());
        }
    }
    Err(Error::CommandTimedOut)
}

fn descriptor(base: u64, index: usize) -> *mut Descriptor {
    (base as usize + index * 16) as *mut Descriptor
}

/// Per-interface state the interrupt handler needs.
static IO_BASE: [core::sync::atomic::AtomicU16; MAX_INTERFACES] = [const { core::sync::atomic::AtomicU16::new(0) }; MAX_INTERFACES];
static FATAL: [AtomicBool; MAX_INTERFACES] = [const { AtomicBool::new(false) }; MAX_INTERFACES];
/// Host (PCI bus) errors the card reported, for `nic`.
static HOST_ERRORS: [core::sync::atomic::AtomicU32; MAX_INTERFACES] = [const { core::sync::atomic::AtomicU32::new(0) }; MAX_INTERFACES];

/// The interrupt handler: acknowledge what the card says, note a host error for the foreground to recover from, and return.
fn isr(index: usize) {
    let base = IO_BASE[index].load(Ordering::Relaxed);
    if base == 0 {
        return;
    }
    let status = read16(base, CMD_STATUS) & (INTERRUPT_MASK | STATUS_INT_REQUESTED);
    if status == 0 {
        return;
    }
    // Commands are not waited for to completion here, but the write itself must not land on a running one.
    issue_command(base, ACK_INTERRUPT | status | STATUS_INT_LATCH);
    crate::msi::claim();
    ISR[index].interrupts.fetch_add(1, Ordering::Relaxed);
    if status & STATUS_HOST_ERROR != 0 {
        FATAL[index].store(true, Ordering::Release);
        HOST_ERRORS[index].fetch_add(1, Ordering::Relaxed);
    }
}

pub struct ThreeCom {
    index: usize,
    pub delivery: Delivery,
    interrupts_enabled: bool,
    io_base: u16,
    b_revision: bool,
    pub mac: [u8; 6],
    descriptors: u64,
    rx_buffers: u64,
    tx_buffer: u64,
    rx_next: usize,
    /// A frame was handed to the download engine and its completion has not been collected (`send` is asynchronous).
    tx_in_flight: bool,
    promiscuous: bool,
    multicast: bool,
    pub frames_received: u64,
    pub frames_sent: u64,
    pub frames_dropped_no_link: u64,
    pub bad_frames: u32,
    pub recoveries: u32,
    /// Sends abandoned because the card did not finish the previous frame in time.
    pub tx_timeouts: u32,
    /// Frames dropped because the card was still busy with the previous one.
    pub tx_busy_drops: u32,
    /// TxStatus entries that reported an error (collisions, underrun, jabber, overflow).
    pub tx_status_errors: u32,
    /// What the transmit descriptor and the download list pointer held at the last send timeout (for `nic`).
    last_timeout: (u32, u32, u16),
    /// DMACtrl and TxStatus at that moment.
    last_timeout_dma: (u32, u8),
    /// Times a stalled download engine was woken again by a repeated `DownUnstall`.
    pub unstall_retries: u32,
    recovery_pending: bool,
}

impl ThreeCom {
    pub fn init(device: &pci::Device, index: usize) -> Result<ThreeCom, Error> {
        device.force_power_state_d0();
        let command = device.read32(4);
        device.write32(4, (command & 0xffff) | 0x5); // I/O decoding and bus mastering
        // The register window is BAR1 on most cards and BAR0 on some.
        let io_base = device.io_bar(1).or_else(|| device.io_bar(0)).ok_or(Error::NoIoBar)?;
        if read16(io_base, CMD_STATUS) == 0xffff {
            return Err(Error::NotResponding);
        }
        let b_revision = matches!(device.id, 0x9054 | 0x9055 | 0x9058 | 0x905a | 0x9200);
        write16(io_base, CMD_STATUS, TOTAL_RESET | RESET_NETWORK);
        wait_command(io_base).map_err(|_| Error::ResetTimedOut)?;
        let below = Some(1u64 << 32); // 32-bit DMA addresses
        let descriptors = memory::allocate_contiguous(1, below).ok_or(Error::OutOfMemory)?;
        let rx_buffers = memory::allocate_contiguous(RX_COUNT as u64, below).ok_or(Error::OutOfMemory)?;
        let tx_buffer = memory::allocate_contiguous(1, below).ok_or(Error::OutOfMemory)?;
        let mut nic = ThreeCom {
            index,
            delivery: Delivery::Polled,
            interrupts_enabled: false,
            io_base,
            b_revision,
            mac: [0; 6],
            descriptors,
            rx_buffers,
            tx_buffer,
            rx_next: 0,
            tx_in_flight: false,
            promiscuous: false,
            multicast: false,
            frames_received: 0,
            frames_sent: 0,
            frames_dropped_no_link: 0,
            bad_frames: 0,
            recoveries: 0,
            tx_timeouts: 0,
            tx_busy_drops: 0,
            tx_status_errors: 0,
            last_timeout: (0, 0, 0),
            last_timeout_dma: (0, 0),
            unstall_retries: 0,
            recovery_pending: false,
        };
        nic.configure()?;
        IO_BASE[index].store(io_base, Ordering::Relaxed);
        Ok(nic)
    }

    pub fn io_base(&self) -> u16 {
        self.io_base
    }

    fn command(&self, command: u16) -> Result<(), Error> {
        issue_command(self.io_base, command);
        wait_command(self.io_base)
    }

    fn select_window(&self, window: u16) {
        issue_command(self.io_base, SELECT_WINDOW | window);
    }

    fn read_eeprom(&self, address: u16) -> Result<u16, Error> {
        self.select_window(0);
        write16(self.io_base, EEPROM_COMMAND, EEPROM_READ | address);
        for _ in 0..100_000u32 {
            if read16(self.io_base, EEPROM_COMMAND) & EEPROM_BUSY == 0 {
                return Ok(read16(self.io_base, EEPROM_DATA));
            }
        }
        Err(Error::EepromTimedOut)
    }

    /// Picks the media (twisted pair with autonegotiation on the cards we know) the way the 3Com driver does: from the
    /// media options in window 3, overridden by the user configuration word in the EEPROM on B/C revisions.
    fn configure_media(&self) -> Result<(), Error> {
        self.select_window(3);
        let mut options = read16(self.io_base, MEDIA_OPTIONS);
        if !self.b_revision {
            options &= 0x7f;
        }
        let link_type = if options & 0x0a == 0x0a || options & 0x0a == 0x02 || options & 0x0a == 0x08 {
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
            self.select_window(3);
            if user_config & 0xff00 == 0x5a00 { u32::from(user_config & 0x000f) } else { link_type }
        } else {
            link_type
        };
        let config = (read32(self.io_base, INTERNAL_CONFIG) & !(0x0f << 20)) | (link_type << 20);
        write32(self.io_base, INTERNAL_CONFIG, config);
        Ok(())
    }

    /// (Re)builds the rings and programs the card; used at init and after a reset.
    fn configure(&mut self) -> Result<(), Error> {
        self.tx_in_flight = false;
        self.select_window(0);
        let mut mac = [0u8; 6];
        for (index, byte) in mac.chunks_exact_mut(2).enumerate() {
            let value = self.read_eeprom(10 + index as u16)?;
            byte.copy_from_slice(&value.to_be_bytes());
        }
        if mac.iter().all(|&byte| byte == 0) || mac == [0xff; 6] {
            return Err(Error::InvalidMac);
        }
        self.mac = mac;
        self.select_window(2);
        for (index, byte) in mac.iter().enumerate() {
            cpu::outb(self.io_base + index as u16, *byte);
        }
        for offset in (6..12).step_by(2) {
            write16(self.io_base, offset, 0);
        }
        self.configure_media()?;

        // The upload (receive) ring: 64 descriptors linked in a circle, each pointing at one page.
        for index in 0..RX_COUNT {
            let next = if index + 1 == RX_COUNT { self.descriptors } else { self.descriptors + ((index + 1) * 16) as u64 };
            unsafe {
                write_volatile(
                    descriptor(self.descriptors, index),
                    Descriptor { next: next as u32, status: 0, address: self.rx_buffer(index) as u32, length: RX_BUFFER_SIZE as u32 | LAST_FRAGMENT },
                );
            }
        }
        // The single download (transmit) descriptor sits after them.
        unsafe { write_volatile(descriptor(self.descriptors, RX_COUNT), Descriptor { next: 0, status: 0, address: self.tx_buffer as u32, length: 0 }) };

        self.command(TX_RESET)?;
        self.command(RX_RESET | 0x04)?;
        self.select_window(7);
        write32(self.io_base, PKT_STATUS, 0x20);
        write32(self.io_base, UP_LIST_PTR, self.descriptors as u32);
        write32(self.io_base, DOWN_LIST_PTR, 0);
        self.select_window(3);
        write16(self.io_base, MAX_PACKET_SIZE, RX_BUFFER_SIZE as u16);
        self.command(SET_STATUS_ENABLE)?; // polling needs no indications yet
        self.command(SET_RX_THRESHOLD | (RX_BUFFER_SIZE as u16 / 4))?;
        self.command(SET_RX_FILTER | self.filter())?;
        self.command(RX_ENABLE)?;
        // The upload engine can stay stalled after RX_RESET: resume it now that the list is posted.
        self.command(UP_UNSTALL)?;
        self.command(TX_ENABLE)?;
        self.command(ACK_INTERRUPT | 0x07ff)?;
        Ok(())
    }

    fn filter(&self) -> u16 {
        RX_STATION | RX_BROADCAST | if self.multicast { RX_MULTICAST } else { 0 } | if self.promiscuous { RX_PROMISCUOUS } else { 0 }
    }

    /// Physical address of receive buffer `index` (one 4 KiB page each).
    fn rx_buffer(&self, index: usize) -> u64 {
        self.rx_buffers + (index * 4096) as u64
    }

    /// The media-status link-beat bit: a small diagnostic, not a full MII link abstraction.
    pub fn link_up(&self) -> bool {
        self.select_window(4);
        read16(self.io_base, MEDIA_STATUS) & MEDIA_LINK_BEAT != 0
    }

    pub fn set_promiscuous(&mut self, on: bool) {
        self.promiscuous = on;
        self.select_window(1);
        let _ = self.command(SET_RX_FILTER | self.filter());
    }

    pub fn set_multicast(&mut self, on: bool) {
        self.multicast = on;
        self.select_window(1);
        let _ = self.command(SET_RX_FILTER | self.filter());
    }

    /// Raises one interrupt by hand (`RequestInterrupt`) and reports whether `arrived` saw it within three ticks.
    fn self_test(&mut self, arrived: impl Fn() -> u64) -> bool {
        let mask = INTERRUPT_MASK | STATUS_INT_REQUESTED;
        let _ = self.command(ACK_INTERRUPT | 0x07ff | STATUS_INT_LATCH);
        let _ = self.command(SET_STATUS_ENABLE | mask);
        let _ = self.command(SET_INTERRUPT_ENABLE | mask);
        let before = arrived();
        issue_command(self.io_base, REQUEST_INTERRUPT);
        let start = apic::ticks();
        while arrived() == before && apic::ticks().wrapping_sub(start) < 3 {
            core::hint::spin_loop();
        }
        arrived() != before
    }

    fn finish_interrupts(&mut self, delivery: Delivery) -> Delivery {
        self.interrupts_enabled = true;
        self.delivery = delivery;
        // From here only the events the driver wants: a received frame and a host error (see the rules at the top).
        let _ = self.command(SET_STATUS_ENABLE | INTERRUPT_MASK);
        let _ = self.command(SET_INTERRUPT_ENABLE | INTERRUPT_MASK);
        delivery
    }

    /// Switches from polling to legacy interrupts through the I/O APIC, keeping a pin only if the self-test interrupt arrives.
    pub fn enable_interrupts(&mut self, device: &pci::Device) -> Result<Delivery, &'static str> {
        let mut reason = "no I/O APIC pin delivered the self-test interrupt";
        let (first, end) = crate::ioapic::gsi_range();
        let candidates = crate::msi::intx_candidates(device.interrupt_line());
        for (position, &gsi) in candidates.iter().enumerate() {
            if gsi == u32::MAX || gsi == 0 || gsi < first || gsi >= end || candidates[..position].contains(&gsi) {
                continue;
            }
            let Some(vector) = crate::msi::attach_intx(gsi, isr, self.index) else { continue };
            // Count only interrupts this card claimed (its status showed a cause): a pin that floats low "delivers"
            // interrupts on its own, which once passed the self-test and then stormed at 125 kHz (skylake, pin 11).
            let index = self.index;
            if self.self_test(|| ISR[index].interrupts.load(Ordering::Relaxed)) {
                return Ok(self.finish_interrupts(Delivery::Intx { gsi, vector }));
            }
            let _ = self.command(SET_INTERRUPT_ENABLE);
            let _ = self.command(SET_STATUS_ENABLE);
            crate::msi::detach_intx(vector, self.index);
            reason = "no I/O APIC pin delivered the self-test interrupt";
        }
        Err(reason)
    }

    /// Resets the card and rebuilds the rings in place (the memory is reused).
    fn recover(&mut self) -> bool {
        write16(self.io_base, CMD_STATUS, TOTAL_RESET | RESET_NETWORK);
        if wait_command(self.io_base).is_err() || self.configure().is_err() {
            return false;
        }
        self.rx_next = 0;
        FATAL[self.index].store(false, Ordering::Relaxed);
        self.recovery_pending = false;
        if self.interrupts_enabled {
            let _ = self.command(ACK_INTERRUPT | 0x07ff | STATUS_INT_LATCH);
            let _ = self.command(SET_STATUS_ENABLE | INTERRUPT_MASK);
            let _ = self.command(SET_INTERRUPT_ENABLE | INTERRUPT_MASK);
        }
        self.recoveries += 1;
        true
    }

    /// The `nic` command's driver-specific line.
    pub fn describe(&self) {
        crate::println!(
            "   3c905{}: I/O {:#06x}, {} bad frames, {} recoveries ({} host errors, {} send timeouts), {} sends dropped busy, {} transmit errors, status {:#06x}",
            if self.b_revision { "B/C" } else { "" },
            self.io_base,
            self.bad_frames,
            self.recoveries,
            HOST_ERRORS[self.index].load(Ordering::Relaxed),
            self.tx_timeouts,
            self.tx_busy_drops,
            self.tx_status_errors,
            read16(self.io_base, CMD_STATUS)
        );
    }

    /// `nic regs`: the status register, the receive ring and the transmit descriptor.
    pub fn dump_registers(&self) {
        let mut complete = 0u32;
        for index in 0..RX_COUNT {
            if unsafe { read_volatile(&(*descriptor(self.descriptors, index)).status) } & RX_COMPLETE != 0 {
                complete += 1;
            }
        }
        let status = read16(self.io_base, CMD_STATUS);
        crate::println!(
            "last send timeout: descriptor status {:08x}, down list {:08x}, card status {:04x}, DMACtrl {:08x}, TxStatus {:02x}; {} unstall retries",
            self.last_timeout.0, self.last_timeout.1, self.last_timeout.2, self.last_timeout_dma.0, self.last_timeout_dma.1, self.unstall_retries
        );
        crate::println!("3C905 I/O {:#06x}: status {status:04x} (window {}), interrupts {}, {:?}", self.io_base, status >> 13, if self.interrupts_enabled { "on" } else { "off" }, self.delivery);
        crate::println!(
            "rx next {} ({complete} of {RX_COUNT} descriptors completed), bad frames {}, recoveries {}, down list {:08x}, up list {:08x}",
            self.rx_next,
            self.bad_frames,
            self.recoveries,
            read32(self.io_base, DOWN_LIST_PTR),
            read32(self.io_base, UP_LIST_PTR)
        );
    }

    /// Empties the TxStatus stack (window 1, offset 0x1b) if the card says it holds entries, and re-enables the transmitter
    /// as the 3Com drivers do: an error entry (jabber, underrun) needs a transmit reset first, a plain one just `TxEnable`.
    fn service_tx_status(&mut self) {
        if read16(self.io_base, CMD_STATUS) & STATUS_TX_COMPLETE == 0 {
            return;
        }
        self.select_window(1);
        let mut reset = false;
        for _ in 0..40 {
            let status = cpu::inb(self.io_base + 0x1b);
            if status & 0x80 == 0 {
                break;
            }
            cpu::outb(self.io_base + 0x1b, 0); // pop
            self.tx_status_errors += u32::from(status & 0x3c != 0);
            reset |= status & 0x30 != 0;
        }
        if reset {
            issue_command(self.io_base, TX_RESET);
            self.tx_in_flight = false;
        }
        issue_command(self.io_base, TX_ENABLE);
        issue_command(self.io_base, ACK_INTERRUPT | STATUS_TX_COMPLETE | STATUS_INT_LATCH);
    }

    /// Queues one frame (padded to 60 bytes). The card has one download descriptor, so the previous frame's completion is
    /// collected first; a frame sent with no link is dropped, as on the other drivers.
    pub fn send(&mut self, frame: &[u8]) -> bool {
        if frame.len() > MAX_FRAME {
            return false;
        }
        if !self.link_up() {
            self.frames_dropped_no_link += 1;
            return true;
        }
        self.service_tx_status();
        let tx = descriptor(self.descriptors, RX_COUNT);
        if self.tx_in_flight {
            let mut attempts = 100_000u32;
            while unsafe { read_volatile(&(*tx).status) } & DOWNLOAD_COMPLETE == 0 {
                attempts -= 1;
                // The download engine can be left stalled; a repeated DownUnstall costs nothing if it is running.
                if attempts % 10_000 == 0 && attempts > 0 {
                    issue_command(self.io_base, DOWN_UNSTALL);
                    self.unstall_retries += 1;
                }
                if attempts == 0 {
                    self.last_timeout = (unsafe { read_volatile(&(*tx).status) }, read32(self.io_base, DOWN_LIST_PTR), read16(self.io_base, CMD_STATUS));
                    self.select_window(1);
                    self.last_timeout_dma = (read32(self.io_base, PKT_STATUS), cpu::inb(self.io_base + 0x1b));
                    issue_command(self.io_base, TX_RESET);
                    issue_command(self.io_base, TX_ENABLE);
                    self.tx_in_flight = false;
                    self.recovery_pending = true;
                    self.tx_timeouts += 1;
                    return false;
                }
            }
            unsafe { write_volatile(&mut (*tx).status, 0) };
            self.tx_in_flight = false;
        }
        // The card keeps the list pointer until it has the whole frame out: on a 10 Mb/s link that is nearly a millisecond for a
        // 1 KiB frame, so a sender pacing frames a millisecond apart finds it busy. Wait (bounded) instead of dropping.
        let deadline = apic::micros() + 50_000;
        while read32(self.io_base, DOWN_LIST_PTR) != 0 {
            if apic::micros() > deadline {
                self.tx_busy_drops += 1;
                return false;
            }
            core::hint::spin_loop();
        }
        let padded = frame.len().max(MIN_FRAME);
        unsafe {
            let buffer = core::slice::from_raw_parts_mut(self.tx_buffer as *mut u8, MAX_FRAME);
            buffer[..frame.len()].copy_from_slice(frame);
            buffer[frame.len()..padded].fill(0);
            // B/C cards use the B-revision frame start: ask for a completion indication, no length rounding.
            // No `TxIndicate` bit: each frame that asks for it pushes an entry on the card's 31-deep TxStatus stack that nothing
            // here pops, and when the stack overflows (after ~30 frames) the transmitter stops dead. The completion this driver
            // waits for is the DownloadComplete bit the card writes into the descriptor.
            let start = padded as u32 | if self.b_revision { TX_ROUNDUP_DEFEAT } else { 0 };
            write_volatile(&mut (*tx).status, start);
            write_volatile(&mut (*tx).length, padded as u32 | LAST_FRAGMENT);
        }
        write32(self.io_base, DOWN_LIST_PTR, (self.descriptors + (RX_COUNT * 16) as u64) as u32);
        issue_command(self.io_base, DOWN_UNSTALL);
        self.tx_in_flight = true;
        self.frames_sent += 1;
        true
    }

    /// Copies the next received frame into `out`; `None` if the ring is empty (or the frame was bad).
    pub fn receive(&mut self, out: &mut [u8]) -> Option<usize> {
        if self.recovery_pending || FATAL[self.index].swap(false, Ordering::AcqRel) {
            self.recover();
        }
        let index = self.rx_next;
        let slot = descriptor(self.descriptors, index);
        let status = unsafe { read_volatile(&(*slot).status) };
        if status & RX_COMPLETE == 0 {
            return None;
        }
        let length = (status & 0x1fff) as usize;
        let valid = status & RX_ERROR == 0 && length <= RX_BUFFER_SIZE;
        let count = length.min(out.len()).min(MAX_FRAME);
        // Copy before handing the descriptor back: once it is free a new frame may overwrite the buffer.
        if valid {
            let source = unsafe { core::slice::from_raw_parts(self.rx_buffer(index) as *const u8, count) };
            out[..count].copy_from_slice(source);
        }
        unsafe { write_volatile(&mut (*slot).status, 0) };
        issue_command(self.io_base, UP_UNSTALL);
        self.rx_next = (index + 1) % RX_COUNT;
        if !valid {
            self.bad_frames += 1;
            return None;
        }
        self.frames_received += 1;
        Some(count)
    }
}
