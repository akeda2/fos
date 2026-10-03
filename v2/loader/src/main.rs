//! fos v2 loader, milestone M0: a UEFI application that boots, narrates what the
//! firmware handed it, and powers the machine off.
//!
//! This is the v2 counterpart of `boot/stage2.S`: the first code of ours that
//! runs, and the one that explains what it sees. M0 only *looks*: it prints the
//! firmware identity, the memory map, the graphics mode and the ACPI root
//! pointer, because those are the four things the kernel will need and the four
//! places real firmware differs from QEMU's OVMF. Later milestones add reading the
//! kernel from an A/B slot, `ExitBootServices` and the jump into the 64-bit
//! kernel (see `docs/v2-design.md`, section 4).
#![no_std]
#![no_main]

mod console;
mod uefi;

use core::panic::PanicInfo;
use uefi::{Handle, MemoryDescriptor, Status, SystemTable};

/// Room for the firmware's memory map: real machines report 50-150 entries of
/// 48 bytes or so. If a firmware needs more, the loader says so and stops.
const MEMORY_MAP_BYTES: usize = 128 * 1024;
static mut MEMORY_MAP: [u8; MEMORY_MAP_BYTES] = [0; MEMORY_MAP_BYTES];

/// The firmware's entry into the loader.
#[unsafe(no_mangle)]
pub extern "efiapi" fn efi_main(_image: Handle, system_table: *mut SystemTable) -> Status {
    let table = unsafe { &*system_table };
    console::init_serial();
    console::set_text_out(table.con_out);

    println!();
    println!("FOS v2 loader (milestone M0): UEFI application running.");
    println!("This stage looks around and reports; it does not load a kernel yet.");
    println!();
    describe_firmware(table);

    // UEFI arms a 5-minute watchdog before starting an application and reboots
    // the machine when it expires. A loader that waits for a person (or a slow
    // serial link) must switch it off.
    let boot = unsafe { &*table.boot_services };
    let status = (boot.set_watchdog_timer)(0, 0, 0, core::ptr::null());
    println!("Firmware watchdog disabled (status {status:#x}).");
    println!();

    describe_memory_map(boot);
    describe_graphics(boot);
    describe_acpi(table);

    println!();
    println!("FOS v2 loader: M0 complete. Powering off through the runtime services.");
    let runtime = unsafe { &*table.runtime_services };
    (runtime.reset_system)(uefi::RESET_SHUTDOWN, uefi::SUCCESS, 0, core::ptr::null())
}

/// Who built this firmware and which UEFI specification it implements.
fn describe_firmware(table: &SystemTable) {
    print!("Firmware: ");
    let mut vendor = table.firmware_vendor;
    while !vendor.is_null() && unsafe { *vendor } != 0 {
        let unit = unsafe { *vendor };
        print!("{}", if unit < 0x80 { unit as u8 as char } else { '?' });
        vendor = unsafe { vendor.add(1) };
    }
    println!(" revision {:#x}", table.firmware_revision);
    let revision = table.header.revision;
    println!("UEFI specification {}.{}", revision >> 16, revision & 0xffff);
}

