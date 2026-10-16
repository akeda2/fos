//! A polling driver for the Intel 82540EM ("e1000"), the NIC QEMU emulates by default
//! for x86 guests. Real Intel gigabit parts (I219, I350, 82579) use close cousins of
//! this register layout; they come with milestone M7.
//!
//! How it works: the card's registers sit in a memory BAR. Frames move through two
//! rings of 16-byte descriptors in RAM. For receive, software hands the card empty
//! buffers; the card fills one, sets the DD (descriptor done) bit and moves on, and
//! software gives the buffer back by advancing the tail register RDT. For transmit,
//! software fills a buffer and a descriptor and advances TDT; the card sets DD when
//! the frame is out. Everything here is polled: no interrupt is used, so there is
//! no handler to get wrong and no lost wakeup.

use core::ptr::{read_volatile, write_volatile};

use crate::{memory, paging, pci};

pub const VENDOR_INTEL: u16 = 0x8086;
/// The classic family: 0x100e is the 82540EM (QEMU's `e1000`); 0x100f and 0x1004 are the
/// same programming model.
#[allow(dead_code)] // documents the family; probing matches by device id elsewhere
const CLASSIC_IDS: &[u16] = &[0x100e, 0x100f, 0x1004];
/// The "e1000e" family: 0x10d3 is the 82574L (QEMU's `e1000e`), 0x1503 the 82579V (a real
/// ITX board), 0x1502 the 82579LM (Dell Latitude E6320; same MAC as the 82579V, added on that
/// evidence, not yet run on that laptop), 0x15b7 an I219-LM (a Skylake PCH: confirmed on real hardware by v1, which
/// found the TCTL fix below; Linux's K1/ULP power-management workarounds are not needed
/// for this narrow use). They keep the legacy descriptor
/// layout but have a stricter reset sequence (see `reset_e1000e`) and a couple of extra
/// control bits.
/// 0x156f is the other I219-LM of the same Skylake PCH generation (Linux's `board_pch_spt` takes 0x156f and 0x15b7
/// alike; a Dell Latitude E7270 has it), added on that evidence: not yet run on that laptop.
const E1000E_IDS: &[u16] = &[0x10d3, 0x1502, 0x1503, 0x15b7, 0x156f];
/// The I350 ("igb" family, a server NIC; the Skylake test machine has four ports). Same
/// legacy descriptors, but the registers moved (multi-queue layout, see `IGB_REGS`), each
/// queue needs an explicit enable bit, and the PHY is shared with management firmware
/// through the `SWSM`/`SW_FW_SYNC` handshake (`reset_i350`). 0x1521 is the copper part,
/// confirmed on real hardware by v1. Never add 0x1520, the SR-IOV virtual function: it has
/// no PHY access and would hang this sequence.
const I350_IDS: &[u16] = &[0x1521];
pub const DEVICE_IDS: &[u16] = &[0x100e, 0x100f, 0x1004, 0x10d3, 0x1502, 0x1503, 0x15b7, 0x156f, 0x1521];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Variant {
    Classic,
    E1000e,
    I350,
}

fn variant_of(device_id: u16) -> Variant {
    if E1000E_IDS.contains(&device_id) {
        Variant::E1000e
    } else if I350_IDS.contains(&device_id) {
        Variant::I350
    } else {
        Variant::Classic
    }
}

/// Where a family keeps the registers that differ between families (offsets from BAR 0).
/// A zero means "this family has no such register".
struct Regs {
    icr: usize,
    ics: usize,
    ims: usize,
    imc: usize,
    itr: usize,
    rdbal: usize,
    rdbah: usize,
    rdlen: usize,
    rdh: usize,
    rdt: usize,
    tdbal: usize,
    tdbah: usize,
    tdlen: usize,
    tdh: usize,
    tdt: usize,
    rxdctl: usize,
    srrctl: usize,
    txdctl: usize,
}

const LEGACY_REGS: Regs = Regs {
    icr: 0x00c0, ics: 0x00c8, ims: 0x00d0, imc: 0x00d8, itr: 0x00c4,
    rdbal: 0x2800, rdbah: 0x2804, rdlen: 0x2808, rdh: 0x2810, rdt: 0x2818,
    tdbal: 0x3800, tdbah: 0x3804, tdlen: 0x3808, tdh: 0x3810, tdt: 0x3818,
    rxdctl: 0, srrctl: 0, txdctl: 0x3828,
};

/// The I350's layout: interrupt registers moved to 0x1500 to make room for MSI-X, and the
/// rings are per-queue arrays (queue 0 at 0xc000 for receive, 0xe000 for transmit).
const IGB_REGS: Regs = Regs {
    icr: 0x1500, ics: 0x1504, ims: 0x1508, imc: 0x150c, itr: 0,
    rdbal: 0xc000, rdbah: 0xc004, rdlen: 0xc008, rdh: 0xc010, rdt: 0xc018,
    tdbal: 0xe000, tdbah: 0xe004, tdlen: 0xe008, tdh: 0xe010, tdt: 0xe018,
    rxdctl: 0xc028, srrctl: 0xc00c, txdctl: 0xe028,
};

fn regs_of(variant: Variant) -> &'static Regs {
    if variant == Variant::I350 { &IGB_REGS } else { &LEGACY_REGS }
}

