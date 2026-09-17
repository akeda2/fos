//! Minimal polling-mode driver for the Realtec RTL8139 Fast Ethernet
//! controller -- a well-documented, widely emulated (QEMU's default `-nic`
//! model) and, for many years, extremely common real-hardware PCI NIC,
//! making it a reasonable single teaching target for "how does a network
//! card actually work" the same way one specific UART chip and one
//! specific PCI serial card were reasonable single targets earlier.
//!
//! Unlike the UART, an Ethernet controller does not hand bytes over one
//! at a time via port I/O: it moves whole frames itself, via DMA, into
//! and out of buffers whose *physical* address the driver hands it ahead
//! of time. There is no paging in this kernel, so physical and virtual
//! addresses are identical and those buffers can be read/written directly
//! through raw pointers -- but they must be actual memory the card can
//! reach, which is why this driver asks `pmem` for real physical frames
//! rather than using `static` kernel buffers.
//!
//! This driver only implements what is needed to send and receive raw
//! Ethernet frames by polling; it does not touch the card's interrupt
//! line (this project's PIC setup leaves it masked regardless -- see
//! `interrupts.rs`), and it does not implement any protocol above
//! Ethernet itself (no ARP, no IPv4). Multicast filtering, wake-on-LAN,
//! and the card's various statistics counters are all left untouched.

use core::cell::Cell;

use crate::arch::x86::{inb, inl, outb, outl, outw};
use crate::pci;
use crate::pmem;

const VENDOR_REALTEK: u16 = 0x10ec;
const DEVICE_RTL8139: u16 = 0x8139;

// Register offsets from the card's I/O base (BAR0).
const REG_IDR0: u16 = 0x00; // First byte of the 6-byte MAC address.
const REG_TSD0: u16 = 0x10; // Transmit status, descriptors 0-3 (4 bytes apart).
const REG_TSAD0: u16 = 0x20; // Transmit start address (physical), descriptors 0-3.
const REG_RBSTART: u16 = 0x30; // Receive buffer start address (physical).
const REG_CR: u16 = 0x37; // Command register.
const REG_CAPR: u16 = 0x38; // Current address of packet read.
const REG_IMR: u16 = 0x3c; // Interrupt mask (left at 0: see module docs).
const REG_ISR: u16 = 0x3e; // Interrupt status (latches regardless of IMR).
const REG_TCR: u16 = 0x40; // Transmit configuration.
const REG_RCR: u16 = 0x44; // Receive configuration.
const REG_CONFIG1: u16 = 0x52;

const CR_BUFE: u8 = 1 << 0; // Receive buffer empty.
const CR_TE: u8 = 1 << 2; // Transmitter enable.
const CR_RE: u8 = 1 << 3; // Receiver enable.
const CR_RST: u8 = 1 << 4; // Software reset.

/// Accept broadcast (AB), multicast (AM), and packets addressed to our
/// own MAC (APM); WRAP lets a packet straddle the end of the ring buffer
/// without wrapping mid-packet, at the cost of needing `RX_BUF_LEN` bytes
/// of slack at the end (see `RX_BUF_LEN`'s doc comment).
const RCR_CONFIG: u32 = (1 << 1) | (1 << 2) | (1 << 3) | (1 << 7);

/// Accept All Packets: with this bit set, the card hands over every frame
/// it sees on the wire regardless of destination address, instead of only
/// unicast/multicast/broadcast frames matched by `RCR_CONFIG`. Left off by
/// `init` (real traffic is noisy and rarely useful to see in full); toggled
/// on only for `set_promiscuous`, which backs the shell's `nic sniff`.
const RCR_AAP: u32 = 1 << 0;

const TSD_TOK: u32 = 1 << 15; // Transmit OK.
const TSD_TUN: u32 = 1 << 14; // Transmit FIFO underrun.
const TSD_TABT: u32 = 1 << 30; // Transmit abort.

/// `TCR`'s Interframe Gap Time field (bits 25:24), set to the standard
/// IEEE 802.3 gap (`11b`). Leaving this at its all-zero reset value (the
/// smallest gap the chip supports) still makes the card report a normal
/// `TOK` for every frame -- that only reflects the chip's own FIFO/abort
/// state, not whether anything downstream accepted the frame -- but real
/// switches and NICs can and do silently drop frames sent with too short
/// a gap between them, which QEMU's virtual NIC never enforces and so
/// never surfaces there. Linux's own historic RTL8139 driver
/// (`8139too.c`) forces this same value for the same reason.
const TCR_STANDARD_IFG: u32 = 0b11 << 24;

/// The receive ring is a plain, non-wrapping-mid-packet buffer: 8 KiB of
/// actual ring, plus 16 bytes the card's documentation reserves, plus
/// 1500 bytes of slack so the last packet before the wraparound point can
/// spill past the 8 KiB mark instead of being split. Rounded up to whole
/// 4 KiB frames, since `pmem` only ever hands out whole frames.
const RX_BUF_LEN: u32 = 8192;
const RX_BUF_TOTAL: u32 = RX_BUF_LEN + 16 + 1500;
const RX_BUF_FRAMES: usize = ((RX_BUF_TOTAL as usize) + 4095) / 4096;

