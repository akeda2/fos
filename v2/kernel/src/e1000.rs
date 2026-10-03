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
/// ITX board), 0x15b7 an I219-LM (a Skylake PCH; its K1/ULP power-management workarounds
/// are not implemented, so expect it to need bring-up). They keep the legacy descriptor
/// layout but have a stricter reset sequence (see `reset_e1000e`) and a couple of extra
/// control bits.
const E1000E_IDS: &[u16] = &[0x10d3, 0x1503, 0x15b7];
pub const DEVICE_IDS: &[u16] = &[0x100e, 0x100f, 0x1004, 0x10d3, 0x1503, 0x15b7];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Variant {
    Classic,
    E1000e,
}

fn variant_of(device_id: u16) -> Variant {
    if E1000E_IDS.contains(&device_id) { Variant::E1000e } else { Variant::Classic }
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
    pub interrupts: core::sync::atomic::AtomicU64,
    pub overruns: core::sync::atomic::AtomicU64,
    pub link_changes: core::sync::atomic::AtomicU64,
}

pub const MAX_INTERFACES: usize = 4;
pub static ISR: [IsrState; MAX_INTERFACES] = [const {
    IsrState {
        registers: core::sync::atomic::AtomicUsize::new(0),
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
    let cause = read(registers, REG_ICR);
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
}

pub struct E1000 {
    registers: usize,
    pub variant: Variant,
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
        device.enable_memory_and_bus_master();
        let (base, size) = device.memory_bar(0).ok_or(Error::NoBar)?;
        paging::map_device(base, size.max(0x20000));
        let registers = base as usize;

        let variant = variant_of(device.id);
        write(registers, REG_IMC, u32::MAX); // quiet until `enable_interrupts`
        match variant {
            Variant::Classic => reset_classic(registers)?,
            Variant::E1000e => reset_e1000e(registers)?,
        }
        write(registers, REG_IMC, u32::MAX);

        let ral = read(registers, REG_RAL0);
        let rah = read(registers, REG_RAH0);
        let mut mac = [0u8; 6];
        mac[..4].copy_from_slice(&ral.to_le_bytes());
        mac[4..].copy_from_slice(&(rah as u16).to_le_bytes());
        if mac == [0; 6] || mac == [0xff; 6] {
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

        write(registers, REG_RDBAL, rx_ring as u32);
        write(registers, REG_RDBAH, 0);
        write(registers, REG_RDLEN, (RX_COUNT * 16) as u32);
        write(registers, REG_RDH, 0);
        write(registers, REG_RDT, (RX_COUNT - 1) as u32); // all but one buffer owned by the card
        write(registers, REG_TDBAL, tx_ring as u32);
        write(registers, REG_TDBAH, 0);
        write(registers, REG_TDLEN, (TX_COUNT * 16) as u32);
        write(registers, REG_TDH, 0);
        write(registers, REG_TDT, 0);
        write(registers, REG_TIPG, 0x0060_200a);
        let mut tctl = TCTL_EN | TCTL_PSP | TCTL_CT | TCTL_COLD;
        if variant == Variant::E1000e {
            // Bits Linux sets on this family (the I219 needs RTLC to transmit at all).
            tctl |= TCTL_RTLC | TCTL_UNDOCUMENTED_28_29;
            write(registers, REG_TXDCTL0, TXDCTL_QUEUE_ENABLE);
        }
        write(registers, REG_TCTL, tctl);
        write(registers, REG_RCTL, RCTL_EN | RCTL_BAM | RCTL_SECRC); // 2 KiB buffers, broadcast + own MAC
        write(registers, REG_CTRL, CTRL_SLU | CTRL_FD);

        Ok(E1000 { registers, variant, index, delivery: Delivery::Polled, mac, rx_ring, tx_ring, rx_buffers, tx_buffers, rx_next: 0, tx_next: 0, frames_received: 0, frames_sent: 0 })
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
        write(self.registers, REG_RDT, self.rx_next as u32);
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
        write(self.registers, REG_TDT, self.tx_next as u32);
        self.frames_sent += 1;
        true
    }

    /// Tries to switch this NIC from polling to MSI: claims a vector, programs the PCI
    /// capability, unmasks the causes, and **proves** a message arrives by raising a link-status
    /// interrupt through ICS (the self-test Linux's e1000e does too). If anything fails the NIC
    /// is left polling, exactly as before. Needs interrupts enabled and the local APIC running.
    pub fn enable_msi(&mut self, device: &pci::Device) -> Result<u8, &'static str> {
        use core::sync::atomic::Ordering;
        if device.find_capability(0x05).is_none() {
            return Err("the device has no MSI capability");
        }
        let vector = crate::msi::allocate(isr, self.index).ok_or("no free MSI vector")?;
        ISR[self.index].registers.store(self.registers, Ordering::Relaxed);
        if !device.enable_msi(vector, crate::apic::id()) {
            crate::msi::release(vector);
            return Err("enabling the MSI capability failed");
        }
        let _ = read(self.registers, REG_ICR);
        write(self.registers, REG_ITR, ITR_VALUE);
        write(self.registers, REG_IMS, INTERRUPT_MASK);
        let before = crate::msi::count(vector);
        write(self.registers, REG_ICS, ICR_LSC); // raise one cause by hand
        let start = crate::apic::ticks();
        while crate::msi::count(vector) == before && crate::apic::ticks().wrapping_sub(start) < 3 {
            core::hint::spin_loop();
        }
        if crate::msi::count(vector) == before {
            write(self.registers, REG_IMC, u32::MAX);
            device.disable_msi();
            crate::msi::release(vector);
            ISR[self.index].registers.store(0, Ordering::Relaxed);
            return Err("the self-test message never arrived");
        }
        self.delivery = Delivery::Msi(vector);
        Ok(vector)
    }
}
