use core::fmt::Write;
use core::panic::PanicInfo;
use fosfor_abi::{BootInfo, BOOT_INFO_MAGIC, BOOT_INFO_VERSION};

mod arch;
mod audio;
mod audioshell;
#[path = "ehci_none.rs"]
mod ehci;
#[path = "hda_none.rs"]
mod hda;
mod block;
mod bootcode;
mod font_data;
mod clock;
mod crash;
mod fatvol;
mod formatshell;
mod fosfs;
mod framebuffer;
mod gpt;
mod lineedit;
mod mailbox;
mod pipower;
mod i2s_pi;
mod diag;
mod netdump;
mod netfs;
mod nicshell;
mod netservice;
mod memory;
mod pci;
mod service_host;
mod shell;
mod shellcmds;
mod tapesync;
mod gpio;
mod gpio_remote;
mod gpio_shell;
mod i2c;
mod install;
mod shellhost;
mod storage;
mod tape_aarch64;
mod tasks;
mod tape_fs;
mod genet;
mod kup_serial;
mod kupshell;
mod slots;
mod sdhci;
mod usb_hub;
mod usb_audio;
mod usb_midi;
mod midishell;
mod usb_kbd;
mod usb_pointer;
mod usbshell;
mod usb_serial;
mod usb_serial_pl2303;
mod usb_serial_ti;
mod usb_storage;
mod virtio_blk;
mod xhci;

/// Everything printed since boot (`dmesg`), also replayed onto the screen once the framebuffer console exists (a Pi shows
/// nothing on a silent serial port otherwise). Bytes beyond the buffer are dropped.
static mut EARLY_LOG: [u8; 16384] = [0; 16384];
static mut EARLY_LEN: usize = 0;

/// The log so far, to the serial console only (it would otherwise be appended to itself).
pub fn dmesg() {
    unsafe {
        let len = *core::ptr::addr_of!(EARLY_LEN);
        let log = &*core::ptr::addr_of!(EARLY_LOG);
        if let Ok(text) = core::str::from_utf8(&log[..len]) {
            for line in text.split_inclusive('\n') {
                let _ = arch::aarch64::pl011::Serial.write_str(line.trim_end_matches('\n'));
                let _ = arch::aarch64::pl011::Serial.write_str("\r\n");
            }
        }
    }
}

pub fn replay_early_log() {
    unsafe {
        let len = *core::ptr::addr_of!(EARLY_LEN);
        let log = &*core::ptr::addr_of!(EARLY_LOG);
        if let Ok(text) = core::str::from_utf8(&log[..len]) {
            framebuffer::write_str(text);
        }
    }
}

/// `println!` goes to the PL011 and, once it exists, the framebuffer; the boot log joins as its module is ported.
pub fn print(arguments: core::fmt::Arguments) {
    struct Both;
    impl Write for Both {
        fn write_str(&mut self, text: &str) -> core::fmt::Result {
            let _ = arch::aarch64::pl011::Serial.write_str(text);
            unsafe {
                {
                    let len = *core::ptr::addr_of!(EARLY_LEN);
                    let room = 16384 - len;
                    let take = text.len().min(room);
                    core::ptr::copy_nonoverlapping(text.as_ptr(), (core::ptr::addr_of_mut!(EARLY_LOG) as *mut u8).add(len), take);
                    *core::ptr::addr_of_mut!(EARLY_LEN) = len + take;
                }
            }
            framebuffer::write_str(text);
            Ok(())
        }
    }
    let _ = Both.write_fmt(arguments);
}

const PI4_GIC_DISTRIBUTOR: u64 = 0xff84_1000;
const PI4_PL011: u64 = 0xfe20_1000;

