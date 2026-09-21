//! Minimal polling-mode driver for NE2000-compatible Ethernet controllers,
//! as found on the PCI bus (the classic Realtek RTL8029AS and its many
//! register-compatible clones -- Winbond's W89C940F/PCI, VIA's VT86C926,
//! Holtek's HT80232, and others all copy the same interface). "NE2000" is
//! not one specific chip; it is a reference register interface, first
//! defined around National Semiconductor's DP8390 NIC core, that a whole
//! generation of cheap clones all imitated closely enough that one driver
//! written against the interface works unmodified against any of them --
//! including QEMU's own `-device ne2k_pci`, which presents the exact PCI
//! identity (vendor `0x10ec`, device `0x8029`) of a real RTL8029AS.
//!
//! This chip is architecturally the odd one out among this project's other
//! NIC drivers. `rtl8139.rs`/`threecom.rs`/`e1000.rs` all hand the card the
//! *physical address* of a DMA buffer in host RAM and let it read/write that
//! memory itself over the PCI bus. An NE2000-family card instead keeps its
//! own packet memory entirely on-card (in practice, historically a small
//! SRAM chip, addressed by the DP8390 core as up to 64 KiB of 256-byte
//! "pages"); the host never gets a physical address into it at all. Moving
//! a packet in or out of that memory instead uses a "remote DMA" channel:
//! despite the name, this is programmed I/O, one 16-bit word at a time,
//! through a single data-port register (`REG_DATA`) -- the driver pushes or
//! pulls every byte of a frame itself, in a tight loop, rather than pointing
//! the hardware at memory and waiting. This is why, uniquely among this
//! project's NIC drivers, `ne2000.rs` never calls `pmem::allocate_contiguous`
//! at all: there is no host-visible DMA buffer to allocate.
//!
//! The receive side uses a ring of on-card pages much like `rtl8139.rs`'s
//! ring buffer, but tracked by two page-numbered registers instead of one
//! byte offset: `BNRY` (the boundary the driver has already consumed up to)
//! and `CURR` (the page the card is about to write next), with a packet
//! header describing each entry's own next-page pointer and byte count.
//!
//! Like every other driver here, this one only implements enough to send
//! and receive raw Ethernet frames by polling; legacy interrupts only
//! acknowledge the card and mark foreground receive work pending, and there
//! is no protocol above Ethernet itself (no ARP, no IPv4), no multicast
//! filtering, and no wake-on-LAN support.

use core::cell::Cell;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::arch::x86::{inb, inw, outb, outw};
use crate::nic::{Health, WatchdogResult};
use crate::pci;

/// Realtek's PCI vendor ID. The RTL8029AS -- by far the most common real
/// NE2000-compatible PCI chip, and the exact identity QEMU's `ne2k_pci`
/// model presents -- is the only device this driver currently matches; see
/// this module's doc comment for other clones that share the identical
/// register interface and could be added here later once confirmed against
/// real hardware (their vendor/device IDs differ even though their
/// registers do not).
const VENDOR_REALTEK: u16 = 0x10ec;
const DEVICE_RTL8029: u16 = 0x8029;

// Command register: always accessible regardless of the selected register
// page (see `CR_PS0` below), unlike every other offset in this map.
const REG_CR: u16 = 0x00;

// Page 0 registers (selected when `CR_PS0`/`CR_PS1` are both clear). Several
// offsets mean different things on read vs. write; only the meaning this
// driver actually uses is named here.
const REG_PSTART: u16 = 0x01; // Write: first ring page. (Init only.)
const REG_PSTOP: u16 = 0x02; // Write: one past the last ring page. (Init only.)
const REG_BNRY: u16 = 0x03; // Read/write: boundary pointer (last page fully consumed).
const REG_TPSR: u16 = 0x04; // Write: transmit buffer's start page.
const REG_TBCR0: u16 = 0x05; // Write: transmit byte count, low byte.
const REG_TBCR1: u16 = 0x06; // Write: transmit byte count, high byte.
const REG_ISR: u16 = 0x07; // Read/write: interrupt status (write 1 to clear a bit).
const REG_RSAR0: u16 = 0x08; // Write: remote DMA start address, low byte.
const REG_RSAR1: u16 = 0x09; // Write: remote DMA start address, high byte (a page number).
const REG_RBCR0: u16 = 0x0a; // Write: remote DMA byte count, low byte.
const REG_RBCR1: u16 = 0x0b; // Write: remote DMA byte count, high byte.
const REG_RCR: u16 = 0x0c; // Write only here: receive configuration (reads back RSR instead).
const REG_TCR: u16 = 0x0d; // Write: transmit configuration.
const REG_DCR: u16 = 0x0e; // Write: data configuration (transfer width/mode).
const REG_IMR: u16 = 0x0f; // Write: interrupt mask.

