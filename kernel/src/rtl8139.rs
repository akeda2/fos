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
//! of time. The kernel's first paging configuration identity-maps DMA memory,
//! so these physical addresses remain usable as raw-pointer addresses. They
//! must still be actual memory the card can reach, which is why this driver
//! asks `pmem` for real physical frames rather than using `static` kernel
//! buffers.
//!
//! This driver only implements what is needed to send and receive raw
//! Ethernet frames by polling; legacy interrupts only acknowledge the card
//! and mark foreground receive work pending, and it does not implement any protocol above
//! Ethernet itself (no ARP, no IPv4). Multicast filtering, wake-on-LAN,
//! and the card's various statistics counters are all left untouched.

use core::cell::Cell;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::arch::x86::{inb, inl, inw, outb, outl, outw};
use crate::nic::{Health, WatchdogResult};
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
const REG_IMR: u16 = 0x3c; // Interrupt mask.
const REG_ISR: u16 = 0x3e; // Interrupt status (latches regardless of IMR).
const REG_TCR: u16 = 0x40; // Transmit configuration.
const REG_RCR: u16 = 0x44; // Receive configuration.
const REG_CONFIG1: u16 = 0x52;
const REG_MSR: u16 = 0x58; // Media Status Register.

/// `MSR`'s "Linkage Bad" bit: `1` while the physical link is down, `0`
/// once it is established. Polled by `check_link_recovery` to detect a
/// down-to-up transition; see that function's doc comment for why this
/// driver needs to react to one at all.
const MSR_LINKB: u8 = 1 << 2;

const CR_BUFE: u8 = 1 << 0; // Receive buffer empty.
const CR_TE: u8 = 1 << 2; // Transmitter enable.
const CR_RE: u8 = 1 << 3; // Receiver enable.
const CR_RST: u8 = 1 << 4; // Software reset.

/// Accept broadcast (AB), multicast (AM), and packets addressed to our
/// own MAC (APM); WRAP lets a packet straddle the end of the ring buffer
/// without wrapping mid-packet, at the cost of needing `RX_BUF_LEN` bytes
/// of slack at the end (see `RX_BUF_LEN`'s doc comment); `RCR_RBLEN_32K`
/// selects a larger-than-default ring size (see its own doc comment).
const RCR_CONFIG: u32 = (1 << 1) | (1 << 2) | (1 << 3) | (1 << 7) | RCR_RBLEN_32K;

/// Accept All Packets: with this bit set, the card hands over every frame
/// it sees on the wire regardless of destination address, instead of only
/// unicast/multicast/broadcast frames matched by `RCR_CONFIG`. Left off by
/// `init` (real traffic is noisy and rarely useful to see in full); toggled
/// on only for `set_promiscuous`, which backs the shell's `nic sniff`.
const RCR_AAP: u32 = 1 << 0;

/// `RCR`'s Rx Buffer Length field (bits 12:11): `10b` selects a 32 KiB
/// ring, four times the reset default (`00b`, 8 KiB -- `RX_BUF_LEN`'s
/// previous value). The largest option, `11b`/64 KiB, is deliberately
/// avoided: at 64 KiB the ring already spans the entire 16-bit range
/// `CAPR`/the ring offset can address, so `RCR_CONFIG`'s `WRAP` bit --
/// which works by letting a packet spill a little past the ring's
/// nominal end into reserved slack space -- has nowhere well-defined
/// left to spill *to*, since "past the end" and "wrapped back to the
/// start" become the same address; this was confirmed to actually
/// corrupt frame data (silently splitting/misaligning a packet) rather
/// than merely being untested, when tried against QEMU's emulated card.
/// A polling-only driver like this one, with no interrupt to react to a
/// frame the instant it lands, is far more exposed to a fast, unpaced
/// sender than an interrupt-driven one would be (see `ISR_RXOVW`'s doc
/// comment): giving the card four times the room to buffer frames while
/// this driver's poll loop catches up measurably reduces -- though, since
/// real bursts are only bounded by wire speed and this loop still has to
/// actually run its own logic between polls, does not entirely eliminate
/// -- how often `recover` has to trigger, and how much tape
/// data is lost to it, under a real, unpaced `send_tape.py` run.
const RCR_RBLEN_32K: u32 = 0b10 << 11;