/// The loader jumps here (through `_start`, arch/aarch64/mod.rs) with the BootInfo pointer.
#[unsafe(no_mangle)]
pub extern "C" fn kmain(info: *const BootInfo) -> ! {
    let info = unsafe { &*info };
    let screen = &info.framebuffer;
    arch::aarch64::early::init(screen.base, screen.stride, screen.width, screen.height);
    arch::aarch64::early::mark(0);
    arch::aarch64::exceptions::init();
    arch::aarch64::early::mark(1);
    // Before the first line is printed: the PL011 is at the address the firmware's SPCR table gives (QEMU `virt` 0x09000000,
    // a Pi 4 0xFE201000). Printing first, to QEMU's address, would write to nothing on a Pi and show no sign of life.
    arch::aarch64::acpi::remember(info.acpi_rsdp);
    let mut uart_from_acpi = false;
    let mut uart_from_pi4 = false;
    if info.magic == BOOT_INFO_MAGIC && info.version == BOOT_INFO_VERSION {
        if let Some(base) = arch::aarch64::acpi::spcr_uart(info.acpi_rsdp) {
            arch::aarch64::pl011::set_base(base);
            uart_from_acpi = true;
        } else if arch::aarch64::acpi::madt_gic(info.acpi_rsdp).is_some_and(|(distributor, _)| distributor == PI4_GIC_DISTRIBUTOR) {
            // edk2 on a Pi 4 gives no usable SPCR; the PL011 on GPIO14/15 is where the firmware's own console runs.
            arch::aarch64::pl011::set_base(PI4_PL011);
            uart_from_pi4 = true;
        }
    }
    arch::aarch64::early::mark(2);
    println!();
    arch::aarch64::early::mark(3);
    println!("Fosfor OS kernel (aarch64): running from the UEFI loader.");
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
    arch::aarch64::early::mark(4);
    // The console and interrupt controller are where the firmware's ACPI tables say (QEMU `virt` and the Pi 4 differ).
    if info.magic == BOOT_INFO_MAGIC {
        if let Some((distributor, cpu_interface)) = arch::aarch64::acpi::madt_gic(info.acpi_rsdp) {
            gic::set_base(distributor, cpu_interface);
        }
    }
    if info.magic == BOOT_INFO_MAGIC {
        let map_bytes = info.memory_map_count * info.memory_map_descriptor_size;
        memory::init(
            info,
            &[
                (info.kernel_base, info.kernel_size),
                (info as *const BootInfo as u64, 4096),
                (info.memory_map, map_bytes),
                (info.acpi_rsdp & !0xfff, 16384),
            ],
        );
        crash::set_boot_crash_count(info.crash_count);
        arch::aarch64::early::mark(5);
        let stats = memory::stats();
        println!("Physical memory: {} MiB managed, {} MiB free.", stats.frames * memory::FRAME / (1024 * 1024), stats.free * memory::FRAME / (1024 * 1024));
        paging::build_and_load(info, &info.framebuffer);
        arch::aarch64::early::mark(6);
        paging::map_device(arch::aarch64::pl011::base(), 4096);
        println!("Console: PL011 at {:#x} ({}).", arch::aarch64::pl011::base(), if uart_from_acpi { "ACPI SPCR" } else if uart_from_pi4 { "Pi 4 default, no usable SPCR" } else { "default" });
        arch::aarch64::early::mark(7);
        let screen = framebuffer::init(&info.framebuffer);
        if screen {
            replay_early_log();
        }
        println!("Framebuffer console: {}.", if screen { "on" } else { "none (serial only)" });
        println!("Page tables loaded: RAM NX in 2 MiB blocks, kernel W^X, page 0 and stack guard unmapped.");
    }
    arch::aarch64::early::mark(8);
    if info.magic == BOOT_INFO_MAGIC {
        gic::init();
        cpu::interrupts_enable();
        let start = timer::micros();
        while timer::micros() - start < 100_000 {
            cpu::sleep();
        }
        println!("GICv2 at {:#x}/{:#x}: {} timer interrupts (INTID 30) in 100 ms, expected about 10.", gic::bases().0, gic::bases().1, gic::irq_ticks());
        arch::aarch64::pl011::enable_interrupt(gic::bases().0 == PI4_GIC_DISTRIBUTOR);
    }
    if info.magic == BOOT_INFO_MAGIC {
        match arch::aarch64::pci_config::init(info.acpi_rsdp) {
            Some((base, kind)) => println!("PCI: config space at {base:#x}: {kind}."),
            None => println!("PCI: no config-space window (no MCFG, not a Pi 4)."),
        }
        println!("PCI: {} function(s) on bus 0.", pci::scan());
        println!("MSI: {}.", msi::init());
        for device in pci::devices() {
            println!(
                "  {:02x}:{:02x}.{} {:04x}:{:04x} {}",
                device.bus, device.device, device.function, device.vendor, device.id,
                pci::class_name(device.class, device.subclass)
            );
        }
        storage::init(info);
        crash::init();
        genet::init();
        if gic::bases().0 == PI4_GIC_DISTRIBUTOR {
            mailbox::second_screen();
        }
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
    tape_aarch64::set_boot_info(info);
    println!("USB: {} keyboard(s) found.", usb_kbd::init());
    let no_msi = shell::apply_settings();
    xhci::enable_interrupts_all(no_msi);
    cpu::interrupts_enable();
    if crash::debug().crash_on_boot {
        println!("debug: crash-on-boot is set; faulting now.");
        unsafe { core::arch::asm!("udf #0") };
    }
    slots::confirm_boot();
    crash::confirm();
    shell::run()
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    println!("\nKERNEL PANIC: {info}");
    loop {
        unsafe { core::arch::asm!("wfe") };
    }
}
