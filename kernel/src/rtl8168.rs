//! A polling driver for Realtek RTL8168/RTL8111 gigabit NICs (PCI `10ec:8168` and `10ec:8161`),
//! ported from v1's `rtl8168.rs`, which was confirmed on two real chips (a TP-Link TG-3468 and an
//! onboard LAN). The chip is driven through its I/O-port BAR 0.
//!
//! How the chip works, briefly. Frames move through two rings of 16-byte descriptors in host
//! RAM, one for receive and one for transmit. A descriptor's first dword (`opts1`) carries the
//! OWN bit (1 = the card owns it), the end-of-ring bit, first/last-segment bits and the buffer
//! length; the other dwords hold the buffer's address. Software hands a descriptor to the card
//! by setting OWN last; the card clears OWN when it is done with it (a received frame is in the
//! buffer, or the frame went out). Transmit is started by a write to `TPPOLL`.
//!
//! The one chip-specific branch is the order of the receiver/transmitter enable: chips from
//! the RTL8168G on (`TxConfig` version bits 0x4c0 and up) want `CR.TE|RE` set *after*
//! `TxConfig`/`RxConfig`, older ones before. Linux's PHY, ERI, OCP and ASPM tuning is left out,
//! as in v1; the v1 notes record that this was enough for real gigabit links.
//!
//! Interrupts: the foreground always polls the rings; MSI (when the chip and chipset pass a
//! self-test, `enable_interrupts`) only wakes the CPU from `hlt` when a frame arrives, as on the
//! e1000 family. v1's notes record that the receive-descriptor-unavailable and FIFO-overflow
//! causes must be handled softly (never answered with a reset, or a gigabit stream resets the
//! card constantly): the handler masks them until `receive` has drained the ring. Only a PCI
//! system error, or a transmit descriptor the card never gives back, resets the chip.
//! The RTL8168G/H ignore the "forced software interrupt" bit the self-test raises, so the test
//! also enables the transmit-OK cause and sends one broadcast frame, whose completion interrupts.

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, AtomicU16, Ordering, fence};

use crate::e1000::{Delivery, ISR, MAX_INTERFACES};
use crate::{apic, cpu, memory, pci};

pub const VENDOR_REALTEK: u16 = 0x10ec;
pub const DEVICE_IDS: &[u16] = &[0x8168, 0x8161];

pub const MAX_FRAME: usize = 1514; // excluding the 4-byte FCS the card appends
const MIN_FRAME: usize = 60;

const REG_IDR0: u16 = 0x00; // the MAC address, 6 bytes
const REG_MAR0: u16 = 0x08; // multicast hash filter, 8 bytes
const REG_TNPDS: u16 = 0x20; // transmit (normal priority) descriptor ring, 64-bit
const REG_CR: u16 = 0x37;
const REG_TPPOLL: u16 = 0x38;
const REG_IMR: u16 = 0x3c;
const REG_ISR: u16 = 0x3e;
const REG_TXCONFIG: u16 = 0x40;
const REG_RXCONFIG: u16 = 0x44;
const REG_9346CR: u16 = 0x50;
const REG_PHYSTATUS: u16 = 0x6c;
const REG_RMS: u16 = 0xda;
const REG_CPLUSCMD: u16 = 0xe0;
const REG_RDSAR: u16 = 0xe4; // receive descriptor ring, 64-bit
const REG_MAXTXPKT: u16 = 0xec;

const CR_RST: u8 = 0x10;
const CR_RE: u8 = 0x08;
const CR_TE: u8 = 0x04;
const TPPOLL_NPQ: u8 = 0x40;
const CFG9346_UNLOCK: u8 = 0xc0;
const CFG9346_LOCK: u8 = 0x00;
const ISR_ROK: u16 = 0x0001;
const ISR_RER: u16 = 0x0002;
const ISR_TOK: u16 = 0x0004; // transmit OK (enabled only during the self-test)
const ISR_RDU: u16 = 0x0010; // receive descriptor unavailable
const ISR_LINKCHG: u16 = 0x0020;
const ISR_FOVW: u16 = 0x0040; // receive FIFO overflow
const ISR_SWINT: u16 = 0x0100;
const ISR_SYSERR: u16 = 0x8000;
const INTERRUPT_MASK: u16 = ISR_ROK | ISR_RER | ISR_RDU | ISR_LINKCHG | ISR_FOVW | ISR_SYSERR | ISR_SWINT;
const SOFT_MASK: u16 = ISR_RDU | ISR_FOVW;
const TPPOLL_FSWINT: u8 = 0x01;