const TSD_TOK: u32 = 1 << 15; // Transmit OK.
const TSD_TUN: u32 = 1 << 14; // Transmit FIFO underrun.
const TSD_TABT: u32 = 1 << 30; // Transmit abort.

/// `ISR`'s Rx Buffer Overflow bit: the ring filled up faster than
/// software drained it (this driver only polls; it never services an
/// interrupt) and the card had nowhere left to DMA an incoming frame.
/// This is expected to happen under a fast, unpaced burst of frames --
/// e.g. `tools/send_tape.py` run with no `--delay` against real hardware,
/// which can offer frames back-to-back far faster than the emulated NIC
/// QEMU presents ever does -- and, once it happens, the ring's read/write
/// pointers are left desynchronized: without an explicit recovery step,
/// every later `receive` call either sees a permanently non-empty buffer
/// that never yields a sane frame again, or silently reads garbage.
const ISR_RXOVW: u16 = 1 << 4;
/// `ISR`'s Rx FIFO Overflow bit: the tiny on-chip FIFO in front of the
/// ring itself could not be drained to the ring fast enough. Distinct
/// from `ISR_RXOVW` but calls for the identical recovery, so both are
/// treated the same way here.
const ISR_FOVW: u16 = 1 << 6;
const ISR_ROK: u16 = 1 << 0;
const ISR_RER: u16 = 1 << 1;
const RTL_INTERRUPT_MASK: u16 = ISR_ROK | ISR_RER | ISR_RXOVW | ISR_FOVW;

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

/// The receive ring is a plain, non-wrapping-mid-packet buffer: 32 KiB of
/// actual ring (see `RCR_RBLEN_32K`'s doc comment for why not the
/// largest, 64 KiB, option), plus 16 bytes the card's documentation
/// reserves, plus 1500 bytes of slack so the last packet before the
/// wraparound point can spill past the ring's end instead of being
/// split. Rounded up to whole 4 KiB frames, since `pmem` only ever hands
/// out whole frames.
const RX_BUF_LEN: u32 = 32768;
const RX_BUF_TOTAL: u32 = RX_BUF_LEN + 16 + 1500;
const RX_BUF_FRAMES: usize = ((RX_BUF_TOTAL as usize) + 4095) / 4096;

const TX_DESCRIPTORS: usize = 4;
const TX_BUF_FRAMES: usize = 1; // One 4 KiB frame comfortably holds one max-size Ethernet frame.

pub const MIN_FRAME_LEN: usize = 60; // Minimum Ethernet payload+header, excluding the 4-byte FCS the card appends.
pub const MAX_FRAME_LEN: usize = 1514; // Standard maximum, excluding the FCS.

fn read8(base: u16, register: u16) -> u8 {
    unsafe { inb(base + register) }
}

