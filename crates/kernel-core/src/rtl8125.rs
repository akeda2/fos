//! A polling driver for the Realtek RTL8125 2.5 Gb/s NIC (PCI `10ec:8125`, and `10ec:3000`, the same chip sold as a
//! Killer E3000), written from Linux's r8169 (`rtl_hw_init_8125`, `rtl_hw_start_8125*`, `rtl8125*_hw_phy_config`) and
//! checked against OpenBSD's rge. Variants handled: the 8125A (TxConfig XID 0x609), 8125B (0x641) and 8125D/K
//! (0x688-0x68a); the 8125BP and CP have firmware-managed (DASH) start steps and are refused for now.
//!
//! **Why not `rtl8168.rs`.** The descriptor rings are the same 16-byte layout (OWN/EOR/FS/LS, the `TD1_*` checksum bits),
//! and this file reuses those helpers and the checksum code. The control registers are not the same: the interrupt mask
//! and status are 32-bit at 0x38/0x3c, where the 8168 has its transmit doorbell (0x38) and a 16-bit mask (0x3c); the
//! doorbell moved to 0x90; `PHYstatus` is 16-bit with a 2500 Mb/s bit; and the chip is driven through memory-mapped
//! registers, with most of its setup behind two indirect windows (below). A shared driver would branch on every access in
//! the paths verified on rack's 8168H.
//!
//! **Indirect access.** `MAC_OCP` (0xb0) reaches the MAC's internal registers: write `flag | (address << 15) | data`, or
//! `address << 15` and read the data back from the same register; no busy bit. `PHY_OCP` (0xb8) reaches the internal PHY
//! the same way but has a busy flag (bit 31) to wait for. The PHY's MII registers sit in that space: page 0 at 0xa400 +
//! 2 x register, page P's registers 0x10-0x17 at P x 16 + 2 x (register - 0x10) (Linux `r8168g_mdio_*`). `EPHYAR` (0x80)
//! reaches the PCI Express PHY, `CSIAR` (0x68) the PCI configuration space beyond what legacy config cycles reach.
//!
//! **Start-up**, as Linux does it at probe and open: the receive gate shut (`MISC.RXDV_GATED_EN`) and the FIFOs drained,
//! the out-of-band firmware told to let go (`MCU.NOW_IS_OOB` off), the "link list" FIFO ready, a reset, then the PHY
//! tuning per variant, and the start sequence: ASPM and clock request off, interrupt coalescing off, the per-variant PCIe
//! PHY table, about forty MAC-OCP writes Linux carries without names (copied with their values, not understood), the
//! receive gate opened, rings, enables. Every wait is bounded; one that runs out is counted and named in `nic` (`describe`)
//! instead of stopping the start, as Linux only logs it.
//!
//! **Left out on purpose**: the PHY firmware patch (`rtl_nic/rtl8125*.fw`) that Linux loads when it can and OpenBSD
//! builds in. Linux runs without it when the file is missing; whether the PHY links at 2500 without it is the first thing
//! to see on real hardware. ASPM stays off (Linux only enables it when the OS owns it), as do EEE changes, LTR, RSS and
//! multiple queues.
//!
//! Interrupts, recovery and jumbo frames follow `rtl8168.rs` (MSI or INTx, kept only when a self-test interrupt arrives,
//! which here too comes from a transmitted frame; soft handling of RDU/FOVW; 9 KiB buffers above an MTU of 1500). The chip
//! takes 16 KiB frames; 9000 is kept as the limit, as on every other card here.
//!
//! Only real hardware can check any of this: QEMU has no RTL8125. Seen working (2026-11-25): an RTL8125B (XID 0x641)
//! linked at 2.5 Gb/s to another RTL8125, frames both ways, MSI after the deferred self-test (`irqprobe`). Not yet
//! measured: throughput, jumbo frames, recovery.

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering, fence};

use crate::e1000::{Delivery, ISR, MAX_INTERFACES};
use crate::irqprobe::{Probe, SelfTest, Step};
use crate::rtl8168::{DESC_EOR, DESC_FS, DESC_LENGTH_MASK, DESC_LS, DESC_OWN, DESC_RX_RES, Rtl8168, read_opts1, write_descriptor};
use crate::{memory, paging, pci, timer};

pub const VENDOR_REALTEK: u16 = 0x10ec;
pub const DEVICE_IDS: &[u16] = &[0x8125, 0x3000];

pub const MAX_FRAME: usize = 1514;
const MIN_FRAME: usize = 60;

const REG_MAC0: usize = 0x00;
const REG_MAC4: usize = 0x04;
const REG_MAR0: usize = 0x08;
const REG_TNPDS: usize = 0x20;
const REG_INT_CFG0: usize = 0x34;
const REG_CR: usize = 0x37;
const REG_IMR: usize = 0x38; // 32-bit on the 8125
const REG_ISR: usize = 0x3c; // 32-bit on the 8125
const REG_TXCONFIG: usize = 0x40;
const REG_RXCONFIG: usize = 0x44;
const REG_9346CR: usize = 0x50;
const REG_CONFIG1: usize = 0x52;
const REG_CONFIG2: usize = 0x53;
const REG_CONFIG3: usize = 0x54;
const REG_CONFIG5: usize = 0x56;
const REG_CSIDR: usize = 0x64;
const REG_CSIAR: usize = 0x68;
const REG_PHYSTATUS: usize = 0x6c; // 16-bit on the 8125
const REG_INT_CFG1: usize = 0x7a;
const REG_EPHYAR: usize = 0x80;
const REG_TXPOLL: usize = 0x90; // 16-bit
const REG_MAC_OCP: usize = 0xb0;
const REG_PHY_OCP: usize = 0xb8;
const REG_MCU: usize = 0xd3;
const REG_RMS: usize = 0xda;
const REG_CPLUSCMD: usize = 0xe0;
const REG_INTR_MITIGATE: usize = 0xe2;
const REG_RDSAR: usize = 0xe4;
const REG_MISC: usize = 0xf0;
const REG_MAC0_BKP: usize = 0x19e0; // where the 8125 keeps the factory address (Linux `MAC0_BKP`)
const REG_RSS_CTRL: usize = 0x4500;
const REG_Q_NUM_CTRL: usize = 0x4800;
const REG_EEE_TXIDLE: usize = 0x6048;
/// The bytes of the register window mapped: the highest register used is 0x6048.
const WINDOW_BYTES: u64 = 0x10000;

const CR_STOP_REQ: u8 = 0x80;
const CR_RST: u8 = 0x10;
const CR_RE: u8 = 0x08;
const CR_TE: u8 = 0x04;
const TXPOLL_NPQ: u16 = 0x0001;
const CFG9346_UNLOCK: u8 = 0xc0;
const CFG9346_LOCK: u8 = 0x00;
const CONFIG2_CLKREQ_EN: u8 = 0x80;
const CONFIG3_RDY_TO_L23: u8 = 0x02;
const CONFIG5_ASPM_EN: u8 = 0x01;
const MCU_NOW_IS_OOB: u8 = 0x80;
const MCU_RXTX_EMPTY: u8 = 0x30;
const MCU_LINK_LIST_READY: u8 = 0x02;
const MISC_RXDV_GATED: u32 = 1 << 19;
const OCP_FLAG: u32 = 1 << 31;

const ISR_ROK: u32 = 0x0001;
const ISR_RER: u32 = 0x0002;
const ISR_TOK: u32 = 0x0004;
const ISR_RDU: u32 = 0x0010;
const ISR_LINKCHG: u32 = 0x0020;
const ISR_FOVW: u32 = 0x0040;
const ISR_SYSERR: u32 = 0x8000;
const INTERRUPT_MASK: u32 = ISR_ROK | ISR_RER | ISR_RDU | ISR_LINKCHG | ISR_FOVW | ISR_SYSERR;
const SOFT_MASK: u32 = ISR_RDU | ISR_FOVW;

