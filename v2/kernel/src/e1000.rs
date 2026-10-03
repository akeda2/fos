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
const CLASSIC_IDS: &[u16] = &[0x100e, 0x100f, 0x1004];
/// The "e1000e" family: 0x10d3 is the 82574L (QEMU's `e1000e`), 0x1503 the 82579V (a real
/// ITX board), 0x15b7 an I219-LM (a Skylake PCH: confirmed on real hardware by v1, which
/// found the TCTL fix below; Linux's K1/ULP power-management workarounds are not needed
/// for this narrow use). They keep the legacy descriptor
/// layout but have a stricter reset sequence (see `reset_e1000e`) and a couple of extra
/// control bits.
const E1000E_IDS: &[u16] = &[0x10d3, 0x1503, 0x15b7];
/// The I350 ("igb" family, a server NIC; the Skylake test machine has four ports). Same
/// legacy descriptors, but the registers moved (multi-queue layout, see `IGB_REGS`), each
/// queue needs an explicit enable bit, and the PHY is shared with management firmware
/// through the `SWSM`/`SW_FW_SYNC` handshake (`reset_i350`). 0x1521 is the copper part,
/// confirmed on real hardware by v1. Never add 0x1520, the SR-IOV virtual function: it has
/// no PHY access and would hang this sequence.
const I350_IDS: &[u16] = &[0x1521];
pub const DEVICE_IDS: &[u16] = &[0x100e, 0x100f, 0x1004, 0x10d3, 0x1503, 0x15b7, 0x1521];

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
const REG_ICR: usize = 0x00c0;
const REG_ITR: usize = 0x00c4;
const REG_ICS: usize = 0x00c8;
const REG_IMS: usize = 0x00d0;
const REG_IMC: usize = 0x00d8;
const REG_EECD: usize = 0x0010;
const REG_EXTCNF_CTRL: usize = 0x0f00;
const REG_TXDCTL0: usize = 0x3828;
const REG_IVAR: usize = 0x00e4;
const REG_RCTL: usize = 0x0100;
const REG_TCTL: usize = 0x0400;
const REG_TIPG: usize = 0x0410;
const REG_RDBAL: usize = 0x2800;
const REG_RDBAH: usize = 0x2804;
const REG_RDLEN: usize = 0x2808;
const REG_RDH: usize = 0x2810;
const REG_RDT: usize = 0x2818;
const REG_TDBAL: usize = 0x3800;
const REG_TDBAH: usize = 0x3804;
const REG_TDLEN: usize = 0x3808;
const REG_TDH: usize = 0x3810;
const REG_TDT: usize = 0x3818;
const REG_RAL0: usize = 0x5400;
const REG_RAH0: usize = 0x5404;
const REG_STATUS: usize = 0x0008;

const CTRL_FD: u32 = 1 << 0;
const CTRL_SLU: u32 = 1 << 6;
const CTRL_RST: u32 = 1 << 26;
const RCTL_EN: u32 = 1 << 1;
const RCTL_BAM: u32 = 1 << 15;
const RCTL_SECRC: u32 = 1 << 26;
const TCTL_EN: u32 = 1 << 1;
const TCTL_PSP: u32 = 1 << 3;
const TCTL_CT: u32 = 0x10 << 4;
const TCTL_COLD: u32 = 0x40 << 12;
const STATUS_LU: u32 = 1 << 1;
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

const RX_COUNT: usize = 128;
const TX_COUNT: usize = 16;
const BUFFER_SIZE: usize = 2048;
pub const MAX_FRAME: usize = 1514;

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
    pub link_changes: core::sync::atomic::AtomicU64,
}

