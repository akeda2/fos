//! The virtio 1.x PCI transport ("modern" interface), shared by `virtio_blk` and `virtio_net`.
//!
//! A device is configured through vendor capabilities (PCI capability id 0x09) that point into its BARs: the common
//! configuration (status, feature negotiation, queue set-up), the notify area (one doorbell per queue, spaced by a
//! multiplier) and the device-specific configuration. Queues are split virtqueues: a descriptor table, an available ring
//! the driver fills and a used ring the device fills, all in ordinary RAM (here one zeroed page per queue: descriptors at
//! 0, available ring at `AVAIL`, used ring at `USED`, which fits queues of up to 128 entries).
//!
//! Memory ordering: a ring publish is `barrier()` before the index write and again before the doorbell; a completion check
//! reads the used index and then `barrier()`s. QEMU's devices are cache-coherent; a real non-coherent DMA device would
//! need non-cacheable buffers or cache maintenance on top.

use crate::{memory, pci};

pub const VENDOR: u16 = 0x1af4;
/// PCI device ids: modern (0x1040 + type) and the transitional ids, which carry the same modern capabilities.
pub const DEVICE_NET: [u16; 2] = [0x1041, 0x1000];
pub const DEVICE_BLOCK: [u16; 2] = [0x1042, 0x1001];

pub const STATUS_ACKNOWLEDGE: u8 = 1;
pub const STATUS_DRIVER: u8 = 2;
pub const STATUS_DRIVER_OK: u8 = 4;
pub const STATUS_FEATURES_OK: u8 = 8;

pub const DESC_NEXT: u16 = 1;
/// The device writes this buffer.
pub const DESC_WRITE: u16 = 2;

pub const AVAIL: u64 = 2048;
pub const USED: u64 = 3072;

/// Orders ring and buffer writes against the doorbell and the device's own writes against our reads.
#[inline]
pub fn barrier() {
    #[cfg(target_arch = "aarch64")]
    unsafe {
        core::arch::asm!("dsb sy", options(nostack))
    };
    #[cfg(not(target_arch = "aarch64"))]
    core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
}

pub struct Transport {
    common: u64,
    notify_base: u64,
    multiplier: u64,
    config: u64,
}

/// A split virtqueue: its page and the doorbell address (physical, as `memory::phys_to_virt` expects).
pub struct Queue {
    pub page: u64,
    pub size: u16,
    notify: u64,
}

impl Queue {
    pub fn descriptor(&self, slot: u16, address: u64, length: u32, flags: u16, next: u16) {
        unsafe {
            let entry = memory::phys_to_virt(self.page + u64::from(slot) * 16) as *mut u64;
            entry.write_volatile(address);
            ((entry as u64 + 8) as *mut u32).write_volatile(length);
            ((entry as u64 + 12) as *mut u16).write_volatile(flags);
            ((entry as u64 + 14) as *mut u16).write_volatile(next);
        }
    }

    /// Puts descriptor chain head `head` at position `position` (a running count) of the available ring and publishes the
    /// new index `position + 1`.
    pub fn publish(&self, position: u16, head: u16) {
        unsafe {
            let ring = memory::phys_to_virt(self.page + AVAIL + 4) as *mut u16;
            ring.add((position % self.size) as usize).write_volatile(head);
            barrier();
            (memory::phys_to_virt(self.page + AVAIL + 2) as *mut u16).write_volatile(position.wrapping_add(1));
        }
    }

    pub fn notify(&self, queue_index: u16) {
        barrier();
        unsafe { (memory::phys_to_virt(self.notify) as *mut u16).write_volatile(queue_index) };
    }

    /// The device's used index (how many chains it has completed, modulo 65536).
    pub fn used_index(&self) -> u16 {
        let index = unsafe { (memory::phys_to_virt(self.page + USED + 2) as *const u16).read_volatile() };
        barrier();
        index
    }

    /// The used-ring entry at running position `position`: (descriptor chain head, bytes written by the device).
    pub fn used_entry(&self, position: u16) -> (u16, u32) {
        let entry = memory::phys_to_virt(self.page + USED + 4 + u64::from(position % self.size) * 8);
        unsafe { ((entry as *const u32).read_volatile() as u16, ((entry + 4) as *const u32).read_volatile()) }
    }
}