// Page 1 registers (selected via `CR_PS0`), holding the station address the
// card filters incoming unicast frames against, and the current-page
// pointer used to detect a waiting frame.
const REG_PAR0: u16 = 0x01; // PAR0..PAR5 occupy offsets 0x01..0x06.
const REG_CURR: u16 = 0x07; // Page the card is about to write next.

const REG_DATA: u16 = 0x10; // Remote-DMA data port (see this module's doc comment).
const REG_RESET: u16 = 0x1f; // Reading then writing this back triggers a software reset.

const CR_STP: u8 = 1 << 0; // Stop.
const CR_STA: u8 = 1 << 1; // Start.
const CR_TXP: u8 = 1 << 2; // Transmit packet; the card clears this once done.
const CR_RD0: u8 = 1 << 3; // Remote DMA: start a read (card -> host).
const CR_RD1: u8 = 1 << 4; // Remote DMA: start a write (host -> card).
const CR_RD2: u8 = 1 << 5; // Remote DMA: abort/complete (also selects "no DMA").
const CR_PS0: u8 = 1 << 6; // Register page select bit 0 (page 1 when set, page 0 clear).

const ISR_PRX: u8 = 1 << 0; // Packet received (possibly with an error; see RSR).
const ISR_PTX: u8 = 1 << 1; // Packet transmitted successfully.
const ISR_RXE: u8 = 1 << 2; // Receive error.
const ISR_TXE: u8 = 1 << 3; // Transmit error.
const ISR_OVW: u8 = 1 << 4; // Overwrite warning: the ring is critically full.
const ISR_RDC: u8 = 1 << 6; // Remote DMA transfer complete.
const ISR_RST: u8 = 1 << 7; // Reset status (also set once a reset finishes).
const ISR_ALL: u8 = 0xff;

/// Interrupt causes this driver cares about: `enable_interrupts` unmasks
/// exactly these, matching the receive/transmit/overflow conditions
/// `handle_interrupt` and `recover` already know how to react to.
const NE_INTERRUPT_MASK: u8 = ISR_PRX | ISR_RXE | ISR_TXE | ISR_OVW;

const RCR_AB: u8 = 1 << 2; // Accept broadcast frames.
const RCR_MON: u8 = 1 << 5; // Monitor mode: receive into the ring without accepting anything.
/// Ordinary operating configuration: broadcast plus our own station
/// address (matched automatically against `PAR0..PAR5`, no RCR bit
/// needed). Multicast is deliberately not accepted -- nothing in this
/// project's protocol stack (ARP, DHCP, the various tape protocols) ever
/// sends to a multicast address, so there is no need to also program the
/// `MAR0..MAR7` filter registers for it.
const RCR_CONFIG: u8 = RCR_AB;

const TCR_NORMAL: u8 = 0x00;
/// Internal loopback (`TCR`'s `LB1:LB0` field, `01b`): frames the card
/// "transmits" are routed back into its own receive logic instead of onto
/// the wire. Used only while reading the MAC address out of the boot PROM
/// during `init`/`recover`, mirroring the sequence Linux's `ne2k-pci`
/// driver uses, so that step cannot itself put garbage on the wire.
const TCR_LOOPBACK_INTERNAL: u8 = 0x02;

/// The standard NE2000 data-configuration value used by essentially every
/// reference driver, including Linux's `ne2k-pci`: word-wide (16-bit)
/// remote-DMA transfers, normal (non-swapped) byte order, an 8-byte FIFO
/// threshold, and burst-mode DMA, with loopback left disabled. This
/// project does not need to distinguish those fields further -- every
/// known NE2000-compatible chip accepts this exact byte unmodified.
const DCR_WORD_WIDE: u8 = 0x49;