#[derive(Clone, Copy)]
pub struct ProbeInfo {
    io_base: u16,
    irq: Option<u8>,
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
    /// Count of Rx buffer/FIFO overflow recoveries performed so far (see
    /// `recover`). Exposed only for diagnostics (e.g. the
    /// shell's `tape load net` can report it if a transfer stalls); it
    /// does not affect driver behavior.
    overflow_count: Cell<u32>,
    recovery_count: Cell<u32>,
    rx_progress: Cell<u32>,
    tx_progress: Cell<u32>,
    recovery_failures: Cell<u32>,
    recovery_pending: AtomicBool,
    /// Last-observed link state (`MSR_LINKB`, inverted), used by
    /// `check_link_recovery` to detect a down-to-up transition. Seeded
    /// from the card's actual state in `init`/`recover`, not assumed.
    link_up: Cell<bool>,
    promiscuous: Cell<bool>,
    irq: Option<u8>,
    interrupt_count: AtomicU32,
    interrupt_errors: AtomicU32,
    rx_overruns: AtomicU32,
    work_pending: AtomicBool,
    overflow_pending: AtomicBool,
    interrupts_enabled: Cell<bool>,
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
    /// Finds every Realtek RTL8139 function with a usable I/O BAR and
    /// enables I/O space and bus mastering for each one.
    pub fn probe_all(out: &mut [ProbeInfo]) -> usize {
        let mut addresses = [None; pci::MAX_DEVICE_MATCHES];
        let found = pci::find_devices(
            VENDOR_REALTEK,
            &[DEVICE_RTL8139],
            &mut addresses,
        );
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

    /// Resets the card, allocates its DMA buffers, and enables receiving
    /// and transmitting. Each instance owns its own DMA buffers, allocated
    /// once here and never freed back to `pmem`; the driver is not meant to
    /// be torn down and re-initialized repeatedly.
    pub fn init(io_base: u16, irq: Option<u8>) -> Result<Self, InitError> {
        Self::soft_reset(io_base)?;

        let rx_buffer = pmem::allocate_contiguous(RX_BUF_FRAMES).ok_or(InitError::OutOfMemory)?;
        let mut tx_buffers = [0u32; TX_DESCRIPTORS];
        for slot in tx_buffers.iter_mut() {
            *slot = pmem::allocate_contiguous(TX_BUF_FRAMES).ok_or(InitError::OutOfMemory)?;
        }

        Self::program_registers(io_base, rx_buffer);

        Ok(Rtl8139 {
            io_base,
            rx_buffer,
            rx_offset: Cell::new(0),
            tx_buffers,
            tx_next: Cell::new(0),
            overflow_count: Cell::new(0),
            recovery_count: Cell::new(0),
            rx_progress: Cell::new(0),
            tx_progress: Cell::new(0),
            recovery_failures: Cell::new(0),
            recovery_pending: AtomicBool::new(cfg!(feature = "nic-watchdog-test")),
            link_up: Cell::new(read8(io_base, REG_MSR) & MSR_LINKB == 0),
            promiscuous: Cell::new(false),
            irq,
            interrupt_count: AtomicU32::new(0),
            interrupt_errors: AtomicU32::new(0),
            rx_overruns: AtomicU32::new(0),
            work_pending: AtomicBool::new(false),
            overflow_pending: AtomicBool::new(false),
            interrupts_enabled: Cell::new(false),
        })
    }

    /// Power on, then software-reset, waiting for the card to clear
    /// `CR_RST` itself once the reset completes. Shared by `init` and by
    /// `recover`'s full-reset recovery path.
    fn soft_reset(io_base: u16) -> Result<(), InitError> {
        write8(io_base, REG_CONFIG1, 0x00);
        write8(io_base, REG_CR, CR_RST);
        let mut attempts = 1_000_000u32;
        while read8(io_base, REG_CR) & CR_RST != 0 {
            attempts -= 1;
            if attempts == 0 {
                return Err(InitError::ResetTimedOut);
            }
        }
        Ok(())
    }

    /// (Re-)programs every register a freshly reset card needs before it
    /// will actually send/receive, pointing it at `rx_buffer` as its
    /// receive ring. Shared by `init` (first bring-up) and
    /// `recover` (recovering after a full reset, reusing
    /// the same DMA buffer rather than allocating a new one).
    fn program_registers(io_base: u16, rx_buffer: u32) {
        write32(io_base, REG_RBSTART, rx_buffer);
        write16(io_base, REG_IMR, 0x0000);
        // The transmitter/receiver enable bits must be set in CR *before*
        // RCR/TCR are written, not after: those two registers also carry
        // FIFO/DMA threshold fields, and on real silicon (unlike QEMU's
        // more forgiving emulated card) writing them while Tx/Rx are still
        // disabled leaves those threshold fields not properly latched --
        // the card still reports a normal-looking TOK for every send, but
        // nothing usable ever reaches the wire. Linux's 8139too.c driver
        // documents this exact ordering requirement.
        write8(io_base, REG_CR, CR_TE | CR_RE);
        write32(io_base, REG_RCR, RCR_CONFIG);
        write32(io_base, REG_TCR, TCR_STANDARD_IFG);
        write16(io_base, REG_ISR, 0xffff); // Clear any latched status from before reset.
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
        self.promiscuous.set(enabled);
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
        let destination =
            unsafe { core::slice::from_raw_parts_mut(buffer as *mut u8, MAX_FRAME_LEN) };
        destination[..frame.len()].copy_from_slice(frame);
        let padded_len = frame.len().max(MIN_FRAME_LEN);
        if padded_len > frame.len() {
            destination[frame.len()..padded_len].fill(0);
        }

        write32(self.io_base, REG_TSAD0 + descriptor as u16 * 4, buffer);
        write32(
            self.io_base,
            REG_TSD0 + descriptor as u16 * 4,
            padded_len as u32,
        );

        let mut attempts = 1_000_000u32;
        loop {
            let status = read32(self.io_base, REG_TSD0 + descriptor as u16 * 4);
            if status & (TSD_TABT | TSD_TUN) != 0 {
                self.recovery_pending.store(true, Ordering::Release);
                return false;
            }
            if status & TSD_TOK != 0 {
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

    /// Recovers a latched Rx buffer/FIFO overflow (`ISR_RXOVW`/`ISR_FOVW`),
    /// a bounded transmit failure, or a ring entry whose length field is
    /// itself nonsensical (see `receive`'s own doc comment: confirmed on
    /// real hardware after a physical link down/up cycle) with a full
    /// software reset
    /// (`soft_reset`) followed by reprogramming every register a fresh
    /// card needs (`program_registers`, reusing the same DMA buffer
    /// rather than allocating a new one) and resetting the ring's read
    /// pointer (`CAPR`, `rx_offset`) to the very start of the buffer.
    /// Testing this driver against real hardware showed that a lighter
    /// recovery -- just toggling `CR_RE` off and on, even while also
    /// rewriting `RBSTART`/`RCR` -- was not enough: the card was left
    /// unable to receive anything at all afterwards, in a way that
    /// outlasted leaving and re-entering `tape load net` and needed a
    /// full reboot to clear. A full software reset is more disruptive
    /// (transmit state is reset too, and this pauses receiving for
    /// longer than the lighter approach would have), but is what actually
    /// leaves the card working afterwards, matching the full
    /// reset-and-reinitialize recovery real-world drivers (e.g. Linux's
    /// historic `8139too.c`) document using for this exact condition.
    /// Called by foreground receive polling and the facade watchdog, so a burst that overflows
    /// the ring (real hardware can be sent frames back-to-back far faster
    /// than QEMU's emulated NIC ever presents them, e.g. `send_tape.py`
    /// run with no `--delay`) recovers into a genuinely working card on
    /// its own on the next poll, instead of the ring wedging forever.
    /// Any frame(s) lost to the overflow are simply gone, exactly as on
    /// any other Ethernet link that dropped them.
    ///
    /// Detects a physical link recovery via `MSR`'s `LinkB` bit and, if
    /// one just happened, flags a full reset the same way an overflow or
    /// bounded transmit failure already does. This is a *separate* real-
    /// hardware finding from the ring-wedge fix in `receive` above: that
    /// fix only helps once garbled bytes actually land in the ring, but
    /// real testing showed a cable pull/replug on an RTL8139-backed
    /// uplink left it completely unable to receive anything ever again --
    /// with the link itself cleanly back up (this bit confirms that) and
    /// nothing ever landing in the ring to trigger the other fix, this
    /// card's receiver logic simply never resumed on its own. Linux's
    /// historic `8139too.c` documents the identical requirement: it polls
    /// this same link-status bit and performs a full re-init
    /// (`rtl8139_hw_start`) on a down-to-up transition rather than
    /// trusting the card to resume Rx by itself. A router uplink that
    /// never rejoined fail-over after its cable was reconnected, while an
    /// otherwise-identical setup using only 3C905 cards recovered fine,
    /// is exactly what this quirk predicts.
    fn check_link_recovery(&self) {
        let link_now = read8(self.io_base, REG_MSR) & MSR_LINKB == 0;
        let was_up = self.link_up.replace(link_now);
        if link_now && !was_up {
            self.recovery_pending.store(true, Ordering::Release);
        }
    }

    fn recover(&self) -> bool {
        let isr = read16(self.io_base, REG_ISR);
        let had_overflow = self.overflow_pending.swap(false, Ordering::AcqRel)
            || isr & (ISR_RXOVW | ISR_FOVW) != 0;
        if !had_overflow
            && !self.recovery_pending.swap(false, Ordering::AcqRel)
            && isr & (ISR_RXOVW | ISR_FOVW) == 0
        {
            return false;
        }

        // A reset that fails to complete within `soft_reset`'s generous
        // bounded wait leaves nothing sane to recover into; there is no
        // sane fallback but to leave the card as-is and let the next
        // `receive` call try again.
        if Self::soft_reset(self.io_base).is_err() {
            self.recovery_failures.set(self.recovery_failures.get().wrapping_add(1));
            return false;
        }
        Self::program_registers(self.io_base, self.rx_buffer);
        if self.promiscuous.get() {
            self.set_promiscuous(true);
        }
        if self.interrupts_enabled.get() {
            self.enable_interrupts();
        }
        self.rx_offset.set(0);
        write16(self.io_base, REG_CAPR, 0u16.wrapping_sub(16));

        if had_overflow {
            self.overflow_count.set(self.overflow_count.get().wrapping_add(1));
        }
        self.recovery_count.set(self.recovery_count.get().wrapping_add(1));
        true
    }

    /// How many times `receive` has detected and recovered from an Rx
    /// buffer/FIFO overflow so far. Diagnostic only (see
    /// `recover`); a nonzero, growing count while a
    /// transfer is in progress is a strong sign frames are being lost to
    /// the sender running faster than this driver's polling loop can
    /// drain the ring, which `tape load net` reports to help tell that
    /// apart from other causes of a stalled transfer.
    pub fn overflow_count(&self) -> u32 {
        self.overflow_count.get()
    }

    pub fn health(&self) -> Health {
        Health {
            rx_progress: self.rx_progress.get(),
            tx_progress: self.tx_progress.get(),
            recoveries: self.recovery_count.get(),
            recovery_failures: self.recovery_failures.get(),
            recovery_pending: self.overflow_pending.load(Ordering::Acquire)
                || self.recovery_pending.load(Ordering::Acquire),
            rx_overruns: self.rx_overruns.load(Ordering::Relaxed),
        }
    }

    pub fn watchdog(&self) -> WatchdogResult {
        self.check_link_recovery();
        if !self.health().recovery_pending
            && read16(self.io_base, REG_ISR) & (ISR_RXOVW | ISR_FOVW) == 0
        {
            return WatchdogResult::Healthy;
        }

        if self.recover() {
            WatchdogResult::Recovered
        } else {
            WatchdogResult::Failed
        }
    }

    pub fn enable_interrupts(&self) {
        write16(self.io_base, REG_ISR, 0xffff);
        write16(self.io_base, REG_IMR, RTL_INTERRUPT_MASK);
        self.interrupts_enabled.set(true);
    }

    pub fn handle_interrupt(&self) {
        let status = read16(self.io_base, REG_ISR) & RTL_INTERRUPT_MASK;
        if status == 0 {
            return;
        }
        write16(self.io_base, REG_ISR, status);
        self.interrupt_count.fetch_add(1, Ordering::Relaxed);
        if status & (ISR_RER | ISR_RXOVW | ISR_FOVW) != 0 {
            self.interrupt_errors.fetch_add(1, Ordering::Relaxed);
        }
        if status & (ISR_RXOVW | ISR_FOVW) != 0 {
            self.rx_overruns.fetch_add(1, Ordering::Relaxed);
            self.overflow_pending.store(true, Ordering::Release);
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
    /// larger than the caller's buffer, though the ring is advanced by
    /// the *actual* frame length either way. Returns `None` if no frame
    /// is currently waiting; this does not block.
    pub fn receive(&self, out: &mut [u8]) -> Option<usize> {
        self.work_pending.store(false, Ordering::Release);
        self.check_link_recovery();
        self.recover();

        if read8(self.io_base, REG_CR) & CR_BUFE != 0 {
            return None; // Buffer empty register bit: nothing waiting.
        }

        let offset = self.rx_offset.get();
        let header = unsafe {
            core::ptr::read_unaligned((self.rx_buffer + u32::from(offset)) as *const u32)
        };
        let status = header & 0xffff;
        let frame_len = (header >> 16) as u16; // Includes the 4-byte FCS.

        if frame_len < 4 || u32::from(frame_len) > RX_BUF_LEN {
            // The length field itself is nonsensical, meaning the ring is
            // desynchronized (or we are reading garbage): there is no sane
            // byte count to advance past, so resynchronizing in place is
            // out of scope for this teaching driver. Left alone, this was
            // a real, confirmed wedge: real hardware showed a physical
            // cable unplug/replug cycle can leave stray bytes at the
            // current read offset (link-training noise landing in the DMA
            // buffer), and with nothing here ever advancing rx_offset/CAPR
            // or flagging a fault, this exact same bogus header was read
            // again on every later poll forever -- the NIC looked
            // permanently dead (no more frames ever received) until a full
            // reboot, exactly as an RTL8139-backed uplink failing to
            // rejoin a router's fail-over rotation after its link came
            // back. Flagging this for the existing full-reset recovery
            // path (already proven correct for overflow/TX-failure, see
            // `recover`'s own doc comment) is the same fix in spirit as
            // that overflow work: the ring's read pointer cannot be
            // trusted to resynchronize itself, so the whole card is reset
            // and reprogrammed from scratch instead, which is disruptive
            // but the only state this driver knows for certain is sane.
            self.recovery_pending.store(true, Ordering::Release);
            return None;
        }

        // Advance past this packet's 4-byte header plus its length,
        // rounded up to a 4-byte boundary, wrapping at RX_BUF_LEN; then
        // tell the card via CAPR, which -- per the card's documented
        // quirk -- must be the new offset minus 16, not the offset
        // itself. This must happen even for a frame the card marked as
        // not "receive OK" (a real CRC/alignment/runt error, more likely
        // under a fast back-to-back burst like `tape`'s than the
        // occasional `chat`/`nic discover` frame): the length field is
        // still meaningful for those, and every entry in the ring -- good
        // or bad -- must be advanced past, or the whole ring wedges
        // forever on the first bad frame, with every later, perfectly
        // good frame behind it in the ring never becoming reachable.
        let mut next_offset = u32::from(offset) + 4 + u32::from(frame_len);
        next_offset = (next_offset + 3) & !3;
        if next_offset >= RX_BUF_LEN {
            next_offset -= RX_BUF_LEN;
        }
        self.rx_offset.set(next_offset as u16);
        write16(self.io_base, REG_CAPR, next_offset.wrapping_sub(16) as u16);

        if status & 0x1 == 0 {
            return None; // Not receive-OK: ring advanced, nothing to report.
        }

        let payload_len = usize::from(frame_len) - 4; // Drop the FCS.
        let copy_len = payload_len.min(out.len());
        let payload_start = self.rx_buffer + u32::from(offset) + 4;
        let payload = unsafe { core::slice::from_raw_parts(payload_start as *const u8, copy_len) };
        out[..copy_len].copy_from_slice(payload);

        self.rx_progress.set(self.rx_progress.get().wrapping_add(1));
        Some(copy_len)
    }
}

const MAX_INSTANCES: usize = pci::MAX_DEVICE_MATCHES;
static mut NICS: [Option<Rtl8139>; MAX_INSTANCES] = [const { None }; MAX_INSTANCES];
static mut NIC_COUNT: usize = 0;
static mut INITIALIZED: bool = false;

/// Why `get` could not make a card available. `NotPresent` means no
/// RTL8139 was found at all (the caller should say so and stop);
/// `InitFailed` means one was found but `Rtl8139::init` itself failed.
#[derive(Debug)]
pub enum Status {
    NotPresent,
    InitFailed,
}

/// Probes and initializes every usable RTL8139, retaining each instance's
/// DMA buffers for the remainder of the boot.
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
            let found = Rtl8139::probe_all(&mut probe_info);
            let mut first_error = None;
            for info in &probe_info[..found] {
                match Rtl8139::init(info.io_base, info.irq) {
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

pub fn get(index: usize) -> Option<&'static Rtl8139> {
    unsafe {
        if index >= NIC_COUNT {
            return None;
        }
        NICS[index].as_ref()
    }
}