/// Asks the firmware for the memory map and summarizes it: which ranges are free
/// RAM, which the firmware still owns (boot services, runtime services), and which
/// are reserved or device memory. The kernel will turn this into its allocator.
fn describe_memory_map(boot: &uefi::BootServices) {
    let mut size = MEMORY_MAP_BYTES;
    let (mut key, mut descriptor_size, mut version) = (0usize, 0usize, 0u32);
    let status = unsafe {
        (boot.get_memory_map)(
            &mut size,
            core::ptr::addr_of_mut!(MEMORY_MAP) as *mut MemoryDescriptor,
            &mut key,
            &mut descriptor_size,
            &mut version,
        )
    };
    if status != uefi::SUCCESS || descriptor_size < core::mem::size_of::<MemoryDescriptor>() {
        println!("Memory map: GetMemoryMap failed (status {status:#x}, needs {size} bytes).");
        return;
    }
    let count = size / descriptor_size;
    println!("Memory map: {count} entries of {descriptor_size} bytes (descriptor version {version}, key {key:#x}).");

    let mut pages_by_kind = [0u64; 16];
    let base = core::ptr::addr_of!(MEMORY_MAP) as *const u8;
    for index in 0..count {
        let entry = unsafe { core::ptr::read_unaligned(base.add(index * descriptor_size) as *const MemoryDescriptor) };
        let end = entry.physical_start + entry.pages * 4096;
        println!(
            "  {:#012x}-{:#012x} {:>9} KiB  {}",
            entry.physical_start,
            end,
            entry.pages * 4,
            uefi::memory_type_name(entry.kind)
        );
        if let Some(total) = pages_by_kind.get_mut(entry.kind as usize) {
            *total += entry.pages;
        }
    }
    println!("Totals by type (MiB):");
    for (kind, pages) in pages_by_kind.iter().enumerate() {
        if *pages != 0 {
            println!("  {:>6} MiB  {}", pages / 256, uefi::memory_type_name(kind as u32));
        }
    }
    // Memory the loader and kernel may use once boot services end: usable RAM,
    // plus the loader's own and the boot-services regions.
    let reusable: u64 = [1usize, 2, 3, 4, 7].iter().map(|&kind| pages_by_kind[kind]).sum();
    println!("Reusable after ExitBootServices: {} MiB.", reusable / 256);
}

/// The graphics output protocol: a linear framebuffer the kernel can draw its
/// text console into, if the firmware set one up.
fn describe_graphics(boot: &uefi::BootServices) {
    let mut interface: *mut core::ffi::c_void = core::ptr::null_mut();
    let status = (boot.locate_protocol)(&uefi::GRAPHICS_OUTPUT_PROTOCOL, core::ptr::null_mut(), &mut interface);
    if status != uefi::SUCCESS || interface.is_null() {
        println!("Graphics: no graphics output protocol (status {status:#x}).");
        return;
    }
    let graphics = unsafe { &*(interface as *const uefi::GraphicsOutput) };
    let mode = unsafe { &*graphics.mode };
    let info = unsafe { &*mode.info };
    let format = match info.pixel_format {
        0 => "RGB",
        1 => "BGR",
        2 => "bit mask",
        _ => "none (no linear framebuffer)",
    };
    println!(
        "Graphics: {}x{} {} pixels, {} per line, mode {} of {}.",
        info.horizontal_resolution,
        info.vertical_resolution,
        format,
        info.pixels_per_scan_line,
        mode.mode,
        mode.max_mode
    );
    println!("  framebuffer at {:#x}, {} KiB.", mode.frame_buffer_base, mode.frame_buffer_size / 1024);
}

/// The ACPI root pointer, found in the firmware's configuration table. The kernel
/// needs it for the interrupt controller layout, the PCIe config space and timers.
fn describe_acpi(table: &SystemTable) {
    let entries = unsafe { core::slice::from_raw_parts(table.configuration_table, table.number_of_table_entries) };
    let find = |guid: &uefi::Guid| entries.iter().find(|entry| entry.vendor_guid == *guid).map(|entry| entry.vendor_table);
    let (root, version) = match (find(&uefi::ACPI_20_TABLE), find(&uefi::ACPI_10_TABLE)) {
        (Some(root), _) => (root, "2.0+"),
        (None, Some(root)) => (root, "1.0"),
        _ => {
            println!("ACPI: no RSDP in the configuration table.");
            return;
        }
    };
    let signature = unsafe { core::slice::from_raw_parts(root as *const u8, 8) };
    println!(
        "ACPI {} RSDP at {:#x}, signature {}.",
        version,
        root as usize,
        if signature == b"RSD PTR " { "valid" } else { "WRONG" }
    );
    println!("Configuration table has {} entries.", entries.len());
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    println!("\nFOS v2 loader PANIC: {info}");
    loop {
        unsafe { core::arch::asm!("hlt") };
    }
}