/// Every implementation of this reference design places its packet memory
/// in the same 16-bide-addressable page range for historical DP8390
/// address-decoding reasons, regardless of how much SRAM a given card
/// actually has: a 6-page (1.5 KiB) transmit buffer immediately followed
/// by a receive ring filling the rest of a 32 KiB window. This exact
/// layout is what QEMU's `ne2k_pci` model expects too, and is safe on any
/// genuine NE2000-compatible card for the same reason RTL8139/3C905/e1000
/// register values are safe across their own clones: it is the whole
/// point of building a "compatible" chip.
const TX_START_PAGE: u8 = 0x40;
const RX_START_PAGE: u8 = 0x46;
const RX_STOP_PAGE: u8 = 0x80;

/// The 4-byte header (receive status, next-page pointer, and a 16-bit byte
/// count covering the header itself) the card prepends to every entry in
/// the receive ring.
const RING_HEADER_LEN: usize = 4;

pub const MIN_FRAME_LEN: usize = 60; // Minimum Ethernet payload+header, excluding the FCS.
pub const MAX_FRAME_LEN: usize = 1514; // Standard maximum, excluding the FCS.

fn read8(base: u16, register: u16) -> u8 {
    unsafe { inb(base + register) }
}

fn write8(base: u16, register: u16, value: u8) {
    unsafe { outb(base + register, value) }
}

fn read16(base: u16, register: u16) -> u16 {
    unsafe { inw(base + register) }
}

fn write16(base: u16, register: u16, value: u16) {
    unsafe { outw(base + register, value) }
}

#[derive(Clone, Copy)]
pub struct ProbeInfo {
    io_base: u16,
    irq: Option<u8>,
}

pub struct Ne2000 {
    io_base: u16,
    mac: Cell<[u8; 6]>,
    /// This driver's own cached copy of the last value written to `RCR`.
    /// `RCR`'s register offset (`REG_RCR`) reads back the *receive status*
    /// register instead of the configuration the driver wrote, unlike
    /// every other write-only register here, so a genuine read-modify-
    /// write (as `set_promiscuous` on other drivers performs) is not
    /// possible without also using page 2's mirrored configuration
    /// registers -- support this driver does not otherwise need. Caching
    /// the value ourselves avoids that entirely.
    rcr_cache: Cell<u8>,
    promiscuous: Cell<bool>,
    overflow_count: Cell<u32>,
    recovery_count: Cell<u32>,
    recovery_failures: Cell<u32>,
    rx_progress: Cell<u32>,
    tx_progress: Cell<u32>,
    recovery_pending: AtomicBool,
    irq: Option<u8>,
    interrupt_count: AtomicU32,
    interrupt_errors: AtomicU32,
    work_pending: AtomicBool,
    interrupts_enabled: Cell<bool>,
}

#[derive(Debug)]
pub enum InitError {
    /// The card did not clear its reset bit, or complete a remote-DMA
    /// transfer, within a generous bounded wait; likely not present or
    /// responding despite being found on PCI.
    ResetTimedOut,
}

impl Ne2000 {
    /// Finds every RTL8029AS-compatible function with a usable I/O BAR and
    /// enables I/O space and bus mastering for each one. (This driver
    /// itself never uses bus mastering -- see this module's doc comment --
    /// but some clones' PCI interface logic still expects the bus-master
    /// enable bit set before their I/O BAR responds correctly, matching
    /// the same precaution `rtl8139.rs`/`threecom.rs` already take.)
    pub fn probe_all(out: &mut [ProbeInfo]) -> usize {
        let mut addresses = [None; pci::MAX_DEVICE_MATCHES];
        let found = pci::find_devices(VENDOR_REALTEK, &[DEVICE_RTL8029], &mut addresses);
        let mut count = 0;
        for address in addresses[..found.min(addresses.len())].iter().flatten() {
            address.enable_io_and_bus_master();
            if let Some(io_base) = address.io_bar0() {
                if count == out.len() {
                    break;
                }
                out[count] = ProbeInfo {
                    io_base,
                    irq: address.interrupt_line(),
                };
                count += 1;
            }
        }
        count
    }

