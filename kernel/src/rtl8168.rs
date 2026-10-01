//! Polling-mode driver for Realtek's RTL8168/RTL8111 family of PCIe Gigabit
//! Ethernet controllers -- the chips behind most cheap PCIe gigabit cards
//! (such as the TP-Link TG-3468, an RTL8168H) and behind the onboard LAN of
//! a great many motherboards from the late 2000s onward.
//!
//! **Status: written from the public programming model (Linux `r8169`,
//! FreeBSD `re(4)`) and not yet verified.** QEMU has no RTL8168 model, so like
//! `i350.rs` and `threecom.rs` every bit of runtime verification is a
//! real-hardware-only cost. `init` therefore logs what it finds (PCI revision,
//! the chip version bits in `TxConfig`, MAC address, PHY link state) so a first
//! run on real silicon teaches us what each chip actually is. Anything this
//! driver does not recognise is still brought up the same way and reported as
//! unverified rather than refused: a card that merely fails to pass traffic
//! costs nothing, while the other NICs keep working.
//!
//! # Two device IDs, one family
//!
//! `0x8168` is the usual ID; `0x8161` is another ID Linux's `r8169` binds to
//! the same register model (seen on one test machine's cable-less onboard LAN
//! beside a `0x8168` add-in card). The ID does not name the silicon -- Realtek
//! reuses it across many revisions -- so `init` reads the version bits Linux
//! uses instead: `TxConfig` (offset `0x40`) bits 30:20, masked with `0xfcf`
//! ("XID"). For the RTL8168G and every later part (XID `0x4c0` and up in
//! Linux's table, including the 8168H at `0x540`/`0x541`) the MACs must be
//! enabled in `CR` *after* `TxConfig`/`RxConfig` are written; older parts want
//! them enabled before. That ordering is the only chip-specific branch here.
//!
//! # What this driver deliberately leaves out
//!
//! Linux programs a long list of per-chip magic for these parts: PHY and ePHY
//! tuning tables, MAC OCP and ERI register writes for flow control, EEE and
//! ASPM, clock request, and wake-on-LAN. They affect power, performance and
//! corner cases rather than whether a basic link passes frames, so they are
//! left out until real hardware shows a symptom that needs one. Checksum
//! offload, VLAN stripping, TSO, jumbo frames and multicast filtering are all
//! off or wide open: `CPlusCmd`'s checksum and VLAN-strip bits are explicitly
//! cleared so frames reach the tape exactly as they were on the wire.
//!
//! # Hardware model in brief
//!
//! The card is reached through its I/O-port BAR0 (the same register map is
//! also exposed in memory BAR2; ports avoid needing a `paging::map_mmio`
//! window). Frames move by DMA through two rings of 16-byte descriptors, one
//! for receive and one for transmit, whose physical base addresses are written
//! to `RDSAR`/`TNPDS`. Each descriptor is `{ opts1, opts2, addr_low,
//! addr_high }`: `opts1` carries the OWN bit (set = the card owns it), the
//! EOR bit (marks the ring's last entry, where the card wraps), the FS/LS
//! (first/last segment) bits and a 14-bit length. The driver fills a transmit
//! descriptor, hands it over by setting OWN, then rings the doorbell
//! (`TxPoll`); for receive it hands the card empty buffers and finds finished
//! ones by OWN having been cleared. Physical memory is identity-mapped, so the
//! DMA addresses are also plain pointers -- see `rtl8139.rs`.

use core::cell::Cell;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering, fence};

use crate::arch::x86::{inb, inl, inw, outb, outl, outw};
use crate::interrupts;
use crate::nic::{Health, WatchdogResult};
use crate::pci;
use crate::pmem;
use crate::println;

const VENDOR_REALTEK: u16 = 0x10ec;
const DEVICE_RTL8168: u16 = 0x8168;
const DEVICE_RTL8161: u16 = 0x8161;

pub const MIN_FRAME_LEN: usize = 60; // Header + payload, excluding the 4-byte FCS the card appends.
pub const MAX_FRAME_LEN: usize = 1514; // Standard maximum, excluding the FCS.