/// TxConfig: interframe gap 3, unlimited DMA burst, automatic FIFO (Linux sets `TXCFG_AUTO_FIFO` from the 8168E-VL on).
const TXCONFIG_VALUE: u32 = (3 << 24) | (7 << 8) | (1 << 7);
const RX_ACCEPT_ALL_PHYS: u32 = 0x01;
const RX_ACCEPT_MY_PHYS: u32 = 0x02;
const RX_ACCEPT_MULTICAST: u32 = 0x04;
const RX_ACCEPT_BROADCAST: u32 = 0x08;
/// RxConfig (Linux `rtl_init_rxcfg`): the 8125's default descriptor fetch count (8 << 27) and unlimited DMA burst; from
/// the 8125B on, `RX_PAUSE_SLOT_ON` (bit 11, the bit that is `RX_EARLY_OFF` on the 8168).
const RX_FETCH_DEFAULT: u32 = 8 << 27;
const RX_DMA_BURST: u32 = 7 << 8;
const RX_PAUSE_SLOT_ON: u32 = 1 << 11;
const CPLUS_RX_CHECKSUM: u16 = 1 << 5;
const CPLUS_RX_VLAN: u16 = 1 << 6;
/// The CPlusCmd bits Linux keeps from what the chip had (`CPCMD_MASK`).
const CPLUS_KEEP: u16 = (1 << 13) | CPLUS_RX_VLAN | CPLUS_RX_CHECKSUM | 0x3;
const PHYSTATUS_FULL: u16 = 0x0001;
const PHYSTATUS_LINK: u16 = 0x0002;
const PHYSTATUS_10: u16 = 0x0004;
const PHYSTATUS_100: u16 = 0x0008;
const PHYSTATUS_1000: u16 = 0x0010;
const PHYSTATUS_2500: u16 = 0x0400;

/// The PHY's page-0 MII registers in the PHY-OCP space (Linux `OCP_STD_PHY_BASE`).
const PHY_STD_BASE: u32 = 0xa400;
const MII_BMCR: u32 = 0;
const MII_CTRL1000: u32 = 9;
const BMCR_RESET: u16 = 0x8000;
const BMCR_ANENABLE: u16 = 0x1000;
const BMCR_PDOWN: u16 = 0x0800;
const BMCR_ANRESTART: u16 = 0x0200;
const CTRL1000_FULL: u16 = 0x0200;
/// The multi-gigabit advertisement (clause 45 7.32, `MDIO_AN_10GBT_CTRL`), which the Realtek PHY keeps at this OCP address
/// (Linux's realtek PHY driver, `RTL_MDIO_AN_10GBT_CTRL`); bit 7 advertises 2.5GBASE-T.
const PHY_AN_10GBT_CTRL: u32 = 0xa5d4;
const ADVERTISE_2500: u16 = 0x0080;
/// The multi-gigabit status (clause 45 7.33, `MDIO_AN_10GBT_STAT`); bit 5: the link partner offers 2.5GBASE-T.
const PHY_AN_10GBT_STAT: u32 = 0xa5d6;
const PARTNER_2500: u16 = 0x0020;

const DESCRIPTOR_BYTES: usize = 16;
/// Receive descriptors. At 2.5 Gb/s 256 buffers of full frames are 1.2 ms of traffic, less than the time the frames
/// can wait for a drain while a tape works (an SMB upload from Linux overflowed them some 150 times per 700 MB); 1024
/// are 5 ms (Realtek's own r8125 driver uses 1024 too). 2 MiB of buffers, 9 MiB with jumbo frames.
const RX_COUNT: usize = 1024;
const TX_COUNT: usize = 256;
const DEFER_LIMIT: usize = 8;
const BUFFER_BYTES: usize = 2048;
const JUMBO_BUFFER_BYTES: usize = 9216;
pub const JUMBO_MAX_MTU: usize = 9000;
const FRAME_OVERHEAD: usize = 18;
const TX_RING_OFFSET: u64 = (RX_COUNT * DESCRIPTOR_BYTES) as u64;
const RING_FRAMES: u64 = ((RX_COUNT + TX_COUNT) * DESCRIPTOR_BYTES).div_ceil(4096) as u64;
const RETRY_TICKS: u64 = 300;
const RETRY_LIMIT: u8 = 3;
const STALL_TICKS: u64 = 5;

static REGISTERS: [AtomicUsize; MAX_INTERFACES] = [const { AtomicUsize::new(0) }; MAX_INTERFACES];
static SOFT_MASKED: [AtomicBool; MAX_INTERFACES] = [const { AtomicBool::new(false) }; MAX_INTERFACES];
static FATAL: [AtomicBool; MAX_INTERFACES] = [const { AtomicBool::new(false) }; MAX_INTERFACES];