    /// Resets the card, reads its burned-in MAC address out of the boot
    /// PROM, and brings the ring buffer up in normal (non-loopback)
    /// operation.
    pub fn init(io_base: u16, irq: Option<u8>) -> Result<Self, InitError> {
        Self::reset(io_base)?;
        let mac = Self::read_mac_prom(io_base)?;
        Self::program_registers(io_base, mac);

        Ok(Ne2000 {
            io_base,
            mac: Cell::new(mac),
            rcr_cache: Cell::new(RCR_CONFIG),
            promiscuous: Cell::new(false),
            overflow_count: Cell::new(0),
            recovery_count: Cell::new(0),
            recovery_failures: Cell::new(0),
            rx_progress: Cell::new(0),
            tx_progress: Cell::new(0),
            recovery_pending: AtomicBool::new(false),
            irq,
            interrupt_count: AtomicU32::new(0),
            interrupt_errors: AtomicU32::new(0),
            work_pending: AtomicBool::new(false),
            interrupts_enabled: Cell::new(false),
        })
    }

    /// Triggers a software reset by reading the reset port and writing the
    /// same value straight back (the documented, if slightly odd, way to
    /// ask an NE2000-family card to reset itself), then waits for `ISR`'s
    /// reset-complete bit. Shared by `init` and `recover`.
    fn reset(io_base: u16) -> Result<(), InitError> {
        let value = read8(io_base, REG_RESET);
        write8(io_base, REG_RESET, value);
        let mut attempts = 1_000_000u32;
        while read8(io_base, REG_ISR) & ISR_RST == 0 {
            attempts -= 1;
            if attempts == 0 {
                return Err(InitError::ResetTimedOut);
            }
        }
        write8(io_base, REG_ISR, ISR_ALL);
        Ok(())
    }

    /// Reads the 32-byte boot PROM via a remote-DMA read performed while
    /// the card is held in monitor/loopback mode, returning the MAC
    /// address from its first 6 bytes. This exact sequence -- byte-wide
    /// `inb` reads of the data port, even though `DCR_WORD_WIDE` requests
    /// word transfers for ordinary traffic -- mirrors Linux's `ne2k-pci`
    /// driver, the clearest working reference available for this specific
    /// step; unlike a real ISA NE2000 (whose 8-bit-wide PROM is read
    /// through a 16-bit bus and needs every other byte discarded), the PCI
    /// clones this driver targets present the MAC directly in the first 6
    /// bytes with no padding to skip.
    fn read_mac_prom(io_base: u16) -> Result<[u8; 6], InitError> {
        write8(io_base, REG_CR, CR_STP | CR_RD2); // Stop, abort/complete DMA, page 0.
        write8(io_base, REG_DCR, DCR_WORD_WIDE);
        write8(io_base, REG_RBCR0, 0);
        write8(io_base, REG_RBCR1, 0);
        write8(io_base, REG_IMR, 0);
        write8(io_base, REG_ISR, ISR_ALL);
        write8(io_base, REG_RCR, RCR_MON);
        write8(io_base, REG_TCR, TCR_LOOPBACK_INTERNAL);
        write8(io_base, REG_RBCR0, 32);
        write8(io_base, REG_RBCR1, 0);
        write8(io_base, REG_RSAR0, 0);
        write8(io_base, REG_RSAR1, 0);
        write8(io_base, REG_CR, CR_STA | CR_RD0); // Start remote-DMA read.

        let mut prom = [0u8; 32];
        for byte in prom.iter_mut() {
            *byte = read8(io_base, REG_DATA);
        }

        let mut attempts = 1_000_000u32;
        while read8(io_base, REG_ISR) & ISR_RDC == 0 {
            attempts -= 1;
            if attempts == 0 {
                return Err(InitError::ResetTimedOut);
            }
        }
        write8(io_base, REG_ISR, ISR_RDC);

        let mut mac = [0u8; 6];
        mac.copy_from_slice(&prom[..6]);
        Ok(mac)
    }