// Register offsets from the I/O base (BAR0).
const REG_IDR0: u16 = 0x00; // First byte of the 6-byte MAC address.
const REG_MAR0: u16 = 0x08; // 64-bit multicast hash filter, 8 bytes.
const REG_TNPDS_LOW: u16 = 0x20; // Transmit (normal priority) descriptor ring, physical low.
const REG_TNPDS_HIGH: u16 = 0x24; // ... high 32 bits.
const REG_CR: u16 = 0x37; // Command register.
const REG_TPPOLL: u16 = 0x38; // Transmit poll (doorbell).
const REG_IMR: u16 = 0x3c; // Interrupt mask.
const REG_ISR: u16 = 0x3e; // Interrupt status (write 1 to clear).
const REG_TXCONFIG: u16 = 0x40; // Transmit configuration; bits 30:20 also hold the chip version.
const REG_RXCONFIG: u16 = 0x44; // Receive configuration.
const REG_9346CR: u16 = 0x50; // EEPROM/config-register write-protect control.
const REG_PHYSTATUS: u16 = 0x6c; // PHY status: link, speed, duplex.
const REG_RMS: u16 = 0xda; // Maximum receive packet size.
const REG_CPLUSCMD: u16 = 0xe0; // C+ command (offload and VLAN switches).
const REG_RDSAR_LOW: u16 = 0xe4; // Receive descriptor ring, physical low.
const REG_RDSAR_HIGH: u16 = 0xe8; // ... high 32 bits.
const REG_MAXTXPKT: u16 = 0xec; // Maximum transmit packet size, in 128-byte units.

const CR_RST: u8 = 0x10; // Software reset; the card clears it when done.
const CR_RE: u8 = 0x08; // Receiver enable.
const CR_TE: u8 = 0x04; // Transmitter enable.
const TPPOLL_NPQ: u8 = 0x40; // Poll the normal-priority transmit queue.
const TPPOLL_FSWINT: u8 = 0x01; // Forced software interrupt (raises `ISR_SWINT`).
const CFG9346_UNLOCK: u8 = 0xc0; // Allow writes to the config registers.
const CFG9346_LOCK: u8 = 0x00;

// Interrupt status/mask bits.
const ISR_ROK: u16 = 0x0001; // Frame received.
const ISR_TOK: u16 = 0x0004; // Frame transmitted (enabled only during the MSI self-test).
const ISR_RER: u16 = 0x0002; // Receive error.
const ISR_RDU: u16 = 0x0010; // Receive descriptor unavailable (overflow).
const ISR_LINKCHG: u16 = 0x0020; // PHY link changed.
const ISR_SWINT: u16 = 0x0100; // Software interrupt, raised on demand via `TPPOLL_FSWINT`.
const ISR_FOVW: u16 = 0x0040; // Receive FIFO overflow.
const ISR_SYSERR: u16 = 0x8000; // PCI system error.
/// Everything the handler wants to hear about. Transmit completions are
/// polled in `send`, so no transmit interrupt is enabled.
/// `ISR_SWINT` is included so the MSI self-test can force an interrupt; it only
/// ever fires when `trigger_test_interrupt` asks for it.
const INTERRUPT_MASK: u16 =
    ISR_ROK | ISR_RER | ISR_RDU | ISR_LINKCHG | ISR_FOVW | ISR_SYSERR | ISR_SWINT;
/// Receive overruns: the descriptor ring ran out (RDU) or the on-chip FIFO
/// overflowed (FOVW) because the foreground was busy. Frames are lost, but the
/// receiver keeps working once descriptors are returned (Linux's `r8169` only
/// counts them on these chips), so they are not reset causes. The handler masks
/// them until the foreground has drained the ring, so a flood of dropped frames
/// cannot become an interrupt flood.
const SOFT_MASK: u16 = ISR_RDU | ISR_FOVW;
/// A PCI system error: the one cause that wants a full reset.
const FATAL_MASK: u16 = ISR_SYSERR;

/// `TxConfig`: inter-frame gap 3 (the IEEE 802.3 standard gap, bits 25:24),
/// unlimited DMA burst (7 in bits 10:8), and the auto-FIFO bit (7) newer parts
/// expect.
const TXCONFIG_VALUE: u32 = (3 << 24) | (7 << 8) | (1 << 7);
/// `RxConfig` accept bits.
const RX_ACCEPT_ALL_PHYS: u32 = 0x01; // Promiscuous.
const RX_ACCEPT_MY_PHYS: u32 = 0x02; // Frames addressed to our MAC.
const RX_ACCEPT_MULTICAST: u32 = 0x04;
const RX_ACCEPT_BROADCAST: u32 = 0x08;
/// `RxConfig`: no receive FIFO threshold (7 in bits 15:13) and unlimited DMA
/// burst (7 in bits 10:8), plus the normal accept filter.
const RXCONFIG_VALUE: u32 = (7 << 13)
    | (7 << 8)
    | RX_ACCEPT_MY_PHYS
    | RX_ACCEPT_MULTICAST
    | RX_ACCEPT_BROADCAST;