#[derive(Debug)]
pub enum Error {
    NoSlot,
    NoMemoryBar,
    NotResponding,
    /// The TxConfig XID: a chip this driver does not know (or the 8125BP/CP, left out for now).
    UnknownChip(u16),
    ResetTimedOut,
    OutOfMemory,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Chip {
    A,
    B,
    D,
}

impl Chip {
    /// Linux's table (`rtl_chip_infos`, mask 0x7cf).
    fn from_xid(xid: u16) -> Option<Chip> {
        match xid {
            0x609 => Some(Chip::A),
            0x641 => Some(Chip::B),
            0x688..=0x68a => Some(Chip::D),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Chip::A => "RTL8125A",
            Chip::B => "RTL8125B",
            Chip::D => "RTL8125D",
        }
    }
}

fn read8(base: usize, register: usize) -> u8 {
    unsafe { read_volatile(memory::phys_to_virt((base + register) as u64) as *const u8) }
}
fn write8(base: usize, register: usize, value: u8) {
    unsafe { write_volatile(memory::phys_to_virt((base + register) as u64) as *mut u8, value) }
}
fn read16(base: usize, register: usize) -> u16 {
    unsafe { read_volatile(memory::phys_to_virt((base + register) as u64) as *const u16) }
}
fn write16(base: usize, register: usize, value: u16) {
    unsafe { write_volatile(memory::phys_to_virt((base + register) as u64) as *mut u16, value) }
}
fn read32(base: usize, register: usize) -> u32 {
    unsafe { read_volatile(memory::phys_to_virt((base + register) as u64) as *const u32) }
}
fn write32(base: usize, register: usize, value: u32) {
    unsafe { write_volatile(memory::phys_to_virt((base + register) as u64) as *mut u32, value) }
}

/// Busy-waits `micros` microseconds (callers may run with interrupts off, so no timer ticks).
fn wait_us(micros: u64) {
    let start = timer::micros();
    for _ in 0..10_000_000u32 {
        if timer::micros().wrapping_sub(start) >= micros {
            return;
        }
        core::hint::spin_loop();
    }
}

fn rx_opts1(index: usize, buffer_bytes: usize) -> u32 {
    DESC_OWN | if index == RX_COUNT - 1 { DESC_EOR } else { 0 } | buffer_bytes as u32
}

/// The MSI handler: acknowledge (write 1 to clear, 32-bit here), count, mask what must not storm.
fn isr(index: usize) {
    let base = REGISTERS[index].load(Ordering::Relaxed);
    if base == 0 {
        return;
    }
    let status = read32(base, REG_ISR) & (INTERRUPT_MASK | ISR_TOK);
    if status == 0 {
        return;
    }
    write32(base, REG_ISR, status);
    crate::msi::claim();
    ISR[index].interrupts.fetch_add(1, Ordering::Relaxed);
    crate::hal::net_event();
    if status & ISR_TOK != 0 {
        write32(base, REG_IMR, INTERRUPT_MASK);
    }
    if status & ISR_LINKCHG != 0 {
        ISR[index].link_changes.fetch_add(1, Ordering::Relaxed);
    }
    if status & SOFT_MASK != 0 {
        ISR[index].overruns.fetch_add(1, Ordering::Relaxed);
        SOFT_MASKED[index].store(true, Ordering::Release);
        write32(base, REG_IMR, INTERRUPT_MASK & !SOFT_MASK);
    }
    if status & ISR_SYSERR != 0 {
        FATAL[index].store(true, Ordering::Release);
        write32(base, REG_IMR, 0);
    }
}

pub struct Rtl8125 {
    index: usize,
    pub delivery: Delivery,
    interrupts_enabled: bool,
    registers: usize,
    /// The PCI function number, which `CSIAR` needs.
    function: u8,
    pub chip: Chip,
    /// TxConfig bits 30:20 masked with 0x7cf, as Linux identifies the chip.
    pub xid: u16,
    rings: u64,
    rx_buffers: u64,
    tx_buffers: u64,
    buffer_bytes: usize,
    mtu: usize,
    pub jumbo_dropped: u64,
    rx_next: usize,
    tx_next: usize,
    defer_doorbell: bool,
    deferred: usize,
    tx_offload: bool,
    promiscuous: bool,
    pub mac: [u8; 6],
    pub frames_received: u64,
    pub frames_sent: u64,
    pub frames_dropped_no_link: u64,
    pub recoveries: u32,
    pub read_request: u32,
    pub tx_ring_full: u64,
    recovery_pending: bool,
    stall_since: u64,
    link_checks: u32,
    link_ok: bool,
    idle_polls: u8,
    retry: Option<pci::Device>,
    retry_after: u64,
    retry_attempts: u8,
    /// The self-test `retry_interrupts` is part-way through.
    probe: Option<Probe>,
    /// Bounded waits that ran out (each is a step Linux would log), and the last one's name.
    /// Why the last `recover` ran (`nic` shows it).
    last_recovery: &'static str,
    pub timeouts: u32,
    pub last_timeout: &'static str,
    /// The PHY indirect access (`PHY_OCP`) failed to complete at least once.
    pub phy_errors: u32,
}

/// The factory address before `init` (`nicorder`: placing `nic N MAC` pins): `MAC0_BKP`, else MAC0, as `read_mac` reads them
/// after `hw_init`. Both hold the EEPROM's address from power-on; this only reads, after the decoding set-up `init` does first.
pub fn peek_mac(device: &pci::Device) -> Option<[u8; 6]> {
    device.force_power_state_d0();
    device.enable_memory_and_bus_master();
    let (base, size) = (0..6).find_map(|bar| device.memory_bar(bar))?;
    paging::map_device(base, size.max(WINDOW_BYTES));
    let registers = base as usize;
    if read32(registers, REG_TXCONFIG) == 0xffff_ffff {
        return None; // not responding: `init` will say so
    }
    let read = |at: usize| core::array::from_fn::<u8, 6, _>(|offset| read8(registers, at + offset));
    let mac = read(REG_MAC0_BKP);
    Some(if mac == [0; 6] || mac[0] & 1 != 0 || mac == [0xff; 6] { read(REG_MAC0) } else { mac })
}

impl Rtl8125 {
    pub fn init(device: &pci::Device, index: usize) -> Result<Rtl8125, Error> {
        if index >= MAX_INTERFACES {
            return Err(Error::NoSlot);
        }
        device.force_power_state_d0();
        device.enable_memory_and_bus_master();
        let read_request = crate::rtl8168::raise_read_request(device);
        // Linux takes the first memory BAR (BAR 2 on these cards; BAR 0 is I/O, BAR 4 the MSI-X table).
        let (base, size) = (0..6).find_map(|bar| device.memory_bar(bar)).ok_or(Error::NoMemoryBar)?;
        paging::map_device(base, size.max(WINDOW_BYTES));
        let registers = base as usize;
        let tx_config = read32(registers, REG_TXCONFIG);
        if tx_config == 0xffff_ffff {
            return Err(Error::NotResponding);
        }
        let xid = ((tx_config >> 20) & 0x7cf) as u16;
        let chip = Chip::from_xid(xid).ok_or(Error::UnknownChip(xid))?;
        let mut nic = Rtl8125 {
            index,
            delivery: Delivery::Polled,
            interrupts_enabled: false,
            registers,
            function: device.function,
            chip,
            xid,
            rings: 0,
            rx_buffers: 0,
            tx_buffers: 0,
            buffer_bytes: BUFFER_BYTES,
            mtu: 1500,
            jumbo_dropped: 0,
            rx_next: 0,
            tx_next: 0,
            defer_doorbell: false,
            deferred: 0,
            tx_offload: false,
            promiscuous: false,
            mac: [0; 6],
            frames_received: 0,
            frames_sent: 0,
            frames_dropped_no_link: 0,
            recoveries: 0,
            read_request,
            tx_ring_full: 0,
            recovery_pending: false,
            stall_since: 0,
            link_checks: 0,
            link_ok: true,
            idle_polls: 0,
            retry: None,
            retry_after: 0,
            retry_attempts: 0,
            probe: None,
            last_recovery: "",
            timeouts: 0,
            last_timeout: "",
            phy_errors: 0,
        };
        // Linux's probe: receive config, interrupts masked and acknowledged, the out-of-band hand-over, a reset.
        write32(registers, REG_RXCONFIG, nic.rx_config_base());
        write32(registers, REG_IMR, 0);
        write32(registers, REG_ISR, 0xffff_ffff);
        nic.hw_init();
        if !nic.reset() {
            return Err(Error::ResetTimedOut);
        }
        nic.mac = nic.read_mac();
        nic.set_mac();
        let below = Some(1u64 << 32); // 64-bit descriptors, but low memory is simplest (as on the 8168)
        nic.rings = memory::allocate_contiguous(RING_FRAMES, below).ok_or(Error::OutOfMemory)?;
        let (rx_frames, tx_frames) = ((RX_COUNT * BUFFER_BYTES / 4096) as u64, (TX_COUNT * BUFFER_BYTES / 4096) as u64);
        nic.rx_buffers = memory::allocate_contiguous(rx_frames, below).ok_or(Error::OutOfMemory)?;
        nic.tx_buffers = memory::allocate_contiguous(tx_frames, below).ok_or(Error::OutOfMemory)?;
        // Linux's open: the PHY first, then the reset work (`rtl8169_cleanup`, `rtl_hw_start`).
        nic.phy_init();
        if !nic.restart() {
            return Err(Error::ResetTimedOut);
        }
        REGISTERS[index].store(registers, Ordering::Relaxed);
        if nic.timeouts != 0 {
            crate::println!("rtl8125 {}: {} start-up step(s) timed out,", index + 1, nic.timeouts);
            crate::println!("     the last: {}.", nic.last_timeout);
        }
        Ok(nic)
    }

    /// Polls `done` every `step` microseconds, at most `count` times; a wait that runs out is counted under `what`.
    fn wait(&mut self, what: &'static str, step: u64, count: u32, done: impl Fn(usize) -> bool) -> bool {
        for _ in 0..count {
            if done(self.registers) {
                return true;
            }
            wait_us(step);
        }
        if done(self.registers) {
            return true;
        }
        self.timeouts += 1;
        self.last_timeout = what;
        false
    }

    // ---- indirect access ----

    fn mac_ocp_read(&self, address: u32) -> u16 {
        write32(self.registers, REG_MAC_OCP, address << 15);
        read32(self.registers, REG_MAC_OCP) as u16
    }

    fn mac_ocp_write(&self, address: u32, value: u16) {
        write32(self.registers, REG_MAC_OCP, OCP_FLAG | address << 15 | u32::from(value));
    }

    fn mac_ocp_modify(&self, address: u32, clear: u16, set: u16) {
        let value = self.mac_ocp_read(address);
        self.mac_ocp_write(address, (value & !clear) | set);
    }

    /// The flag is set when a read completes (Linux: 10 polls 25 us apart).
    fn phy_ocp_try_read(&self, address: u32) -> Option<u16> {
        write32(self.registers, REG_PHY_OCP, address << 15);
        for _ in 0..=10 {
            let value = read32(self.registers, REG_PHY_OCP);
            if value & OCP_FLAG != 0 {
                return Some(value as u16);
            }
            wait_us(25);
        }
        None
    }