impl Transport {
    /// Finds the capabilities, enables memory and bus mastering and maps the BARs the structures live in.
    pub fn probe(device: &pci::Device) -> Option<Transport> {
        let (mut common, mut notify, mut config) = (None, None, None);
        let mut multiplier = 0;
        if device.read32(4) >> 16 & (1 << 4) == 0 {
            return None;
        }
        let mut offset = (device.read32(0x34) & 0xfc) as u8;
        for _ in 0..48 {
            if offset < 0x40 {
                break;
            }
            let header = device.read32(offset);
            if header as u8 == 0x09 {
                let place = (device.read32(offset + 4) as u8, device.read32(offset + 8));
                match (header >> 24) as u8 {
                    1 => common = Some(place),
                    2 => {
                        notify = Some(place);
                        multiplier = device.read32(offset + 16);
                    }
                    4 => config = Some(place),
                    _ => {}
                }
            }
            offset = ((header >> 8) & 0xfc) as u8;
        }
        let ((common_bar, common_at), (notify_bar, notify_at), (config_bar, config_at)) = (common?, notify?, config?);
        device.enable_memory_and_bus_master();
        let mut bars = [0u64; 6];
        for bar in [common_bar, notify_bar, config_bar] {
            let (base, size) = device.memory_bar(bar)?;
            crate::paging::map_device(base, size);
            bars[bar as usize] = base;
        }
        Some(Transport {
            common: memory::phys_to_virt(bars[common_bar as usize] + u64::from(common_at)),
            notify_base: bars[notify_bar as usize] + u64::from(notify_at),
            multiplier: u64::from(multiplier),
            config: memory::phys_to_virt(bars[config_bar as usize] + u64::from(config_at)),
        })
    }

    fn r8(&self, offset: u64) -> u8 {
        unsafe { ((self.common + offset) as *const u8).read_volatile() }
    }
    fn w8(&self, offset: u64, value: u8) {
        unsafe { ((self.common + offset) as *mut u8).write_volatile(value) }
    }
    fn r16(&self, offset: u64) -> u16 {
        unsafe { ((self.common + offset) as *const u16).read_volatile() }
    }
    fn w16(&self, offset: u64, value: u16) {
        unsafe { ((self.common + offset) as *mut u16).write_volatile(value) }
    }
    fn r32(&self, offset: u64) -> u32 {
        unsafe { ((self.common + offset) as *const u32).read_volatile() }
    }
    fn w32(&self, offset: u64, value: u32) {
        unsafe { ((self.common + offset) as *mut u32).write_volatile(value) }
    }
    fn w64(&self, offset: u64, value: u64) {
        unsafe { ((self.common + offset) as *mut u64).write_volatile(value) }
    }

    /// Resets the device and negotiates features: VERSION_1 always, plus the bits of `wanted` (feature word 0) the device
    /// offers. Returns the accepted word-0 bits, `None` for a legacy-only device or one that refuses the set.
    pub fn negotiate(&self, wanted: u32) -> Option<u32> {
        self.w8(20, 0);
        while self.r8(20) != 0 {}
        self.w8(20, STATUS_ACKNOWLEDGE | STATUS_DRIVER);
        self.w32(0, 0);
        let accepted = self.r32(4) & wanted;
        self.w32(0, 1);
        if self.r32(4) & 1 == 0 {
            return None;
        }
        self.w32(8, 0);
        self.w32(12, accepted);
        self.w32(8, 1);
        self.w32(12, 1);
        self.w8(20, STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK);
        if self.r8(20) & STATUS_FEATURES_OK == 0 {
            return None;
        }
        Some(accepted)
    }

    /// Sets up queue `index` with `size` entries (a power of two the device supports) in one zeroed page.
    pub fn queue(&self, index: u16, size: u16) -> Option<Queue> {
        self.w16(22, index);
        if self.r16(24) < size {
            return None;
        }
        self.w16(24, size);
        let page = memory::allocate_contiguous(1, None)?;
        self.w64(32, page);
        self.w64(40, page + AVAIL);
        self.w64(48, page + USED);
        let notify = self.notify_base + u64::from(self.r16(30)) * self.multiplier;
        self.w16(28, 1);
        Some(Queue { page, size, notify })
    }

    pub fn driver_ok(&self) {
        self.w8(20, STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK);
    }

    pub fn config_u8(&self, offset: u64) -> u8 {
        unsafe { ((self.config + offset) as *const u8).read_volatile() }
    }

    pub fn config_u16(&self, offset: u64) -> u16 {
        unsafe { ((self.config + offset) as *const u16).read_volatile() }
    }

    pub fn config_u64(&self, offset: u64) -> u64 {
        unsafe { ((self.config + offset) as *const u64).read_volatile() }
    }
}
