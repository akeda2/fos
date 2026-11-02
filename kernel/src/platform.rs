//! The embedded kernel's answer to `fos_hal::Platform`: shared code (`kernel-core`) reaches disks, the clock and the console
//! through this, so it never names a driver. Installed first thing in `kmain`.

use fos_hal::{BlockError, Platform};

struct Embedded;

fn convert(error: crate::block::Error) -> BlockError {
    match error {
        crate::block::Error::NoSuchDisk => BlockError::NoSuchDisk,
        crate::block::Error::OutOfRange => BlockError::OutOfRange,
        crate::block::Error::Device => BlockError::Device,
    }
}

impl Platform for Embedded {
    fn block_read(&self, disk: usize, lba: u64, buffer: &mut [u8]) -> Result<(), BlockError> {
        crate::block::read(disk, lba, buffer).map_err(convert)
    }

    fn block_write(&self, disk: usize, lba: u64, buffer: &[u8]) -> Result<(), BlockError> {
        crate::block::write(disk, lba, buffer).map_err(convert)
    }

    fn unix_seconds(&self) -> u64 {
        crate::clock::unix_seconds()
    }

    fn block_info(&self, disk: usize) -> Option<fos_hal::BlockInfo> {
        crate::block::get(disk).map(|disk| fos_hal::BlockInfo { sectors: disk.sectors, model: disk.model })
    }

    fn block_count(&self) -> usize {
        crate::block::count()
    }

    fn boot_disk(&self) -> Option<usize> {
        crate::storage::boot_disk()
    }

    fn timer_micros(&self) -> u64 {
        crate::timer::micros()
    }

    fn timer_counter(&self) -> u64 {
        #[cfg(target_arch = "x86_64")]
        return unsafe { core::arch::x86_64::_rdtsc() };
        #[cfg(target_arch = "aarch64")]
        crate::timer::counter()
    }

    fn timer_ticks(&self) -> u64 {
        crate::timer::ticks()
    }

    fn allocate_contiguous(&self, count: u64, below: Option<u64>) -> Option<u64> {
        crate::memory::allocate_contiguous(count, below)
    }

    fn map_device(&self, address: u64, length: u64) {
        crate::paging::map_device(address, length);
    }

    fn wait_ticks(&self, count: u64) {
        crate::timer::wait_ticks(count);
    }

    fn pci_read32(&self, bus: u8, device: u8, function: u8, offset: u8) -> u32 {
        crate::pci_config::read32(bus, device, function, offset)
    }

    fn pci_write32(&self, bus: u8, device: u8, function: u8, offset: u8, value: u32) {
        crate::pci_config::write32(bus, device, function, offset, value);
    }

    #[cfg(target_arch = "aarch64")]
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

    #[cfg(target_arch = "x86_64")]
    fn port_in8(&self, port: u16) -> u8 {
        crate::cpu::inb(port)
    }
    #[cfg(target_arch = "x86_64")]
    fn port_in16(&self, port: u16) -> u16 {
        crate::cpu::inw(port)
    }
    #[cfg(target_arch = "x86_64")]
    fn port_in32(&self, port: u16) -> u32 {
        crate::cpu::inl(port)
    }
    #[cfg(target_arch = "x86_64")]
    fn port_out8(&self, port: u16, value: u8) {
        crate::cpu::outb(port, value);
    }
    #[cfg(target_arch = "x86_64")]
    fn port_out16(&self, port: u16, value: u16) {
        crate::cpu::outw(port, value);
    }
    #[cfg(target_arch = "x86_64")]
    fn port_out32(&self, port: u16, value: u32) {
        crate::cpu::outl(port, value);
    }
    #[cfg(target_arch = "x86_64")]
    fn interrupts_save_and_disable(&self) -> bool {
        crate::cpu::interrupts_save_and_disable()
    }
    #[cfg(target_arch = "x86_64")]
    fn interrupts_restore(&self, was_enabled: bool) {
        crate::cpu::interrupts_restore(was_enabled);
    }

    #[cfg(target_arch = "x86_64")]
    fn msi_claim(&self) {
        crate::msi::claim();
    }
    #[cfg(target_arch = "x86_64")]
    fn msi_intx_candidates(&self, interrupt_line: u8) -> [u32; 9] {
        crate::msi::intx_candidates(interrupt_line)
    }
    #[cfg(target_arch = "x86_64")]
    fn msi_attach_intx(&self, gsi: u32, handler: fn(usize), context: usize) -> Option<u8> {
        crate::msi::attach_intx(gsi, handler, context)
    }
    #[cfg(target_arch = "x86_64")]
    fn msi_detach_intx(&self, vector: u8, context: usize) {
        crate::msi::detach_intx(vector, context);
    }
    #[cfg(target_arch = "x86_64")]
    fn interrupt_target(&self) -> u32 {
        crate::hal::interrupt_target()
    }
    #[cfg(target_arch = "x86_64")]
    fn gsi_range(&self) -> (u32, u32) {
        crate::hal::gsi_range()
    }

    // The Pi 4 has no legacy INTx routing for these drivers and no I/O APIC.
    #[cfg(target_arch = "aarch64")]
    fn msi_claim(&self) {}
    #[cfg(target_arch = "aarch64")]
    fn msi_intx_candidates(&self, _interrupt_line: u8) -> [u32; 9] {
        [u32::MAX; 9]
    }
    #[cfg(target_arch = "aarch64")]
    fn msi_attach_intx(&self, _gsi: u32, _handler: fn(usize), _context: usize) -> Option<u8> {
        None
    }
    #[cfg(target_arch = "aarch64")]
    fn msi_detach_intx(&self, _vector: u8, _context: usize) {}
    #[cfg(target_arch = "aarch64")]
    fn interrupt_target(&self) -> u32 {
        0
    }
    #[cfg(target_arch = "aarch64")]
    fn gsi_range(&self) -> (u32, u32) {
        (0, 0)
    }

    fn print(&self, arguments: core::fmt::Arguments) {
        crate::print(arguments);
    }
}

static EMBEDDED: Embedded = Embedded;

pub fn install() {
    fos_hal::set_platform(&EMBEDDED);
}
