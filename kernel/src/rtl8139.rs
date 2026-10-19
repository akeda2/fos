//! A driver for the Realtek RTL8139 Fast Ethernet controller (`10ec:8139`), ported from v1's `rtl8139.rs`, which was run on real
//! cards and is QEMU's classic emulated NIC (`-device rtl8139`), so unlike the 3C905 this one has automated coverage
//! (`make test-rtl8139`).
//!
//! How the card works, briefly. All registers sit in an I/O-port window (BAR0). There are no descriptor rings of the modern
//! kind: *receive* is one contiguous ring buffer in host RAM (here 32 KiB plus slack) into which the card DMAs each frame,
//! preceded by a 4-byte header (status, length including the 4-byte CRC), and the driver chases the card's write pointer
//! with its own read offset, telling the card how far it has read through `CAPR` (which, by a documented quirk, must be the
//! offset minus 16). *Transmit* has four fixed slots: the driver writes a buffer's physical address into `TSADn` and the
//! frame length into `TSDn`, which starts the DMA; `TSDn.TOK` says it went out. The card's DMA addresses are 32 bits, so
//! every buffer is taken from memory below 4 GiB.
//!
//! Hard-won rules carried over from v1 (found on real hardware, mostly invisible in QEMU):
//! * `CR.TE|RE` must be set *before* `RCR`/`TCR` are written: those registers hold FIFO/DMA threshold fields that real silicon
//!   does not latch while the transmitter/receiver are off (every send then reported TOK but nothing reached the wire).
//! * `TCR`'s interframe-gap field is set to the standard 802.3 value (`11b`); at the reset value real switches and NICs drop
//!   back-to-back frames that QEMU's virtual wire never objects to.
//! * The 32 KiB ring (`RCR.RBLEN = 10b`) with `WRAP` set; 64 KiB makes "past the end" and "wrapped" the same address and
//!   corrupts frames. A frame the card marks as bad (CRC, runt) must still be stepped over, or the ring wedges on it forever.
//! * A ring entry whose length field is nonsense means the ring is desynchronised (a cable pull leaves stray bytes in the
//!   buffer): the only sane fix is a full software reset, which also reinitialises every register and the read offset.
//! * A physical link down-to-up transition (`MSR.LINKB` going from 1 to 0) also needs that full reset: the receiver never
//!   resumes by itself, which on real hardware left a router uplink dead after its cable was replugged. Linux's `8139too`
//!   does the same re-init.
//! * Receive-buffer and FIFO overflow (`ISR.RXOVW`/`FOVW`) leave the read/write pointers desynchronised: full reset again. They
//!   are counted, and the ring is big enough that a gigabit-paced burst rarely does it.
//! * A transmit abort/underrun, or a slot that never completes, is recovered the same way.
//!
//! Interrupts only wake the CPU (as on the other NICs): the foreground always polls the ring. The card has no MSI, so legacy
//! INTx through the I/O APIC is tried (candidate pins probed, as for the RTL8168 and 3C905) and kept only if a self-test
//! interrupt arrives. The RTL8139 has no "raise an interrupt now" command, but it has a timer: writing `TCTR` restarts a free
//! running counter and when it reaches `TimerInt` the card sets `ISR.TimeOut`, which is the self-test's cause.
//! If no pin delivers it the NIC stays polled and says so.

use core::sync::atomic::{AtomicBool, AtomicU16, Ordering};

use crate::e1000::{Delivery, ISR, MAX_INTERFACES};
use crate::{cpu, memory, pci, timer};

pub const VENDOR_REALTEK: u16 = 0x10ec;
pub const DEVICE_IDS: &[u16] = &[0x8139];