    fn phy_ocp_read(&mut self, address: u32) -> u16 {
        self.phy_ocp_try_read(address).unwrap_or_else(|| {
            self.phy_errors += 1;
            self.timeouts += 1;
            self.last_timeout = "a PHY read";
            0
        })
    }

    /// The flag clears when a write completes.
    fn phy_ocp_write(&mut self, address: u32, value: u16) -> bool {
        write32(self.registers, REG_PHY_OCP, OCP_FLAG | address << 15 | u32::from(value));
        let done = self.wait("a PHY write", 25, 10, |r| read32(r, REG_PHY_OCP) & OCP_FLAG == 0);
        if !done {
            self.phy_errors += 1;
        }
        done
    }

    fn phy_ocp_modify(&mut self, address: u32, clear: u16, set: u16) {
        let value = self.phy_ocp_read(address);
        self.phy_ocp_write(address, (value & !clear) | set);
    }

    /// MII register `register` of page `page` (0: the standard registers 0-15; otherwise registers 0x10-0x17).
    fn phy_address(page: u32, register: u32) -> u32 {
        if page == 0 { PHY_STD_BASE + register * 2 } else { (page << 4) + (register - 0x10) * 2 }
    }

    fn phy_modify_paged(&mut self, page: u32, register: u32, clear: u16, set: u16) {
        self.phy_ocp_modify(Self::phy_address(page, register), clear, set);
    }

    fn phy_write_paged(&mut self, page: u32, register: u32, value: u16) {
        self.phy_ocp_write(Self::phy_address(page, register), value);
    }

    /// Linux `r8168g_phy_param`: a parameter number into page 0xa43 register 0x13, the value through register 0x14.
    fn phy_param_a43(&mut self, parameter: u16, clear: u16, set: u16) {
        self.phy_write_paged(0xa43, 0x13, parameter);
        self.phy_modify_paged(0xa43, 0x14, clear, set);
    }

    /// Linux `rtl8125_phy_param`: the same through the vendor MMD window at 0xb87c/0xb87e.
    fn phy_param_b87c(&mut self, parameter: u16, clear: u16, set: u16) {
        self.phy_ocp_write(0xb87c, parameter);
        self.phy_ocp_modify(0xb87e, clear, set);
    }

    /// The PCI Express PHY. Linux masks the register number to 5 bits (`EPHYAR_REG_MASK`), so its 8125A/B tables' second
    /// half (0x44, 0x4a, ...) lands on the first half's registers again; OpenBSD and Realtek's driver use 7 bits and reach
    /// a second set. Linux's way is copied, as the one with the most machines behind it.
    fn ephy_read(&mut self, register: u32) -> u16 {
        write32(self.registers, REG_EPHYAR, (register & 0x1f) << 16);
        if self.wait("a PCIe PHY read", 10, 100, |r| read32(r, REG_EPHYAR) & OCP_FLAG != 0) {
            read32(self.registers, REG_EPHYAR) as u16
        } else {
            0xffff
        }
    }

    fn ephy_write(&mut self, register: u32, value: u16) {
        write32(self.registers, REG_EPHYAR, OCP_FLAG | (register & 0x1f) << 16 | u32::from(value));
        self.wait("a PCIe PHY write", 10, 100, |r| read32(r, REG_EPHYAR) & OCP_FLAG == 0);
        wait_us(10);
    }

    fn ephy_table(&mut self, table: &[(u32, u16, u16)]) {
        for &(register, clear, set) in table {
            let value = (self.ephy_read(register) & !clear) | set;
            self.ephy_write(register, value);
        }
    }

    /// PCI configuration space through the chip (`CSIAR`): Linux's fallback for the byte at 0x70f, beyond the 256 bytes a
    /// legacy configuration cycle reaches.
    fn csi_read(&mut self, address: u32) -> u32 {
        let command = (address & 0xfff) | u32::from(self.function) << 16 | 0xf000;
        write32(self.registers, REG_CSIAR, command);
        if self.wait("a CSI read", 10, 100, |r| read32(r, REG_CSIAR) & OCP_FLAG != 0) { read32(self.registers, REG_CSIDR) } else { !0 }
    }

    fn csi_write(&mut self, address: u32, value: u32) {
        write32(self.registers, REG_CSIDR, value);
        let command = OCP_FLAG | (address & 0xfff) | u32::from(self.function) << 16 | 0xf000;
        write32(self.registers, REG_CSIAR, command);
        self.wait("a CSI write", 10, 100, |r| read32(r, REG_CSIAR) & OCP_FLAG == 0);
    }

    // ---- bring-up ----

    fn rx_config_base(&self) -> u32 {
        RX_FETCH_DEFAULT | RX_DMA_BURST | if self.chip == Chip::A { 0 } else { RX_PAUSE_SLOT_ON }
    }

    /// Linux `rtl_wait_txrx_fifo_empty`: the 8125B and later are asked to stop first and report empty in two places.
    fn wait_fifos_empty(&mut self) {
        if self.chip != Chip::A {
            write8(self.registers, REG_CR, read8(self.registers, REG_CR) | CR_STOP_REQ);
        }
        self.wait("the FIFOs to empty", 100, 42, |r| read8(r, REG_MCU) & MCU_RXTX_EMPTY == MCU_RXTX_EMPTY);
        if self.chip != Chip::A {
            self.wait("the FIFOs to empty (2)", 100, 42, |r| read16(r, REG_INTR_MITIGATE) & 0x0103 == 0x0103);
        }
    }

    /// Shuts the receive gate: no frame enters the FIFO (Linux `rtl_enable_rxdvgate`).
    fn gate_receive(&mut self) {
        write32(self.registers, REG_MISC, read32(self.registers, REG_MISC) | MISC_RXDV_GATED);
        timer::busy_wait_ms(2);
        self.wait_fifos_empty();
    }

    fn wait_link_list(&mut self) {
        self.wait("the link-list FIFO", 100, 42, |r| read8(r, REG_MCU) & MCU_LINK_LIST_READY != 0);
    }

    /// Linux `rtl_hw_init_8125`: take the chip from the firmware (out-of-band mode off) and set up its shared FIFO.
    fn hw_init(&mut self) {
        self.gate_receive();
        write8(self.registers, REG_CR, read8(self.registers, REG_CR) & !(CR_TE | CR_RE));
        timer::busy_wait_ms(1);
        write8(self.registers, REG_MCU, read8(self.registers, REG_MCU) & !MCU_NOW_IS_OOB);
        self.mac_ocp_modify(0xe8de, 1 << 14, 0);
        self.wait_link_list();
        self.mac_ocp_write(0xc0aa, 0x07d0);
        self.mac_ocp_write(0xc0a6, 0x0150);
        self.mac_ocp_write(0xc01e, 0x5555);
        self.wait_link_list();
    }

    /// `CR.RST`, waited for (Linux: 100 polls 100 us apart).
    fn reset(&mut self) -> bool {
        write8(self.registers, REG_CR, CR_RST);
        self.wait("the reset", 100, 100, |r| read8(r, REG_CR) & CR_RST == 0)
    }

    /// The factory address: the 8125 keeps it at 0x19e0 (Linux reads it there first); `MAC0` as a fallback.
    fn read_mac(&self) -> [u8; 6] {
        let mut mac = [0u8; 6];
        for (offset, byte) in mac.iter_mut().enumerate() {
            *byte = read8(self.registers, REG_MAC0_BKP + offset);
        }
        if mac == [0; 6] || mac[0] & 1 != 0 || mac == [0xff; 6] {
            for (offset, byte) in mac.iter_mut().enumerate() {
                *byte = read8(self.registers, REG_MAC0 + offset);
            }
        }
        mac
    }

