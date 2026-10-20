//! Broadcom GENETv5 Ethernet, as found on the Raspberry Pi 4 (BCM2711): MAC at 0xFD580000, a BCM54213PE gigabit PHY on MDIO
//! address 1, wired by RGMII.
//!
//! Register layout, bring-up order and the one-queue setup follow U-Boot's `drivers/net/bcmgenet.c` (itself taken from Linux's
//! `bcmgenet`): all 256 descriptors belong to the default ring (16) for each direction, and the descriptors are not in RAM but
//! in MMIO registers (address low/high plus a length/status word), so only the frame buffers need DMA-safe memory. Those come
//! from the non-cacheable DMA pool (`memory::dma_pool`), which also takes care of the cache questions: nothing is cleaned or
//! invalidated here. Everything is polled, no interrupts; one frame is sent at a time (the call returns when the MAC has
//! consumed it, bounded).
//!
//! The PHY: the firmware (edk2) already ran it for network boot, so it is not reset. Its RGMII receive skew (the
//! `rgmii-rxid` the Pi's device tree names) is written again, which is harmless if it already is so, and auto-negotiation is
//! restarted only when there is no link. The MAC address is the one the firmware left in the UMAC registers (it gets it from the
//! board's OTP). Link speed is read from the PHY's standard registers (what the partner advertised, and ours) and set in UMAC_CMD.
//!
//! Promiscuous mode is the only receive filter: the MAC without its multicast filter list takes broadcast and our address, so
//! multicast needs promiscuous (`set_promiscuous`, which `nic_set_multicast` also uses). **Unverified on hardware** when written
//! (no Genet in QEMU): the order of the bring-up, the PHY handling and the speed bits are the places to look first.

use crate::{memory, timer};

const BASE: u64 = 0xfd58_0000;
const SIZE: u64 = 0x1_0000;
const PHY_ADDRESS: u32 = 1;

const SYS_REV_CTRL: u64 = 0x00;
const SYS_PORT_CTRL: u64 = 0x04;
const PORT_MODE_EXT_GPHY: u32 = 3;
const SYS_RBUF_FLUSH_CTRL: u64 = 0x08;
const EXT_RGMII_OOB_CTRL: u64 = 0x80 + 0x0c;
const RGMII_LINK: u32 = 1 << 4;
const OOB_DISABLE: u32 = 1 << 5;
const RGMII_MODE_EN: u32 = 1 << 6;
const ID_MODE_DIS: u32 = 1 << 16;
const RBUF_CTRL: u64 = 0x300;
const RBUF_ALIGN_2B: u32 = 1 << 1;
const RBUF_TBUF_SIZE_CTRL: u64 = 0x300 + 0xb4;

// INTRL2_0, the interrupt block for the default queue (Linux bcmgenet.h): status, set, clear, mask status/set/clear.
const INTRL2_0: u64 = 0x200;
const INTRL2_CLEAR: u64 = INTRL2_0 + 0x08;
const INTRL2_MASK_SET: u64 = INTRL2_0 + 0x10;
const INTRL2_MASK_CLEAR: u64 = INTRL2_0 + 0x14;
/// RXDMA multi-buffer, packet and buffer done (bits 13..=15): which of them the hardware raises per frame is not
/// documented, so all three are unmasked and any one wakes the foreground.
const IRQ_RXDMA: u32 = 0b111 << 13;
/// GIC INTID of Genet interrupt 0 (SPI 157).
const IRQ_INTID: u32 = 32 + 157;

static IRQS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static IRQ_BITS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

const UMAC: u64 = 0x800;
const UMAC_CMD: u64 = UMAC + 0x008;
const UMAC_MAC0: u64 = UMAC + 0x00c;
const UMAC_MAC1: u64 = UMAC + 0x010;
const UMAC_MAX_FRAME_LEN: u64 = UMAC + 0x014;
const UMAC_TX_FLUSH: u64 = UMAC + 0x334;
const UMAC_MIB_CTRL: u64 = UMAC + 0x580;
const MDIO_CMD: u64 = UMAC + 0x614;
const MDIO_START_BUSY: u32 = 1 << 29;
const MDIO_READ_FAIL: u32 = 1 << 28;
const MDIO_READ: u32 = 2 << 26;
const MDIO_WRITE: u32 = 1 << 26;