const REG_CTRL: usize = 0x0000;
#[allow(dead_code)]
const REG_ICR: usize = 0x00c0;
#[allow(dead_code)]
const REG_ITR: usize = 0x00c4;
#[allow(dead_code)]
const REG_ICS: usize = 0x00c8;
#[allow(dead_code)]
const REG_IMS: usize = 0x00d0;
#[allow(dead_code)]
const REG_IMC: usize = 0x00d8;
const REG_EECD: usize = 0x0010;
const REG_EXTCNF_CTRL: usize = 0x0f00;
#[allow(dead_code)]
const REG_TXDCTL0: usize = 0x3828;
const REG_IVAR: usize = 0x00e4;
const REG_RCTL: usize = 0x0100;
const REG_TCTL: usize = 0x0400;
const REG_TIPG: usize = 0x0410;
#[allow(dead_code)]
const REG_RDBAL: usize = 0x2800;
#[allow(dead_code)]
const REG_RDBAH: usize = 0x2804;
#[allow(dead_code)]
/// Statistics registers, clear on read: missed packets (the receive ring was full) and receive no buffers.
const REG_MPC: usize = 0x4010;
const REG_RNBC: usize = 0x40a0;
// IEEE 1588 hardware timestamping (82580 family: the I350). SYSTIM is a free-running 40-bit nanosecond
// counter (SYSTIMH carries 8 bits, so it wraps about every 18 minutes); the card latches the SYSTIM value
// of one received PTP event frame in RXSTMP and of one transmitted frame in TXSTMP, until software reads
// the high word. Register map and sequence follow Linux's igb_ptp.c; only real hardware can check it.
const REG_SYSTIML: usize = 0xb600;
const REG_SYSTIMH: usize = 0xb604;
const REG_SYSTIMR: usize = 0xb6f8;
const REG_TSAUXC: usize = 0xb640;
const REG_TIMINCA: usize = 0xb608;
/// SRRCTL: descriptor type 1 (advanced, one buffer) and "put the receive timestamp in front of the frame".
const SRRCTL_DESCTYPE_ADV_ONEBUF: u32 = 0x0200_0000;
const SRRCTL_TIMESTAMP: u32 = 0x4000_0000;
/// TSYNCRXCTL type: stamp every received frame (82580 and later).
const RXCTL_TYPE_ALL: u32 = 0x08;
/// Advanced receive write-back status bits (the dword at offset 8): done, end of packet, "a timestamp precedes the frame".
const ADV_DD: u32 = 1;
const ADV_EOP: u32 = 2;
const ADV_TSIP: u32 = 1 << 15;
/// The timestamp header the card puts before a frame: 16 bytes, SYSTIML and SYSTIMH in the last eight.
const TS_HEADER: usize = 16;
const REG_TSYNCTXCTL: usize = 0xb614;
const REG_TXSTMPL: usize = 0xb618;
const REG_TXSTMPH: usize = 0xb61c;
const REG_TSYNCRXCTL: usize = 0xb620;
const REG_RXSTMPL: usize = 0xb624;
const REG_RXSTMPH: usize = 0xb628;
const REG_TSYNCRXCFG: usize = 0x5f50;
// The e1000e family with IEEE 1588 (82579, I217/I218/I219; Linux's e1000e ptp.c): the same SYSTIM, TXSTMP, RXSTMP and
// TSYNC registers, but SYSTIM is a free-running 64-bit counter in fractions of a nanosecond (`ns = SYSTIM >> shift`),
// TIMINCA must be programmed (nominal rate), and the clock cannot be written: a step is an offset kept here.
const REG_FEXTNVM7: usize = 0xe4;
const REG_RXMTRL: usize = 0xb634;
const REG_RXUDP: usize = 0xb638;
/// Per chip: (nominal TIMINCA, shift from SYSTIM units to ns, needs FEXTNVM7 bit 0 to run the clock).
/// 82579: 96 MHz (INCPERIOD 3, INCVALUE 125 << 17). I219 of the Skylake PCH: 24 MHz (INCPERIOD 3, INCVALUE 125 << 14).
fn pch_clock(device_id: u16) -> Option<(u32, u32, bool)> {
    match device_id {
        0x1502 | 0x1503 => Some((3 << 24 | 125 << 17, 19, false)),
        0x15b7 | 0x156f => Some((3 << 24 | 125 << 14, 14, true)),
        _ => None,
    }
}
const REG_ETQF3: usize = 0x5cbc;
const REG_FTQF3: usize = 0x59ec;
const TSYNC_VALID: u32 = 1;
const TSYNC_ENABLE: u32 = 1 << 4;
/// TSYNCRXCTL type 0x0a (bits 3:1 = 5): the PTP v2 event messages, over layer 2 or UDP.
const RXCTL_TYPE_EVENT_V2: u32 = 0x0a;
/// ETQF: filter enable (26) and "timestamp this ethertype" (30), ethertype in 15:0 (0x88f7 = PTP over Ethernet).
const ETQF_PTP_L2: u32 = (1 << 26) | (1 << 30) | 0x88f7;
const REG_IMIR3: usize = 0x5a8c;
const REG_IMIREXT3: usize = 0x5aac;
const IMIREXT_SIZE_BP: u32 = 0x1000;
const IMIREXT_CTRL_BP: u32 = 0x8_0000;
/// FTQF: protocol UDP, port filter in IMIR, VF bypass (15), "timestamp" (27); mask bits 31:28 set except the protocol one (28).
const FTQF_UDP_STAMP: u32 = 17 | (1 << 15) | (1 << 27) | 0xe000_0000;
/// Advanced transmit data descriptor: type data (DTYP 3), end of packet, insert FCS, report status,
/// descriptor extension, and MAC_TSTAMP to have the card timestamp this frame.
const ADV_TX_DATA_TS: u32 = 0x0030_0000 | (1 << 24) | (1 << 25) | (1 << 27) | (1 << 29) | 0x0008_0000;
/// Advanced transmit data descriptor without a timestamp: data type, descriptor extension, end of packet, insert FCS,
/// report status (the card then writes DD back).
const ADV_TX_DATA: u32 = 0x0030_0000 | (1 << 24) | (1 << 25) | (1 << 27) | (1 << 29);
/// Advanced transmit context descriptor type (DTYP 2) with the descriptor extension bit.
const ADV_CTXT: u32 = 0x0020_0000 | (1 << 29);
/// POPTS.TXSM (insert the TCP/UDP checksum) in the data descriptor's olinfo/status dword, bits 15:8.
const ADV_TXSM: u32 = 0x02 << 8;
/// Whether `frame` is a PTP event message (Sync, Delay_Req, ...: messageType below 8) over Ethernet (0x88f7)
/// or UDP port 319, i.e. what the card stamps.
fn is_ptp_event(frame: &[u8]) -> bool {
    if frame.len() >= 15 && frame[12..14] == [0x88, 0xf7] {
        return frame[14] & 0x08 == 0;
    }
    if frame.len() >= 42 && frame[12..14] == [0x08, 0x00] && frame[23] == 17 {
        let udp = 14 + usize::from(frame[14] & 0x0f) * 4;
        return frame.len() >= udp + 8 && frame[udp + 2..udp + 4] == [0x01, 0x3f];
    }
    false
}
const REG_RDLEN: usize = 0x2808;
#[allow(dead_code)]
const REG_RDH: usize = 0x2810;
#[allow(dead_code)]
const REG_RDT: usize = 0x2818;
#[allow(dead_code)]
const REG_TDBAL: usize = 0x3800;
#[allow(dead_code)]
const REG_TDBAH: usize = 0x3804;
#[allow(dead_code)]
const REG_TDLEN: usize = 0x3808;
#[allow(dead_code)]
const REG_TDH: usize = 0x3810;
#[allow(dead_code)]
const REG_TDT: usize = 0x3818;
/// Multicast table array: 128 registers = 4096 bits, indexed by 12 bits of the destination address (RCTL.MO = 0).
/// VLAN filter table array: 128 registers, one bit per 12-bit VLAN id (the same address on the I350, 82574 and PCH parts).
const REG_VFTA: usize = 0x5600;
/// RCTL.VFE: drop tagged frames whose VLAN id is not set in the VFTA. Untagged frames are not affected.
const RCTL_VFE: u32 = 1 << 18;
const REG_MTA: usize = 0x5200;
const MTA_REGISTERS: usize = 128;
const REG_RAL0: usize = 0x5400;
const REG_RAH0: usize = 0x5404;
const REG_STATUS: usize = 0x0008;

const CTRL_FD: u32 = 1 << 0;
const CTRL_SLU: u32 = 1 << 6;
const CTRL_RST: u32 = 1 << 26;
const RCTL_EN: u32 = 1 << 1;
const RCTL_BAM: u32 = 1 << 15;
const RCTL_SECRC: u32 = 1 << 26;
/// Long packet enable: without it the card drops anything over 1518 bytes on the wire (FCS included), which is a
/// 1514-byte frame; a VLAN-tagged full frame is 1522.
const RCTL_LPE: u32 = 1 << 5;
const DESCRIPTOR_EOP: u8 = 1 << 1;
const TCTL_EN: u32 = 1 << 1;
const TCTL_PSP: u32 = 1 << 3;
const TCTL_CT: u32 = 0x10 << 4;
const TCTL_COLD: u32 = 0x40 << 12;
const STATUS_LU: u32 = 1 << 1;
const STATUS_FD: u32 = 1;
const STATUS_GIO_MASTER_ENABLE: u32 = 1 << 19;
const CTRL_GIO_MASTER_DISABLE: u32 = 1 << 2;
const EECD_AUTO_RD: u32 = 1 << 9;
const EXTCNF_MDIO_SW_OWNERSHIP: u32 = 1 << 5;
const TCTL_RTLC: u32 = 1 << 24;
const TCTL_UNDOCUMENTED_28_29: u32 = (1 << 28) | (1 << 29);
const TXDCTL_QUEUE_ENABLE: u32 = 1 << 25;
const ICR_LSC: u32 = 1 << 2;
const ICR_RXQ0: u32 = 1 << 20;
const ICR_TXQ0: u32 = 1 << 22;
const ICR_OTHER: u32 = 1 << 24;
const MSIX_CAUSES: u32 = ICR_RXQ0 | ICR_TXQ0 | ICR_OTHER;
const ICR_RXSEQ: u32 = 1 << 3;
const ICR_RXDMT0: u32 = 1 << 4;
const ICR_RXO: u32 = 1 << 6;
const ICR_RXT0: u32 = 1 << 7;
/// Causes worth an interrupt: a received frame, the ring running low, an overrun, link
/// changes. (Transmit completion is polled through the descriptor's DD bit.)
const INTERRUPT_MASK: u32 = ICR_LSC | ICR_RXSEQ | ICR_RXDMT0 | ICR_RXO | ICR_RXT0;
/// Interrupt throttle: minimum gap between interrupts in 256 ns units, so a 64-byte-frame
/// flood costs at most ~39,000 interrupts a second instead of ~1.4 million.
const ITR_VALUE: u32 = 100;

const DESCRIPTOR_DONE: u8 = 1;
const TX_EOP_IFCS_RS: u8 = (1 << 0) | (1 << 1) | (1 << 3);

/// 256 descriptors of 16 bytes fill the one page the ring gets. 128 let a 1 Gbit/s burst of 1400-byte
/// frames overrun a slower CPU (rack: ~360 missed packets in 5 s); the card counts them in MPC.
const RX_COUNT: usize = 128;
/// 16 descriptors were 10 us of runway at 64-byte line rate: a tape that came back late for a few microseconds left
/// the wire idle. 128 (the ring still fits its page) keeps a burst queued while the tape is away.
const TX_COUNT: usize = 128;
const BUFFER_SIZE: usize = 2048;
/// 1514 plus a 4-byte VLAN tag: a tagged full-size frame must cross a bridge intact.
pub const MAX_FRAME: usize = 1518;

#[repr(C)]
#[derive(Clone, Copy)]
struct Descriptor {
    address: u64,
    length: u16,
    /// Receive: checksum. Transmit: checksum offset and command.
    word: u16,
    status: u8,
    errors_or_css: u8,
    special: u16,
}

