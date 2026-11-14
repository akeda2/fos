//! virtio-net over PCI (virtio 1.x "modern" interface): the network card QEMU gives the aarch64 test machine, so the
//! server kernel's network stack (term net, KUP, NETFS, DHCP) can be exercised before the Pi 4's GENET driver exists.
//!
//! Two split virtqueues (`virtio`): queue 0 receives, queue 1 transmits. Every receive descriptor owns a fixed 2 KiB buffer
//! that is handed back to the device after the frame is copied out; transmit buffers come from a free list, because the
//! device may complete them in any order. With VERSION_1 each frame carries a 12-byte `virtio_net_hdr` (all zero here: no
//! checksum offload, no segmentation, no mergeable buffers). Frames are copied, so callers need no DMA-capable memory.
//! Polled only (`Delivery::Polled`): `receive` looks at the used ring, which is cheap enough for the service loop.

use crate::e1000::Delivery;
use crate::virtio::{self, Queue, Transport, DESC_WRITE};
use crate::{memory, pci};

const SIZE: u16 = 64;
const BUFFER: u64 = 2048;
const HEADER: usize = 12;
const FEATURE_MAC: u32 = 1 << 5;
const FEATURE_STATUS: u32 = 1 << 16;
const STATUS_LINK_UP: u16 = 1;

pub const VENDOR_VIRTIO: u16 = virtio::VENDOR;
pub const DEVICE_IDS: [u16; 2] = virtio::DEVICE_NET;

pub struct VirtioNet {
    pub delivery: Delivery,
    pub mac: [u8; 6],
    transport: Transport,
    rx: Queue,
    tx: Queue,
    rx_buffers: u64,
    tx_buffers: u64,
    rx_available: u16,
    rx_last_used: u16,
    tx_available: u16,
    tx_last_used: u16,
    tx_free: [u16; SIZE as usize],
    tx_free_count: usize,
    has_status: bool,
    pub frames_received: u64,
    pub frames_sent: u64,
    pub frames_dropped_no_link: u64,
}

#[derive(Debug)]
pub enum Error {
    /// Not a modern virtio device, or its capabilities are missing.
    Transport,
    Features,
    Queues,
    Memory,
}

impl VirtioNet {
    pub fn init(device: &pci::Device, _index: usize) -> Result<VirtioNet, Error> {
        let transport = Transport::probe(device).ok_or(Error::Transport)?;
        let features = transport.negotiate(FEATURE_MAC | FEATURE_STATUS).ok_or(Error::Features)?;
        let rx = transport.queue(0, SIZE).ok_or(Error::Queues)?;
        let tx = transport.queue(1, SIZE).ok_or(Error::Queues)?;
        let pages = (u64::from(SIZE) * BUFFER).div_ceil(4096);
        let rx_buffers = memory::allocate_contiguous(pages, None).ok_or(Error::Memory)?;
        let tx_buffers = memory::allocate_contiguous(pages, None).ok_or(Error::Memory)?;

        let mut mac = [0u8; 6];
        if features & FEATURE_MAC != 0 {
            for (index, byte) in mac.iter_mut().enumerate() {
                *byte = transport.config_u8(index as u64);
            }
        } else {
            mac = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56]; // QEMU's default prefix; a device without the MAC feature is unusual
        }

        let mut nic = VirtioNet {
            delivery: Delivery::Polled,
            mac,
            transport,
            rx,
            tx,
            rx_buffers,
            tx_buffers,
            rx_available: 0,
            rx_last_used: 0,
            tx_available: 0,
            tx_last_used: 0,
            tx_free: [0; SIZE as usize],
            tx_free_count: SIZE as usize,
            has_status: features & FEATURE_STATUS != 0,
            frames_received: 0,
            frames_sent: 0,
            frames_dropped_no_link: 0,
        };
        for slot in 0..SIZE {
            nic.tx_free[slot as usize] = slot;
            nic.rx.descriptor(slot, rx_buffers + u64::from(slot) * BUFFER, BUFFER as u32, DESC_WRITE, 0);
            nic.rx.publish(nic.rx_available, slot);
            nic.rx_available = nic.rx_available.wrapping_add(1);
        }
        nic.transport.driver_ok();
        nic.rx.notify(0);
        Ok(nic)
    }

    pub fn link_up(&self) -> bool {
        !self.has_status || self.transport.config_u16(6) & STATUS_LINK_UP != 0
    }

    pub fn describe(&self) {
        crate::println!("   virtio-net, {} receive and {} transmit buffers of {} bytes, polled", SIZE, SIZE, BUFFER);
    }

    /// The next received frame, copied to `out`; `None` when there is none.
    pub fn receive(&mut self, out: &mut [u8]) -> Option<usize> {
        if self.rx.used_index() == self.rx_last_used {
            return None;
        }
        let (slot, length) = self.rx.used_entry(self.rx_last_used);
        self.rx_last_used = self.rx_last_used.wrapping_add(1);
        let length = (length as usize).saturating_sub(HEADER).min(BUFFER as usize - HEADER);
        let source = memory::phys_to_virt(self.rx_buffers + u64::from(slot) * BUFFER + HEADER as u64) as *const u8;
        let copied = length.min(out.len());
        unsafe { core::ptr::copy_nonoverlapping(source, out.as_mut_ptr(), copied) };
        // Back to the device: the descriptor still points at the same buffer.
        self.rx.publish(self.rx_available, slot);
        self.rx_available = self.rx_available.wrapping_add(1);
        self.rx.notify(0);
        self.frames_received += 1;
        Some(copied)
    }

    pub fn send(&mut self, frame: &[u8]) -> bool {
        if !self.link_up() {
            self.frames_dropped_no_link += 1;
            return false;
        }
        if frame.is_empty() || frame.len() + HEADER > BUFFER as usize {
            return false;
        }
        while self.tx.used_index() != self.tx_last_used {
            let (slot, _) = self.tx.used_entry(self.tx_last_used);
            self.tx_last_used = self.tx_last_used.wrapping_add(1);
            self.tx_free[self.tx_free_count] = slot;
            self.tx_free_count += 1;
        }
        if self.tx_free_count == 0 {
            return false;
        }
        self.tx_free_count -= 1;
        let slot = self.tx_free[self.tx_free_count];
        let buffer = self.tx_buffers + u64::from(slot) * BUFFER;
        unsafe {
            let at = memory::phys_to_virt(buffer) as *mut u8;
            core::ptr::write_bytes(at, 0, HEADER);
            core::ptr::copy_nonoverlapping(frame.as_ptr(), at.add(HEADER), frame.len());
        }
        self.tx.descriptor(slot, buffer, (frame.len() + HEADER) as u32, 0, 0);
        self.tx.publish(self.tx_available, slot);
        self.tx_available = self.tx_available.wrapping_add(1);
        self.tx.notify(1);
        self.frames_sent += 1;
        true
    }

    /// No control queue is negotiated, so the receive filter stays at the device's default; there is nothing to switch.
    pub fn set_promiscuous(&mut self, _on: bool) {}
}