const CMD_TX_EN: u32 = 1 << 0;
const CMD_RX_EN: u32 = 1 << 1;
const CMD_SPEED_SHIFT: u32 = 2;
const CMD_PROMISC: u32 = 1 << 4;
const CMD_SW_RESET: u32 = 1 << 13;
const CMD_LCL_LOOP_EN: u32 = 1 << 15;

const DESCRIPTORS: usize = 256;
const DESC_SIZE: u64 = 12;
const DEFAULT_RING: u64 = 0x10;
const DMA_RING_SIZE: u64 = 0x40;
const DMA_RINGS_SIZE: u64 = DMA_RING_SIZE * (DEFAULT_RING + 1);

const RX_DESCRIPTORS: u64 = 0x2000;
const TX_DESCRIPTORS: u64 = 0x4000;
const RDMA: u64 = RX_DESCRIPTORS + DESCRIPTORS as u64 * DESC_SIZE;
const TDMA: u64 = TX_DESCRIPTORS + DESCRIPTORS as u64 * DESC_SIZE;
const RDMA_RING: u64 = RDMA + DEFAULT_RING * DMA_RING_SIZE;
const TDMA_RING: u64 = TDMA + DEFAULT_RING * DMA_RING_SIZE;
const RDMA_REGS: u64 = RDMA + DMA_RINGS_SIZE;
const TDMA_REGS: u64 = TDMA + DMA_RINGS_SIZE;

// Per-ring registers (offsets from the ring base) and per-direction ones (from the `*_REGS` base).
const RDMA_WRITE_PTR: u64 = RDMA_RING;
const RDMA_PROD_INDEX: u64 = RDMA_RING + 0x08;
const RDMA_CONS_INDEX: u64 = RDMA_RING + 0x0c;
const RDMA_XON_XOFF_THRESH: u64 = RDMA_RING + 0x28;
const RDMA_READ_PTR: u64 = RDMA_RING + 0x2c;
const TDMA_READ_PTR: u64 = TDMA_RING;
const TDMA_CONS_INDEX: u64 = TDMA_RING + 0x08;
const TDMA_PROD_INDEX: u64 = TDMA_RING + 0x0c;
const TDMA_FLOW_PERIOD: u64 = TDMA_RING + 0x28;
const TDMA_WRITE_PTR: u64 = TDMA_RING + 0x2c;
const DMA_RING_BUF_SIZE: u64 = 0x10;
const DMA_START_ADDR: u64 = 0x14;
const DMA_END_ADDR: u64 = 0x1c;
const DMA_MBUF_DONE_THRESH: u64 = 0x24;
const DMA_RING_CFG: u64 = 0x00;
const DMA_CTRL: u64 = 0x04;
const DMA_SCB_BURST_SIZE: u64 = 0x0c;
const DMA_EN: u32 = 1;
const DMA_MAX_BURST_LENGTH: u32 = 8;
const DMA_FC_THRESH: u32 = (5 << 16) | (DESCRIPTORS as u32 >> 4);

const DMA_OWN: u32 = 0x8000;
const DMA_EOP: u32 = 0x4000;
const DMA_SOP: u32 = 0x2000;
/// Receive error flags in a descriptor's status: RXER, overrun, no-octet, length, CRC.
const DMA_RX_ERRORS: u32 = 0x1f;
const DMA_TX_APPEND_CRC: u32 = 0x40;
const TX_QUEUE_TAG: u32 = 0x3f << 7;

const BUFFER_LENGTH: usize = 2048;
/// The MAC puts two bytes of padding before a frame so the IP header is aligned (`RBUF_ALIGN_2B`).
const RX_PAD: usize = 2;
const MAX_FRAME: u32 = 1536;

const BMCR: u32 = 0;
const BMSR: u32 = 1;
const PHY_ID1: u32 = 2;
const PHY_ID2: u32 = 3;
const ADVERTISE: u32 = 4;
const LPA: u32 = 5;
const CTRL1000: u32 = 9;
const STAT1000: u32 = 10;
const BCM_AUX_CONTROL: u32 = 0x18;
const BCM_SHADOW: u32 = 0x1c;