/// Per-interface interrupt bookkeeping, written by the interrupt handler and read by the
/// foreground. Indexed by the interface number so the handler needs no pointer to the driver.
pub struct IsrState {
    registers: core::sync::atomic::AtomicUsize,
    /// Offset of this NIC's ICR (it differs between families).
    icr: core::sync::atomic::AtomicUsize,
    pub interrupts: core::sync::atomic::AtomicU64,
    pub overruns: core::sync::atomic::AtomicU64,
    /// Frames the card dropped for want of a receive buffer (MPC) and the times it had none (RNBC),
    /// summed over `nic list` calls: both registers clear when read.
    pub missed: core::sync::atomic::AtomicU64,
    pub no_buffers: core::sync::atomic::AtomicU64,
    pub link_changes: core::sync::atomic::AtomicU64,
    /// CRC errors, symbol errors, PAUSE frames received (XOFF) and sent (XOFF), summed over `nic list` calls (clear on read).
    pub crc_errors: core::sync::atomic::AtomicU64,
    pub symbol_errors: core::sync::atomic::AtomicU64,
    pub xoff_received: core::sync::atomic::AtomicU64,
    pub xoff_sent: core::sync::atomic::AtomicU64,
}

pub const MAX_INTERFACES: usize = 8;
pub static ISR: [IsrState; MAX_INTERFACES] = [const {
    IsrState {
        registers: core::sync::atomic::AtomicUsize::new(0),
        icr: core::sync::atomic::AtomicUsize::new(0),
        interrupts: core::sync::atomic::AtomicU64::new(0),
        overruns: core::sync::atomic::AtomicU64::new(0),
        missed: core::sync::atomic::AtomicU64::new(0),
        no_buffers: core::sync::atomic::AtomicU64::new(0),
        link_changes: core::sync::atomic::AtomicU64::new(0),
        crc_errors: core::sync::atomic::AtomicU64::new(0),
        symbol_errors: core::sync::atomic::AtomicU64::new(0),
        xoff_received: core::sync::atomic::AtomicU64::new(0),
        xoff_sent: core::sync::atomic::AtomicU64::new(0),
    }
}; MAX_INTERFACES];

/// The MSI handler: reading ICR acknowledges (and clears) the causes; counting is all it
/// does. The foreground notices new frames by polling the ring after it wakes.
fn isr(index: usize) {
    use core::sync::atomic::Ordering;
    let state = &ISR[index];
    let registers = state.registers.load(Ordering::Relaxed);
    if registers == 0 {
        return;
    }
    let cause = read(registers, state.icr.load(Ordering::Relaxed));
    if cause == 0 {
        return; // a shared line, or a spurious one: not claimed
    }
    crate::msi::claim();
    state.interrupts.fetch_add(1, Ordering::Relaxed);
    if cause & ICR_RXO != 0 {
        state.overruns.fetch_add(1, Ordering::Relaxed);
    }
    if cause & ICR_LSC != 0 {
        state.link_changes.fetch_add(1, Ordering::Relaxed);
    }
}

/// How a NIC's interrupts are delivered.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Delivery {
    /// No interrupts: the foreground polls.
    Polled,
    /// MSI on this vector.
    Msi(u8),
    /// MSI-X (table entry 0) on this vector.
    Msix(u8),
    /// Legacy INTx: I/O APIC pin `gsi`, delivered on `vector`.
    Intx { gsi: u32, vector: u8 },
}

pub struct E1000 {
    registers: usize,
    pub variant: Variant,
    r: &'static Regs,
    pub index: usize,
    pub delivery: Delivery,
    pub mac: [u8; 6],
    rx_ring: u64,
    tx_ring: u64,
    rx_buffers: u64,
    tx_buffers: u64,
    rx_next: usize,
    /// Inside an oversize frame that spans several buffers (RCTL.LPE lets it in): drop it up to its last descriptor.
    rx_skipping: bool,
    tx_next: usize,
    pub frames_received: u64,
    pub frames_sent: u64,
    /// Frames "sent" while the link was down (lost, as on an unplugged cable).
    pub frames_dropped_no_link: u64,
    /// Sends since the link was last read; the STATUS read is an uncached MMIO load, so it is paid every 16th send.
    link_checks: u8,
    link_ok: bool,
    /// Timestamps armed (`ts_enable`): `receive` then attaches the card's stamp to each PTP event frame.
    ts_on: bool,
    /// Every received frame is stamped (`ts_stamp_all`), in a 16-byte header the card puts before it.
    ts_all: bool,
    /// SYSTIM stamp of the frame `receive` returned last (0: none), taken by `take_stamp`.
    last_stamp: u64,
    /// `pch_clock` for this card (e1000e family with timestamps), else None.
    ts_clock: Option<(u32, u32, bool)>,
    /// Nanoseconds added to the card's clock by `ts_step` on a card whose SYSTIM cannot be written.
    ts_offset: u64,
    /// Groups joined per multicast-table bit (`multicast_join`): two groups can share a bit, so each is counted.
    mc_users: [u8; 4096],
    /// Shadow of the VLAN filter table (VFTA): one bit per VLAN id the card lets through when RCTL.VFE is on.
    vlan_ids: [u32; 128],
    promiscuous: bool,
    /// Transmit checksum offload on (I350): `send` hands IPv4 UDP/TCP frames to the card to checksum.
    tx_offload: bool,
    /// Ring slots that hold an advanced context descriptor (not a buffer address) until the slot is reused.
    tx_ctx: [bool; TX_COUNT],
}

#[derive(Debug)]
pub enum Error {
    NoBar,
    OutOfMemory,
    ResetTimeout,
    BadMac,
    NoSlot,
}

fn read(registers: usize, offset: usize) -> u32 {
    unsafe { read_volatile((registers + offset) as *const u32) }
}
fn write(registers: usize, offset: usize, value: u32) {
    unsafe { write_volatile((registers + offset) as *mut u32, value) }
}

fn short_wait(ticks: u64) {
    crate::apic::wait_ticks(ticks);
}

/// The classic reset: set CTRL.RST and wait for it to clear.
fn reset_classic(registers: usize) -> Result<(), Error> {
    write(registers, REG_CTRL, CTRL_RST);
    let mut waited = 0;
    while read(registers, REG_CTRL) & CTRL_RST != 0 {
        waited += 1;
        if waited > 1_000_000 {
            return Err(Error::ResetTimeout);
        }
        core::hint::spin_loop();
    }
    Ok(())
}

const REG_SWSM: usize = 0x5b50;
const REG_SW_FW_SYNC: usize = 0x5b5c;
const SWSM_SMBI: u32 = 1 << 0;
const SWSM_SWESMBI: u32 = 1 << 1;
const SWFW_PHY0_SM: u32 = 1 << 0;

/// The I350 reset: take the PHY away from management firmware first (a two-step handshake,
/// as Linux's `igb` does it: win the `SWSM` hardware semaphore, claim the PHY bit in
/// `SW_FW_SYNC` under its protection, drop the semaphore), reset, then give the PHY back.
/// Every wait is bounded so a firmware fault reports an error instead of hanging boot.
fn reset_i350(registers: usize) -> Result<(), Error> {
    let mut attempts = 100_000u32;
    while read(registers, REG_SWSM) & SWSM_SMBI != 0 {
        attempts -= 1;
        if attempts == 0 {
            return Err(Error::ResetTimeout);
        }
        core::hint::spin_loop();
    }
    write(registers, REG_SWSM, read(registers, REG_SWSM) | SWSM_SWESMBI);
    let mut attempts = 100_000u32;
    loop {
        let sync = read(registers, REG_SW_FW_SYNC);
        if sync & SWFW_PHY0_SM == 0 {
            write(registers, REG_SW_FW_SYNC, sync | SWFW_PHY0_SM);
            break;
        }
        attempts -= 1;
        if attempts == 0 {
            write(registers, REG_SWSM, read(registers, REG_SWSM) & !(SWSM_SMBI | SWSM_SWESMBI));
            return Err(Error::ResetTimeout);
        }
        core::hint::spin_loop();
    }
    write(registers, REG_SWSM, read(registers, REG_SWSM) & !(SWSM_SMBI | SWSM_SWESMBI));
    let result = reset_classic(registers);
    write(registers, REG_SW_FW_SYNC, read(registers, REG_SW_FW_SYNC) & !SWFW_PHY0_SM);
    result
}