    /// Linux `rtl_rar_set`: the address the receive filter matches. High half first, each write flushed by a read.
    fn set_mac(&self) {
        let r = self.registers;
        write8(r, REG_9346CR, CFG9346_UNLOCK);
        write32(r, REG_MAC4, u32::from(u16::from_le_bytes([self.mac[4], self.mac[5]])));
        read8(r, REG_CR);
        write32(r, REG_MAC0, u32::from_le_bytes([self.mac[0], self.mac[1], self.mac[2], self.mac[3]]));
        read8(r, REG_CR);
        write8(r, REG_9346CR, CFG9346_LOCK);
    }

    /// Linux `rtl8169_init_phy` for these chips: wake the PHY, the per-variant tuning (firmware left out, see the top),
    /// advertise 2.5G, 1000 full and the rest, and restart autonegotiation with a soft reset.
    fn phy_init(&mut self) {
        self.phy_modify_paged(0, MII_BMCR, BMCR_PDOWN, 0);
        match self.chip {
            Chip::A => self.phy_config_a(),
            Chip::B => self.phy_config_b(),
            Chip::D => {
                self.enable_gphy_10m();
                self.legacy_force_mode();
            }
        }
        self.phy_modify_paged(0xa43, 0x10, 1 << 2, 0); // ALDPS (link-down power saving) off, as Linux leaves it
        self.config_eee_phy();
        self.phy_ocp_modify(PHY_AN_10GBT_CTRL, 0, ADVERTISE_2500);
        self.phy_modify_paged(0, MII_CTRL1000, 0, CTRL1000_FULL);
        self.phy_ocp_write(PHY_STD_BASE, BMCR_RESET | BMCR_ANENABLE | BMCR_ANRESTART);
        // The reset bit clears itself (genphy_soft_reset allows 600 ms).
        for _ in 0..60 {
            timer::busy_wait_ms(10);
            if self.phy_ocp_read(PHY_STD_BASE) & BMCR_RESET == 0 {
                return;
            }
        }
        self.timeouts += 1;
        self.last_timeout = "the PHY reset";
    }

    fn enable_gphy_10m(&mut self) {
        self.phy_modify_paged(0xa44, 0x11, 0, 1 << 11);
    }

    fn legacy_force_mode(&mut self) {
        self.phy_modify_paged(0xa5b, 0x12, 1 << 15, 0);
    }

    /// Linux `rtl8125_config_eee_phy`, the PHY side of its EEE setup.
    fn config_eee_phy(&mut self) {
        self.phy_modify_paged(0xa43, 0x11, 0, 1 << 4);
        self.phy_modify_paged(0xa6d, 0x14, 0x0010, 0);
        self.phy_modify_paged(0xa42, 0x14, 0x0080, 0);
        self.phy_modify_paged(0xa4a, 0x11, 0x0200, 0);
    }

    /// Linux `rtl8125a_2_hw_phy_config`, the firmware step left out.
    fn phy_config_a(&mut self) {
        for &(page, register, clear, set) in &[
            (0xad4, 0x17, 0x0000, 0x0010),
            (0xad1, 0x13, 0x03ff, 0x03ff),
            (0xad3, 0x11, 0x003f, 0x0006),
            (0xac0, 0x14, 0x1100, 0x0000),
            (0xacc, 0x10, 0x0003, 0x0002),
            (0xad4, 0x10, 0x00e7, 0x0044),
            (0xac1, 0x12, 0x0080, 0x0000),
            (0xac8, 0x10, 0x0300, 0x0000),
            (0xac5, 0x17, 0x0007, 0x0002),
        ] {
            self.phy_modify_paged(page, register, clear, set);
        }
        self.phy_write_paged(0xad4, 0x16, 0x00a8);
        self.phy_write_paged(0xac5, 0x16, 0x01ff);
        self.phy_modify_paged(0xac8, 0x15, 0x00f0, 0x0030);
        self.phy_param_b87c(0x80a2, 0xffff, 0x0153);
        self.phy_param_b87c(0x809c, 0xffff, 0x0153);
        // A table written through page 0xa43's parameter window, starting at parameter 0x81b3.
        self.phy_write_paged(0xa43, 0x13, 0x81b3);
        for value in [0x0043, 0x00a7, 0x00d6, 0x00ec, 0x00f6, 0x00fb, 0x00fd, 0x00ff, 0x00bb, 0x0058, 0x0029, 0x0013, 0x0009, 0x0004, 0x0002] {
            self.phy_write_paged(0xa43, 0x14, value);
        }
        for _ in 0..25 {
            self.phy_write_paged(0xa43, 0x14, 0);
        }
        self.phy_param_a43(0x8257, 0xffff, 0x020f);
        self.phy_param_a43(0x80ea, 0xffff, 0x7843);
        // (Linux applies the PHY firmware here.)
        self.phy_modify_paged(0xd06, 0x14, 0x0000, 0x2000);
        self.phy_param_a43(0x81a2, 0x0000, 0x0100);
        self.phy_modify_paged(0xb54, 0x16, 0xff00, 0xdb00);
        self.phy_modify_paged(0xa45, 0x12, 0x0001, 0x0000);
        self.phy_modify_paged(0xa5d, 0x12, 0x0000, 0x0020);
        self.phy_modify_paged(0xad4, 0x17, 0x0010, 0x0000);
        self.phy_modify_paged(0xa86, 0x15, 0x0001, 0x0000);
        self.enable_gphy_10m();
    }

    /// Linux `rtl8125b_hw_phy_config`, the firmware step (its first) left out.
    fn phy_config_b(&mut self) {
        self.enable_gphy_10m();
        self.phy_modify_paged(0xac4, 0x13, 0x00f0, 0x0090);
        self.phy_modify_paged(0xad3, 0x10, 0x0003, 0x0001);
        self.phy_param_b87c(0x80f5, 0xffff, 0x760e);
        self.phy_param_b87c(0x8107, 0xffff, 0x360e);
        self.phy_param_b87c(0x8551, 0xff00, 0x0800);
        self.phy_modify_paged(0xbf0, 0x10, 0xe000, 0xa000);
        self.phy_modify_paged(0xbf4, 0x13, 0x0f00, 0x0300);
        for parameter in (0x8044..=0x807a).step_by(6) {
            self.phy_param_a43(parameter, 0xffff, 0x2417);
        }
        self.phy_modify_paged(0xa4c, 0x15, 0x0000, 0x0040);
        self.phy_modify_paged(0xbf8, 0x12, 0xe000, 0xa000);
        self.legacy_force_mode();
    }

    /// Linux `rtl8169_cleanup` then `rtl_hw_start`: interrupts off, receive closed and gated, reset, fresh rings, start.
    fn restart(&mut self) -> bool {
        let r = self.registers;
        write32(r, REG_IMR, 0);
        write32(r, REG_ISR, 0xffff_ffff);
        write32(r, REG_RXCONFIG, read32(r, REG_RXCONFIG) & !0x3f);
        self.gate_receive();
        if !self.reset() {
            return false;
        }
        init_rings(self.rings, self.rx_buffers, self.tx_buffers, self.buffer_bytes);
        self.rx_next = 0;
        self.tx_next = 0;
        self.deferred = 0;
        self.start();
        true
    }