struct Genet {
    mac: [u8; 6],
    rx_buffers: u64,
    tx_buffer: u64,
    /// Next descriptor to read, and the consumer index we last gave the MAC (16 bits, wrapping).
    rx_index: usize,
    consumed: u32,
    tx_index: usize,
    phy: u32,
    /// 0 = no link, else Mbit/s.
    speed: u32,
    full_duplex: bool,
    promiscuous: bool,
    sent: u32,
    received: u32,
    dropped: u32,
    last_drop: u32,
}

static mut GENET: Option<Genet> = None;

fn state() -> Option<&'static mut Genet> {
    unsafe { (*core::ptr::addr_of_mut!(GENET)).as_mut() }
}

fn read(offset: u64) -> u32 {
    unsafe { ((BASE + offset) as *const u32).read_volatile() }
}

fn write(offset: u64, value: u32) {
    unsafe { ((BASE + offset) as *mut u32).write_volatile(value) }
}

fn update(offset: u64, clear: u32, set: u32) {
    write(offset, (read(offset) & !clear) | set);
}

fn barrier() {
    unsafe { core::arch::asm!("dsb sy", options(nostack, preserves_flags)) };
}

fn mdio_wait() -> bool {
    let start = timer::micros();
    while read(MDIO_CMD) & MDIO_START_BUSY != 0 {
        if timer::micros() - start > 20_000 {
            return false;
        }
    }
    true
}

fn mdio_read(phy: u32, register: u32) -> Option<u16> {
    write(MDIO_CMD, MDIO_READ | phy << 21 | register << 16);
    update(MDIO_CMD, 0, MDIO_START_BUSY);
    if !mdio_wait() {
        return None;
    }
    let value = read(MDIO_CMD);
    (value & MDIO_READ_FAIL == 0).then_some(value as u16)
}

fn mdio_write(phy: u32, register: u32, value: u16) -> bool {
    write(MDIO_CMD, MDIO_WRITE | phy << 21 | register << 16 | u32::from(value));
    update(MDIO_CMD, 0, MDIO_START_BUSY);
    mdio_wait()
}

/// A Broadcom shadow register (select, then read back through the same register).
fn bcm_shadow_read(phy: u32, shadow: u32) -> Option<u16> {
    mdio_write(phy, BCM_SHADOW, (shadow << 10) as u16);
    mdio_read(phy, BCM_SHADOW).map(|value| value & 0x3ff)
}

fn bcm_shadow_write(phy: u32, shadow: u32, value: u16) -> bool {
    mdio_write(phy, BCM_SHADOW, 0x8000 | (shadow << 10) as u16 | (value & 0x3ff))
}

/// `rgmii-rxid` as Linux's and U-Boot's BCM54xx drivers set it: skew on the receive clock inside the PHY, none on transmit.
fn configure_phy_delays(phy: u32) {
    // Auxiliary control shadow 7: write the select in both the shadow and the read-selector fields, read, then write back.
    mdio_write(phy, BCM_AUX_CONTROL, 0x7007);
    if let Some(value) = mdio_read(phy, BCM_AUX_CONTROL) {
        mdio_write(phy, BCM_AUX_CONTROL, 0x8007 | (value & 0x7ff8) | 0x0100); // write enable, RGMII skew on
    }
    // Clock control shadow 3: bit 9 is the PHY's transmit clock delay, off.
    if let Some(value) = bcm_shadow_read(phy, 3) {
        bcm_shadow_write(phy, 3, value & !(1 << 9));
    }
}

fn find_phy() -> Option<u32> {
    for address in [PHY_ADDRESS].into_iter().chain((0..32).filter(|&address| address != PHY_ADDRESS)) {
        if let Some(id) = mdio_read(address, PHY_ID1) {
            if id != 0xffff && id != 0 {
                return Some(address);
            }
        }
    }
    None
}