    /// (Re-)programs every register a freshly reset card needs before it
    /// will actually send/receive: the ring's page boundaries, the station
    /// address filter (`PAR0..PAR5`, on page 1), and normal (non-loopback)
    /// transmit/receive configuration. Shared by `init` and `recover`.
    fn program_registers(io_base: u16, mac: [u8; 6]) {
        write8(io_base, REG_CR, CR_STP | CR_RD2);
        write8(io_base, REG_DCR, DCR_WORD_WIDE);
        write8(io_base, REG_RBCR0, 0);
        write8(io_base, REG_RBCR1, 0);
        write8(io_base, REG_TPSR, TX_START_PAGE);
        write8(io_base, REG_PSTART, RX_START_PAGE);
        write8(io_base, REG_BNRY, RX_START_PAGE);
        write8(io_base, REG_PSTOP, RX_STOP_PAGE);
        write8(io_base, REG_ISR, ISR_ALL);
        write8(io_base, REG_IMR, 0); // No interrupts until `enable_interrupts` is called.
        write8(io_base, REG_RCR, RCR_MON);
        write8(io_base, REG_TCR, TCR_LOOPBACK_INTERNAL);

        // Station address and the ring's initial "next write" page live on
        // page 1.
        write8(io_base, REG_CR, CR_STP | CR_RD2 | CR_PS0);
        for (index, byte) in mac.iter().enumerate() {
            write8(io_base, REG_PAR0 + index as u16, *byte);
        }
        write8(io_base, REG_CURR, RX_START_PAGE.wrapping_add(1));

        // Back to page 0, start the card, and switch to normal operation.
        write8(io_base, REG_CR, CR_STA | CR_RD2);
        write8(io_base, REG_TCR, TCR_NORMAL);
        write8(io_base, REG_RCR, RCR_CONFIG);
        write8(io_base, REG_ISR, ISR_ALL);
    }

    pub fn mac_address(&self) -> [u8; 6] {
        self.mac.get()
    }

    /// Enables or disables promiscuous mode. Unlike `rtl8139.rs`'s
    /// equivalent, this cannot read `RCR` back to preserve its other bits
    /// (see `rcr_cache`'s doc comment), so it works from the cached value
    /// this driver itself last wrote instead.
    pub fn set_promiscuous(&self, enabled: bool) {
        const RCR_PRO: u8 = 1 << 4;
        let mut rcr = self.rcr_cache.get();
        if enabled {
            rcr |= RCR_PRO;
        } else {
            rcr &= !RCR_PRO;
        }
        write8(self.io_base, REG_RCR, rcr);
        self.rcr_cache.set(rcr);
        self.promiscuous.set(enabled);
    }

    /// Performs one remote-DMA write: `data` is pushed, one 16-bit word at
    /// a time, into the card's on-board memory starting at `page_address`
    /// (an absolute on-card byte address, i.e. `page << 8`). Returns
    /// `false` if the card does not report completion within a bounded
    /// wait.
    fn remote_write(&self, page_address: u16, data: &[u8]) -> bool {
        write8(self.io_base, REG_ISR, ISR_RDC); // Clear any stale completion bit first.
        write8(self.io_base, REG_RBCR0, (data.len() & 0xff) as u8);
        write8(self.io_base, REG_RBCR1, (data.len() >> 8) as u8);
        write8(self.io_base, REG_RSAR0, (page_address & 0xff) as u8);
        write8(self.io_base, REG_RSAR1, (page_address >> 8) as u8);
        write8(self.io_base, REG_CR, CR_STA | CR_RD1); // Start remote-DMA write.

        let mut index = 0;
        while index + 1 < data.len() {
            let word = u16::from(data[index]) | (u16::from(data[index + 1]) << 8);
            write16(self.io_base, REG_DATA, word);
            index += 2;
        }
        if index < data.len() {
            write16(self.io_base, REG_DATA, u16::from(data[index]));
        }

        let mut attempts = 1_000_000u32;
        while read8(self.io_base, REG_ISR) & ISR_RDC == 0 {
            attempts -= 1;
            if attempts == 0 {
                return false;
            }
        }
        write8(self.io_base, REG_ISR, ISR_RDC);
        true
    }

