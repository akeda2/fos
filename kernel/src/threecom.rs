//! Minimal polling driver for 3Com 3C905/B/C PCI Ethernet controllers.
//!
//! These cards use the Cyclone/Tornado bus-master interface: receive and
//! transmit descriptors live in physical memory, while the device is
//! controlled through a windowed command register at BAR0/BAR1. This driver
//! intentionally implements only the raw Ethernet operations the kernel
//! already exposes to tapes. It does not add interrupts, link management,
//! checksum offload, or a general descriptor allocator.

use core::cell::Cell;
use core::ptr::{read_volatile, write_volatile};

use crate::arch::x86::{inl, inw, outb, outl, outw};
use crate::pci;
use crate::pmem;

const VENDOR_3COM: u16 = 0x10b7;
const DEVICE_IDS: &[u16] = &[0x9050, 0x9051, 0x9054, 0x9055, 0x9058, 0x905a, 0x9200];

const CMD_STATUS: u16 = 0x0e;
const EEPROM_COMMAND: u16 = 0x0a;
const EEPROM_DATA: u16 = 0x0c;
const PKT_STATUS: u16 = 0x20;
const DOWN_LIST_PTR: u16 = 0x24;
const UP_LIST_PTR: u16 = 0x38;
const INTERNAL_CONFIG: u16 = 0x00;
const MAX_PACKET_SIZE: u16 = 0x04;
const MEDIA_OPTIONS: u16 = 0x08;
const MEDIA_STATUS: u16 = 0x0a;

const TOTAL_RESET: u16 = 0 << 11;
const RESET_NETWORK: u16 = 1 << 2;
const SELECT_WINDOW: u16 = 1 << 11;
const RX_ENABLE: u16 = 4 << 11;
const RX_RESET: u16 = 5 << 11;
const UP_UNSTALL: u16 = (6 << 11) + 1;
const TX_ENABLE: u16 = 9 << 11;
const TX_RESET: u16 = 11 << 11;
const ACK_INTERRUPT: u16 = 13 << 11;
const SET_STATUS_ENABLE: u16 = 15 << 11;
const SET_RX_FILTER: u16 = 16 << 11;
const SET_RX_THRESHOLD: u16 = 17 << 11;
const DOWN_UNSTALL: u16 = (6 << 11) + 3;
const SET_INTERRUPT_ENABLE: u16 = 14 << 11;

const RX_STATION: u16 = 1;
const RX_BROADCAST: u16 = 4;
const RX_PROMISCUOUS: u16 = 8;

const LINK_10BASE_T: u32 = 0;
const LINK_AUI: u32 = 1;
const LINK_10BASE_2: u32 = 3;
const LINK_100BASE_FX: u32 = 5;
const LINK_MII: u32 = 6;
const LINK_AUTONEGOTIATE: u32 = 8;
const MEDIA_LINK_BEAT: u16 = 1 << 11;

const RX_COMPLETE: u32 = 1 << 15;
const RX_ERROR: u32 = 1 << 14;
const LAST_FRAGMENT: u32 = 1 << 31;
const TX_INDICATE: u32 = 1 << 15;
const DOWNLOAD_COMPLETE: u32 = 1 << 16;
const TX_ROUNDUP_DEFEAT: u32 = 1 << 28;
const EEPROM_READ: u16 = 0x80;
const EEPROM_BUSY: u16 = 0x8000;

const RX_DESC_COUNT: usize = 4;
const RX_BUFFER_SIZE: usize = 1536;
pub const MIN_FRAME_LEN: usize = 60;
pub const MAX_FRAME_LEN: usize = 1514;