    /// Linux `rtl_hw_start` for the 8125, ASPM left off.
    fn start(&mut self) {
        let r = self.registers;
        write8(r, REG_9346CR, CFG9346_UNLOCK);
        // ASPM and clock request off before the PCIe PHY is touched.
        self.mac_ocp_modify(0xe092, 0x00ff, 0);
        write8(r, REG_CONFIG2, read8(r, REG_CONFIG2) & !CONFIG2_CLKREQ_EN);
        write8(r, REG_CONFIG5, read8(r, REG_CONFIG5) & !CONFIG5_ASPM_EN);
        // No receive checksum or VLAN stripping: frames arrive as they were on the wire.
        write16(r, REG_CPLUSCMD, read16(r, REG_CPLUSCMD) & CPLUS_KEEP & !(CPLUS_RX_CHECKSUM | CPLUS_RX_VLAN));
        write16(r, REG_EEE_TXIDLE, (self.mtu + 14 + 0x20) as u16);
        // rtl_hw_start_8125: vector mapping off, interrupt coalescing off, extended tally counter on.
        write8(r, REG_INT_CFG0, 0);
        let coalescing_end = if self.chip == Chip::B { 0xa80 } else { 0xb00 };
        for register in (0xa00..coalescing_end).step_by(4) {
            write32(r, register, 0);
        }
        if self.chip == Chip::B {
            write16(r, REG_INT_CFG1, 0);
        }
        self.mac_ocp_modify(0xea84, 0, 0x0003);
        // Per variant: ASPM entry latency (L0s 7 us, L1 16 us; harmless with ASPM off), the PCIe PHY table.
        let latency = (self.csi_read(0x070c) & 0x00ff_ffff) | 0x27 << 24;
        self.csi_write(0x070c, latency);
        match self.chip {
            Chip::A => self.ephy_table(&[
                (0x04, 0xffff, 0xd000),
                (0x0a, 0xffff, 0x8653),
                (0x23, 0xffff, 0xab66),
                (0x20, 0xffff, 0x9455),
                (0x21, 0xffff, 0x99ff),
                (0x29, 0xffff, 0xfe04),
                (0x44, 0xffff, 0xd000),
                (0x4a, 0xffff, 0x8653),
                (0x63, 0xffff, 0xab66),
                (0x60, 0xffff, 0x9455),
                (0x61, 0xffff, 0x99ff),
                (0x69, 0xffff, 0xfe04),
            ]),
            Chip::B => self.ephy_table(&[
                (0x0b, 0xffff, 0xa908),
                (0x1e, 0xffff, 0x20eb),
                (0x4b, 0xffff, 0xa908),
                (0x5e, 0xffff, 0x20eb),
                (0x22, 0x0030, 0x0020),
                (0x62, 0x0030, 0x0020),
            ]),
            Chip::D => {}
        }
        self.start_common();
        self.mac_ocp_modify(0xc0ac, 0, 0x1f80); // the events that bring the link out of L1 (Linux `rtl_enable_exit_l1`)
        write16(r, REG_RMS, (self.buffer_bytes - 1) as u16);
        let tx_ring = self.rings + TX_RING_OFFSET;
        write32(r, REG_TNPDS + 4, (tx_ring >> 32) as u32);
        write32(r, REG_TNPDS, tx_ring as u32);
        write32(r, REG_RDSAR + 4, (self.rings >> 32) as u32);
        write32(r, REG_RDSAR, self.rings as u32);
        write32(r, REG_RSS_CTRL, 0);
        write8(r, REG_9346CR, CFG9346_LOCK);
        read8(r, REG_CR); // flush the writes before the enables
        write8(r, REG_CR, CR_TE | CR_RE);
        let accept = RX_ACCEPT_MY_PHYS | RX_ACCEPT_MULTICAST | RX_ACCEPT_BROADCAST | if self.promiscuous { RX_ACCEPT_ALL_PHYS } else { 0 };
        write32(r, REG_RXCONFIG, self.rx_config_base() | accept);
        write32(r, REG_TXCONFIG, TXCONFIG_VALUE);
        write32(r, REG_MAR0 + 4, 0xffff_ffff);
        write32(r, REG_MAR0, 0xffff_ffff);
        write32(r, REG_ISR, 0xffff_ffff);
        write32(r, REG_IMR, if self.interrupts_enabled { INTERRUPT_MASK } else { 0 });
    }

    /// Linux `rtl_hw_start_8125_common`. The MAC-OCP addresses and values are Linux's (from Realtek's driver); most have no
    /// name there either.
    fn start_common(&mut self) {
        let r = self.registers;
        write8(r, REG_CONFIG3, read8(r, REG_CONFIG3) & !CONFIG3_RDY_TO_L23);
        write16(r, 0x382, 0x221b);
        write32(r, REG_RSS_CTRL, 0);
        write16(r, REG_Q_NUM_CTRL, 0);
        self.mac_ocp_modify(0xd40a, 0x0010, 0); // "disable UPS"
        write8(r, REG_CONFIG1, read8(r, REG_CONFIG1) & !0x10);
        self.mac_ocp_write(0xc140, 0xffff);
        self.mac_ocp_write(0xc142, 0xffff);
        self.mac_ocp_modify(0xd3e2, 0x0fff, 0x03a9);
        self.mac_ocp_modify(0xd3e4, 0x00ff, 0x0000);
        self.mac_ocp_modify(0xe860, 0x0000, 0x0080);
        self.mac_ocp_modify(0xeb58, 0x0001, 0x0000); // the old 16-byte transmit descriptors, the ones used here
        let (e614, e63e) = if self.chip == Chip::B { (0x0200, 0x0000) } else { (0x0300, 0x0020) };
        self.mac_ocp_modify(0xe614, 0x0700, e614);
        self.mac_ocp_modify(0xe63e, 0x0c30, e63e);
        for &(address, clear, set) in &[
            (0xc0b4, 0x0000, 0x000c),
            (0xeb6a, 0x00ff, 0x0033),
            (0xeb50, 0x03e0, 0x0040),
            (0xe056, 0x00f0, 0x0000),
            (0xe040, 0x1000, 0x0000),
            (0xea1c, 0x0003, 0x0001),
            (0xea1c, 0x0004, 0x0000),
            (0xe0c0, 0x4f0f, 0x4403),
            (0xe052, 0x0080, 0x0068),
            (0xd430, 0x0fff, 0x047f),
            (0xea1c, 0x0004, 0x0000),
            (0xeb54, 0x0000, 0x0001),
        ] {
            self.mac_ocp_modify(address, clear, set);
        }
        wait_us(1);
        self.mac_ocp_modify(0xeb54, 0x0001, 0x0000);
        write16(r, 0x1880, read16(r, 0x1880) & !0x0030);
        self.mac_ocp_write(0xe098, 0xc302);
        // MAC-OCP reads are not closures over `registers` alone, so this wait is written out.
        let mut ready = false;
        for _ in 0..=10 {
            if self.mac_ocp_read(0xe00e) & (1 << 13) == 0 {
                ready = true;
                break;
            }
            wait_us(1000);
        }
        if !ready {
            self.timeouts += 1;
            self.last_timeout = "MAC-OCP 0xe00e bit 13";
        }
        // Open the receive gate.
        write32(r, REG_MISC, read32(r, REG_MISC) & !MISC_RXDV_GATED);
    }

    // ---- interrupts ----

    fn finish_interrupts(&mut self, delivery: Delivery) -> Delivery {
        self.interrupts_enabled = true;
        self.delivery = delivery;
        write32(self.registers, REG_IMR, INTERRUPT_MASK);
        delivery
    }

    /// As `Rtl8168::enable_interrupts`: MSI, then INTx pins, each kept only if its self-test interrupt (the completion of
    /// one broadcast frame; the 8125 has no forced software interrupt) arrives (`irqprobe`). Needs interrupts on. With no
    /// link it is retried from `receive`, one step at a time.
    pub fn enable_interrupts(&mut self, device: &pci::Device) -> Result<Delivery, &'static str> {
        if !self.link_up() {
            self.retry = Some(*device);
            self.retry_after = timer::ticks() + RETRY_TICKS;
            return Err("no link yet, so the self-test cannot send a frame; it is retried when the link comes up");
        }
        let delivery = Probe::new(*device).run(self)?;
        Ok(self.finish_interrupts(delivery))
    }