const TX_DESCRIPTORS: usize = 4;
const TX_BUF_FRAMES: usize = 1; // One 4 KiB frame comfortably holds one max-size Ethernet frame.

pub const MIN_FRAME_LEN: usize = 60; // Minimum Ethernet payload+header, excluding the 4-byte FCS the card appends.
pub const MAX_FRAME_LEN: usize = 1514; // Standard maximum, excluding the FCS.

fn read8(base: u16, register: u16) -> u8 {
    unsafe { inb(base + register) }
}

fn write8(base: u16, register: u16, value: u8) {
    unsafe { outb(base + register, value) }
}

fn write16(base: u16, register: u16, value: u16) {
    unsafe { outw(base + register, value) }
}

fn read32(base: u16, register: u16) -> u32 {
    unsafe { inl(base + register) }
}

fn write32(base: u16, register: u16, value: u32) {
    unsafe { outl(base + register, value) }
}

pub struct Rtl8139 {
    io_base: u16,
    rx_buffer: u32,
    rx_offset: Cell<u16>,
    tx_buffers: [u32; TX_DESCRIPTORS],
    tx_next: Cell<usize>,
}

/// What went wrong trying to bring a detected card up. Detection (finding
/// the card at all) is reported separately by `probe` returning `None`.
#[derive(Debug)]
pub enum InitError {
    /// The DMA buffers this driver needs did not fit in tracked physical
    /// memory (see `pmem`).
    OutOfMemory,
    /// The card did not clear its reset bit within a generous bounded
    /// wait; likely not present/responding despite being found on PCI.
    ResetTimedOut,
}

impl Rtl8139 {
    /// Looks for exactly one Realtek RTL8139 on the PCI bus and, if
    /// found, enables its I/O space and bus-mastering (DMA) bits,
    /// returning its I/O base address (BAR0). Returns `None` if no such
    /// card is present, or if it is found but its BAR0 is not I/O-mapped.
    /// Use `init` on the result to actually bring the card up.
    pub fn probe() -> Option<u16> {
        let address = pci::find_device(VENDOR_REALTEK, DEVICE_RTL8139)?;
        address.enable_io_and_bus_master();
        address.io_bar0()
    }

    /// Resets the card, allocates its DMA buffers, and enables receiving
    /// and transmitting. Only one `Rtl8139` should exist per physical
    /// card at a time (each owns its own DMA buffers, allocated once here
    /// and never freed back to `pmem` -- this driver is not meant to be
    /// torn down and re-initialized repeatedly).
    pub fn init(io_base: u16) -> Result<Self, InitError> {
        // Power on, then software-reset, waiting for the card to clear
        // RST itself once the reset completes.
        write8(io_base, REG_CONFIG1, 0x00);
        write8(io_base, REG_CR, CR_RST);
        let mut attempts = 1_000_000u32;
        while read8(io_base, REG_CR) & CR_RST != 0 {
            attempts -= 1;
            if attempts == 0 {
                return Err(InitError::ResetTimedOut);
            }
        }

        let rx_buffer = pmem::allocate_contiguous(RX_BUF_FRAMES).ok_or(InitError::OutOfMemory)?;
        let mut tx_buffers = [0u32; TX_DESCRIPTORS];
        for slot in tx_buffers.iter_mut() {
            *slot = pmem::allocate_contiguous(TX_BUF_FRAMES).ok_or(InitError::OutOfMemory)?;
        }

        write32(io_base, REG_RBSTART, rx_buffer);
        write16(io_base, REG_IMR, 0x0000); // No interrupt line use; see module docs.
        write32(io_base, REG_RCR, RCR_CONFIG);
        write32(io_base, REG_TCR, TCR_STANDARD_IFG);
        write8(io_base, REG_CR, CR_TE | CR_RE);
        write16(io_base, REG_ISR, 0xffff); // Clear any latched status from before reset.

        Ok(Rtl8139 { io_base, rx_buffer, rx_offset: Cell::new(0), tx_buffers, tx_next: Cell::new(0) })
    }

    /// This card's burned-in MAC address, read from its `IDR0..IDR5`
    /// registers (populated by the card itself from an on-board EEPROM
    /// at power-on).
    pub fn mac_address(&self) -> [u8; 6] {
        let mut mac = [0u8; 6];
        for (index, byte) in mac.iter_mut().enumerate() {
            *byte = read8(self.io_base, REG_IDR0 + index as u16);
        }
        mac
    }

    /// Enables or disables promiscuous mode (`RCR`'s `AAP` bit, see its
    /// doc comment). Read-modify-write, since `RCR` also holds the
    /// unrelated accept-filter bits `init` set up in `RCR_CONFIG` and
    /// this must not disturb them.
    pub fn set_promiscuous(&self, enabled: bool) {
        let mut rcr = read32(self.io_base, REG_RCR);
        if enabled {
            rcr |= RCR_AAP;
        } else {
            rcr &= !RCR_AAP;
        }
        write32(self.io_base, REG_RCR, rcr);
    }