#[repr(C)]
#[derive(Clone, Copy)]
struct RxDescriptor {
    next: u32,
    status: u32,
    address: u32,
    length: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct TxDescriptor {
    next: u32,
    status: u32,
    address: u32,
    length: u32,
}

fn read16(base: u16, offset: u16) -> u16 {
    unsafe { inw(base + offset) }
}

fn write16(base: u16, offset: u16, value: u16) {
    unsafe { outw(base + offset, value) }
}

fn write32(base: u16, offset: u16, value: u32) {
    unsafe { outl(base + offset, value) }
}

fn read32(base: u16, offset: u16) -> u32 {
    unsafe { inl(base + offset) }
}

fn wait_command(base: u16) -> Result<(), InitError> {
    let mut attempts = 100_000u32;
    while read16(base, CMD_STATUS) & 0x1000 != 0 {
        attempts -= 1;
        if attempts == 0 {
            return Err(InitError::CommandTimedOut);
        }
    }
    Ok(())
}

fn descriptor_ptr<T>(base: u32, index: usize) -> *mut T {
    (base as usize + index * 16) as *mut T
}

pub struct ThreeCom {
    io_base: u16,
    b_revision: bool,
    mac: [u8; 6],
    descriptors: u32,
    rx_buffers: [u32; RX_DESC_COUNT],
    tx_buffer: u32,
    rx_next: Cell<usize>,
    overflow_count: Cell<u32>,
}

struct ProbeInfo {
    io_base: u16,
    b_revision: bool,
}

#[derive(Debug)]
pub enum InitError {
    OutOfMemory,
    ResetTimedOut,
    CommandTimedOut,
    EepromTimedOut,
    InvalidMac,
}

impl ThreeCom {
    /// Finds every supported 3C905/B/C function with a usable I/O BAR.
    /// Cyclone and Tornado cards commonly expose the register window at
    /// BAR1, while some BIOSes leave a usable BAR0.
    fn probe_all(out: &mut [ProbeInfo]) -> usize {
        let mut addresses = [None; pci::MAX_DEVICE_MATCHES];
        let found = pci::find_devices(VENDOR_3COM, DEVICE_IDS, &mut addresses);
        let mut count = 0;
        for address in addresses[..found.min(addresses.len())].iter().flatten() {
            address.enable_io_and_bus_master();
            if let Some(io_base) = address.io_bar(1).or_else(|| address.io_bar(0)) {
                if count == out.len() {
                    break;
                }
                out[count] = ProbeInfo {
                    io_base,
                    b_revision: matches!(
                        address.device_id(),
                        0x9054 | 0x9055 | 0x9058 | 0x905a | 0x9200
                    ),
                };
                count += 1;
            }
        }
        count
    }

    fn init(io_base: u16, b_revision: bool) -> Result<Self, InitError> {
        Self::reset(io_base)?;
        let descriptors = pmem::allocate_contiguous(1).ok_or(InitError::OutOfMemory)?;
        let mut rx_buffers = [0u32; RX_DESC_COUNT];
        for buffer in &mut rx_buffers {
            *buffer = pmem::allocate_contiguous(1).ok_or(InitError::OutOfMemory)?;
        }
        let tx_buffer = pmem::allocate_contiguous(1).ok_or(InitError::OutOfMemory)?;
        let mut nic = Self {
            io_base,
            b_revision,
            mac: [0; 6],
            descriptors,
            rx_buffers,
            tx_buffer,
            rx_next: Cell::new(0),
            overflow_count: Cell::new(0),
        };
        nic.configure()?;
        Ok(nic)
    }

    fn reset(io_base: u16) -> Result<(), InitError> {
        write16(io_base, CMD_STATUS, TOTAL_RESET | RESET_NETWORK);
        wait_command(io_base).map_err(|error| match error {
            InitError::CommandTimedOut => InitError::ResetTimedOut,
            other => other,
        })
    }

    fn command(&self, command: u16) -> Result<(), InitError> {
        write16(self.io_base, CMD_STATUS, command);
        wait_command(self.io_base)
    }

    fn select_window(&self, window: u16) {
        write16(self.io_base, CMD_STATUS, SELECT_WINDOW | window);
    }

    fn configure_media(&self) -> Result<(), InitError> {
        self.select_window(3);
        let mut options = read16(self.io_base, MEDIA_OPTIONS);
        if !self.b_revision {
            options &= 0x7f;
        }
        let link_type = if options & 0x0a == 0x0a
            || options & 0x0a == 0x02
            || options & 0x0a == 0x08
        {
            LINK_AUTONEGOTIATE
        } else if options & 0x40 != 0 {
            LINK_MII
        } else if options & 0x04 != 0 {
            LINK_100BASE_FX
        } else if options & 0x10 != 0 {
            LINK_10BASE_2
        } else if options & 0x20 != 0 {
            LINK_AUI
        } else {
            LINK_10BASE_T
        };
        let link_type = if self.b_revision {
            let user_config = self.read_eeprom(0x16)?;
            if user_config & 0xff00 == 0x5a00 {
                u32::from(user_config & 0x000f)
            } else {
                link_type
            }
        } else {
            link_type
        };
        let config =
            (read32(self.io_base, INTERNAL_CONFIG) & !(0x0f << 20)) | (link_type << 20);
        write32(self.io_base, INTERNAL_CONFIG, config);
        Ok(())
    }