/// Reads the PHY's negotiated speed and duplex; true when the link is up. Updates the MAC to match.
fn update_link(genet: &mut Genet) -> bool {
    let phy = genet.phy;
    let _ = mdio_read(phy, BMSR); // link status is latched low: the first read reports a past drop
    let up = mdio_read(phy, BMSR).is_some_and(|status| status & 4 != 0);
    if !up {
        genet.speed = 0;
        return false;
    }
    let (advertised_gigabit, partner_gigabit) = (mdio_read(phy, CTRL1000).unwrap_or(0), mdio_read(phy, STAT1000).unwrap_or(0));
    let common = u32::from(partner_gigabit) & (u32::from(advertised_gigabit) << 2);
    let (speed, full) = if common & 0x0800 != 0 {
        (1000, true)
    } else if common & 0x0400 != 0 {
        (1000, false)
    } else {
        let both = mdio_read(phy, ADVERTISE).unwrap_or(0) & mdio_read(phy, LPA).unwrap_or(0);
        if both & 0x0100 != 0 {
            (100, true)
        } else if both & 0x0080 != 0 {
            (100, false)
        } else if both & 0x0040 != 0 {
            (10, true)
        } else {
            (10, false)
        }
    };
    genet.speed = speed;
    genet.full_duplex = full;
    // UMAC_CMD bits 3:2 are the speed (0 = 10, 1 = 100, 2 = 1000); the MAC's duplex follows the link through the RGMII in-band status.
    let code = match speed {
        1000 => 2,
        100 => 1,
        _ => 0,
    };
    let promiscuous = if genet.promiscuous { CMD_PROMISC } else { 0 };
    let enabled = read(UMAC_CMD) & (CMD_TX_EN | CMD_RX_EN);
    write(UMAC_CMD, code << CMD_SPEED_SHIFT | promiscuous | enabled);
    update(EXT_RGMII_OOB_CTRL, OOB_DISABLE, RGMII_LINK | RGMII_MODE_EN | ID_MODE_DIS);
    true
}

/// Waits up to `milliseconds` for the link, restarting auto-negotiation once if there is none at first.
fn wait_for_link(genet: &mut Genet, milliseconds: u64) -> bool {
    if update_link(genet) {
        return true;
    }
    let phy = genet.phy;
    if let Some(control) = mdio_read(phy, BMCR) {
        mdio_write(phy, BMCR, control | 0x1200); // auto-negotiation on, restart
    }
    let start = timer::micros();
    while timer::micros() - start < milliseconds * 1000 {
        timer::busy_wait_ms(50);
        if update_link(genet) {
            return true;
        }
    }
    false
}

fn dma_enable(on: bool) {
    let control = (1u32 << (DEFAULT_RING + 1)) | DMA_EN;
    if on {
        write(TDMA_REGS + DMA_CTRL, control);
        update(RDMA_REGS + DMA_CTRL, 0, control);
    } else {
        update(TDMA_REGS + DMA_CTRL, DMA_EN, 0);
        update(RDMA_REGS + DMA_CTRL, DMA_EN, 0);
        write(UMAC_TX_FLUSH, 1);
        timer::busy_wait_ms(1);
        write(UMAC_TX_FLUSH, 0);
    }
}

