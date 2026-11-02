//! The platform interface of the shared kernel code.
//!
//! `kernel-core` modules (the filesystems first) never name a driver or an architecture: they call the functions here, and
//! whichever core is running has installed a [`Platform`] that answers them. This is the "slow path" half of the
//! plan in `docs/big-kernel.md`: one indirect call per disk transfer or log line costs nothing measurable. Hot paths (port
//! I/O, `micros`) are not here; they stay inline in the code that is generic over its core.
//!
//! The platform is set once, before the first call and before other CPUs run; it is never replaced while in use.

#![no_std]

use core::fmt;

/// Why a block transfer failed (the same three reasons the kernel's disk layer has).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockError {
    NoSuchDisk,
    OutOfRange,
    Device,
}

/// What a platform knows of a disk.
#[derive(Debug, Clone, Copy)]
pub struct BlockInfo {
    pub sectors: u64,
    pub model: [u8; 40],
}

/// What a core provides. Every method is cheap to implement by forwarding to the core's own modules.
pub trait Platform: Sync {
    /// Reads whole 512-byte sectors (`buffer.len()` a multiple of 512) of disk `disk` starting at `lba`.
    fn block_read(&self, disk: usize, lba: u64, buffer: &mut [u8]) -> Result<(), BlockError>;
    /// Writes whole 512-byte sectors.
    fn block_write(&self, disk: usize, lba: u64, buffer: &[u8]) -> Result<(), BlockError>;
    /// The wall clock as Unix seconds (local reading taken as UTC), 0 when it was never set.
    fn unix_seconds(&self) -> u64;
    /// Size and model of disk `disk` (0-based), `None` when there is no such disk.
    fn block_info(&self, disk: usize) -> Option<BlockInfo>;
    /// How many disks there are.
    fn block_count(&self) -> usize;
    /// Registers the USB mass-storage device in `usb_storage`'s table slot `slot` as a disk; its disk number.
    fn block_register_usb(&self, slot: usize, sectors: u64, model: [u8; 40], origin: &str) -> Option<usize>;
    /// The disk number of that USB device (the slot is the stable identity; disk numbers move).
    fn block_usb_disk(&self, slot: usize) -> Option<usize>;
    /// The disk the system booted from, if known.
    fn boot_disk(&self) -> Option<usize>;
    /// Microseconds on a monotonic clock.
    fn timer_micros(&self) -> u64;
    /// A free-running hardware counter (TSC or the generic timer's count); only its low bits are meant as noise.
    fn timer_counter(&self) -> u64;
    /// Timer interrupt ticks since boot.
    fn timer_ticks(&self) -> u64;
    /// Console text, as `println!` produces it.
    fn print(&self, arguments: fmt::Arguments);

    // The machine: what drivers in `kernel-core` ask of the core underneath. One indirect call each; a port access or a
    // PCI config cycle costs far more than that, and nothing on a packet's hot path uses them.

    /// `count` zeroed physically contiguous 4 KiB frames, entirely below `below` when given; the physical address.
    fn allocate_contiguous(&self, count: u64, below: Option<u64>) -> Option<u64>;
    /// Gives back frames from `allocate_contiguous`.
    fn free_frames(&self, address: u64, count: u64);
    /// Maps `length` bytes of device registers at physical `address` (identity, uncached, never executable).
    fn map_device(&self, address: u64, length: u64);
    fn wait_ticks(&self, count: u64);

    fn pci_read32(&self, bus: u8, device: u8, function: u8, offset: u8) -> u32;
    fn pci_write32(&self, bus: u8, device: u8, function: u8, offset: u8, value: u32);
    /// The CPU address of a PCI memory address (identity unless a bridge translates).
    fn pci_cpu_address(&self, pci: u64) -> u64 {
        pci
    }

    /// Legacy port I/O (x86 only; other cores read all ones and ignore writes).
    fn port_in8(&self, _port: u16) -> u8 {
        0xff
    }
    fn port_in16(&self, _port: u16) -> u16 {
        0xffff
    }
    fn port_in32(&self, _port: u16) -> u32 {
        0xffff_ffff
    }
    fn port_out8(&self, _port: u16, _value: u8) {}
    fn port_out16(&self, _port: u16, _value: u16) {}
    fn port_out32(&self, _port: u16, _value: u32) {}
    /// Disables interrupts and says whether they were on; `interrupts_restore` undoes it.
    fn interrupts_save_and_disable(&self) -> bool {
        false
    }
    fn interrupts_restore(&self, _was_enabled: bool) {}
    /// The foreground made progress (the watchdog's "pet"); wait loops call it.
    fn watchdog_pet(&self) {}