/// Per-interface state the interrupt handler needs (it gets only the interface number).
static IO_BASE: [AtomicU16; MAX_INTERFACES] = [const { AtomicU16::new(0) }; MAX_INTERFACES];
static SOFT_MASKED: [AtomicBool; MAX_INTERFACES] = [const { AtomicBool::new(false) }; MAX_INTERFACES];
static FATAL: [AtomicBool; MAX_INTERFACES] = [const { AtomicBool::new(false) }; MAX_INTERFACES];

/// The MSI handler: acknowledge the causes (write 1 to clear), count them, and mask what must not
/// storm. Descriptor work stays in the foreground.
fn isr(index: usize) {
    let base = IO_BASE[index].load(Ordering::Relaxed);
    if base == 0 {
        return;
    }
    let status = read16(base, REG_ISR) & (INTERRUPT_MASK | ISR_TOK);
    if status == 0 {
        return;
    }
    write16(base, REG_ISR, status);
    ISR[index].interrupts.fetch_add(1, Ordering::Relaxed);
    if status & ISR_TOK != 0 {
        write16(base, REG_IMR, INTERRUPT_MASK); // the self-test's extra cause is no longer wanted
    }
    if status & ISR_LINKCHG != 0 {
        ISR[index].link_changes.fetch_add(1, Ordering::Relaxed);
    }
    if status & SOFT_MASK != 0 {
        ISR[index].overruns.fetch_add(1, Ordering::Relaxed);
        SOFT_MASKED[index].store(true, Ordering::Release);
        write16(base, REG_IMR, INTERRUPT_MASK & !SOFT_MASK);
    }
    if status & ISR_SYSERR != 0 {
        FATAL[index].store(true, Ordering::Release);
        write16(base, REG_IMR, 0);
    }
}

const TXCONFIG_VALUE: u32 = (3 << 24) | (7 << 8) | (1 << 7); // standard IFG, max DMA burst
const RX_ACCEPT_ALL_PHYS: u32 = 0x01;
const RX_ACCEPT_MY_PHYS: u32 = 0x02;
const RX_ACCEPT_MULTICAST: u32 = 0x04;
const RX_ACCEPT_BROADCAST: u32 = 0x08;
const RXCONFIG_VALUE: u32 = (7 << 13) | (7 << 8) | RX_ACCEPT_MY_PHYS | RX_ACCEPT_MULTICAST | RX_ACCEPT_BROADCAST;
const CPLUS_RX_CHECKSUM: u16 = 1 << 5;
const CPLUS_RX_VLAN: u16 = 1 << 6;
const XID_G_AND_LATER: u16 = 0x4c0;
const PHYSTATUS_LINK: u8 = 0x02;

const DESC_OWN: u32 = 1 << 31;
const DESC_EOR: u32 = 1 << 30;
const DESC_FS: u32 = 1 << 29;
const DESC_LS: u32 = 1 << 28;
const DESC_RX_RES: u32 = 1 << 21; // receive error summary
const DESC_LENGTH_MASK: u32 = 0x3fff;

const DESCRIPTOR_BYTES: usize = 16;
/// v1's lesson: with only 8-16 descriptors a tape forwarding a gigabit stream overran constantly.
const RX_COUNT: usize = 64;
const TX_COUNT: usize = 32;
const BUFFER_BYTES: usize = 2048;
const TX_RING_OFFSET: u64 = (RX_COUNT * DESCRIPTOR_BYTES) as u64;
const RX_BUFFER_FRAMES: u64 = (RX_COUNT * BUFFER_BYTES / 4096) as u64;
const TX_BUFFER_FRAMES: u64 = (TX_COUNT * BUFFER_BYTES / 4096) as u64;

#[derive(Debug)]
pub enum Error {
    NoIoBar,
    NotResponding,
    ResetTimedOut,
    OutOfMemory,
}

fn read8(base: u16, register: u16) -> u8 {
    cpu::inb(base + register)
}
fn write8(base: u16, register: u16, value: u8) {
    cpu::outb(base + register, value)
}
fn read16(base: u16, register: u16) -> u16 {
    cpu::inw(base + register)
}
fn write16(base: u16, register: u16, value: u16) {
    cpu::outw(base + register, value)
}
fn read32(base: u16, register: u16) -> u32 {
    cpu::inl(base + register)
}
fn write32(base: u16, register: u16, value: u32) {
    cpu::outl(base + register, value)
}

