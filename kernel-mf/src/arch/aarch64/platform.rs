//! kernel-mf's answer to `fos_hal::Platform` on aarch64: what the shared drivers in `kernel-core` need from the machine. The same
//! contract as `arch/x86_64/platform.rs`; the differences are PCI config access (ECAM, or the Pi 4's own scheme), MSI delivery
//! (GICv2m or the Pi 4's MSI block), no port I/O and no legacy INTx routing.

use fos_hal::{BlockError, BlockInfo, Platform};

struct Mf;

fn convert(error: crate::block::Error) -> BlockError {
    match error {
        crate::block::Error::NoSuchDisk => BlockError::NoSuchDisk,
        crate::block::Error::OutOfRange => BlockError::OutOfRange,
        crate::block::Error::Device => BlockError::Device,
        crate::block::Error::Unsupported => BlockError::Unsupported,
    }
}

impl Platform for Mf {
    fn block_read(&self, disk: usize, lba: u64, buffer: &mut [u8]) -> Result<(), BlockError> {
        crate::block::read(disk, lba, buffer).map_err(convert)
    }
    fn block_write(&self, disk: usize, lba: u64, buffer: &[u8]) -> Result<(), BlockError> {
        crate::block::write(disk, lba, buffer).map_err(convert)
    }
    fn block_write_deferred(&self, disk: usize, lba: u64, buffer: &[u8]) -> Result<(), BlockError> {
        crate::block::write_deferred(disk, lba, buffer).map_err(convert)
    }
    fn block_flush(&self, disk: usize) -> Result<(), BlockError> {
        crate::block::flush(disk).map_err(convert)
    }
    fn block_discard(&self, disk: usize, ranges: &[(u64, u64)]) -> Result<(), BlockError> {
        crate::block::discard(disk, ranges).map_err(convert)
    }
    fn block_info(&self, disk: usize) -> Option<BlockInfo> {
        crate::block::get(disk).map(|disk| BlockInfo { sectors: disk.sectors, model: disk.model })
    }
    fn block_count(&self) -> usize {
        crate::block::count()
    }
    fn block_register_usb(&self, slot: usize, sectors: u64, model: [u8; 40], origin: &str) -> Option<usize> {
        crate::block::register(crate::block::Backend::Usb(slot), sectors, model, origin)
    }
    fn block_usb_disk(&self, slot: usize) -> Option<usize> {
        crate::block::usb_disk(slot)
    }
    fn boot_disk(&self) -> Option<usize> {
        crate::storage::boot_disk()
    }

    fn unix_seconds(&self) -> u64 {
        crate::clock::unix_seconds()
    }
    fn timer_micros(&self) -> u64 {
        crate::time::micros()
    }
    fn timer_counter(&self) -> u64 {
        crate::timer::counter()
    }
    fn timer_ticks(&self) -> u64 {
        crate::time::ticks()
    }
    fn busy_wait_ms(&self, milliseconds: u64) {
        crate::time::busy_wait_ms(milliseconds);
    }
    fn wait_ticks(&self, count: u64) {
        let until = crate::time::ticks() + count;
        while crate::time::ticks() < until {
            crate::cpu::sleep();
        }
    }

    fn print(&self, arguments: core::fmt::Arguments) {
        crate::serial::print(arguments);
    }
    fn allocate_contiguous(&self, count: u64, below: Option<u64>) -> Option<u64> {
        crate::pmm::allocate(count, below)
    }
    fn allocate_contiguous_uncleared(&self, count: u64, below: Option<u64>) -> Option<u64> {
        crate::pmm::allocate_uncleared(count, below)
    }
    fn largest_free_run(&self) -> u64 {
        crate::pmm::largest_free_run()
    }
    fn free_frames(&self, address: u64, count: u64) {
        crate::pmm::free(address, count);
    }
    fn map_device(&self, address: u64, length: u64) {
        crate::vm::map_device(address, length);
    }
    fn pci_read32(&self, bus: u8, device: u8, function: u8, offset: u8) -> u32 {
        crate::pci_config::read32(bus, device, function, offset)
    }
    fn pci_write32(&self, bus: u8, device: u8, function: u8, offset: u8, value: u32) {
        crate::pci_config::write32(bus, device, function, offset, value);
    }
    fn pci_cpu_address(&self, pci: u64) -> u64 {
        crate::pci_config::cpu_address(pci)
    }

    fn msi_allocate(&self, handler: fn(usize), context: usize) -> Option<u8> {
        crate::msi::allocate(handler, context)
    }
    fn msi_release(&self, vector: u8) {
        crate::msi::release(vector);
    }
    fn msi_count(&self, vector: u8) -> u64 {
        crate::msi::count(vector)
    }
    fn msi_message(&self, vector: u8) -> (u64, u32) {
        crate::msi::message(vector)
    }
    // No legacy INTx routing and no I/O APIC: devices use MSI.
    fn msi_claim(&self) {}
    fn msi_intx_candidates(&self, _interrupt_line: u8) -> [u32; 9] {
        [u32::MAX; 9]
    }
    fn msi_attach_intx(&self, _gsi: u32, _handler: fn(usize), _context: usize) -> Option<u8> {
        None
    }
    fn msi_detach_intx(&self, _vector: u8, _context: usize) {}
    fn interrupt_target(&self) -> u32 {
        0
    }
    fn gsi_range(&self) -> (u32, u32) {
        (0, 0)
    }

    fn interrupts_save_and_disable(&self) -> bool {
        crate::cpu::interrupts_save_and_disable()
    }
    fn interrupts_restore(&self, was_enabled: bool) {
        crate::cpu::interrupts_restore(was_enabled);
    }
}

static MF: Mf = Mf;

pub fn install() {
    // RAM is identity-mapped: the direct map adds nothing.
    fos_hal::memory::set_phys_offset(crate::vm::DIRECT_BASE);
    fos_hal::set_platform(&MF);
    kernel_core::i2s::set(&I2S);
    kernel_core::screen::set(&SCREEN);
}

static I2S: kernel_core::i2s::Ops = kernel_core::i2s::Ops {
    present: crate::i2s_pi::present,
    label: crate::i2s_pi::label,
    start: crate::i2s_pi::start,
    stop: crate::i2s_pi::stop,
    poll: crate::i2s_pi::poll,
    read: crate::i2s_pi::read,
    write: crate::i2s_pi::write,
    playback_space: crate::i2s_pi::playback_space,
    format: crate::i2s_pi::format,
    select: crate::i2s_pi::select,
    print_modes: crate::i2s_pi::print_modes,
    status: crate::i2s_pi::status,
    fill_info: crate::i2s_pi::fill_info,
    fill_stats: crate::i2s_pi::fill_stats,
    buffer_info: crate::i2s_pi::buffer_info,
};

static DSI: kernel_core::screen::Dsi = kernel_core::screen::Dsi {
    width: crate::dsitouch::WIDTH,
    height: crate::dsitouch::HEIGHT,
    present: crate::dsitouch::present,
    poll: crate::dsitouch::poll,
    reports: crate::dsitouch::reports,
};

/// No pixel console yet, so no screen size and no rotation; the DSI chip's touches still reach the pointer queue.
static SCREEN: kernel_core::screen::Screen = kernel_core::screen::Screen { gfx_info: || None, rotation: |_| None, dsi: Some(&DSI) };