    /// The read counterpart of `remote_write`: pulls `out.len()` bytes from
    /// the card's on-board memory starting at `page_address` into `out`.
    fn remote_read(&self, page_address: u16, out: &mut [u8]) -> bool {
        write8(self.io_base, REG_ISR, ISR_RDC);
        write8(self.io_base, REG_RBCR0, (out.len() & 0xff) as u8);
        write8(self.io_base, REG_RBCR1, (out.len() >> 8) as u8);
        write8(self.io_base, REG_RSAR0, (page_address & 0xff) as u8);
        write8(self.io_base, REG_RSAR1, (page_address >> 8) as u8);
        write8(self.io_base, REG_CR, CR_STA | CR_RD0); // Start remote-DMA read.

        let mut index = 0;
        while index + 1 < out.len() {
            let word = read16(self.io_base, REG_DATA);
            out[index] = (word & 0xff) as u8;
            out[index + 1] = (word >> 8) as u8;
            index += 2;
        }
        if index < out.len() {
            let word = read16(self.io_base, REG_DATA);
            out[index] = (word & 0xff) as u8;
        }

        let mut attempts = 1_000_000u32;
        while read8(self.io_base, REG_ISR) & ISR_RDC == 0 {
            attempts -= 1;
            if attempts == 0 {
                return false;
            }
        }
        write8(self.io_base, REG_ISR, ISR_RDC);
        true
    }

    /// Transmits one raw Ethernet frame. Unlike `rtl8139.rs`'s four
    /// transmit descriptors, this card has exactly one transmit buffer, so
    /// a send must wait for any previous transmission to finish (`CR_TXP`
    /// clearing) before starting a new one -- normally instantaneous, since
    /// callers here never pipeline sends.
    pub fn send(&self, frame: &[u8]) -> bool {
        if frame.len() > MAX_FRAME_LEN {
            return false;
        }

        let mut attempts = 1_000_000u32;
        while read8(self.io_base, REG_CR) & CR_TXP != 0 {
            attempts -= 1;
            if attempts == 0 {
                self.recovery_pending.store(true, Ordering::Release);
                return false;
            }
        }

        let padded_len = frame.len().max(MIN_FRAME_LEN);
        let mut buffer = [0u8; MAX_FRAME_LEN];
        buffer[..frame.len()].copy_from_slice(frame);

        if !self.remote_write(u16::from(TX_START_PAGE) << 8, &buffer[..padded_len]) {
            self.recovery_pending.store(true, Ordering::Release);
            return false;
        }

        write8(self.io_base, REG_TPSR, TX_START_PAGE);
        write8(self.io_base, REG_TBCR0, (padded_len & 0xff) as u8);
        write8(self.io_base, REG_TBCR1, (padded_len >> 8) as u8);
        write8(self.io_base, REG_CR, CR_STA | CR_TXP);

        let mut attempts = 1_000_000u32;
        loop {
            let isr = read8(self.io_base, REG_ISR);
            if isr & ISR_TXE != 0 {
                write8(self.io_base, REG_ISR, ISR_TXE | ISR_PTX);
                self.recovery_pending.store(true, Ordering::Release);
                return false;
            }
            if isr & ISR_PTX != 0 {
                write8(self.io_base, REG_ISR, ISR_PTX);
                self.tx_progress.set(self.tx_progress.get().wrapping_add(1));
                return true;
            }
            attempts -= 1;
            if attempts == 0 {
                self.recovery_pending.store(true, Ordering::Release);
                return false;
            }
        }
    }