    /// The deferred half of `enable_interrupts`, called from `receive`: once the link is up it runs the self-test one
    /// `Probe::step` per call, since `receive` may hold a lock with interrupts off (the server kernel's NIC lock) and the
    /// test interrupt only arrives once that is let go. At most `RETRY_LIMIT` searches, `RETRY_TICKS` apart.
    fn retry_interrupts(&mut self) {
        let Some(device) = self.retry else { return };
        if self.interrupts_enabled {
            self.retry = None;
            return;
        }
        let mut probe = match self.probe.take() {
            Some(probe) => probe,
            None if timer::ticks() < self.retry_after || !self.link_up() => return,
            None => {
                self.retry_attempts += 1;
                Probe::new(device)
            }
        };
        match probe.step(self) {
            Step::Pending => self.probe = Some(probe),
            Step::Passed(delivery) => {
                self.retry = None;
                self.finish_interrupts(delivery);
                crate::println!("rtl8125 {}: link is up; interrupts: {delivery:?} (the self-test interrupt arrived).", self.index + 1);
            }
            Step::Failed(reason) => {
                crate::println!("rtl8125 {}: link is up, interrupts stay polled ({reason}).", self.index + 1);
                if self.retry_attempts < RETRY_LIMIT {
                    self.retry_after = timer::ticks() + RETRY_TICKS;
                } else {
                    self.retry = None;
                }
            }
        }
    }

    // ---- what the shell and `nic` see ----

    pub fn describe(&self) {
        match self.link_speed() {
            Some((speed, full)) => crate::println!(
                "   rtl8125: {} (XID {:#05x}), {speed} Mb/s {}, {} recoveries",
                self.chip.name(),
                self.xid,
                if full { "full duplex" } else { "half duplex" },
                self.recoveries
            ),
            None => crate::println!("   rtl8125: {} (XID {:#05x}), no link, {} recoveries", self.chip.name(), self.xid, self.recoveries),
        }
        if self.recoveries > 0 {
            crate::println!("   rtl8125: the last recovery was for {}", self.last_recovery);
        }
        crate::println!("   rtl8125: receive ring ({RX_COUNT}) overflowed {} time(s) (RDU/FOVW)", ISR[self.index].overruns.load(Ordering::Relaxed));
        crate::println!("   rtl8125: transmit ring ({TX_COUNT}) found full {} time(s)", self.tx_ring_full);
        crate::println!("   rtl8125: PCIe max read request {} bytes", self.read_request);
        let yes = |value: Option<u16>, bit: u16| match value {
            Some(value) if value & bit != 0 => "yes",
            Some(_) => "no",
            None => "unreadable",
        };
        crate::println!(
            "   rtl8125: 2.5G advertised {}, offered by the partner {}",
            yes(self.phy_ocp_try_read(PHY_AN_10GBT_CTRL), ADVERTISE_2500),
            yes(self.phy_ocp_try_read(PHY_AN_10GBT_STAT), PARTNER_2500)
        );
        if self.timeouts != 0 || self.phy_errors != 0 {
            crate::println!("   rtl8125: {} wait(s) ran out, the last: {}", self.timeouts, self.last_timeout);
            crate::println!("   rtl8125: {} PHY access error(s)", self.phy_errors);
        }
    }

    pub fn dump_registers(&self) {
        let r = self.registers;
        crate::println!(
            "RTL8125 at {r:#x}: CR={:02x} IMR={:08x} ISR={:08x} PHY={:04x} CPlus={:04x}",
            read8(r, REG_CR),
            read32(r, REG_IMR),
            read32(r, REG_ISR),
            read16(r, REG_PHYSTATUS),
            read16(r, REG_CPLUSCMD)
        );
        crate::println!(
            "TxConfig={:08x} RxConfig={:08x} RMS={:04x} MCU={:02x} MISC={:08x}",
            read32(r, REG_TXCONFIG),
            read32(r, REG_RXCONFIG),
            read16(r, REG_RMS),
            read8(r, REG_MCU),
            read32(r, REG_MISC)
        );
        let (mut rx_card, mut tx_card) = (0usize, 0usize);
        for index in 0..RX_COUNT {
            rx_card += usize::from(read_opts1(self.rings, index) & DESC_OWN != 0);
        }
        for index in 0..TX_COUNT {
            tx_card += usize::from(read_opts1(self.rings + TX_RING_OFFSET, index) & DESC_OWN != 0);
        }
        crate::println!("rx next {} (card owns {rx_card} of {RX_COUNT}), tx next {} (card owns {tx_card} of {TX_COUNT})", self.rx_next, self.tx_next);
        crate::println!("rx[next] opts1 {:08x}; recoveries {}, interrupts {:?}", read_opts1(self.rings, self.rx_next), self.recoveries, self.delivery);
    }

    pub fn link_up(&self) -> bool {
        read16(self.registers, REG_PHYSTATUS) & PHYSTATUS_LINK != 0
    }

    pub fn link_speed(&self) -> Option<(u16, bool)> {
        let status = read16(self.registers, REG_PHYSTATUS);
        if status & PHYSTATUS_LINK == 0 {
            return None;
        }
        let speed = match status {
            s if s & PHYSTATUS_2500 != 0 => 2500,
            s if s & PHYSTATUS_1000 != 0 => 1000,
            s if s & PHYSTATUS_100 != 0 => 100,
            s if s & PHYSTATUS_10 != 0 => 10,
            _ => return None,
        };
        Some((speed, status & PHYSTATUS_FULL != 0))
    }

    /// The PHY's basic mode control register (page 0, register 0), for `nicpark`.
    pub fn phy_control(&self) -> Option<u16> {
        self.phy_ocp_try_read(PHY_STD_BASE)
    }

    /// MII register `register` (0-15) of the PHY, for `nic phy`.
    pub fn phy_read(&self, register: u32) -> Option<u16> {
        (register < 16).then(|| self.phy_ocp_try_read(Self::phy_address(0, register))).flatten()
    }

    pub fn set_phy_control(&mut self, value: u16) -> bool {
        self.phy_ocp_write(PHY_STD_BASE, value)
    }

    pub fn set_promiscuous(&mut self, on: bool) {
        let config = read32(self.registers, REG_RXCONFIG);
        write32(self.registers, REG_RXCONFIG, if on { config | RX_ACCEPT_ALL_PHYS } else { config & !RX_ACCEPT_ALL_PHYS });
        self.promiscuous = on;
    }

    fn recover(&mut self, reason: &'static str) -> bool {
        self.last_recovery = reason;
        if !self.restart() {
            return false;
        }
        FATAL[self.index].store(false, Ordering::Relaxed);
        SOFT_MASKED[self.index].store(false, Ordering::Relaxed);
        self.recoveries += 1;
        self.recovery_pending = false;
        true
    }

    /// As `Rtl8168::set_mtu`: the first MTU above 1500 moves both rings to 9 KiB buffers and restarts the chip on them.
    pub fn set_mtu(&mut self, mtu: usize) -> bool {
        if !(1500..=JUMBO_MAX_MTU).contains(&mtu) {
            return false;
        }
        if mtu > 1500 && self.buffer_bytes != JUMBO_BUFFER_BYTES {
            let below = Some(1u64 << 32);
            let Some(rx_buffers) = memory::allocate_contiguous((RX_COUNT * JUMBO_BUFFER_BYTES).div_ceil(4096) as u64, below) else { return false };
            let Some(tx_buffers) = memory::allocate_contiguous((TX_COUNT * JUMBO_BUFFER_BYTES).div_ceil(4096) as u64, below) else { return false };
            // Only now: `send` takes frames up to the MTU, which the 2 KiB buffers could not hold.
            (self.rx_buffers, self.tx_buffers, self.buffer_bytes, self.mtu) = (rx_buffers, tx_buffers, JUMBO_BUFFER_BYTES, mtu);
            return self.recover("the MTU change");
        }
        self.mtu = mtu;
        // The EEE transmit idle timer follows the MTU (Linux `rtl_set_eee_txidle_timer`).
        write16(self.registers, REG_EEE_TXIDLE, (mtu + 14 + 0x20) as u16);
        true
    }

    pub fn mtu(&self) -> usize {
        self.mtu
    }

    pub fn largest_mtu(&self) -> usize {
        JUMBO_MAX_MTU
    }

    pub fn set_tx_offload(&mut self, on: bool) -> bool {
        self.tx_offload = on;
        true
    }

