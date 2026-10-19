//! virtio-blk over PCI (virtio 1.x "modern" interface): the disk QEMU gives the aarch64 test machine.
//!
//! It exists so the portable storage stack (GPT, FOSFS, the boot-disk search) can run on aarch64 before the Pi's own
//! SD and USB drivers exist. The device is found by PCI vendor 0x1AF4 (id 0x1042, or the transitional 0x1001, which
//! carries the same modern capabilities), configured through the vendor capabilities that point into its BARs, and
//! driven by polling: one request in flight, three chained descriptors (header, data, status) on queue 0, no interrupts.
//!
//! Memory ordering: the rings are ordinary RAM shared with the device, so every publish is a `dsb sy` before the
//! notify write and every completion check re-reads the used index after one. DMA goes straight to the caller's
//! buffer (RAM is identity-mapped, so virtual is physical); QEMU is cache-coherent. A real DMA device on the Pi would
//! need non-cacheable buffers or cache maintenance.

use crate::{memory, pci, timer};

const VENDOR: u16 = 0x1af4;
const QUEUE_SIZE: u16 = 16;
const MAX_DRIVES: usize = 2;
/// Largest transfer per request (the device has no segment limit of its own without `SIZE_MAX`).
const CHUNK: usize = 128 * 1024;

const STATUS_ACKNOWLEDGE: u8 = 1;
const STATUS_DRIVER: u8 = 2;
const STATUS_DRIVER_OK: u8 = 4;
const STATUS_FEATURES_OK: u8 = 8;
const FEATURE_VERSION_1: u32 = 1; // bit 32 = bit 0 of feature word 1

#[derive(Clone, Copy, Debug)]
pub enum Error {
    Timeout,
    Device,
}

struct Queue {
    notify: u64,
    /// Descriptor table, available ring and used ring share one zeroed page: 0, 256 and 512.
    page: u64,
    request: u64,
    next_available: u16,
    last_used: u16,
}

static mut QUEUES: [Option<Queue>; MAX_DRIVES] = [const { None }; MAX_DRIVES];

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
        let queue = unsafe { (*core::ptr::addr_of_mut!(QUEUES))[self.index].as_mut().ok_or(Error::Device)? };
        let header = queue.request as *mut u32;
        let status = (queue.request + 16) as *mut u8;
        unsafe {
            header.write_volatile(kind);
            header.add(1).write_volatile(0);
            (queue.request as *mut u64).add(1).write_volatile(lba);
            status.write_volatile(0xff);
        }
        let descriptor = |slot: u64, address: u64, length: u32, flags: u16, next: u16| unsafe {
            let entry = (queue.page + slot * 16) as *mut u64;
            entry.write_volatile(address);
            ((entry as u64 + 8) as *mut u32).write_volatile(length);
            ((entry as u64 + 12) as *mut u16).write_volatile(flags);
            ((entry as u64 + 14) as *mut u16).write_volatile(next);
        };
        const NEXT: u16 = 1;
        const WRITE: u16 = 2; // the device writes this buffer
        descriptor(0, queue.request, 16, NEXT, 1);
        descriptor(1, data as u64, length as u32, NEXT | if kind == 0 { WRITE } else { 0 }, 2);
        descriptor(2, queue.request + 16, 1, WRITE, 0);

        let available = queue.page + 256;
        unsafe {
            let ring = (available + 4) as *mut u16;
            ring.add((queue.next_available % QUEUE_SIZE) as usize).write_volatile(0);
            queue.next_available = queue.next_available.wrapping_add(1);
            core::arch::asm!("dsb sy", options(nostack));
            ((available + 2) as *mut u16).write_volatile(queue.next_available);
            core::arch::asm!("dsb sy", options(nostack));
            (queue.notify as *mut u16).write_volatile(0);
        }
        let used_index = (queue.page + 512 + 2) as *const u16;
        let start = timer::micros();
        while unsafe { used_index.read_volatile() } == queue.last_used {
            if timer::micros() - start > 5_000_000 {
                return Err(Error::Timeout);
            }
        }
        queue.last_used = queue.last_used.wrapping_add(1);
        unsafe { core::arch::asm!("dsb sy", options(nostack)) };
        if unsafe { status.read_volatile() } == 0 { Ok(()) } else { Err(Error::Device) }
    }
}