/// `CPlusCmd` bits cleared at start-up: receive checksum offload and VLAN tag
/// stripping, either of which would alter frames before the tape sees them.
const CPLUS_RX_CHECKSUM: u16 = 1 << 5;
const CPLUS_RX_VLAN: u16 = 1 << 6;

/// Chip versions at and above this XID (RTL8168G onward, including the 8168H)
/// need the MACs enabled after `TxConfig`/`RxConfig`; see the module comment.
const XID_G_AND_LATER: u16 = 0x4c0;

// PHYstatus bits.
const PHYSTATUS_FULL_DUPLEX: u8 = 0x01;
const PHYSTATUS_LINK: u8 = 0x02;
const PHYSTATUS_10: u8 = 0x04;
const PHYSTATUS_100: u8 = 0x08;
const PHYSTATUS_1000: u8 = 0x10;

// Descriptor `opts1` bits.
const DESC_OWN: u32 = 1 << 31; // The card owns this descriptor.
const DESC_EOR: u32 = 1 << 30; // End of ring: the card wraps after this one.
const DESC_FS: u32 = 1 << 29; // First segment of a frame.
const DESC_LS: u32 = 1 << 28; // Last segment of a frame.
const DESC_RX_RES: u32 = 1 << 21; // Receive error summary.
const DESC_LENGTH_MASK: u32 = 0x3fff;

const DESCRIPTOR_BYTES: usize = 16;
/// 64 receive descriptors (16 B each, 2 KiB buffers): 16 held only ~2 ms of
/// line-rate traffic, far less than the foreground's worst-case 10 ms sleep
/// while a tape forwards frames between NICs.
const RX_COUNT: usize = 64;
/// Transmit ring depth. `send` is asynchronous: it posts a frame and returns,
/// waiting only when the descriptor it needs is still owned by the card. 32
/// descriptors (512 bytes) still fit the frame the two rings share, and the
/// buffers are one contiguous run (16 pages of 2 KiB buffers).
const TX_COUNT: usize = 32;
/// One DMA buffer per descriptor. 2 KiB comfortably holds a maximum frame plus
/// its 4-byte FCS (1518), so every frame arrives in a single descriptor.
const BUFFER_BYTES: usize = 2048;
/// Both rings share one frame. Each ring base must be 256-byte aligned: the
/// receive ring (256 bytes) sits at the start, the transmit ring right after.
const TX_RING_OFFSET: u32 = (RX_COUNT * DESCRIPTOR_BYTES) as u32;
const RX_BUFFER_FRAMES: usize = RX_COUNT * BUFFER_BYTES / 4096;
const TX_BUFFER_FRAMES: usize = TX_COUNT * BUFFER_BYTES / 4096;

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

fn read32(base: u16, register: u16) -> u32 {
    unsafe { inl(base + register) }
}

fn write32(base: u16, register: u16, value: u32) {
    unsafe { outl(base + register, value) }
}

/// Pure delay with no register access; `interrupts::ticks()` runs at 100 Hz.
fn wait_ticks(count: u64) {
    let start = interrupts::ticks();
    while interrupts::ticks().wrapping_sub(start) < count {
        core::hint::spin_loop();
    }
}

/// Pointer to one 32-bit field of descriptor `index` in the ring at `ring`.
/// Fields: 0 = opts1, 1 = opts2, 2 = address low, 3 = address high.
fn field(ring: u32, index: usize, field: usize) -> *mut u32 {
    (ring as usize + index * DESCRIPTOR_BYTES + field * 4) as *mut u32
}

fn read_opts1(ring: u32, index: usize) -> u32 {
    unsafe { read_volatile(field(ring, index, 0)) }
}

/// Fills in a descriptor. Address and `opts2` go first and `opts1` -- which
/// carries OWN -- last, behind a fence, so the card can never observe a
/// descriptor it owns but whose other fields are still half-written.
fn write_descriptor(ring: u32, index: usize, opts1: u32, address: u32) {
    unsafe {
        write_volatile(field(ring, index, 1), 0);
        write_volatile(field(ring, index, 2), address);
        write_volatile(field(ring, index, 3), 0);
    }
    fence(Ordering::SeqCst);
    unsafe { write_volatile(field(ring, index, 0), opts1) };
}

fn rx_opts1(index: usize) -> u32 {
    DESC_OWN | if index == RX_COUNT - 1 { DESC_EOR } else { 0 } | BUFFER_BYTES as u32
}

#[derive(Clone, Copy)]
pub struct ProbeInfo {
    io_base: u16,
    irq: Option<u8>,
    device_id: u16,
    revision: u8,
    address: Option<pci::PciAddress>,
}