    /// Message-signalled and legacy interrupts: claim a vector for `handler(context)`, and so on.
    fn msi_allocate(&self, handler: fn(usize), context: usize) -> Option<u8>;
    fn msi_release(&self, vector: u8);
    fn msi_count(&self, vector: u8) -> u64;
    /// The (address, data) message a device writes to raise `vector` on this machine.
    fn msi_message(&self, vector: u8) -> (u64, u32);
    fn msi_claim(&self);
    fn msi_intx_candidates(&self, interrupt_line: u8) -> [u32; 9];
    fn msi_attach_intx(&self, gsi: u32, handler: fn(usize), context: usize) -> Option<u8>;
    fn msi_detach_intx(&self, vector: u8, context: usize);
    /// The interrupt controller id MSI messages are addressed to, and the range of GSIs the I/O APIC serves.
    fn interrupt_target(&self) -> u32;
    fn gsi_range(&self) -> (u32, u32);
}

static mut PLATFORM: Option<&'static dyn Platform> = None;

/// Installs the platform. Call once, early, from the core's entry code.
pub fn set_platform(platform: &'static dyn Platform) {
    unsafe { *core::ptr::addr_of_mut!(PLATFORM) = Some(platform) };
}

fn platform() -> Option<&'static dyn Platform> {
    unsafe { *core::ptr::addr_of!(PLATFORM) }
}

/// Block devices, as the platform numbers them (`disk1` is 1... as the kernel's `block` module does).
pub mod block {
    use super::{BlockError, platform};

    pub type Error = BlockError;

    pub fn read(disk: usize, lba: u64, buffer: &mut [u8]) -> Result<(), Error> {
        platform().ok_or(BlockError::NoSuchDisk)?.block_read(disk, lba, buffer)
    }

    pub fn write(disk: usize, lba: u64, buffer: &[u8]) -> Result<(), Error> {
        platform().ok_or(BlockError::NoSuchDisk)?.block_write(disk, lba, buffer)
    }

    /// Size of the disk in sectors.
    pub fn sectors(disk: usize) -> Option<u64> {
        info(disk).map(|info| info.sectors)
    }

    pub fn info(disk: usize) -> Option<super::BlockInfo> {
        platform()?.block_info(disk)
    }

    pub fn count() -> usize {
        platform().map_or(0, |platform| platform.block_count())
    }

    pub fn register_usb(slot: usize, sectors: u64, model: [u8; 40], origin: &str) -> Option<usize> {
        platform()?.block_register_usb(slot, sectors, model, origin)
    }

    pub fn usb_disk(slot: usize) -> Option<usize> {
        platform()?.block_usb_disk(slot)
    }

    pub fn boot_disk() -> Option<usize> {
        platform()?.boot_disk()
    }
}

pub mod timer {
    pub fn wait_ticks(count: u64) {
        if let Some(platform) = super::platform() {
            platform.wait_ticks(count);
        }
    }

    pub fn micros() -> u64 {
        super::platform().map_or(0, |platform| platform.timer_micros())
    }

    pub fn counter() -> u64 {
        super::platform().map_or(0, |platform| platform.timer_counter())
    }

    pub fn ticks() -> u64 {
        super::platform().map_or(0, |platform| platform.timer_ticks())
    }
}

pub mod clock {
    /// Unix seconds of the wall clock; 0 when unset or when no platform is installed.
    pub fn unix_seconds() -> u64 {
        super::platform().map_or(0, |platform| platform.unix_seconds())
    }
}

/// Physical memory.
pub mod memory {
    pub const FRAME: u64 = 4096;

    pub fn allocate_contiguous(count: u64, below: Option<u64>) -> Option<u64> {
        super::platform()?.allocate_contiguous(count, below)
    }