// Register offsets from the I/O base (BAR0).
const REG_IDR0: u16 = 0x00; // first byte of the 6-byte MAC address
const REG_MAR0: u16 = 0x08; // 64-bit multicast hash filter
const REG_TSD0: u16 = 0x10; // transmit status, slots 0-3 (4 bytes apart)
const REG_TSAD0: u16 = 0x20; // transmit start address (physical), slots 0-3
const REG_RBSTART: u16 = 0x30; // receive buffer start (physical)
const REG_CR: u16 = 0x37;
const REG_CAPR: u16 = 0x38; // current address of packet read (offset - 16)
const REG_IMR: u16 = 0x3c;
const REG_ISR: u16 = 0x3e; // latches regardless of IMR; write 1 to clear
const REG_TCR: u16 = 0x40;
const REG_RCR: u16 = 0x44;
const REG_TCTR: u16 = 0x48; // free-running timer; any write restarts it
const REG_TIMERINT: u16 = 0x54; // ISR.TimeOut is raised when TCTR reaches this (0 = never)
const REG_CONFIG1: u16 = 0x52;
const REG_MSR: u16 = 0x58;

/// `MSR`'s "Linkage Bad" bit: 1 while the physical link is down.
const MSR_LINKB: u8 = 1 << 2;

const CR_BUFE: u8 = 1 << 0; // receive buffer empty
const CR_TE: u8 = 1 << 2;
const CR_RE: u8 = 1 << 3;
const CR_RST: u8 = 1 << 4;

/// `RCR` bits 12:11: ring size. `10b` is 32 KiB (see the rules above for why not 64 KiB).
const RCR_RBLEN_32K: u32 = 0b10 << 11;
/// Accept physical-match (APM), multicast (AM, through the MAR hash), broadcast (AB); WRAP lets the last frame before the
/// ring end spill into the slack instead of being split.
const RCR_CONFIG: u32 = (1 << 1) | (1 << 2) | (1 << 3) | (1 << 7) | RCR_RBLEN_32K;
/// Accept All Packets: promiscuous mode.
const RCR_AAP: u32 = 1 << 0;
const TCR_STANDARD_IFG: u32 = 0b11 << 24;

const TSD_TOK: u32 = 1 << 15;
const TSD_TUN: u32 = 1 << 14;
const TSD_TABT: u32 = 1 << 30;

const ISR_ROK: u16 = 1 << 0;
const ISR_RER: u16 = 1 << 1;
const ISR_RXOVW: u16 = 1 << 4;
const ISR_FOVW: u16 = 1 << 6;
const ISR_TIMEOUT: u16 = 1 << 14;
const INTERRUPT_MASK: u16 = ISR_ROK | ISR_RER | ISR_RXOVW | ISR_FOVW;

/// The ring proper, the 16 bytes the datasheet reserves, and 1500 bytes of slack for `WRAP`, in whole frames.
const RX_BUF_LEN: u32 = 32768;
const RX_BUF_FRAMES: u64 = ((RX_BUF_LEN as u64 + 16 + 1500) + 4095) / 4096;
const TX_SLOTS: usize = 4;
pub const MIN_FRAME: usize = 60;
pub const MAX_FRAME: usize = 1514;

#[derive(Debug)]
pub enum Error {
    NoIoBar,
    NotResponding,
    OutOfMemory,
    ResetTimedOut,
}