    /// Recovers a latched overwrite warning (`ISR_OVW`, the ring is
    /// critically full), a bounded transmit failure, or a ring entry whose
    /// header looks nonsensical (see `receive`'s own doc comment) with a
    /// full software reset and reprogram, the same "trust nothing, start
    /// over" approach this project's RTL8139 driver settled on after real
    /// hardware showed a lighter, register-only fixup left that card
    /// unable to receive anything at all afterward. The official DP8390
    /// datasheet documents a more surgical overflow-recovery procedure,
    /// but a full reset needs no separate code path here and is at least
    /// as likely to leave a real card in a working state.
    fn recover(&self) -> bool {
        let isr = read8(self.io_base, REG_ISR);
        let had_overflow = isr & ISR_OVW != 0;
        if !had_overflow && !self.recovery_pending.swap(false, Ordering::AcqRel) {
            return false;
        }

        if Self::reset(self.io_base).is_err() {
            self.recovery_failures
                .set(self.recovery_failures.get().wrapping_add(1));
            return false;
        }
        let mac = self.mac.get();
        Self::program_registers(self.io_base, mac);
        self.rcr_cache.set(RCR_CONFIG);
        if self.promiscuous.get() {
            self.set_promiscuous(true);
        }
        if self.interrupts_enabled.get() {
            self.enable_interrupts();
        }

        if had_overflow {
            write8(self.io_base, REG_ISR, ISR_OVW);
            self.overflow_count.set(self.overflow_count.get().wrapping_add(1));
        }
        self.recovery_count.set(self.recovery_count.get().wrapping_add(1));
        true
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
            recovery_pending: self.recovery_pending.load(Ordering::Acquire)
                || read8(self.io_base, REG_ISR) & ISR_OVW != 0,
        }
    }

    pub fn watchdog(&self) -> WatchdogResult {
        if !self.health().recovery_pending {
            return WatchdogResult::Healthy;
        }
        if self.recover() {
            WatchdogResult::Recovered
        } else {
            WatchdogResult::Failed
        }
    }

    pub fn enable_interrupts(&self) {
        write8(self.io_base, REG_ISR, ISR_ALL);
        write8(self.io_base, REG_IMR, NE_INTERRUPT_MASK);
        self.interrupts_enabled.set(true);
    }

    pub fn handle_interrupt(&self) {
        let status = read8(self.io_base, REG_ISR) & NE_INTERRUPT_MASK;
        if status == 0 {
            return;
        }
        write8(self.io_base, REG_ISR, status);
        self.interrupt_count.fetch_add(1, Ordering::Relaxed);
        if status & (ISR_RXE | ISR_TXE | ISR_OVW) != 0 {
            self.interrupt_errors.fetch_add(1, Ordering::Relaxed);
        }
        if status & (ISR_TXE | ISR_OVW) != 0 {
            self.recovery_pending.store(true, Ordering::Release);
        }
        self.work_pending.store(true, Ordering::Release);
    }

    pub fn interrupt_stats(&self) -> (Option<u8>, u32, u32, bool) {
        (
            self.irq,
            self.interrupt_count.load(Ordering::Relaxed),
            self.interrupt_errors.load(Ordering::Relaxed),
            self.work_pending.load(Ordering::Acquire),
        )
    }

    /// Copies out one received frame (if any is waiting) into `out`,
    /// returning its length -- clipped to `out.len()` if the frame is
    /// larger than the caller's buffer, though the ring is advanced by the
    /// *actual* frame length either way, exactly matching `rtl8139.rs`'s
    /// convention for the same situation.
    pub fn receive(&self, out: &mut [u8]) -> Option<usize> {
        self.work_pending.store(false, Ordering::Release);
        self.recover();

        write8(self.io_base, REG_CR, CR_STA | CR_PS0); // Select page 1 to read CURR.
        let current = read8(self.io_base, REG_CURR);
        write8(self.io_base, REG_CR, CR_STA); // Back to page 0 to read BNRY.
        let boundary = read8(self.io_base, REG_BNRY);

        let next_read = if boundary.wrapping_add(1) >= RX_STOP_PAGE {
            RX_START_PAGE
        } else {
            boundary + 1
        };
        if next_read == current {
            return None; // BNRY has caught up to CURR: nothing waiting.
        }

        let mut header = [0u8; RING_HEADER_LEN];
        if !self.remote_read(u16::from(next_read) << 8, &mut header) {
            self.recovery_pending.store(true, Ordering::Release);
            return None;
        }
        // The DP8390 ring header layout is: byte 0 is the receive status
        // (matching `RSR`), byte 1 is the next packet's page number, and
        // bytes 2-3 are the little-endian byte count (matching QEMU's own
        // `ne2000_receive`: `p[0] = s->rsr; p[1] = next >> 8; p[2] =
        // total_len; p[3] = total_len >> 8;`).
        let status = header[0];
        let next_page = header[1];
        let length = u16::from(header[2]) | (u16::from(header[3]) << 8);

        // As with `rtl8139.rs`'s own ring, a header field that cannot
        // possibly be right means the ring itself is desynchronized (or we
        // just read stale/garbled bytes): there is no sane way to advance
        // past an entry whose own advancement information is not trusted,
        // so this is flagged for the same full-reset recovery path
        // instead of ever looping on it.
        let sane = next_page >= RX_START_PAGE
            && next_page < RX_STOP_PAGE
            && (length as usize) >= RING_HEADER_LEN
            && (length as usize) <= MAX_FRAME_LEN + RING_HEADER_LEN;
        if !sane {
            self.recovery_pending.store(true, Ordering::Release);
            return None;
        }

        let payload_len = length as usize - RING_HEADER_LEN;
        let copy_len = payload_len.min(out.len()).min(MAX_FRAME_LEN);

        // The payload may straddle the ring's wraparound point (PSTOP back
        // to PSTART) mid-packet -- unlike `rtl8139.rs`, which reserves
        // slack space specifically to avoid this, an NE2000-style ring has
        // no such reservation, so a payload read spanning the boundary
        // must be split into two remote-DMA reads.
        let start_address = (u16::from(next_read) << 8) + RING_HEADER_LEN as u16;
        let ring_end = u16::from(RX_STOP_PAGE) << 8;
        let before_wrap = usize::from(ring_end - start_address);
        let read_ok = if copy_len <= before_wrap {
            self.remote_read(start_address, &mut out[..copy_len])
        } else {
            let (first, second) = out[..copy_len].split_at_mut(before_wrap);
            self.remote_read(start_address, first)
                && self.remote_read(u16::from(RX_START_PAGE) << 8, second)
        };
        if !read_ok {
            self.recovery_pending.store(true, Ordering::Release);
            return None;
        }

        // The boundary pointer must land one page *before* the next page
        // the card itself is about to write, not on it -- a documented
        // DP8390 quirk shared by every reference driver of this design.
        let new_boundary = if next_page == RX_START_PAGE {
            RX_STOP_PAGE - 1
        } else {
            next_page - 1
        };
        write8(self.io_base, REG_BNRY, new_boundary);

        if status & ISR_PRX == 0 {
            // Not receive-OK (a real CRC/alignment/runt error): the ring
            // is still advanced above, exactly like `rtl8139.rs`'s
            // equivalent case, but nothing is reported to the caller.
            return None;
        }

        self.rx_progress.set(self.rx_progress.get().wrapping_add(1));
        Some(copy_len)
    }
}