/// Starts the Pi 4's Ethernet; returns true when a MAC is up (the link may still be down: see `status`).
pub fn init() -> bool {
    if crate::gic::bases().0 != 0xff84_1000 {
        return false;
    }
    crate::paging::map_device(BASE, SIZE);
    let revision = read(SYS_REV_CTRL);
    let major = (revision >> 24) & 0xf;
    if major != 6 {
        crate::println!("Genet: unexpected revision {revision:#x} at {BASE:#x} (expected major 6, GENETv5); not started.");
        return false;
    }
    // The address the firmware left in the MAC registers.
    let (low, high) = (read(UMAC_MAC0), read(UMAC_MAC1));
    let mut mac = [(low >> 24) as u8, (low >> 16) as u8, (low >> 8) as u8, low as u8, (high >> 8) as u8, high as u8];
    if mac == [0; 6] || mac[0] & 1 != 0 {
        mac = [0x02, 0xf0, 0x5b, 0x00, 0x00, 0x01];
        crate::println!("Genet: no usable MAC address from the firmware; using {mac:02x?}.");
    }
    let Some(rx_buffers) = memory::allocate_contiguous((DESCRIPTORS * BUFFER_LENGTH / 4096) as u64, Some(1 << 32)) else {
        crate::println!("Genet: no memory for the receive buffers.");
        return false;
    };
    let Some(tx_buffer) = memory::allocate_contiguous(1, Some(1 << 32)) else {
        crate::println!("Genet: no memory for the transmit buffer.");
        return false;
    };

    write(SYS_PORT_CTRL, PORT_MODE_EXT_GPHY);
    // MAC reset (with RGMII loopback so the receive clock is stable while it runs), flush the buffers.
    write(SYS_RBUF_FLUSH_CTRL, 2);
    timer::busy_wait_ms(1);
    write(SYS_RBUF_FLUSH_CTRL, 0);
    timer::busy_wait_ms(1);
    write(UMAC_CMD, 0);
    write(UMAC_CMD, CMD_SW_RESET | CMD_LCL_LOOP_EN);
    timer::busy_wait_ms(1);
    write(UMAC_CMD, 0);
    write(UMAC_MIB_CTRL, 7); // reset the RX, runt and TX counters
    write(UMAC_MIB_CTRL, 0);
    write(UMAC_MAX_FRAME_LEN, MAX_FRAME);
    update(RBUF_CTRL, 0, RBUF_ALIGN_2B);
    write(RBUF_TBUF_SIZE_CTRL, 1);
    write(UMAC_MAC0, u32::from_be_bytes([mac[0], mac[1], mac[2], mac[3]]));
    write(UMAC_MAC1, u32::from(mac[4]) << 8 | u32::from(mac[5]));

    dma_enable(false);
    // Receive ring: all descriptors, each pointing at its own buffer, owned by the MAC.
    write(RDMA_REGS + DMA_SCB_BURST_SIZE, DMA_MAX_BURST_LENGTH);
    write(RDMA_RING + DMA_START_ADDR, 0);
    write(RDMA_READ_PTR, 0);
    write(RDMA_WRITE_PTR, 0);
    write(RDMA_RING + DMA_END_ADDR, (DESCRIPTORS as u64 * DESC_SIZE / 4 - 1) as u32);
    // The producer index cannot be set, so the consumer index is set to match it.
    write(RDMA_PROD_INDEX, 0);
    write(RDMA_CONS_INDEX, 0);
    let consumed = read(RDMA_PROD_INDEX) & 0xffff;
    write(RDMA_CONS_INDEX, consumed);
    write(RDMA_RING + DMA_RING_BUF_SIZE, (DESCRIPTORS as u32) << 16 | BUFFER_LENGTH as u32);
    write(RDMA_XON_XOFF_THRESH, DMA_FC_THRESH);
    write(RDMA_RING + DMA_MBUF_DONE_THRESH, 1);
    write(RDMA_REGS + DMA_RING_CFG, 1 << DEFAULT_RING);
    for index in 0..DESCRIPTORS as u64 {
        let address = rx_buffers + index * BUFFER_LENGTH as u64;
        write(RX_DESCRIPTORS + index * DESC_SIZE + 4, address as u32);
        write(RX_DESCRIPTORS + index * DESC_SIZE + 8, (address >> 32) as u32);
        write(RX_DESCRIPTORS + index * DESC_SIZE, (BUFFER_LENGTH as u32) << 16 | DMA_OWN);
    }
    // Transmit ring.
    write(TDMA_REGS + DMA_SCB_BURST_SIZE, DMA_MAX_BURST_LENGTH);
    write(TDMA_RING + DMA_START_ADDR, 0);
    write(TDMA_READ_PTR, 0);
    write(TDMA_WRITE_PTR, 0);
    write(TDMA_RING + DMA_END_ADDR, (DESCRIPTORS as u64 * DESC_SIZE / 4 - 1) as u32);
    let transmitted = read(TDMA_CONS_INDEX) & 0xffff;
    write(TDMA_PROD_INDEX, transmitted);
    write(TDMA_RING + DMA_MBUF_DONE_THRESH, 1);
    write(TDMA_FLOW_PERIOD, 0);
    write(TDMA_RING + DMA_RING_BUF_SIZE, (DESCRIPTORS as u32) << 16 | BUFFER_LENGTH as u32);
    write(TDMA_REGS + DMA_RING_CFG, 1 << DEFAULT_RING);
    dma_enable(true);

    let Some(phy) = find_phy() else {
        crate::println!("Genet: MAC {:02x?} is up but no PHY answers on MDIO.", mac);
        return false;
    };
    let id = (u32::from(mdio_read(phy, PHY_ID1).unwrap_or(0)) << 16) | u32::from(mdio_read(phy, PHY_ID2).unwrap_or(0));
    configure_phy_delays(phy);
    let mut genet = Genet {
        mac,
        rx_buffers,
        tx_buffer,
        rx_index: 0,
        consumed,
        tx_index: (transmitted & 0xff) as usize,
        phy,
        speed: 0,
        full_duplex: true,
        promiscuous: false,
        sent: 0,
        received: 0,
        dropped: 0,
        last_drop: 0,
    };
    write(UMAC_CMD, CMD_TX_EN | CMD_RX_EN);
    let linked = wait_for_link(&mut genet, 4000);
    crate::println!(
        "Genet: MAC {:02x?}, PHY {id:#010x} at MDIO {phy}, {}.",
        mac,
        if linked { "link up" } else { "no link (cable?)" }
    );
    if linked {
        crate::println!("Genet: {} Mbit/s, {} duplex.", genet.speed, if genet.full_duplex { "full" } else { "half" });
    }
    unsafe { *core::ptr::addr_of_mut!(GENET) = Some(genet) };
    // Frames wake the CPU: a level interrupt per receive event, cleared by the handler, the ring still drained by
    // `receive` in the foreground.
    write(INTRL2_MASK_SET, 0xffff_ffff);
    write(INTRL2_CLEAR, 0xffff_ffff);
    crate::gic::register(IRQ_INTID, true, interrupt);
    write(INTRL2_MASK_CLEAR, IRQ_RXDMA);
    true
}