fn field(ring: u64, index: usize, dword: usize) -> *mut u32 {
    (ring as usize + index * DESCRIPTOR_BYTES + dword * 4) as *mut u32
}

fn read_opts1(ring: u64, index: usize) -> u32 {
    unsafe { read_volatile(field(ring, index, 0)) }
}

/// Fills a descriptor and hands it over: address first, OWN (in `opts1`) last behind a fence.
fn write_descriptor(ring: u64, index: usize, opts1: u32, address: u64) {
    unsafe {
        write_volatile(field(ring, index, 1), 0); // VLAN
        write_volatile(field(ring, index, 2), address as u32);
        write_volatile(field(ring, index, 3), (address >> 32) as u32);
    }
    fence(Ordering::SeqCst);
    unsafe { write_volatile(field(ring, index, 0), opts1) };
}

fn rx_opts1(index: usize) -> u32 {
    DESC_OWN | if index == RX_COUNT - 1 { DESC_EOR } else { 0 } | BUFFER_BYTES as u32
}

pub struct Rtl8168 {
    index: usize,
    pub delivery: Delivery,
    interrupts_enabled: bool,
    io_base: u16,
    rings: u64,
    rx_buffers: u64,
    tx_buffers: u64,
    rx_next: usize,
    tx_next: usize,
    enable_after_config: bool,
    promiscuous: bool,
    pub mac: [u8; 6],
    pub frames_received: u64,
    pub frames_sent: u64,
    pub frames_dropped_no_link: u64,
    pub recoveries: u32,
    recovery_pending: bool,
    /// The chip version bits of `TxConfig` (0x4c0 and up: RTL8168G or later).
    pub version: u16,
}

fn wait_reset(base: u16) -> bool {
    write8(base, REG_CR, CR_RST);
    for _ in 0..1_000_000u32 {
        if read8(base, REG_CR) & CR_RST == 0 {
            apic::wait_ticks(1);
            return true;
        }
        core::hint::spin_loop();
    }
    false
}

impl Rtl8168 {
    pub fn init(device: &pci::Device, index: usize) -> Result<Rtl8168, Error> {
        device.force_power_state_d0();
        // Memory decoding is not needed (the I/O BAR is used) but I/O decoding and bus mastering are.
        let command = device.read32(4);
        device.write32(4, (command & 0xffff) | 0x5);
        let io_base = device.io_bar(0).ok_or(Error::NoIoBar)?;
        let tx_config = read32(io_base, REG_TXCONFIG);
        if tx_config == 0xffff_ffff {
            return Err(Error::NotResponding);
        }
        let version = ((tx_config >> 20) & 0xfcf) as u16;
        let enable_after_config = version >= XID_G_AND_LATER;
        if !wait_reset(io_base) {
            return Err(Error::ResetTimedOut);
        }
        let below = Some(1u64 << 32); // descriptors carry 64-bit addresses, but low memory is simplest
        let rings = memory::allocate_contiguous(1, below).ok_or(Error::OutOfMemory)?;
        let rx_buffers = memory::allocate_contiguous(RX_BUFFER_FRAMES, below).ok_or(Error::OutOfMemory)?;
        let tx_buffers = memory::allocate_contiguous(TX_BUFFER_FRAMES, below).ok_or(Error::OutOfMemory)?;
        init_rings(rings, rx_buffers, tx_buffers);
        program(io_base, rings, enable_after_config, false);
        let mut mac = [0u8; 6];
        for (index, byte) in mac.iter_mut().enumerate() {
            *byte = read8(io_base, REG_IDR0 + index as u16);
        }
        IO_BASE[index].store(io_base, Ordering::Relaxed);
        Ok(Rtl8168 {
            index,
            delivery: Delivery::Polled,
            interrupts_enabled: false,
            io_base,
            rings,
            rx_buffers,
            tx_buffers,
            rx_next: 0,
            tx_next: 0,
            enable_after_config,
            promiscuous: false,
            mac,
            frames_received: 0,
            frames_sent: 0,
            frames_dropped_no_link: 0,
            recoveries: 0,
            recovery_pending: false,
            version,
        })
    }