pub struct Rtl8168 {
    io_base: u16,
    /// Physical address of the descriptor frame: receive ring at the start,
    /// transmit ring at `TX_RING_OFFSET`.
    rings: u32,
    rx_buffers: u32,
    tx_buffers: u32,
    rx_next: Cell<usize>,
    tx_next: Cell<usize>,
    /// Whether this chip wants the MACs enabled after `TxConfig`/`RxConfig`.
    enable_after_config: bool,
    overflow_count: Cell<u32>,
    recovery_count: Cell<u32>,
    rx_progress: Cell<u32>,
    tx_progress: Cell<u32>,
    recovery_failures: Cell<u32>,
    recovery_pending: AtomicBool,
    promiscuous: Cell<bool>,
    irq: Option<u8>,
    /// Where this card sits on the PCI bus, for `nic.rs` to program its MSI
    /// capability, and the MSI vector in use once that succeeds (`None` = legacy).
    pci: Cell<Option<pci::PciAddress>>,
    msi_vector: Cell<Option<u8>>,
    interrupt_count: AtomicU32,
    interrupt_errors: AtomicU32,
    rx_overruns: AtomicU32,
    soft_masked: AtomicBool,
    work_pending: AtomicBool,
    overflow_pending: AtomicBool,
    interrupts_enabled: Cell<bool>,
}

#[derive(Debug)]
pub enum InitError {
    /// The DMA rings/buffers did not fit in tracked physical memory.
    OutOfMemory,
    /// The card did not clear its reset bit within a bounded wait.
    ResetTimedOut,
    /// Every register read came back all-ones: nothing answers at this I/O
    /// base (a bad BAR, or the function is powered down).
    NotResponding,
}

impl Rtl8168 {
    /// Finds every RTL8168-family function with a usable I/O BAR, brings it to
    /// full power and enables I/O space and bus mastering.
    pub fn probe_all(out: &mut [ProbeInfo]) -> usize {
        let mut addresses = [None; pci::MAX_DEVICE_MATCHES];
        let found = pci::find_devices(
            VENDOR_REALTEK,
            &[DEVICE_RTL8168, DEVICE_RTL8161],
            &mut addresses,
        );
        let mut count = 0;
        for address in addresses[..found.min(addresses.len())].iter().flatten() {
            // A function BIOS left in D3hot ignores port accesses; force D0
            // and wait out the PCI power-management settle delay first.
            if address.force_power_state_d0() {
                wait_ticks(2);
            }
            address.enable_io_and_bus_master();
            if let Some(io_base) = address.io_bar0() {
                if count == out.len() {
                    break;
                }
                out[count] = ProbeInfo {
                    io_base,
                    irq: address.interrupt_line(),
                    device_id: address.device_id(),
                    revision: address.revision(),
                    address: Some(*address),
                };
                count += 1;
            }
        }
        count
    }

    /// Identifies the chip, resets it, allocates its rings and buffers once
    /// (never freed, like every other NIC backend), and enables receive and
    /// transmit.
    pub fn init(info: ProbeInfo) -> Result<Self, InitError> {
        let io_base = info.io_base;
        let tx_config = read32(io_base, REG_TXCONFIG);
        if tx_config == 0xffff_ffff {
            return Err(InitError::NotResponding);
        }
        // The chip version bits, masked as Linux's `r8169` does (`xid`).
        let xid = ((tx_config >> 20) & 0xfcf) as u16;
        let enable_after_config = xid >= XID_G_AND_LATER;
        println!(
            "RTL8168: PCI 10ec:{:04x} rev {:02x}, I/O 0x{:04x}, IRQ {:?}.",
            info.device_id, info.revision, io_base, info.irq
        );
        println!(
            "RTL8168: TxConfig 0x{tx_config:08x} -> chip version 0x{xid:03x} ({}).",
            if enable_after_config {
                "RTL8168G or later"
            } else {
                "older than 8168G; init order unverified"
            }
        );

        Self::soft_reset(io_base)?;

        let rings = pmem::allocate_contiguous(1).ok_or(InitError::OutOfMemory)?;
        let rx_buffers = pmem::allocate_contiguous(RX_BUFFER_FRAMES).ok_or(InitError::OutOfMemory)?;
        let tx_buffers = pmem::allocate_contiguous(TX_BUFFER_FRAMES).ok_or(InitError::OutOfMemory)?;
        Self::init_rings(rings, rx_buffers, tx_buffers);
        Self::program_registers(io_base, rings, enable_after_config, false);

        let nic = Rtl8168 {
            io_base,
            rings,
            rx_buffers,
            tx_buffers,
            rx_next: Cell::new(0),
            tx_next: Cell::new(0),
            enable_after_config,
            overflow_count: Cell::new(0),
            recovery_count: Cell::new(0),
            rx_progress: Cell::new(0),
            tx_progress: Cell::new(0),
            recovery_failures: Cell::new(0),
            recovery_pending: AtomicBool::new(false),
            promiscuous: Cell::new(false),
            irq: info.irq,
            pci: Cell::new(info.address),
            msi_vector: Cell::new(None),
            interrupt_count: AtomicU32::new(0),
            interrupt_errors: AtomicU32::new(0),
            rx_overruns: AtomicU32::new(0),
            soft_masked: AtomicBool::new(false),
            work_pending: AtomicBool::new(false),
            overflow_pending: AtomicBool::new(false),
            interrupts_enabled: Cell::new(false),
        };
        let mac = nic.mac_address();
        println!(
            "RTL8168: MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}, PHY status 0x{:02x}.",
            mac[0],
            mac[1],
            mac[2],
            mac[3],
            mac[4],
            mac[5],
            read8(io_base, REG_PHYSTATUS)
        );
        Ok(nic)
    }