/// IRQ context: note which cause fired and clear it (the status bits are latched, the line is level).
fn interrupt(_intid: u32) {
    let status = read(INTRL2_0) & IRQ_RXDMA;
    write(INTRL2_CLEAR, 0xffff_ffff);
    IRQ_BITS.fetch_or(status, core::sync::atomic::Ordering::Relaxed);
    IRQS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    // A cause that cannot be cleared would starve everything else: more than 2000 interrupts in one timer tick
    // masks them all, and the foreground keeps draining the ring.
    use core::sync::atomic::Ordering::Relaxed;
    static TICK: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
    static BURST: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
    let now = crate::gic::irq_ticks();
    if TICK.swap(now, Relaxed) != now {
        BURST.store(0, Relaxed);
    }
    if BURST.fetch_add(1, Relaxed) > 2000 {
        write(INTRL2_MASK_SET, 0xffff_ffff);
        STORM.store(true, Relaxed);
    }
}

static STORM: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

pub fn present() -> bool {
    state().is_some()
}

pub fn mac() -> Option<[u8; 6]> {
    state().map(|genet| genet.mac)
}

pub fn set_promiscuous(on: bool) -> bool {
    let Some(genet) = state() else { return false };
    genet.promiscuous = on;
    update(UMAC_CMD, CMD_PROMISC, if on { CMD_PROMISC } else { 0 });
    true
}

/// Sends one Ethernet frame (without FCS; the MAC appends it); false when there is no NIC or the MAC did not take it.
pub fn send(frame: &[u8]) -> bool {
    let Some(genet) = state() else { return false };
    if frame.len() > BUFFER_LENGTH - 4 {
        return false;
    }
    let length = frame.len().max(60);
    let buffer = genet.tx_buffer as *mut u8;
    unsafe {
        core::ptr::write_bytes(buffer, 0, length);
        core::ptr::copy_nonoverlapping(frame.as_ptr(), buffer, frame.len());
    }
    let produced = read(TDMA_PROD_INDEX) & 0xffff;
    let descriptor = TX_DESCRIPTORS + genet.tx_index as u64 * DESC_SIZE;
    write(descriptor + 4, genet.tx_buffer as u32);
    write(descriptor + 8, (genet.tx_buffer >> 32) as u32);
    write(descriptor, (length as u32) << 16 | TX_QUEUE_TAG | DMA_TX_APPEND_CRC | DMA_SOP | DMA_EOP);
    genet.tx_index = (genet.tx_index + 1) % DESCRIPTORS;
    let produced = (produced + 1) & 0xffff;
    barrier();
    write(TDMA_PROD_INDEX, produced);
    let start = timer::micros();
    while (produced.wrapping_sub(read(TDMA_CONS_INDEX)) & 0xffff) != 0 {
        if timer::micros() - start > 10_000 {
            genet.dropped += 1;
            return false;
        }
    }
    genet.sent += 1;
    true
}

