//! The server kernel ("FOS-mf"). It is entered by the same UEFI and BIOS loaders as the embedded kernel, with the same
//! `BootInfo`; it narrates over the serial console. One kernel for every CPU: what differs (entry, interrupts, timers, page-table
//! hardware, SMP start, the platform) lives in `arch/{x86_64,aarch64}` behind a small set of hooks; the rest is shared
//! (docs/big-kernel.md, "mf on aarch64").
#![no_std]
#![no_main]

extern crate alloc;

#[macro_use]
mod serial;
mod arch;
pub use arch::*;
mod boot;
mod heap;
mod log;
mod pmm;
mod percpu;
mod sched;
mod selftest;
mod sync;
mod time;
mod vm;
mod fb;
// the shared files of the embedded kernel name the macro by path
pub(crate) use serial::{print, println};

/// Modules that only build for x86-64 so far. The aarch64 port makes them portable one by one (each moves out of this list
/// when it compiles there); nothing in it is meant to stay x86-only unless it is hardware that only x86 has.
macro_rules! until_ported {
    ($($item:item)*) => { $( #[cfg(target_arch = "x86_64")] $item )* };
}

mod process;
mod tapenet;
mod tapeaudio;
#[cfg(target_arch = "aarch64")]
mod tapepi;
#[cfg(target_arch = "aarch64")]
mod gpio_remote;
mod audioshell;
mod tapeapi;
mod threads;
mod tapesys;
mod user_test;

#[path = "../../kernel/src/install.rs"]
mod install;
#[path = "../../kernel/src/bootcode.rs"]
mod bootcode;

until_ported! {
    mod tapeserial;
    mod ps2;
}

#[cfg(not(target_arch = "x86_64"))]
mod unported;
#[cfg(not(target_arch = "x86_64"))]
use unported::*;
mod usb;
mod net;
mod netfs;
mod netclient;
mod service;
mod vnic;
mod tapesync;
mod usbshell;
mod session;
mod tapefs;
mod config;
mod debug;
mod debugcpu;
mod sysrq;
mod shell;
mod shellhost;
mod script;
mod sysshell;
mod aoedisk;
mod aoetarget;
mod mkimage;
mod mdvol;
mod top;
#[path = "../../kernel/src/formatshell.rs"]
mod formatshell;
mod helptext;
#[path = "../../kernel/src/kup_serial.rs"]
mod kup_serial;
mod slots;
mod block;
mod storage;
mod private;
use kernel_core::{fatvol, fosfs, gpt, pci};
#[cfg(target_arch = "aarch64")]
use kernel_core::usb_pointer;

use alloc::{boxed::Box, collections::BTreeMap, string::String, vec::Vec};
use core::panic::PanicInfo;
use fosfor_abi::{BootInfo, TYPE_CONVENTIONAL};

/// Read by KUP, `fosadm` and the tests from the ELF section table, so the two kernels are never mixed up.
#[used]
#[unsafe(link_section = ".fos_flavour")]
static FLAVOUR: [u8; 2] = *b"mf";

fn mib(bytes: u64) -> u64 {
    bytes >> 20
}

/// On a Pi 4 the firmware can drive the DSI panel and a second HDMI output: the panel gets its own 800x480 console (the UEFI one
/// is 800x600), the second output becomes tape screen 1. The mailbox needs the timer, hence this late.
#[cfg(target_arch = "aarch64")]
fn pi_screens(info: &BootInfo) {
    if !arch::hal::is_pi4() {
        return;
    }
    if let Some(dsi) = mailbox::dsi_framebuffer(&info.framebuffer) {
        if fb::init(&dsi).is_some() {
            fb::write_str(log::contents());
        }
    }
    mailbox::second_screen();
}

#[unsafe(no_mangle)]
pub extern "C" fn kmain(info: *const BootInfo) -> ! {
    let info = unsafe { &*info };
    arch::early(info);
    serial::init();
    println!();
    println!("Fosfor OS server kernel (mf, slice 8): {}", arch::running_on());
    if !info.is_valid() {
        println!("BootInfo is not valid (magic {:#x}, version {}): stopping.", info.magic, info.version);
        halt();
    }
    println!("BootInfo: version {}, kernel at {:#x} ({} KiB), booted from slot {}.", info.version, info.kernel_base, info.kernel_size / 1024, info.boot_slot);
    let fb = &info.framebuffer;
    println!("Framebuffer: {}x{} at {:#x}. ACPI RSDP at {:#x}.", fb.width, fb.height, fb.base, info.acpi_rsdp);

    // Everything the kernel needs from loader memory is copied out before the switch to our own page tables.
    let Some(machine) = boot::Machine::from_info(info) else {
        println!("The memory map has more than 1024 descriptors: stopping.");
        halt();
    };
    boot::remember(&machine);

    let (mut regions, mut ram_pages, mut free_pages) = (0u64, 0u64, 0u64);
    for d in machine.descriptors() {
        regions += 1;
        if fosfor_abi::RAM_LIKE_TYPES.contains(&d.kind) {
            ram_pages += d.pages;
        }
        if d.kind == TYPE_CONVENTIONAL {
            free_pages += d.pages;
        }
    }
    println!("Memory map: {regions} regions, {} MiB of RAM-like memory, {} MiB conventional.", mib(ram_pages * 4096), mib(free_pages * 4096));

    pmm::init(&machine, &[(machine.kernel_base, machine.kernel_size), (machine.info_page, 4096), machine.map, arch::reserved_extra(info)]);
    let frames = pmm::stats();
    println!("Physical memory: {} MiB managed in {} frames, {} MiB free.", mib(frames.managed * 4096), frames.managed, mib(frames.free * 4096));

    let info = arch::init_memory(&machine, info);

    if !heap::init() {
        println!("The kernel heap could not be created: stopping.");
        halt();
    }
    heap_self_test();
    let heap = heap::stats();
    println!("Heap: self-test passed, {} KiB in the heap, {} KiB free.", heap.total / 1024, heap.free / 1024);

    arch::init_interrupts();
    let century = arch::init_machine(&machine);
    boot_services(info, century)
}

/// What follows the CPU bring-up: the clock, PCI and the drivers, the config files, the scheduler and the service loop.
#[cfg(target_arch = "x86_64")]
fn boot_services(info: &BootInfo, century: u8) -> ! {
    clock::init(century);

    // Device drivers: `kernel-core` reaches the machine through the platform below. Interrupts stay on, as the drivers wait on ticks.
    platform::install();
    let devices = pci::scan();
    println!("PCI: {devices} function(s) found.");
    for device in pci::devices() {
        println!("  {:02x}:{:02x}.{} {:04x}:{:04x} {} (class {:02x}{:02x}, interface {:02x})", device.bus, device.device, device.function, device.vendor, device.id, pci::class_name(device.class, device.subclass), device.class, device.subclass, device.interface);
    }
    storage::init(info);
    // A USB stick is a disk only once USB is up; a machine that boots from one gets USB before its config is read (so `no-msi`
    // in `settings` cannot apply to USB on such a machine).
    let usb_first = storage::boot_disk().is_none();
    if usb_first {
        usb::init();
        storage::find_boot_disk(info);
    }
    if let Some(disk) = storage::boot_disk() {
        slots::init(info, disk);
    }
    debug::init(info.crash_count);
    debug::report_previous_crash();
    // `settings` and `hostname` may live on a USB stick, which is a disk only once USB is up. The controllers stay polled, so
    // `no-msi` has nothing to change there.
    if !usb_first {
        usb::init();
    }
    config::load();
    kernel_core::hda::init_all(config::no_msi());
    if config::crash_on_boot() {
        println!("debug: crash-on-boot is set; faulting now.");
        arch::crash_now();
    }
    slots::confirm_boot();
    net::init();
    vnic::apply();
    ps2::init();
    selftest::run();
    // After the self-test: that is where the scheduler starts, and private drivers may start threads.
    private::init();
    if config::md_auto() {
        mdvol::start_keeper();
    }
    aoetarget::init();
    // Drains the NICs on their interrupts, so a long shell command on the service thread does not stop the network.
    sched::spawn_any("net-rx", net::rx_thread, 0);
    sched::spawn("service", service_thread, 0).expect("the service thread");
    println!("kernel-mf: slice 8 reached the end of boot; entering the service loop.");
    // The boot context is the idle thread: it runs when nothing else is ready and sleeps until the next interrupt.
    sched::become_idle();
    loop {
        sched::yield_now();
        // Interrupts are on: the next tick (or NIC interrupt) wakes the CPU.
        arch::wait_for_interrupt();
    }
}

/// The aarch64 port has the CPU, memory, heap, timer and the shell on the serial console; PCI and the drivers (storage,
/// network, USB) follow, and with them the config files and the slot metadata that need a disk.
#[cfg(target_arch = "aarch64")]
fn boot_services(info: &BootInfo, _century: u8) -> ! {
    // Device drivers: `kernel-core` reaches the machine through the platform. The config-space window comes from the ACPI MCFG
    // table (QEMU virt) or is the Pi 4's own controller.
    platform::install();
    match arch::pci_config::init(info.acpi_rsdp) {
        Some((base, kind)) => println!("PCI: config space at {base:#x}: {kind}."),
        None => println!("PCI: no config-space window (no MCFG, not a Pi 4)."),
    }
    println!("PCI: {} function(s) on bus 0.", pci::scan());
    println!("MSI: {}.", arch::msi::init());
    for device in pci::devices() {
        println!("  {:02x}:{:02x}.{} {:04x}:{:04x} {} (class {:02x}{:02x}, interface {:02x})", device.bus, device.device, device.function, device.vendor, device.id, pci::class_name(device.class, device.subclass), device.class, device.subclass, device.interface);
    }
    storage::init(info);
    // As on x86: a machine that boots from a USB stick needs USB up before its boot disk exists.
    let usb_first = storage::boot_disk().is_none();
    if usb_first {
        usb::init();
        storage::find_boot_disk(info);
    }
    if let Some(disk) = storage::boot_disk() {
        slots::init(info, disk);
    }
    debug::init(info.crash_count);
    debug::report_previous_crash();
    if !usb_first {
        usb::init();
    }
    config::load();
    #[cfg(target_arch = "aarch64")]
    pi_screens(info);
    #[cfg(target_arch = "aarch64")]
    if dsitouch::init() {
        println!("DSI touch: FT5406-style controller at 0x38 on the display connector's I2C bus (`touch`).");
    }
    slots::confirm_boot();
    net::init();
    vnic::apply();
    selftest::run();
    // After the self-test: that is where the scheduler starts, and private drivers may start threads.
    private::init();
    sched::spawn("service", service_thread, 0).expect("the service thread");
    println!("kernel-mf: slice 8 reached the end of boot; entering the service loop.");
    sched::become_idle();
    loop {
        sched::yield_now();
        arch::wait_for_interrupt();
    }
}

/// Exercises the heap the way the kernel will: boxes, growing vectors, strings, a map, an over-aligned block, and growth
/// beyond the first chunk; then checks that freeing everything returns the memory.
fn heap_self_test() {
    let before = heap::stats().free;
    {
        let boxed = Box::new(0x1234_5678_u64);
        assert_eq!(*boxed, 0x1234_5678);

        let mut numbers: Vec<u32> = Vec::new();
        for n in 0..100_000 {
            numbers.push(n);
        }
        assert_eq!(numbers.iter().map(|&n| n as u64).sum::<u64>(), 100_000 * 99_999 / 2);

        let mut text = String::new();
        for n in 0..200 {
            text.push_str(&alloc::format!("{n},"));
        }
        assert!(text.starts_with("0,1,2,") && text.ends_with("199,"));

        let mut map = BTreeMap::new();
        for n in 0..1000u32 {
            map.insert(n.wrapping_mul(2_654_435_761), n);
        }
        assert_eq!(map.len(), 1000);
        assert_eq!(map.get(&5u32.wrapping_mul(2_654_435_761)), Some(&5));

        let aligned = alloc::alloc::Layout::from_size_align(100, 4096).unwrap();
        let block = unsafe { alloc::alloc::alloc(aligned) };
        assert!(!block.is_null() && block as usize % 4096 == 0);
        unsafe { alloc::alloc::dealloc(block, aligned) };

        // More than the first chunk: the heap has to grow.
        let big = alloc::vec![0xa5u8; 6 << 20];
        assert!(big.iter().all(|&b| b == 0xa5));
        assert!(heap::stats().total > 2 << 20);
    }
    let after = heap::stats().free;
    assert!(after >= before, "the heap lost memory: {before} free before, {after} after");
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    serial::break_lock();
    println!("\nFosfor OS server kernel PANIC: {info}");
    let mut frame: trap::Frame = unsafe { core::mem::zeroed() };
    frame.vector = debug::PANIC_VECTOR;
    trap::fatal_reset(&frame)
}

/// The server's housekeeping: NICs, serial input, the reset timer and the slot streak. Polls once per pass and gives way.
extern "C" fn service_thread(_: usize) {
    // The `start` file runs here, as at the end of the embedded kernel's boot; the prompt follows it. Disks other machines
    // export (`aoe-attach` in settings) come first, so `start` and the services find them (the shell's `aoe attach` runs on
    // this thread too).
    shell::init();
    aoedisk::attach_from_settings();
    config::run_start(shell::run);
    shell::prompt();
    loop {
        net::poll();
        usb::poll();
        ps2::poll();
        while let Some(byte) = serial::read_byte() {
            shell::feed(byte);
        }
        shell::poll_escape();
        top::poll();
        net::housekeeping();
        service::poll();
        slots::housekeeping();
        debug::pet();
        sched::yield_now();
    }
}
