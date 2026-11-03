//! kernel-mf's answer to `fos_hal::Platform`: what the shared drivers in `kernel-core` need from the machine. Same contract
//! as the embedded kernel's `platform.rs`. Differences: device memory is mapped through the direct map (`vm`) and the drivers reach it with `memory::phys_to_virt`, all device
//! interrupts go to the boot CPU, and there are no tasks yet (the defaults stand in until the scheduler, slice 8).

use fos_hal::{BlockError, BlockInfo, Platform};

struct Mf;

fn convert(error: crate::block::Error) -> BlockError {
    match error {
        crate::block::Error::NoSuchDisk => BlockError::NoSuchDisk,
        crate::block::Error::OutOfRange => BlockError::OutOfRange,
        crate::block::Error::Device => BlockError::Device,
    }
}

impl Platform for Mf {
    fn block_read(&self, disk: usize, lba: u64, buffer: &mut [u8]) -> Result<(), BlockError> {
        crate::block::read(disk, lba, buffer).map_err(convert)
    }
    fn block_write(&self, disk: usize, lba: u64, buffer: &[u8]) -> Result<(), BlockError> {
        crate::block::write(disk, lba, buffer).map_err(convert)
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

    /// No RTC driver yet (DEFERRED): files get no meaningful modification time.
    fn unix_seconds(&self) -> u64 {
        0
    }
    fn timer_micros(&self) -> u64 {
        crate::apic::micros()
    }
    fn timer_counter(&self) -> u64 {
        crate::cpu::rdtsc()
    }
    fn timer_ticks(&self) -> u64 {
        crate::apic::ticks()
    }
    fn busy_wait_ms(&self, milliseconds: u64) {
        crate::apic::busy_wait_ms(milliseconds);
    }
    fn wait_ticks(&self, count: u64) {
        let until = crate::apic::ticks() + count;
        while crate::apic::ticks() < until {
            core::hint::spin_loop();
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
        (0xfee0_0000 | u64::from(self.interrupt_target()) << 12, u32::from(vector))
    }
    fn msi_claim(&self) {
        crate::msi::claim();
    }
    fn msi_intx_candidates(&self, interrupt_line: u8) -> [u32; 9] {
        crate::msi::intx_candidates(interrupt_line)
    }
    fn msi_attach_intx(&self, gsi: u32, handler: fn(usize), context: usize) -> Option<u8> {
        crate::msi::attach_intx(gsi, handler, context)
    }
    fn msi_detach_intx(&self, vector: u8, context: usize) {
        crate::msi::detach_intx(vector, context);
    }
    fn interrupt_target(&self) -> u32 {
        crate::percpu::get(0).apic_id
    }
    fn gsi_range(&self) -> (u32, u32) {
        crate::apic::ioapic::gsi_range()
    }

    fn port_in8(&self, port: u16) -> u8 {
        crate::cpu::inb(port)
    }
    fn port_in16(&self, port: u16) -> u16 {
        crate::cpu::inw(port)
    }
    fn port_in32(&self, port: u16) -> u32 {
        crate::cpu::inl(port)
    }
    fn port_out8(&self, port: u16, value: u8) {
        crate::cpu::outb(port, value);
    }
    fn port_out16(&self, port: u16, value: u16) {
        crate::cpu::outw(port, value);
    }
    fn port_out32(&self, port: u16, value: u32) {
        crate::cpu::outl(port, value);
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
    fos_hal::memory::set_phys_offset(crate::vm::DIRECT_BASE);
    fos_hal::set_platform(&MF);
}