/// The modern capabilities of a device: where each structure lives (bar, offset) and the notify multiplier.
struct Capabilities {
    common: Option<(u8, u32)>,
    notify: Option<(u8, u32)>,
    device: Option<(u8, u32)>,
    multiplier: u32,
}

fn capabilities(device: &pci::Device) -> Capabilities {
    let mut found = Capabilities { common: None, notify: None, device: None, multiplier: 0 };
    if device.read32(4) >> 16 & (1 << 4) == 0 {
        return found;
    }
    let mut offset = (device.read32(0x34) & 0xfc) as u8;
    for _ in 0..48 {
        if offset < 0x40 {
            break;
        }
        let header = device.read32(offset);
        if header as u8 == 0x09 {
            let kind = (header >> 24) as u8;
            let place = (device.read32(offset + 4) as u8, device.read32(offset + 8));
            match kind {
                1 => found.common = Some(place),
                2 => {
                    found.notify = Some(place);
                    found.multiplier = device.read32(offset + 16);
                }
                4 => found.device = Some(place),
                _ => {}
            }
        }
        offset = ((header >> 8) & 0xfc) as u8;
    }
    found
}

fn bring_up(device: &pci::Device, slot: usize) -> Option<Drive> {
    let caps = capabilities(device);
    let ((common_bar, common_at), (notify_bar, notify_at), (device_bar, device_at)) = (caps.common?, caps.notify?, caps.device?);
    device.enable_memory_and_bus_master();
    let mut bars = [0u64; 6];
    for bar in [common_bar, notify_bar, device_bar] {
        let (base, size) = device.memory_bar(bar)?;
        crate::paging::map_device(base, size);
        bars[bar as usize] = base;
    }
    let common = bars[common_bar as usize] + u64::from(common_at);
    let notify_base = bars[notify_bar as usize] + u64::from(notify_at);
    let config = bars[device_bar as usize] + u64::from(device_at);

    let r8 = |offset: u64| unsafe { ((common + offset) as *const u8).read_volatile() };
    let w8 = |offset: u64, value: u8| unsafe { ((common + offset) as *mut u8).write_volatile(value) };
    let r16 = |offset: u64| unsafe { ((common + offset) as *const u16).read_volatile() };
    let w16 = |offset: u64, value: u16| unsafe { ((common + offset) as *mut u16).write_volatile(value) };
    let r32 = |offset: u64| unsafe { ((common + offset) as *const u32).read_volatile() };
    let w32 = |offset: u64, value: u32| unsafe { ((common + offset) as *mut u32).write_volatile(value) };
    let w64 = |offset: u64, value: u64| unsafe { ((common + offset) as *mut u64).write_volatile(value) };

    w8(20, 0); // reset
    while r8(20) != 0 {}
    w8(20, STATUS_ACKNOWLEDGE | STATUS_DRIVER);
    w32(0, 1); // device features, word 1 (bits 32-63)
    if r32(4) & FEATURE_VERSION_1 == 0 {
        return None; // a legacy-only device
    }
    w32(8, 0);
    w32(12, 0);
    w32(8, 1);
    w32(12, FEATURE_VERSION_1);
    w8(20, STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK);
    if r8(20) & STATUS_FEATURES_OK == 0 {
        return None;
    }

    w16(22, 0); // queue 0
    if r16(24) < QUEUE_SIZE {
        return None;
    }
    w16(24, QUEUE_SIZE);
    let page = memory::allocate()?;
    let request = memory::allocate()?;
    w64(32, page);
    w64(40, page + 256);
    w64(48, page + 512);
    let notify = notify_base + u64::from(r16(30)) * u64::from(caps.multiplier);
    w16(28, 1);
    w8(20, STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK);

    let sectors = unsafe { (config as *const u64).read_volatile() };
    unsafe { (*core::ptr::addr_of_mut!(QUEUES))[slot] = Some(Queue { notify, page, request, next_available: 0, last_used: 0 }) };
    Some(Drive { index: slot, sectors })
}

/// Brings up every virtio-blk device on the PCI bus (`pci::scan` has run) and hands each to `register(drive, number)`.
pub fn init_all(mut register: impl FnMut(Drive, usize)) {
    let mut slot = 0;
    for device in pci::devices() {
        if slot == MAX_DRIVES {
            break;
        }
        if device.vendor == VENDOR && (device.id == 0x1042 || device.id == 0x1001) {
            if let Some(drive) = bring_up(&device, slot) {
                register(drive, slot);
                slot += 1;
            }
        }
    }
}