    /// Raises one interrupt by hand and reports whether `arrived` saw it within three ticks: the
    /// proof that the chosen delivery path really works. Older chips raise a software interrupt
    /// when `TPPOLL.FSWINT` is written; the RTL8168G/H ignore that bit, so one broadcast frame is
    /// sent too, whose completion (transmit OK, enabled just for this test) interrupts instead.
    fn self_test(&mut self, arrived: impl Fn() -> u64) -> bool {
        write16(self.io_base, REG_ISR, 0xffff);
        write16(self.io_base, REG_IMR, INTERRUPT_MASK | ISR_TOK);
        let before = arrived();
        write8(self.io_base, REG_TPPOLL, TPPOLL_FSWINT);
        let mut frame = [0u8; 14];
        frame[..6].fill(0xff);
        frame[6..12].copy_from_slice(&self.mac);
        frame[12..14].copy_from_slice(&[0x88, 0xb5]);
        self.send(&frame);
        let start = apic::ticks();
        while arrived() == before && apic::ticks().wrapping_sub(start) < 3 {
            core::hint::spin_loop();
        }
        arrived() != before
    }

    fn finish_interrupts(&mut self, delivery: Delivery) -> Delivery {
        self.interrupts_enabled = true;
        self.delivery = delivery;
        write16(self.io_base, REG_IMR, INTERRUPT_MASK);
        delivery
    }