    pub fn free(address: u64, count: u64) {
        if let Some(platform) = super::platform() {
            platform.free_frames(address, count);
        }
    }
}

pub mod paging {
    pub fn map_device(address: u64, length: u64) {
        if let Some(platform) = super::platform() {
            platform.map_device(address, length);
        }
    }
}

/// PCI configuration space, by bus/device/function/offset.
pub mod pci_config {
    pub fn read32(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
        super::platform().map_or(0xffff_ffff, |platform| platform.pci_read32(bus, device, function, offset))
    }

    pub fn write32(bus: u8, device: u8, function: u8, offset: u8, value: u32) {
        if let Some(platform) = super::platform() {
            platform.pci_write32(bus, device, function, offset, value);
        }
    }

    pub fn cpu_address(pci: u64) -> u64 {
        super::platform().map_or(pci, |platform| platform.pci_cpu_address(pci))
    }
}

/// Port I/O and the interrupt flag (x86).
pub mod cpu {
    pub fn inb(port: u16) -> u8 {
        super::platform().map_or(0xff, |platform| platform.port_in8(port))
    }
    pub fn inw(port: u16) -> u16 {
        super::platform().map_or(0xffff, |platform| platform.port_in16(port))
    }
    pub fn inl(port: u16) -> u32 {
        super::platform().map_or(0xffff_ffff, |platform| platform.port_in32(port))
    }
    pub fn outb(port: u16, value: u8) {
        if let Some(platform) = super::platform() {
            platform.port_out8(port, value);
        }
    }
    pub fn outw(port: u16, value: u16) {
        if let Some(platform) = super::platform() {
            platform.port_out16(port, value);
        }
    }
    pub fn outl(port: u16, value: u32) {
        if let Some(platform) = super::platform() {
            platform.port_out32(port, value);
        }
    }
    pub fn interrupts_save_and_disable() -> bool {
        super::platform().is_some_and(|platform| platform.interrupts_save_and_disable())
    }
    pub fn interrupts_restore(was_enabled: bool) {
        if let Some(platform) = super::platform() {
            platform.interrupts_restore(was_enabled);
        }
    }
}

/// Crash and watchdog support.
pub mod debug {
    pub fn pet() {
        if let Some(platform) = super::platform() {
            platform.watchdog_pet();
        }
    }
}

/// Interrupt vectors for devices.
pub mod msi {
    pub fn allocate(handler: fn(usize), context: usize) -> Option<u8> {
        super::platform()?.msi_allocate(handler, context)
    }
    pub fn release(vector: u8) {
        if let Some(platform) = super::platform() {
            platform.msi_release(vector);
        }
    }
    pub fn count(vector: u8) -> u64 {
        super::platform().map_or(0, |platform| platform.msi_count(vector))
    }
    /// The (address, data) a device writes to raise `vector`.
    pub fn message(vector: u8) -> (u64, u32) {
        super::platform().map_or((0, 0), |platform| platform.msi_message(vector))
    }
    pub fn claim() {
        if let Some(platform) = super::platform() {
            platform.msi_claim();
        }
    }
    pub fn intx_candidates(interrupt_line: u8) -> [u32; 9] {
        super::platform().map_or([u32::MAX; 9], |platform| platform.msi_intx_candidates(interrupt_line))
    }
    pub fn attach_intx(gsi: u32, handler: fn(usize), context: usize) -> Option<u8> {
        super::platform()?.msi_attach_intx(gsi, handler, context)
    }
    pub fn detach_intx(vector: u8, context: usize) {
        if let Some(platform) = super::platform() {
            platform.msi_detach_intx(vector, context);
        }
    }
}

pub fn interrupt_target() -> u32 {
    platform().map_or(0, |platform| platform.interrupt_target())
}

pub fn gsi_range() -> (u32, u32) {
    platform().map_or((0, 0), |platform| platform.gsi_range())
}

/// `println!` for shared code: formats and hands the text to the platform's console.
pub fn print(arguments: fmt::Arguments) {
    if let Some(platform) = platform() {
        platform.print(arguments);
    }
}

#[macro_export]
macro_rules! println {
    () => { $crate::print(format_args!("\n")) };
    ($($argument:tt)*) => { $crate::print(format_args!("{}\n", format_args!($($argument)*))) };
}