/// The e1000e-family reset, following Linux's `e1000_reset_hw_ich8lan`: quiesce receive and
/// transmit, stop the device mastering the bus and wait for in-flight DMA to drain, take
/// software ownership of the PHY (`EXTCNF_CTRL.MDIO_SW_OWNERSHIP`, so management firmware
/// is not mid-access), reset, and then **touch no register at all** for a few ticks: on
/// real 82579V silicon even a read right after `CTRL.RST` hangs the machine. Completion is
/// signalled by `EECD.AUTO_RD` (the NVM auto-read), not by `CTRL.RST` clearing.
fn reset_e1000e(registers: usize) -> Result<(), Error> {
    write(registers, REG_RCTL, 0);
    write(registers, REG_TCTL, TCTL_PSP);
    short_wait(2);
    write(registers, REG_CTRL, read(registers, REG_CTRL) | CTRL_GIO_MASTER_DISABLE);
    for _ in 0..100 {
        if read(registers, REG_STATUS) & STATUS_GIO_MASTER_ENABLE == 0 {
            break;
        }
        short_wait(1);
    }
    let mut owned = false;
    for _ in 0..10 {
        write(registers, REG_EXTCNF_CTRL, read(registers, REG_EXTCNF_CTRL) | EXTCNF_MDIO_SW_OWNERSHIP);
        if read(registers, REG_EXTCNF_CTRL) & EXTCNF_MDIO_SW_OWNERSHIP != 0 {
            owned = true;
            break;
        }
        short_wait(1);
    }
    if !owned {
        return Err(Error::ResetTimeout);
    }
    write(registers, REG_CTRL, CTRL_RST);
    short_wait(3); // no register access in this window
    let mut done = false;
    for _ in 0..20 {
        if read(registers, REG_EECD) & EECD_AUTO_RD != 0 {
            done = true;
            break;
        }
        short_wait(1);
    }
    write(registers, REG_EXTCNF_CTRL, read(registers, REG_EXTCNF_CTRL) & !EXTCNF_MDIO_SW_OWNERSHIP);
    if done { Ok(()) } else { Err(Error::ResetTimeout) }
}

impl E1000 {
    pub fn init(device: &pci::Device, index: usize) -> Result<E1000, Error> {
        if index >= MAX_INTERFACES {
            return Err(Error::NoSlot); // more NICs than interface slots: skip it, do not index past the tables
        }
        device.force_power_state_d0(); // a PCH-integrated function can be left in D3hot by the BIOS
        device.enable_memory_and_bus_master();
        let (base, size) = device.memory_bar(0).ok_or(Error::NoBar)?;
        paging::map_device(base, size.max(0x20000));
        let registers = base as usize;

        let variant = variant_of(device.id);
        let r = regs_of(variant);
        write(registers, r.imc, u32::MAX); // quiet until `enable_interrupts`
        match variant {
            Variant::Classic => reset_classic(registers)?,
            Variant::E1000e => reset_e1000e(registers)?,
            Variant::I350 => reset_i350(registers)?,
        }
        write(registers, r.imc, u32::MAX);

        // The I350 loads its receive-address registers from NVM *after* CTRL.RST clears, so
        // the first read can return garbage (confirmed on real hardware): retry until it is
        // a plausible address. The other families have the address ready here.
        let mut mac = [0u8; 6];
        let mut valid = false;
        for _ in 0..if variant == Variant::I350 { 20 } else { 1 } {
            let ral = read(registers, REG_RAL0);
            let rah = read(registers, REG_RAH0);
            mac[..4].copy_from_slice(&ral.to_le_bytes());
            mac[4..].copy_from_slice(&(rah as u16).to_le_bytes());
            if mac != [0; 6] && mac != [0xff; 6] {
                valid = true;
                break;
            }
            short_wait(1);
        }
        if !valid {
            return Err(Error::BadMac);
        }

        // DMA memory: one page of descriptors per ring, buffers as one contiguous run.
        // Below 4 GiB keeps the high address registers at zero on every variant.
        let limit = Some(1u64 << 32);
        let rx_ring = memory::allocate_contiguous(1, limit).ok_or(Error::OutOfMemory)?;
        let tx_ring = memory::allocate_contiguous(1, limit).ok_or(Error::OutOfMemory)?;
        let pages_per_buffer = (BUFFER_SIZE as u64) / memory::FRAME; // 2 KiB: half a page
        let _ = pages_per_buffer;
        let rx_buffers = memory::allocate_contiguous((RX_COUNT * BUFFER_SIZE) as u64 / memory::FRAME, limit).ok_or(Error::OutOfMemory)?;
        let tx_buffers = memory::allocate_contiguous((TX_COUNT * BUFFER_SIZE) as u64 / memory::FRAME, limit).ok_or(Error::OutOfMemory)?;

        for index in 0..RX_COUNT {
            let descriptor = Descriptor { address: rx_buffers + (index * BUFFER_SIZE) as u64, length: 0, word: 0, status: 0, errors_or_css: 0, special: 0 };
            unsafe { write_volatile((rx_ring as *mut Descriptor).add(index), descriptor) };
        }
        for index in 0..TX_COUNT {
            // status = DD: "this transmit descriptor is free"
            let descriptor = Descriptor { address: tx_buffers + (index * BUFFER_SIZE) as u64, length: 0, word: 0, status: DESCRIPTOR_DONE, errors_or_css: 0, special: 0 };
            unsafe { write_volatile((tx_ring as *mut Descriptor).add(index), descriptor) };
        }

        write(registers, r.rdbal, rx_ring as u32);
        write(registers, r.rdbah, 0);
        write(registers, r.rdlen, (RX_COUNT * 16) as u32);
        write(registers, r.rdh, 0);
        // All but one buffer owned by the card. (The I350 gets its tail only after its queue
        // is enabled, below.)
        write(registers, r.rdt, if variant == Variant::I350 { 0 } else { (RX_COUNT - 1) as u32 });
        write(registers, r.tdbal, tx_ring as u32);
        write(registers, r.tdbah, 0);
        write(registers, r.tdlen, (TX_COUNT * 16) as u32);
        write(registers, r.tdh, 0);
        write(registers, r.tdt, 0);
        write(registers, REG_TIPG, 0x0060_200a);
        let mut tctl = TCTL_EN | TCTL_PSP | TCTL_CT | TCTL_COLD;
        if variant == Variant::E1000e {
            // Found on a real I219-LM (the Skylake machine) by comparing with Linux's live TCTL
            // (`ethtool -d`) when transmit sat stuck with TDH never moving: full-duplex
            // collision distance 0x3f (not the half-duplex 0x40), collision threshold 0x0f,
            // re-transmit on late collision (RTLC), and two bits (28, 29) Linux sets on this
            // silicon and nobody can name. TCTL first, then the queue enable, as v1 does.
            tctl = TCTL_EN | TCTL_PSP | (0x0f << 4) | (0x3f << 12) | TCTL_RTLC | TCTL_UNDOCUMENTED_28_29;
            write(registers, REG_TCTL, tctl);
            write(registers, r.txdctl, TXDCTL_QUEUE_ENABLE);
        }
        write(registers, REG_TCTL, tctl);
        for index in 0..MTA_REGISTERS {
            write(registers, REG_MTA + index * 4, 0); // no multicast group until a tape joins one
        }
        write(registers, REG_RCTL, RCTL_EN | RCTL_BAM | RCTL_SECRC | RCTL_LPE); // 2 KiB buffers, broadcast + own MAC
        write(registers, REG_CTRL, CTRL_SLU | CTRL_FD);
        if variant == Variant::I350 {
            // 2 KiB receive buffers (SRRCTL counts KiB), then enable both queues explicitly and
            // wait for the receive enable to latch before handing the card any buffers.
            // Advanced one-buffer descriptors: the only format that lets the card prepend a timestamp to a frame.
            write(registers, r.srrctl, (BUFFER_SIZE / 1024) as u32 | SRRCTL_DESCTYPE_ADV_ONEBUF);
            write(registers, r.txdctl, TXDCTL_QUEUE_ENABLE);
            write(registers, r.rxdctl, TXDCTL_QUEUE_ENABLE);
            let mut waited = 0;
            while read(registers, r.rxdctl) & TXDCTL_QUEUE_ENABLE == 0 {
                waited += 1;
                if waited > 100_000 {
                    return Err(Error::ResetTimeout);
                }
                core::hint::spin_loop();
            }
            write(registers, r.rdt, (RX_COUNT - 1) as u32);
        }

        Ok(E1000 { registers, variant, r, index, delivery: Delivery::Polled, mac, rx_ring, tx_ring, rx_buffers, tx_buffers, rx_next: 0, rx_skipping: false, tx_next: 0, frames_received: 0, frames_sent: 0, frames_dropped_no_link: 0, link_checks: 0, link_ok: true, ts_on: false, ts_all: false, last_stamp: 0, ts_clock: pch_clock(device.id), ts_offset: 0, mc_users: [0; 4096], vlan_ids: [0; 128], promiscuous: false, tx_offload: false, tx_ctx: [false; TX_COUNT] })
    }