    /// Transmits one raw Ethernet frame (destination MAC + source MAC +
    /// ethertype + payload; the card appends the 4-byte FCS itself).
    /// Frames shorter than `MIN_FRAME_LEN` are padded with zero bytes, as
    /// real Ethernet requires. Returns `false` if `frame` is too long, or
    /// if the card reports a transmit error or does not finish within a
    /// generous bounded wait.
    pub fn send(&self, frame: &[u8]) -> bool {
        if frame.len() > MAX_FRAME_LEN {
            return false;
        }
        let descriptor = self.tx_next.get();
        self.tx_next.set((descriptor + 1) % TX_DESCRIPTORS);
        let buffer = self.tx_buffers[descriptor];

        // Physical memory is identity-mapped, so the DMA buffer's
        // physical address doubles as a plain pointer we can write
        // through directly.
        let destination = unsafe { core::slice::from_raw_parts_mut(buffer as *mut u8, MAX_FRAME_LEN) };
        destination[..frame.len()].copy_from_slice(frame);
        let padded_len = frame.len().max(MIN_FRAME_LEN);
        if padded_len > frame.len() {
            destination[frame.len()..padded_len].fill(0);
        }

        write32(self.io_base, REG_TSAD0 + descriptor as u16 * 4, buffer);
        write32(self.io_base, REG_TSD0 + descriptor as u16 * 4, padded_len as u32);

        let mut attempts = 1_000_000u32;
        loop {
            let status = read32(self.io_base, REG_TSD0 + descriptor as u16 * 4);
            if status & (TSD_TABT | TSD_TUN) != 0 {
                return false;
            }
            if status & TSD_TOK != 0 {
                return true;
            }
            attempts -= 1;
            if attempts == 0 {
                return false;
            }
        }
    }

    /// Copies out one received frame (if any is waiting) into `out`,
    /// returning its length -- clipped to `out.len()` if the frame is
    /// larger than the caller's buffer, though the ring is advanced by
    /// the *actual* frame length either way. Returns `None` if no frame
    /// is currently waiting; this does not block.
    pub fn receive(&self, out: &mut [u8]) -> Option<usize> {
        if read8(self.io_base, REG_CR) & CR_BUFE != 0 {
            return None; // Buffer empty register bit: nothing waiting.
        }

        let offset = self.rx_offset.get();
        let header = unsafe { core::ptr::read_unaligned((self.rx_buffer + u32::from(offset)) as *const u32) };
        let status = header & 0xffff;
        let frame_len = (header >> 16) as u16; // Includes the 4-byte FCS.

        if status & 0x1 == 0 || frame_len < 4 {
            // Not "receive OK", or a nonsensical length: resynchronizing
            // the ring here is out of scope for this teaching driver, so
            // just report nothing rather than risk reading garbage.
            return None;
        }

        let payload_len = usize::from(frame_len) - 4; // Drop the FCS.
        let copy_len = payload_len.min(out.len());
        let payload_start = self.rx_buffer + u32::from(offset) + 4;
        let payload = unsafe { core::slice::from_raw_parts(payload_start as *const u8, copy_len) };
        out[..copy_len].copy_from_slice(payload);

        // Advance past this packet's 4-byte header plus its length,
        // rounded up to a 4-byte boundary, wrapping at RX_BUF_LEN; then
        // tell the card via CAPR, which -- per the card's documented
        // quirk -- must be the new offset minus 16, not the offset
        // itself.
        let mut next_offset = u32::from(offset) + 4 + u32::from(frame_len);
        next_offset = (next_offset + 3) & !3;
        if next_offset >= RX_BUF_LEN {
            next_offset -= RX_BUF_LEN;
        }
        self.rx_offset.set(next_offset as u16);
        write16(self.io_base, REG_CAPR, next_offset.wrapping_sub(16) as u16);

        Some(copy_len)
    }
}

/// The one `Rtl8139` this kernel ever brings up, created lazily on first
/// use by `get`. A single instance is all this driver supports (its DMA
/// buffers are allocated once and never freed); this matches the
/// project's general approach of building exactly as much as one worked
/// example needs, not a general multi-device driver model.
static mut NIC: Option<Rtl8139> = None;

/// Why `get` could not make a card available. `NotPresent` means no
/// RTL8139 was found at all (the caller should say so and stop);
/// `InitFailed` means one was found but `Rtl8139::init` itself failed.
#[derive(Debug)]
pub enum Status {
    NotPresent,
    InitFailed(InitError),
}

/// Returns the singleton driver instance, probing and initializing the
/// card the first time this is called and simply reusing it on every
/// later call (repeated calls do not re-probe or re-reset the card).
pub fn get() -> Result<&'static Rtl8139, Status> {
    let nic = &raw mut NIC;
    unsafe {
        if (*nic).is_none() {
            let io_base = Rtl8139::probe().ok_or(Status::NotPresent)?;
            let initialized = Rtl8139::init(io_base).map_err(Status::InitFailed)?;
            *nic = Some(initialized);
        }
        Ok((*nic).as_ref().unwrap())
    }
}