pub const MAX_INTERFACES: usize = 4;
pub static ISR: [IsrState; MAX_INTERFACES] = [const {
    IsrState {
        registers: core::sync::atomic::AtomicUsize::new(0),
        icr: core::sync::atomic::AtomicUsize::new(0),
        interrupts: core::sync::atomic::AtomicU64::new(0),
        overruns: core::sync::atomic::AtomicU64::new(0),
        link_changes: core::sync::atomic::AtomicU64::new(0),
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
    tx_next: usize,
    pub frames_received: u64,
    pub frames_sent: u64,
}

#[derive(Debug)]
pub enum Error {
    NoBar,
    OutOfMemory,
    ResetTimeout,
    BadMac,
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
        write(registers, REG_RCTL, RCTL_EN | RCTL_BAM | RCTL_SECRC); // 2 KiB buffers, broadcast + own MAC
        write(registers, REG_CTRL, CTRL_SLU | CTRL_FD);
        if variant == Variant::I350 {
            // 2 KiB receive buffers (SRRCTL counts KiB), then enable both queues explicitly and
            // wait for the receive enable to latch before handing the card any buffers.
            write(registers, r.srrctl, (BUFFER_SIZE / 1024) as u32);
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

        Ok(E1000 { registers, variant, r, index, delivery: Delivery::Polled, mac, rx_ring, tx_ring, rx_buffers, tx_buffers, rx_next: 0, tx_next: 0, frames_received: 0, frames_sent: 0 })
    }

    /// Promiscuous mode: receive every frame on the wire, not only ours and broadcasts
    /// (RCTL.UPE unicast, MPE multicast). A bridge or a link-impairment tape needs it.
    pub fn set_promiscuous(&mut self, on: bool) {
        let control = read(self.registers, REG_RCTL);
        let bits = (1 << 3) | (1 << 4);
        write(self.registers, REG_RCTL, if on { control | bits } else { control & !bits });
    }

    pub fn link_up(&self) -> bool {
        read(self.registers, REG_STATUS) & STATUS_LU != 0
    }

    /// Copies the next received frame into `out`; returns its length, or None if the
    /// ring is empty.
    pub fn receive(&mut self, out: &mut [u8]) -> Option<usize> {
        let descriptor = unsafe { (self.rx_ring as *mut Descriptor).add(self.rx_next) };
        let status = unsafe { read_volatile(core::ptr::addr_of!((*descriptor).status)) };
        if status & DESCRIPTOR_DONE == 0 {
            return None;
        }
        let length = unsafe { read_volatile(core::ptr::addr_of!((*descriptor).length)) } as usize;
        let length = length.min(BUFFER_SIZE).min(out.len());
        let buffer = (self.rx_buffers + (self.rx_next * BUFFER_SIZE) as u64) as *const u8;
        unsafe { core::ptr::copy_nonoverlapping(buffer, out.as_mut_ptr(), length) };
        // Hand the buffer back: clear status, then advance the tail onto it.
        unsafe { write_volatile(core::ptr::addr_of_mut!((*descriptor).status), 0) };
        write(self.registers, self.r.rdt, self.rx_next as u32);
        self.rx_next = (self.rx_next + 1) % RX_COUNT;
        self.frames_received += 1;
        Some(length)
    }

    /// Queues one frame (padded to the 60-byte minimum). Returns false if the ring
    /// stays full, which means the link is down or the card is wedged.
    pub fn send(&mut self, frame: &[u8]) -> bool {
        if frame.len() > MAX_FRAME {
            return false;
        }
        let descriptor = unsafe { (self.tx_ring as *mut Descriptor).add(self.tx_next) };
        let mut waited = 0u32;
        while unsafe { read_volatile(core::ptr::addr_of!((*descriptor).status)) } & DESCRIPTOR_DONE == 0 {
            waited += 1;
            if waited > 2_000_000 {
                return false;
            }
            core::hint::spin_loop();
        }
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

        // Legacy INTx: candidates are the firmware's interrupt line (a hint) and the PCI
        // pins an Intel/QEMU chipset routes to the I/O APIC (GSI 16-23).
        let (first, end) = crate::ioapic::gsi_range();
        let line = u32::from(device.interrupt_line());
        let mut candidates = [u32::MAX; 9];
        candidates[0] = line;
        for (slot, gsi) in (16..24u32).enumerate() {
            candidates[slot + 1] = gsi;
        }
        for (position, &gsi) in candidates.iter().enumerate() {
            if gsi == u32::MAX || gsi == 0 || gsi < first || gsi >= end || candidates[..position].contains(&gsi) {
                continue;
            }
            let Some(vector) = crate::msi::attach_intx(gsi, isr, self.index) else { continue };
            if self.self_test(false, || crate::msi::intx_count(vector)) {
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