    /// Switches this NIC from polling to interrupts, trying MSI and then legacy INTx through the
    /// I/O APIC (probing candidate pins, as the e1000 driver does, since the ACPI routing table
    /// is not read). A path is kept only if its self-test interrupt arrives; otherwise the NIC
    /// stays polled and says why. Needs interrupts enabled and the local APIC running.
    pub fn enable_interrupts(&mut self, device: &pci::Device) -> Result<Delivery, &'static str> {
        let mut reason = "no interrupt path passed its self-test";
        if device.find_capability(0x05).is_some() {
            match crate::msi::allocate(isr, self.index) {
                Some(vector) if device.enable_msi(vector, apic::id()) => {
                    if self.self_test(|| crate::msi::count(vector)) {
                        return Ok(self.finish_interrupts(Delivery::Msi(vector)));
                    }
                    write16(self.io_base, REG_IMR, 0);
                    device.disable_msi();
                    crate::msi::release(vector);
                    reason = "the MSI self-test message never arrived";
                }
                Some(vector) => crate::msi::release(vector),
                None => reason = "no free MSI vector",
            }
        } else {
            reason = "no MSI capability";
        }
        // Legacy INTx: the firmware's interrupt line (a hint) and the PCI pins an Intel/QEMU
        // chipset routes to the I/O APIC (GSI 16-23).
        let (first, end) = crate::ioapic::gsi_range();
        let mut candidates = [u32::MAX; 9];
        candidates[0] = u32::from(device.interrupt_line());
        for (slot, gsi) in (16..24u32).enumerate() {
            candidates[slot + 1] = gsi;
        }
        for (position, &gsi) in candidates.iter().enumerate() {
            if gsi == u32::MAX || gsi == 0 || gsi < first || gsi >= end || candidates[..position].contains(&gsi) {
                continue;
            }
            let Some(vector) = crate::msi::attach_intx(gsi, isr, self.index) else { continue };
            if self.self_test(|| crate::msi::intx_count(vector)) {
                return Ok(self.finish_interrupts(Delivery::Intx { gsi, vector }));
            }
            write16(self.io_base, REG_IMR, 0);
            crate::msi::detach_intx(vector, self.index);
            reason = "no I/O APIC pin delivered the self-test interrupt";
        }
        Err(reason)
    }

    /// The `nic` command's driver-specific line.
    pub fn describe(&self) {
        match self.link_speed() {
            Some((speed, full)) => crate::println!(
                "   rtl8168: chip version {:#05x}, {speed} Mb/s {}, {} recoveries",
                self.version,
                if full { "full duplex" } else { "half duplex" },
                self.recoveries
            ),
            None => crate::println!("   rtl8168: chip version {:#05x}, no link, {} recoveries", self.version, self.recoveries),
        }
    }

    /// `nic regs`: the registers that matter when a link or interrupt misbehaves, and the ring
    /// state (which descriptors the card still owns).
    pub fn dump_registers(&self) {
        let io = self.io_base;
        crate::println!(
            "RTL8168 I/O {io:#06x}: CR={:02x} IMR={:04x} ISR={:04x} PHY={:02x} CPlus={:04x}",
            read8(io, REG_CR),
            read16(io, REG_IMR),
            read16(io, REG_ISR),
            read8(io, REG_PHYSTATUS),
            read16(io, REG_CPLUSCMD)
        );
        crate::println!("TxConfig={:08x} RxConfig={:08x} RMS={:04x}", read32(io, REG_TXCONFIG), read32(io, REG_RXCONFIG), read16(io, REG_RMS));
        let (mut rx_card, mut tx_card) = (0u64, 0u64);
        for index in 0..RX_COUNT {
            if read_opts1(self.rings, index) & DESC_OWN != 0 {
                rx_card |= 1 << index;
            }
        }
        for index in 0..TX_COUNT {
            if read_opts1(self.rings + TX_RING_OFFSET, index) & DESC_OWN != 0 {
                tx_card |= 1 << index;
            }
        }
        crate::println!("rx next {} (card owns {rx_card:016x}), tx next {} (card owns {tx_card:08x})", self.rx_next, self.tx_next);
        crate::println!(
            "rx[next] opts1 {:08x}; recoveries {}, pending {}, interrupts {:?}",
            read_opts1(self.rings, self.rx_next),
            self.recoveries,
            self.recovery_pending,
            self.delivery
        );
    }

    pub fn io_base(&self) -> u16 {
        self.io_base
    }

    pub fn link_up(&self) -> bool {
        read8(self.io_base, REG_PHYSTATUS) & PHYSTATUS_LINK != 0
    }

    /// Link speed in Mb/s and whether it is full duplex, from the PHY status register.
    pub fn link_speed(&self) -> Option<(u16, bool)> {
        let status = read8(self.io_base, REG_PHYSTATUS);
        if status & PHYSTATUS_LINK == 0 {
            return None;
        }
        let speed = if status & 0x10 != 0 { 1000 } else if status & 0x08 != 0 { 100 } else if status & 0x04 != 0 { 10 } else { return None };
        Some((speed, status & 0x01 != 0))
    }

    pub fn set_promiscuous(&mut self, on: bool) {
        let config = read32(self.io_base, REG_RXCONFIG);
        write32(self.io_base, REG_RXCONFIG, if on { config | RX_ACCEPT_ALL_PHYS } else { config & !RX_ACCEPT_ALL_PHYS });
        self.promiscuous = on;
    }

    /// Resets the chip and reprograms it from scratch (the same recovery v1 used after a stuck
    /// transmit or a PCI system error).
    fn recover(&mut self) -> bool {
        if !wait_reset(self.io_base) {
            return false;
        }
        init_rings(self.rings, self.rx_buffers, self.tx_buffers);
        self.rx_next = 0;
        self.tx_next = 0;
        program(self.io_base, self.rings, self.enable_after_config, self.promiscuous);
        FATAL[self.index].store(false, Ordering::Relaxed);
        SOFT_MASKED[self.index].store(false, Ordering::Relaxed);
        if self.interrupts_enabled {
            write16(self.io_base, REG_ISR, 0xffff);
            write16(self.io_base, REG_IMR, INTERRUPT_MASK);
        }
        self.recoveries += 1;
        self.recovery_pending = false;
        true
    }

    /// Queues one frame (padded to the 60-byte minimum). A frame sent with no link is dropped, as
    /// on the e1000 driver, instead of waiting for a descriptor that would never complete.
    pub fn send(&mut self, frame: &[u8]) -> bool {
        if frame.len() > MAX_FRAME {
            return false;
        }
        if !self.link_up() {
            self.frames_dropped_no_link += 1;
            return true;
        }
        let ring = self.rings + TX_RING_OFFSET;
        let index = self.tx_next;
        for _ in 0..1_000_000u32 {
            if read_opts1(ring, index) & DESC_OWN == 0 {
                break;
            }
            core::hint::spin_loop();
        }
        if read_opts1(ring, index) & DESC_OWN != 0 {
            self.recovery_pending = true; // the card never gave the descriptor back
            return false;
        }
        self.tx_next = (index + 1) % TX_COUNT;
        let buffer = self.tx_buffers + (index * BUFFER_BYTES) as u64;
        let destination = unsafe { core::slice::from_raw_parts_mut(buffer as *mut u8, BUFFER_BYTES) };
        destination[..frame.len()].copy_from_slice(frame);
        let length = frame.len().max(MIN_FRAME);
        destination[frame.len()..length].fill(0);
        let eor = if index == TX_COUNT - 1 { DESC_EOR } else { 0 };
        write_descriptor(ring, index, DESC_OWN | eor | DESC_FS | DESC_LS | length as u32, buffer);
        write8(self.io_base, REG_TPPOLL, TPPOLL_NPQ); // doorbell: look at the normal-priority queue
        self.frames_sent += 1;
        true
    }

    /// Copies the next received frame (without the FCS) into `out`; `None` if the ring is empty.
    pub fn receive(&mut self, out: &mut [u8]) -> Option<usize> {
        // With interrupts the handler records a system error; polled, look at the status register.
        let fatal = FATAL[self.index].swap(false, Ordering::AcqRel)
            || (!self.interrupts_enabled && read16(self.io_base, REG_ISR) & ISR_SYSERR != 0);
        if self.recovery_pending || fatal {
            self.recover();
            write16(self.io_base, REG_ISR, 0xffff);
        }
        let index = self.rx_next;
        let opts1 = read_opts1(self.rings, index);
        if opts1 & DESC_OWN != 0 {
            // The ring is drained: the overflow causes the handler masked may fire again.
            if SOFT_MASKED[self.index].swap(false, Ordering::AcqRel) {
                write16(self.io_base, REG_ISR, SOFT_MASK);
                write16(self.io_base, REG_IMR, INTERRUPT_MASK);
            }
            return None;
        }
        fence(Ordering::SeqCst);
        let length = (opts1 & DESC_LENGTH_MASK) as usize; // includes the 4-byte FCS
        let whole = opts1 & (DESC_FS | DESC_LS) == (DESC_FS | DESC_LS);
        let good = whole && opts1 & DESC_RX_RES == 0 && (14 + 4..=BUFFER_BYTES).contains(&length);
        let buffer = self.rx_buffers + (index * BUFFER_BYTES) as u64;
        let copied = good.then(|| {
            let count = (length - 4).min(out.len());
            unsafe { core::ptr::copy_nonoverlapping(buffer as *const u8, out.as_mut_ptr(), count) };
            count
        });
        // Hand the descriptor back whether or not the frame was good.
        write_descriptor(self.rings, index, rx_opts1(index), buffer);
        self.rx_next = (index + 1) % RX_COUNT;
        if copied.is_some() {
            self.frames_received += 1;
        }
        // A bad frame still consumed a slot: report "nothing" and let the caller poll again.
        copied
    }
}