    /// Software reset: set `CR_RST` and wait for the card to clear it, then
    /// give it one tick to finish internal start-up before any other access.
    fn soft_reset(io_base: u16) -> Result<(), InitError> {
        write8(io_base, REG_CR, CR_RST);
        let mut attempts = 1_000_000u32;
        while read8(io_base, REG_CR) & CR_RST != 0 {
            attempts -= 1;
            if attempts == 0 {
                return Err(InitError::ResetTimedOut);
            }
        }
        wait_ticks(1);
        Ok(())
    }

    /// Hands every receive buffer to the card and clears every transmit
    /// descriptor. Shared by `init` and `recover` (which reuses the same
    /// memory after a full reset).
    fn init_rings(rings: u32, rx_buffers: u32, tx_buffers: u32) {
        for index in 0..RX_COUNT {
            let address = rx_buffers + (index * BUFFER_BYTES) as u32;
            write_descriptor(rings, index, rx_opts1(index), address);
        }
        for index in 0..TX_COUNT {
            let address = tx_buffers + (index * BUFFER_BYTES) as u32;
            let eor = if index == TX_COUNT - 1 { DESC_EOR } else { 0 };
            write_descriptor(rings + TX_RING_OFFSET, index, eor, address);
        }
    }

    /// Programs a freshly reset card. Shared by `init` (first bring-up) and
    /// `recover`.
    fn program_registers(io_base: u16, rings: u32, enable_after_config: bool, promiscuous: bool) {
        // Many of these registers are write-protected until the config unlock.
        write8(io_base, REG_9346CR, CFG9346_UNLOCK);

        // Frames must reach the tape untouched: no checksum offload, no VLAN
        // tag stripping.
        let cplus = read16(io_base, REG_CPLUSCMD);
        write16(io_base, REG_CPLUSCMD, cplus & !(CPLUS_RX_CHECKSUM | CPLUS_RX_VLAN));
        // A frame (with FCS) always fits one 2 KiB buffer, so none ever spans
        // two descriptors: cap the accepted size just below the buffer size.
        write16(io_base, REG_RMS, (BUFFER_BYTES - 1) as u16);
        write8(io_base, REG_MAXTXPKT, 0x3f);

        // Ring bases: high dword first, then low (the low write latches).
        write32(io_base, REG_TNPDS_HIGH, 0);
        write32(io_base, REG_TNPDS_LOW, rings + TX_RING_OFFSET);
        write32(io_base, REG_RDSAR_HIGH, 0);
        write32(io_base, REG_RDSAR_LOW, rings);

        if !enable_after_config {
            write8(io_base, REG_CR, CR_TE | CR_RE);
        }
        write32(io_base, REG_TXCONFIG, TXCONFIG_VALUE);
        let mut rx_config = RXCONFIG_VALUE;
        if promiscuous {
            rx_config |= RX_ACCEPT_ALL_PHYS;
        }
        write32(io_base, REG_RXCONFIG, rx_config);
        // Accept every multicast hash; no filtering in this teaching driver.
        write32(io_base, REG_MAR0, 0xffff_ffff);
        write32(io_base, REG_MAR0 + 4, 0xffff_ffff);
        if enable_after_config {
            write8(io_base, REG_CR, CR_TE | CR_RE);
        }

        write8(io_base, REG_9346CR, CFG9346_LOCK);
        write16(io_base, REG_ISR, 0xffff); // Clear anything latched before reset.
    }