    /// `Rtl8168::offload_flags`, except for UDP to the PTP ports (319, 320): the 8125A/B compute a wrong checksum for a
    /// short one unless it is padded (Linux `rtl8125_quirk_udp_padto`, from Realtek's driver); those go unsummed instead.
    fn offload_flags(&self, frame: &[u8], buffer: *mut u8) -> Option<u32> {
        if frame.len() >= 42 && frame[12..14] == [0x08, 0x00] && frame[23] == 17 {
            let l4 = 14 + usize::from(frame[14] & 15) * 4;
            if l4 + 4 <= frame.len() && matches!(u16::from_be_bytes([frame[l4 + 2], frame[l4 + 3]]), 319 | 320) {
                return None;
            }
        }
        Rtl8168::offload_flags(frame, buffer)
    }

    pub fn send(&mut self, frame: &[u8]) -> bool {
        let limit = if self.mtu > 1500 { self.mtu + FRAME_OVERHEAD } else { MAX_FRAME };
        if frame.len() > limit {
            return false;
        }
        self.link_checks = self.link_checks.wrapping_add(1);
        if self.link_checks % 16 == 1 || !self.link_ok {
            self.link_ok = self.link_up();
        }
        if !self.link_ok {
            self.frames_dropped_no_link += 1;
            return true;
        }
        let ring = self.rings + TX_RING_OFFSET;
        let index = self.tx_next;
        if read_opts1(ring, index) & DESC_OWN != 0 {
            self.tx_ring_full += 1;
            if self.deferred != 0 {
                self.kick();
            }
        }
        for _ in 0..1_000_000u32 {
            if read_opts1(ring, index) & DESC_OWN == 0 {
                break;
            }
            core::hint::spin_loop();
        }
        if read_opts1(ring, index) & DESC_OWN != 0 {
            self.recovery_pending = true;
            return false;
        }
        self.tx_next = (index + 1) % TX_COUNT;
        let buffer = self.tx_buffers + (index * self.buffer_bytes) as u64;
        let destination = unsafe { core::slice::from_raw_parts_mut(memory::phys_to_virt(buffer) as *mut u8, self.buffer_bytes) };
        destination[..frame.len()].copy_from_slice(frame);
        // Linux pads every frame to 60 bytes on the 8125 (`rtl_quirk_packet_padto`); so does this driver, on every chip.
        let length = frame.len().max(MIN_FRAME);
        destination[frame.len()..length].fill(0);
        let flags = if self.tx_offload { self.offload_flags(frame, memory::phys_to_virt(buffer) as *mut u8) } else { None };
        let eor = if index == TX_COUNT - 1 { DESC_EOR } else { 0 };
        write_descriptor(ring, index, DESC_OWN | eor | DESC_FS | DESC_LS | length as u32, buffer, flags.unwrap_or(0));
        if self.defer_doorbell {
            self.deferred += 1;
        } else {
            write16(self.registers, REG_TXPOLL, TXPOLL_NPQ);
            self.deferred = 0;
        }
        self.frames_sent += 1;
        true
    }

    /// `send` without the doorbell. A memory write is posted (the CPU does not wait for it), so the gain is smaller than
    /// on the 8168's I/O port, but one doorbell per burst still saves the card a descriptor fetch per frame.
    pub fn send_deferred(&mut self, frame: &[u8]) -> bool {
        if self.deferred >= DEFER_LIMIT {
            self.kick();
        }
        self.defer_doorbell = true;
        let sent = self.send(frame);
        self.defer_doorbell = false;
        sent
    }

    pub fn kick(&mut self) {
        if self.deferred != 0 {
            write16(self.registers, REG_TXPOLL, TXPOLL_NPQ);
            self.deferred = 0;
        }
    }

    /// As `Rtl8168::receiver_stalled`: an overflow cause still latched with the ring empty for `STALL_TICKS` means reset.
    fn receiver_stalled(&mut self) -> bool {
        self.idle_polls = self.idle_polls.wrapping_add(1);
        if self.idle_polls % 32 != 0 {
            return false;
        }
        if read32(self.registers, REG_ISR) & SOFT_MASK == 0 {
            self.stall_since = 0;
            return false;
        }
        let now = timer::ticks().max(1);
        if self.stall_since == 0 {
            self.stall_since = now;
            return false;
        }
        if now.wrapping_sub(self.stall_since) < STALL_TICKS {
            return false;
        }
        self.stall_since = 0;
        true
    }

    pub fn receive(&mut self, out: &mut [u8]) -> Option<usize> {
        if self.retry.is_some() {
            self.retry_interrupts();
        }
        let fatal = FATAL[self.index].swap(false, Ordering::AcqRel)
            || (!self.interrupts_enabled && read32(self.registers, REG_ISR) & ISR_SYSERR != 0);
        if fatal {
            self.recover("a system error (SYSERR)");
        } else if self.recovery_pending {
            self.recover("a transmit descriptor the card never gave back");
        }
        let index = self.rx_next;
        let opts1 = read_opts1(self.rings, index);
        if opts1 & DESC_OWN != 0 {
            if SOFT_MASKED[self.index].swap(false, Ordering::AcqRel) {
                write32(self.registers, REG_ISR, SOFT_MASK);
                write32(self.registers, REG_IMR, INTERRUPT_MASK);
            }
            if self.interrupts_enabled && self.receiver_stalled() {
                self.recover("receive overflow latched with the ring empty");
            }
            return None;
        }
        fence(Ordering::SeqCst);
        let length = (opts1 & DESC_LENGTH_MASK) as usize; // includes the 4-byte FCS
        let whole = opts1 & (DESC_FS | DESC_LS) == (DESC_FS | DESC_LS);
        let good = whole && opts1 & DESC_RX_RES == 0 && (14 + 4..=self.buffer_bytes).contains(&length);
        let oversize = self.buffer_bytes == JUMBO_BUFFER_BYTES && good && (length - 4 > self.mtu + FRAME_OVERHEAD || length - 4 > out.len());
        if oversize {
            self.jumbo_dropped += 1;
        }
        let good = good && !oversize;
        let buffer = self.rx_buffers + (index * self.buffer_bytes) as u64;
        let copied = good.then(|| {
            let count = (length - 4).min(out.len());
            unsafe { core::ptr::copy_nonoverlapping(memory::phys_to_virt(buffer) as *const u8, out.as_mut_ptr(), count) };
            count
        });
        write_descriptor(self.rings, index, rx_opts1(index, self.buffer_bytes), buffer, 0);
        self.rx_next = (index + 1) % RX_COUNT;
        if copied.is_some() {
            self.frames_received += 1;
        }
        copied
    }
}

fn init_rings(rings: u64, rx_buffers: u64, tx_buffers: u64, buffer_bytes: usize) {
    for index in 0..RX_COUNT {
        write_descriptor(rings, index, rx_opts1(index, buffer_bytes), rx_buffers + (index * buffer_bytes) as u64, 0);
    }
    for index in 0..TX_COUNT {
        let eor = if index == TX_COUNT - 1 { DESC_EOR } else { 0 };
        write_descriptor(rings + TX_RING_OFFSET, index, eor, tx_buffers + (index * buffer_bytes) as u64, 0);
    }
}

impl SelfTest for Rtl8125 {
    fn probe_index(&self) -> usize {
        self.index
    }

    fn probe_handler(&self) -> fn(usize) {
        isr
    }

    fn probe_link_up(&self) -> bool {
        self.link_up()
    }

    fn probe_arm(&mut self) {
        write32(self.registers, REG_ISR, 0xffff_ffff);
        write32(self.registers, REG_IMR, INTERRUPT_MASK | ISR_TOK);
        let mut frame = [0u8; 14];
        frame[..6].fill(0xff);
        frame[6..12].copy_from_slice(&self.mac);
        frame[12..14].copy_from_slice(&[0x88, 0xb5]);
        self.send(&frame);
    }

    fn probe_disarm(&mut self) {
        write32(self.registers, REG_IMR, 0);
    }
}