    /// Promiscuous mode: receive every frame on the wire, not only ours and broadcasts
    /// (RCTL.UPE unicast, MPE multicast). A bridge or a link-impairment tape needs it.
    pub fn set_promiscuous(&mut self, on: bool) {
        let control = read(self.registers, REG_RCTL);
        let bits = (1 << 3) | (1 << 4);
        write(self.registers, REG_RCTL, if on { control | bits } else { control & !bits });
        self.promiscuous = on;
        self.vlan_apply();
    }

    /// Lets frames tagged with VLAN id `vid` through the card's VLAN filter (`on`) or stops them. The filter (RCTL.VFE)
    /// turns on with the first id and off with the last, so a tape that never asks sees every tag as before; it also
    /// stays off while the card is promiscuous (Linux does the same: a bridge wants every tag). Untagged frames always
    /// pass. False for an id above 4095.
    pub fn vlan_filter(&mut self, vid: u16, on: bool) -> bool {
        // The 82579 and I219 (PCH) have no VLAN filter: VFE and the VFTA do not hold a write (read back on real parts).
        if vid > 4095 || self.ts_clock.is_some() {
            return false;
        }
        let (register, bit) = (usize::from(vid >> 5), 1u32 << (vid & 31));
        self.vlan_ids[register] = if on { self.vlan_ids[register] | bit } else { self.vlan_ids[register] & !bit };
        write(self.registers, REG_VFTA + register * 4, self.vlan_ids[register]);
        self.vlan_apply();
        true
    }

    /// Forgets every VLAN id and turns the filter off (a tape ended).
    pub fn vlan_clear(&mut self) {
        if self.ts_clock.is_some() {
            return;
        }
        self.vlan_ids = [0; 128];
        for index in 0..128 {
            write(self.registers, REG_VFTA + index * 4, 0);
        }
        self.vlan_apply();
    }

    /// `nic regs`: the receive filter state: RCTL, the VLAN filter table's set bits and what the card holds in its MTA.
    pub fn dump_registers(&self) {
        let control = read(self.registers, REG_RCTL);
        crate::println!("   RCTL {control:#010x}: VFE {}, UPE {}, MPE {}, BAM {}", control >> 18 & 1, control >> 3 & 1, control >> 4 & 1, control >> 15 & 1);
        for index in 0..128 {
            let word = read(self.registers, REG_VFTA + index * 4);
            if word != 0 {
                crate::println!("   VFTA[{index}] {word:#010x} (shadow {:#010x})", self.vlan_ids[index]);
            }
        }
        for index in 0..MTA_REGISTERS {
            let word = read(self.registers, REG_MTA + index * 4);
            if word != 0 {
                crate::println!("   MTA[{index}] {word:#010x}");
            }
        }
    }

    /// Whether the VLAN filter is on: ids are registered and the card is not promiscuous.
    pub fn vlan_active(&self) -> bool {
        read(self.registers, REG_RCTL) & RCTL_VFE != 0
    }

    fn vlan_apply(&mut self) {
        let wanted = !self.promiscuous && self.vlan_ids.iter().any(|word| *word != 0);
        let control = read(self.registers, REG_RCTL);
        write(self.registers, REG_RCTL, if wanted { control | RCTL_VFE } else { control & !RCTL_VFE });
    }

    /// Multicast promiscuous (RCTL.MPE, bit 4): receive every multicast frame. The multicast table array is left
    /// empty, so without this the NIC drops all multicast, including IGMP queries.
    pub fn set_multicast(&mut self, on: bool) {
        let control = read(self.registers, REG_RCTL);
        write(self.registers, REG_RCTL, if on { control | (1 << 4) } else { control & !(1 << 4) });
    }

    /// The multicast table has 4096 bits on the I350 and the 82574 (hash = address bits 47:36) but 1024 on the ICH/PCH
    /// parts (82579, I217-I219; Linux's `e1000_hash_mc_addr` with mta_reg_count 32), which take bits 47:38. The two
    /// agree only for addresses whose last two bytes are 0, such as PTP's 01:1b:19:00:00:00.
    fn multicast_hash(&self, mac: [u8; 6]) -> usize {
        fosfor_abi::multicast_hash(mac, self.ts_clock.is_some())
    }

    /// Joins (`on`) or leaves one multicast group in the card's hash filter, so that frames for it arrive without opening
    /// every multicast frame (`set_multicast`). The card tests one bit per frame, chosen by the destination address
    /// (`multicast_hash`; the byte order of Linux igb's `igb_hash_mc_addr`): other groups can share a bit and
    /// get through too, so a tape still checks the address. False if the address is not a multicast one.
    pub fn multicast_join(&mut self, mac: [u8; 6], on: bool) -> bool {
        if mac[0] & 1 == 0 {
            return false;
        }
        let hash = self.multicast_hash(mac);
        let users = &mut self.mc_users[hash];
        *users = if on { users.saturating_add(1) } else { users.saturating_sub(1) };
        let register = REG_MTA + (hash >> 5) * 4;
        let bits = read(self.registers, register);
        let bit = 1u32 << (hash & 31);
        write(self.registers, register, if self.mc_users[hash] > 0 { bits | bit } else { bits & !bit });
        true
    }

    /// Forgets every joined group (a tape that ended without leaving them).
    pub fn multicast_clear(&mut self) {
        self.mc_users = [0; 4096];
        for index in 0..MTA_REGISTERS {
            write(self.registers, REG_MTA + index * 4, 0);
        }
    }

    /// Whether the card has IEEE 1588 timestamp registers this driver knows (the I350, the 82579 and the Skylake I219).
    pub fn ts_supported(&self) -> bool {
        self.variant == Variant::I350 || self.ts_clock.is_some()
    }

    /// A raw SYSTIM or stamp value in nanoseconds on the clock tapes see.
    fn ts_ns(&self, raw: u64) -> u64 {
        match self.ts_clock {
            Some((_, shift, _)) => (raw >> shift).wrapping_add(self.ts_offset),
            None => raw,
        }
    }

    /// Starts the timestamp clock and arms receive stamping of PTP-over-Ethernet event frames
    /// (Sync, Delay_Req, ...) and transmit stamping. Returns false on a card without it.
    pub fn ts_enable(&mut self) -> bool {
        if !self.ts_supported() {
            return false;
        }
        if let Some((timinca, _, fextnvm7)) = self.ts_clock {
            if fextnvm7 {
                let value = read(self.registers, REG_FEXTNVM7);
                write(self.registers, REG_FEXTNVM7, value | 1);
            }
            write(self.registers, REG_TIMINCA, timinca);
            write(self.registers, REG_TSYNCTXCTL, TSYNC_ENABLE);
            write(self.registers, REG_TSYNCRXCTL, TSYNC_ENABLE | RXCTL_TYPE_EVENT_V2);
            // Which frames are stamped: Ethernet type 0x88f7 (PTP over layer 2) and UDP destination port 319.
            write(self.registers, REG_RXMTRL, 0x88f7);
            write(self.registers, REG_RXUDP, u32::from(319u16.swap_bytes()));
            let _ = read(self.registers, REG_TXSTMPH);
            let _ = read(self.registers, REG_RXSTMPH);
            self.ts_on = true;
            return true;
        }
        write(self.registers, REG_TSAUXC, 0); // bit 31 clear: SYSTIM runs
        write(self.registers, REG_TSYNCTXCTL, TSYNC_ENABLE);
        write(self.registers, REG_TSYNCRXCTL, TSYNC_ENABLE | RXCTL_TYPE_EVENT_V2);
        write(self.registers, REG_TSYNCRXCFG, 0);
        write(self.registers, REG_ETQF3, ETQF_PTP_L2);
        // Layer 4: UDP destination port 319 (PTP events) is stamped too. FTQF3 matches the protocol only (UDP = 17),
        // IMIR3 holds the port (as the card reads it, byte-swapped), IMIREXT3 bypasses size and control-bit checks.
        write(self.registers, REG_IMIR3, 319u16.swap_bytes() as u32);
        write(self.registers, REG_IMIREXT3, IMIREXT_SIZE_BP | IMIREXT_CTRL_BP);
        write(self.registers, REG_FTQF3, FTQF_UDP_STAMP);
        // Reading the high words releases any stamp left latched.
        let _ = read(self.registers, REG_TXSTMPH);
        let _ = read(self.registers, REG_RXSTMPH);
        if self.ts_all {
            self.ts_stamp_all(false);
        }
        self.ts_on = true;
        true
    }

    pub fn ts_all(&self) -> bool {
        self.ts_all
    }