const MAX_INSTANCES: usize = pci::MAX_DEVICE_MATCHES;
static mut NICS: [Option<Ne2000>; MAX_INSTANCES] = [const { None }; MAX_INSTANCES];
static mut NIC_COUNT: usize = 0;
static mut INITIALIZED: bool = false;

#[derive(Debug)]
pub enum Status {
    NotPresent,
    InitFailed,
}

/// Probes and initializes every usable NE2000-compatible card.
pub fn initialize_all() -> Result<usize, Status> {
    unsafe {
        if !INITIALIZED {
            INITIALIZED = true;
            let mut probe_info = [const {
                ProbeInfo {
                    io_base: 0,
                    irq: None,
                }
            }; MAX_INSTANCES];
            let found = Ne2000::probe_all(&mut probe_info);
            let mut first_error = None;
            for info in &probe_info[..found] {
                match Ne2000::init(info.io_base, info.irq) {
                    Ok(initialized) => {
                        NICS[NIC_COUNT] = Some(initialized);
                        NIC_COUNT += 1;
                    }
                    Err(error) if first_error.is_none() => first_error = Some(error),
                    Err(_) => {}
                }
            }
            if NIC_COUNT == 0 {
                return if first_error.is_some() {
                    Err(Status::InitFailed)
                } else {
                    Err(Status::NotPresent)
                };
            }
        }
        Ok(NIC_COUNT)
    }
}

pub fn get(index: usize) -> Option<&'static Ne2000> {
    unsafe {
        if index >= NIC_COUNT {
            return None;
        }
        NICS[index].as_ref()
    }
}