/// Copies the next received frame into `buffer`; returns its length, or 0 when none is waiting.
pub fn receive(buffer: &mut [u8]) -> usize {
    let Some(genet) = state() else { return 0 };
    loop {
        if read(RDMA_PROD_INDEX) & 0xffff == genet.consumed {
            return 0;
        }
        barrier();
        let descriptor = RX_DESCRIPTORS + genet.rx_index as u64 * DESC_SIZE;
        let status = read(descriptor);
        if status == (BUFFER_LENGTH as u32) << 16 | DMA_OWN {
            // The index moved before the MAC wrote this descriptor: look again later.
            return 0;
        }
        let length = ((status >> 16) & 0xfff) as usize;
        let address = genet.rx_buffers + genet.rx_index as u64 * BUFFER_LENGTH as u64;
        let whole = status & (DMA_SOP | DMA_EOP) == (DMA_SOP | DMA_EOP) && status & DMA_RX_ERRORS == 0 && length > RX_PAD;
        let copied = if whole { (length - RX_PAD).min(buffer.len()) } else { 0 };
        if copied != 0 {
            unsafe { core::ptr::copy_nonoverlapping((address as *const u8).add(RX_PAD), buffer.as_mut_ptr(), copied) };
            genet.received += 1;
        } else {
            genet.dropped += 1;
            genet.last_drop = status;
        }
        // Give the descriptor back to the MAC.
        genet.consumed = (genet.consumed + 1) & 0xffff;
        genet.rx_index = (genet.rx_index + 1) % DESCRIPTORS;
        barrier();
        write(RDMA_CONS_INDEX, genet.consumed);
        if copied != 0 {
            return copied;
        }
    }
}

/// The `nic` shell command: state, a link re-check and the PHY's registers.
pub fn status() {
    let Some(genet) = state() else {
        crate::println!("nic: no Ethernet controller (the Genet is only started on a Raspberry Pi 4).");
        return;
    };
    let linked = update_link(genet);
    crate::println!("Genet {:02x?}: {}", genet.mac, if linked { "link up" } else { "no link" });
    if linked {
        crate::println!("  {} Mbit/s, {} duplex.", genet.speed, if genet.full_duplex { "full" } else { "half" });
    }
    crate::println!(
        "  receive interrupts {} (causes seen {:#x}).",
        IRQS.load(core::sync::atomic::Ordering::Relaxed),
        IRQ_BITS.load(core::sync::atomic::Ordering::Relaxed) >> 13
    );
    if STORM.load(core::sync::atomic::Ordering::Relaxed) {
        crate::println!("  interrupt storm: receive interrupts are masked, the ring is polled.");
    }
    crate::println!("  frames sent {}, received {}, dropped {} (last status {:08x}).", genet.sent, genet.received, genet.dropped, genet.last_drop);
    crate::println!(
        "  PHY {}: BMCR {:04x?} BMSR {:04x?} ADV {:04x?} LPA {:04x?} CTRL1000 {:04x?} STAT1000 {:04x?}",
        genet.phy,
        mdio_read(genet.phy, BMCR).unwrap_or(0xffff),
        mdio_read(genet.phy, BMSR).unwrap_or(0xffff),
        mdio_read(genet.phy, ADVERTISE).unwrap_or(0xffff),
        mdio_read(genet.phy, LPA).unwrap_or(0xffff),
        mdio_read(genet.phy, CTRL1000).unwrap_or(0xffff),
        mdio_read(genet.phy, STAT1000).unwrap_or(0xffff)
    );
}

/// `nic rx`: for three seconds prints a line per received frame (length, destination, source, EtherType, IPv4 protocol).
pub fn show_rx() {
    let mut frame = [0u8; 1600];
    let start = timer::micros();
    let mut shown = 0;
    while timer::micros() - start < 3_000_000 && shown < 40 {
        let length = receive(&mut frame);
        if length < 14 {
            continue;
        }
        shown += 1;
        crate::println!(
            "  {length:4} B to {:02x?} from {:02x?} type {:02x}{:02x} proto {:02x}",
            &frame[0..6],
            &frame[6..12],
            frame[12],
            frame[13],
            frame.get(23).copied().unwrap_or(0)
        );
    }
}
