//! virtio-blk over PCI (virtio 1.x "modern" interface): the disk QEMU gives the aarch64 test machine.
//!
//! It exists so the portable storage stack (GPT, FOSFS, the boot-disk search) can run on aarch64 before the Pi's own
//! SD and USB drivers exist. The device is found by PCI vendor 0x1AF4 (id 0x1042, or the transitional 0x1001, which
//! carries the same modern capabilities), configured through the shared `virtio` transport, and
//! driven by polling: one request in flight, three chained descriptors (header, data, status) on queue 0, no interrupts.
//!
//! Memory ordering: the rings are ordinary RAM shared with the device, so every publish is a `dsb sy` before the
//! notify write and every completion check re-reads the used index after one. DMA goes straight to the caller's
//! buffer (RAM is identity-mapped, so virtual is physical); QEMU is cache-coherent. A real DMA device on the Pi would
//! need non-cacheable buffers or cache maintenance.

use crate::virtio::{self, Queue, Transport, DESC_NEXT, DESC_WRITE};
use crate::{memory, pci, timer};

const QUEUE_SIZE: u16 = 16;
const MAX_DRIVES: usize = 2;
/// Largest transfer per request (the device has no segment limit of its own without `SIZE_MAX`).
const CHUNK: usize = 128 * 1024;

#[derive(Clone, Copy, Debug)]
pub enum Error {
    Timeout,
    Device,
}

struct Disk {
    queue: Queue,
    request: u64,
    next_available: u16,
    last_used: u16,
}

static mut DISKS: [Option<Disk>; MAX_DRIVES] = [const { None }; MAX_DRIVES];

#[derive(Clone, Copy)]
pub struct Drive {
    index: usize,
    pub sectors: u64,
}

impl Drive {
    pub fn model(&self) -> [u8; 40] {
        let mut model = [b' '; 40];
        model[..10].copy_from_slice(b"virtio-blk");
        model
    }

    pub fn read(&self, lba: u64, buffer: &mut [u8]) -> Result<(), Error> {
        for (index, chunk) in buffer.chunks_mut(CHUNK).enumerate() {
            self.transfer(0, lba + (index * CHUNK / 512) as u64, chunk.as_mut_ptr(), chunk.len())?;
        }
        Ok(())
    }

    pub fn write(&self, lba: u64, buffer: &[u8]) -> Result<(), Error> {
        for (index, chunk) in buffer.chunks(CHUNK).enumerate() {
            self.transfer(1, lba + (index * CHUNK / 512) as u64, chunk.as_ptr() as *mut u8, chunk.len())?;
        }
        Ok(())
    }

    /// One request: `kind` 0 = read, 1 = write.
    fn transfer(&self, kind: u32, lba: u64, data: *mut u8, length: usize) -> Result<(), Error> {
        let disk = unsafe { (*core::ptr::addr_of_mut!(DISKS))[self.index].as_mut().ok_or(Error::Device)? };
        let header = memory::phys_to_virt(disk.request) as *mut u32;
        let status = memory::phys_to_virt(disk.request + 16) as *mut u8;
        unsafe {
            header.write_volatile(kind);
            header.add(1).write_volatile(0);
            (memory::phys_to_virt(disk.request) as *mut u64).add(1).write_volatile(lba);
            status.write_volatile(0xff);
        }
        let queue = &disk.queue;
        queue.descriptor(0, disk.request, 16, DESC_NEXT, 1);
        queue.descriptor(1, memory::virt_to_phys(data as u64), length as u32, DESC_NEXT | if kind == 0 { DESC_WRITE } else { 0 }, 2);
        queue.descriptor(2, disk.request + 16, 1, DESC_WRITE, 0);
        queue.publish(disk.next_available, 0);
        disk.next_available = disk.next_available.wrapping_add(1);
        queue.notify(0);
        let start = timer::micros();
        while queue.used_index() == disk.last_used {
            if timer::micros() - start > 5_000_000 {
                return Err(Error::Timeout);
            }
        }
        disk.last_used = disk.last_used.wrapping_add(1);
        if unsafe { status.read_volatile() } == 0 { Ok(()) } else { Err(Error::Device) }
    }
}

fn bring_up(device: &pci::Device, slot: usize) -> Option<Drive> {
    let transport = Transport::probe(device)?;
    transport.negotiate(0)?;
    let queue = transport.queue(0, QUEUE_SIZE)?;
    let request = memory::allocate_contiguous(1, None)?;
    transport.driver_ok();
    let sectors = transport.config_u64(0);
    unsafe { (*core::ptr::addr_of_mut!(DISKS))[slot] = Some(Disk { queue, request, next_available: 0, last_used: 0 }) };
    Some(Drive { index: slot, sectors })
}

/// Brings up every virtio-blk device on the PCI bus (`pci::scan` has run) and hands each to `register(drive, number)`.
pub fn init_all(mut register: impl FnMut(Drive, usize)) {
    let mut slot = 0;
    for device in pci::devices() {
        if slot == MAX_DRIVES {
            break;
        }
        if device.vendor == virtio::VENDOR && virtio::DEVICE_BLOCK.contains(&device.id) {
            if let Some(drive) = bring_up(&device, slot) {
                register(drive, slot);
                slot += 1;
            }
        }
    }
}
