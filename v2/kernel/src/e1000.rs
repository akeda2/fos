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
/// 0x100e is the 82540EM; 0x100f and 0x1004 are the same programming model.
pub const DEVICE_IDS: &[u16] = &[0x100e, 0x100f, 0x1004];

const REG_CTRL: usize = 0x0000;
const REG_IMC: usize = 0x00d8;
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

const DESCRIPTOR_DONE: u8 = 1;
const TX_EOP_IFCS_RS: u8 = (1 << 0) | (1 << 1) | (1 << 3);

const RX_COUNT: usize = 64;
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

pub struct E1000 {
    registers: usize,
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

impl E1000 {
    pub fn init(device: &pci::Device) -> Result<E1000, Error> {
        device.enable_memory_and_bus_master();
        let (base, size) = device.memory_bar(0).ok_or(Error::NoBar)?;
        paging::map_device(base, size.max(0x20000));
        let registers = base as usize;

        write(registers, REG_IMC, u32::MAX); // no interrupts: this driver polls
        write(registers, REG_CTRL, CTRL_RST);
        let mut waited = 0;
        while read(registers, REG_CTRL) & CTRL_RST != 0 {
            waited += 1;
            if waited > 1_000_000 {
                return Err(Error::ResetTimeout);
            }
            core::hint::spin_loop();
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
        write(registers, REG_TCTL, TCTL_EN | TCTL_PSP | TCTL_CT | TCTL_COLD);
        write(registers, REG_RCTL, RCTL_EN | RCTL_BAM | RCTL_SECRC); // 2 KiB buffers, broadcast + own MAC
        write(registers, REG_CTRL, CTRL_SLU | CTRL_FD);

        Ok(E1000 { registers, mac, rx_ring, tx_ring, rx_buffers, tx_buffers, rx_next: 0, tx_next: 0, frames_received: 0, frames_sent: 0 })
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
}