    fn configure(&mut self) -> Result<(), InitError> {
        self.select_window(0);
        let mut mac = [0u8; 6];
        for (index, byte) in mac.chunks_exact_mut(2).enumerate() {
            let value = self.read_eeprom(10 + index as u16)?;
            byte.copy_from_slice(&value.to_be_bytes());
        }
        if mac.iter().all(|&byte| byte == 0) || mac == [0xff; 6] {
            return Err(InitError::InvalidMac);
        }
        self.mac = mac;

        self.select_window(2);
        for (index, byte) in mac.iter().enumerate() {
            unsafe { outb(self.io_base + index as u16, *byte) };
        }
        for offset in (6..12).step_by(2) {
            write16(self.io_base, offset, 0);
        }
        self.configure_media()?;

        let rx_desc = self.descriptors as usize as *mut RxDescriptor;
        for index in 0..RX_DESC_COUNT {
            let next = if index + 1 == RX_DESC_COUNT {
                self.descriptors
            } else {
                self.descriptors + ((index + 1) * 16) as u32
            };
            unsafe {
                write_volatile(
                    rx_desc.add(index),
                    RxDescriptor {
                        next,
                        status: 0,
                        address: self.rx_buffers[index],
                        length: RX_BUFFER_SIZE as u32 | LAST_FRAGMENT,
                    },
                );
            }
        }
        let tx_desc = descriptor_ptr::<TxDescriptor>(self.descriptors, RX_DESC_COUNT);
        unsafe {
            write_volatile(
                tx_desc,
                TxDescriptor {
                    next: 0,
                    status: 0,
                    address: self.tx_buffer,
                    length: 0,
                },
            );
        }

        self.command(TX_RESET)?;
        self.command(RX_RESET | 0x04)?;

        self.select_window(7);
        write32(self.io_base, PKT_STATUS, 0x20);
        write32(self.io_base, UP_LIST_PTR, self.descriptors);
        write32(self.io_base, DOWN_LIST_PTR, 0);

        self.select_window(3);
        write16(self.io_base, MAX_PACKET_SIZE, RX_BUFFER_SIZE as u16);
        self.command(SET_STATUS_ENABLE)?;
        self.command(SET_INTERRUPT_ENABLE)?;
        self.command(SET_RX_THRESHOLD | (RX_BUFFER_SIZE as u16 / 4))?;
        self.command(SET_RX_FILTER | RX_STATION | RX_BROADCAST)?;
        self.command(RX_ENABLE)?;
        self.command(TX_ENABLE)?;
        self.command(ACK_INTERRUPT | 0x07ff)?;
        Ok(())
    }

    fn read_eeprom(&self, address: u16) -> Result<u16, InitError> {
        self.select_window(0);
        write16(self.io_base, EEPROM_COMMAND, EEPROM_READ | address);
        let mut attempts = 100_000u32;
        while read16(self.io_base, EEPROM_COMMAND) & EEPROM_BUSY != 0 {
            attempts -= 1;
            if attempts == 0 {
                return Err(InitError::EepromTimedOut);
            }
        }
        Ok(read16(self.io_base, EEPROM_DATA))
    }

    pub fn mac_address(&self) -> [u8; 6] {
        self.mac
    }

    /// Reads the 3Com media-status link-beat bit. This is intentionally a
    /// small diagnostic rather than a complete MII link-state abstraction.
    pub fn link_up(&self) -> bool {
        self.select_window(4);
        read16(self.io_base, MEDIA_STATUS) & MEDIA_LINK_BEAT != 0
    }

    pub fn set_promiscuous(&self, enabled: bool) {
        self.select_window(1);
        let filter = RX_STATION | RX_BROADCAST | if enabled { RX_PROMISCUOUS } else { 0 };
        let _ = self.command(SET_RX_FILTER | filter);
    }

    pub fn overflow_count(&self) -> u32 {
        self.overflow_count.get()
    }