    /// Stamps every received frame, not just PTP events (the card's clock at the moment the frame arrived), or stops
    /// doing so. Needs `ts_enable`. Receive is stopped for the moment the buffer format changes.
    pub fn ts_stamp_all(&mut self, on: bool) -> bool {
        if !self.ts_on || self.variant != Variant::I350 {
            return false;
        }
        let control = read(self.registers, REG_RCTL);
        write(self.registers, REG_RCTL, control & !RCTL_EN);
        let base = (BUFFER_SIZE / 1024) as u32 | SRRCTL_DESCTYPE_ADV_ONEBUF;
        write(self.registers, self.r.srrctl, if on { base | SRRCTL_TIMESTAMP } else { base });
        write(self.registers, REG_TSYNCRXCTL, TSYNC_ENABLE | if on { RXCTL_TYPE_ALL } else { RXCTL_TYPE_EVENT_V2 });
        write(self.registers, REG_RCTL, control);
        self.ts_all = on;
        true
    }

    /// The stamp of the frame `receive` returned last, once (0: the frame had none).
    pub fn take_stamp(&mut self) -> u64 {
        core::mem::take(&mut self.last_stamp)
    }

    /// The card's clock in nanoseconds. The value latches on the lowest register, SYSTIMR here.
    pub fn systim(&self) -> u64 {
        if self.ts_clock.is_none() {
            let _ = read(self.registers, REG_SYSTIMR);
        }
        let low = u64::from(read(self.registers, REG_SYSTIML));
        self.ts_ns(low | u64::from(read(self.registers, REG_SYSTIMH)) << 32)
    }

    /// Speeds the card's clock up (positive) or slows it (negative) by `ppb` parts per billion, through TIMINCA's
    /// fine adjustment (bit 31 = slower; the size is ppb * 2^26 / 1953125,
    /// as Linux igb computes it for the 82580 family). 0 restores the nominal rate. Returns the value written.
    pub fn ts_adjust(&mut self, ppb: i32) -> u32 {
        if let Some((timinca, _, _)) = self.ts_clock {
            // Here the increment itself is scaled: INCVALUE (the low 24 bits) * (1e9 + ppb) / 1e9.
            let base = i64::from(timinca & 0x00ff_ffff);
            let value = (base + base * i64::from(ppb) / 1_000_000_000) as u32 & 0x00ff_ffff;
            let written = (timinca & !0x00ff_ffff) | value;
            write(self.registers, REG_TIMINCA, written);
            return written;
        }
        let magnitude = (u64::from(ppb.unsigned_abs()) << 26) / 1_953_125;
        let value = (magnitude as u32 & 0x7fff_ffff) | if ppb < 0 { 0x8000_0000 } else { 0 };
        write(self.registers, REG_TIMINCA, value);
        value
    }

    /// Jumps the clock by `delta` nanoseconds: read, add, write back SYSTIML then SYSTIMH. The two reads and the write
    /// take about a microsecond, so a step is only that exact; ptp steps once and then steers with `ts_adjust`.
    pub fn ts_step(&mut self, delta: i64) {
        if self.ts_clock.is_some() {
            self.ts_offset = self.ts_offset.wrapping_add(delta as u64);
            return;
        }
        let target = self.systim().wrapping_add(delta as u64) & 0xff_ffff_ffff;
        write(self.registers, REG_SYSTIML, target as u32);
        write(self.registers, REG_SYSTIMH, (target >> 32) as u32);
    }

    /// TSYNCRXCTL, for diagnostics: bit 0 is "a stamp is latched".
    pub fn ts_rx_control(&self) -> u32 {
        read(self.registers, REG_TSYNCRXCTL)
    }

    /// The SYSTIM stamp of the last PTP event frame received, if one is latched (and releases it).
    /// Only one frame is stamped at a time, so poll often; the stamp belongs to the newest PTP frame.
    pub fn ts_rx_stamp(&mut self) -> Option<u64> {
        if read(self.registers, REG_TSYNCRXCTL) & TSYNC_VALID == 0 {
            return None;
        }
        let low = u64::from(read(self.registers, REG_RXSTMPL));
        Some(self.ts_ns(low | u64::from(read(self.registers, REG_RXSTMPH)) << 32))
    }

    /// Sends `frame` with an advanced descriptor that asks for a transmit stamp, and returns it
    /// as (sent, stamp); the stamp is 0 when none came within a bounded wait. One at a time.
    pub fn send_stamped(&mut self, frame: &[u8]) -> (bool, u64) {
        if !self.ts_supported() || frame.len() > MAX_FRAME || !self.link_up() {
            return (false, 0);
        }
        let _ = read(self.registers, REG_TXSTMPH);
        if self.ts_clock.is_some() {
            // A data descriptor (DEXT, DTYP_D) with the timestamp bit in its upper dword, as Linux's e1000_tx_queue builds it.
            if !self.claim_tx_slot(self.tx_next) {
                return (false, 0);
            }
            let descriptor = unsafe { (self.tx_ring as *mut Descriptor).add(self.tx_next) };
            let buffer = (self.tx_buffers + (self.tx_next * BUFFER_SIZE) as u64) as *mut u8;
            let length = frame.len().max(60);
            unsafe {
                core::ptr::write_bytes(buffer, 0, length);
                core::ptr::copy_nonoverlapping(frame.as_ptr(), buffer, frame.len());
                write_volatile(core::ptr::addr_of_mut!((*descriptor).length), length as u16);
                write_volatile(core::ptr::addr_of_mut!((*descriptor).word), (u16::from(TX_EOP_IFCS_RS | 0x20) << 8) | 0x10);
                write_volatile(core::ptr::addr_of_mut!((*descriptor).status), 0x10);
            }
            self.tx_next = (self.tx_next + 1) % TX_COUNT;
            write(self.registers, self.r.tdt, self.tx_next as u32);
            self.frames_sent += 1;
            for _ in 0..2_000_000u32 {
                if read(self.registers, REG_TSYNCTXCTL) & TSYNC_VALID != 0 {
                    let low = u64::from(read(self.registers, REG_TXSTMPL));
                    return (true, self.ts_ns(low | u64::from(read(self.registers, REG_TXSTMPH)) << 32));
                }
                core::hint::spin_loop();
            }
            return (true, 0);
        }
        if !self.claim_tx_slot(self.tx_next) {
            return (false, 0);
        }
        let descriptor = unsafe { (self.tx_ring as *mut Descriptor).add(self.tx_next) };
        let buffer = (self.tx_buffers + (self.tx_next * BUFFER_SIZE) as u64) as *mut u8;
        let length = frame.len().max(60);
        unsafe {
            core::ptr::write_bytes(buffer, 0, length);
            core::ptr::copy_nonoverlapping(frame.as_ptr(), buffer, frame.len());
            let words = descriptor as *mut u32;
            write_volatile(words.add(2), ADV_TX_DATA_TS | length as u32);
            write_volatile(words.add(3), (length as u32) << 14); // PAYLEN; DD (status) clear
        }
        self.tx_next = (self.tx_next + 1) % TX_COUNT;
        write(self.registers, self.r.tdt, self.tx_next as u32);
        self.frames_sent += 1;
        // Wait for the descriptor and the stamp; a few hundred microseconds at most on a live link.
        let mut stamp = 0;
        for _ in 0..2_000_000u32 {
            if read(self.registers, REG_TSYNCTXCTL) & TSYNC_VALID != 0 {
                let low = u64::from(read(self.registers, REG_TXSTMPL));
                stamp = low | u64::from(read(self.registers, REG_TXSTMPH)) << 32;
                break;
            }
            core::hint::spin_loop();
        }
        // Leave the slot the way the legacy send path expects it: done, nothing else set.
        let mut waited = 0u32;
        while unsafe { read_volatile(core::ptr::addr_of!((*descriptor).status)) } & DESCRIPTOR_DONE == 0 && waited < 2_000_000 {
            waited += 1;
            core::hint::spin_loop();
        }
        unsafe {
            let words = descriptor as *mut u32;
            write_volatile(words.add(2), 0);
            write_volatile(words.add(3), u32::from(DESCRIPTOR_DONE));
        }
        (true, stamp)
    }

    /// `nic list`: what the card itself counted lost for want of receive buffers. These are the
    /// frames a foreground that polls too slowly costs; the ring is `RX_COUNT` buffers deep.
    pub fn describe(&self) {
        let [missed, none, crc, symbol, xoff_rx, xoff_tx] = self.error_counters();
        crate::println!("   receive ring: {RX_COUNT} buffers; card counted {missed} missed packets, {none} no-buffer events");
        crate::println!("   errors: {crc} CRC, {symbol} symbol; PAUSE (XOFF): {xoff_rx} received, {xoff_tx} sent");
    }