    /// This card's burned-in MAC address (`IDR0..IDR5`, loaded by the card
    /// itself from its EEPROM or eFuse at power-on).
    pub fn mac_address(&self) -> [u8; 6] {
        let mut mac = [0u8; 6];
        for (index, byte) in mac.iter_mut().enumerate() {
            *byte = read8(self.io_base, REG_IDR0 + index as u16);
        }
        mac
    }

    /// `(speed_mbps, full_duplex)` if the PHY reports link up, from
    /// `PHYstatus`; `None` while the cable is unplugged or still negotiating.
    pub fn link_status(&self) -> Option<(u16, bool)> {
        let status = read8(self.io_base, REG_PHYSTATUS);
        if status & PHYSTATUS_LINK == 0 {
            return None;
        }
        let speed = if status & PHYSTATUS_1000 != 0 {
            1000
        } else if status & PHYSTATUS_100 != 0 {
            100
        } else if status & PHYSTATUS_10 != 0 {
            10
        } else {
            return None;
        };
        Some((speed, status & PHYSTATUS_FULL_DUPLEX != 0))
    }

    /// Toggles `AcceptAllPhys`, leaving the other accept bits as `init` set them.
    pub fn set_promiscuous(&self, enabled: bool) {
        let mut rx_config = read32(self.io_base, REG_RXCONFIG);
        if enabled {
            rx_config |= RX_ACCEPT_ALL_PHYS;
        } else {
            rx_config &= !RX_ACCEPT_ALL_PHYS;
        }
        write32(self.io_base, REG_RXCONFIG, rx_config);
        self.promiscuous.set(enabled);
    }

    /// Transmits one raw Ethernet frame (the card appends the FCS), padding
    /// short frames to `MIN_FRAME_LEN`. Waits (bounded) for the card to hand
    /// the descriptor back; `false` if the frame is too long or the card
    /// never finished, which also flags a full reset.
    pub fn send(&self, frame: &[u8]) -> bool {
        if frame.len() > MAX_FRAME_LEN {
            return false;
        }
        let index = self.tx_next.get();
        let ring = self.rings + TX_RING_OFFSET;
        // The descriptor and its buffer are free again once the card has let go
        // of the frame it carried the last time round the ring.
        let mut attempts = 1_000_000u32;
        while read_opts1(ring, index) & DESC_OWN != 0 {
            attempts -= 1;
            if attempts == 0 {
                self.recovery_pending.store(true, Ordering::Release);
                return false;
            }
            core::hint::spin_loop();
        }
        self.tx_next.set((index + 1) % TX_COUNT);
        let buffer = self.tx_buffers + (index * BUFFER_BYTES) as u32;
        let destination =
            unsafe { core::slice::from_raw_parts_mut(buffer as *mut u8, BUFFER_BYTES) };
        destination[..frame.len()].copy_from_slice(frame);
        let length = frame.len().max(MIN_FRAME_LEN);
        destination[frame.len()..length].fill(0);

        let eor = if index == TX_COUNT - 1 { DESC_EOR } else { 0 };
        write_descriptor(
            ring,
            index,
            DESC_OWN | eor | DESC_FS | DESC_LS | length as u32,
            buffer,
        );
        write8(self.io_base, REG_TPPOLL, TPPOLL_NPQ);
        self.tx_progress.set(self.tx_progress.get().wrapping_add(1));
        true
    }

    /// Returns one received frame (without its FCS) if any is waiting.
    /// Frames the card flagged as errored are dropped and their descriptor
    /// handed straight back, so one bad frame never wedges the ring.
    pub fn receive(&self, out: &mut [u8]) -> Option<usize> {
        self.work_pending.store(false, Ordering::Release);
        self.recover();

        let index = self.rx_next.get();
        let opts1 = read_opts1(self.rings, index);
        if opts1 & DESC_OWN != 0 {
            // Drained. If an overrun silenced its interrupt, turn it back on.
            if self.soft_masked.swap(false, Ordering::AcqRel) {
                write16(self.io_base, REG_ISR, SOFT_MASK);
                write16(self.io_base, REG_IMR, INTERRUPT_MASK);
            }
            return None; // Still the card's: nothing waiting.
        }
        fence(Ordering::SeqCst);

        let length = (opts1 & DESC_LENGTH_MASK) as usize; // Includes the 4-byte FCS.
        let whole_frame = opts1 & (DESC_FS | DESC_LS) == (DESC_FS | DESC_LS);
        let good = whole_frame
            && opts1 & DESC_RX_RES == 0
            && length >= 14 + 4
            && length <= BUFFER_BYTES;

        let copied = if good {
            let buffer = self.rx_buffers + (index * BUFFER_BYTES) as u32;
            let payload = length - 4;
            let count = payload.min(out.len());
            let source = unsafe { core::slice::from_raw_parts(buffer as *const u8, count) };
            out[..count].copy_from_slice(source);
            Some(count)
        } else {
            None
        };

        // Give the buffer back to the card and move on, good frame or not.
        let address = self.rx_buffers + (index * BUFFER_BYTES) as u32;
        write_descriptor(self.rings, index, rx_opts1(index), address);
        self.rx_next.set((index + 1) % RX_COUNT);
        if copied.is_some() {
            self.rx_progress.set(self.rx_progress.get().wrapping_add(1));
        }
        copied
    }