    pub fn send(&self, frame: &[u8]) -> bool {
        if frame.len() > MAX_FRAME_LEN {
            return false;
        }
        // This driver owns one download descriptor. Do not overwrite it
        // while the card still owns the previous packet; dropping here is
        // preferable to making a polling bridge wait on a dead link.
        if read32(self.io_base, DOWN_LIST_PTR) != 0 {
            return false;
        }
        let padded_len = frame.len().max(MIN_FRAME_LEN);
        unsafe {
            let buffer = core::slice::from_raw_parts_mut(self.tx_buffer as *mut u8, MAX_FRAME_LEN);
            buffer[..frame.len()].copy_from_slice(frame);
            buffer[frame.len()..padded_len].fill(0);
            let descriptor = descriptor_ptr::<TxDescriptor>(self.descriptors, RX_DESC_COUNT);
            let frame_start = if self.b_revision {
                // 3C905B/C cards use the B-revision frame-start header:
                // request completion indication and disable hardware
                // packet-length rounding. The payload length belongs in the
                // final-fragment field below.
                TX_INDICATE | TX_ROUNDUP_DEFEAT
            } else {
                padded_len as u32
            };
            write_volatile(&mut (*descriptor).status, frame_start);
            write_volatile(&mut (*descriptor).length, padded_len as u32 | LAST_FRAGMENT);
        }
        write32(
            self.io_base,
            DOWN_LIST_PTR,
            self.descriptors + (RX_DESC_COUNT * 16) as u32,
        );
        write16(self.io_base, CMD_STATUS, DOWN_UNSTALL);

        let descriptor = descriptor_ptr::<TxDescriptor>(self.descriptors, RX_DESC_COUNT);
        let mut attempts = 100_000u32;
        while unsafe { read_volatile(&(*descriptor).status) } & DOWNLOAD_COMPLETE == 0 {
            attempts -= 1;
            if attempts == 0 {
                write16(self.io_base, CMD_STATUS, TX_RESET);
                write16(self.io_base, CMD_STATUS, TX_ENABLE);
                return false;
            }
        }
        unsafe { write_volatile(&mut (*descriptor).status, 0) };
        true
    }

    pub fn receive(&self, out: &mut [u8]) -> Option<usize> {
        let index = self.rx_next.get();
        let descriptor = descriptor_ptr::<RxDescriptor>(self.descriptors, index);
        let status = unsafe { read_volatile(&(*descriptor).status) };
        if status & RX_COMPLETE == 0 {
            return None;
        }
        let length = (status & 0x1fff) as usize;
        unsafe { write_volatile(&mut (*descriptor).status, 0) };
        write16(self.io_base, CMD_STATUS, UP_UNSTALL);
        self.rx_next.set((index + 1) % RX_DESC_COUNT);
        if status & RX_ERROR != 0 || length > RX_BUFFER_SIZE {
            self.overflow_count.set(self.overflow_count.get() + 1);
            return None;
        }
        let copy_len = length.min(out.len()).min(MAX_FRAME_LEN);
        unsafe {
            let source = core::slice::from_raw_parts(self.rx_buffers[index] as *const u8, copy_len);
            out[..copy_len].copy_from_slice(source);
        }
        Some(copy_len)
    }
}

const MAX_INSTANCES: usize = pci::MAX_DEVICE_MATCHES;
static mut NICS: [Option<ThreeCom>; MAX_INSTANCES] = [const { None }; MAX_INSTANCES];
static mut NIC_COUNT: usize = 0;
static mut INITIALIZED: bool = false;

#[derive(Debug)]
pub enum Status {
    NotPresent,
    InitFailed,
}

pub fn initialize_all() -> Result<usize, Status> {
    unsafe {
        if !INITIALIZED {
            INITIALIZED = true;
            let mut probe_info = [const {
                ProbeInfo {
                    io_base: 0,
                    b_revision: false,
                }
            }; MAX_INSTANCES];
            let found = ThreeCom::probe_all(&mut probe_info);
            let mut first_error = None;
            for info in &probe_info[..found] {
                match ThreeCom::init(info.io_base, info.b_revision) {
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

pub fn get(index: usize) -> Option<&'static ThreeCom> {
    unsafe {
        if index >= NIC_COUNT {
            return None;
        }
        NICS[index].as_ref()
    }
}