    /// Running totals since boot of missed packets, no-buffer events, CRC errors, symbol errors and PAUSE (XOFF) frames
    /// received and sent. The statistics registers clear when read, so each read is added to a total here; a caller
    /// takes two snapshots and subtracts.
    pub fn error_counters(&self) -> [u64; 6] {
        use core::sync::atomic::Ordering;
        let state = &ISR[self.index];
        let take = |register: usize, total: &core::sync::atomic::AtomicU64| {
            let now = u64::from(read(self.registers, register));
            total.fetch_add(now, Ordering::Relaxed) + now
        };
        [
            take(REG_MPC, &state.missed),
            take(REG_RNBC, &state.no_buffers),
            take(0x4000, &state.crc_errors),
            take(0x4008, &state.symbol_errors),
            take(0x4050, &state.xoff_received),
            take(0x4054, &state.xoff_sent),
        ]
    }

    pub fn link_up(&self) -> bool {
        read(self.registers, REG_STATUS) & STATUS_LU != 0
    }

    /// The negotiated speed in Mbit/s and whether it is full duplex, from STATUS bits 7:6 (0 = 10, 1 = 100, 2 and 3 =
    /// 1000) and bit 0; `None` without link. A gigabit port that comes up at 100 is a cable or port fault.
    pub fn link_speed(&self) -> Option<(u16, bool)> {
        let status = read(self.registers, REG_STATUS);
        if status & STATUS_LU == 0 {
            return None;
        }
        Some((match (status >> 6) & 3 { 0 => 10, 1 => 100, _ => 1000 }, status & STATUS_FD != 0))
    }

    /// The PHY's basic mode control register (MII register 0) through MDIC, for parking. Only the classic and I350
    /// parts (internal copper PHY at MDIO address 1); the e1000e family (82574, 82579, I219) shares its PHY with the
    /// management engine and needs the software/firmware semaphore, so it is left alone.
    pub fn phy_control(&self) -> Option<u16> {
        if self.variant == Variant::E1000e {
            return None;
        }
        self.mdic(2 << 26, 0)
    }

    pub fn set_phy_control(&mut self, value: u16) -> bool {
        self.variant != Variant::E1000e && self.mdic(1 << 26, value).is_some()
    }

    /// One MDIC cycle: opcode (read 2, write 1) in bits 27:26, PHY address 1, register 0; ready is bit 28, error bit 30.
    /// A standard MII register (clause 22) of the internal copper PHY; classic and I350 parts only (see `phy_control`).
    pub fn phy_read(&self, register: u32) -> Option<u16> {
        if self.variant == Variant::E1000e {
            return None;
        }
        self.mdic(2 << 26 | (register & 31) << 16, 0)
    }

    fn mdic(&self, opcode: u32, data: u16) -> Option<u16> {
        write(self.registers, 0x20, opcode | 1 << 21 | u32::from(data));
        for _ in 0..100_000 {
            let value = read(self.registers, 0x20);
            if value & (1 << 30) != 0 {
                return None;
            }
            if value & (1 << 28) != 0 {
                return Some(value as u16);
            }
            core::hint::spin_loop();
        }
        None
    }

    /// Copies the next received frame into `out`; returns its length, or None if the
    /// ring is empty.
    pub fn receive(&mut self, out: &mut [u8]) -> Option<usize> {
        let advanced = self.variant == Variant::I350;
        loop {
            let descriptor = unsafe { (self.rx_ring as *mut Descriptor).add(self.rx_next) };
            let buffer = (self.rx_buffers + (self.rx_next * BUFFER_SIZE) as u64) as *const u8;
            let (done, end_of_packet, length, stamped) = if advanced {
                // Write-back format: status dword at offset 8, length at 12.
                let words = descriptor as *const u8;
                let status = unsafe { read_volatile(words.add(8) as *const u32) };
                (status & ADV_DD != 0, status & ADV_EOP != 0, unsafe { read_volatile(words.add(12) as *const u16) } as usize, status & ADV_TSIP != 0)
            } else {
                let status = unsafe { read_volatile(core::ptr::addr_of!((*descriptor).status)) };
                (status & DESCRIPTOR_DONE != 0, status & DESCRIPTOR_EOP != 0, unsafe { read_volatile(core::ptr::addr_of!((*descriptor).length)) } as usize, false)
            };
            if !done {
                return None;
            }
            let skip = if stamped { TS_HEADER.min(length) } else { 0 };
            let length = (length - skip).min(BUFFER_SIZE - skip).min(out.len());
            let keep = !self.rx_skipping && end_of_packet;
            let mut stamp = 0;
            if keep {
                unsafe { core::ptr::copy_nonoverlapping(buffer.add(skip), out.as_mut_ptr(), length) };
                if stamped {
                    let (low, high) = unsafe { (read_volatile(buffer.add(8) as *const u32), read_volatile(buffer.add(12) as *const u32)) };
                    stamp = (u64::from(low) | u64::from(high) << 32) & 0xff_ffff_ffff;
                }
            }
            self.rx_skipping = !end_of_packet;
            // Hand the buffer back. The advanced write-back overwrote the buffer address, so it is written again.
            if advanced {
                unsafe {
                    write_volatile(descriptor as *mut u64, buffer as u64);
                    write_volatile((descriptor as *mut u64).add(1), 0);
                }
            } else {
                unsafe { write_volatile(core::ptr::addr_of_mut!((*descriptor).status), 0) };
            }
            write(self.registers, self.r.rdt, self.rx_next as u32);
            self.rx_next = (self.rx_next + 1) % RX_COUNT;
            if keep {
                self.frames_received += 1;
                // Every frame carries its own stamp in `ts_stamp_all` mode. Otherwise only PTP event frames are
                // stamped, and only one at a time: the stamp belongs to this frame when it is one (the ring is read
                // in arrival order, and PTP events are tens of ms apart).
                self.last_stamp = if self.ts_all {
                    stamp
                } else if self.ts_on && is_ptp_event(&out[..length]) {
                    self.ts_rx_stamp().unwrap_or(0)
                } else {
                    0
                };
                return Some(length);
            }
        }
    }

    /// Turns transmit checksum offload on or off. Only the I350 (advanced descriptors); false elsewhere.
    pub fn set_tx_offload(&mut self, on: bool) -> bool {
        if self.variant != Variant::I350 {
            return false;
        }
        self.tx_offload = on;
        true
    }

    /// Waits (bounded) until transmit slot `slot` is free and makes it an ordinary slot again. A slot that holds a
    /// context descriptor has no status of its own that says "done" (its dwords are context fields), but the card works the
    /// ring in order, so the data descriptor after it completing means the context was consumed too; its buffer address
    /// was overwritten by the context and is put back here.
    fn claim_tx_slot(&mut self, slot: usize) -> bool {
        let watched = if self.tx_ctx[slot] { (slot + 1) % TX_COUNT } else { slot };
        let descriptor = unsafe { (self.tx_ring as *mut Descriptor).add(watched) };
        let mut waited = 0u32;
        while unsafe { read_volatile(core::ptr::addr_of!((*descriptor).status)) } & DESCRIPTOR_DONE == 0 {
            waited += 1;
            if waited > 2_000_000 {
                return false;
            }
            core::hint::spin_loop();
        }
        if self.tx_ctx[slot] {
            let own = unsafe { (self.tx_ring as *mut Descriptor).add(slot) };
            let free = Descriptor { address: self.tx_buffers + (slot * BUFFER_SIZE) as u64, length: 0, word: 0, status: DESCRIPTOR_DONE, errors_or_css: 0, special: 0 };
            unsafe { write_volatile(own, free) };
            self.tx_ctx[slot] = false;
        }
        true
    }