fn init_rings(rings: u64, rx_buffers: u64, tx_buffers: u64) {
    for index in 0..RX_COUNT {
        write_descriptor(rings, index, rx_opts1(index), rx_buffers + (index * BUFFER_BYTES) as u64);
    }
    for index in 0..TX_COUNT {
        let eor = if index == TX_COUNT - 1 { DESC_EOR } else { 0 };
        write_descriptor(rings + TX_RING_OFFSET, index, eor, tx_buffers + (index * BUFFER_BYTES) as u64);
    }
}

/// Programs the registers after a reset. Config registers are write-protected unless `9346CR`
/// says otherwise; the ring base addresses and the receiver/transmitter enables follow the
/// chip-generation order described at the top of the file.
fn program(base: u16, rings: u64, enable_after_config: bool, promiscuous: bool) {
    write8(base, REG_9346CR, CFG9346_UNLOCK);
    let cplus = read16(base, REG_CPLUSCMD);
    write16(base, REG_CPLUSCMD, cplus & !(CPLUS_RX_CHECKSUM | CPLUS_RX_VLAN)); // no offloads, no VLAN stripping
    write16(base, REG_RMS, (BUFFER_BYTES - 1) as u16);
    write8(base, REG_MAXTXPKT, 0x3f);
    write32(base, REG_TNPDS + 4, ((rings + TX_RING_OFFSET) >> 32) as u32);
    write32(base, REG_TNPDS, (rings + TX_RING_OFFSET) as u32);
    write32(base, REG_RDSAR + 4, (rings >> 32) as u32);
    write32(base, REG_RDSAR, rings as u32);
    if !enable_after_config {
        write8(base, REG_CR, CR_TE | CR_RE);
    }
    write32(base, REG_TXCONFIG, TXCONFIG_VALUE);
    write32(base, REG_RXCONFIG, RXCONFIG_VALUE | if promiscuous { RX_ACCEPT_ALL_PHYS } else { 0 });
    write32(base, REG_MAR0, 0xffff_ffff);
    write32(base, REG_MAR0 + 4, 0xffff_ffff);
    if enable_after_config {
        write8(base, REG_CR, CR_TE | CR_RE);
    }
    write8(base, REG_9346CR, CFG9346_LOCK);
    write16(base, REG_ISR, 0xffff); // clear anything latched before the reset
    write16(base, REG_IMR, 0); // polling: no interrupt causes enabled
}