fn read8(base: u16, offset: u16) -> u8 {
    cpu::inb(base + offset)
}
fn write8(base: u16, offset: u16, value: u8) {
    cpu::outb(base + offset, value)
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

/// Per-interface state the interrupt handler needs.
static IO_BASE: [AtomicU16; MAX_INTERFACES] = [const { AtomicU16::new(0) }; MAX_INTERFACES];
/// Set by the handler when it saw (and cleared) an overflow cause, for the foreground to reset the card.
static OVERFLOWED: [AtomicBool; MAX_INTERFACES] = [const { AtomicBool::new(false) }; MAX_INTERFACES];

/// The interrupt handler: acknowledge what the card says (the line is level-triggered, so a cause left set would storm), note
/// an overflow for the foreground and return. Frames are found by polling the ring after the CPU wakes.
fn isr(index: usize) {
    let base = IO_BASE[index].load(Ordering::Relaxed);
    if base == 0 {
        return;
    }
    let status = read16(base, REG_ISR) & (INTERRUPT_MASK | ISR_TIMEOUT);
    if status == 0 {
        return;
    }
    write16(base, REG_ISR, status);
    crate::msi::claim();
    ISR[index].interrupts.fetch_add(1, Ordering::Relaxed);
    if status & ISR_TIMEOUT != 0 {
        write32(base, REG_TIMERINT, 0); // the self-test's cause: one shot, not wanted again
    }
    if status & (ISR_RXOVW | ISR_FOVW) != 0 {
        ISR[index].overruns.fetch_add(1, Ordering::Relaxed);
        OVERFLOWED[index].store(true, Ordering::Release);
    }
}

pub struct Rtl8139 {
    index: usize,
    pub delivery: Delivery,
    interrupts_enabled: bool,
    io_base: u16,
    pub mac: [u8; 6],
    rx_buffer: u64,
    tx_buffers: u64,
    /// Read offset into the ring (a multiple of 4, below `RX_BUF_LEN`).
    rx_offset: u32,
    tx_next: usize,
    /// Slots whose frame was started but whose completion has not been seen yet.
    tx_pending: [bool; TX_SLOTS],
    promiscuous: bool,
    multicast: bool,
    /// Last observed link state, to detect a down-to-up transition.
    link: bool,
    recovery_pending: bool,
    pub frames_received: u64,
    pub frames_sent: u64,
    pub frames_dropped_no_link: u64,
    pub recoveries: u32,
    pub recovery_failures: u32,
    /// Ring entries skipped because the card marked them bad (CRC, runt, ...).
    pub bad_frames: u32,
    /// Full resets caused by a receive overflow.
    pub overflows: u32,
}

/// Power on, then software reset, waiting for the card to clear `CR.RST` itself.
fn soft_reset(base: u16) -> bool {
    write8(base, REG_CONFIG1, 0x00);
    write8(base, REG_CR, CR_RST);
    for _ in 0..1_000_000u32 {
        if read8(base, REG_CR) & CR_RST == 0 {
            return true;
        }
        core::hint::spin_loop();
    }
    false
}

impl Rtl8139 {
    pub fn init(device: &pci::Device, index: usize) -> Result<Rtl8139, Error> {
        device.force_power_state_d0();
        let command = device.read32(4);
        device.write32(4, (command & 0xffff) | 0x5); // I/O decoding and bus mastering
        let io_base = device.io_bar(0).ok_or(Error::NoIoBar)?;
        if read8(io_base, REG_CR) == 0xff {
            return Err(Error::NotResponding);
        }
        if !soft_reset(io_base) {
            return Err(Error::ResetTimedOut);
        }
        let below = Some(1u64 << 32); // 32-bit DMA addresses
        let rx_buffer = memory::allocate_contiguous(RX_BUF_FRAMES, below).ok_or(Error::OutOfMemory)?;
        let tx_buffers = memory::allocate_contiguous(TX_SLOTS as u64, below).ok_or(Error::OutOfMemory)?;
        let mut mac = [0u8; 6];
        for (offset, byte) in mac.iter_mut().enumerate() {
            *byte = read8(io_base, REG_IDR0 + offset as u16);
        }
        if mac.iter().all(|&byte| byte == 0) || mac == [0xff; 6] {
            return Err(Error::NotResponding);
        }
        let mut nic = Rtl8139 {
            index,
            delivery: Delivery::Polled,
            interrupts_enabled: false,
            io_base,
            mac,
            rx_buffer,
            tx_buffers,
            rx_offset: 0,
            tx_next: 0,
            tx_pending: [false; TX_SLOTS],
            promiscuous: false,
            multicast: false,
            link: read8(io_base, REG_MSR) & MSR_LINKB == 0,
            recovery_pending: false,
            frames_received: 0,
            frames_sent: 0,
            frames_dropped_no_link: 0,
            recoveries: 0,
            recovery_failures: 0,
            bad_frames: 0,
            overflows: 0,
        };
        nic.program_registers();
        IO_BASE[index].store(io_base, Ordering::Relaxed);
        Ok(nic)
    }

    pub fn io_base(&self) -> u16 {
        self.io_base
    }

    /// (Re)programs everything a freshly reset card needs; used at init and after a recovery reset (the memory is reused).
    fn program_registers(&mut self) {
        let base = self.io_base;
        write32(base, REG_RBSTART, self.rx_buffer as u32);
        write16(base, REG_IMR, 0);
        // Enable the transmitter and receiver *before* writing RCR/TCR: see the rules at the top.
        write8(base, REG_CR, CR_TE | CR_RE);
        write32(base, REG_RCR, RCR_CONFIG | if self.promiscuous { RCR_AAP } else { 0 });
        write32(base, REG_TCR, TCR_STANDARD_IFG);
        self.write_multicast_filter();
        write16(base, REG_ISR, 0xffff); // clear anything latched before the reset
        self.rx_offset = 0;
        write16(base, REG_CAPR, 0u16.wrapping_sub(16));
        self.tx_next = 0;
        self.tx_pending = [false; TX_SLOTS];
    }

    /// `RCR.AM` accepts a multicast frame only if its hash bit is set in MAR0-7, so "all multicast" is eight 0xff bytes.
    /// (v1 left the filter zero, which dropped every multicast frame; broadcast, which discovery uses, was never affected.)
    fn write_multicast_filter(&self) {
        let value = if self.multicast { 0xffff_ffff } else { 0 };
        write32(self.io_base, REG_MAR0, value);
        write32(self.io_base, REG_MAR0 + 4, value);
    }

    /// The PHY's basic mode control register: the 8139 maps it straight into I/O space (`BMCR`, 0x62).
    pub fn phy_control(&self) -> Option<u16> {
        Some(read16(self.io_base, 0x62))
    }

    pub fn set_phy_control(&mut self, value: u16) -> bool {
        write16(self.io_base, 0x62, value);
        true
    }

    /// The `MSR` link-status bit (inverted: LINKB is "bad").
    pub fn link_up(&self) -> bool {
        read8(self.io_base, REG_MSR) & MSR_LINKB == 0
    }

    /// Promiscuous mode is `RCR.AAP`, but writing `RCR` is not harmless: QEMU's model restarts the receive ring (read and write
    /// pointers to the start) on any `RCR` write, while the datasheet says nothing of the kind, so a driver that kept its own
    /// read offset would be left chasing a ring that moved (found by `make test-rtl8139`: the sniffer then read garbage and
    /// took the nonsensical-length recovery). The behaviour that is correct on either is to make the ring state a known one:
    /// reset the card and reprogram it with the new filter, which also resets the offset. Frames still in the ring are lost,
    /// which a mode switch for sniffing can afford.
    pub fn set_promiscuous(&mut self, on: bool) {
        if self.promiscuous != on {
            self.promiscuous = on;
            self.reset_and_program();
        }
    }

    pub fn set_multicast(&mut self, on: bool) {
        self.multicast = on;
        self.write_multicast_filter();
    }

    /// Raises one interrupt through the card's timer and reports whether `arrived` saw it within three ticks.
    fn self_test(&mut self, arrived: impl Fn() -> u64) -> bool {
        write16(self.io_base, REG_ISR, 0xffff);
        write16(self.io_base, REG_IMR, INTERRUPT_MASK | ISR_TIMEOUT);
        let before = arrived();
        write32(self.io_base, REG_TIMERINT, 0x2000); // ~250 microseconds at the counter's 33 MHz
        write32(self.io_base, REG_TCTR, 0); // restart the counter
        let start = timer::ticks();
        while arrived() == before && timer::ticks().wrapping_sub(start) < 3 {
            core::hint::spin_loop();
        }
        arrived() != before
    }

    fn finish_interrupts(&mut self, delivery: Delivery) -> Delivery {
        self.interrupts_enabled = true;
        self.delivery = delivery;
        write32(self.io_base, REG_TIMERINT, 0);
        write16(self.io_base, REG_ISR, 0xffff);
        write16(self.io_base, REG_IMR, INTERRUPT_MASK);
        delivery
    }

    /// Switches from polling to legacy interrupts through the I/O APIC (the card has no MSI), probing candidate pins as the
    /// RTL8168 and 3C905 drivers do since the ACPI routing table is not read, and keeping one only if the self-test arrives.
    pub fn enable_interrupts(&mut self, device: &pci::Device) -> Result<Delivery, &'static str> {
        let mut reason = "no I/O APIC pin delivered the self-test interrupt";
        let (first, end) = crate::ioapic::gsi_range();
        let candidates = crate::msi::intx_candidates(device.interrupt_line());
        for (position, &gsi) in candidates.iter().enumerate() {
            if gsi == u32::MAX || gsi == 0 || gsi < first || gsi >= end || candidates[..position].contains(&gsi) {
                continue;
            }
            let Some(vector) = crate::msi::attach_intx(gsi, isr, self.index) else { continue };
            let index = self.index;
            if self.self_test(|| ISR[index].interrupts.load(Ordering::Relaxed)) {
                return Ok(self.finish_interrupts(Delivery::Intx { gsi, vector }));
            }
            write16(self.io_base, REG_IMR, 0);
            write32(self.io_base, REG_TIMERINT, 0);
            crate::msi::detach_intx(vector, self.index);
            reason = "no I/O APIC pin delivered the self-test interrupt";
        }
        Err(reason)
    }

    /// Notes a physical link recovery (down then up) and asks for a full reset: the receiver never resumes by itself.
    fn check_link_recovery(&mut self) {
        let now = self.link_up();
        if now && !self.link {
            self.recovery_pending = true;
        }
        self.link = now;
    }

    /// Resets and reprograms the card if an overflow, a transmit failure, a nonsensical ring entry or a link recovery asked
    /// for it. Frames in the ring or in flight are lost, as on any link that dropped them.
    fn recover(&mut self) -> bool {
        let isr = if self.interrupts_enabled { 0 } else { read16(self.io_base, REG_ISR) };
        let overflow = OVERFLOWED[self.index].swap(false, Ordering::AcqRel) || isr & (ISR_RXOVW | ISR_FOVW) != 0;
        if !overflow && !self.recovery_pending {
            return false;
        }
        self.recovery_pending = false;
        if !self.reset_and_program() {
            self.recovery_pending = true;
            return false;
        }
        self.overflows += u32::from(overflow);
        self.recoveries += 1;
        true
    }

    /// Software reset, then every register again (the memory is reused); interrupts are re-armed if they were on.
    fn reset_and_program(&mut self) -> bool {
        if !soft_reset(self.io_base) {
            self.recovery_failures += 1;
            return false;
        }
        self.program_registers();
        if self.interrupts_enabled {
            write16(self.io_base, REG_IMR, INTERRUPT_MASK);
        }
        true
    }

    /// Waits for slot `slot`'s previous frame to finish; false (and a recovery request) if it aborted, underran or never ended.
    fn finish_transmit(&mut self, slot: usize) -> bool {
        for _ in 0..1_000_000u32 {
            let status = read32(self.io_base, REG_TSD0 + slot as u16 * 4);
            if status & (TSD_TABT | TSD_TUN) != 0 {
                break;
            }
            if status & TSD_TOK != 0 {
                self.tx_pending[slot] = false;
                return true;
            }
        }
        self.tx_pending[slot] = false;
        self.recovery_pending = true;
        false
    }

    /// Queues one frame (padded to 60 bytes; the card appends the CRC). Asynchronous: a slot is only waited for when its
    /// previous frame is still in flight by the time the four-slot rotation comes round to it.
    pub fn send(&mut self, frame: &[u8]) -> bool {
        if frame.len() > MAX_FRAME {
            return false;
        }
        if !self.link_up() {
            self.frames_dropped_no_link += 1;
            return true;
        }
        let slot = self.tx_next;
        if self.tx_pending[slot] && !self.finish_transmit(slot) {
            return false;
        }
        self.tx_next = (slot + 1) % TX_SLOTS;
        let buffer = self.tx_buffers + (slot * 4096) as u64;
        let padded = frame.len().max(MIN_FRAME);
        unsafe {
            let destination = core::slice::from_raw_parts_mut(buffer as *mut u8, MAX_FRAME);
            destination[..frame.len()].copy_from_slice(frame);
            destination[frame.len()..padded].fill(0);
        }
        write32(self.io_base, REG_TSAD0 + slot as u16 * 4, buffer as u32);
        write32(self.io_base, REG_TSD0 + slot as u16 * 4, padded as u32); // starting the DMA
        self.tx_pending[slot] = true;
        self.frames_sent += 1;
        true
    }

    /// Copies the next received frame (without the CRC) into `out`; `None` if the ring is empty or the entry was bad.
    pub fn receive(&mut self, out: &mut [u8]) -> Option<usize> {
        self.check_link_recovery();
        self.recover();
        if read8(self.io_base, REG_CR) & CR_BUFE != 0 {
            return None;
        }
        let offset = self.rx_offset;
        let header = unsafe { core::ptr::read_unaligned((self.rx_buffer + u64::from(offset)) as *const u32) };
        let status = header & 0xffff;
        let frame_len = header >> 16; // includes the 4-byte CRC
        if frame_len < 4 || frame_len > RX_BUF_LEN {
            // The ring is desynchronised and there is no sane length to step over: reset the whole card (see the rules above).
            self.recovery_pending = true;
            return None;
        }
        // Step past the header and the frame, rounded up to 4 bytes, wrapping at the ring size, and tell the card (CAPR is
        // offset - 16). This happens for bad frames too, or the ring would wedge on the first one.
        let mut next = (offset + 4 + frame_len + 3) & !3;
        if next >= RX_BUF_LEN {
            next -= RX_BUF_LEN;
        }
        self.rx_offset = next;
        write16(self.io_base, REG_CAPR, next.wrapping_sub(16) as u16);
        if status & 1 == 0 {
            self.bad_frames += 1;
            return None;
        }
        let length = (frame_len as usize - 4).min(out.len()).min(MAX_FRAME);
        let source = unsafe { core::slice::from_raw_parts((self.rx_buffer + u64::from(offset) + 4) as *const u8, length) };
        out[..length].copy_from_slice(source);
        self.frames_received += 1;
        Some(length)
    }

    /// The `nic` command's driver-specific line.
    pub fn describe(&self) {
        crate::println!(
            "   rtl8139: I/O {:#06x}, {} bad frames, {} recoveries ({} overflows, {} failed), ring offset {}",
            self.io_base,
            self.bad_frames,
            self.recoveries,
            self.overflows,
            self.recovery_failures,
            self.rx_offset
        );
    }

    /// `nic regs`: the registers that say what the card thinks.
    pub fn dump_registers(&self) {
        let base = self.io_base;
        crate::println!(
            "RTL8139 I/O {:#06x}: CR {:02x}, ISR {:04x}, IMR {:04x}, RCR {:08x}, TCR {:08x}, MSR {:02x}, CAPR {:04x}, {:?}",
            base,
            read8(base, REG_CR),
            read16(base, REG_ISR),
            read16(base, REG_IMR),
            read32(base, REG_RCR),
            read32(base, REG_TCR),
            read8(base, REG_MSR),
            read16(base, REG_CAPR),
            self.delivery
        );
        crate::println!(
            "rx offset {}, tx next {}, pending {:?}, TSD {:08x} {:08x} {:08x} {:08x}",
            self.rx_offset,
            self.tx_next,
            self.tx_pending,
            read32(base, REG_TSD0),
            read32(base, REG_TSD0 + 4),
            read32(base, REG_TSD0 + 8),
            read32(base, REG_TSD0 + 12)
        );
    }
}
