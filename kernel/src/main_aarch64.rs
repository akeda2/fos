use core::fmt::Write;
use core::panic::PanicInfo;
use fosfor_abi::{BootInfo, BOOT_INFO_MAGIC, BOOT_INFO_VERSION};

mod arch;
mod block;
mod font_data;
mod fosfs;
mod framebuffer;
mod gpt;
mod lineedit;
mod memory;
mod pci;
mod shell;
mod storage;
mod virtio_blk;

/// `println!` goes to the PL011 and, once it exists, the framebuffer; the boot log joins as its module is ported.
pub fn print(arguments: core::fmt::Arguments) {
    struct Both;
    impl Write for Both {
        fn write_str(&mut self, text: &str) -> core::fmt::Result {
            let _ = arch::aarch64::pl011::Serial.write_str(text);
            framebuffer::write_str(text);
            Ok(())
        }
    }
    let _ = Both.write_fmt(arguments);
}

/// The loader jumps here (through `_start`, arch/aarch64/mod.rs) with the BootInfo pointer.
#[unsafe(no_mangle)]
pub extern "C" fn kmain(info: *const BootInfo) -> ! {
    println!();
    println!("Fosfor OS kernel (aarch64): running from the UEFI loader.");
    let info = unsafe { &*info };
    if info.magic != BOOT_INFO_MAGIC || info.version != BOOT_INFO_VERSION || info.size as usize != core::mem::size_of::<BootInfo>() {
        println!("BootInfo is not what this kernel expects (magic {:#x}, version {}, size {}).", info.magic, info.version, info.size);
    } else {
        println!(
            "BootInfo v{}: {} memory-map entries, kernel at {:#x} ({} KiB), slot {}, ACPI RSDP {:#x}.",
            info.version,
            info.memory_map_count,
            info.kernel_base,
            info.kernel_size / 1024,
            info.boot_slot,
            info.acpi_rsdp
        );
    }
    arch::aarch64::exceptions::init();
    if info.magic == BOOT_INFO_MAGIC {
        let map_bytes = info.memory_map_count * info.memory_map_descriptor_size;
        memory::init(
            info,
            &[
                (info.kernel_base, info.kernel_size),
                (info as *const BootInfo as u64, 4096),
                (info.memory_map, map_bytes),
                (info.acpi_rsdp & !0xfff, 8192),
            ],
        );
        let stats = memory::stats();
        println!("Physical memory: {} MiB managed, {} MiB free.", stats.frames * memory::FRAME / (1024 * 1024), stats.free * memory::FRAME / (1024 * 1024));
        paging::build_and_load(info, &info.framebuffer);
        paging::map_device(arch::aarch64::pl011::QEMU_VIRT_BASE, 4096);
        let screen = framebuffer::init(&info.framebuffer);
        println!("Framebuffer console: {}.", if screen { "on" } else { "none (serial only)" });
        println!("Page tables loaded: RAM NX in 2 MiB blocks, kernel W^X, page 0 and stack guard unmapped.");
    }
    if info.magic == BOOT_INFO_MAGIC {
        gic::init();
        cpu::interrupts_enable();
        let start = timer::micros();
        while timer::micros() - start < 100_000 {
            cpu::sleep();
        }
        println!("GICv2 up: {} timer interrupts (INTID 30) in 100 ms, expected about 10.", gic::irq_ticks());
    }
    if info.magic == BOOT_INFO_MAGIC {
        match arch::aarch64::pci_config::init(info.acpi_rsdp) {
            Some(base) => println!("PCI: ECAM window at {base:#x} (ACPI MCFG)."),
            None => println!("PCI: the firmware describes no ECAM window."),
        }
        println!("PCI: {} function(s) on bus 0.", pci::scan());
        for device in pci::devices() {
            println!(
                "  {:02x}:{:02x}.{} {:04x}:{:04x} {}",
                device.bus, device.device, device.function, device.vendor, device.id,
                pci::class_name(device.class, device.subclass)
            );
        }
        storage::init(info);
    }
    let before = timer::micros();
    timer::busy_wait_ms(100);
    let waited = timer::micros() - before;
    println!(
        "Exception vectors installed (running at EL{}). Generic timer: {} Hz, ticks {}; a 100 ms wait took {} us.",
        arch::aarch64::exceptions::current_el(),
        timer::frequency(),
        timer::ticks(),
        waited
    );
    // Deliberate faults for the exception test (make test-aarch64-faults): one per Cargo feature.
    #[cfg(feature = "fault-ud")]
    unsafe {
        core::arch::asm!("udf #0")
    };
    #[cfg(feature = "fault-null")]
    unsafe {
        core::ptr::read_volatile(0x10 as *const u64);
    }
    cpu::interrupts_enable();
    shell::run()
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    println!("\nKERNEL PANIC: {info}");
    loop {
        unsafe { core::arch::asm!("wfe") };
    }
}