    /// Queues an IPv4 UDP or TCP frame with the card computing the L4 checksum (a context descriptor, then an advanced
    /// data descriptor with TXSM). The pseudo-header sum goes into the checksum field first, as the card wants it (not
    /// complemented); it then sums the L4 header and payload to the end of the buffer, whose padding is zeros, and
    /// complements. Only a UDP frame whose checksum field is 0 ("none") or a TCP frame is touched; anything else, a
    /// fragment or an odd frame goes the ordinary way. Returns None for those.
    fn send_offloaded(&mut self, frame: &[u8]) -> Option<bool> {
        if frame.len() < 42 || frame[12..14] != [0x08, 0x00] || frame[14] >> 4 != 4 {
            return None;
        }
        let ip_length = usize::from(frame[14] & 15) * 4;
        let total = usize::from(u16::from_be_bytes([frame[16], frame[17]]));
        let fragment = u16::from_be_bytes([frame[20], frame[21]]) & 0x3fff;
        let l4 = 14 + ip_length;
        if ip_length < 20 || fragment != 0 || total < ip_length || 14 + total > frame.len() {
            return None;
        }
        let (checksum_at, tucmd) = match frame[23] {
            17 if l4 + 8 <= 14 + total && frame[l4 + 6..l4 + 8] == [0, 0] => (l4 + 6, 0x0400u32),
            6 if l4 + 20 <= 14 + total => (l4 + 16, 0x0400 | 0x0800),
            _ => return None,
        };
        if !self.link_up() {
            self.frames_dropped_no_link += 1;
            return Some(true);
        }
        let (context, data) = (self.tx_next, (self.tx_next + 1) % TX_COUNT);
        if !self.claim_tx_slot(context) || !self.claim_tx_slot(data) {
            return Some(false);
        }
        let mut sum = 0u32;
        for pair in frame[26..34].chunks(2) {
            sum += u32::from(u16::from_be_bytes([pair[0], pair[1]]));
        }
        sum += u32::from(frame[23]) + (total - ip_length) as u32;
        while sum >> 16 != 0 {
            sum = (sum & 0xffff) + (sum >> 16);
        }
        let length = (14 + total).max(60);
        let buffer = (self.tx_buffers + (data * BUFFER_SIZE) as u64) as *mut u8;
        unsafe {
            core::ptr::write_bytes(buffer, 0, length);
            core::ptr::copy_nonoverlapping(frame.as_ptr(), buffer, 14 + total);
            *buffer.add(checksum_at) = (sum >> 8) as u8;
            *buffer.add(checksum_at + 1) = sum as u8;
            let words = (self.tx_ring as *mut Descriptor).add(data) as *mut u32;
            write_volatile(words.add(2), ADV_TX_DATA | length as u32);
            write_volatile(words.add(3), (length as u32) << 14 | ADV_TXSM); // PAYLEN, POPTS; DD (status) clear
            let words = (self.tx_ring as *mut Descriptor).add(context) as *mut u32;
            write_volatile(words, 14 << 9 | ip_length as u32); // MACLEN, IPLEN
            write_volatile(words.add(1), 0);
            write_volatile(words.add(2), ADV_CTXT | tucmd);
            write_volatile(words.add(3), 0);
        }
        self.tx_ctx[context] = true;
        self.tx_next = (data + 1) % TX_COUNT;
        write(self.registers, self.r.tdt, self.tx_next as u32);
        self.frames_sent += 1;
        Some(true)
    }

    /// Queues one frame (padded to the 60-byte minimum). Returns false if the ring
    /// stays full, which means the link is down or the card is wedged.
    pub fn send(&mut self, frame: &[u8]) -> bool {
        if frame.len() > MAX_FRAME {
            return false;
        }
        if self.tx_offload {
            if let Some(result) = self.send_offloaded(frame) {
                return result;
            }
        }
        // No link: the card will not transmit and the descriptor would never complete, so a
        // sender would wait out its timeout and report a hardware failure. A cable that is not
        // plugged in just loses the frame; do the same, so protocol code (a router starting
        // with a dead uplink, say) keeps working and retries as it would on a real wire.
        self.link_checks = self.link_checks.wrapping_add(1);
        if self.link_checks % 16 == 1 || !self.link_ok {
            self.link_ok = self.link_up();
        }
        if !self.link_ok {
            self.frames_dropped_no_link += 1;
            return true;
        }
        if !self.claim_tx_slot(self.tx_next) {
            return false;
        }
        let descriptor = unsafe { (self.tx_ring as *mut Descriptor).add(self.tx_next) };
        let buffer = (self.tx_buffers + (self.tx_next * BUFFER_SIZE) as u64) as *mut u8;
        let length = frame.len().max(60);
        unsafe {
            core::ptr::write_bytes(buffer, 0, length);
            core::ptr::copy_nonoverlapping(frame.as_ptr(), buffer, frame.len());
            write_volatile(core::ptr::addr_of_mut!((*descriptor).length), length as u16);
            write_volatile(core::ptr::addr_of_mut!((*descriptor).word), u16::from(TX_EOP_IFCS_RS) << 8);
            write_volatile(core::ptr::addr_of_mut!((*descriptor).status), 0);
        }
        self.tx_next = (self.tx_next + 1) % TX_COUNT;
        write(self.registers, self.r.tdt, self.tx_next as u32);
        self.frames_sent += 1;
        true
    }

    /// Unmasks the receive/link causes, raises one cause by hand (link status through ICS)
    /// and reports whether `arrived` saw an interrupt within three ticks: the proof that the
    /// chosen delivery path really works, whatever it is.
    fn self_test(&self, msix: bool, arrived: impl Fn() -> u64) -> bool {
        let _ = read(self.registers, self.r.icr);
        if self.r.itr != 0 {
            write(self.registers, self.r.itr, ITR_VALUE);
        }
        // In MSI-X mode the 82574 reports its causes as per-vector bits instead: receive
        // queue 0 (bit 20), transmit queue 0 (22) and "other" (24: link status and the
        // rest), which IVAR maps to vectors.
        let mask = if msix { INTERRUPT_MASK | MSIX_CAUSES } else { INTERRUPT_MASK };
        write(self.registers, self.r.ims, mask);
        let before = arrived();
        write(self.registers, self.r.ics, if msix { ICR_OTHER } else { ICR_LSC });
        let start = crate::apic::ticks();
        while arrived() == before && crate::apic::ticks().wrapping_sub(start) < 3 {
            core::hint::spin_loop();
        }
        arrived() != before
    }

    fn quiet(&self) {
        write(self.registers, self.r.imc, u32::MAX);
        let _ = read(self.registers, self.r.icr);
    }

    /// Switches this NIC from polling to interrupts, trying in order: MSI-X (only when
    /// `prefer_msix`, and only on the 82574, whose `IVAR` register maps causes to vectors),
    /// MSI, then legacy INTx through the I/O APIC (probing candidate pins, since the ACPI
    /// routing table cannot be read). Each path must pass `self_test` to be kept; if all fail
    /// the NIC stays polled. Needs interrupts enabled and the local APIC running.
    pub fn enable_interrupts(&mut self, device: &pci::Device, prefer_msix: bool) -> Result<Delivery, &'static str> {
        use core::sync::atomic::Ordering;
        ISR[self.index].registers.store(self.registers, Ordering::Relaxed);
        ISR[self.index].icr.store(self.r.icr, Ordering::Relaxed);
        let mut reason = "no interrupt path passed its self-test";

        if prefer_msix && device.id == 0x10d3 && device.find_capability(0x11).is_some() {
            if let Some(vector) = crate::msi::allocate(isr, self.index) {
                if device.enable_msix(vector, crate::apic::id()) {
                    // 82574 MSI-X: IVAR maps receive queue 0, transmit queue 0 and "other" causes
                    // to vector 0 (each field: vector in the low bits, a valid bit above it).
                    write(self.registers, REG_IVAR, 0x8000_0000 | 0x8 | (0x8 << 8) | (0x8 << 16));
                    if self.self_test(true, || crate::msi::count(vector)) {
                        self.delivery_set(Delivery::Msix(vector));
                        return Ok(Delivery::Msix(vector));
                    }
                    self.quiet();
                    device.disable_msix();
                    crate::println!("  (MSI-X on vector {vector:#x}: the self-test message never arrived; trying MSI)");
                    reason = "the MSI-X self-test message never arrived";
                }
                crate::msi::release(vector);
            }
        }

        if device.find_capability(0x05).is_some() {
            match crate::msi::allocate(isr, self.index) {
                Some(vector) if device.enable_msi(vector, crate::apic::id()) => {
                    if self.self_test(false, || crate::msi::count(vector)) {
                        self.delivery_set(Delivery::Msi(vector));
                        return Ok(Delivery::Msi(vector));
                    }
                    self.quiet();
                    device.disable_msi();
                    crate::msi::release(vector);
                    reason = "the MSI self-test message never arrived";
                }
                Some(vector) => crate::msi::release(vector),
                None => reason = "no free MSI vector",
            }
        }

        // Legacy INTx: the PCI pins an Intel/QEMU chipset routes to the I/O APIC (GSI 16-23), then the firmware's
        // interrupt line (a hint).
        let (first, end) = crate::ioapic::gsi_range();
        let candidates = crate::msi::intx_candidates(device.interrupt_line());
        for (position, &gsi) in candidates.iter().enumerate() {
            if gsi == u32::MAX || gsi == 0 || gsi < first || gsi >= end || candidates[..position].contains(&gsi) {
                continue;
            }
            let Some(vector) = crate::msi::attach_intx(gsi, isr, self.index) else { continue };
            if self.self_test(false, || ISR[self.index].interrupts.load(core::sync::atomic::Ordering::Relaxed)) {
                self.delivery_set(Delivery::Intx { gsi, vector });
                return Ok(Delivery::Intx { gsi, vector });
            }
            self.quiet();
            crate::msi::detach_intx(vector, self.index);
            reason = "no I/O APIC pin delivered the self-test interrupt";
        }
        ISR[self.index].registers.store(0, Ordering::Relaxed);
        Err(reason)
    }

    fn delivery_set(&mut self, delivery: Delivery) {
        self.delivery = delivery;
    }
}
