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

    fn block_register_usb(&self, slot: usize, sectors: u64, model: [u8; 40], origin: &str) -> Option<usize> {
        crate::block::register(crate::block::Backend::Usb(slot), sectors, model, origin)
    }

    fn block_usb_disk(&self, slot: usize) -> Option<usize> {
        crate::block::usb_disk(slot)
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

    fn allocate_contiguous_uncleared(&self, count: u64, below: Option<u64>) -> Option<u64> {
        crate::memory::allocate_contiguous_uncleared(count, below)
    }

    fn largest_free_run(&self) -> u64 {
        crate::memory::largest_free_run()
    }

    fn busy_wait_ms(&self, milliseconds: u64) {
        crate::timer::busy_wait_ms(milliseconds);
    }

    fn free_frames(&self, address: u64, count: u64) {
        crate::memory::free(address, count);
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

    fn task_context(&self) -> usize {
        crate::tasks::context()
    }

    fn task_context_live(&self, context: usize) -> bool {
        crate::tasks::context_live(context)
    }

    #[cfg(target_arch = "x86_64")]
    fn msi_message(&self, vector: u8) -> (u64, u32) {
        // Physical destination mode, fixed delivery, to the boot CPU's local APIC.
        (0xfee0_0000 | u64::from(crate::hal::interrupt_target()) << 12, u32::from(vector))
    }
    #[cfg(target_arch = "aarch64")]
    fn msi_message(&self, vector: u8) -> (u64, u32) {
        crate::msi::message(vector)
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
    fn interrupts_save_and_disable(&self) -> bool {
        crate::cpu::interrupts_save_and_disable()
    }
    fn interrupts_restore(&self, was_enabled: bool) {
        crate::cpu::interrupts_restore(was_enabled);
    }

    #[cfg(target_arch = "x86_64")]
    fn watchdog_pet(&self) {
        crate::debug::pet();
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
    crate::usb_register::install();
    kernel_core::screen::set(&SCREEN);
    #[cfg(target_arch = "aarch64")]
    kernel_core::i2s::set(&I2S);
}

#[cfg(target_arch = "aarch64")]
static DSI: kernel_core::screen::Dsi = kernel_core::screen::Dsi {
    width: crate::dsitouch::WIDTH,
    height: crate::dsitouch::HEIGHT,
    present: crate::dsitouch::present,
    poll: crate::dsitouch::poll,
    reports: crate::dsitouch::reports,
};

static SCREEN: kernel_core::screen::Screen = kernel_core::screen::Screen {
    gfx_info: crate::framebuffer::gfx_info,
    rotation: crate::framebuffer::rotation,
    #[cfg(target_arch = "aarch64")]
    dsi: Some(&DSI),
    #[cfg(not(target_arch = "aarch64"))]
    dsi: None,
};

#[cfg(target_arch = "aarch64")]
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