    /// Full reset-and-reprogram recovery (the strategy that proved reliable
    /// for the RTL8139): after a latched overflow, a PCI system error, or a
    /// transmit that never completed. Reuses the same DMA memory.
    fn recover(&self) -> bool {
        let isr = read16(self.io_base, REG_ISR);
        let overflow = self.overflow_pending.swap(false, Ordering::AcqRel) || isr & FATAL_MASK != 0;
        if !overflow && !self.recovery_pending.swap(false, Ordering::AcqRel) {
            return false;
        }
        if Self::soft_reset(self.io_base).is_err() {
            self.recovery_failures.set(self.recovery_failures.get().wrapping_add(1));
            return false;
        }
        Self::init_rings(self.rings, self.rx_buffers, self.tx_buffers);
        self.rx_next.set(0);
        self.tx_next.set(0);
        Self::program_registers(
            self.io_base,
            self.rings,
            self.enable_after_config,
            self.promiscuous.get(),
        );
        if self.interrupts_enabled.get() {
            self.enable_interrupts();
        }
        if overflow {
            self.overflow_count.set(self.overflow_count.get().wrapping_add(1));
        }
        self.recovery_count.set(self.recovery_count.get().wrapping_add(1));
        true
    }

    /// `nic regs`: the registers and ring state worth looking at when this card
    /// misbehaves, so a symptom can be read from the shell instead of by pushing
    /// a diagnostic kernel.
    pub fn dump_registers(&self) {
        let io = self.io_base;
        println!(
            "RTL8168 I/O 0x{io:04x}: CR={:02x} IMR={:04x} ISR={:04x} PHY={:02x} CPlus={:04x}",
            read8(io, REG_CR),
            read16(io, REG_IMR),
            read16(io, REG_ISR),
            read8(io, REG_PHYSTATUS),
            read16(io, REG_CPLUSCMD)
        );
        println!(
            "TxConfig={:08x} RxConfig={:08x} RMS={:04x}",
            read32(io, REG_TXCONFIG),
            read32(io, REG_RXCONFIG),
            read16(io, REG_RMS)
        );
        let mut rx_card = 0u32;
        for index in 0..RX_COUNT {
            if read_opts1(self.rings, index) & DESC_OWN != 0 {
                rx_card |= 1 << index;
            }
        }
        let mut tx_card = 0u32;
        for index in 0..TX_COUNT {
            if read_opts1(self.rings + TX_RING_OFFSET, index) & DESC_OWN != 0 {
                tx_card |= 1 << index;
            }
        }
        println!(
            "rx next {} (card owns mask {rx_card:04x}), tx next {} (card owns {tx_card:x})",
            self.rx_next.get(),
            self.tx_next.get()
        );
        println!(
            "rx[next] opts1 {:08x}; overflows {}, recoveries {}, failures {}",
            read_opts1(self.rings, self.rx_next.get()),
            self.overflow_count.get(),
            self.recovery_count.get(),
            self.recovery_failures.get()
        );
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
            recovery_pending: self.overflow_pending.load(Ordering::Acquire)
                || self.recovery_pending.load(Ordering::Acquire),
            rx_overruns: self.rx_overruns.load(Ordering::Relaxed),
        }
    }

    pub fn watchdog(&self) -> WatchdogResult {
        if !self.health().recovery_pending && read16(self.io_base, REG_ISR) & FATAL_MASK == 0 {
            return WatchdogResult::Healthy;
        }
        if self.recover() {
            WatchdogResult::Recovered
        } else {
            WatchdogResult::Failed
        }
    }

    pub fn pci_address(&self) -> Option<pci::PciAddress> {
        self.pci.get()
    }

    pub fn msi_vector(&self) -> Option<u8> {
        self.msi_vector.get()
    }

    pub fn set_msi_vector(&self, vector: Option<u8>) {
        self.msi_vector.set(vector);
    }

    /// Forces one interrupt so `nic.rs` can check that an MSI message really
    /// arrives: `TxPoll` bit 0 raises `ISR_SWINT` (enabled in `INTERRUPT_MASK`),
    /// which `handle_interrupt` counts and acknowledges like any other cause.
    ///
    /// The RTL8168G and later (seen on a TP-Link RTL8168H) never set `ISR_SWINT`
    /// from `FSWInt`, so a second, genuine cause is raised too: `ISR_TOK` is
    /// enabled just for this test and one minimal broadcast frame is sent.
    /// `handle_interrupt` switches `ISR_TOK` off again at the first interrupt.
    pub fn trigger_test_interrupt(&self) {
        write16(self.io_base, REG_IMR, INTERRUPT_MASK | ISR_TOK);
        write8(self.io_base, REG_TPPOLL, TPPOLL_FSWINT);
        let mut frame = [0u8; 14];
        frame[..6].fill(0xff);
        frame[6..12].copy_from_slice(&self.mac_address());
        frame[12..].copy_from_slice(&[0x88, 0xb5]); // IEEE local-experimental ethertype
        self.send(&frame);
    }

    pub fn enable_interrupts(&self) {
        write16(self.io_base, REG_ISR, 0xffff);
        write16(self.io_base, REG_IMR, INTERRUPT_MASK);
        self.interrupts_enabled.set(true);
    }

    /// Interrupt handler body: acknowledge, count, and flag foreground work.
    /// Never touches the rings -- polling stays the authority.
    pub fn handle_interrupt(&self) {
        let status = read16(self.io_base, REG_ISR) & (INTERRUPT_MASK | ISR_TOK);
        if status == 0 {
            return; // A shared line: not ours.
        }
        write16(self.io_base, REG_ISR, status);
        if status & ISR_TOK != 0 {
            write16(self.io_base, REG_IMR, INTERRUPT_MASK); // Self-test only.
        }
        self.interrupt_count.fetch_add(1, Ordering::Relaxed);
        if status & (ISR_RER | SOFT_MASK | FATAL_MASK) != 0 {
            self.interrupt_errors.fetch_add(1, Ordering::Relaxed);
        }
        if status & SOFT_MASK != 0 {
            self.rx_overruns.fetch_add(1, Ordering::Relaxed);
            // Quiet these two until `receive` has drained the ring.
            self.soft_masked.store(true, Ordering::Release);
            write16(self.io_base, REG_IMR, INTERRUPT_MASK & !SOFT_MASK);
        }
        if status & FATAL_MASK != 0 {
            self.overflow_pending.store(true, Ordering::Release);
            // Stop all interrupts from this card until `recover` has reset it
            // and turned them back on: a latched fatal cause (a full receive
            // ring that nothing is draining, a PCI error) must not be able to
            // re-raise this level-triggered line as fast as the CPU can
            // acknowledge it.
            write16(self.io_base, REG_IMR, 0);
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
}

// At most the onboard NIC plus one add-in card are expected; a small fixed
// table keeps `.bss` (which is tight below `0x9f000`) small.
const MAX_INSTANCES: usize = 4;
static mut NICS: [Option<Rtl8168>; MAX_INSTANCES] = [const { None }; MAX_INSTANCES];
static mut NIC_COUNT: usize = 0;
static mut INITIALIZED: bool = false;

#[derive(Debug)]
pub enum Status {
    NotPresent,
    InitFailed,
}

/// Probes and initializes every RTL8168-family function, retaining each
/// instance's DMA memory for the rest of the boot.
pub fn initialize_all() -> Result<usize, Status> {
    unsafe {
        if !INITIALIZED {
            INITIALIZED = true;
            let mut probe_info = [const {
                ProbeInfo {
                    io_base: 0,
                    irq: None,
                    device_id: 0,
                    revision: 0,
                    address: None,
                }
            }; MAX_INSTANCES];
            let found = Rtl8168::probe_all(&mut probe_info);
            let mut failed = false;
            for info in &probe_info[..found] {
                match Rtl8168::init(*info) {
                    Ok(initialized) => {
                        NICS[NIC_COUNT] = Some(initialized);
                        NIC_COUNT += 1;
                    }
                    Err(error) => {
                        println!(
                            "RTL8168: 10ec:{:04x} at I/O 0x{:04x} failed to start: {error:?}.",
                            info.device_id, info.io_base
                        );
                        failed = true;
                    }
                }
            }
            if NIC_COUNT == 0 {
                return if failed {
                    Err(Status::InitFailed)
                } else {
                    Err(Status::NotPresent)
                };
            }
        }
        Ok(NIC_COUNT)
    }
}

pub fn get(index: usize) -> Option<&'static Rtl8168> {
    unsafe {
        if index >= NIC_COUNT {
            return None;
        }
        NICS[index].as_ref()
    }
}
